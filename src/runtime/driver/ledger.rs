//! The table mapping a completion's `user_data` back to the operation it
//! belongs to.
//!
//! An entry lives from submission until the result is taken, which is not the
//! same as until the future is dropped: a dropped future leaves its operation
//! running in the kernel, and the entry has to outlive it to keep the buffer
//! alive and to have somewhere for the completion to land. That is what
//! [`State::Detached`] is for.
//!
//! # Telling one occupant of an entry from the next
//!
//! `user_data` is a [`Slab`] key and a sequence number packed into one word.
//! A completion that arrives after its operation was cancelled and its entry
//! reused finds a sequence number that no longer matches and is dropped,
//! instead of being delivered to whoever holds that entry now. Without it the
//! cancellation path would be a use-after-free waiting for the right timing.
//!
//! The sequence number counts *submissions*, not reuses of a particular entry,
//! because a slab has nowhere to keep anything in a vacant entry — it only
//! needs to differ between successive occupants, and a monotonic counter does
//! that. It wraps after four billion operations, which would have to happen
//! while one cancelled completion is still outstanding for it to matter.

use std::mem;
use std::task::{LocalWaker, Poll};

use slab::Slab;

use crate::io_uring::sys;

use super::Driver;

/// What to do with a completion, decided while the ledger is borrowed and
/// carried out after that borrow has been released.
///
/// Both of the interesting arms re-enter the driver — waking pushes onto the
/// run queue, and a detached action can submit further work — so neither may
/// run while the ledger is still borrowed.
pub(crate) enum Outcome {
    /// A stale completion, or one whose result nobody has asked for yet.
    Ignore,
    /// Somebody is waiting for this result.
    Wake(LocalWaker),
    /// Nobody is waiting; run the action the operation left behind.
    Detached(Box<dyn OnComplete>, i32, sys::CqeFlags),
}

/// What a detached operation does when its completion finally arrives.
///
/// Implementors carry whatever the kernel still has a pointer to, so dropping
/// the box is itself part of the contract: it is the moment the buffer becomes
/// free again.
pub(crate) trait OnComplete {
    fn on_complete(self: Box<Self>, driver: &Driver, res: i32, flags: sys::CqeFlags);
}

/// A detached operation with nothing to clean up, which still needs an entry so
/// that its completion is accounted for rather than looking stale.
pub(crate) struct Discard;

impl OnComplete for Discard {
    fn on_complete(self: Box<Self>, _driver: &Driver, _res: i32, _flags: sys::CqeFlags) {}
}

/// A handle to one ledger entry, and the `user_data` of the operation in it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct OpKey {
    index: u32,
    seq: u32,
}

impl OpKey {
    /// The token to put in the SQE and read back out of the CQE.
    pub(crate) fn user_data(self) -> u64 {
        (self.seq as u64) << 32 | self.index as u64
    }

    fn from_user_data(user_data: u64) -> OpKey {
        OpKey {
            index: user_data as u32,
            seq: (user_data >> 32) as u32,
        }
    }
}

/// One entry, and the sequence number that says which operation is in it.
struct Entry {
    seq: u32,
    state: State,
}

enum State {
    /// In the kernel, with nobody waiting on it yet.
    Submitted,
    /// In the kernel, with a future waiting on it.
    Waiting(LocalWaker),
    /// Finished, with the result not yet taken.
    Completed(i32, sys::CqeFlags),
    /// In the kernel, with the future gone. The action owns anything the kernel
    /// still refers to.
    Detached(Box<dyn OnComplete>),
}

/// The operation table.
pub(crate) struct Ledger {
    ops: Slab<Entry>,
    /// Handed to the next operation, then incremented.
    next_seq: u32,
    /// Entries the kernel has yet to post a completion for.
    outstanding: u32,
}

impl Ledger {
    pub(crate) fn with_capacity(capacity: usize) -> Ledger {
        Ledger {
            ops: Slab::with_capacity(capacity),
            next_seq: 0,
            outstanding: 0,
        }
    }

