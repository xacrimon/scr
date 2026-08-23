use std::cell::UnsafeCell;
use std::collections::VecDeque;
use std::marker::PhantomData;
use std::panic::Location;
use std::ptr::NonNull;

use crate::runtime::task::{Header, JoinHandle, OwnedTasks, Runnable, Task};

const BASE_QUEUE_SIZE: usize = 32;

/// The state a runtime shares with every task spawned on it.
pub(crate) struct Handle {
    /// Tasks that have been woken and are waiting to be polled.
    queue: Queue,

    /// Every task spawned on this runtime, so that they can all be shut down
    /// when it goes away.
    owned: OwnedTasks,
}

impl Handle {
    pub(crate) fn new() -> Handle {
        Handle {
            queue: Queue::new(),
            owned: OwnedTasks::new(),
        }
    }

    /// Spawns `future` onto the runtime.
    pub(crate) fn spawn<F>(
        &self,
        future: F,
        spawned_at: &'static Location<'static>,
    ) -> JoinHandle<F::Output>
    where
        F: Future + 'static,
        F::Output: 'static,
    {
        let (join, runnable) = self.owned.bind(future, spawned_at);

        // A task bound while the registry is closed is already shut down, and
        // must not be queued.
        if let Some(runnable) = runnable {
            self.schedule(runnable);
        }

        join
    }

    /// Puts a woken task on the run queue.
    pub(crate) fn schedule(&self, runnable: Runnable) {
        debug_assert!(
            !self.owned.is_foreign(runnable.header_ptr()),
            "a task was woken while a different runtime was entered"
        );

        self.queue.push(runnable);
    }

    /// Takes the task at the front of the run queue.
    pub(crate) fn next_task(&self) -> Option<Runnable> {
        self.queue.pop()
    }

    /// Takes a task that has completed out of the registry, handing back the
    /// reference the registry held.
    pub(crate) fn release(&self, header: NonNull<Header>) -> Option<Task> {
        self.owned.remove(header)
    }

    /// Shuts down every task on this runtime and drains the run queue.
    pub(crate) fn shutdown(&self) {
        while !self.owned.is_empty() || !self.queue.is_empty() {
            // Closing the registry means that a task spawned by a destructor
            // running below is shut down immediately, so this loop terminates.
            self.owned.close_and_shutdown_all();

            while let Some(runnable) = self.queue.pop() {
                drop(runnable);
            }
        }
    }
}

/// The run queue.
///
/// With no work stealing and no remote injection this is just a `VecDeque`
/// behind an `UnsafeCell`. No reference into it is ever held across a call into
/// user code, so the `&mut` each method takes never overlaps with another.
struct Queue {
    inner: UnsafeCell<VecDeque<Runnable>>,
    _not_send_or_sync: PhantomData<*const ()>,
}

impl Queue {
    fn new() -> Queue {
        Queue {
            inner: UnsafeCell::new(VecDeque::with_capacity(BASE_QUEUE_SIZE)),
            _not_send_or_sync: PhantomData,
        }
    }

    fn push(&self, runnable: Runnable) {
        // Safety: see the note on the type.
        unsafe { (*self.inner.get()).push_back(runnable) };
    }

    fn pop(&self) -> Option<Runnable> {
        // Safety: see the note on the type.
        unsafe { (*self.inner.get()).pop_front() }
    }

    fn is_empty(&self) -> bool {
        // Safety: see the note on the type.
        unsafe { (*self.inner.get()).is_empty() }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::cell::Cell;
    use std::rc::Rc;

    /// Binding a task to a closed registry must shut the task down without
    /// polling it, without queueing it, and without ever adding it to the
    /// registry.
    #[test]
    fn bind_on_closed_registry_shuts_the_task_down() {
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
            handle.spawn(
                async move {
                    let _guard = guard;
                    polled.set(true);
                    std::future::pending::<()>().await;
                },
                Location::caller(),
            )
        };

        assert!(
            !polled.get(),
            "a task bound to a closed registry must not be polled"
        );
        assert!(dropped.get(), "its future must still be dropped");
        assert!(handle.queue.is_empty(), "it must not be queued");
        assert!(handle.owned.is_empty(), "it must not be in the registry");
        assert!(join.is_finished());
    }
}
