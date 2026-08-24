use std::cell::Cell;
use std::fmt;

/// The task state: lifecycle flags packed alongside a reference count.
///
/// A task is only ever touched from the thread that owns the runtime, so this
/// is a plain `Cell<usize>`. Every transition below is a load, some arithmetic
/// and a store; none of them can lose a race and have to be retried, so each
/// one reads as straight-line code.
pub(super) struct State {
    val: Cell<usize>,
}

/// A state value read out of a [`State`].
#[derive(Copy, Clone)]
pub(super) struct Snapshot(usize);

/// The task is being polled. Acts as a lock over the stage field.
const RUNNING: usize = 0b00_0001;

/// The future has finished and its result has been stored. Never unset.
const COMPLETE: usize = 0b00_0010;

/// A `Runnable` for this task exists, so it must not be queued a second time.
const NOTIFIED: usize = 0b00_0100;

/// A `JoinHandle` for this task exists.
const JOIN_INTEREST: usize = 0b00_1000;

/// The task should be cancelled at the next opportunity. Meaningless once
/// `COMPLETE` is set.
const CANCELLED: usize = 0b01_0000;

/// The task is registered in an `OwnedTasks`, in the slot named by its id.
const OWNED: usize = 0b10_0000;

/// Every bit that is not part of the reference count.
const STATE_MASK: usize = RUNNING | COMPLETE | NOTIFIED | JOIN_INTEREST | CANCELLED | OWNED;

/// Bits holding the reference count.
const REF_COUNT_MASK: usize = !STATE_MASK;

/// How far the reference count is shifted up.
const REF_COUNT_SHIFT: usize = REF_COUNT_MASK.count_zeros() as usize;

/// One reference.
const REF_ONE: usize = 1 << REF_COUNT_SHIFT;

/// The state a task is created with.
///
/// It starts with three references: one for the `OwnedTasks` it is about to be
/// registered in, one for the `Runnable` handed to the run queue, and one for
/// the `JoinHandle`. The latter two are reflected in `NOTIFIED` and
/// `JOIN_INTEREST`; `OWNED` is set by the registry once the task is in it.
const INITIAL_STATE: usize = (REF_ONE * 3) | JOIN_INTEREST | NOTIFIED;

/// The outcome of taking the poll lock.
#[must_use]
pub(super) enum TransitionToRunning {
    /// The lock was taken; poll the future.
    Polled,
    /// The lock was taken, but the task has been cancelled; kill it instead.
    Cancelled,
    /// The task is already running or already complete. Drop the reference.
    Dead,
}

/// The outcome of releasing the poll lock after a `Pending` poll.
#[must_use]
pub(super) enum TransitionToIdle {
    /// Nothing happened during the poll. Drop the reference.
    Idle,
    /// The task was woken while it was being polled. No reference was
    /// minted for this: the one the poll was holding becomes the new
    /// `Runnable`'s, so queue it and stop, without a matching drop.
    Notified,
    /// The task was cancelled while it was being polled. Kill it.
    Cancelled,
}

/// What happens to the caller's own reference when notifying a task.
pub(super) enum CallerRef {
    /// The caller keeps using its reference afterward, so a notification
    /// that finds the task idle must mint a new one for the `Runnable`.
    Kept,
    /// The caller is giving up its reference as part of this call. A
    /// notification that finds the task idle mints nothing: that very
    /// reference becomes the `Runnable`'s.
    Consumed,
}

impl State {
    /// Returns the state a new task starts in.
    pub(super) fn new() -> State {
        State {
            val: Cell::new(INITIAL_STATE),
        }
    }

    #[inline]
    pub(super) fn load(&self) -> Snapshot {
        Snapshot(self.val.get())
    }

    #[inline]
    fn store(&self, snapshot: Snapshot) {
        self.val.set(snapshot.0);
    }

    /// Takes the poll lock, consuming the reference that grants the right to
    /// poll. That reference is held by a `Runnable`, or by the registry when a
    /// task is being shut down.
    ///
    /// `NOTIFIED` is cleared so that wakes arriving during the poll can be
    /// told apart from the one that led here.
    pub(super) fn transition_to_running(&self) -> TransitionToRunning {
        let mut next = self.load();

        if !next.is_idle() {
            return TransitionToRunning::Dead;
        }

        next.set_running();
        next.unset_notified();
        self.store(next);

        if next.is_cancelled() {
            TransitionToRunning::Cancelled
        } else {
            TransitionToRunning::Polled
        }
    }

    /// Releases the poll lock after the future returned `Pending`.
    ///
    /// A wake that lands while the task is running mints no reference of its
    /// own: the poll lock's own reference simply becomes the next
    /// `Runnable`'s, since nothing else needs it once the poll returns. That
    /// is what [`TransitionToIdle::Notified`] hands back — a signal to
    /// requeue with the same reference, not a fresh one to drop afterward.
    pub(super) fn transition_to_idle(&self) -> TransitionToIdle {
        let mut next = self.load();
        debug_assert!(next.is_running());

        if next.is_cancelled() {
            return TransitionToIdle::Cancelled;
        }

        next.unset_running();
        self.store(next);

        if next.is_notified() {
            TransitionToIdle::Notified
        } else {
            TransitionToIdle::Idle
        }
    }

    /// Marks the task complete, returning the state it settled in.
    pub(super) fn transition_to_complete(&self) -> Snapshot {
        let mut next = self.load();
        debug_assert!(next.is_running());
        debug_assert!(!next.is_complete());

        next.unset_running();
        next.set_complete();
        self.store(next);

        next
    }

