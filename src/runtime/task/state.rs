use std::cell::Cell;
use std::fmt;

/// The task state.
///
/// Unlike `tokio`, this is a plain `Cell<usize>` rather than an `AtomicUsize`:
/// a task is only ever touched from the thread that owns the runtime, so every
/// transition is a load, a bit of arithmetic and a store. There are no CAS
/// loops, which means each transition below reads as straight-line code.
pub(super) struct State {
    val: Cell<usize>,
}

/// Current state value.
#[derive(Copy, Clone)]
pub(super) struct Snapshot(usize);

/// The task is currently being run.
const RUNNING: usize = 0b0001;

/// The task is complete.
///
/// Once this bit is set, it is never unset.
const COMPLETE: usize = 0b0010;

/// Extracts the task's lifecycle value from the state.
const LIFECYCLE_MASK: usize = 0b11;

/// Flag tracking if the task has been pushed into a run queue.
const NOTIFIED: usize = 0b100;

/// The join handle is still around.
const JOIN_INTEREST: usize = 0b1_000;

/// The task has been forcibly cancelled.
const CANCELLED: usize = 0b10_000;

/// All bits.
const STATE_MASK: usize = LIFECYCLE_MASK | NOTIFIED | JOIN_INTEREST | CANCELLED;

/// Bits used by the ref count portion of the state.
const REF_COUNT_MASK: usize = !STATE_MASK;

/// Number of positions to shift the ref count.
const REF_COUNT_SHIFT: usize = REF_COUNT_MASK.count_zeros() as usize;

/// One ref count.
const REF_ONE: usize = 1 << REF_COUNT_SHIFT;

/// State a task is initialized with.
///
/// A task is initialized with three references:
///
///  * A reference that will be stored in an `OwnedTasks`.
///  * A reference that will be sent to the scheduler as an ordinary notification.
///  * A reference for the `JoinHandle`.
///
/// As the task starts with a `JoinHandle`, `JOIN_INTEREST` is set.
/// As the task starts with a `Runnable`, `NOTIFIED` is set.
const INITIAL_STATE: usize = (REF_ONE * 3) | JOIN_INTEREST | NOTIFIED;

#[must_use]
pub(super) enum TransitionToRunning {
    Success,
    Cancelled,
    Failed,
    Dealloc,
}

#[must_use]
pub(super) enum TransitionToIdle {
    Ok,
    OkNotified,
    OkDealloc,
    Cancelled,
}

#[must_use]
pub(super) enum TransitionToNotifiedByVal {
    DoNothing,
    Submit,
    Dealloc,
}

#[must_use]
pub(super) enum TransitionToNotifiedByRef {
    DoNothing,
    Submit,
}

impl State {
    /// Returns a task's initial state.
    pub(super) fn new() -> State {
        // The raw task returned by this method has a ref-count of three. See
        // the comment on INITIAL_STATE for more.
        State {
            val: Cell::new(INITIAL_STATE),
        }
    }

    /// Loads the current state.
    #[inline]
    pub(super) fn load(&self) -> Snapshot {
        Snapshot(self.val.get())
    }

    #[inline]
    fn store(&self, snapshot: Snapshot) {
        self.val.set(snapshot.0);
    }

    /// Attempts to transition the lifecycle to `Running`. This sets the
    /// notified bit to false so notifications during the poll can be detected.
    pub(super) fn transition_to_running(&self) -> TransitionToRunning {
        let mut next = self.load();
        debug_assert!(next.is_notified());

        if !next.is_idle() {
            // This happens if the task is either currently running or if it
            // has already completed, e.g. if it was cancelled during
            // shutdown. Consume the ref-count and return.
            next.ref_dec();
            self.store(next);

            if next.ref_count() == 0 {
                TransitionToRunning::Dealloc
            } else {
                TransitionToRunning::Failed
            }
        } else {
            // We are able to lock the RUNNING bit.
            next.set_running();
            next.unset_notified();
            self.store(next);

            if next.is_cancelled() {
                TransitionToRunning::Cancelled
            } else {
                TransitionToRunning::Success
            }
        }
    }

