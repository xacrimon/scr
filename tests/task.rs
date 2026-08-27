#![feature(local_waker)]

use std::cell::{Cell, RefCell};
use std::future;
use std::pin::Pin;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::task::{Context, LocalWaker, Poll};

use scr::Runtime;
use scr::task::{self, JoinHandle};

/// Tasks always finish with `()`, so a test that cares about what a task
/// computed hands the value out through a shared slot instead.
fn slot<T>() -> Rc<Cell<Option<T>>> {
    Rc::new(Cell::new(None))
}

#[test]
fn block_on_returns_output() {
    let rt = Runtime::new().expect("Runtime::new");
    assert_eq!(rt.block_on(async { 1 + 2 }), 3);
}

#[test]
fn spawn_and_join() {
    let rt = Runtime::new().expect("Runtime::new");
    let out = slot::<&'static str>();

    rt.block_on({
        let out = Rc::clone(&out);
        async move {
            let handle = task::spawn(async move { out.set(Some("hello")) });
            handle.await.unwrap();
        }
    });

    assert_eq!(out.take(), Some("hello"));
}

#[test]
fn spawn_from_outside_block_on() {
    let rt = Runtime::new().expect("Runtime::new");
    let out = slot::<u32>();

    let handle = {
        let out = Rc::clone(&out);
        rt.spawn(async move { out.set(Some(7)) })
    };

    rt.block_on(handle).unwrap();

    assert_eq!(out.take(), Some(7));
}

#[test]
fn tasks_run_in_spawn_order() {
    let rt = Runtime::new().expect("Runtime::new");
    let order = Rc::new(Cell::new(String::new()));

    rt.block_on({
        let order = Rc::clone(&order);
        async move {
            for c in ['a', 'b', 'c'] {
                let order = Rc::clone(&order);
                task::spawn(async move {
                    let mut s = order.take();
                    s.push(c);
                    order.set(s);
                });
            }

            // Let the spawned tasks run.
            for _ in 0..4 {
                task::yield_now().await;
            }
        }
    });

    assert_eq!(order.take(), "abc");
}

#[test]
fn nested_spawn() {
    let rt = Runtime::new().expect("Runtime::new");
    let out = slot::<u32>();

    rt.block_on({
        let out = Rc::clone(&out);
        async move {
            task::spawn(async move {
                task::spawn(async move { out.set(Some(42)) }).await.unwrap();
            })
            .await
            .unwrap();
        }
    });

    assert_eq!(out.take(), Some(42));
}

#[test]
fn detached_task_still_runs() {
    let rt = Runtime::new().expect("Runtime::new");
    let ran = Rc::new(Cell::new(false));

    rt.block_on({
        let ran = Rc::clone(&ran);
        async move {
            // Drop the `JoinHandle` immediately; the task is detached.
            drop(task::spawn(async move { ran.set(true) }));

            task::yield_now().await;
        }
    });

    assert!(ran.get());
}

/// A task's future is dropped as soon as it finishes, without waiting for the
/// `JoinHandle` to be awaited or dropped.
#[test]
fn a_finished_task_drops_its_future() {
    struct OnDrop(Rc<Cell<bool>>);
    impl Drop for OnDrop {
        fn drop(&mut self) {
            self.0.set(true);
        }
    }

    let rt = Runtime::new().expect("Runtime::new");
    let dropped = Rc::new(Cell::new(false));

    rt.block_on({
        let dropped = Rc::clone(&dropped);
        async move {
            let guard = OnDrop(dropped);
            let handle = task::spawn(async move {
                let _guard = guard;
            });

            // Let the task complete.
            task::yield_now().await;
            task::yield_now().await;

            assert!(handle.is_finished());
            drop(handle);
        }
    });

    assert!(dropped.get());
}

#[test]
fn task_panic_is_reported_to_join_handle() {
    let rt = Runtime::new().expect("Runtime::new");

    let err = rt.block_on(async {
        let handle = task::spawn(async { panic!("boom") });
        handle.await.unwrap_err()
    });

    assert!(err.is_panic());
    assert!(!err.is_cancelled());
    assert_eq!(
        err.into_panic().downcast_ref::<&'static str>().copied(),
        Some("boom")
    );
}

