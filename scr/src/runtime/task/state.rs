use std::cell::Cell;
use std::fmt;

pub(super) struct State {
    val: Cell<usize>,
}

#[derive(Copy, Clone)]
pub(super) struct Snapshot(usize);

const RUNNING: usize = 0b000_0001;
const COMPLETE: usize = 0b000_0010;
const NOTIFIED: usize = 0b000_0100;
const JOIN_INTEREST: usize = 0b000_1000;
const CANCELLED: usize = 0b001_0000;
const OWNED: usize = 0b010_0000;
const OUTPUT_TAKEN: usize = 0b100_0000;

const STATE_MASK: usize =
    RUNNING | COMPLETE | NOTIFIED | JOIN_INTEREST | CANCELLED | OWNED | OUTPUT_TAKEN;

const REF_COUNT_MASK: usize = !STATE_MASK;

const REF_COUNT_SHIFT: usize = REF_COUNT_MASK.count_zeros() as usize;

const REF_ONE: usize = 1 << REF_COUNT_SHIFT;

const INITIAL_STATE: usize = (REF_ONE * 3) | JOIN_INTEREST | NOTIFIED;

#[must_use]
pub(super) enum TransitionToRunning {
    Polled,
    Cancelled,
    Dead,
}

#[must_use]
pub(super) enum TransitionToIdle {
    Idle,
    Notified,
    Cancelled,
}

pub(super) enum CallerRef {
    Kept,
    Consumed,
}

impl State {
    pub(super) fn new() -> State {
        State {
            val: Cell::new(INITIAL_STATE),
        }
    }

    pub(super) fn load(&self) -> Snapshot {
        Snapshot(self.val.get())
    }

    fn store(&self, snapshot: Snapshot) {
        self.val.set(snapshot.0);
    }

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

    pub(super) fn transition_to_complete(&self, cancelled: bool) -> Snapshot {
        let mut next = self.load();
        debug_assert!(next.is_running());
        debug_assert!(!next.is_complete());

        next.unset_running();
        next.set_complete();

        if cancelled {
            next.set_cancelled();
        } else {
            next.unset_cancelled();
        }

        self.store(next);

        next
    }

    pub(super) fn transition_to_terminal(&self, count: usize) -> bool {
        let prev = self.load();
        debug_assert!(prev.ref_count() >= count);

        self.store(Snapshot(prev.0 - count * REF_ONE));

        prev.ref_count() == count
    }

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

    pub(super) fn set_cancelled(&self) {
        let mut next = self.load();

        if next.is_complete() {
            return;
        }

        next.set_cancelled();
        self.store(next);
    }

    pub(super) fn take_output(&self) -> bool {
        let mut next = self.load();
        debug_assert!(next.is_complete());

        if next.is_output_taken() {
            return false;
        }

        next.set_output_taken();
        self.store(next);

        true
    }

    pub(super) fn set_owned(&self) {
        let mut next = self.load();
        debug_assert!(!next.is_owned());
        next.set_owned();
        self.store(next);
    }

    pub(super) fn unset_owned(&self) -> bool {
        let mut next = self.load();

        if !next.is_owned() {
            return false;
        }

        next.unset_owned();
        self.store(next);

        true
    }

    pub(super) fn unset_join_interested(&self) {
        let mut next = self.load();
        debug_assert!(next.is_join_interested());

        next.unset_join_interested();
        self.store(next);
    }

    pub(super) fn ref_inc(&self) {
        let mut next = self.load();
        next.ref_inc();
        self.store(next);
    }

    pub(super) fn ref_dec(&self) -> bool {
        let mut next = self.load();
        next.ref_dec();
        self.store(next);

        next.ref_count() == 0
    }
}

impl Snapshot {
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

    pub(super) fn is_cancelled(self) -> bool {
        self.0 & CANCELLED != 0
    }

    fn set_cancelled(&mut self) {
        self.0 |= CANCELLED;
    }

    fn unset_cancelled(&mut self) {
        self.0 &= !CANCELLED;
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

    fn is_output_taken(self) -> bool {
        self.0 & OUTPUT_TAKEN != 0
    }

    fn set_output_taken(&mut self) {
        self.0 |= OUTPUT_TAKEN;
    }

    fn ref_count(self) -> usize {
        (self.0 & REF_COUNT_MASK) >> REF_COUNT_SHIFT
    }

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
            .field("is_output_taken", &self.is_output_taken())
            .field("ref_count", &self.ref_count())
            .finish()
    }
}