    /// Occupied entries: operations in the kernel, plus results nobody has
    /// collected. The tests read it to tell the two apart.
    #[cfg(test)]
    pub(crate) fn live(&self) -> u32 {
        self.ops.len() as u32
    }

    /// Completions the kernel still owes us.
    ///
    /// Deliberately *not* [`live`](Ledger::live). The executor uses this to
    /// decide whether it may block waiting for a completion, and a result that
    /// has already arrived is not one the kernel will deliver again — waiting on
    /// an entry like that would wait for ever.
    pub(crate) fn outstanding(&self) -> u32 {
        self.outstanding
    }

    fn alloc(&mut self, state: State) -> OpKey {
        let seq = self.next_seq;
        self.next_seq = self.next_seq.wrapping_add(1);
        self.outstanding += 1;

        let entry = self.ops.vacant_entry();
        let index = u32::try_from(entry.key()).expect("more than 4 billion operations in flight");
        entry.insert(Entry { seq, state });

        OpKey { index, seq }
    }

    /// Take an entry for an operation a future will wait on.
    pub(crate) fn submit(&mut self) -> OpKey {
        self.alloc(State::Submitted)
    }

    /// Take an entry for an operation nobody will wait on.
    pub(crate) fn submit_detached(&mut self, action: Box<dyn OnComplete>) -> OpKey {
        self.alloc(State::Detached(action))
    }

    /// The entry `key` names, or `None` if the completion it came from is stale.
    fn get(&mut self, key: OpKey) -> Option<&mut Entry> {
        self.ops
            .get_mut(key.index as usize)
            .filter(|entry| entry.seq == key.seq)
    }

    /// Record a completion and decide what the driver should do about it.
    pub(crate) fn complete(&mut self, user_data: u64, res: i32, flags: sys::CqeFlags) -> Outcome {
        let key = OpKey::from_user_data(user_data);
        let Some(entry) = self.get(key) else {
            // The operation was cancelled and its entry freed or reused, or this
            // is a completion for something that never had an entry at all.
            return Outcome::Ignore;
        };

        let previous = mem::replace(&mut entry.state, State::Completed(res, flags));

        if matches!(previous, State::Completed(..)) {
            debug_assert!(false, "operation {user_data:#x} completed twice");
            return Outcome::Ignore;
        }

        // The kernel is done with this one; only the result is left to collect.
        self.outstanding -= 1;

        match previous {
            State::Submitted => Outcome::Ignore,
            State::Waiting(waker) => Outcome::Wake(waker),
            State::Detached(action) => {
                // Nobody will ever take this result, so the entry is done with.
                self.ops.remove(key.index as usize);
                Outcome::Detached(action, res, flags)
            }
            State::Completed(..) => unreachable!("returned above"),
        }
    }

    /// Take the result if it has arrived, and register `waker` for it if not.
    pub(crate) fn poll(&mut self, key: OpKey, waker: &LocalWaker) -> Poll<(i32, sys::CqeFlags)> {
        let entry = self
            .get(key)
            .expect("an operation was polled after its entry was freed");

        match &mut entry.state {
            State::Completed(res, flags) => {
                let done = (*res, *flags);
                self.ops.remove(key.index as usize);
                Poll::Ready(done)
            }
            state => {
                *state = State::Waiting(waker.clone());
                Poll::Pending
            }
        }
    }

    /// Take the result if it has already arrived, freeing the entry.
    ///
    /// The abandon path checks this first: an operation that finished before its
    /// future was dropped needs no detaching, and so no boxed action either.
    pub(crate) fn take_completed(&mut self, key: OpKey) -> Option<(i32, sys::CqeFlags)> {
        let entry = self.get(key)?;

        if let State::Completed(res, flags) = entry.state {
            self.ops.remove(key.index as usize);
            return Some((res, flags));
        }

        None
    }

