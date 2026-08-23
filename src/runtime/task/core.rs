//! Core task module.
//!
//! # Safety
//!
//! The functions in this module are private to the `task` module. All of them
//! should be considered `unsafe` to use, but are not marked as such since it
//! would be too noisy.
//!
//! Make sure to consult the relevant safety section of each function before
//! use.

#![expect(unsafe_op_in_unsafe_fn)]

use std::panic::Location;
use std::pin::Pin;
use std::ptr::{self, NonNull};
use std::task::{Context, Poll, Waker};

use super::Id;
use super::Schedule;
use super::list::Pointers;
use super::raw::{self, Vtable};
use super::state::State;
use crate::runtime::context;
use crate::util::UnsafeCell;

/// The task cell. Contains the components of the task.
///
/// It is critical for `Header` to be the first field as the task structure will
/// be referenced by both *mut Cell and *mut Header.
///
/// Any changes to the layout of this struct _must_ also be reflected in the
/// `const` fns in raw.rs.
#[repr(C)]
pub(super) struct Cell<T: Future, S> {
    /// Hot task state data.
    pub(super) header: Header,

    /// Either the future or output, depending on the execution stage.
    pub(super) core: Core<T, S>,

    /// Cold data.
    pub(super) trailer: Trailer,
}

pub(super) struct CoreStage<T: Future> {
    stage: UnsafeCell<Stage<T>>,
}

/// The core of the task.
///
/// Holds the future or output, depending on the stage of execution.
///
/// Any changes to the layout of this struct _must_ also be reflected in the
/// `const` fns in raw.rs.
#[repr(C)]
pub(super) struct Core<T: Future, S> {
    /// Scheduler used to drive this future.
    pub(super) scheduler: S,

    /// The task's ID, is used for the `JoinHandle` and for tracking which task
    /// is currently running.
    pub(super) task_id: Id,

    /// The source code location where this task was spawned.
    pub(super) spawned_at: &'static Location<'static>,

    /// Either the future or the output.
    pub(super) stage: CoreStage<T>,
}

/// Crate public as this is also needed by the pool.
///
/// The header is the only part of the task that is accessed without knowing the
/// concrete `T` and `S` types, so it is kept as small as possible: two words.
/// Everything that is only needed on cold paths lives in the [`Trailer`].
#[repr(C)]
pub(crate) struct Header {
    /// Task state.
    pub(super) state: State,

    /// Table of function pointers for executing actions on the task.
    pub(super) vtable: &'static Vtable,
}

/// Cold data is stored after the future. Any change to the layout of this
/// struct _must_ also be reflected in `Trailer::addr_of_owned`.
#[repr(C)]
pub(super) struct Trailer {
    /// Pointers for the linked list in the `OwnedTasks` that owns this task.
    pub(super) owned: Pointers,

    /// Consumer task waiting on completion of this task.
    pub(super) waker: UnsafeCell<Option<Waker>>,
}

/// Either the future or the output.
#[repr(C)]
pub(super) enum Stage<T: Future> {
    Running(T),
    Finished(super::Result<T::Output>),
    Consumed,
}

impl<T: Future, S: Schedule> Cell<T, S> {
    /// Allocates a new task cell, containing the header, trailer, and core
    /// structures.
    pub(super) fn new(
        future: T,
        scheduler: S,
        state: State,
        task_id: Id,
        spawned_at: &'static Location<'static>,
    ) -> Box<Cell<T, S>> {
        // Separated into a non-generic function to reduce LLVM codegen
        fn new_header(state: State, vtable: &'static Vtable) -> Header {
            Header { state, vtable }
        }

        let vtable = raw::vtable::<T, S>();
        let result = Box::new(Cell {
            trailer: Trailer::new(),
            header: new_header(state, vtable),
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
            // Using a separate function for this code avoids instantiating it separately for every
            // generic parameter.
            #[inline(never)]
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

                let spawn_location_addr =
                    spawn_location as *const &'static Location<'static> as usize;
                let spawn_location_ptr =
                    unsafe { Header::get_spawn_location_ptr(NonNull::from(header)) };
                assert_eq!(spawn_location_addr, spawn_location_ptr.as_ptr() as usize);
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

/// Set and clear the task id in the context when the future is executed or
/// dropped, or when the output produced by the future is dropped.
pub(crate) struct TaskIdGuard {
    parent_task_id: Option<Id>,
}

impl TaskIdGuard {
    fn enter(id: Id) -> Self {
        TaskIdGuard {
            parent_task_id: context::set_current_task_id(Some(id)),
        }
    }
}

impl Drop for TaskIdGuard {
    fn drop(&mut self) {
        context::set_current_task_id(self.parent_task_id);
    }
}

impl<T: Future, S: Schedule> Core<T, S> {
    /// Polls the future.
    ///
    /// # Safety
    ///
    /// The caller must ensure it is safe to mutate the `stage` field. This
    /// requires ensuring that the task is not concurrently polled, which is
    /// guaranteed by the `RUNNING` bit acting as a lock.
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
            // Safety: the caller ensures mutual exclusion to the field.
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

    /// Gets the source code location where the task containing this `Header`
    /// was spawned.
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
            owned: Pointers::new(),
        }
    }

    /// Gets a pointer to the `owned` field of the `Trailer`.
    ///
    /// # Safety
    ///
    /// The provided raw pointer must point at the trailer of a task.
    pub(super) unsafe fn addr_of_owned(me: NonNull<Trailer>) -> NonNull<Pointers> {
        let me = me.as_ptr();
        let field = ptr::addr_of_mut!((*me).owned);
        NonNull::new_unchecked(field)
    }

    /// # Safety
    ///
    /// The `JoinHandle` is the only owner of this field, and this runtime is
    /// single threaded, so the caller must simply be the `JoinHandle` (or the
    /// runtime once `JOIN_INTEREST` has been unset).
    pub(super) unsafe fn set_waker(&self, waker: Option<Waker>) {
        self.waker.with_mut(|ptr| {
            *ptr = waker;
        });
    }

    /// Returns `true` if a waker is stored and it wakes the same task as
    /// `waker`.
    ///
    /// # Safety
    ///
    /// See [`Trailer::set_waker`].
    pub(super) unsafe fn will_wake(&self, waker: &Waker) -> bool {
        self.waker.with(|ptr| match &*ptr {
            Some(stored) => stored.will_wake(waker),
            None => false,
        })
    }

    /// Wakes the join waker, if one has been stored.
    ///
    /// Unlike `tokio`, a missing waker is not an error: the `JOIN_WAKER` bit
    /// that would tell us whether one is present does not exist in a single
    /// threaded runtime, so we simply check the `Option`.
    pub(super) fn wake_join(&self) {
        self.waker.with(|ptr| {
            // Safety: the runtime only reads the waker once the task is
            // complete, at which point the `JoinHandle` will not write to it.
            if let Some(waker) = unsafe { &*ptr } {
                waker.wake_by_ref();
            }
        });
    }
}

#[test]
fn header_lte_cache_line() {
    assert!(std::mem::size_of::<Header>() <= 8 * std::mem::size_of::<*const ()>());
}
