//! Sleeping, ticking, and giving up waiting.
//!
//! # Resolution
//!
//! Deadlines are exact: nothing here rounds to a tick, and
//! [`sleep(Duration::from_micros(50))`](sleep) means fifty microseconds and not
//! the next millisecond boundary.
//!
//! Timers are always *late*, never early, which is the direction that composes:
//! a deadline is a promise not to fire before it, and every source of error here
//! only adds.

use std::io;
use std::pin::Pin;
use std::rc::Rc;
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use crate::runtime::context;
use crate::runtime::timers::{TimerKey, Timers};

/// A future that completes at a point in time. See [`sleep`].
///
/// Armed on construction rather than on first poll, so that the reactor will not
/// sleep past the deadline of a `Sleep` that has been created but not yet
/// awaited — a `select` over one and something slower depends on it.
pub struct Sleep {
    timers: Rc<Timers>,
    key: TimerKey,
    deadline: u64,
}

/// Completes after `duration` has elapsed.
///
/// # Panics
///
/// Panics if called from outside of a runtime.
#[track_caller]
pub fn sleep(duration: Duration) -> Sleep {
    let timers = context::timers();
    let deadline = timers
        .now()
        .saturating_add(duration.as_nanos().try_into().unwrap_or(u64::MAX));

    Sleep::arm(timers, deadline)
}

/// Completes at `deadline`, immediately if it has already passed.
///
/// # Panics
///
/// Panics if called from outside of a runtime.
#[track_caller]
pub fn sleep_until(deadline: Instant) -> Sleep {
    let timers = context::timers();
    let deadline = timers.since_anchor(deadline);

    Sleep::arm(timers, deadline)
}

impl Sleep {
    fn arm(timers: Rc<Timers>, deadline: u64) -> Sleep {
        let key = timers.insert(deadline);

        Sleep {
            timers,
            key,
            deadline,
        }
    }

    /// The instant this will complete at.
    pub fn deadline(&self) -> Instant {
        self.timers.to_instant(self.deadline)
    }

    /// Whether the deadline has passed.
    ///
    /// Reads the clock rather than the timer's state, so it does not depend on
    /// the reactor having got round to expiring anything.
    pub fn is_elapsed(&self) -> bool {
        self.timers.now() >= self.deadline
    }

    /// Re-aim at a new deadline, whether or not the old one has passed.
    ///
    /// Reuses the entry rather than dropping and re-arming.
    pub fn reset(&mut self, deadline: Instant) {
        self.reset_to(self.timers.since_anchor(deadline));
    }

    fn reset_to(&mut self, deadline: u64) {
        self.deadline = deadline;
        self.timers.reset(self.key, deadline);
    }
}

impl Future for Sleep {
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        // Nothing here is self-referential; the pin is only what `Future` asks
        // for, and `Sleep` is `Unpin` because of it.
        self.timers.poll(self.key, cx.local_waker())
    }
}

impl Drop for Sleep {
    fn drop(&mut self) {
        self.timers.remove(self.key);
    }
}

/// What to do when a tick is not consumed until after the next one was due.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum MissedTickBehavior {
    /// Tick immediately until the backlog is caught up, keeping the long-run
    /// rate exactly the period. The default, and the right answer when ticks
    /// count something that must not be lost.
    #[default]
    Burst,

    /// Drop the backlog and tick a full period from now, giving up the original
    /// phase. The right answer for a rate limiter: a period between ticks
    /// matters and their absolute placement does not.
    Delay,

    /// Drop the backlog and tick at the next multiple of the period from the
    /// original start, keeping the phase. The right answer when ticks line up
    /// with something outside the process.
    Skip,
}

/// A stream of deadlines a period apart. See [`interval`].
pub struct Interval {
    sleep: Sleep,
    period: u64,
    behavior: MissedTickBehavior,
}

/// Ticks every `period`, starting immediately.
///
/// The first tick completes without waiting, so a loop that ticks at the top
/// does its first pass at once rather than a period late.
///
/// # Panics
///
/// Panics if `period` is zero, or if called from outside of a runtime.
#[track_caller]
pub fn interval(period: Duration) -> Interval {
    interval_from(sleep(Duration::ZERO), period)
}