    /// Transitions the task from `Running` -> `Idle`.
    ///
    /// The transition to `Idle` fails if the task has been flagged to be
    /// cancelled.
    pub(super) fn transition_to_idle(&self) -> TransitionToIdle {
        let mut next = self.load();
        debug_assert!(next.is_running());

        if next.is_cancelled() {
            return TransitionToIdle::Cancelled;
        }

        next.unset_running();

        if !next.is_notified() {
            // Polling the future consumes the ref-count of the `Runnable`.
            next.ref_dec();
            self.store(next);

            if next.ref_count() == 0 {
                TransitionToIdle::OkDealloc
            } else {
                TransitionToIdle::Ok
            }
        } else {
            // The caller will schedule a new notification, so we create a new
            // ref-count for the notification. Our own ref-count is kept for
            // now, and the caller will drop it shortly.
            next.ref_inc();
            self.store(next);

            TransitionToIdle::OkNotified
        }
    }

    /// Transitions the task from `Running` -> `Complete`.
    pub(super) fn transition_to_complete(&self) -> Snapshot {
        const DELTA: usize = RUNNING | COMPLETE;

        let prev = self.load();
        debug_assert!(prev.is_running());
        debug_assert!(!prev.is_complete());

        let next = Snapshot(prev.0 ^ DELTA);
        self.store(next);

        next
    }

    /// Transitions from `Complete` -> `Terminal`, decrementing the reference
    /// count the specified number of times.
    ///
    /// Returns true if the task should be deallocated.
    pub(super) fn transition_to_terminal(&self, count: usize) -> bool {
        let prev = self.load();
        assert!(
            prev.ref_count() >= count,
            "current: {}, sub: {}",
            prev.ref_count(),
            count
        );

        self.store(Snapshot(prev.0 - count * REF_ONE));

        prev.ref_count() == count
    }

    /// Transitions the state to `NOTIFIED`.
    ///
    /// If no task needs to be submitted, a ref-count is consumed.
    ///
    /// If a task needs to be submitted, the ref-count is incremented for the
    /// new `Runnable`.
    pub(super) fn transition_to_notified_by_val(&self) -> TransitionToNotifiedByVal {
        let mut next = self.load();

        if next.is_running() {
            // If the task is running, we mark it as notified, but we should
            // not submit anything as the poll currently in progress is
            // responsible for that.
            next.set_notified();
            next.ref_dec();

            // The caller that set the running bit also holds a ref-count.
            debug_assert!(next.ref_count() > 0);
            self.store(next);

            TransitionToNotifiedByVal::DoNothing
        } else if next.is_complete() || next.is_notified() {
            // We do not need to submit any notifications, but we have to
            // decrement the ref-count.
            next.ref_dec();
            self.store(next);

            if next.ref_count() == 0 {
                TransitionToNotifiedByVal::Dealloc
            } else {
                TransitionToNotifiedByVal::DoNothing
            }
        } else {
            // We create a new notified that we can submit. The caller retains
            // ownership of the ref-count they passed in.
            next.set_notified();
            next.ref_inc();
            self.store(next);

            TransitionToNotifiedByVal::Submit
        }
    }

    /// Transitions the state to `NOTIFIED`.
    pub(super) fn transition_to_notified_by_ref(&self) -> TransitionToNotifiedByRef {
        let mut next = self.load();

        if next.is_complete() || next.is_notified() {
            // The complete state is final, and if the task is already notified
            // there is nothing to do.
            TransitionToNotifiedByRef::DoNothing
        } else if next.is_running() {
            // If the task is running, we mark it as notified, but we should not
            // submit as the poll currently in progress is responsible for that.
            next.set_notified();
            self.store(next);

            TransitionToNotifiedByRef::DoNothing
        } else {
            // The task is idle and not notified. We should submit a
            // notification.
            next.set_notified();
            next.ref_inc();
            self.store(next);

            TransitionToNotifiedByRef::Submit
        }
    }

    /// Sets the cancelled bit and transitions the state to `NOTIFIED` if idle.
    ///
    /// Returns `true` if the task needs to be submitted to the run queue for
    /// execution.
    pub(super) fn transition_to_notified_and_cancel(&self) -> bool {
        let mut next = self.load();

        if next.is_cancelled() || next.is_complete() {
            // Aborts to completed or cancelled tasks are no-ops.
            false
        } else if next.is_running() {
            // If the task is running, we mark it as cancelled. The poll in
            // progress will notice the cancelled bit when it stops polling and
            // will kill the task.
            //
            // The set_notified() call is not strictly necessary but it will in
            // some cases let a `wake_by_ref` call return early.
            next.set_notified();
            next.set_cancelled();
            self.store(next);

            false
        } else {
            // The task is idle. We set the cancelled and notified bits and
            // submit a notification if the notified bit was not already set.
            next.set_cancelled();

            if next.is_notified() {
                self.store(next);
                false
            } else {
                next.set_notified();
                next.ref_inc();
                self.store(next);
                true
            }
        }
    }

