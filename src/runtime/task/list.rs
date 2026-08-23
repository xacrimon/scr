//! Storage for the tasks spawned on a scheduler.
//!
//! `tokio` has two containers here: a sharded, thread-safe `OwnedTasks` and a
//! `LocalOwnedTasks` for non-`Send` tasks. Only the latter shape is relevant
//! for a single threaded runtime, so this module has one container, and it
//! stores the tasks in an intrusive doubly linked list threaded through each
//! task's `Trailer`. This keeps binding and releasing a task allocation free.
//!
//! The collection can be closed to prevent adding new tasks during shutdown of
//! the scheduler that owns it.

// It doesn't make sense to enforce `unsafe_op_in_unsafe_fn` for the list
// itself: the intrusive linked list naturally relies on unsafe operations and
// excessive `unsafe {}` blocks hurt readability significantly.
#![allow(unsafe_op_in_unsafe_fn)]

use std::marker::{PhantomData, PhantomPinned};
use std::mem::ManuallyDrop;
use std::ptr::{self, NonNull};

use crate::runtime::task::core::{Header, Trailer};
use crate::runtime::task::{JoinHandle, Notified, Schedule, SpawnLocation, Task};
use crate::util::UnsafeCell;

// ===== impl Pointers =====

/// Previous / next pointers for the owned-tasks list, stored in the `Trailer`.
pub(super) struct Pointers {
    inner: UnsafeCell<PointersInner>,
}

/// We do not want the compiler to put the `noalias` attribute on mutable
/// references to this type, so the type has been made `!Unpin` with a
/// `PhantomPinned` field.
///
/// Additionally, we never access the `prev` or `next` fields directly, as any
/// such access would implicitly involve the creation of a reference to the
/// field, which we want to avoid since the fields are not `!Unpin`, and would
/// hence be given the `noalias` attribute if we were to do such an access.
///
/// See this link for more information:
/// <https://github.com/rust-lang/rust/pull/82834>
struct PointersInner {
    /// The previous node in the list. `None` if there is no previous node.
    prev: Option<NonNull<Header>>,

    /// The next node in the list. `None` if there is no next node.
    next: Option<NonNull<Header>>,

    /// This type is !Unpin due to the heuristic from:
    /// <https://github.com/rust-lang/rust/pull/82834>
    _pin: PhantomPinned,
}

impl Pointers {
    pub(super) fn new() -> Pointers {
        Pointers {
            inner: UnsafeCell::new(PointersInner {
                prev: None,
                next: None,
                _pin: PhantomPinned,
            }),
        }
    }

    fn get_prev(&self) -> Option<NonNull<Header>> {
        // SAFETY: Pointers is `!Unpin`, so we read through a raw pointer
        // instead of creating a reference to the field.
        unsafe { ptr::addr_of!((*self.inner.get()).prev).read() }
    }

    fn get_next(&self) -> Option<NonNull<Header>> {
        // SAFETY: see `get_prev`.
        unsafe { ptr::addr_of!((*self.inner.get()).next).read() }
    }

    fn set_prev(&mut self, value: Option<NonNull<Header>>) {
        // SAFETY: see `get_prev`.
        unsafe { ptr::addr_of_mut!((*self.inner.get()).prev).write(value) }
    }

    fn set_next(&mut self, value: Option<NonNull<Header>>) {
        // SAFETY: see `get_prev`.
        unsafe { ptr::addr_of_mut!((*self.inner.get()).next).write(value) }
    }
}

/// Returns the list pointers of the task with the given header.
///
/// # Safety
///
/// `target` must point at the header of a live task.
unsafe fn pointers(target: NonNull<Header>) -> NonNull<Pointers> {
    Trailer::addr_of_owned(Header::get_trailer(target))
}

// ===== impl LinkedList =====

/// An intrusive linked list of tasks.
///
/// The list is not emptied on drop; `OwnedTasks` is responsible for shutting
/// down every task it owns before it goes away.
struct LinkedList<S: 'static> {
    head: Option<NonNull<Header>>,
    tail: Option<NonNull<Header>>,
    _p: PhantomData<Task<S>>,
}

