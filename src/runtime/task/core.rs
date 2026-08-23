#![expect(unsafe_op_in_unsafe_fn)]

use std::panic::Location;
use std::pin::Pin;
use std::ptr::NonNull;
use std::task::{Context, Poll, Waker};
use std::cell::UnsafeCell;
use super::Id;
use super::state::State;
use super::raw::Vtable;

use crate::runtime::context;

#[repr(C)]
pub(super) struct Cell<T: Future, S> {
    pub(super) header: Header,
    pub(super) core: Core<T, S>,
    pub(super) trailer: Trailer,
}

pub(super) struct CoreStage<T: Future> {
    stage: UnsafeCell<Stage<T>>,
}

#[repr(C)]
pub(super) struct Core<T: Future, S> {
    pub(super) scheduler: S,
    pub(super) task_id: Id,
    pub(super) spawned_at: &'static Location<'static>,
    pub(super) stage: CoreStage<T>,
}

#[repr(C)]
pub(crate) struct Header {
    pub(super) state: std::cell::Cell<State>,
    pub(super) vtable: &'static Vtable,
}

pub(super) struct Trailer {
    pub(super) waker: UnsafeCell<Option<Waker>>,
}

#[repr(C)]
pub(super) enum Stage<T: Future> {
    Running(T),
    Finished(super::Result<T::Output>),
    Consumed,
}

impl<T: Future, S: Schedule> Cell<T, S> {
    pub(super) fn new(
        future: T,
        scheduler: S,
        state: State,
        task_id: Id,
        spawned_at: &'static Location<'static>,
    ) -> Box<Cell<T, S>> {
        fn new_header(
            state: State,
            vtable: &'static Vtable,
        ) -> Header {
            Header {
                state: std::cell::Cell::new(state),
                vtable,
            }
        }

        let vtable = raw::vtable::<T, S>();
        let result = Box::new(Cell {
            trailer: Trailer::new(),
            header: new_header(
                state,
                vtable,
            ),
            core: Core {
                scheduler,
                stage: CoreStage {
                    stage: UnsafeCell::new(Stage::Running(future)),
                },
                task_id,
                spawned_at,
            },
        });

        #[cfg(debug_assertions)]
        {
            unsafe fn check<S>(
                header: &Header,
                trailer: &Trailer,
                scheduler: &S,
                task_id: &Id,
                spawn_location: &&'static Location<'static>,
            ) {
                let trailer_addr = trailer as *const Trailer as usize;
                let trailer_ptr = unsafe { Header::get_trailer(NonNull::from(header)) };
                assert_eq!(trailer_addr, trailer_ptr.as_ptr() as usize);

                let scheduler_addr = scheduler as *const S as usize;
                let scheduler_ptr = unsafe { Header::get_scheduler::<S>(NonNull::from(header)) };
                assert_eq!(scheduler_addr, scheduler_ptr.as_ptr() as usize);

                let id_addr = task_id as *const Id as usize;
                let id_ptr = unsafe { Header::get_id_ptr(NonNull::from(header)) };
                assert_eq!(id_addr, id_ptr.as_ptr() as usize);

                {
                    let spawn_location_addr =
                        spawn_location as *const &'static Location<'static> as usize;
                    let spawn_location_ptr =
                        unsafe { Header::get_spawn_location_ptr(NonNull::from(header)) };
                    assert_eq!(spawn_location_addr, spawn_location_ptr.as_ptr() as usize);
                }
            }
            unsafe {
                check(
                    &result.header,
                    &result.trailer,
                    &result.core.scheduler,
                    &result.core.task_id,
                    &result.core.spawned_at,
                );
            }
        }

        result
    }
}

impl<T: Future> CoreStage<T> {
    pub(super) fn with_mut<R>(&self, f: impl FnOnce(*mut Stage<T>) -> R) -> R {
        self.stage.with_mut(f)
    }
}

pub(crate) struct TaskIdGuard;

impl TaskIdGuard {
    fn enter(id: Id) -> Self {
        context::set_task_id(Some(id));
        Self
    }
}

impl Drop for TaskIdGuard {
    fn drop(&mut self) {
        context::set_task_id(None);
    }
}

impl<T: Future, S: Schedule> Core<T, S> {
    /// Polls the future.
    ///
    /// # Safety
    ///
    /// The caller must ensure it is safe to mutate the `state` field. This
    /// requires ensuring mutual exclusion between any concurrent thread that
    /// might modify the future or output field.
    ///
    /// `self` must also be pinned. This is handled by storing the task on the
    /// heap.
    pub(super) fn poll(&self, mut cx: Context<'_>) -> Poll<T::Output> {
        let res = {
            self.stage.stage.with_mut(|ptr| {
                // Safety: The caller ensures mutual exclusion to the field.
                let future = match unsafe { &mut *ptr } {
                    Stage::Running(future) => future,
                    _ => unreachable!("unexpected stage"),
                };

                // Safety: The caller ensures the future is pinned.
                let future = unsafe { Pin::new_unchecked(future) };

                let _guard = TaskIdGuard::enter(self.task_id);
                future.poll(&mut cx)
            })
        };

        if res.is_ready() {
            self.drop_future_or_output();
        }

        res
    }

