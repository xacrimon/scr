//! What the timers actually resolve to on this machine.
//!
//! The runtime's part of a sleep is exact — a deadline is nanoseconds in a heap,
//! and the reactor is told not to sleep past the earliest one. Everything after
//! that belongs to the kernel: arming an `hrtimer`, expiring it, and waking a
//! thread parked in `io_uring_enter`. So the honest thing to do is measure it,
//! and to measure `std::thread::sleep` alongside as a floor — that is
//! `clock_nanosleep` on the same clock, doing the same job with no ring
//! involved, so it says what the machine is capable of rather than what this
//! crate manages.
//!
//! Expect the two columns to be close. A virtualised machine will show both of
//! them far above the target, because a sleeping vCPU is rescheduled at the
//! host's convenience; that is the platform, not the runtime.
//!
//!     cargo run --release --example timer_resolution

use std::time::{Duration, Instant};

use scr::{Runtime, time};

/// Enough samples for a p99 to mean something, few enough to finish quickly at
/// the largest target below.
const SAMPLES: usize = 500;

const TARGETS: [Duration; 5] = [
    Duration::from_nanos(1),
    Duration::from_micros(1),
    Duration::from_micros(10),
    Duration::from_micros(100),
    Duration::from_millis(1),
];

fn main() {
    let rt = Runtime::new().expect("Runtime::new");

    println!(
        "{SAMPLES} samples per target. \"over\" is how far past the deadline the \
         wakeup landed.\n"
    );
    println!(
        "{:>10} │ {:^34} │ {:^34}",
        "", "scr::time::sleep (over)", "thread::sleep (over)"
    );
    println!(
        "{:>10} │ {:>10} {:>10} {:>10} │ {:>10} {:>10} {:>10}",
        "target", "min", "p50", "p99", "min", "p50", "p99"
    );
    println!("{:─>11}┼{:─>36}┼{:─>36}", "", "", "");

    for target in TARGETS {
        let ours = rt.block_on(async move {
            let mut over = Vec::with_capacity(SAMPLES);

            for _ in 0..SAMPLES {
                let start = Instant::now();
                time::sleep(target).await;
                over.push(start.elapsed().saturating_sub(target));
            }

            over
        });

        let theirs: Vec<Duration> = (0..SAMPLES)
            .map(|_| {
                let start = Instant::now();
                std::thread::sleep(target);
                start.elapsed().saturating_sub(target)
            })
            .collect();

        println!(
            "{:>10} │ {} │ {}",
            format_duration(target),
            row(ours),
            row(theirs),
        );
    }

    // The number the reactor's polling budget bounds rather than the kernel: a
    // deadline that comes due while tasks are runnable waits for the batch to
    // give way, and the batch is cut short by the deadline for exactly this
    // reason.
    println!();
    busy_reactor(&rt);
}

/// A timer competing with a run queue that never empties.
fn busy_reactor(rt: &Runtime) {
    const PERIOD: Duration = Duration::from_micros(200);
    const TICKS: usize = 500;

    let over = rt.block_on(async {
        // A task that yields forever, so the executor always has something to
        // run and never reaches the branch that would park it.
        let spinner = scr::task::spawn(async {
            loop {
                scr::task::yield_now().await;
            }
        });

        let mut interval = time::interval(PERIOD);
        let mut over = Vec::with_capacity(TICKS);
        interval.tick().await;

        for _ in 0..TICKS {
            let scheduled = interval.period_start();
            interval.tick().await;
            over.push(Instant::now().saturating_duration_since(scheduled));
        }

        spinner.abort();
        over
    });

    println!(
        "an interval of {} against a run queue that never empties: {}",
        format_duration(PERIOD),
        row(over).trim(),
    );
}

fn row(mut over: Vec<Duration>) -> String {
    over.sort_unstable();

    format!(
        "{:>10} {:>10} {:>10}",
        format_duration(over[0]),
        format_duration(over[over.len() / 2]),
        format_duration(over[over.len() * 99 / 100]),
    )
}

fn format_duration(d: Duration) -> String {
    let ns = d.as_nanos();

    match ns {
        0..1_000 => format!("{ns}ns"),
        1_000..1_000_000 => format!("{:.1}µs", ns as f64 / 1e3),
        _ => format!("{:.2}ms", ns as f64 / 1e6),
    }
}
