//! Timers against the real reactor.

use std::cell::Cell;
use std::rc::Rc;
use std::time::{Duration, Instant};

use scr::Runtime;
use scr::task;
use scr::time::{self, Elapsed, MissedTickBehavior};

/// How far past a deadline a firing is still considered prompt.
const SLACK: Duration = Duration::from_millis(50);

#[test]
fn sleep_waits_at_least_its_duration() {
    let rt = Runtime::new().expect("Runtime::new");

    rt.block_on(async {
        let start = Instant::now();
        time::sleep(Duration::from_millis(20)).await;
        let elapsed = start.elapsed();

        assert!(elapsed >= Duration::from_millis(20), "{elapsed:?}");
        assert!(elapsed < Duration::from_millis(20) + SLACK, "{elapsed:?}");
    });
}

/// The whole reason this runtime does not use a millisecond timer wheel: a
/// deadline below a millisecond is honoured as one rather than rounded to a tick.
#[test]
fn a_sleep_far_below_a_millisecond_is_not_rounded_up_to_one() {
    let rt = Runtime::new().expect("Runtime::new");

    rt.block_on(async {
        const ROUNDS: usize = 200;
        const NAP: Duration = Duration::from_micros(50);

        let mut taken = Vec::with_capacity(ROUNDS);
        for _ in 0..ROUNDS {
            let start = Instant::now();
            time::sleep(NAP).await;
            taken.push(start.elapsed());
        }

        let quickest = taken.iter().copied().min().expect("ROUNDS > 0");
        assert!(
            quickest >= NAP,
            "a sleep returned after {quickest:?}, short of the {NAP:?} it promised"
        );
        assert!(
            quickest < Duration::from_millis(1),
            "the quickest of {ROUNDS} sleeps of {NAP:?} took {quickest:?}, which is \
             what rounding the deadline up to a millisecond tick would cost"
        );
    });
}

#[test]
fn a_deadline_already_past_completes_without_blocking() {
    let rt = Runtime::new().expect("Runtime::new");

    rt.block_on(async {
        let start = Instant::now();
        time::sleep(Duration::ZERO).await;
        time::sleep_until(Instant::now() - Duration::from_secs(1)).await;

        assert!(start.elapsed() < SLACK, "{:?}", start.elapsed());
    });
}

#[test]
fn sleep_until_fires_at_the_instant_it_was_given() {
    let rt = Runtime::new().expect("Runtime::new");

    rt.block_on(async {
        let deadline = Instant::now() + Duration::from_millis(15);
        let sleep = time::sleep_until(deadline);
        assert_eq!(sleep.deadline(), deadline);
        assert!(!sleep.is_elapsed());

        sleep.await;
        assert!(Instant::now() >= deadline);
    });
}

/// Timers from different tasks share one store, so the ordering they come back
/// in is the ordering the store imposes and not the order they were armed in.
#[test]
fn concurrent_sleeps_finish_in_deadline_order() {
    let rt = Runtime::new().expect("Runtime::new");
    let order = Rc::new(std::cell::RefCell::new(Vec::new()));

    rt.block_on({
        let order = Rc::clone(&order);
        async move {
            let mut joins = Vec::new();

            // Armed longest-first, so any structure that fires in arrival order
            // would produce the reverse of what is asserted below.
            for i in (0..8u32).rev() {
                let order = Rc::clone(&order);
                joins.push(task::spawn(async move {
                    time::sleep(Duration::from_millis(u64::from(i) * 5)).await;
                    order.borrow_mut().push(i);
                }));
            }

            for join in joins {
                join.await.expect("the task completed");
            }
        }
    });

    assert_eq!(*order.borrow(), (0..8).collect::<Vec<_>>());
}

#[test]
fn a_sleep_dropped_before_firing_wakes_nobody() {
    let rt = Runtime::new().expect("Runtime::new");
    let fired = Rc::new(Cell::new(false));

    rt.block_on({
        let fired = Rc::clone(&fired);
        async move {
            let join = task::spawn({
                let fired = Rc::clone(&fired);
                async move {
                    time::sleep(Duration::from_millis(10)).await;
                    fired.set(true);
                }
            });

            // Abort before the deadline, which drops the `Sleep` mid-flight and
            // has to take its entry out of the store with it — otherwise the
            // reactor would still be woken for a timer with nobody behind it.
            join.abort();
            assert!(join.await.is_err(), "the task was aborted");

            time::sleep(Duration::from_millis(30)).await;
        }
    });

    assert!(!fired.get());
}

#[test]
fn interval_ticks_immediately_then_on_the_period() {
    let rt = Runtime::new().expect("Runtime::new");

    rt.block_on(async {
        let start = Instant::now();
        let mut interval = time::interval(Duration::from_millis(10));

        interval.tick().await;
        assert!(start.elapsed() < SLACK, "the first tick did not wait");

        for n in 1..=4u64 {
            let scheduled = interval.tick().await;
            assert!(
                Instant::now() >= scheduled,
                "tick {n} reported a deadline in the future"
            );
            assert!(
                start.elapsed() >= Duration::from_millis(10 * n),
                "tick {n} at {:?}",
                start.elapsed()
            );
        }
    });
}

