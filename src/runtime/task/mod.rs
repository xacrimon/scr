//! Spawned tasks.
//!
//! A task is a future the runtime owns, together with everything needed to
//! drive it: a state word, a reference count, a slot for the future and then
//! for its result, and a vtable so that the runtime can handle any future at
//! all through a single pointer.
//!
//! # References
//!
//! A task lives in one heap allocation, kept alive by the reference count in
//! its state word. There are four kinds of reference:
//!
//!  * [`Task`] — the registry's, held from the moment the task is spawned
//!    until it completes.
//!  * [`Runnable`] — the permission to poll the task, and the token that sits
//!    in the run queue. At most one exists at a time, which is what stops a
//!    task from being queued twice.
//!  * [`JoinHandle`] — the permission to await the task's result. At most one.
//!  * [`AbortHandle`] and `Waker` — any number of either.
//!
//! # State
//!
//! The state word packs the reference count with six flags:
//!
//!  * `RUNNING` — the task is being polled. Acts as a lock over the slot
//!    holding the future or its result.
//!  * `COMPLETE` — the future has finished and its result has been stored.
//!    Never unset, and never set at the same time as `RUNNING`.
//!  * `NOTIFIED` — a `Runnable` for this task exists.
//!  * `JOIN_INTEREST` — a `JoinHandle` for this task exists.
//!  * `CANCELLED` — the task should be killed at the next opportunity.
//!  * `OWNED` — the task is in the registry, in the slot named by its id.
//!
//! # Access to the slot
//!
//! While `COMPLETE` is unset, the slot holding the future belongs to whoever
//! holds `RUNNING`. Once `COMPLETE` is set, it holds the result, and belongs to
//! the `JoinHandle`; if there is none, the poll that completed the task drops
//! the result on its way out.
//!
//! The waker of whoever is awaiting the task belongs to the `JoinHandle` for as
//! long as `JOIN_INTEREST` is set. The runtime reads it once, on completion,
//! which cannot overlap with the `JoinHandle` writing it because both happen on
//! the same thread.
//!
//! Everything else in a task is written once, when it is created.
//!
//! # Thread affinity
//!
//! Every task is polled and dropped on the thread that owns the runtime, so the
//! state word is a plain `Cell<usize>` and no task type is `Send` or `Sync`.
//! That makes a `Waker` for one of these tasks thread affine as well: waking
//! from another thread would race on the reference count. The runtime must
//! never hand a waker to anything that could move it off this thread.
//!
//! # Re-entrancy
//!
//! Polling a task from inside its own poll, or shutting it down from there, is
//! not prevented by this API, so it has to be safe. The `RUNNING` lock makes
//! the inner call return immediately. If that inner call was a shutdown, it
//! leaves `CANCELLED` set, and the poll in progress kills the task when it
//! returns.

mod abort;
mod error;
mod id;
mod join;
mod owned;
mod raw;
mod state;
mod waker;

pub use self::abort::AbortHandle;
pub use self::error::JoinError;
pub use self::id::{Id, id, try_id};
pub use self::join::JoinHandle;

pub(crate) use self::owned::OwnedTasks;
pub(crate) use self::raw::{Header, RawTask};

use std::fmt;
use std::mem;
use std::panic::Location;
use std::ptr::NonNull;

/// What awaiting a task yields: its output, or why it never produced one.
pub(crate) type Result<T> = std::result::Result<T, JoinError>;

/// The registry's reference to a task.
#[repr(transparent)]
pub(crate) struct Task {
    raw: RawTask,
}

/// The permission to poll a task, and the token that sits in the run queue.
///
/// The `NOTIFIED` flag records whether one of these is outstanding. Waking a
/// task mints one only if that flag was clear, so a task is never queued twice.
#[repr(transparent)]
pub(crate) struct Runnable(Task);

/// Creates a task holding `future`, along with the three references it starts
/// with: the registry's, the run queue's, and the caller's `JoinHandle`.
fn new_task<T>(
    future: T,
    id: Id,
    spawned_at: &'static Location<'static>,
) -> (Task, Runnable, JoinHandle<T::Output>)
where
    T: Future + 'static,
    T::Output: 'static,
{
    let raw = RawTask::new(future, id, spawned_at);

    (Task { raw }, Runnable(Task { raw }), JoinHandle::new(raw))
}

impl Task {
    /// # Safety
    ///
    /// `ptr` must point at a live task, and the caller must own a reference to
    /// it to hand over.
    unsafe fn from_raw(ptr: NonNull<Header>) -> Task {
        Task {
            // Safety: forwarded to the caller.
            raw: unsafe { RawTask::from_raw(ptr) },
        }
    }

    fn header(&self) -> &Header {
        self.raw.header()
    }

    fn header_ptr(&self) -> NonNull<Header> {
        self.raw.header_ptr()
    }

    /// Kills the task, consuming the registry's reference.
    pub(crate) fn shutdown(self) {
        let raw = self.raw;
        mem::forget(self);

        raw.shutdown();
    }
}

impl Runnable {
    /// Polls the task, consuming the permission to do so.
    pub(crate) fn run(self) {
        let raw = self.0.raw;
        mem::forget(self);

        raw.poll();
    }

    pub(crate) fn header_ptr(&self) -> NonNull<Header> {
        self.0.header_ptr()
    }
}

impl Drop for Task {
    fn drop(&mut self) {
        self.raw.drop_reference();
    }
}

impl fmt::Debug for Task {
    fn fmt(&self, fmt: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.header().fmt(fmt)
    }
}

impl fmt::Debug for Runnable {
    fn fmt(&self, fmt: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(fmt)
    }
}