impl<S: 'static> LinkedList<S> {
    const fn new() -> LinkedList<S> {
        LinkedList {
            head: None,
            tail: None,
            _p: PhantomData,
        }
    }

    /// Adds a task to the front of the list, taking ownership of its ref-count.
    fn push_front(&mut self, task: Task<S>) {
        // The value should not be dropped, it is being inserted into the list.
        let task = ManuallyDrop::new(task);
        let ptr = task.header_ptr();
        assert_ne!(self.head, Some(ptr));

        unsafe {
            pointers(ptr).as_mut().set_next(self.head);
            pointers(ptr).as_mut().set_prev(None);

            if let Some(head) = self.head {
                pointers(head).as_mut().set_prev(Some(ptr));
            }

            self.head = Some(ptr);

            if self.tail.is_none() {
                self.tail = Some(ptr);
            }
        }
    }

    /// Removes the last task from the list and returns it, or `None` if the
    /// list is empty.
    fn pop_back(&mut self) -> Option<Task<S>> {
        unsafe {
            let last = self.tail?;
            self.tail = pointers(last).as_ref().get_prev();

            if let Some(prev) = pointers(last).as_ref().get_prev() {
                pointers(prev).as_mut().set_next(None);
            } else {
                self.head = None;
            }

            pointers(last).as_mut().set_prev(None);
            pointers(last).as_mut().set_next(None);

            Some(Task::from_raw(last))
        }
    }

    /// Removes the specified task from the list, returning `None` if it is not
    /// linked into this list.
    ///
    /// This relies on the invariant that a node in no list has both pointers
    /// null: such a node takes the `else` branch below, and `head` only ever
    /// points at a linked node, so it returns `None` before mutating anything.
    /// `Pointers::new` establishes the invariant and every method here restores
    /// it by nulling both pointers on the way out.
    ///
    /// # Safety
    ///
    /// The caller must ensure that `node` is currently contained by `self`, or
    /// is not contained by any list.
    unsafe fn remove(&mut self, node: NonNull<Header>) -> Option<Task<S>> {
        if let Some(prev) = pointers(node).as_ref().get_prev() {
            debug_assert_eq!(pointers(prev).as_ref().get_next(), Some(node));
            pointers(prev)
                .as_mut()
                .set_next(pointers(node).as_ref().get_next());
        } else {
            if self.head != Some(node) {
                return None;
            }

            self.head = pointers(node).as_ref().get_next();
        }

        if let Some(next) = pointers(node).as_ref().get_next() {
            debug_assert_eq!(pointers(next).as_ref().get_prev(), Some(node));
            pointers(next)
                .as_mut()
                .set_prev(pointers(node).as_ref().get_prev());
        } else {
            // This might be the last item in the list.
            if self.tail != Some(node) {
                return None;
            }

            self.tail = pointers(node).as_ref().get_prev();
        }

        pointers(node).as_mut().set_next(None);
        pointers(node).as_mut().set_prev(None);

        Some(Task::from_raw(node))
    }

    fn is_empty(&self) -> bool {
        if self.head.is_some() {
            return false;
        }

        debug_assert!(self.tail.is_none());
        true
    }
}

// ===== impl OwnedTasks =====

pub(crate) struct OwnedTasks<S: 'static> {
    inner: UnsafeCell<Inner<S>>,
    _not_send_or_sync: PhantomData<*const ()>,
}

struct Inner<S: 'static> {
    list: LinkedList<S>,
    closed: bool,
}

impl<S: 'static> OwnedTasks<S> {
    pub(crate) fn new() -> Self {
        Self {
            inner: UnsafeCell::new(Inner {
                list: LinkedList::new(),
                closed: false,
            }),
            _not_send_or_sync: PhantomData,
        }
    }

    /// Binds a new task to this list, returning its `JoinHandle` and, unless
    /// the list is closed, the `Notified` that should be scheduled.
    pub(crate) fn bind<T>(
        &self,
        task: T,
        scheduler: S,
        id: super::Id,
        spawned_at: SpawnLocation,
    ) -> (JoinHandle<T::Output>, Option<Notified<S>>)
    where
        S: Schedule,
        T: Future + 'static,
        T::Output: 'static,
    {
        let (task, notified, join) = super::new_task(task, scheduler, id, spawned_at);

        if self.is_closed() {
            drop(notified);
            task.shutdown();
            (join, None)
        } else {
            self.with_inner(|inner| {
                inner.list.push_front(task);
            });
            (join, Some(notified))
        }
    }

    /// Shuts down all tasks in the collection. This call also closes the
    /// collection, preventing new items from being added.
    pub(crate) fn close_and_shutdown_all(&self)
    where
        S: Schedule,
    {
        self.with_inner(|inner| inner.closed = true);

        // The `shutdown` call below reenters this type through
        // `Schedule::release`, so the task must be popped outside of
        // `with_inner`.
        while let Some(task) = self.with_inner(|inner| inner.list.pop_back()) {
            task.shutdown();
        }
    }

    /// Removes a task from the collection, returning the ref-count that the
    /// collection held.
    ///
    /// Returns `None` if the task is not linked into this list, which is not an
    /// error: `release` runs for every task that completes, and two paths reach
    /// it with an unlinked task. A task bound while the list was closed is
    /// never linked at all, and `close_and_shutdown_all` unlinks a task before
    /// shutting it down.
    pub(crate) fn remove(&self, task: &Task<S>) -> Option<Task<S>> {
        self.with_inner(|inner|
            // Safety: A task is only ever released through the scheduler stored
            // in its own `Core`, which is the handle owning this list, so the
            // task is either linked into this list or into no list at all.
            unsafe { inner.list.remove(task.header_ptr()) })
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
        self.with_inner(|inner| inner.list.is_empty())
    }
}
