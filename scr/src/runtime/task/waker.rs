use std::marker::PhantomData;
use std::mem::ManuallyDrop;
use std::ops::Deref;
use std::ptr::NonNull;
use std::task::{LocalWaker, RawWaker, RawWakerVTable};

use crate::runtime::task::{Header, RawTask};

pub(super) struct WakerRef<'a> {
    waker: ManuallyDrop<LocalWaker>,
    _marker: PhantomData<&'a ()>,
}

impl Deref for WakerRef<'_> {
    type Target = LocalWaker;

    fn deref(&self) -> &Self::Target {
        &self.waker
    }
}

pub(super) unsafe fn waker_ref(task: &RawTask) -> WakerRef<'_> {
    let waker = unsafe { LocalWaker::from_raw(raw_waker(task.header_ptr())) };

    WakerRef {
        waker: ManuallyDrop::new(waker),
        _marker: PhantomData,
    }
}

unsafe fn task_of(ptr: *const ()) -> RawTask {
    unsafe { RawTask::from_raw(NonNull::new_unchecked(ptr.cast_mut().cast())) }
}

fn raw_waker(ptr: NonNull<Header>) -> RawWaker {
    RawWaker::new(ptr.as_ptr().cast_const().cast(), &VTABLE)
}

static VTABLE: RawWakerVTable = RawWakerVTable::new(clone, wake, wake_by_ref, drop_waker);

unsafe fn clone(ptr: *const ()) -> RawWaker {
    let task = unsafe { task_of(ptr) };
    task.ref_inc();
    raw_waker(task.header_ptr())
}

unsafe fn wake(ptr: *const ()) {
    unsafe { task_of(ptr) }.wake_by_val();
}

unsafe fn wake_by_ref(ptr: *const ()) {
    unsafe { task_of(ptr) }.wake_by_ref();
}

unsafe fn drop_waker(ptr: *const ()) {
    unsafe { task_of(ptr) }.drop_reference();
}