    /// Sets the `CANCELLED` bit and attempts to transition to `Running`.
    ///
    /// Returns `true` if the transition to `Running` succeeded.
    pub(super) fn transition_to_shutdown(&self) -> bool {
        let prev = self.load();
        let mut next = prev;

        if next.is_idle() {
            next.set_running();
        }

        // If the task was not idle, the poll in progress will notice the
        // cancelled bit and cancel the task once the poll completes.
        next.set_cancelled();
        self.store(next);

        prev.is_idle()
    }

    /// Optimistically tries to swap the state assuming the join handle is
    /// __immediately__ dropped on spawn.
    pub(super) fn drop_join_handle_fast(&self) -> Result<(), ()> {
        if self.val.get() == INITIAL_STATE {
            self.val.set((INITIAL_STATE - REF_ONE) & !JOIN_INTEREST);
            Ok(())
        } else {
            Err(())
        }
    }

    /// Unsets the `JOIN_INTEREST` flag.
    ///
    /// Returns `true` if the `JoinHandle` is responsible for dropping the
    /// output of the future, which is the case when the task has already
    /// completed.
    pub(super) fn transition_to_join_handle_dropped(&self) -> bool {
        let mut next = self.load();
        debug_assert!(next.is_join_interested());

        next.unset_join_interested();
        self.store(next);

        // If `COMPLETE` is set the task has already stored its output, so the
        // `JoinHandle` is responsible for dropping it.
        next.is_complete()
    }

    #[inline]
    pub(super) fn ref_inc(&self) {
        let prev = self.val.get();

        // If the reference count overflowed, abort.
        if prev > isize::MAX as usize {
            std::process::abort();
        }

        self.val.set(prev + REF_ONE);
    }

    /// Returns `true` if the task should be released.
    #[inline]
    pub(super) fn ref_dec(&self) -> bool {
        let prev = self.load();
        debug_assert!(prev.ref_count() >= 1);
        self.store(Snapshot(prev.0 - REF_ONE));
        prev.ref_count() == 1
    }
}

// ===== impl Snapshot =====

impl Snapshot {
    /// Returns `true` if the task is in an idle state.
    pub(super) fn is_idle(self) -> bool {
        self.0 & (RUNNING | COMPLETE) == 0
    }

    /// Returns `true` if the task has been flagged as notified.
    pub(super) fn is_notified(self) -> bool {
        self.0 & NOTIFIED == NOTIFIED
    }

    fn unset_notified(&mut self) {
        self.0 &= !NOTIFIED;
    }

    fn set_notified(&mut self) {
        self.0 |= NOTIFIED;
    }

    pub(super) fn is_running(self) -> bool {
        self.0 & RUNNING == RUNNING
    }

    fn set_running(&mut self) {
        self.0 |= RUNNING;
    }

    fn unset_running(&mut self) {
        self.0 &= !RUNNING;
    }

    pub(super) fn is_cancelled(self) -> bool {
        self.0 & CANCELLED == CANCELLED
    }

    fn set_cancelled(&mut self) {
        self.0 |= CANCELLED;
    }

    /// Returns `true` if the task's future has completed execution.
    pub(super) fn is_complete(self) -> bool {
        self.0 & COMPLETE == COMPLETE
    }

    pub(super) fn is_join_interested(self) -> bool {
        self.0 & JOIN_INTEREST == JOIN_INTEREST
    }

    fn unset_join_interested(&mut self) {
        self.0 &= !JOIN_INTEREST;
    }

    pub(super) fn ref_count(self) -> usize {
        (self.0 & REF_COUNT_MASK) >> REF_COUNT_SHIFT
    }

    fn ref_inc(&mut self) {
        assert!(self.0 <= isize::MAX as usize);
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
            .field("ref_count", &self.ref_count())
            .finish()
    }
}
