use std::cell::RefCell;
use std::mem;
use std::task::{LocalWaker, Poll};
use std::time::{Duration, Instant};

use slab::Slab;

const BASE_CAPACITY: usize = 256;
const ARITY: usize = 4;

/// When expiring, rebuild the whole heap rather than unlink one timer at a time
/// once more than `1 / REBUILD_RATIO` of it is due.
const REBUILD_RATIO: usize = 8;

/// Timers to take one at a time before falling back to a sweep.
const SWEEP_AFTER: usize = 4;

/// A handle to one armed timer.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct TimerKey(usize);

pub(crate) struct Timers {
    inner: RefCell<Inner>,

    /// Wakers of timers that have just expired.
    ready: RefCell<Vec<LocalWaker>>,

    /// Zero on the nanosecond scale everything else here is measured in.
    anchor: Instant,
}

struct Inner {
    /// Min-heap of the deadlines still to come, as slab keys.
    heap: Vec<Slot>,
    entries: Slab<Entry>,

    /// Scratch for [`Inner::take_due`], kept for its capacity.
    due: Vec<usize>,
}

/// One heap element.
///
/// The deadline is duplicated out of the entry deliberately: sifting is all
/// comparison, and keeping the key here would turn every one of them into a
/// slab lookup.
#[derive(Clone, Copy)]
struct Slot {
    deadline: u64,
    index: usize,
}

struct Entry {
    /// Where in the heap this entry sits, or `None` once its deadline has
    /// passed and it has left the heap.
    pos: Option<usize>,

    /// Whoever is waiting for this deadline, once they have asked.
    /// `None` between arming and the first poll.
    waker: Option<LocalWaker>,
}

impl Timers {
    pub(crate) fn new() -> Timers {
        Timers {
            inner: RefCell::new(Inner {
                heap: Vec::with_capacity(BASE_CAPACITY),
                entries: Slab::with_capacity(BASE_CAPACITY),
                due: Vec::new(),
            }),
            ready: RefCell::new(Vec::new()),
            anchor: Instant::now(),
        }
    }

    /// Now, on this store's scale.
    pub(crate) fn now(&self) -> u64 {
        self.since_anchor(Instant::now())
    }

    /// Where `instant` falls on this store's scale.
    ///
    /// An instant from before the runtime existed saturates to zero, which
    /// reads as a deadline already past — the same thing it means.
    pub(crate) fn since_anchor(&self, instant: Instant) -> u64 {
        instant.saturating_duration_since(self.anchor).as_nanos() as u64
    }

    /// The inverse, for reporting a deadline back to the caller.
    pub(crate) fn to_instant(&self, deadline: u64) -> Instant {
        self.anchor + Duration::from_nanos(deadline)
    }

    /// Arm a timer for `deadline`, with nobody waiting on it yet.
    pub(crate) fn insert(&self, deadline: u64) -> TimerKey {
        let mut inner = self.inner.borrow_mut();

        let index = inner.entries.insert(Entry {
            pos: None,
            waker: None,
        });
        inner.push(Slot { deadline, index });

        TimerKey(index)
    }

    /// Disarm `key` and drop its entry. Idempotent only in the sense that a
    /// fired timer is still a live entry; a key is never removed twice.
    pub(crate) fn remove(&self, key: TimerKey) {
        let mut inner = self.inner.borrow_mut();
        let entry = inner.entries.remove(key.0);

        if let Some(pos) = entry.pos {
            inner.unlink(pos);
        }
    }

    /// Move `key` to a new deadline, whether or not it has already fired.
    pub(crate) fn reset(&self, key: TimerKey, deadline: u64) {
        let mut inner = self.inner.borrow_mut();
        let index = key.0;

        match inner.entries[index].pos {
            // Still pending: shift it and let the heap re-settle. The new
            // deadline can be either side of the old one, so this may go up or
            // down.
            Some(pos) => inner.settle(pos, Slot { deadline, index }),
            // Already fired, so it is out of the heap and has to go back in.
            None => inner.push(Slot { deadline, index }),
        }
    }

