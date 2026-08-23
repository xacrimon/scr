//! Asynchronous green-threads.

use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};

pub use crate::runtime::task::{AbortHandle, Id, JoinError, JoinHandle, id, try_id};

use crate::runtime::context;
use crate::runtime::sched::Handle;

/// Spawns a new asynchronous task on the current runtime, returning a
/// [`JoinHandle`] for it.
///
/// # Panics
///
/// Panics if called from outside of a runtime.
#[track_caller]
pub fn spawn<F>(future: F) -> JoinHandle<F::Output>
where
    F: Future + 'static,
    F::Output: 'static,
{
    let handle = context::current_handle();
    Handle::spawn(&handle, future)
}

/// Yields execution back to the runtime, so that other tasks in the run queue
/// get a chance to make progress.
pub async fn yield_now() {
    /// Yields once, then completes.
    struct YieldNow {
        yielded: bool,
    }

    impl Future for YieldNow {
        type Output = ();

        fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
            if self.yielded {
                return Poll::Ready(());
            }

            self.yielded = true;

            // Put ourselves back on the run queue behind everything that is
            // already there.
            cx.waker().wake_by_ref();

            Poll::Pending
        }
    }

    YieldNow { yielded: false }.await
}