/// A detached task that panics has nowhere to report to; the payload is dropped
/// on the completion path rather than leaking.
#[test]
fn a_detached_task_that_panics_swallows_the_panic() {
    // The payload must be `Send`, so the flag it sets is an atomic rather than
    // the `Rc<Cell<_>>` the other tests use.
    struct OnDrop(Arc<AtomicBool>);
    impl Drop for OnDrop {
        fn drop(&mut self) {
            self.0.store(true, Ordering::Relaxed);
        }
    }

    let rt = Runtime::new().expect("Runtime::new");
    let payload_dropped = Arc::new(AtomicBool::new(false));

    rt.block_on({
        let payload_dropped = Arc::clone(&payload_dropped);
        async move {
            // The payload carries a value whose destructor the test observes.
            drop(task::spawn(async move {
                std::panic::panic_any(OnDrop(payload_dropped));
            }));

            task::yield_now().await;
            task::yield_now().await;
        }
    });

    assert!(
        payload_dropped.load(Ordering::Relaxed),
        "the panic payload of a detached task must be dropped"
    );
}

#[test]
fn abort_before_first_poll() {
    let rt = Runtime::new().expect("Runtime::new");
    let ran = Rc::new(Cell::new(false));

    let err = rt.block_on({
        let ran = Rc::clone(&ran);
        async move {
            let handle = task::spawn(async move {
                ran.set(true);
            });
            handle.abort();
            handle.await.unwrap_err()
        }
    });

    assert!(err.is_cancelled());
    assert!(!ran.get(), "an aborted task must not be polled");
}

#[test]
fn abort_pending_task() {
    let rt = Runtime::new().expect("Runtime::new");

    let err = rt.block_on(async {
        let handle = task::spawn(future::pending::<()>());

        // Let the task be polled once so that it parks.
        task::yield_now().await;

        handle.abort();
        handle.await.unwrap_err()
    });

    assert!(err.is_cancelled());
}

#[test]
fn abort_handle_clone_and_id() {
    let rt = Runtime::new().expect("Runtime::new");

    rt.block_on(async {
        let handle = task::spawn(future::pending::<()>());
        let abort = handle.abort_handle();
        let abort2 = abort.clone();

        assert_eq!(abort.id(), handle.id());
        assert_eq!(abort2.id(), handle.id());
        assert!(!abort.is_finished());

        task::yield_now().await;
        abort2.abort();

        assert!(handle.await.unwrap_err().is_cancelled());
        assert!(abort.is_finished());
    });
}

#[test]
fn completed_task_is_finished() {
    let rt = Runtime::new().expect("Runtime::new");

    rt.block_on(async {
        let handle = task::spawn(async {});
        assert!(!handle.is_finished());

        task::yield_now().await;
        task::yield_now().await;

        assert!(handle.is_finished());
        handle.await.unwrap();
    });
}

#[test]
fn task_id_is_visible_from_inside_the_task() {
    let rt = Runtime::new().expect("Runtime::new");
    let seen = slot::<scr::task::Id>();

    rt.block_on({
        let seen = Rc::clone(&seen);
        async move {
            // `block_on` is not a task.
            assert!(task::try_id().is_none());

            let handle = {
                let seen = Rc::clone(&seen);
                task::spawn(async move { seen.set(Some(task::id())) })
            };
            let outer = handle.id();
            handle.await.unwrap();

            assert_eq!(seen.get(), Some(outer));
        }
    });
}

#[test]
fn spawn_location_is_captured() {
    let rt = Runtime::new().expect("Runtime::new");

    rt.block_on(async {
        let handle = task::spawn(async {});
        let location = handle.spawned_at();

        assert!(location.file().ends_with("task.rs"));
        handle.await.unwrap();
    });
}

#[test]
fn dropping_the_runtime_shuts_down_pending_tasks() {
    let rt = Runtime::new().expect("Runtime::new");
    let dropped = Rc::new(Cell::new(false));

    struct OnDrop(Rc<Cell<bool>>);
    impl Drop for OnDrop {
        fn drop(&mut self) {
            self.0.set(true);
        }
    }

    let handle = {
        let dropped = Rc::clone(&dropped);
        rt.spawn(async move {
            let _guard = OnDrop(dropped);
            future::pending::<()>().await;
        })
    };

    // Poll the task once so that it is parked inside the runtime.
    rt.block_on(async { task::yield_now().await });

    drop(rt);

    assert!(dropped.get(), "the task's future must be dropped");
    assert!(handle.is_finished());
}

