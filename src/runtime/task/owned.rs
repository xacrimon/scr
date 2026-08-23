//! Storage for the tasks spawned on a scheduler.
//!
//! `tokio` has two containers here: a sharded, thread-safe `OwnedTasks` and a
//! `LocalOwnedTasks` for non-`Send` tasks. Only the latter shape is relevant
//! for a single threaded runtime, and even that one is an intrusive doubly
//! linked list threaded through each task's `Trailer`. That buys allocation
//! free binding, at the cost of a fair amount of raw pointer manipulation and
//! of a `!Unpin` dance to keep `noalias` off the link fields.
//!
//! Here the tasks live in a `Slab` instead, and each task records the key it
//! was inserted under in its `Trailer`. Removal stays O(1), as it was with the
//! list, but the only pointer work left is reading a `usize` out of the
//! trailer. Binding is no longer allocation free, though the slab amortizes
//! that away.
//!
//! The collection can be closed to prevent adding new tasks during shutdown of
//! the scheduler that owns it.

use std::marker::PhantomData;
use std::mem;

use slab::Slab;

use crate::runtime::task::core::{Header, Trailer};
use crate::runtime::task::{JoinHandle, Runnable, Schedule, SpawnLocation, Task};
use crate::util::UnsafeCell;

/// The key stored in a task that is not in any `OwnedTasks`.
///
/// A slab key is an index into a `Vec`, and a `Vec` can never hold `usize::MAX`
/// elements, so this cannot collide with a real key. See [`OwnedTasks::remove`]
/// for why the distinction has to be drawn.
pub(super) const NOT_OWNED: usize = usize::MAX;

/// Returns the trailer of a task, which is where its slab key is stored.
fn trailer<S: 'static>(task: &Task<S>) -> &Trailer {
    // Safety: The `Task` holds a ref-count, so the allocation is live for at
    // least as long as the borrow we return. The trailer is never borrowed
    // mutably, so handing out a shared borrow of it is fine.
    unsafe { Header::get_trailer(task.header_ptr()).as_ref() }
}

pub(crate) struct OwnedTasks<S: 'static> {
    inner: UnsafeCell<Inner<S>>,
    _not_send_or_sync: PhantomData<*const ()>,
}

struct Inner<S: 'static> {
    tasks: Slab<Task<S>>,
    closed: bool,
}

impl<S: 'static> OwnedTasks<S> {
    pub(crate) fn new() -> Self {
        Self {
            inner: UnsafeCell::new(Inner {
                tasks: Slab::new(),
                closed: false,
            }),
            _not_send_or_sync: PhantomData,
        }
    }

    /// Binds a new task to this collection, returning its `JoinHandle` and,
    /// unless the collection is closed, the `Runnable` that should be
    /// scheduled.
    pub(crate) fn bind<T>(
        &self,
        task: T,
        scheduler: S,
        id: super::Id,
        spawned_at: SpawnLocation,
    ) -> (JoinHandle<T::Output>, Option<Runnable<S>>)
    where
        S: Schedule,
        T: Future + 'static,
        T::Output: 'static,
    {
        let (task, runnable, join) = super::new_task(task, scheduler, id, spawned_at);

        if self.is_closed() {
            drop(runnable);
            task.shutdown();
            (join, None)
        } else {
            self.with_inner(|inner| {
                // Reserve the slot first: inserting moves the task into the
                // slab, so its key has to be recorded before that happens.
                let entry = inner.tasks.vacant_entry();
                trailer(&task).owned.set(entry.key());
                entry.insert(task);
            });
            (join, Some(runnable))
        }
    }

    /// Shuts down all tasks in the collection. This call also closes the
    /// collection, preventing new items from being added.
    pub(crate) fn close_and_shutdown_all(&self)
    where
        S: Schedule,
    {
        // Take every task out in a single pass, marking each one as no longer
        // owned. Shutting a task down drops its future, which runs arbitrary
        // user code and reenters this type through `Schedule::release`, so it
        // has to happen outside of `with_inner` and after the tasks have been
        // marked.
        let tasks = self.with_inner(|inner| {
            inner.closed = true;

            let tasks = mem::take(&mut inner.tasks);
            for (_, task) in &tasks {
                trailer(task).owned.set(NOT_OWNED);
            }
            tasks
        });

        for (_, task) in tasks {
            task.shutdown();
        }
    }

    /// Removes a task from the collection, returning the ref-count that the
    /// collection held.
    ///
    /// Returns `None` if the task is not in this collection, which is not an
    /// error: `release` runs for every task that completes, and two paths reach
    /// it with a task that is already out. A task bound while the collection
    /// was closed is never inserted at all, and `close_and_shutdown_all` takes
    /// a task out before shutting it down.
    pub(crate) fn remove(&self, task: &Task<S>) -> Option<Task<S>> {
        let key = match trailer(task).owned.get() {
            NOT_OWNED => return None,
            key => key,
        };

        self.with_inner(|inner| {
            // A key other than the sentinel means the task is in a collection,
            // and a task is only ever released through the scheduler stored in
            // its own `Core`, which is the handle owning this one. So the slot
            // is occupied, and it holds this very task.
            let removed = inner.tasks.remove(key);
            debug_assert_eq!(removed.header_ptr(), task.header_ptr());

            // Restore the sentinel. A task is released exactly once, so
            // nothing should read this key again; keeping the invariant
            // uniform means that if anything ever did, it would find the
            // sentinel rather than evict whichever task the slab hands this
            // recycled key to next.
            trailer(&removed).owned.set(NOT_OWNED);

            Some(removed)
        })
    }

    #[inline]
    fn with_inner<F, T>(&self, f: F) -> T
    where
        F: FnOnce(&mut Inner<S>) -> T,
    {
        // Safety: This type is not Sync, so concurrent calls of this method
        // can't happen. Furthermore, all uses of this method in this file make
        // sure that they don't call `with_inner` recursively.
        self.inner.with_mut(|ptr| unsafe { f(&mut *ptr) })
    }

    pub(crate) fn is_closed(&self) -> bool {
        self.with_inner(|inner| inner.closed)
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.with_inner(|inner| inner.tasks.is_empty())
    }
}
