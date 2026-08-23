//! The registry of tasks spawned on a runtime.
//!
//! Tasks live in a slab, and each one records the slot it was put in as its id.
//! Removal is therefore a single indexed lookup, and the id a task reports is
//! the same number the registry files it under.
//!
//! The registry can be closed, which stops new tasks from being added while a
//! runtime shuts down.

use std::cell::UnsafeCell;
use std::marker::PhantomData;
use std::mem;
use std::panic::Location;
use std::ptr::NonNull;

use slab::Slab;

use crate::runtime::task::{Header, Id, JoinHandle, Runnable, Task, new_task};

pub(crate) struct OwnedTasks {
    inner: UnsafeCell<Inner>,

    /// The registry is only ever touched from the thread running the runtime.
    _not_send_or_sync: PhantomData<*const ()>,
}

struct Inner {
    tasks: Slab<Task>,
    closed: bool,
}

impl OwnedTasks {
    pub(crate) fn new() -> OwnedTasks {
        OwnedTasks {
            inner: UnsafeCell::new(Inner {
                tasks: Slab::new(),
                closed: false,
            }),
            _not_send_or_sync: PhantomData,
        }
    }

    /// Creates a task for `future` and registers it, returning its
    /// `JoinHandle` and the `Runnable` that should be queued.
    ///
    /// A task bound while the registry is closed is shut down instead of
    /// registered, and no `Runnable` is returned.
    pub(crate) fn bind<T>(
        &self,
        future: T,
        spawned_at: &'static Location<'static>,
    ) -> (JoinHandle<T::Output>, Option<Runnable>)
    where
        T: Future + 'static,
        T::Output: 'static,
    {
        // The slot is reserved before the task is created, because the key it
        // hands out becomes the id baked into the allocation. Creating a task
        // allocates but runs no user code, so it cannot re-enter the registry.
        let bound = self.with_inner(|inner| {
            let entry = inner.tasks.vacant_entry();
            let (task, runnable, join) = new_task(future, Id::from_slot(entry.key()), spawned_at);

            if inner.closed {
                return Err((task, runnable, join));
            }

            task.header().state.set_owned();
            entry.insert(task);

            Ok((join, runnable))
        });

        match bound {
            Ok((join, runnable)) => (join, Some(runnable)),
            Err((task, runnable, join)) => {
                // The task will never be queued, so its `Runnable` goes away
                // here. Shutting the task down drops the future, which runs
                // user code and so cannot happen under the borrow above.
                drop(runnable);
                task.shutdown();

                (join, None)
            }
        }
    }

    /// Closes the registry and shuts down every task in it.
    pub(crate) fn close_and_shutdown_all(&self) {
        // Every task comes out in one pass, marked as registered nowhere.
        // Shutting one down drops its future, which runs user code that can
        // reach this type again, so it happens outside the borrow.
        let tasks = self.with_inner(|inner| {
            inner.closed = true;

            let tasks = mem::take(&mut inner.tasks);
            for (_, task) in &tasks {
                task.header().state.unset_owned();
            }

            tasks
        });

        for (_, task) in tasks {
            task.shutdown();
        }
    }

    /// Takes a task out of the registry, handing back the reference it held.
    ///
    /// Returns `None` if the task is not in this registry, which is not an
    /// error: a task bound while the registry was closed was never added, and
    /// [`OwnedTasks::close_and_shutdown_all`] takes tasks out before shutting
    /// them down.
    pub(crate) fn remove(&self, header: NonNull<Header>) -> Option<Task> {
        let slot = {
            // Safety: the caller holds a reference, so the task is live.
            let task = unsafe { header.as_ref() };

            // Clearing the bit here is what makes this happen at most once, so
            // only one caller ever reaches the slot below.
            if !task.state.unset_owned() {
                return None;
            }

            task.id.slot()
        };

        self.with_inner(|inner| match inner.tasks.get(slot) {
            Some(task) if task.header_ptr() == header => Some(inner.tasks.remove(slot)),
            _ => {
                // The task belongs to another runtime, which can only happen if
                // it was woken while this one was entered. Leaving the slot
                // alone leaks the other runtime's entry, where evicting it
                // would take an unrelated task down with it.
                debug_assert!(false, "task released through the wrong runtime");
                None
            }
        })
    }

    /// Returns `true` if the task is registered somewhere other than here.
    pub(crate) fn is_foreign(&self, header: NonNull<Header>) -> bool {
        // Safety: the caller holds a reference, so the task is live.
        let task = unsafe { header.as_ref() };

        // A task in no registry at all is being shut down, and belongs to
        // whoever is doing the shutting down.
        if !task.state.load().is_owned() {
            return false;
        }

        self.with_inner(|inner| match inner.tasks.get(task.id.slot()) {
            Some(task) => task.header_ptr() != header,
            None => true,
        })
    }

    #[cfg(test)]
    pub(crate) fn is_closed(&self) -> bool {
        self.with_inner(|inner| inner.closed)
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.with_inner(|inner| inner.tasks.is_empty())
    }

    #[inline]
    fn with_inner<F, T>(&self, f: F) -> T
    where
        F: FnOnce(&mut Inner) -> T,
    {
        // Safety: this type is not `Sync`, so two of these cannot overlap
        // across threads, and no caller in this file runs anything that could
        // re-enter the registry while the borrow is live.
        f(unsafe { &mut *self.inner.get() })
    }
}