#[test]
fn waking_a_parked_task_reschedules_it() {
    let rt = Runtime::new().expect("Runtime::new");

    /// Returns `Pending` on the first poll, storing its waker; the waker is
    /// invoked by the test to reschedule the task.
    struct WakeMeOnce {
        waker: Rc<Cell<Option<LocalWaker>>>,
        repolled: Rc<Cell<bool>>,
        polled: bool,
    }

    impl Future for WakeMeOnce {
        type Output = ();

        fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
            if self.polled {
                self.repolled.set(true);
                return Poll::Ready(());
            }

            self.polled = true;
            self.waker.set(Some(cx.local_waker().clone()));
            Poll::Pending
        }
    }

    let repolled = Rc::new(Cell::new(false));

    rt.block_on({
        let repolled = Rc::clone(&repolled);
        async move {
            let waker: Rc<Cell<Option<LocalWaker>>> = Rc::new(Cell::new(None));
            let handle = task::spawn(WakeMeOnce {
                waker: Rc::clone(&waker),
                repolled,
                polled: false,
            });

            // Let the task park.
            task::yield_now().await;

            waker.take().expect("task should have parked").wake();

            handle.await.unwrap();
        }
    });

    assert!(repolled.get());
}

#[test]
fn yield_now_lets_other_tasks_run() {
    let rt = Runtime::new().expect("Runtime::new");
    let log = Rc::new(Cell::new(String::new()));

    let push = |log: &Rc<Cell<String>>, c: char| {
        let mut s = log.take();
        s.push(c);
        log.set(s);
    };

    rt.block_on({
        let log = Rc::clone(&log);
        async move {
            let a = {
                let log = Rc::clone(&log);
                task::spawn(async move {
                    push(&log, '1');
                    task::yield_now().await;
                    push(&log, '3');
                })
            };
            let b = {
                let log = Rc::clone(&log);
                task::spawn(async move {
                    push(&log, '2');
                    task::yield_now().await;
                    push(&log, '4');
                })
            };

            a.await.unwrap();
            b.await.unwrap();
        }
    });

    assert_eq!(log.take(), "1234");
}

/// A task that aborts itself from inside its own poll must not have its future
/// dropped while that future is still live on the stack; the drop is deferred
/// until the poll returns.
#[test]
fn task_aborting_itself_defers_the_drop() {
    struct OnDrop(Rc<Cell<bool>>);
    impl Drop for OnDrop {
        fn drop(&mut self) {
            self.0.set(true);
        }
    }

    let rt = Runtime::new().expect("Runtime::new");
    let dropped = Rc::new(Cell::new(false));
    let survived_abort = Rc::new(Cell::new(false));

    let err = rt.block_on({
        let dropped = Rc::clone(&dropped);
        let survived_abort = Rc::clone(&survived_abort);
        async move {
            let slot: Rc<Cell<Option<scr::task::AbortHandle>>> = Rc::new(Cell::new(None));

            let handle = {
                let slot = Rc::clone(&slot);
                task::spawn(async move {
                    let _guard = OnDrop(Rc::clone(&dropped));

                    // Abort ourselves from inside our own poll.
                    slot.take().unwrap().abort();

                    // If `abort` had dropped this future in place, we would be
                    // executing inside a destroyed future right now.
                    assert!(!dropped.get(), "a running future must not be dropped");
                    survived_abort.set(true);

                    future::pending::<()>().await;
                })
            };

            slot.set(Some(handle.abort_handle()));
            handle.await.unwrap_err()
        }
    });

    assert!(survived_abort.get());
    assert!(err.is_cancelled());
    assert!(dropped.get(), "the future is dropped once the poll returns");
}

/// A task that requests its own cancellation and then finishes anyway in that
/// same poll ran to completion, so it reports success. The cancellation flag is
/// still set when the task completes, so it must not be mistaken for the
/// outcome.
#[test]
fn a_task_that_aborts_itself_and_then_finishes_reports_success() {
    let rt = Runtime::new().expect("Runtime::new");
    let ran = Rc::new(Cell::new(false));

    rt.block_on({
        let ran = Rc::clone(&ran);
        async move {
            let slot: Rc<Cell<Option<scr::task::AbortHandle>>> = Rc::new(Cell::new(None));

            let handle = {
                let slot = Rc::clone(&slot);
                task::spawn(async move {
                    slot.take().unwrap().abort();

                    // Returns `Ready` in the very poll that asked to be
                    // cancelled, so the cancellation never takes effect.
                    ran.set(true);
                })
            };

            slot.set(Some(handle.abort_handle()));
            handle.await.unwrap();
        }
    });

    assert!(ran.get());
}

