//! The io_uring driver: one ring, its direct descriptor table, and the
//! operations in flight on it.
//!
//! # Why every turn is a syscall
//!
//! The ring is created with [`sys::SetupFlags::DEFER_TASKRUN`], which holds
//! completion work until the owning thread next enters the kernel. That is what
//! makes completions arrive in batches on our own schedule rather than at
//! interrupt time, and it is why the ring is bound to one thread.
//!
//! It also means there is no userspace signal that completions are waiting. The
//! kernel raises [`sys::SqRingFlags::TASKRUN`] only from its ordinary task-work
//! path, never from the deferred one, so an empty completion ring proves
//! nothing — the only way to find out is to call `io_uring_enter` with
//! [`sys::EnterFlags::GETEVENTS`]. [`Driver::turn`] therefore always enters
//! while anything is outstanding, and the executor's polling budget is what
//! keeps the rate of those calls bounded.
//!
//! # Submission
//!
//! Entries are written straight into the submission ring, and the tail is
//! published once per turn rather than once per operation: without SQPOLL the
//! kernel only reads it inside `io_uring_enter`, so the intervening stores would
//! be for nobody's benefit.
pub(crate) mod ledger;
pub(crate) mod op;

use std::cell::RefCell;
use std::cmp;
use std::collections::VecDeque;
use std::ffi::c_void;
use std::ptr;
use std::time::Duration;

use crate::io_uring::fixed::FixedFiles;
use crate::io_uring::op as uring_op;
use crate::io_uring::ring::Ring;
use crate::io_uring::sys;
use crate::io_uring::syscall::{self, Errno};

use self::ledger::{Discard, Ledger, OnComplete, OpKey, Outcome};

/// Submission ring entries. Small on purpose: the backlog path is a correctness
/// requirement rather than an edge case, so it should be exercised constantly
/// instead of lying dormant until production.
const SQ_ENTRIES: u32 = 64;

/// Completion ring entries. Twice the submission ring, so an ordinary batch
/// lands without overflowing into the kernel's side list.
const CQ_ENTRIES: u32 = 128;

/// Direct descriptor slots. Fixed at registration — the table cannot grow —
/// so this is the connection ceiling.
const FILE_SLOTS: u32 = 1024;

/// Slots left to the kernel's own allocator.
const KERNEL_SLOTS: u32 = 256;

/// The minimum amount of time the program will wait in io_uring_enter if there was no more work to do.
const MIN_WAIT_US: u32 = 25;

/// What [`Driver::turn`] should do once it has submitted what is queued.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Turn {
    /// Collect whatever has already completed and come straight back. For when
    /// there is other work to get on with.
    Flush,

    /// Block until at least one completion arrives.
    ///
    /// The caller must have checked that something is actually outstanding.
    Wait,

    /// Block until a completion arrives or `timeout` elapses, whichever comes
    /// first.
    WaitFor(Duration),
}

impl Turn {
    fn blocks(self) -> bool {
        !matches!(self, Turn::Flush)
    }
}

// ---------------------------------------------------------------------------
// Driver
// ---------------------------------------------------------------------------

pub(crate) struct Driver {
    /// Every method on it takes `&self`, so it needs no cell of its own.
    ring: Ring,
    files: RefCell<FixedFiles>,
    sub: RefCell<Submission>,
    ledger: RefCell<Ledger>,
}

struct Submission {
    /// Written up to, but not yet published to the kernel.
    tail: u32,
    /// Entries that did not fit in the ring, in submission order.
    backlog: VecDeque<sys::Sqe>,
}

impl Driver {
    pub(crate) fn new() -> Result<Driver, Errno> {
        Driver::with_capacity(SQ_ENTRIES, CQ_ENTRIES, FILE_SLOTS, KERNEL_SLOTS)
    }

    fn with_capacity(
        sq_entries: u32,
        cq_entries: u32,
        file_slots: u32,
        kernel_slots: u32,
    ) -> Result<Driver, Errno> {
        let mut params = sys::Params {
            flags: sys::SetupFlags::SINGLE_ISSUER
                | sys::SetupFlags::DEFER_TASKRUN
                | sys::SetupFlags::NO_SQARRAY
                | sys::SetupFlags::CQSIZE
                // Without this the kernel stops consuming a batch at the first
                // malformed entry, turning one bad SQE into a stall.
                | sys::SetupFlags::SUBMIT_ALL,
            cq_entries,
            ..sys::Params::default()
        };

        let mut ring = Ring::with_params(sq_entries, &mut params)?;
        // Saves a file table lookup on every enter. A kernel that refuses it
        // costs us nothing but the lookup.
        ring.register_ring_fd().ok();

        let files = FixedFiles::register(&ring, file_slots, kernel_slots)?;
        let tail = ring.sq().tail();

        Ok(Driver {
            ring,
            files: RefCell::new(files),
            sub: RefCell::new(Submission {
                tail,
                backlog: VecDeque::with_capacity(sq_entries as usize),
            }),
            ledger: RefCell::new(Ledger::with_capacity(sq_entries as usize)),
        })
    }