/// Ticks every `period`, with the first tick at `start`.
///
/// # Panics
///
/// Panics if `period` is zero, or if called from outside of a runtime.
#[track_caller]
pub fn interval_at(start: Instant, period: Duration) -> Interval {
    interval_from(sleep_until(start), period)
}

#[track_caller]
fn interval_from(sleep: Sleep, period: Duration) -> Interval {
    assert!(
        !period.is_zero(),
        "an interval of zero would tick without ever yielding"
    );

    Interval {
        sleep,
        period: period.as_nanos().try_into().unwrap_or(u64::MAX),
        behavior: MissedTickBehavior::default(),
    }
}

impl Interval {
    /// Completes at the next tick, returning the instant it was scheduled for.
    pub async fn tick(&mut self) -> Instant {
        struct Tick<'a>(&'a mut Interval);

        impl Future for Tick<'_> {
            type Output = Instant;

            fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Instant> {
                self.0.poll_tick(cx)
            }
        }

        Tick(self).await
    }

    /// [`tick`](Interval::tick) for a caller that already has a [`Context`].
    pub fn poll_tick(&mut self, cx: &mut Context<'_>) -> Poll<Instant> {
        if Pin::new(&mut self.sleep).poll(cx).is_pending() {
            return Poll::Pending;
        }

        let fired = self.sleep.deadline;
        let now = self.sleep.timers.now();

        let next = match self.behavior {
            MissedTickBehavior::Burst => fired + self.period,
            MissedTickBehavior::Delay => now + self.period,
            MissedTickBehavior::Skip => {
                let missed = now.saturating_sub(fired) / self.period;
                fired + self.period * (missed + 1)
            }
        };

        self.sleep.reset_to(next);

        Poll::Ready(self.sleep.timers.to_instant(fired))
    }

    /// The instant the next tick is scheduled for.
    pub fn period_start(&self) -> Instant {
        self.sleep.deadline()
    }

    pub fn missed_tick_behavior(&self) -> MissedTickBehavior {
        self.behavior
    }

    pub fn set_missed_tick_behavior(&mut self, behavior: MissedTickBehavior) {
        self.behavior = behavior;
    }

    /// Skip whatever backlog has built up and resume from `deadline`.
    pub fn reset_at(&mut self, deadline: Instant) {
        self.sleep.reset(deadline);
    }
}

/// A deadline passed before the future completed. See [`timeout`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Elapsed;

impl std::fmt::Display for Elapsed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("deadline has elapsed")
    }
}

impl std::error::Error for Elapsed {}

impl From<Elapsed> for io::Error {
    fn from(_: Elapsed) -> io::Error {
        io::Error::new(io::ErrorKind::TimedOut, Elapsed)
    }
}

/// A future with a deadline. See [`timeout`].
pub struct Timeout<F> {
    future: F,
    sleep: Sleep,
}

/// Runs `future`, giving up after `duration`.
///
/// # Panics
///
/// Panics if called from outside of a runtime.
#[track_caller]
pub fn timeout<F: Future>(duration: Duration, future: F) -> Timeout<F> {
    Timeout {
        future,
        sleep: sleep(duration),
    }
}

/// Runs `future`, giving up at `deadline`.
///
/// # Panics
///
/// Panics if called from outside of a runtime.
#[track_caller]
pub fn timeout_at<F: Future>(deadline: Instant, future: F) -> Timeout<F> {
    Timeout {
        future,
        sleep: sleep_until(deadline),
    }
}

impl<F> Timeout<F> {
    /// The future this was wrapped around, giving up on the deadline.
    pub fn into_inner(self) -> F {
        self.future
    }
}

impl<F: Future> Future for Timeout<F> {
    type Output = Result<F::Output, Elapsed>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        // SAFETY: `future` is never moved out of `self` and nothing else takes a
        // `&mut` to it, so it stays pinned for exactly as long as `self` does.
        let this = unsafe { self.get_unchecked_mut() };
        let future = unsafe { Pin::new_unchecked(&mut this.future) };

        if let Poll::Ready(output) = future.poll(cx) {
            return Poll::Ready(Ok(output));
        }

        match Pin::new(&mut this.sleep).poll(cx) {
            Poll::Ready(()) => Poll::Ready(Err(Elapsed)),
            Poll::Pending => Poll::Pending,
        }
    }
}