/// Repeated wakes of a parked task must collapse into a single re-poll.
#[test]
fn repeated_wakes_queue_the_task_once() {
    let rt = Runtime::new().expect("Runtime::new");
    let polls = Rc::new(Cell::new(0u32));

    struct CountPolls {
        polls: Rc<Cell<u32>>,
        waker: Rc<Cell<Option<LocalWaker>>>,
    }

    impl Future for CountPolls {
        type Output = ();

        fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
            self.polls.set(self.polls.get() + 1);

            if self.polls.get() >= 2 {
                return Poll::Ready(());
            }

            self.waker.set(Some(cx.local_waker().clone()));
            Poll::Pending
        }
    }

    rt.block_on({
        let polls = Rc::clone(&polls);
        async move {
            let waker: Rc<Cell<Option<LocalWaker>>> = Rc::new(Cell::new(None));
            let handle = task::spawn(CountPolls {
                polls: Rc::clone(&polls),
                waker: Rc::clone(&waker),
            });

            // Let the task park.
            task::yield_now().await;

            let waker = waker.take().expect("task should have parked");
            for _ in 0..5 {
                waker.wake_by_ref();
            }

            handle.await.unwrap();
        }
    });

    assert_eq!(
        polls.get(),
        2,
        "five wakes must produce exactly one re-poll"
    );
}

/// A task that wakes itself *by value* from inside its own poll must be
/// re-queued once, not scheduled while it is still on the stack.
#[test]
fn self_wake_by_value_during_poll() {
    let rt = Runtime::new().expect("Runtime::new");
    let polls = Rc::new(Cell::new(0u32));

    struct SelfWake {
        polls: Rc<Cell<u32>>,
    }

    impl Future for SelfWake {
        type Output = ();

        fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
            self.polls.set(self.polls.get() + 1);

            if self.polls.get() >= 3 {
                return Poll::Ready(());
            }

            // `wake`, not `wake_by_ref`: this consumes a ref-count while the
            // task is RUNNING, which is the path under test here.
            #[allow(clippy::waker_clone_wake)]
            cx.local_waker().clone().wake();

            Poll::Pending
        }
    }

    rt.block_on({
        let polls = Rc::clone(&polls);
        async move {
            task::spawn(SelfWake { polls }).await.unwrap();
        }
    });

    assert_eq!(polls.get(), 3);
}

/// The task registry is a slab, so the slot a finished task occupied is handed
/// to a later one. Each task records its own key, and clears it on the way out,
/// so a recycled slot must never let one task's release evict another. Parked
/// tasks are interleaved between the waves so that the completing tasks free
/// slots from the middle of the registry rather than only off the end.
#[test]
fn registry_slots_are_reused_as_tasks_complete() {
    let rt = Runtime::new().expect("Runtime::new");
    let seen: Rc<RefCell<Vec<u32>>> = Rc::new(RefCell::new(Vec::new()));

    rt.block_on({
        let seen = Rc::clone(&seen);
        async move {
            let mut parked: Vec<JoinHandle> = Vec::new();

            for wave in 0..4u32 {
                // Never completes, so it holds its slot for the rest of the test.
                parked.push(task::spawn(future::pending::<()>()));

                let handles: Vec<JoinHandle> = (0..8u32)
                    .map(|i| {
                        let seen = Rc::clone(&seen);
                        task::spawn(async move { seen.borrow_mut().push(wave * 8 + i) })
                    })
                    .collect();

                for handle in handles {
                    handle.await.unwrap();
                }
            }

            for handle in &parked {
                assert!(!handle.is_finished(), "the parked tasks must still be live");
            }
        }
    });

    assert_eq!(seen.take(), (0..32).collect::<Vec<u32>>());

    // Dropping the runtime now shuts down four parked tasks that sit in a slab
    // full of recycled and vacant slots.
}

