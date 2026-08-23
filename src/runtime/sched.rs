use std::cell::UnsafeCell;
use std::collections::VecDeque;
use std::marker::PhantomData;
use std::rc::Rc;

use crate::runtime::task::{self, JoinHandle, Notified, OwnedTasks, Schedule, SpawnLocation, Task};

const BASE_QUEUE_SIZE: usize = 32;

/// The run queue.
///
/// A single threaded runtime has no work stealing and no remote injection, so
/// this is just a `VecDeque` behind an `UnsafeCell`. No reference into the
/// queue is ever held across a call into user code, so the `&mut` handed out by
/// each method never overlaps with another.
pub(crate) struct Queue {
    inner: UnsafeCell<VecDeque<Notified<Rc<Handle>>>>,
    _marker: PhantomData<*const ()>,
}

impl Queue {
    pub(crate) fn new() -> Self {
        Self {
            inner: UnsafeCell::new(VecDeque::with_capacity(BASE_QUEUE_SIZE)),
            _marker: PhantomData,
        }
    }

    pub(crate) fn push(&self, runnable: Notified<Rc<Handle>>) {
        unsafe {
            (*self.inner.get()).push_back(runnable);
        }
    }

    pub(crate) fn pop(&self) -> Option<Notified<Rc<Handle>>> {
        unsafe { (*self.inner.get()).pop_front() }
    }

    pub(crate) fn len(&self) -> usize {
        unsafe { (*self.inner.get()).len() }
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// The scheduler state shared by every handle to a runtime.
///
/// This is the `S` of the task module: a task holds an `Rc<Handle>` and uses it
/// to schedule and release itself.
pub(crate) struct Handle {
    /// Tasks that have been notified and are waiting to be polled.
    pub(crate) queue: Queue,

    /// Every task spawned on this runtime, so that they can be shut down when
    /// the runtime goes away.
    pub(crate) owned: OwnedTasks<Rc<Handle>>,
}

impl Handle {
    pub(crate) fn new() -> Rc<Handle> {
        Rc::new(Handle {
            queue: Queue::new(),
            owned: OwnedTasks::new(),
        })
    }

    /// Spawns a task onto the runtime.
    #[track_caller]
    pub(crate) fn spawn<F>(me: &Rc<Handle>, future: F) -> JoinHandle<F::Output>
    where
        F: Future + 'static,
        F::Output: 'static,
    {
        let id = task::Id::next();
        let (join, notified) = me
            .owned
            .bind(future, Rc::clone(me), id, SpawnLocation::capture());

        if let Some(notified) = notified {
            me.queue.push(notified);
        }

        join
    }

    /// Shuts down every task owned by this runtime and drains the run queue.
    pub(crate) fn shutdown(&self) {
        while !self.owned.is_empty() || !self.queue.is_empty() {
            // Closing the list means that any task spawned from a destructor
            // running below is shut down immediately, so this loop terminates.
            self.owned.close_and_shutdown_all();

            while let Some(task) = self.queue.pop() {
                drop(task);
            }
        }
    }
}

impl Schedule for Rc<Handle> {
    fn release(&self, task: &Task<Self>) -> Option<Task<Self>> {
        self.owned.remove(task)
    }

    fn schedule(&self, task: Notified<Self>) {
        self.queue.push(task);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::cell::Cell;

    /// Binding a task to a closed `OwnedTasks` must shut the task down without
    /// polling it, without queueing it, and without ever linking it into the
    /// list. This is the one path on which `OwnedTasks::remove` is called for a
    /// task that was never inserted.
    #[test]
    fn bind_on_closed_list_shuts_the_task_down() {
        struct OnDrop(Rc<Cell<bool>>);
        impl Drop for OnDrop {
            fn drop(&mut self) {
                self.0.set(true);
            }
        }

        let handle = Handle::new();
        handle.owned.close_and_shutdown_all();
        assert!(handle.owned.is_closed());

        let dropped = Rc::new(Cell::new(false));
        let polled = Rc::new(Cell::new(false));

        let join = {
            // The guard is constructed here, not in the body, so that it is a
            // capture of the future: an un-polled future drops its captures,
            // but never runs its body.
            let guard = OnDrop(Rc::clone(&dropped));
            let polled = Rc::clone(&polled);
            Handle::spawn(&handle, async move {
                let _guard = guard;
                polled.set(true);
                std::future::pending::<()>().await;
            })
        };

        assert!(
            !polled.get(),
            "a task bound to a closed list must not be polled"
        );
        assert!(dropped.get(), "its future must still be dropped");
        assert!(handle.queue.is_empty(), "it must not be queued");
        assert!(
            handle.owned.is_empty(),
            "it must not be linked into the list"
        );
        assert!(join.is_finished());
    }
}
