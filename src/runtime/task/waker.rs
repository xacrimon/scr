//! The waker handed to a task's future while it is polled.
//!
//! A waker is a reference to a task, so its data pointer is the task's header
//! and its vtable is the single static below, shared by every task in the
//! program.

use std::mem::ManuallyDrop;
use std::ptr::NonNull;
use std::task::{RawWaker, RawWakerVTable, Waker};

use crate::runtime::task::{Header, RawTask};

/// Borrows a task as a `Waker` without touching its reference count.
///
/// The result must not be dropped, which is what the `ManuallyDrop` is for: no
/// reference was taken for it, so releasing one would be a reference too many.
/// A clone of it does take one, as usual.
///
/// # Safety
///
/// `ptr` must point at a live task, which the caller must keep alive for as
/// long as the returned waker is borrowed.
pub(super) unsafe fn waker_ref(ptr: NonNull<Header>) -> ManuallyDrop<Waker> {
    // Every task shares one vtable so that `Waker::will_wake` can compare two
    // wakers for the same task by pointer, rather than always reporting them as
    // different.
    //
    // Safety: `raw_waker` builds a waker the vtable below understands.
    ManuallyDrop::new(unsafe { Waker::from_raw(raw_waker(ptr)) })
}

fn raw_waker(ptr: NonNull<Header>) -> RawWaker {
    RawWaker::new(ptr.as_ptr().cast_const().cast(), &VTABLE)
}

static VTABLE: RawWakerVTable = RawWakerVTable::new(clone, wake, wake_by_ref, drop_waker);

/// # Safety
///
/// `ptr` must be a header pointer handed out by [`raw_waker`].
unsafe fn task_of(ptr: *const ()) -> RawTask {
    // Safety: the caller guarantees the pointer came from `raw_waker`, which
    // only ever builds one from a live task.
    unsafe { RawTask::from_raw(NonNull::new_unchecked(ptr.cast_mut().cast())) }
}

unsafe fn clone(ptr: *const ()) -> RawWaker {
    // Safety: see `task_of`.
    let task = unsafe { task_of(ptr) };
    task.ref_inc();
    raw_waker(task.header_ptr())
}

unsafe fn wake(ptr: *const ()) {
    // Safety: see `task_of`; this waker's reference is consumed.
    unsafe { task_of(ptr) }.wake_by_val();
}

unsafe fn wake_by_ref(ptr: *const ()) {
    // Safety: see `task_of`.
    unsafe { task_of(ptr) }.wake_by_ref();
}

unsafe fn drop_waker(ptr: *const ()) {
    // Safety: see `task_of`; this waker's reference is released.
    unsafe { task_of(ptr) }.drop_reference();
}
