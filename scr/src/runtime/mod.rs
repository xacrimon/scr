pub(crate) mod context;
pub(crate) mod driver;
pub(crate) mod sched;
pub(crate) mod task;
pub(crate) mod timers;

mod blocked_on;
mod stub_waker;

use std::io;
use std::marker::PhantomData;
use std::panic::Location;
use std::pin::pin;
use std::task::{ContextBuilder, Poll};
use std::time::Duration;

use self::driver::Turn;
use self::sched::Handle;
use self::stub_waker::stub_waker;
use self::task::JoinHandle;

/// How long to keep polling tasks before turning the reactor.
const POLL_BUDGET: Duration = Duration::from_micros(100);

/// Tasks to poll between readings of the clock.
const CLOCK_INTERVAL: u32 = 5;

pub struct Runtime {
    handle: Handle,
    _marker: PhantomData<*const ()>,
}

impl Runtime {
    /// Create a runtime, along with the io_uring ring that backs it.
    ///
    /// The ring is bound to the calling thread — `IORING_SETUP_SINGLE_ISSUER`
    /// and `IORING_SETUP_DEFER_TASKRUN` both require that only one thread
    /// submits and reaps — which is why a [`Runtime`] is neither `Send` nor
    /// `Sync`.
    pub fn new() -> io::Result<Runtime> {
        Ok(Runtime {
            handle: Handle::new()?,
            _marker: PhantomData,
        })
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

        let driver = self.handle.driver();
        let timers = self.handle.timers();

        loop {
            // Only poll the blocked-on future when it has actually been woken.
            if signal.take_notified()
                && let Poll::Ready(output) = future.as_mut().poll(&mut cx)
            {
                return output;
            }

            let now = timers.now();
            let budget_ends = now + POLL_BUDGET.as_nanos() as u64;
            let batch_ends = match timers.earliest() {
                Some(deadline) => deadline.min(budget_ends),
                None => budget_ends,
            };
            let mut polled = 0u32;

            while let Some(task) = self.handle.next_task() {
                task.run();
                polled += 1;

                if driver.backlog_pending() {
                    break;
                }

                if polled.is_multiple_of(CLOCK_INTERVAL) && timers.now() >= batch_ends {
                    break;
                }
            }

            // Before the idle check below, because this is one of the two things
            // that can put work back on an empty queue.
            timers.expire(timers.now());

            let idle = self.handle.queue_is_empty() && !signal.is_notified();
            if !idle {
                // There is more to do, so this is a flush: submit what the batch
                // queued and collect whatever came back, without waiting.
                driver.turn(Turn::Flush).expect("io_uring_enter");
                continue;
            }

            let now = timers.now();
            let turn = match timers.earliest() {
                Some(deadline) if deadline <= now => Turn::Flush,
                Some(deadline) => Turn::WaitFor(Duration::from_nanos(deadline - now)),
                None if driver.in_flight() == 0 => panic!(
                    "`block_on` deadlocked: the run queue is empty, the blocked-on \
                     future is pending, no timer is armed, and no operation is in \
                     flight. This runtime's wakers are thread affine, so nothing \
                     can wake it."
                ),
                None => Turn::Wait,
            };

            driver.turn(turn).expect("io_uring_enter");
        }
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
