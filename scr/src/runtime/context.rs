use std::cell::Cell;
use std::marker::PhantomData;
use std::ptr::NonNull;
use std::rc::Rc;

use crate::runtime::driver::Driver;
use crate::runtime::sched::Handle;
use crate::runtime::task::Id;
use crate::runtime::timers::Timers;

struct Context {
    /// The runtime the current thread is entered into, if any.
    handle: Cell<Option<NonNull<Handle>>>,

    /// The task currently being polled, if any.
    current_task_id: Cell<Option<Id>>,
}

impl Context {
    const fn empty() -> Context {
        Context {
            handle: Cell::new(None),
            current_task_id: Cell::new(None),
        }
    }
}

thread_local! {
    static CONTEXT: Context = const { Context::empty() };
}

/// Runs `f` with the runtime the current thread is entered into.
///
/// # Panics
///
/// Panics if the current thread is not inside a runtime. A task's wakers and
/// abort handles may only be used while the runtime that owns the task is
/// entered, which the runtime is for the whole of `block_on`, `spawn` and its
/// own shutdown.
#[track_caller]
pub(crate) fn with_handle<F, T>(f: F) -> T
where
    F: FnOnce(&Handle) -> T,
{
    let handle = CONTEXT
        .with(|ctx| ctx.handle.get())
        .expect("called from outside of a runtime");

    // Safety: the pointer was installed by `enter`, whose guard borrows the
    // runtime it points at and is still alive.
    f(unsafe { handle.as_ref() })
}

/// The io_uring driver of the runtime the current thread is entered into.
///
/// Handed out by `Rc` rather than by reference because a socket outlives the
/// call that made it and has to reach the driver from its own `Drop`, which may
/// run when no runtime is entered at all.
///
/// # Panics
///
/// Panics if the current thread is not inside a runtime.
#[track_caller]
pub(crate) fn driver() -> Rc<Driver> {
    with_handle(|handle| Rc::clone(handle.driver()))
}

/// The timer store of the runtime the current thread is entered into.
///
/// Handed out by `Rc` for the same reason as [`driver`]: an armed timer is
/// disarmed from its future's `Drop`, which can run with no runtime entered.
///
/// # Panics
///
/// Panics if the current thread is not inside a runtime.
#[track_caller]
pub(crate) fn timers() -> Rc<Timers> {
    with_handle(|handle| Rc::clone(handle.timers()))
}

/// Enters `handle` until the returned guard is dropped, so that spawning and
/// waking can find it.
pub(crate) fn enter(handle: &Handle) -> EnterGuard<'_> {
    let prev = CONTEXT.with(|ctx| ctx.handle.replace(Some(NonNull::from(handle))));

    EnterGuard {
        prev,
        _p: PhantomData,
    }
}

/// Restores the previously entered runtime, if any, when dropped.
#[must_use]
pub(crate) struct EnterGuard<'a> {
    prev: Option<NonNull<Handle>>,
    _p: PhantomData<&'a Handle>,
}

impl Drop for EnterGuard<'_> {
    fn drop(&mut self) {
        CONTEXT.with(|ctx| ctx.handle.set(self.prev));
    }
}

/// Sets the id of the task being polled, returning the previous value so that
/// the caller can restore it.
pub(crate) fn set_current_task_id(id: Option<Id>) -> Option<Id> {
    CONTEXT.with(|ctx| ctx.current_task_id.replace(id))
}

/// Returns the id of the task being polled, if any.
pub(crate) fn current_task_id() -> Option<Id> {
    CONTEXT.with(|ctx| ctx.current_task_id.get())
}
