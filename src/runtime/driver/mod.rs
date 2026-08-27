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
//!
//! When the ring fills, further entries go on a backlog and the executor stops
//! polling tasks to come drain it. Under load that settles into one enter per
//! full ring, which is the batching the ring size is for.

pub(crate) mod ledger;
pub(crate) mod op;

use std::cell::RefCell;
use std::collections::VecDeque;
use std::ptr;

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
    pub(crate) fn submit(&self, mut sqe: sys::Sqe) -> OpKey {
        let key = self.ledger.borrow_mut().submit();
        sqe.user_data = key.user_data();
        self.push(sqe);
        key
    }

    /// Submit a chain of linked operations, which run in order and stop at the
    /// first failure.
    ///
    /// Every entry but the last must carry [`sys::SqeFlags::IO_LINK`] or
    /// [`sys::SqeFlags::IO_HARDLINK`]; the chain is only a chain because of
    /// those flags, and this keeps the whole run together on its way to the
    /// kernel. Each member gets its own ledger entry, and each posts exactly one
    /// completion — its own result, or `ECANCELED` once an earlier one failed.
    pub(crate) fn submit_chain<const N: usize>(&self, mut sqes: [sys::Sqe; N]) -> [OpKey; N] {
        debug_assert!(
            sqes[..N - 1]
                .iter()
                .all(|sqe| is_linked(sqe) && !is_linked(&sqes[N - 1])),
            "a chain is linked on every entry but the last"
        );

        let keys: [OpKey; N] = {
            let mut ledger = self.ledger.borrow_mut();
            std::array::from_fn(|_| ledger.submit())
        };
        for (sqe, key) in sqes.iter_mut().zip(keys) {
            sqe.user_data = key.user_data();
        }

        self.push_chain(&sqes);
        keys
    }

    /// Submit an operation nobody will wait for, with an action to run when it
    /// lands.
    pub(crate) fn submit_detached(&self, mut sqe: sys::Sqe, action: Box<dyn OnComplete>) {
        let key = self.ledger.borrow_mut().submit_detached(action);
        sqe.user_data = key.user_data();
        self.push(sqe);
    }

    /// Ask the kernel to stop the operation `key` names.
    ///
    /// Best effort by nature: it may already have completed, in which case the
    /// cancel completes with `ENOENT` and the original result stands.
    pub(crate) fn cancel(&self, key: OpKey) {
        let sqe = uring_op::AsyncCancel::new()
            .target(key.user_data())
            .into_sqe();
        self.submit_detached(sqe, Box::new(Discard));
    }

    /// Stop every operation still in flight on `slot`, in one entry.
    pub(crate) fn cancel_slot(&self, slot: u32) {
        let sqe = uring_op::AsyncCancel::new()
            .fd(slot as i32)
            .flags(
                sys::AsyncCancelFlags::FD
                    | sys::AsyncCancelFlags::FD_FIXED
                    | sys::AsyncCancelFlags::ALL,
            )
            .into_sqe();
        self.submit_detached(sqe, Box::new(Discard));
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

    fn push(&self, sqe: sys::Sqe) {
        let mut sub = self.sub.borrow_mut();
        self.write(&mut sub, sqe);
    }

    fn push_chain(&self, sqes: &[sys::Sqe]) {
        assert!(
            sqes.len() <= self.ring.sq().entries() as usize,
            "a chain of {} does not fit a submission ring of {}",
            sqes.len(),
            self.ring.sq().entries()
        );

        let mut sub = self.sub.borrow_mut();
        if sub.backlog.is_empty() && self.space_left(&sub) as usize >= sqes.len() {
            for &sqe in sqes {
                self.write(&mut sub, sqe);
            }
        } else {
            // As a block, so that the drain can recognise it as one.
            sub.backlog.extend(sqes.iter().copied());
        }
    }

    /// Write one entry, into the ring if it fits and onto the backlog if not.
    ///
    /// Once anything is on the backlog everything goes there, or later
    /// submissions would overtake the ones already waiting.
    fn write(&self, sub: &mut Submission, sqe: sys::Sqe) {
        if !sub.backlog.is_empty() || self.space_left(sub) == 0 {
            sub.backlog.push_back(sqe);
            return;
        }

        // SAFETY: `space_left` is computed against the kernel's head, so this
        // slot is at or above the last published tail and not one the kernel is
        // reading.
        unsafe { self.ring.sq().sqe(sub.tail).write(sqe) };
        sub.tail = sub.tail.wrapping_add(1);
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
    /// With `wait`, blocks until at least one completion arrives; the caller
    /// must have checked that something is actually outstanding, or it will
    /// block forever. Without it, this is a flush.
    pub(crate) fn turn(&self, wait: bool) -> Result<(), Errno> {
        let mut wait = wait;

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
            if to_submit == 0 && !wait && self.in_flight() == 0 {
                return Ok(());
            }

            // SAFETY: no argument is passed, so `arg` and `argsz` are trivially
            // valid for these flags. Every queued entry was built by `op`, and
            // anything it points at is owned by a ledger entry that outlives the
            // operation — that is what detaching is for.
            let entered = unsafe {
                syscall::io_uring_enter(
                    self.ring.enter_fd(),
                    to_submit,
                    wait as u32,
                    self.ring.enter_flags() | sys::EnterFlags::GETEVENTS,
                    ptr::null(),
                    0,
                )
            };

            match entered {
                Ok(_) => {}
                // Interrupted, nothing ready, or no room to post more
                // completions. All three want the same thing: reap, and let
                // whatever was not submitted wait on the backlog.
                Err(Errno::INTR) | Err(Errno::AGAIN) | Err(Errno::BUSY) => {}
                Err(e) => return Err(e),
            }

            self.reap();

            // One task can fill the ring several times over in a single poll, so
            // keep going while the backlog is actually shrinking. It stops
            // shrinking when the kernel is not consuming, and then the answer is
            // to reap rather than to push harder.
            let backlog_after = self.sub.borrow().backlog.len();
            if backlog_after == 0 || backlog_after >= backlog_before {
                return Ok(());
            }
            wait = false;
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
            if self.turn(true).is_err() {
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
        uring_op::Nop::new().into_sqe()
    }

    fn linked_nop() -> sys::Sqe {
        uring_op::Nop::new()
            .sqe_flags(|f| f | sys::SqeFlags::IO_LINK)
            .into_sqe()
    }

    /// A small ring, so the paths that only open up under pressure are the
    /// ordinary case here.
    fn driver() -> Driver {
        Driver::with_capacity(8, 16, 16, 0).expect("Driver::with_capacity")
    }

    /// Drain, without waiting on a ring that has nothing left to give.
    fn drain(driver: &Driver) {
        while driver.in_flight() > 0 {
            driver.turn(true).expect("turn");
        }
    }

    #[test]
    fn a_batch_larger_than_the_ring_still_all_completes() {
        let driver = driver();
        const N: u32 = 100;

        for _ in 0..N {
            driver.submit_detached(nop(), Box::new(Discard));
        }

        assert_eq!(driver.in_flight(), N);
        assert!(
            driver.backlog_pending(),
            "a hundred entries do not fit a ring of eight"
        );

        drain(&driver);
        assert!(!driver.backlog_pending(), "the backlog drained with them");
    }

    /// More completions than the completion ring holds, so the kernel has to
    /// park the rest in its overflow list and hand them over on a later enter.
    #[test]
    fn completions_beyond_the_ring_are_not_lost() {
        let driver = driver();
        const N: u32 = 200;

        for _ in 0..N {
            driver.submit_detached(nop(), Box::new(Discard));
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
            driver.submit_detached(nop(), Box::new(Discard));
        }
        assert!(!driver.backlog_pending(), "six of eight fit");

        let keys = driver.submit_chain([linked_nop(), linked_nop(), nop()]);
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

        driver.turn(false).expect("turn");
        assert_eq!(driver.in_flight(), 0);
    }
}