    // -----------------------------------------------------------------------
    // Queries
    // -----------------------------------------------------------------------

    /// Completions the kernel still owes us.
    ///
    /// The predicate for whether blocking is safe, which is why it counts what
    /// the kernel owes rather than what the ledger holds: a result that has
    /// already arrived and is waiting to be collected will never be delivered a
    /// second time, so waiting on one would wait for ever.
    pub(crate) fn in_flight(&self) -> u32 {
        self.ledger.borrow().outstanding()
    }

    /// Whether entries are waiting for room in the submission ring.
    ///
    /// The executor checks this after every task it polls: the ring filling is
    /// the signal that we are producing faster than one turn per budget can
    /// drain, and the right response is to go turn the reactor now.
    pub(crate) fn backlog_pending(&self) -> bool {
        !self.sub.borrow().backlog.is_empty()
    }

    /// Reserve a direct descriptor slot, or `None` when the table is full.
    pub(crate) fn alloc_slot(&self) -> Option<u32> {
        self.files.borrow_mut().alloc()
    }

    /// Return a slot to the table. Call this when the operation that emptied it
    /// *completes*, never when it is submitted.
    pub(crate) fn free_slot(&self, slot: u32) {
        self.files.borrow_mut().free(slot);
    }

    /// Slots currently handed out.
    ///
    /// The number every cleanup path is trying to keep honest: a slot that is
    /// never given back is a connection nothing can reach and a seat nothing
    /// else can take. Only the tests read it so far.
    #[cfg(test)]
    pub(crate) fn slots_used(&self) -> u32 {
        self.files.borrow().used()
    }

    // -----------------------------------------------------------------------
    // Submission
    // -----------------------------------------------------------------------