/// Shutting a task down is polling it with cancellation already requested, so
/// it has to work on a task that is sitting in the run queue, having never been
/// polled at all.
#[test]
fn shutdown_kills_a_task_that_is_still_queued() {
    struct OnDrop(Rc<Cell<bool>>);
    impl Drop for OnDrop {
        fn drop(&mut self) {
            self.0.set(true);
        }
    }

    let rt = Runtime::new().expect("Runtime::new");
    let dropped = Rc::new(Cell::new(false));

    let handle = {
        // A capture, so that it is dropped even though the body never runs.
        let guard = OnDrop(Rc::clone(&dropped));
        rt.spawn(async move {
            let _guard = guard;
            future::pending::<()>().await;
        })
    };

    // The runtime is dropped without ever draining its run queue.
    drop(rt);

    assert!(dropped.get(), "the task's future must be dropped");
    assert!(handle.is_finished());
}

/// A future's destructor runs while its task holds the poll lock and is in no
/// registry, and it may wake another task from there. That wake has to reach
/// the run queue, which the shutdown loop then drains.
#[test]
fn a_destructor_may_wake_another_task_during_shutdown() {
    struct WakeOnDrop(Rc<Cell<Option<LocalWaker>>>);
    impl Drop for WakeOnDrop {
        fn drop(&mut self) {
            if let Some(waker) = self.0.take() {
                waker.wake();
            }
        }
    }

    /// Parks on the first poll, leaving its waker in `slot`.
    struct ParkAndShare {
        slot: Rc<Cell<Option<LocalWaker>>>,
    }

    impl Future for ParkAndShare {
        type Output = ();

        fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
            self.slot.set(Some(cx.local_waker().clone()));
            Poll::Pending
        }
    }

    let rt = Runtime::new().expect("Runtime::new");
    let slot: Rc<Cell<Option<LocalWaker>>> = Rc::new(Cell::new(None));
    let woke = Rc::new(Cell::new(false));

    // The waker is spawned first so that it is shut down first, while the task
    // it wakes is still alive.
    let waker_task = {
        let guard = WakeOnDrop(Rc::clone(&slot));
        let woke = Rc::clone(&woke);
        rt.spawn(async move {
            let _guard = guard;
            woke.set(true);
            future::pending::<()>().await;
        })
    };

    let parked = rt.spawn(ParkAndShare {
        slot: Rc::clone(&slot),
    });

    rt.block_on(async { task::yield_now().await });

    assert!(woke.get(), "both tasks must have parked");
    drop(rt);

    assert!(waker_task.is_finished());
    assert!(parked.is_finished());
}

/// An id names a registry slot, so the slot a finished task held is handed to
/// the next task spawned.
#[test]
fn ids_are_reused_once_a_task_completes() {
    let rt = Runtime::new().expect("Runtime::new");

    rt.block_on(async {
        let first = task::spawn(async {});
        let id = first.id();
        first.await.unwrap();

        let second = task::spawn(async {});
        assert_eq!(second.id(), id);
        second.await.unwrap();
    });
}

/// Two runtimes may exist on one thread; each drives only its own tasks.
#[test]
fn two_runtimes_on_one_thread_are_independent() {
    let a = Runtime::new().expect("Runtime::new");
    let b = Runtime::new().expect("Runtime::new");

    let seen = Rc::new(RefCell::new(Vec::new()));

    let run = |rt: &Runtime, n: u32| {
        let seen = Rc::clone(&seen);
        rt.block_on(async move {
            task::spawn(async move { seen.borrow_mut().push(n) })
                .await
                .unwrap();
        });
    };

    run(&a, 1);
    run(&b, 2);
    run(&a, 3);

    assert_eq!(seen.take(), vec![1, 2, 3]);
}

/// A task's waker may only be fired while its runtime is entered, since that is
/// how the task finds the run queue to put itself on.
#[test]
#[should_panic(expected = "outside of a runtime")]
fn waking_a_task_outside_of_a_runtime_panics() {
    struct ParkAndShare {
        slot: Rc<Cell<Option<LocalWaker>>>,
    }

    impl Future for ParkAndShare {
        type Output = ();

        fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
            self.slot.set(Some(cx.local_waker().clone()));
            Poll::Pending
        }
    }

    let rt = Runtime::new().expect("Runtime::new");
    let slot: Rc<Cell<Option<LocalWaker>>> = Rc::new(Cell::new(None));

    let _handle = rt.spawn(ParkAndShare {
        slot: Rc::clone(&slot),
    });

    rt.block_on(async { task::yield_now().await });

    slot.take().expect("the task should have parked").wake();
}

