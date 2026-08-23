use std::fmt;
use std::marker::PhantomData;
use std::panic::{self, AssertUnwindSafe, Location, RefUnwindSafe, UnwindSafe};
use std::pin::Pin;
use std::task::{Context, Poll, Waker};

use crate::runtime::task::raw::TaskIdGuard;
use crate::runtime::task::{AbortHandle, Header, Id, RawTask};

/// An owned permission to join a task, that is, to await its result.
///
/// Dropping a `JoinHandle` *detaches* the task: it keeps running, but there is
/// no longer any way to join it.
///
/// The destructor of a spawned task is guaranteed to have finished before its
/// completion is observed through awaiting a `JoinHandle`, through
/// [`JoinHandle::is_finished`], or through [`AbortHandle::is_finished`].
///
/// # Cancel safety
///
/// Awaiting a `&mut JoinHandle<T>` is cancel safe: if the await is cancelled,
/// the task's result is not lost.
pub struct JoinHandle<T> {
    raw: RawTask,
    _p: PhantomData<T>,
}

impl<T> UnwindSafe for JoinHandle<T> {}
impl<T> RefUnwindSafe for JoinHandle<T> {}
impl<T> Unpin for JoinHandle<T> {}

impl<T> JoinHandle<T> {
    pub(super) fn new(raw: RawTask) -> JoinHandle<T> {
        JoinHandle {
            raw,
            _p: PhantomData,
        }
    }

    /// Aborts the task.
    ///
    /// Awaiting an aborted task may still yield its result, if it had already
    /// finished when the abort landed; otherwise it fails with a [cancelled]
    /// `JoinError`.
    ///
    /// [cancelled]: method@super::JoinError::is_cancelled
    pub fn abort(&self) {
        self.raw.remote_abort();
    }

    /// Returns a new [`AbortHandle`] for this task.
    #[must_use = "abort handles do nothing unless `.abort` is called"]
    pub fn abort_handle(&self) -> AbortHandle {
        self.raw.ref_inc();
        AbortHandle::new(self.raw)
    }

    /// Returns whether the task has finished.
    ///
    /// This can still be `false` after [`abort`] has been called, since a task
    /// is only cancelled once the runtime gets back to it.
    ///
    /// [`abort`]: method@JoinHandle::abort
    pub fn is_finished(&self) -> bool {
        self.raw.state().load().is_complete()
    }

    /// Returns the [`Id`] of the task.
    pub fn id(&self) -> Id {
        self.raw.header().id
    }

    /// Returns the source location the task was spawned from.
    pub fn spawned_at(&self) -> &'static Location<'static> {
        self.raw.header().spawned_at
    }
}

impl<T> Future for JoinHandle<T> {
    type Output = super::Result<T>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        if !can_read_output(self.raw.header(), cx.waker()) {
            return Poll::Pending;
        }

        // The result is moved out through a pointer, since taking it needs the
        // task's future type, which only its vtable still knows.
        let mut output = Poll::Pending;

        // Safety: `T` is the task's output type, and the check above says the
        // task is complete.
        unsafe { self.raw.take_output(&mut output) };

        assert!(
            output.is_ready(),
            "`JoinHandle` polled after its result was taken"
        );

        output
    }
}

impl<T> Drop for JoinHandle<T> {
    fn drop(&mut self) {
        if self.raw.state().transition_to_join_handle_dropped() {
            // The task finished, and nobody is left to read its result, so it
            // is dropped here where its type is known. There may be nothing
            // left to take, if the handle was awaited before being dropped.
            let mut output: Poll<super::Result<T>> = Poll::Pending;
            let _guard = TaskIdGuard::enter(self.raw.header().id);

            // Safety: `T` is the task's output type, and `COMPLETE` is set.
            unsafe { self.raw.take_output(&mut output) };

            // Dropping the handle is how a caller says it is not interested in
            // what the task produced, so a panicking result is dropped quietly.
            let _ = panic::catch_unwind(AssertUnwindSafe(move || drop(output)));
        }

        // Safety: `JOIN_INTEREST` is now unset, so the runtime will not read
        // the waker, and this handle was its only other accessor.
        unsafe { self.raw.header().set_waker(None) };

        self.raw.drop_reference();
    }
}

impl<T> fmt::Debug for JoinHandle<T> {
    fn fmt(&self, fmt: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt.debug_struct("JoinHandle")
            .field("id", &self.id())
            .finish()
    }
}

/// Returns `true` if the task's result is ready to be taken, storing `waker` to
/// be notified when it is otherwise.
fn can_read_output(header: &Header, waker: &Waker) -> bool {
    let snapshot = header.state.load();
    debug_assert!(snapshot.is_join_interested());

    if snapshot.is_complete() {
        return true;
    }

    // Safety: `JOIN_INTEREST` is set, so the `JoinHandle` this call comes from
    // owns the field.
    unsafe {
        if !header.will_wake(waker) {
            header.set_waker(Some(waker.clone()));
        }
    }

    false
}