    /// Give up on an operation that is still running, leaving `action` for its
    /// completion to find.
    ///
    /// The caller should ask the kernel to cancel it afterwards, or an operation
    /// that never finishes on its own — a read on an idle socket — holds its
    /// entry, and whatever that entry owns, forever.
    pub(crate) fn detach(&mut self, key: OpKey, action: Box<dyn OnComplete>) {
        let entry = self
            .get(key)
            .expect("an operation was abandoned after its entry was freed");

        debug_assert!(
            !matches!(entry.state, State::Completed(..)),
            "detaching a finished operation discards its result"
        );
        debug_assert!(
            !matches!(entry.state, State::Detached(_)),
            "detaching twice would drop the first action, freeing what it owns"
        );

        entry.state = State::Detached(action);
    }

    /// Give up on everything, for shutdown.
    ///
    /// Results nobody took are discarded. Operations still in the kernel become
    /// detached, so that whatever they own outlives them, and their keys come
    /// back so the caller can cancel them.
    ///
    /// Entries that were *already* detached keep the action they have. Handing
    /// them a fresh one would drop the old one, freeing a buffer the kernel is
    /// still writing into — which is the exact bug detaching exists to prevent.
    pub(crate) fn abandon_all(&mut self) -> Vec<OpKey> {
        let mut outstanding = Vec::new();
        let mut finished = Vec::new();

        for (index, entry) in self.ops.iter_mut() {
            match entry.state {
                State::Completed(..) => finished.push(index),
                State::Detached(_) => {}
                State::Submitted | State::Waiting(_) => {
                    entry.state = State::Detached(Box::new(Discard));
                    outstanding.push(OpKey {
                        index: index as u32,
                        seq: entry.seq,
                    });
                }
            }
        }

        for index in finished {
            self.ops.remove(index);
        }

        outstanding
    }
}

#[cfg(test)]
mod tests {
    use super::super::noop_local_waker;
    use super::*;

    fn flags() -> sys::CqeFlags {
        sys::CqeFlags::empty()
    }

    #[test]
    fn a_key_round_trips_through_user_data() {
        let key = OpKey {
            index: 0xdead,
            seq: 0xbeef,
        };
        assert_eq!(key.user_data(), 0x0000_beef_0000_dead);
        assert_eq!(OpKey::from_user_data(key.user_data()), key);
    }

    #[test]
    fn a_reused_entry_gets_a_new_sequence_number() {
        let mut l = Ledger::with_capacity(4);

        let first = l.submit();
        assert_eq!(l.live(), 1);
        l.complete(first.user_data(), 0, flags());
        assert_eq!(
            l.poll(first, &noop_local_waker()),
            Poll::Ready((0, flags()))
        );
        assert_eq!(l.live(), 0);

        let second = l.submit();
        assert_eq!(second.index, first.index, "the slab entry came back");
        assert_ne!(second.seq, first.seq, "but it is a different operation");
    }

    /// The whole point of the sequence number: a completion for an operation
    /// whose entry has since been handed to somebody else must not be delivered
    /// to the new owner.
    #[test]
    fn a_completion_for_a_reused_entry_is_ignored() {
        let mut l = Ledger::with_capacity(4);

        let stale = l.submit();
        l.detach(stale, Box::new(Discard));
        l.complete(stale.user_data(), 1, flags());

        let fresh = l.submit();
        assert_eq!(
            fresh.index, stale.index,
            "the same entry, to make the point"
        );

        // A second, late completion for the cancelled operation.
        assert!(matches!(
            l.complete(stale.user_data(), 7, flags()),
            Outcome::Ignore
        ));

        // The new owner is untouched and still waiting for its own result.
        let waker = noop_local_waker();
        assert_eq!(l.poll(fresh, &waker), Poll::Pending);
    }

    /// A completion for an entry that was freed and *not* reused is stale too;
    /// the slab lookup misses rather than the sequence check failing.
    #[test]
    fn a_completion_for_a_freed_entry_is_ignored() {
        let mut l = Ledger::with_capacity(4);
        let key = l.submit();
        l.complete(key.user_data(), 0, flags());
        let _ = l.poll(key, &noop_local_waker());

        assert!(matches!(
            l.complete(key.user_data(), 0, flags()),
            Outcome::Ignore
        ));
    }

