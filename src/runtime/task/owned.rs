use std::cell::UnsafeCell;
use std::marker::PhantomData;
use std::mem;
use std::panic::Location;
use std::ptr::NonNull;

use slab::Slab;

use crate::runtime::task::{Header, Id, JoinHandle, Runnable, Task, new_task};

const BASE_TASK_CAPACITY: usize = 4096;

pub(crate) struct OwnedTasks {
    inner: UnsafeCell<Inner>,
    _marker: PhantomData<*const ()>,
}

struct Inner {
    tasks: Slab<Task>,
    closed: bool,
}

impl OwnedTasks {
    pub(crate) fn new() -> Self {
        Self {
            inner: UnsafeCell::new(Inner {
                tasks: Slab::with_capacity(BASE_TASK_CAPACITY),
                closed: false,
            }),
            _marker: PhantomData,
        }
    }

    pub(crate) fn bind<T>(
        &self,
        future: T,
        spawned_at: &'static Location<'static>,
    ) -> (JoinHandle, Option<Runnable>)
    where
        T: Future<Output = ()> + 'static,
    {
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
                drop(runnable);
                task.shutdown();

                (join, None)
            }
        }
    }

    pub(crate) fn close_and_shutdown_all(&self) {
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

    pub(crate) fn remove(&self, header: NonNull<Header>) -> Option<Task> {
        let slot = {
            let task = unsafe { header.as_ref() };

            if !task.state.unset_owned() {
                return None;
            }

            task.id.slot()
        };

        self.with_inner(|inner| {
            debug_assert!(
                inner
                    .tasks
                    .get(slot)
                    .is_some_and(|t| t.header_ptr() == header),
                "task released through the wrong runtime"
            );

            inner.tasks.try_remove(slot)
        })
    }

    pub(crate) fn is_foreign(&self, header: NonNull<Header>) -> bool {
        let task = unsafe { header.as_ref() };

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

    fn with_inner<F, T>(&self, f: F) -> T
    where
        F: FnOnce(&mut Inner) -> T,
    {
        f(unsafe { &mut *self.inner.get() })
    }
}