    /// Take `key`'s expiry if it has happened, or leave `waker` to be told when
    /// it does.
    pub(crate) fn poll(&self, key: TimerKey, waker: &LocalWaker) -> Poll<()> {
        let mut inner = self.inner.borrow_mut();
        let entry = &mut inner.entries[key.0];

        if entry.pos.is_none() {
            return Poll::Ready(());
        }

        match &entry.waker {
            // Same task polling again, which is the common case by far — a
            // clone per poll of a pending timer would be pure waste.
            Some(current) if current.will_wake(waker) => {}
            _ => entry.waker = Some(waker.clone()),
        }

        Poll::Pending
    }

    /// The deadline the reactor may not sleep past, if there is one.
    pub(crate) fn earliest(&self) -> Option<u64> {
        self.inner.borrow().heap.first().map(|slot| slot.deadline)
    }

    /// Wake everything due at `now`, returning how many fired.
    pub(crate) fn expire(&self, now: u64) -> usize {
        let mut fired = 0;

        while fired < SWEEP_AFTER {
            if !self.next_is_due(now) {
                return fired;
            }

            if let Some(waker) = self.pop_root() {
                waker.wake();
            }

            fired += 1;
        }

        fired + self.sweep(now)
    }

    /// Whether the earliest deadline has arrived.
    fn next_is_due(&self, now: u64) -> bool {
        self.inner
            .borrow()
            .heap
            .first()
            .is_some_and(|slot| slot.deadline <= now)
    }

    /// Take the earliest timer, returning whoever was waiting on it.
    fn pop_root(&self) -> Option<LocalWaker> {
        let mut inner = self.inner.borrow_mut();

        let index = inner.heap.first().expect("the caller found one due").index;
        inner.unlink(0);

        let entry = &mut inner.entries[index];
        entry.pos = None;

        entry.waker.take()
    }

    /// Take every due timer in one pass.
    fn sweep(&self, now: u64) -> usize {
        let mut ready = mem::take(&mut *self.ready.borrow_mut());
        debug_assert!(ready.is_empty(), "a previous sweep left wakers behind");

        let fired = self.inner.borrow_mut().take_due(now, &mut ready);

        for waker in ready.drain(..) {
            waker.wake();
        }

        *self.ready.borrow_mut() = ready;
        fired
    }

    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.inner.borrow().heap.len()
    }


    #[cfg(test)]
    pub(crate) fn entries(&self) -> usize {
        self.inner.borrow().entries.len()
    }
}

impl Inner {
    /// Write `slot` at `pos` and record where it went.
    fn place(&mut self, pos: usize, slot: Slot) {
        self.heap[pos] = slot;
        self.entries[slot.index].pos = Some(pos);
    }

    fn push(&mut self, slot: Slot) {
        let pos = self.heap.len();
        // Reserves the cell only; `sift_up` decides what actually lands there
        // and records the position once, when it stops.
        self.heap.push(slot);
        self.sift_up(pos, slot);
    }

    /// Take the slot at `pos` out of the heap, filling the hole with the last
    /// element and letting it settle.
    ///
    /// Does not touch the entry at `pos` — the caller has either just removed it
    /// from the slab or is about to mark it fired.
    fn unlink(&mut self, pos: usize) {
        let last = self.heap.pop().expect("pos indexes a non-empty heap");

        if pos == self.heap.len() {
            return;
        }

        self.settle(pos, last);
    }

    /// Seat `slot` in the hole at `pos`, moving the hole to wherever `slot`
    /// belongs.
    fn settle(&mut self, pos: usize, slot: Slot) {
        if pos > 0 && self.heap[(pos - 1) / ARITY].deadline > slot.deadline {
            self.sift_up(pos, slot);
        } else {
            self.sift_down(pos, slot);
        }
    }