#[test]
fn interval_at_takes_its_phase_from_the_start() {
    let rt = Runtime::new().expect("Runtime::new");

    rt.block_on(async {
        let start = Instant::now() + Duration::from_millis(15);
        let mut interval = time::interval_at(start, Duration::from_millis(10));

        assert_eq!(interval.tick().await, start);
        assert_eq!(interval.tick().await, start + Duration::from_millis(10));
    });
}

/// The three policies differ only once the consumer is late, so each is checked
/// against a consumer that deliberately overruns.
#[test]
fn a_late_consumer_gets_the_behaviour_it_asked_for() {
    let rt = Runtime::new().expect("Runtime::new");
    const PERIOD: Duration = Duration::from_millis(10);

    rt.block_on(async {
        // Burst hands out the backlog at once, so the long-run rate is kept.
        let mut interval = time::interval(PERIOD);
        assert_eq!(interval.missed_tick_behavior(), MissedTickBehavior::Burst);
        let first = interval.tick().await;
        // Two and a half periods of overrun: ticks two and three are now due.
        time::sleep(PERIOD * 2 + PERIOD / 2).await;

        let start = Instant::now();
        assert_eq!(interval.tick().await, first + PERIOD);
        assert_eq!(interval.tick().await, first + PERIOD * 2);
        assert!(
            start.elapsed() < SLACK,
            "the backlog was handed out without waiting"
        );

        // Skip drops the backlog but keeps the phase.
        let mut interval = time::interval(PERIOD);
        interval.set_missed_tick_behavior(MissedTickBehavior::Skip);
        let first = interval.tick().await;
        time::sleep(PERIOD * 2 + PERIOD / 2).await;

        assert_eq!(
            interval.tick().await,
            first + PERIOD,
            "the tick that fired is still the one reported"
        );
        assert_eq!(
            interval.period_start(),
            first + PERIOD * 3,
            "but the next one is back on the original phase, with the \
             backlog dropped rather than handed out"
        );

        // Delay drops the backlog and gives up the phase.
        let mut interval = time::interval(PERIOD);
        interval.set_missed_tick_behavior(MissedTickBehavior::Delay);
        let first = interval.tick().await;
        time::sleep(PERIOD * 2 + PERIOD / 2).await;

        let resumed = Instant::now();
        let late = interval.tick().await;
        assert!(
            late >= first + PERIOD,
            "the tick reported is the one that was due"
        );
        assert!(late < resumed, "and it was already overdue when collected");
        assert!(
            interval.period_start() >= resumed + PERIOD,
            "the phase was given up: the next tick is a full period from the \
             moment the late one was collected, not from where the schedule was"
        );
        interval.tick().await;
        assert!(Instant::now() >= resumed + PERIOD);
    });
}

#[test]
#[should_panic(expected = "an interval of zero")]
fn an_interval_of_zero_is_rejected() {
    let rt = Runtime::new().expect("Runtime::new");
    rt.block_on(async { time::interval(Duration::ZERO) });
}

#[test]
fn a_future_that_finishes_in_time_keeps_its_output() {
    let rt = Runtime::new().expect("Runtime::new");

    rt.block_on(async {
        let out = time::timeout(Duration::from_secs(30), async {
            time::sleep(Duration::from_millis(5)).await;
            "done"
        })
        .await;

        assert_eq!(out, Ok("done"));
    });
}

#[test]
fn a_future_that_overruns_elapses() {
    let rt = Runtime::new().expect("Runtime::new");

    rt.block_on(async {
        let start = Instant::now();
        let out = time::timeout(
            Duration::from_millis(10),
            time::sleep(Duration::from_secs(30)),
        )
        .await;

        assert_eq!(out, Err(Elapsed));
        assert!(start.elapsed() >= Duration::from_millis(10));
        assert!(start.elapsed() < Duration::from_millis(10) + SLACK);
    });
}

/// A future ready on its first poll must not be able to lose to a deadline that
/// has also already passed.
#[test]
fn a_ready_future_wins_against_a_deadline_already_past() {
    let rt = Runtime::new().expect("Runtime::new");

    rt.block_on(async {
        assert_eq!(time::timeout(Duration::ZERO, async { 7 }).await, Ok(7));
        assert_eq!(
            time::timeout_at(Instant::now() - Duration::from_secs(1), async { 7 }).await,
            Ok(7)
        );
    });
}

#[test]
fn an_elapsed_timeout_drops_the_future_it_gave_up_on() {
    let rt = Runtime::new().expect("Runtime::new");
    let dropped = Rc::new(Cell::new(false));

    rt.block_on({
        let dropped = Rc::clone(&dropped);
        async move {
            struct OnDrop(Rc<Cell<bool>>);
            impl Drop for OnDrop {
                fn drop(&mut self) {
                    self.0.set(true);
                }
            }

            let out = time::timeout(Duration::from_millis(10), async move {
                let _guard = OnDrop(dropped);
                std::future::pending::<()>().await;
            })
            .await;

            assert_eq!(out, Err(Elapsed));
        }
    });

    assert!(dropped.get());
}

