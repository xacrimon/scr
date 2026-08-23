use crate::runtime::sched::Handle;
use crate::runtime::task::Id;

use std::cell::Cell;
use std::rc::Rc;

struct Context {
    /// The runtime the current thread is entered into, if any.
    handle: Cell<Option<Rc<Handle>>>,

    /// The task currently being polled, if any.
    current_task_id: Cell<Option<Id>>,
}

impl Context {
    const fn empty() -> Self {
        Self {
            handle: Cell::new(None),
            current_task_id: Cell::new(None),
        }
    }
}

thread_local! {
    static CONTEXT: Context = const { Context::empty() };
}

/// Sets the task id of the task currently being polled, returning the previous
/// value so that the caller can restore it.
pub(crate) fn set_current_task_id(id: Option<Id>) -> Option<Id> {
    CONTEXT.with(|ctx| ctx.current_task_id.replace(id))
}

/// Returns the id of the task currently being polled, if any.
pub(crate) fn current_task_id() -> Option<Id> {
    CONTEXT.with(|ctx| ctx.current_task_id.get())
}

/// Returns a handle to the runtime the current thread is entered into.
pub(crate) fn try_current_handle() -> Option<Rc<Handle>> {
    CONTEXT.with(|ctx| {
        // `Cell` has no `get` for non-`Copy` values, so take the handle out and
        // put a clone back.
        let handle = ctx.handle.take();
        let cloned = handle.clone();
        ctx.handle.set(handle);
        cloned
    })
}

/// Returns a handle to the runtime the current thread is entered into.
///
/// # Panics
///
/// Panics if called from outside of a runtime.
#[track_caller]
pub(crate) fn current_handle() -> Rc<Handle> {
    try_current_handle()
        .expect("there is no reactor running, must be called from the context of a `scr` runtime")
}

/// Restores the previously entered runtime, if any, when dropped.
#[must_use]
pub(crate) struct EnterGuard {
    prev: Option<Rc<Handle>>,
}

impl Drop for EnterGuard {
    fn drop(&mut self) {
        let prev = self.prev.take();
        CONTEXT.with(|ctx| ctx.handle.set(prev));
    }
}

/// Enters the runtime, so that `spawn` and friends can find it.
pub(crate) fn enter_runtime(handle: &Rc<Handle>) -> EnterGuard {
    let prev = CONTEXT.with(|ctx| ctx.handle.replace(Some(Rc::clone(handle))));

    EnterGuard { prev }
}
