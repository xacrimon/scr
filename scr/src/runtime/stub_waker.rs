use std::ptr;
use std::task::{RawWaker, RawWakerVTable, Waker};

pub(super) fn stub_waker() -> Waker {
    unsafe { Waker::from_raw(raw_stub_waker()) }
}

fn raw_stub_waker() -> RawWaker {
    RawWaker::new(ptr::null(), &VTABLE)
}

static VTABLE: RawWakerVTable = RawWakerVTable::new(clone, wake, wake_by_ref, drop_waker);

unsafe fn clone(_ptr: *const ()) -> RawWaker {
    raw_stub_waker()
}

unsafe fn wake(_ptr: *const ()) {
    unsupported("wake");
}

unsafe fn wake_by_ref(_ptr: *const ()) {
    unsupported("wake_by_ref");
}

unsafe fn drop_waker(_ptr: *const ()) {}

#[cold]
#[inline(never)]
fn unsupported(op: &str) -> ! {
    panic!(
        "`Waker::{op}` called on a runtime that only supports `LocalWaker`; \
         this future should be using `cx.local_waker()`, not `cx.waker()`"
    );
}
