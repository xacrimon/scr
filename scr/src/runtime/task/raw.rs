use std::any::Any;
use std::fmt;
use std::mem::ManuallyDrop;
use std::panic::{self, AssertUnwindSafe, Location};
use std::pin::Pin;
use std::ptr::NonNull;
use std::task::{Context, ContextBuilder, LocalWaker, Poll};
use std::{cell, mem};

use crate::runtime::context;
use crate::runtime::stub_waker::stub_waker;
use crate::runtime::task::state::{CallerRef, State, TransitionToIdle, TransitionToRunning};
use crate::runtime::task::waker::waker_ref;
use crate::runtime::task::{Id, Runnable, Task};

pub(super) type Panic = Box<dyn Any + Send + 'static>;

#[repr(C)]
struct Cell<T> {
    header: Header,

    future: cell::UnsafeCell<ManuallyDrop<T>>,
}

pub(crate) struct Header {
    pub(super) state: State,

    vtable: &'static Vtable,

    pub(super) id: Id,

    pub(super) spawned_at: &'static Location<'static>,

    waker: cell::UnsafeCell<Option<LocalWaker>>,

    panic: cell::UnsafeCell<Option<Panic>>,
}

struct Vtable {
    poll: unsafe fn(NonNull<Header>),

    dealloc: unsafe fn(NonNull<Header>),
}

fn vtable<T: Future<Output = ()>>() -> &'static Vtable {
    &Vtable {
        poll: poll::<T>,
        dealloc: dealloc::<T>,
    }
}

#[derive(Clone, Copy)]
pub(crate) struct RawTask {
    ptr: NonNull<Header>,
}

impl RawTask {
    pub(super) fn new<T>(future: T, id: Id, spawned_at: &'static Location<'static>) -> RawTask
    where
        T: Future<Output = ()>,
    {
        let ptr = Box::into_raw(Box::new(Cell {
            header: Header {
                state: State::new(),
                vtable: vtable::<T>(),
                id,
                spawned_at,
                waker: cell::UnsafeCell::new(None),
                panic: cell::UnsafeCell::new(None),
            },
            future: cell::UnsafeCell::new(ManuallyDrop::new(future)),
        }));

        RawTask {
            ptr: unsafe { NonNull::new_unchecked(ptr.cast()) },
        }
    }

    pub(super) unsafe fn from_raw(ptr: NonNull<Header>) -> RawTask {
        RawTask { ptr }
    }

    pub(super) fn header_ptr(self) -> NonNull<Header> {
        self.ptr
    }

    pub(super) fn header(self) -> &'static Header {
        unsafe { self.ptr.as_ref() }
    }

    pub(super) fn state(self) -> &'static State {
        &self.header().state
    }

    pub(super) fn poll(self) {
        unsafe { (self.header().vtable.poll)(self.ptr) }
    }

    pub(super) fn shutdown(self) {
        self.state().set_cancelled();
        self.poll();
    }

    pub(super) fn dealloc(self) {
        unsafe { (self.header().vtable.dealloc)(self.ptr) }
    }

    pub(super) fn ref_inc(self) {
        self.state().ref_inc();
    }

    pub(super) fn drop_reference(self) {
        if self.state().ref_dec() {
            self.dealloc();
        }
    }

    fn schedule(self) {
        let runnable = Runnable(unsafe { Task::from_raw(self.ptr) });

        context::with_handle(|handle| handle.schedule(runnable));
    }

    fn notify(self) {
        if self.state().transition_to_notified(CallerRef::Kept) {
            self.schedule();
        }
    }

    pub(super) fn wake_by_val(self) {
        if self.state().transition_to_notified(CallerRef::Consumed) {
            self.schedule();
        } else {
            self.drop_reference();
        }
    }

    pub(super) fn wake_by_ref(self) {
        self.notify();
    }

    pub(super) fn remote_abort(self) {
        self.state().set_cancelled();
        self.notify();
    }
}

impl Header {
    pub(super) unsafe fn set_waker(&self, waker: Option<LocalWaker>) {
        unsafe { *self.waker.get() = waker };
    }

    pub(super) unsafe fn will_wake(&self, waker: &LocalWaker) -> bool {
        match unsafe { &*self.waker.get() } {
            Some(stored) => stored.will_wake(waker),
            None => false,
        }
    }

    fn wake_join(&self) {
        if let Some(waker) = unsafe { &*self.waker.get() } {
            waker.wake_by_ref();
        }
    }

    fn set_panic(&self, panic: Option<Panic>) {
        unsafe { *self.panic.get() = panic };
    }

    pub(super) fn take_panic(&self) -> Option<Panic> {
        unsafe { (*self.panic.get()).take() }
    }
}

impl fmt::Debug for Header {
    fn fmt(&self, fmt: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt.debug_struct("Task")
            .field("id", &self.id)
            .field("state", &self.state)
            .finish()
    }
}