    /// Drops the future.
    ///
    /// # Safety
    ///
    /// The caller must ensure it is safe to mutate the `stage` field.
    pub(super) fn drop_future_or_output(&self) {
        // Safety: the caller ensures mutual exclusion to the field.
        unsafe {
            self.set_stage(Stage::Consumed);
        }
    }

    /// Stores the task output.
    ///
    /// # Safety
    ///
    /// The caller must ensure it is safe to mutate the `stage` field.
    pub(super) fn store_output(&self, output: super::Result<T::Output>) {
        // Safety: the caller ensures mutual exclusion to the field.
        unsafe {
            self.set_stage(Stage::Finished(output));
        }
    }

    /// Takes the task output.
    ///
    /// # Safety
    ///
    /// The caller must ensure it is safe to mutate the `stage` field.
    pub(super) fn take_output(&self) -> super::Result<T::Output> {
        use std::mem;

        self.stage.stage.with_mut(|ptr| {
            // Safety:: the caller ensures mutual exclusion to the field.
            match mem::replace(unsafe { &mut *ptr }, Stage::Consumed) {
                Stage::Finished(output) => output,
                _ => panic!("JoinHandle polled after completion"),
            }
        })
    }

    unsafe fn set_stage(&self, stage: Stage<T>) {
        let _guard = TaskIdGuard::enter(self.task_id);
        self.stage.stage.with_mut(|ptr| *ptr = stage);
    }
}

impl Header {
    pub(super) unsafe fn set_next(&self, next: Option<NonNull<Header>>) {
        self.queue_next.with_mut(|ptr| *ptr = next);
    }

    /// Gets a pointer to the `Trailer` of the task containing this `Header`.
    ///
    /// # Safety
    ///
    /// The provided raw pointer must point at the header of a task.
    pub(super) unsafe fn get_trailer(me: NonNull<Header>) -> NonNull<Trailer> {
        let offset = me.as_ref().vtable.trailer_offset;
        let trailer = me.as_ptr().cast::<u8>().add(offset).cast::<Trailer>();
        NonNull::new_unchecked(trailer)
    }

    /// Gets a pointer to the scheduler of the task containing this `Header`.
    ///
    /// # Safety
    ///
    /// The provided raw pointer must point at the header of a task.
    ///
    /// The generic type S must be set to the correct scheduler type for this
    /// task.
    pub(super) unsafe fn get_scheduler<S>(me: NonNull<Header>) -> NonNull<S> {
        let offset = me.as_ref().vtable.scheduler_offset;
        let scheduler = me.as_ptr().cast::<u8>().add(offset).cast::<S>();
        NonNull::new_unchecked(scheduler)
    }

    /// Gets a pointer to the id of the task containing this `Header`.
    ///
    /// # Safety
    ///
    /// The provided raw pointer must point at the header of a task.
    pub(super) unsafe fn get_id_ptr(me: NonNull<Header>) -> NonNull<Id> {
        let offset = me.as_ref().vtable.id_offset;
        let id = me.as_ptr().cast::<u8>().add(offset).cast::<Id>();
        NonNull::new_unchecked(id)
    }

    /// Gets the id of the task containing this `Header`.
    ///
    /// # Safety
    ///
    /// The provided raw pointer must point at the header of a task.
    pub(super) unsafe fn get_id(me: NonNull<Header>) -> Id {
        let ptr = Header::get_id_ptr(me).as_ptr();
        *ptr
    }

    /// Gets a pointer to the source code location where the task containing
    /// this `Header` was spawned.
    ///
    /// # Safety
    ///
    /// The provided raw pointer must point at the header of a task.
    pub(super) unsafe fn get_spawn_location_ptr(
        me: NonNull<Header>,
    ) -> NonNull<&'static Location<'static>> {
        let offset = me.as_ref().vtable.spawn_location_offset;
        let spawned_at = me
            .as_ptr()
            .cast::<u8>()
            .add(offset)
            .cast::<&'static Location<'static>>();
        NonNull::new_unchecked(spawned_at)
    }

    /// Gets the source code location where the task containing
    /// this `Header` was spawned
    ///
    /// # Safety
    ///
    /// The provided raw pointer must point at the header of a task.
    pub(super) unsafe fn get_spawn_location(me: NonNull<Header>) -> &'static Location<'static> {
        let ptr = Header::get_spawn_location_ptr(me).as_ptr();
        *ptr
    }
}

impl Trailer {
    fn new() -> Self {
        Trailer {
            waker: UnsafeCell::new(None),
        }
    }

    pub(super) unsafe fn set_waker(&self, waker: Option<Waker>) {
        self.waker.with_mut(|ptr| {
            *ptr = waker;
        });
    }

    pub(super) unsafe fn will_wake(&self, waker: &Waker) -> bool {
        self.waker
            .with(|ptr| (*ptr).as_ref().unwrap().will_wake(waker))
    }

    pub(super) fn wake_join(&self) {
        self.waker.with(|ptr| match unsafe { &*ptr } {
            Some(waker) => waker.wake_by_ref(),
            None => panic!("waker missing"),
        });
    }
}

#[test]
fn header_lte_cache_line() {
    assert!(std::mem::size_of::<Header>() <= 8 * std::mem::size_of::<*const ()>());
}