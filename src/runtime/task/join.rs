use crate::runtime::task::{AbortHandle, Header, RawTask};

use std::fmt;
use std::marker::PhantomData;
use std::panic::{RefUnwindSafe, UnwindSafe};
use std::pin::Pin;
use std::task::{Context, Poll};

/// An owned permission to join on a task (await its termination).
///
/// A `JoinHandle` *detaches* the associated task when it is dropped, which
/// means that there is no longer any handle to the task, and no way to `join`
/// on it.
///
/// It is guaranteed that the destructor of the spawned task has finished before
/// task completion is observed via `JoinHandle` `await`,
/// [`JoinHandle::is_finished`] or [`AbortHandle::is_finished`].
///
/// # Cancel safety
///
/// Awaiting a `&mut JoinHandle<T>` is cancel safe: if the await is cancelled,
/// the output of the task is not lost.
pub struct JoinHandle<T> {
    raw: RawTask,
    _p: PhantomData<T>,
}

impl<T> UnwindSafe for JoinHandle<T> {}
impl<T> RefUnwindSafe for JoinHandle<T> {}

impl<T> JoinHandle<T> {
    pub(super) fn new(raw: RawTask) -> JoinHandle<T> {
        JoinHandle {
            raw,
            _p: PhantomData,
        }
    }

    /// Aborts the task associated with the handle.
    ///
    /// Awaiting a cancelled task might complete as usual if the task was
    /// already completed at the time it was cancelled, but most likely it will
    /// fail with a [cancelled] `JoinError`.
    ///
    /// [cancelled]: method@super::JoinError::is_cancelled
    pub fn abort(&self) {
        self.raw.remote_abort();
    }

    /// Checks if the task associated with this `JoinHandle` has finished.
    ///
    /// Note that this method can return `false` even if [`abort`] has been
    /// called on the task, because the cancellation only takes effect once the
    /// task is polled again.
    ///
    /// [`abort`]: method@JoinHandle::abort
    pub fn is_finished(&self) -> bool {
        let state = self.raw.state().load();
        state.is_complete()
    }

    /// Returns a new [`AbortHandle`] that can be used to abort this task.
    #[must_use = "abort handles do nothing unless `.abort` is called"]
    pub fn abort_handle(&self) -> AbortHandle {
        self.raw.ref_inc();
        AbortHandle::new(self.raw)
    }

    /// Returns a [task ID] that uniquely identifies this task relative to other
    /// currently spawned tasks.
    ///
    /// [task ID]: crate::task::Id
    pub fn id(&self) -> super::Id {
        // Safety: The header pointer is valid.
        unsafe { Header::get_id(self.raw.header_ptr()) }
    }

    /// Returns the source code location where this task was spawned.
    pub fn spawned_at(&self) -> &'static std::panic::Location<'static> {
        // Safety: The header pointer is valid.
        unsafe { Header::get_spawn_location(self.raw.header_ptr()) }
    }
}

impl<T> Unpin for JoinHandle<T> {}

impl<T> Future for JoinHandle<T> {
    type Output = super::Result<T>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let mut ret = Poll::Pending;

        // Try to read the task output. If the task is not yet complete, the
        // waker is stored and is notified once the task does complete.
        //
        // The function must go via the vtable, which requires erasing generic
        // types. To do this, the function "return" is placed on the stack
        // **before** calling the function and is passed into the function using
        // `*mut ()`.
        //
        // Safety:
        //
        // The type of `T` must match the task's output type.
        unsafe {
            self.raw.try_read_output(&mut ret, cx.waker());
        }

        ret
    }
}

impl<T> Drop for JoinHandle<T> {
    fn drop(&mut self) {
        if self.raw.state().drop_join_handle_fast().is_ok() {
            return;
        }

        self.raw.drop_join_handle_slow();
    }
}

impl<T> fmt::Debug for JoinHandle<T>
where
    T: fmt::Debug,
{
    fn fmt(&self, fmt: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Safety: The header pointer is valid.
        let id_ptr = unsafe { Header::get_id_ptr(self.raw.header_ptr()) };
        let id = unsafe { id_ptr.as_ref() };
        fmt.debug_struct("JoinHandle").field("id", id).finish()
    }
}
