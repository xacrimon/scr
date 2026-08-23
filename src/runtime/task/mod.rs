//! The task module.
//!
//! The task module contains the code that manages spawned tasks and provides a
//! safe API for the rest of the runtime to use. Each task in a runtime is
//! stored in an `OwnedTasks` object.
//!
//! This is a translation of `tokio`'s task module for a runtime that is single
//! threaded by construction. The differences worth knowing about are collected
//! in the "Single threaded" section below.
//!
//! # Task reference types
//!
//! A task is usually referenced by multiple handles, and there are several
//! types of handles.
//!
//!  * `OwnedTask` - tasks stored in an `OwnedTasks` are of this reference type.
//!
//!  * `JoinHandle` - each task has a `JoinHandle` that allows access to the
//!    output of the task.
//!
//!  * `Waker` - every waker for a task has this reference type. There can be any
//!    number of waker references.
//!
//!  * `Notified` - tracks whether the task is notified.
//!
//! The task uses a reference count to keep track of how many active references
//! exist. Each reference type takes up a single ref-count.
//!
//! Besides the waker type, each task has at most one of each reference type.
//!
//! # State
//!
//! The task stores its state in a `Cell<usize>` with various bitfields for the
//! necessary information. The state has the following bitfields:
//!
//!  * `RUNNING` - Tracks whether the task is currently being polled or cancelled.
//!    This bit functions as a lock around the task.
//!
//!  * `COMPLETE` - Is one once the future has fully completed and has been
//!    dropped. Never unset once set. Never set together with RUNNING.
//!
//!  * `NOTIFIED` - Tracks whether a Notified object currently exists.
//!
//!  * `CANCELLED` - Is set to one for tasks that should be cancelled as soon as
//!    possible. May take any value for completed tasks.
//!
//!  * `JOIN_INTEREST` - Is set to one if there exists a `JoinHandle`.
//!
//! The rest of the bits are used for the ref-count.
//!
//! # Fields in the task
//!
//! The task has various fields. This section describes how and when it is safe
//! to access a field.
//!
//!  * The `OwnedTask` reference has exclusive access to the `owned` field.
//!
//!  * The `owner_id` field can be set as part of construction of the task, but
//!    is otherwise only modified while removing the task from its list.
//!
//!  * If COMPLETE is one, then the `JoinHandle` has exclusive access to the
//!    stage field. If COMPLETE is zero, then the RUNNING bitfield functions as
//!    a lock for the stage field, and it can be accessed only by the caller
//!    that set RUNNING to one.
//!
//!  * The waker field is owned by the `JoinHandle` for as long as
//!    `JOIN_INTEREST` is set. The runtime reads it once, during completion,
//!    which cannot overlap with a `JoinHandle` poll because both happen on the
//!    same thread. See the "Single threaded" section.
//!
//! All other fields are immutable and can be accessed immutably without
//! synchronization by anyone.
//!
//! # Single threaded
//!
//! This runtime polls every task, and drops every task, on the thread that owns
//! the runtime. That removes the need for several mechanisms that `tokio` needs:
//!
//!  * The task state is a `Cell<usize>` instead of an `AtomicUsize`, and every
//!    transition is a load/modify/store instead of a CAS loop.
//!
//!  * The `JOIN_WAKER` bit is gone. In `tokio` it is an access control bit that
//!    arbitrates between a `JoinHandle` writing the waker field on one thread
//!    and the runtime reading it on another. Here those two events are ordered
//!    by being on the same thread, so the waker field is a plain `Option<Waker>`
//!    that the `JoinHandle` owns until it unsets `JOIN_INTEREST`.
//!
//!  * There is no `LocalNotified`, because every `Notified` is local, and no
//!    `UnownedTask`, because there are no blocking tasks.
//!
//!  * No task type implements `Send` or `Sync`.
//!
//! Note that this makes a `Waker` for one of these tasks thread-affine: waking
//! from another thread would race on the non-atomic ref-count. The runtime is
//! responsible for never handing a waker to something that could move it to
//! another thread.
//!
//! # Safety
//!
//! This section goes through various situations and explains why the API is
//! safe in that situation.
//!
//! ## Polling or dropping the future
//!
//! Any mutable access to the future happens after obtaining a lock by modifying
//! the RUNNING field, so exclusive access is ensured.
//!
//! When the task completes, exclusive access to the output is transferred to
//! the `JoinHandle`. If the `JoinHandle` is already dropped when the transition
//! to complete happens, the caller performing that transition retains exclusive
//! access to the output and should immediately drop it.
//!
//! ## Recursive poll/shutdown
//!
//! Calling poll from inside a shutdown call or vice-versa is not prevented by
//! the API exposed by the task module, so this has to be safe. In either case,
//! the lock in the RUNNING bitfield makes the inner call return immediately. If
//! the inner call is a `shutdown` call, then the CANCELLED bit is set, and the
//! poll call will notice it when the poll finishes, and the task is cancelled
//! at that point.

mod abort;
pub use self::abort::AbortHandle;

mod core;
use self::core::Cell;
use self::core::Header;
use self::core::Trailer;

mod error;
pub use self::error::JoinError;

mod harness;
use self::harness::Harness;

mod id;
pub use self::id::{Id, id, try_id};

mod join;
pub use self::join::JoinHandle;

mod list;
pub(crate) use self::list::OwnedTasks;

mod raw;
pub(crate) use self::raw::RawTask;

mod state;
use self::state::State;

