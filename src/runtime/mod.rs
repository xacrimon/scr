pub(crate) mod context;
pub(crate) mod sched;
pub(crate) mod task;

mod blocking;

use std::future::Future;
use std::marker::PhantomData;
use std::pin::pin;
use std::rc::Rc;
use std::task::{Context, Poll};

use self::sched::Handle;
use self::task::JoinHandle;

/// How many tasks to run before checking the future passed to `block_on`
/// again.
const EVENT_INTERVAL: u32 = 61;

/// A single threaded runtime.
///
/// The runtime, every task spawned on it, and every waker handed out by it are
/// bound to the thread that created it. `Runtime` is therefore neither `Send`
/// nor `Sync`.
pub struct Runtime {
    handle: Rc<Handle>,

    /// The runtime is thread affine; see the task module docs.
    _not_send_or_sync: PhantomData<*const ()>,
}

impl Runtime {
    /// Creates a new runtime.
    pub fn new() -> Runtime {
        Runtime {
            handle: Handle::new(),
            _not_send_or_sync: PhantomData,
        }
    }

    /// Spawns a task onto the runtime.
    ///
    /// The task starts running as soon as the runtime next polls its run
    /// queue; it does not run at the point of the `spawn` call.
    #[track_caller]
    pub fn spawn<F>(&self, future: F) -> JoinHandle<F::Output>
    where
        F: Future + 'static,
        F::Output: 'static,
    {
        Handle::spawn(&self.handle, future)
    }

    /// Runs a future to completion, driving any spawned tasks in the meantime.
    ///
    /// # Panics
    ///
    /// Panics if the runtime runs out of work while `future` is still pending.
    /// Since there is no I/O or timer driver yet, and wakers cannot leave this
    /// thread, nothing could ever wake the future at that point, so this would
    /// otherwise be a silent hang.
    pub fn block_on<F: Future>(&self, future: F) -> F::Output {
        let _enter = context::enter_runtime(&self.handle);

        let mut future = pin!(future);
        let signal = blocking::Signal::new();
        let waker = signal.waker();
        let mut cx = Context::from_waker(&waker);

        loop {
            // Only poll the blocked-on future when it has actually been woken.
            if signal.take_notified()
                && let Poll::Ready(output) = future.as_mut().poll(&mut cx)
            {
                return output;
            }

            let mut ran = false;

            for _ in 0..EVENT_INTERVAL {
                let Some(task) = self.handle.queue.pop() else {
                    break;
                };

                task.run();
                ran = true;

                if signal.is_notified() {
                    break;
                }
            }

            if !ran && !signal.is_notified() {
                panic!(
                    "`block_on` deadlocked: the run queue is empty and the blocked-on \
                     future is pending. This runtime has no I/O or timer driver, and its \
                     wakers are thread affine, so nothing can wake it."
                );
            }
        }
    }
}

impl Default for Runtime {
    fn default() -> Self {
        Runtime::new()
    }
}

impl Drop for Runtime {
    fn drop(&mut self) {
        self.handle.shutdown();
    }
}
