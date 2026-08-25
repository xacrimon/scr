//! The waker handed to the future passed to [`Runtime::block_on`].
//!
//! [`Runtime::block_on`]: crate::Runtime::block_on

use std::cell::Cell;
use std::rc::Rc;
use std::task::{LocalWaker, RawWaker, RawWakerVTable, Waker};

/// A flag recording whether the blocked-on future has been woken.
///
/// Like the task wakers, this waker is thread affine: it is only ever woken
/// from the thread running `block_on`, either by the future itself or by a task
/// polled by the same loop.
pub(super) struct Signal {
    notified: Rc<Cell<bool>>,
}

impl Signal {
    pub(super) fn new() -> Signal {
        Signal {
            // Start notified so that the future is polled once up front.
            notified: Rc::new(Cell::new(true)),
        }
    }

    pub(super) fn waker(&self) -> Waker {
        let ptr = Rc::into_raw(Rc::clone(&self.notified)).cast::<()>();

        // Safety: the vtable below only ever handles pointers produced by
        // `Rc::into_raw` on an `Rc<Cell<bool>>`.
        unsafe { Waker::from_raw(RawWaker::new(ptr, &VTABLE)) }
    }

    pub(super) fn waker_local(&self) -> LocalWaker {
        let ptr = Rc::into_raw(Rc::clone(&self.notified)).cast::<()>();

        // Safety: the vtable below only ever handles pointers produced by
        // `Rc::into_raw` on an `Rc<Cell<bool>>`.
        unsafe { LocalWaker::from_raw(RawWaker::new(ptr, &VTABLE_LOCAL)) }
    }

    pub(super) fn is_notified(&self) -> bool {
        self.notified.get()
    }

    /// Returns whether the signal was notified, clearing it.
    pub(super) fn take_notified(&self) -> bool {
        self.notified.replace(false)
    }
}

static VTABLE: RawWakerVTable = RawWakerVTable::new(clone, wake, wake_by_ref, drop_waker);

unsafe fn clone(_ptr: *const ()) -> RawWaker {
    panic!("clone");
}

unsafe fn wake(_ptr: *const ()) {
    panic!("wake");
}

unsafe fn wake_by_ref(_ptr: *const ()) {
    panic!("wake_by_ref");
}

unsafe fn drop_waker(_ptr: *const ()) {
    panic!("drop_waker");
}

static VTABLE_LOCAL: RawWakerVTable = RawWakerVTable::new(clone_local, wake_local, wake_by_ref_local, drop_waker_local);

unsafe fn clone_local(ptr: *const ()) -> RawWaker {
    unsafe { Rc::increment_strong_count(ptr.cast::<Cell<bool>>()) };
    RawWaker::new(ptr, &VTABLE_LOCAL)
}

unsafe fn wake_local(ptr: *const ()) {
    let notified = unsafe { Rc::from_raw(ptr.cast::<Cell<bool>>()) };
    notified.set(true);
}

unsafe fn wake_by_ref_local(ptr: *const ()) {
    unsafe { (*ptr.cast::<Cell<bool>>()).set(true) };
}

unsafe fn drop_waker_local(ptr: *const ()) {
    drop(unsafe { Rc::from_raw(ptr.cast::<Cell<bool>>()) });
}