    #[test]
    fn a_completion_before_a_poll_is_kept_for_it() {
        let mut l = Ledger::with_capacity(4);
        let key = l.submit();

        assert!(matches!(
            l.complete(key.user_data(), 42, flags()),
            Outcome::Ignore
        ));

        assert_eq!(l.poll(key, &noop_local_waker()), Poll::Ready((42, flags())));
        assert_eq!(l.live(), 0, "taking the result frees the entry");
    }

    #[test]
    fn a_poll_before_the_completion_registers_a_waker() {
        let mut l = Ledger::with_capacity(4);
        let key = l.submit();
        let waker = noop_local_waker();

        assert_eq!(l.poll(key, &waker), Poll::Pending);
        assert!(matches!(
            l.complete(key.user_data(), 42, flags()),
            Outcome::Wake(_)
        ));
        assert_eq!(l.poll(key, &waker), Poll::Ready((42, flags())));
    }

    /// A result that has arrived is not one the kernel will send again. The
    /// executor blocks whenever something is outstanding, so counting an
    /// uncollected result as outstanding is a wait that never ends.
    #[test]
    fn a_collected_result_is_no_longer_outstanding() {
        let mut l = Ledger::with_capacity(4);
        let key = l.submit();
        assert_eq!(l.outstanding(), 1);

        l.complete(key.user_data(), 0, flags());
        assert_eq!(l.outstanding(), 0, "the kernel has handed it back");
        assert_eq!(l.live(), 1, "but the result is still sitting there");

        assert_eq!(l.poll(key, &noop_local_waker()), Poll::Ready((0, flags())));
        assert_eq!(l.live(), 0);
    }

    #[test]
    fn a_finished_operation_hands_its_result_straight_back() {
        let mut l = Ledger::with_capacity(4);
        let key = l.submit();
        l.complete(key.user_data(), 5, flags());

        let taken = l.take_completed(key);
        assert_eq!(taken, Some((5, flags())), "no need to detach at all");
        assert_eq!(l.live(), 0);
    }

    #[test]
    fn detaching_a_running_operation_keeps_the_entry_until_it_lands() {
        let mut l = Ledger::with_capacity(4);
        let key = l.submit();

        assert_eq!(l.take_completed(key), None, "it has not finished");
        l.detach(key, Box::new(Discard));
        assert_eq!(l.live(), 1, "the kernel still owes us a completion");

        assert!(matches!(
            l.complete(key.user_data(), 5, flags()),
            Outcome::Detached(..)
        ));
        assert_eq!(l.live(), 0);
    }

    /// Churn the table and check that `live` tracks reality and that every
    /// completion for an operation we have finished with is refused, however
    /// many times the entry underneath it has changed hands.
    #[test]
    fn churn_leaves_no_entry_stranded_and_no_stale_key_live() {
        let mut l = Ledger::with_capacity(0);
        let mut live: Vec<OpKey> = Vec::new();
        let mut done: Vec<OpKey> = Vec::new();
        let mut state = 0x243f_6a88u32;

        for _ in 0..10_000 {
            state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);

            if live.len() > 4 && state.is_multiple_of(2) {
                let key = live.swap_remove(state as usize % live.len());
                l.complete(key.user_data(), 0, flags());
                assert_eq!(l.poll(key, &noop_local_waker()), Poll::Ready((0, flags())));
                done.push(key);
            } else {
                live.push(l.submit());
            }

            assert_eq!(l.live() as usize, live.len());
        }

        for key in &done {
            assert!(
                matches!(l.complete(key.user_data(), 0, flags()), Outcome::Ignore),
                "entry {} accepted a completion for a finished operation",
                key.index
            );
        }

        for key in live {
            l.complete(key.user_data(), 0, flags());
            let _ = l.poll(key, &noop_local_waker());
        }
        assert_eq!(l.live(), 0, "every entry came back");
    }
}
