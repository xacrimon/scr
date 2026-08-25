pub(crate) mod context;
pub(crate) mod sched;
pub(crate) mod task;

mod blocked_on;
mod stub_waker;

use std::marker::PhantomData;
use std::panic::Location;
use std::pin::pin;
use std::task::{ContextBuilder, Poll};

use self::sched::Handle;
use self::stub_waker::stub_waker;
use self::task::JoinHandle;

const EVENT_INTERVAL: u32 = 61;

pub struct Runtime {
    handle: Handle,
    _marker: PhantomData<*const ()>,
}

impl Runtime {
    pub fn new() -> Self {
        Self {
            handle: Handle::new(),
            _marker: PhantomData,
        }
    }
    #[track_caller]
    pub fn spawn<F>(&self, future: F) -> JoinHandle
    where
        F: Future<Output = ()> + 'static,
    {
        let spawned_at = Location::caller();
        let _enter = context::enter(&self.handle);

        self.handle.spawn(future, spawned_at)
    }

    pub fn block_on<F: Future>(&self, future: F) -> F::Output {
        let _enter = context::enter(&self.handle);

        let mut future = pin!(future);
        let signal = blocked_on::Signal::new();
        let waker = stub_waker();
        let local_waker = signal.waker();
        let mut cx = ContextBuilder::from_waker(&waker)
            .local_waker(&local_waker)
            .build();

        loop {
            // Only poll the blocked-on future when it has actually been woken.
            if signal.take_notified()
                && let Poll::Ready(output) = future.as_mut().poll(&mut cx)
            {
                return output;
            }

            let mut ran = false;

            for _ in 0..EVENT_INTERVAL {
                let Some(task) = self.handle.next_task() else {
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
    fn default() -> Runtime {
        Runtime::new()
    }
}

impl Drop for Runtime {
    fn drop(&mut self) {
        // Shutting the tasks down drops their futures, whose destructors can
        // wake other tasks, so the runtime has to be entered for it.
        let _enter = context::enter(&self.handle);

        self.handle.shutdown();
    }
}