mod waker;

use std::marker::PhantomData;
use std::panic::Location;
use std::ptr::NonNull;
use std::{fmt, mem};

/// An owned handle to the task, tracked by ref count.
#[repr(transparent)]
pub(crate) struct Task<S: 'static> {
    raw: RawTask,
    _p: PhantomData<S>,
}

/// A task was notified.
#[repr(transparent)]
pub(crate) struct Notified<S: 'static>(Task<S>);

/// Task result sent back.
pub(crate) type Result<T> = std::result::Result<T, JoinError>;

pub(crate) trait Schedule: Sized + 'static {
    /// The task has completed work and is ready to be released. The scheduler
    /// should release it immediately and return it. The task module will batch
    /// the ref-dec with setting other options.
    ///
    /// If the scheduler has already released the task, then None is returned.
    fn release(&self, task: &Task<Self>) -> Option<Task<Self>>;

    /// Schedule the task.
    fn schedule(&self, task: Notified<Self>);

    /// Schedule the task to run in the near future, yielding the thread to
    /// other tasks.
    fn yield_now(&self, task: Notified<Self>) {
        self.schedule(task);
    }

    /// Polling the task resulted in a panic. Should the runtime shutdown?
    fn unhandled_panic(&self) {
        // By default, do nothing.
    }
}

/// This is the constructor for a new task. Three references to the task are
/// created. The first task reference is usually put into an `OwnedTasks`
/// immediately. The Notified is sent to the scheduler as an ordinary
/// notification.
fn new_task<T, S>(
    task: T,
    scheduler: S,
    id: Id,
    spawned_at: SpawnLocation,
) -> (Task<S>, Notified<S>, JoinHandle<T::Output>)
where
    S: Schedule,
    T: Future + 'static,
    T::Output: 'static,
{
    let raw = RawTask::new::<T, S>(task, scheduler, id, spawned_at);
    let task = Task {
        raw,
        _p: PhantomData,
    };
    let notified = Notified(Task {
        raw,
        _p: PhantomData,
    });
    let join = JoinHandle::new(raw);

    (task, notified, join)
}

impl<S: 'static> Task<S> {
    unsafe fn new(raw: RawTask) -> Task<S> {
        Task {
            raw,
            _p: PhantomData,
        }
    }

    /// # Safety
    ///
    /// `ptr` must be a valid pointer to a [`Header`].
    unsafe fn from_raw(ptr: NonNull<Header>) -> Task<S> {
        unsafe { Task::new(RawTask::from_raw(ptr)) }
    }

    fn header(&self) -> &Header {
        self.raw.header()
    }

    fn header_ptr(&self) -> NonNull<Header> {
        self.raw.header_ptr()
    }

    fn trailer(&self) -> &Trailer {
        self.raw.trailer()
    }

    /// Returns a [task ID] that uniquely identifies this task relative to other
    /// currently spawned tasks.
    ///
    /// [task ID]: crate::task::Id
    #[allow(dead_code)]
    pub(crate) fn id(&self) -> Id {
        // Safety: The header pointer is valid.
        unsafe { Header::get_id(self.raw.header_ptr()) }
    }

    /// Returns the source code location where this task was spawned.
    #[allow(dead_code)]
    pub(crate) fn spawned_at(&self) -> &'static Location<'static> {
        // Safety: The header pointer is valid.
        unsafe { Header::get_spawn_location(self.raw.header_ptr()) }
    }
}

impl<S: 'static> Notified<S> {
    fn trailer(&self) -> &Trailer {
        self.0.trailer()
    }

    /// Returns a [task ID] that uniquely identifies this task relative to other
    /// currently spawned tasks.
    ///
    /// [task ID]: crate::task::Id
    #[allow(dead_code)]
    pub(crate) fn id(&self) -> Id {
        self.0.id()
    }
}

impl<S: Schedule> Task<S> {
    /// Preemptively cancels the task as part of the shutdown process.
    pub(crate) fn shutdown(self) {
        let raw = self.raw;
        mem::forget(self);
        raw.shutdown();
    }
}

impl<S: Schedule> Notified<S> {
    /// Runs the task.
    pub(crate) fn run(self) {
        let raw = self.0.raw;
        mem::forget(self);
        raw.poll();
    }
}

impl<S: 'static> Drop for Task<S> {
    fn drop(&mut self) {
        // Decrement the ref count
        if self.header().state.ref_dec() {
            // Deallocate if this is the final ref count
            self.raw.dealloc();
        }
    }
}

impl<S> fmt::Debug for Task<S> {
    fn fmt(&self, fmt: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(fmt, "Task({:p})", self.header())
    }
}

impl<S> fmt::Debug for Notified<S> {
    fn fmt(&self, fmt: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(fmt, "task::Notified({:p})", self.0.header())
    }
}

/// Wrapper around [`std::panic::Location`] for the source code location where a
/// task was spawned.
///
/// Unlike `tokio`, spawn locations are always captured: there is no
/// `tokio_unstable`-style cfg gating them out.
#[derive(Copy, Clone)]
pub(crate) struct SpawnLocation(pub(crate) &'static Location<'static>);

impl From<&'static Location<'static>> for SpawnLocation {
    fn from(location: &'static Location<'static>) -> Self {
        Self(location)
    }
}

impl SpawnLocation {
    #[track_caller]
    #[inline]
    pub(crate) fn capture() -> Self {
        Self::from(Location::caller())
    }
}