    /// Drops `count` references at once, returning `true` if that was the last
    /// of them and the task should be deallocated.
    ///
    /// Completion releases the references held by the registry and by the
    /// `Runnable` together, and must not let the task be deallocated part way
    /// through.
    pub(super) fn transition_to_terminal(&self, count: usize) -> bool {
        let prev = self.load();
        debug_assert!(prev.ref_count() >= count);

        self.store(Snapshot(prev.0 - count * REF_ONE));

        prev.ref_count() == count
    }

    /// Marks the task notified, returning `true` if the caller should queue a
    /// `Runnable` for it.
    ///
    /// If that is idle-triggered, `caller_ref` decides where its reference
    /// comes from: a fresh one is minted unless the caller is [handing over
    /// its own](CallerRef::Consumed), in which case nothing is minted and
    /// that reference is what the caller must schedule.
    pub(super) fn transition_to_notified(&self, caller_ref: CallerRef) -> bool {
        let mut next = self.load();

        if next.is_complete() || next.is_notified() {
            return false;
        }

        next.set_notified();
        let was_idle = !next.is_running();

        if was_idle && matches!(caller_ref, CallerRef::Kept) {
            next.ref_inc();
        }

        self.store(next);
        was_idle
    }

    /// Requests that the task be cancelled. The task itself acts on this the
    /// next time it takes or releases the poll lock.
    pub(super) fn set_cancelled(&self) {
        let mut next = self.load();
        next.set_cancelled();
        self.store(next);
    }

    /// Records that the task has been registered in an `OwnedTasks`.
    pub(super) fn set_owned(&self) {
        let mut next = self.load();
        debug_assert!(!next.is_owned());
        next.set_owned();
        self.store(next);
    }

    /// Records that the task has left its `OwnedTasks`, returning `true` if it
    /// was in one. A task is removed at most once, so only the call that sees
    /// `true` may touch the registry slot.
    pub(super) fn unset_owned(&self) -> bool {
        let mut next = self.load();

        if !next.is_owned() {
            return false;
        }

        next.unset_owned();
        self.store(next);

        true
    }

    /// Unsets `JOIN_INTEREST`, returning `true` if the `JoinHandle` is the one
    /// that has to drop the task's output, which is the case when the task has
    /// already stored it.
    pub(super) fn transition_to_join_handle_dropped(&self) -> bool {
        let mut next = self.load();
        debug_assert!(next.is_join_interested());

        next.unset_join_interested();
        self.store(next);

        next.is_complete()
    }

    #[inline]
    pub(super) fn ref_inc(&self) {
        let mut next = self.load();
        next.ref_inc();
        self.store(next);
    }

    /// Drops a reference, returning `true` if it was the last one.
    #[inline]
    pub(super) fn ref_dec(&self) -> bool {
        let mut next = self.load();
        next.ref_dec();
        self.store(next);

        next.ref_count() == 0
    }
}

impl Snapshot {
    /// Returns `true` if the task is neither being polled nor finished, and so
    /// can have the poll lock taken.
    pub(super) fn is_idle(self) -> bool {
        self.0 & (RUNNING | COMPLETE) == 0
    }

    pub(super) fn is_running(self) -> bool {
        self.0 & RUNNING != 0
    }

    fn set_running(&mut self) {
        self.0 |= RUNNING;
    }

    fn unset_running(&mut self) {
        self.0 &= !RUNNING;
    }

    /// Returns `true` if the future has finished and its result is stored.
    pub(super) fn is_complete(self) -> bool {
        self.0 & COMPLETE != 0
    }

    fn set_complete(&mut self) {
        self.0 |= COMPLETE;
    }

    fn is_notified(self) -> bool {
        self.0 & NOTIFIED != 0
    }

    fn set_notified(&mut self) {
        self.0 |= NOTIFIED;
    }

    fn unset_notified(&mut self) {
        self.0 &= !NOTIFIED;
    }

    fn is_cancelled(self) -> bool {
        self.0 & CANCELLED != 0
    }

    fn set_cancelled(&mut self) {
        self.0 |= CANCELLED;
    }

    pub(super) fn is_join_interested(self) -> bool {
        self.0 & JOIN_INTEREST != 0
    }

    fn unset_join_interested(&mut self) {
        self.0 &= !JOIN_INTEREST;
    }

    pub(super) fn is_owned(self) -> bool {
        self.0 & OWNED != 0
    }

    fn set_owned(&mut self) {
        self.0 |= OWNED;
    }

    fn unset_owned(&mut self) {
        self.0 &= !OWNED;
    }

    fn ref_count(self) -> usize {
        (self.0 & REF_COUNT_MASK) >> REF_COUNT_SHIFT
    }

    // The reference count occupies all but the low six bits of a `usize`, and
    // every reference is a live handle of at least a word, so on a 64 bit
    // target overflow would need more memory than can be addressed. On a 32 bit
    // target the ceiling is low enough to be worth a real check.
    fn ref_inc(&mut self) {
        debug_assert!(self.ref_count() < REF_COUNT_MASK >> REF_COUNT_SHIFT);
        self.0 += REF_ONE;
    }

    fn ref_dec(&mut self) {
        debug_assert!(self.ref_count() > 0);
        self.0 -= REF_ONE;
    }
}

impl fmt::Debug for State {
    fn fmt(&self, fmt: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.load().fmt(fmt)
    }
}

impl fmt::Debug for Snapshot {
    fn fmt(&self, fmt: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt.debug_struct("Snapshot")
            .field("is_running", &self.is_running())
            .field("is_complete", &self.is_complete())
            .field("is_notified", &self.is_notified())
            .field("is_cancelled", &self.is_cancelled())
            .field("is_join_interested", &self.is_join_interested())
            .field("is_owned", &self.is_owned())
            .field("ref_count", &self.ref_count())
            .finish()
    }
}