    /// Submit one operation and take a ledger entry for its result.
    ///
    /// `prep` fills the reserved slot in place — see [`Driver::fill`].
    /// `sqe_flags` is ORed onto whatever the prep function set.
    pub(crate) fn submit<F>(&self, sqe_flags: sys::SqeFlags, prep: F) -> OpKey
    where
        F: FnOnce(uring_op::Slot<'_>),
    {
        let key = self.ledger.borrow_mut().submit();
        let slot = self.fill();
        let sqe = slot.as_raw();
        prep(slot);
        unsafe {
            (*sqe).flags = sqe_flags;
            (*sqe).user_data = key.user_data();
        }
        key
    }

    /// Submit a chain of linked operations, which run in order and stop at the
    /// first failure.
    ///
    /// `prep` fills all `N` reserved slots; this then ORs `per_entry_flags` on
    /// and adds [`sys::SqeFlags::IO_LINK`] to every entry but the last — the
    /// links are what make it a chain, and keeping the whole run in one
    /// submission is what stops a dangling link head from splitting it. Each
    /// member gets its own ledger entry and posts exactly one completion: its
    /// own result, or `ECANCELED` once an earlier one failed.
    pub(crate) fn submit_chain<const N: usize, F>(
        &self,
        per_entry_flags: [sys::SqeFlags; N],
        prep: F,
    ) -> [OpKey; N]
    where
        F: FnOnce([uring_op::Slot<'_>; N]),
    {
        assert!(
            N <= self.ring.sq().entries() as usize,
            "a chain of {N} does not fit a submission ring of {}",
            self.ring.sq().entries(),
        );

        let keys: [OpKey; N] = {
            let mut ledger = self.ledger.borrow_mut();
            std::array::from_fn(|_| ledger.submit())
        };

        let mut sub = self.sub.borrow_mut();

        // All of it goes to the ring, or all of it to the backlog as one block —
        // never split.
        let contiguous = sub.backlog.is_empty() && self.space_left(&sub) as usize >= N;

        let ptrs: [*mut sys::Sqe; N] = if contiguous {
            // SAFETY: `space_left >= N`, so `tail + i` is a free slot. Each
            // pointer derives from the same SQE array base.
            std::array::from_fn(|i| unsafe { self.ring.sq().sqe(sub.tail.wrapping_add(i as u32)) })
        } else {
            for _ in 0..N {
                sub.backlog.push_back(sys::Sqe::ZEROED);
            }
            let n = sub.backlog.len();
            let mut fresh = sub.backlog.make_contiguous()[n - N..].iter_mut();
            std::array::from_fn(|_| fresh.next().expect("N entries just pushed") as *mut sys::Sqe)
        };

        // SAFETY: each pointer names a distinct, writable slot that stays valid
        // for this borrow.
        prep(ptrs.map(|p| unsafe { uring_op::Slot::from_raw(p) }));

        for i in 0..N {
            let link = if i + 1 < N {
                sys::SqeFlags::IO_LINK
            } else {
                sys::SqeFlags::empty()
            };
            // SAFETY: `prep` fully initialised each slot bar `user_data`.
            unsafe {
                (*ptrs[i]).flags |= per_entry_flags[i] | link;
                (*ptrs[i]).user_data = keys[i].user_data();
            }
        }

        if contiguous {
            sub.tail = sub.tail.wrapping_add(N as u32);
        }

        keys
    }

    /// Submit an operation nobody will wait for, with an action to run when it
    /// lands.
    pub(crate) fn submit_detached(
        &self,
        sqe_flags: sys::SqeFlags,
        prep: impl FnOnce(uring_op::Slot<'_>),
        action: Box<dyn OnComplete>,
    ) {
        let key = self.ledger.borrow_mut().submit_detached(action);
        let slot = self.fill();
        let sqe = slot.as_raw();
        prep(slot);
        unsafe {
            (*sqe).flags = sqe_flags;
            (*sqe).user_data = key.user_data();
        }
    }

    /// Ask the kernel to stop the operation `key` names.
    ///
    /// Best effort by nature: it may already have completed, in which case the
    /// cancel completes with `ENOENT` and the original result stands.
    pub(crate) fn cancel(&self, key: OpKey) {
        self.submit_detached(
            sys::SqeFlags::empty(),
            |slot| uring_op::prep_cancel(slot, key.user_data(), sys::AsyncCancelFlags::empty()),
            Box::new(Discard),
        );
    }

    /// Stop every operation still in flight on `slot`, in one entry.
    pub(crate) fn cancel_slot(&self, slot: u32) {
        self.submit_detached(
            sys::SqeFlags::empty(),
            |s| {
                uring_op::prep_cancel_fd(
                    s,
                    slot as i32,
                    sys::AsyncCancelFlags::FD
                        | sys::AsyncCancelFlags::FD_FIXED
                        | sys::AsyncCancelFlags::ALL,
                )
            },
            Box::new(Discard),
        );
    }

    // -----------------------------------------------------------------------
    // Ledger access
    //
    // Every one of these keeps its borrow inside itself. Callers run arbitrary
    // code with the result — waking, cleaning up, submitting more — and none of
    // that may happen while the ledger is borrowed.
    // -----------------------------------------------------------------------

    /// Take an operation's result, or register `waker` to be told when it lands.
    pub(crate) fn poll_op(
        &self,
        key: OpKey,
        waker: &std::task::LocalWaker,
    ) -> std::task::Poll<(i32, sys::CqeFlags)> {
        self.ledger.borrow_mut().poll(key, waker)
    }

    /// Take an operation's result if it has already arrived.
    pub(crate) fn take_completed(&self, key: OpKey) -> Option<(i32, sys::CqeFlags)> {
        self.ledger.borrow_mut().take_completed(key)
    }

    /// Leave `action` for a running operation's completion to find.
    pub(crate) fn detach(&self, key: OpKey, action: Box<dyn OnComplete>) {
        self.ledger.borrow_mut().detach(key, action);
    }

    /// Reserve a submission slot and let `prep` build the entry in it.
    ///
    /// The slot is in the entry's final home — a ring slot while there is room,
    /// a fresh backlog slot once the ring is full or the backlog is non-empty —
    /// so the SQE is constructed once, where it stays. `prep` initialises the
    /// whole entry bar `user_data`; this then ORs `sqe_flags` on and stamps
    /// `user_data`. `prep` must not re-enter the driver: the submission ring is
    /// borrowed for the duration.
    fn fill(&self) -> uring_op::Slot<'_> {
        let mut sub = self.sub.borrow_mut();

        if !sub.backlog.is_empty() || self.space_left(&sub) == 0 {
            sub.backlog.push_back(sys::Sqe::ZEROED);
            let ptr = sub.backlog.back_mut().expect("just pushed") as *mut sys::Sqe;
            let slot = unsafe { uring_op::Slot::from_raw(ptr) };
            return slot;
        }

        // SAFETY: `space_left` is computed against the kernel's head, so this
        // slot is at or above the last published tail and not one the kernel is
        // reading.
        let ptr = unsafe { self.ring.sq().sqe(sub.tail) };
        let slot = unsafe { uring_op::Slot::from_raw(ptr) };
        sub.tail = sub.tail.wrapping_add(1);
        slot
    }

    /// Room in the ring, measured against our unpublished tail rather than the
    /// ring's own, which lags by up to a turn.
    fn space_left(&self, sub: &Submission) -> u32 {
        let sq = self.ring.sq();
        sq.entries() - sub.tail.wrapping_sub(sq.head())
    }

    /// Move as much of the backlog into the ring as fits, a chain at a time.
    fn flush_backlog(&self, sub: &mut Submission) {
        while !sub.backlog.is_empty() {
            let run = leading_chain_len(&sub.backlog);
            if self.space_left(sub) < run {
                break;
            }
            for _ in 0..run {
                let sqe = sub.backlog.pop_front().expect("counted just above");
                // SAFETY: as in `write`; the run was checked to fit.
                unsafe { self.ring.sq().sqe(sub.tail).write(sqe) };
                sub.tail = sub.tail.wrapping_add(1);
            }
        }
    }

    // -----------------------------------------------------------------------
    // Turning the reactor
    // -----------------------------------------------------------------------

    /// Submit what is queued and collect whatever has completed.
    ///
    /// See [`Turn`] for what the three modes cost and what each of them
    /// requires of the caller.
    ///
    /// "Submit" means queued for the kernel, not handed to it: a backlog too
    /// small to fill the ring is left in the ring for the next call to take out.
    /// Callers that need everything through — [`Driver::shutdown`] and the like
    /// — already loop until nothing is outstanding, which covers it.
    pub(crate) fn turn(&self, turn: Turn) -> Result<(), Errno> {
        let mut turn = turn;

        loop {
            let backlog_before = self.sub.borrow().backlog.len();

            let to_submit = {
                let mut sub = self.sub.borrow_mut();
                self.flush_backlog(&mut sub);
                let sq = self.ring.sq();
                sq.set_tail(sub.tail);
                sub.tail.wrapping_sub(sq.head())
            };

            // Nothing queued, nobody to wait for, nothing outstanding: entering
            // the kernel could not tell us anything.
            if to_submit == 0 && !turn.blocks() && self.in_flight() == 0 {
                return Ok(());
            }

            self.enter(to_submit, turn)?;
            self.reap();

            let mut sub = self.sub.borrow_mut();
            let backlog_after = sub.backlog.len();
            if backlog_after == 0 {
                return Ok(());
            }

            if backlog_after <= self.space_left(&sub) as usize / 4 {
                self.flush_backlog(&mut sub);
                return Ok(());
            }

            // One task can fill the ring several times over in a single poll, so
            // keep going while the backlog is actually shrinking. It stops
            // shrinking when the kernel is not consuming, and then the answer is
            // to reap rather than to push harder.
            if backlog_after >= backlog_before {
                return Ok(());
            }

            drop(sub);
            // Whatever we were waiting for either arrived or timed out; the
            // remaining passes are only here to push the backlog through.
            turn = Turn::Flush;
        }
    }

    /// One `io_uring_enter`, with the wait bounded as `turn` asks.
    fn enter(&self, to_submit: u32, turn: Turn) -> Result<(), Errno> {
        let flags = self.ring.enter_flags() | sys::EnterFlags::GETEVENTS;
        let min_complete = turn.blocks() as u32;

        let entered = match turn {
            Turn::Flush | Turn::Wait => {
                // SAFETY: no argument is passed, so `arg` and `argsz` are
                // trivially valid for these flags.
                unsafe {
                    syscall::io_uring_enter(
                        self.ring.enter_fd(),
                        to_submit,
                        min_complete,
                        flags,
                        ptr::null(),
                        0,
                    )
                }
            }
            Turn::WaitFor(timeout) => {
                let mut ts = sys::Timespec {
                    tv_sec: timeout.as_secs() as i64,
                    tv_nsec: timeout.subsec_nanos() as i64,
                };
                if ts.tv_sec == 0 {
                    ts.tv_nsec = cmp::max(ts.tv_nsec, MIN_WAIT_US as i64 * 1000);
                }
                let arg = sys::GeteventsArg {
                    sigmask: 0,
                    sigmask_sz: 0,
                    min_wait_usec: MIN_WAIT_US,
                    ts: &raw const ts as u64,
                };

                // SAFETY: `arg` is a live `GeteventsArg` for the duration of the
                // call and `EXT_ARG` is set to say so.
                unsafe {
                    syscall::io_uring_enter(
                        self.ring.enter_fd(),
                        to_submit,
                        min_complete,
                        flags | sys::EnterFlags::EXT_ARG,
                        &raw const arg as *const c_void,
                        size_of::<sys::GeteventsArg>(),
                    )
                }
            }
        };

        match entered {
            Ok(_) => Ok(()),
            // Interrupted, nothing ready, no room to post more completions, or
            // the wait timed out.
            Err(Errno::INTR | Errno::AGAIN | Errno::BUSY | Errno::TIME) => Ok(()),
            Err(e) => Err(e),
        }
    }

    /// Deliver every completion the ring is holding.
    fn reap(&self) {
        for completion in self.ring.cq().completions() {
            let res = completion.raw().res;
            let flags = completion.flags();

            // The borrow ends with this statement, deliberately: waking a task
            // and running a detached action both re-enter the driver.
            let outcome = self
                .ledger
                .borrow_mut()
                .complete(completion.user_data(), res, flags);

            match outcome {
                Outcome::Ignore => {}
                Outcome::Wake(waker) => waker.wake(),
                Outcome::Detached(action, res, flags) => action.on_complete(self, res, flags),
            }
        }
    }

    /// Cancel everything and wait for the kernel to give it all back.
    ///
    /// Buffers handed to the kernel are owned by detached ledger entries, so
    /// they live exactly as long as the operations pointing at them — but only
    /// if we stay here until those operations finish. Returning early would drop
    /// the ring, and its mappings, with the kernel still writing into them.
    pub(crate) fn shutdown(&self) {
        for key in self.ledger.borrow_mut().abandon_all() {
            self.cancel(key);
        }

        while self.in_flight() > 0 {
            if self.turn(Turn::Wait).is_err() {
                break;
            }
        }
    }
}

fn is_linked(sqe: &sys::Sqe) -> bool {
    sqe.flags
        .intersects(sys::SqeFlags::IO_LINK | sys::SqeFlags::IO_HARDLINK)
}

/// How many entries at the front of the backlog have to go out together.
///
/// A link chain has to reach the kernel in one submission. At the end of a
/// batch the kernel issues a dangling link head immediately rather than holding
/// it for the next one, so a chain split across two calls does not stall — it
/// silently becomes two unrelated runs, and the second one starts against
/// whatever state the first left.
fn leading_chain_len(backlog: &VecDeque<sys::Sqe>) -> u32 {
    let mut run = 0;
    for sqe in backlog {
        run += 1;
        if !is_linked(sqe) {
            return run;
        }
    }

    // Chains are only ever appended whole, so the terminator is always there.
    debug_assert!(false, "the backlog ends in the middle of a chain");
    run
}

/// A local waker that does nothing, for tests that only care about the state
/// machine either side of it.
#[cfg(test)]
pub(crate) fn noop_local_waker() -> std::task::LocalWaker {
    use std::task::{LocalWaker, RawWaker, RawWakerVTable};

    static VTABLE: RawWakerVTable = RawWakerVTable::new(
        |_| RawWaker::new(ptr::null(), &VTABLE),
        |_| {},
        |_| {},
        |_| {},
    );

    // SAFETY: the vtable ignores its pointer entirely.
    unsafe { LocalWaker::from_raw(RawWaker::new(ptr::null(), &VTABLE)) }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn nop() -> sys::Sqe {
        let mut sqe = sys::Sqe::ZEROED;
        // SAFETY: `sqe` outlives the slot.
        uring_op::prep_nop(unsafe { uring_op::Slot::from_raw(&mut sqe) });
        sqe
    }

    fn linked_nop() -> sys::Sqe {
        let mut sqe = nop();
        sqe.flags |= sys::SqeFlags::IO_LINK;
        sqe
    }

    /// Fill a slot with a `Nop`, for the submit paths that take a prep closure.
    fn prep_nop(slot: uring_op::Slot<'_>) {
        uring_op::prep_nop(slot);
    }

    /// A small ring, so the paths that only open up under pressure are the
    /// ordinary case here.
    fn driver() -> Driver {
        Driver::with_capacity(8, 16, 16, 0).expect("Driver::with_capacity")
    }

    /// Drain, without waiting on a ring that has nothing left to give.
    fn drain(driver: &Driver) {
        while driver.in_flight() > 0 {
            driver.turn(Turn::Wait).expect("turn");
        }
    }

    #[test]
    fn a_batch_larger_than_the_ring_still_all_completes() {
        let driver = driver();
        const N: u32 = 100;

        for _ in 0..N {
            driver.submit_detached(sys::SqeFlags::empty(), prep_nop, Box::new(Discard));
        }

        assert_eq!(driver.in_flight(), N);
        assert!(
            driver.backlog_pending(),
            "a hundred entries do not fit a ring of eight"
        );

        drain(&driver);
        assert!(!driver.backlog_pending(), "the backlog drained with them");
    }

    /// Draining stops one short. The batch that fills the ring goes out, and the
    /// remainder — being smaller than the ring — is moved into it and left
    /// there, unsubmitted, rather than being given an `io_uring_enter` of its
    /// own. Emptying the backlog is what lets the executor go back to producing
    /// the entries the next turn will carry them out with.
    #[test]
    fn a_remainder_that_fits_the_ring_waits_for_the_next_turn() {
        let driver = driver();

        for _ in 0..10 {
            driver.submit_detached(sys::SqeFlags::empty(), prep_nop, Box::new(Discard));
        }
        assert!(
            driver.backlog_pending(),
            "four of twelve do not fit a ring of eight"
        );

        driver.turn(Turn::Flush).expect("turn");

        assert!(
            !driver.backlog_pending(),
            "the remainder moved into the ring"
        );
        assert_eq!(driver.in_flight(), 2, "and is sitting there, still ours");

        drain(&driver);
    }

    /// More completions than the completion ring holds, so the kernel has to
    /// park the rest in its overflow list and hand them over on a later enter.
    #[test]
    fn completions_beyond_the_ring_are_not_lost() {
        let driver = driver();
        const N: u32 = 200;

        for _ in 0..N {
            driver.submit_detached(sys::SqeFlags::empty(), prep_nop, Box::new(Discard));
        }
        drain(&driver);

        assert_eq!(driver.in_flight(), 0, "every completion was accounted for");
    }

    /// The rule the whole chain-aware backlog exists for. A dangling link head
    /// at the end of a submission is issued immediately rather than held, so a
    /// chain split across two enters silently becomes two unrelated runs.
    #[test]
    fn a_chain_that_does_not_fit_waits_rather_than_being_split() {
        let driver = driver();

        // Six of eight slots taken, so three will not fit.
        for _ in 0..6 {
            driver.submit_detached(sys::SqeFlags::empty(), prep_nop, Box::new(Discard));
        }
        assert!(!driver.backlog_pending(), "six of eight fit");

        let keys = driver.submit_chain([sys::SqeFlags::empty(); 3], |[a, b, c]| {
            prep_nop(a);
            prep_nop(b);
            prep_nop(c);
        });
        assert!(
            driver.backlog_pending(),
            "the chain went to the backlog whole rather than filling the last two slots"
        );

        drain(&driver);
        assert!(!driver.backlog_pending());

        // Collect the results the way a `Chain` future would; an entry nobody
        // collects stays occupied by design.
        for key in keys {
            assert!(
                driver.take_completed(key).is_some(),
                "every member of the chain ran"
            );
        }
    }

    #[test]
    fn a_chain_is_counted_up_to_and_including_its_terminator() {
        let mut backlog = VecDeque::new();
        backlog.push_back(linked_nop());
        backlog.push_back(linked_nop());
        backlog.push_back(nop());
        backlog.push_back(nop());

        assert_eq!(
            leading_chain_len(&backlog),
            3,
            "two linked entries and the one that ends them"
        );
    }

    #[test]
    fn an_unlinked_entry_is_a_run_of_one() {
        let mut backlog = VecDeque::new();
        backlog.push_back(nop());
        backlog.push_back(nop());

        assert_eq!(leading_chain_len(&backlog), 1);
    }

    /// With nothing submitted and nothing outstanding there is nothing an
    /// `io_uring_enter` could tell us, so it is skipped.
    #[test]
    fn an_idle_turn_is_free() {
        let driver = driver();

        driver.turn(Turn::Flush).expect("turn");
        assert_eq!(driver.in_flight(), 0);
    }
}
