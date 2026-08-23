use crate::runtime::task;
use std::cell::Cell;

pub(crate) struct ThreadContext {
    rt: Cell<Option<()>>,
    task_id: Cell<Option<task::Id>>,
}

impl ThreadContext {
    const fn empty() -> Self {
        Self {
            rt: Cell::new(None),
            task_id: Cell::new(None),
        }
    }
}

thread_local! {
    static THREAD_CONTEXT: ThreadContext = const { ThreadContext::empty() };
}

pub(crate) fn set_rt<T>(rt: Option<()>) {
    THREAD_CONTEXT.with(|ctx| ctx.rt.set(rt));
}

pub(crate) fn get_rt() -> Option<()> {
    THREAD_CONTEXT.with(|ctx| ctx.rt.get())
}

pub(crate) fn set_task_id<T>(task_id: Option<task::Id>)
{
    THREAD_CONTEXT.with(|ctx| ctx.task_id.set(task_id));
}

pub(crate) fn get_task_id() -> Option<task::Id> {
    THREAD_CONTEXT.with(|ctx| ctx.task_id.get())
}