/// Handles kept past the end of the runtime stay usable. Every task is complete
/// by then, and nothing a complete task can be asked to do reaches the runtime.
#[test]
fn handles_outliving_the_runtime_are_inert() {
    let rt = Runtime::new().expect("Runtime::new");

    let handle = rt.spawn(future::pending::<()>());
    let abort = handle.abort_handle();

    rt.block_on(async { task::yield_now().await });
    drop(rt);

    assert!(handle.is_finished());
    assert!(abort.is_finished());
    assert_eq!(abort.id(), handle.id());

    // Neither of these may go looking for a run queue.
    abort.abort();
    handle.abort();
}

/// A future whose destructor panics while the task is being cancelled reports
/// the panic rather than the cancellation.
#[test]
fn a_panic_while_cancelling_is_reported_as_a_panic() {
    struct PanicOnDrop;
    impl Drop for PanicOnDrop {
        fn drop(&mut self) {
            panic!("drop boom");
        }
    }

    let rt = Runtime::new().expect("Runtime::new");

    let err = rt.block_on(async {
        let handle = task::spawn(async {
            let _guard = PanicOnDrop;
            future::pending::<()>().await;
        });

        task::yield_now().await;
        handle.abort();
        handle.await.unwrap_err()
    });

    assert!(err.is_panic());
    assert!(!err.is_cancelled());
}

/// Aborting a task that has already finished must leave its result alone: the
/// cancellation flag is what a completed task reports, so setting it late would
/// turn a success into a cancellation.
#[test]
fn aborting_a_finished_task_does_nothing() {
    let rt = Runtime::new().expect("Runtime::new");
    let ran = Rc::new(Cell::new(false));

    rt.block_on({
        let ran = Rc::clone(&ran);
        async move {
            let handle = task::spawn(async move { ran.set(true) });

            task::yield_now().await;
            task::yield_now().await;
            assert!(handle.is_finished());

            handle.abort_handle().abort();
            handle.await.unwrap();
        }
    });

    assert!(ran.get());
}

/// A task's result can only be taken once, and asking twice is a bug worth
/// reporting rather than a silent hang.
#[test]
#[should_panic(expected = "polled after its result was taken")]
fn polling_a_join_handle_after_taking_its_result_panics() {
    let rt = Runtime::new().expect("Runtime::new");

    rt.block_on(async {
        let mut handle = task::spawn(async {});
        (&mut handle).await.unwrap();

        let _ = (&mut handle).await;
    });
}

/// A future that panics while being polled is left in the task, and has to be
/// dropped before the panic can be stored in its place. That drop may panic in
/// turn, and the panic already being reported is the one that wins.
#[test]
fn a_destructor_that_panics_after_a_failed_poll_is_swallowed() {
    struct PanicOnPollAndDrop;

    impl Future for PanicOnPollAndDrop {
        type Output = ();

        fn poll(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<()> {
            panic!("poll boom");
        }
    }

    impl Drop for PanicOnPollAndDrop {
        fn drop(&mut self) {
            panic!("drop boom");
        }
    }

    let rt = Runtime::new().expect("Runtime::new");

    let err = rt.block_on(async {
        let handle = task::spawn(PanicOnPollAndDrop);
        handle.await.unwrap_err()
    });

    assert!(err.is_panic());
    assert_eq!(
        err.into_panic().downcast_ref::<&'static str>().copied(),
        Some("poll boom"),
        "the panic from the poll is the one reported"
    );
}

/// A future that finishes and then panics on the way out is reported as a
/// panic. The task still completes rather than being left holding the poll
/// lock.
#[test]
fn a_destructor_that_panics_after_a_ready_poll_is_reported() {
    struct PanicOnDrop;

    impl Future for PanicOnDrop {
        type Output = ();

        fn poll(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<()> {
            Poll::Ready(())
        }
    }

    impl Drop for PanicOnDrop {
        fn drop(&mut self) {
            panic!("drop boom");
        }
    }

    let rt = Runtime::new().expect("Runtime::new");

    let err = rt.block_on(async {
        let handle = task::spawn(PanicOnDrop);
        handle.await.unwrap_err()
    });

    assert!(err.is_panic());
    assert_eq!(
        err.into_panic().downcast_ref::<&'static str>().copied(),
        Some("drop boom")
    );
}