#[test]
fn elapsed_converts_to_a_timed_out_io_error() {
    let err = std::io::Error::from(Elapsed);
    assert_eq!(err.kind(), std::io::ErrorKind::TimedOut);
}

/// The reactor's only wakeup here is the timeout on its own `io_uring_enter`:
/// nothing is in flight and nothing is runnable, which is the case that used to
/// be a deadlock panic.
#[test]
fn a_runtime_with_nothing_but_a_timer_still_makes_progress() {
    let rt = Runtime::new().expect("Runtime::new");

    rt.block_on(async {
        let done = Rc::new(Cell::new(false));
        let join = task::spawn({
            let done = Rc::clone(&done);
            async move {
                time::sleep(Duration::from_millis(5)).await;
                done.set(true);
            }
        });

        join.await.expect("the task completed");
        assert!(done.get());
    });
}

/// A timer and an operation both want the reactor woken, and the two mechanisms
/// are unrelated: a completion arrives through the ring, a deadline through the
/// timeout on the `io_uring_enter` that was waiting for one. Whichever comes
/// first has to end that wait.
mod with_io {
    use super::*;

    use scr::io::{AsyncRead, AsyncWrite};
    use scr::net::{TcpListener, TcpStream};

    /// A connected pair, and the listener kept alive alongside them.
    ///
    /// `connect` completes as soon as the kernel puts the connection on the
    /// listener's backlog, so it does not need an `accept` running concurrently
    /// to make progress and the two can be awaited in order.
    async fn pair() -> (TcpListener, TcpStream, TcpStream) {
        let listener = TcpListener::bind("127.0.0.1:0".parse().expect("literal"))
            .await
            .expect("bind");
        let addr = listener.local_addr().await.expect("local_addr");

        let client = TcpStream::connect(addr).await.expect("connect");
        let (server, _) = listener.accept().await.expect("accept");

        (listener, server, client)
    }

    /// The read never completes, so the only thing that can end the wait is the
    /// deadline — on a reactor that does have an operation in flight, which is
    /// the case a bare sleep does not cover.
    #[test]
    fn a_read_that_never_arrives_times_out() {
        let rt = Runtime::new().expect("Runtime::new");

        rt.block_on(async {
            let (_listener, server, _client) = pair().await;

            let start = Instant::now();
            let out = time::timeout(Duration::from_millis(20), server.read(vec![0u8; 64])).await;

            assert_eq!(out.err(), Some(Elapsed));
            assert!(start.elapsed() >= Duration::from_millis(20));
            assert!(start.elapsed() < Duration::from_millis(20) + SLACK);
        });
    }

    /// The other way round: the completion arrives well inside the deadline, so
    /// the timer must not be what ends the wait — and must leave the store clean
    /// when the `Timeout` around it is dropped.
    #[test]
    fn a_read_that_arrives_beats_its_deadline() {
        let rt = Runtime::new().expect("Runtime::new");

        rt.block_on(async {
            let (_listener, server, client) = pair().await;

            let (written, _) = client.write(b"ping".to_vec()).await;
            assert_eq!(written.expect("write"), 4);

            let start = Instant::now();
            let (read, buf) = time::timeout(Duration::from_secs(30), server.read(vec![0u8; 64]))
                .await
                .expect("the read beat the deadline");

            assert_eq!(&buf[..read.expect("read")], b"ping");
            assert!(start.elapsed() < SLACK, "it did not wait out the deadline");
        });
    }

    /// A timer armed while an operation is in flight, with the operation the one
    /// that lands first. The wait is bounded by the deadline, so the reactor has
    /// to go back to sleep for the remainder rather than treat one wakeup as the
    /// end of the sleep.
    #[test]
    fn a_sleep_survives_completions_landing_inside_it() {
        let rt = Runtime::new().expect("Runtime::new");

        rt.block_on(async {
            let (_listener, server, client) = pair().await;

            // Chatter arriving throughout the sleep below, each round trip
            // waking the reactor early.
            let chatter = task::spawn(async move {
                for _ in 0..10u32 {
                    let (written, buf) = client.write(b"tick".to_vec()).await;
                    assert_eq!(written.expect("write"), 4);
                    let (read, _) = server.read(buf).await;
                    assert_eq!(read.expect("read"), 4);
                    time::sleep(Duration::from_millis(1)).await;
                }
            });

            let start = Instant::now();
            time::sleep(Duration::from_millis(30)).await;

            assert!(
                start.elapsed() >= Duration::from_millis(30),
                "the sleep returned early at {:?}, so a completion was mistaken \
                 for the deadline",
                start.elapsed()
            );

            chatter.abort();
            let _ = chatter.await;
        });
    }
}