pub(super) struct TaskIdGuard {
    parent: Option<Id>,
}

impl TaskIdGuard {
    pub(super) fn enter(id: Id) -> TaskIdGuard {
        TaskIdGuard {
            parent: context::set_current_task_id(Some(id)),
        }
    }
}

impl Drop for TaskIdGuard {
    fn drop(&mut self) {
        context::set_current_task_id(self.parent);
    }
}

unsafe fn future_of<'a, T>(ptr: NonNull<Header>) -> &'a cell::UnsafeCell<ManuallyDrop<T>> {
    unsafe { &ptr.cast::<Cell<T>>().as_ref().future }
}

unsafe fn drop_future<T>(ptr: NonNull<Header>) -> Option<Panic> {
    panic::catch_unwind(AssertUnwindSafe(|| unsafe {
        ManuallyDrop::drop(&mut *future_of::<T>(ptr).get())
    }))
    .err()
}

unsafe fn poll<T: Future<Output = ()>>(ptr: NonNull<Header>) {
    let raw = unsafe { RawTask::from_raw(ptr) };

    let _guard = TaskIdGuard::enter(raw.header().id);

    match raw.state().transition_to_running() {
        TransitionToRunning::Polled => {
            let local_waker = unsafe { waker_ref(&raw) };
            let waker = stub_waker();
            let cx = ContextBuilder::from_waker(waker)
                .local_waker(&local_waker)
                .build();

            if unsafe { poll_future::<T>(ptr, cx) }.is_ready() {
                unsafe { complete(ptr, false) };
                return;
            }

            match raw.state().transition_to_idle() {
                TransitionToIdle::Idle => raw.drop_reference(),
                TransitionToIdle::Notified => raw.schedule(),
                TransitionToIdle::Cancelled => unsafe { cancel::<T>(ptr) },
            }
        }
        TransitionToRunning::Cancelled => unsafe { cancel::<T>(ptr) },
        TransitionToRunning::Dead => raw.drop_reference(),
    }
}

unsafe fn poll_future<T: Future<Output = ()>>(
    ptr: NonNull<Header>,
    mut cx: Context<'_>,
) -> Poll<()> {
    let polled = panic::catch_unwind(AssertUnwindSafe(|| unsafe {
        let future = &mut *future_of::<T>(ptr).get();

        Pin::new_unchecked(&mut **future).poll(&mut cx)
    }));

    let panicked = match polled {
        Ok(Poll::Pending) => return Poll::Pending,
        Ok(Poll::Ready(())) => None,
        Err(panic) => Some(panic),
    };

    let dropped = unsafe { drop_future::<T>(ptr) };

    unsafe { ptr.as_ref() }.set_panic(match (panicked, dropped) {
        (Some(panicked), Some(dropped)) => {
            drop_panic(dropped);
            Some(panicked)
        }
        (panicked, dropped) => panicked.or(dropped),
    });

    Poll::Ready(())
}

unsafe fn cancel<T>(ptr: NonNull<Header>) {
    let dropped = unsafe { drop_future::<T>(ptr) };

    unsafe { ptr.as_ref() }.set_panic(dropped);
    unsafe { complete(ptr, true) };
}

unsafe fn complete(ptr: NonNull<Header>, cancelled: bool) {
    let raw = unsafe { RawTask::from_raw(ptr) };
    let snapshot = raw.state().transition_to_complete(cancelled);

    let caught = panic::catch_unwind(AssertUnwindSafe(|| {
        if snapshot.is_join_interested() {
            raw.header().wake_join();
        } else {
            drop(raw.header().take_panic());
        }
    }));

    if let Err(panic) = caught {
        drop_panic(panic);
    }

    if raw.state().transition_to_terminal(release(ptr)) {
        raw.dealloc();
    }
}

#[cold]
#[inline(never)]
fn drop_panic(panic: Panic) {
    drop(panic);
}

fn release(ptr: NonNull<Header>) -> usize {
    if !unsafe { ptr.as_ref() }.state.load().is_owned() {
        return 1;
    }

    context::with_handle(|handle| match handle.release(ptr) {
        Some(task) => {
            mem::forget(task);
            2
        }
        None => 1,
    })
}

unsafe fn dealloc<T>(ptr: NonNull<Header>) {
    let cell = ptr.cast::<Cell<T>>();

    if !unsafe { ptr.as_ref() }.state.load().is_complete() {
        unsafe { ManuallyDrop::drop(&mut *cell.as_ref().future.get()) };
    }

    drop(unsafe { Box::from_raw(cell.as_ptr()) });
}

#[test]
fn header_fits_in_a_cache_line() {
    assert!(size_of::<Header>() <= 8 * size_of::<*const ()>());
}

#[test]
fn vtable_is_two_words() {
    assert_eq!(size_of::<Vtable>(), 2 * size_of::<*const ()>());
}