    /// Take every timer due at `now` out of the heap, collecting the wakers of
    /// those anybody was waiting on. Returns how many were taken.
    fn take_due(&mut self, now: u64, ready: &mut Vec<LocalWaker>) -> usize {
        if self.heap.first().is_none_or(|slot| slot.deadline > now) {
            return 0;
        }

        // Breadth-first from the root, descending only through nodes that are
        // themselves due: a node is never later than its children, so a node
        // that is not due has no due descendants. `due` doubles as the queue and
        // the result, which also means it comes out in ascending order — a
        // breadth-first walk of a heap array visits positions in order, because
        // one node's children all precede the next node's.
        let mut due = mem::take(&mut self.due);
        due.clear();
        due.push(0);

        let mut read = 0;
        while read < due.len() {
            let pos = due[read];
            read += 1;

            let first = pos * ARITY + 1;
            let last = (first + ARITY).min(self.heap.len());
            for child in first..last {
                if self.heap[child].deadline <= now {
                    due.push(child);
                }
            }
        }

        // Mark them fired and collect the wakers before touching the heap, while
        // the recorded positions still describe it.
        for &pos in &due {
            let index = self.heap[pos].index;
            let entry = &mut self.entries[index];
            entry.pos = None;
            if let Some(waker) = entry.waker.take() {
                ready.push(waker);
            }
        }

        let fired = due.len();
        if fired * REBUILD_RATIO < self.heap.len() {
            // Descending, which keeps every unlink below the positions still
            // queued: the survivor pulled in from the tail always comes from
            // above the current position, and the parent it is compared against
            // is itself still-due and so no later than it — meaning the sift up
            // stops at once and the sift down stays clear of the rest.
            for &pos in due.iter().rev() {
                self.unlink(pos);
            }
        } else {
            self.rebuild();
        }

        self.due = due;
        fired
    }

    /// Drop every fired slot and re-heapify what is left, bottom-up.
    ///
    /// Floyd's method: sifting down from the last internal node backwards makes
    /// each subtree a heap before its parent is considered, in `O(n)` rather
    /// than the `O(n log n)` of inserting them one at a time. Positions are
    /// written once at the end instead of at every step of every sift, which is
    /// most of the saving.
    fn rebuild(&mut self) {
        let entries = &self.entries;
        self.heap.retain(|slot| entries[slot.index].pos.is_some());

        let len = self.heap.len();
        if len > 1 {
            // `(len - 2) / ARITY` is the parent of the last element, and so the
            // last node with any children at all.
            for pos in (0..=(len - 2) / ARITY).rev() {
                let slot = self.heap[pos];
                self.sift_down_detached(pos, slot);
            }
        }

        for pos in 0..len {
            self.entries[self.heap[pos].index].pos = Some(pos);
        }
    }

    /// Walk the hole at `pos` up, pulling parents down into it while they are
    /// later than `slot`, then seat `slot` where it stops.
    fn sift_up(&mut self, mut pos: usize, slot: Slot) {
        while pos > 0 {
            let parent = (pos - 1) / ARITY;
            if self.heap[parent].deadline <= slot.deadline {
                break;
            }

            let up = self.heap[parent];
            self.place(pos, up);
            pos = parent;
        }

        self.place(pos, slot);
    }

    /// Walk the hole at `pos` down, pulling the earliest child up into it while
    /// that child is earlier than `slot`, then seat `slot` where it stops.
    fn sift_down(&mut self, mut pos: usize, slot: Slot) {
        while let Some(child) = self.earliest_child(pos, slot.deadline) {
            let down = self.heap[child];
            self.place(pos, down);
            pos = child;
        }

        self.place(pos, slot);
    }

    /// [`sift_down`](Inner::sift_down) without maintaining [`Entry::pos`], for
    /// [`rebuild`](Inner::rebuild), which sets every position afterwards anyway.
    fn sift_down_detached(&mut self, mut pos: usize, slot: Slot) {
        while let Some(child) = self.earliest_child(pos, slot.deadline) {
            self.heap[pos] = self.heap[child];
            pos = child;
        }

        self.heap[pos] = slot;
    }

