use std::cell::{RefCell, UnsafeCell};
use std::collections::VecDeque;
use std::io;
use std::marker::PhantomData;
use std::panic::Location;
use std::ptr::NonNull;
use std::rc::Rc;

use crate::runtime::driver::Driver;
use crate::runtime::task::{Header, JoinHandle, OwnedTasks, Runnable, Task};
use crate::runtime::timers::Timers;
use crate::util::Rand32;

const BASE_QUEUE_CAPACITY: usize = 1024;

pub(crate) struct Handle {
    queue: Queue,
    owned: OwnedTasks,
    /// Shared with every socket, which needs it in its own `Drop` — after the
    /// runtime may already have gone.
    driver: Rc<Driver>,
    /// Shared with every armed timer, for the same reason.
    timers: Rc<Timers>,
    rng: RefCell<Rand32>,
}

impl Handle {
    pub(crate) fn new() -> io::Result<Handle> {
        Ok(Handle {
            queue: Queue::new(),
            owned: OwnedTasks::new(),
            driver: Rc::new(Driver::new()?),
            timers: Rc::new(Timers::new()),
            rng: RefCell::new(Rand32::with_random_seed()),
        })
    }

    pub(crate) fn driver(&self) -> &Rc<Driver> {
        &self.driver
    }

    pub(crate) fn timers(&self) -> &Rc<Timers> {
        &self.timers
    }

    pub(crate) fn rng(&self) -> &RefCell<Rand32> {
        &self.rng
    }

    pub(crate) fn queue_is_empty(&self) -> bool {
        self.queue.is_empty()
    }

    pub(crate) fn spawn<F>(&self, future: F, spawned_at: &'static Location<'static>) -> JoinHandle
    where
        F: Future<Output = ()> + 'static,
    {
        let (join, runnable) = self.owned.bind(future, spawned_at);

        if let Some(runnable) = runnable {
            self.schedule(runnable);
        }

        join
    }

    pub(crate) fn schedule(&self, runnable: Runnable) {
        debug_assert!(
            !self.owned.is_foreign(runnable.header_ptr()),
            "a task was woken while a different runtime was entered"
        );

        self.queue.push(runnable);
    }

    pub(crate) fn next_task(&self) -> Option<Runnable> {
        self.queue.pop()
    }

    pub(crate) fn release(&self, header: NonNull<Header>) -> Option<Task> {
        self.owned.remove(header)
    }

    pub(crate) fn shutdown(&self) {
        while !self.owned.is_empty() || !self.queue.is_empty() {
            self.owned.close_and_shutdown_all();

            while let Some(runnable) = self.queue.pop() {
                drop(runnable);
            }
        }

        // Only now that every future — and so every socket and every operation
        // future — has been dropped, because dropping them is what hands their
        // buffers to the driver to hold.
        self.driver.shutdown();
    }
}

struct Queue {
    inner: UnsafeCell<VecDeque<Runnable>>,
    _not_send_or_sync: PhantomData<*const ()>,
}

impl Queue {
    fn new() -> Queue {
        Queue {
            inner: UnsafeCell::new(VecDeque::with_capacity(BASE_QUEUE_CAPACITY)),
            _not_send_or_sync: PhantomData,
        }
    }

    fn push(&self, runnable: Runnable) {
        unsafe { (*self.inner.get()).push_back(runnable) };
    }

    fn pop(&self) -> Option<Runnable> {
        unsafe { (*self.inner.get()).pop_front() }
    }

    fn is_empty(&self) -> bool {
        unsafe { (*self.inner.get()).is_empty() }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::cell::Cell;
    use std::rc::Rc;

    #[test]
    fn bind_on_closed_registry_shuts_the_task_down() {
        struct OnDrop(Rc<Cell<bool>>);
        impl Drop for OnDrop {
            fn drop(&mut self) {
                self.0.set(true);
            }
        }

        let handle = Handle::new().expect("Handle::new");
        handle.owned.close_and_shutdown_all();
        assert!(handle.owned.is_closed());

        let dropped = Rc::new(Cell::new(false));
        let polled = Rc::new(Cell::new(false));

        let join = {
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