    /// The child of `pos` to swap with, or `None` if none is earlier than
    /// `deadline` — which is where a sift down stops.
    ///
    /// It has to be the *earliest* child: swapping with any other would leave it
    /// above a sibling earlier than itself.
    fn earliest_child(&self, pos: usize, deadline: u64) -> Option<usize> {
        let len = self.heap.len();
        let first = pos * ARITY + 1;
        if first >= len {
            return None;
        }

        // Adjacent, so however wide `ARITY` is this stays within a cache line
        // or two.
        let mut child = first;
        for candidate in (first + 1)..(first + ARITY).min(len) {
            if self.heap[candidate].deadline < self.heap[child].deadline {
                child = candidate;
            }
        }

        (self.heap[child].deadline < deadline).then_some(child)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::runtime::driver::noop_local_waker;

    /// The heap's invariant, checked from the outside: every parent is due no
    /// later than its children, and every entry's `pos` points back at itself.
    fn check(timers: &Timers) {
        let inner = timers.inner.borrow();

        for (pos, slot) in inner.heap.iter().enumerate() {
            if pos > 0 {
                let parent = (pos - 1) / ARITY;
                assert!(
                    inner.heap[parent].deadline <= slot.deadline,
                    "heap[{parent}] = {} is later than its child heap[{pos}] = {}",
                    inner.heap[parent].deadline,
                    slot.deadline,
                );
            }
            assert_eq!(
                inner.entries[slot.index].pos,
                Some(pos),
                "entry {} thinks it is elsewhere",
                slot.index,
            );
        }

        for (index, entry) in inner.entries.iter() {
            if let Some(pos) = entry.pos {
                assert_eq!(
                    inner.heap[pos].index, index,
                    "entry {index} points at a stranger"
                );
            }
        }
    }

    /// Deadlines chosen to make the sift paths work: not sorted, not reverse
    /// sorted, with duplicates and a run that forces a fill from a different
    /// subtree.
    const SPREAD: [u64; 12] = [50, 10, 90, 10, 70, 30, 30, 110, 20, 60, 40, 80];

    #[test]
    fn the_earliest_deadline_is_the_one_reported() {
        let timers = Timers::new();

        for (i, &deadline) in SPREAD.iter().enumerate() {
            timers.insert(deadline);
            check(&timers);
            assert_eq!(
                timers.earliest(),
                Some(SPREAD[..=i].iter().copied().min().expect("non-empty")),
            );
        }
    }

    #[test]
    fn expiry_is_in_deadline_order() {
        let timers = Timers::new();
        let keys: Vec<_> = SPREAD.iter().map(|&d| timers.insert(d)).collect();

        let mut sorted = SPREAD;
        sorted.sort_unstable();

        for &deadline in &sorted {
            assert_eq!(timers.earliest(), Some(deadline));
            assert!(timers.next_is_due(deadline));
            timers.pop_root();
            check(&timers);
        }

        assert_eq!(timers.earliest(), None);
        // Every entry is still alive: expiring takes a timer out of the heap,
        // not out of the store.
        assert_eq!(timers.entries(), SPREAD.len());

        for key in keys {
            timers.remove(key);
        }
        assert_eq!(timers.entries(), 0);
    }

    #[test]
    fn only_due_timers_are_taken() {
        let timers = Timers::new();
        for &deadline in &SPREAD {
            timers.insert(deadline);
        }

        assert!(!timers.next_is_due(9), "nothing is due before the first");
        assert_eq!(timers.expire(9), 0);
        assert_eq!(timers.len(), SPREAD.len());

        // The three at 10 and 10 and 20.
        assert_eq!(timers.expire(20), 3);
        assert_eq!(timers.len(), SPREAD.len() - 3);
        check(&timers);
    }

    /// The property tombstoning would give up: a cancelled timer leaves nothing
    /// behind, whatever order the cancellations come in.
    #[test]
    fn cancelling_leaves_no_trace() {
        for step in 1..=SPREAD.len() {
            let timers = Timers::new();
            let keys: Vec<_> = SPREAD.iter().map(|&d| timers.insert(d)).collect();

            // A different traversal order each time round, so that removals hit
            // the root, the last element, and the middle.
            let mut taken = vec![false; keys.len()];
            let mut at = 0;
            for _ in 0..keys.len() {
                while taken[at] {
                    at = (at + 1) % keys.len();
                }
                taken[at] = true;
                timers.remove(keys[at]);
                check(&timers);
                at = (at + step) % keys.len();
            }

            assert_eq!(timers.len(), 0, "step {step}");
            assert_eq!(timers.entries(), 0, "step {step}");
        }
    }

    #[test]
    fn removing_a_fired_timer_is_still_clean() {
        let timers = Timers::new();
        let early = timers.insert(10);
        let late = timers.insert(20);

        assert_eq!(timers.expire(10), 1);
        timers.remove(early);
        check(&timers);

        assert_eq!(timers.earliest(), Some(20));
        timers.remove(late);
        assert_eq!(timers.entries(), 0);
    }

    #[test]
    fn reset_moves_a_pending_timer_in_both_directions() {
        let timers = Timers::new();
        let a = timers.insert(50);
        timers.insert(60);
        timers.insert(70);

        timers.reset(a, 80);
        check(&timers);
        assert_eq!(timers.earliest(), Some(60), "it moved behind the others");

        timers.reset(a, 5);
        check(&timers);
        assert_eq!(timers.earliest(), Some(5), "and back to the front");
    }

    /// What an [`Interval`](crate::time::Interval) does every tick: the timer
    /// has left the heap, and resetting it has to put it back rather than
    /// silently do nothing.
    #[test]
    fn reset_rearms_a_fired_timer() {
        let timers = Timers::new();
        let key = timers.insert(10);

        assert_eq!(timers.expire(10), 1);
        assert_eq!(timers.earliest(), None);

        timers.reset(key, 20);
        check(&timers);
        assert_eq!(timers.earliest(), Some(20));
        assert_eq!(timers.expire(20), 1);
    }

    #[test]
    fn a_pending_timer_registers_its_waker_and_a_fired_one_reports_ready() {
        let timers = Timers::new();
        let key = timers.insert(10);
        let waker = noop_local_waker();

        assert_eq!(timers.poll(key, &waker), Poll::Pending);
        assert_eq!(timers.expire(10), 1);
        assert_eq!(timers.poll(key, &waker), Poll::Ready(()));
        // Idempotent: a future may be polled again before it is dropped.
        assert_eq!(timers.poll(key, &waker), Poll::Ready(()));

        timers.remove(key);
    }

    /// A timer whose deadline passes before anybody polls it. There is nobody to
    /// wake, which must not be mistaken for nothing having happened.
    #[test]
    fn a_timer_can_expire_before_it_is_ever_polled() {
        let timers = Timers::new();
        let key = timers.insert(10);

        assert_eq!(timers.expire(10), 1);
        assert_eq!(timers.poll(key, &noop_local_waker()), Poll::Ready(()));

        timers.remove(key);
    }

    /// The property every expiry tier has to share: [`Timers::expire`] fires
    /// exactly the timers at or before the cutoff, and leaves a heap behind.
    #[test]
    fn expiry_fires_exactly_the_timers_that_are_due() {
        let mut rng = 0x243F_6A88_85A3_08D3u64;
        let mut next = move || {
            rng ^= rng << 13;
            rng ^= rng >> 7;
            rng ^= rng << 17;
            rng
        };

        let mut pops = 0;
        let mut unlinks = 0;
        let mut rebuilds = 0;

        for _ in 0..500 {
            let count = 1 + (next() % 400) as usize;
            let deadlines: Vec<u64> = (0..count).map(|_| next() % 1_000).collect();
            // Deliberately reaches past both ends of the range.
            let cutoff = next() % 1_100;

            // Derived from what a timer store means rather than from a second
            // implementation, so that a mistake both paths share — an off-by-one
            // at the cutoff, say — still fails this.
            let expected = deadlines.iter().filter(|&&d| d <= cutoff).count();

            let batched = Timers::new();
            let batched_keys: Vec<TimerKey> =
                deadlines.iter().map(|&d| batched.insert(d)).collect();
            let fired = batched.expire(cutoff);
            check(&batched);

            assert_eq!(
                fired, expected,
                "{count} timers, cutoff {cutoff}: fired {fired}, {expected} were due"
            );
            assert_eq!(
                batched.len(),
                count - expected,
                "the survivors were miscounted"
            );

            if expected <= SWEEP_AFTER {
                pops += 1;
            } else if expected * REBUILD_RATIO < count {
                unlinks += 1;
            } else {
                rebuilds += 1;
            }

            // The survivors must still be a usable heap: every one of them comes
            // out, in order, and nothing else does.
            let mut last = 0;
            let mut drained = 0;
            while batched.next_is_due(u64::MAX - 1) {
                let deadline = batched.earliest().expect("one is due");
                assert!(deadline >= last, "the survivors came out of order");
                last = deadline;
                batched.pop_root();
                drained += 1;
            }
            assert_eq!(drained, count - expected, "a survivor was lost");
            check(&batched);

            for key in batched_keys {
                batched.remove(key);
            }
            assert_eq!(batched.entries(), 0, "entries outlived their futures");
        }

        // The randomisation is only worth anything if it reached every tier.
        assert!(pops > 20, "the pop tier ran only {pops} times");
        assert!(unlinks > 20, "the unlink tier ran only {unlinks} times");
        assert!(rebuilds > 20, "the rebuild tier ran only {rebuilds} times");
    }

    /// The rebuild path specifically, at a size where it is the one that runs.
    #[test]
    fn expiring_almost_everything_rebuilds_a_valid_heap() {
        let timers = Timers::new();
        let keys: Vec<TimerKey> = (0..2_000).map(|i| timers.insert((i * 7) % 2_000)).collect();

        // Nine tenths due, so this is the rebuild and not the unlink path.
        let fired = timers.expire(1_799);
        assert_eq!(fired, 1_800);
        check(&timers);
        assert_eq!(timers.len(), 200);

        assert_eq!(timers.earliest(), Some(1_800));
        assert_eq!(timers.expire(u64::MAX - 1), 200);
        check(&timers);

        for key in keys {
            timers.remove(key);
        }
        assert_eq!(timers.entries(), 0);
    }

    /// Expiring nothing must not disturb the heap or the scratch buffers.
    #[test]
    fn expiring_when_nothing_is_due_is_a_no_op() {
        let timers = Timers::new();
        for &deadline in &SPREAD {
            timers.insert(deadline);
        }

        assert_eq!(timers.expire(9), 0);
        assert_eq!(timers.len(), SPREAD.len());
        check(&timers);
        assert_eq!(timers.earliest(), Some(10));
    }

    /// A due timer deep in the heap is only reachable through due ancestors, so
    /// the pruning walk has to find it. A single chain of increasing deadlines
    /// makes every node the parent of the next.
    #[test]
    fn a_due_timer_below_other_due_timers_is_still_found() {
        let timers = Timers::new();
        let keys: Vec<TimerKey> = (0..64).map(|i| timers.insert(i)).collect();

        assert_eq!(timers.expire(63), 64, "the whole chain was due");
        assert_eq!(timers.len(), 0);
        check(&timers);

        for key in keys {
            timers.remove(key);
        }
        assert_eq!(timers.entries(), 0);
    }

    #[test]
    fn an_instant_before_the_anchor_is_already_due() {
        let timers = Timers::new();
        let before = timers.anchor - Duration::from_secs(1);

        assert_eq!(timers.since_anchor(before), 0);
    }

    #[test]
    fn the_anchor_round_trips() {
        let timers = Timers::new();
        let then = timers.anchor + Duration::from_nanos(1_234_567);

        assert_eq!(timers.since_anchor(then), 1_234_567);
        assert_eq!(timers.to_instant(1_234_567), then);
    }
}
