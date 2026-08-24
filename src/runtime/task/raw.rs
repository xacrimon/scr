//! The task allocation, and the type erased operations over it.
//!
//! The runtime refers to a task by a `NonNull<Header>`, which says nothing
//! about the future it holds. Everything that needs the future type again goes
//! through the three entries of [`Vtable`], each of which casts the pointer
//! back to a [`Cell<T>`].

#[cfg(not(debug_assertions))]
use std::hint;
use std::mem;
use std::panic::{self, AssertUnwindSafe, Location};
use std::pin::Pin;
use std::ptr::NonNull;
use std::task::{Context, Poll, Waker};
use std::{cell, fmt};

use crate::runtime::context;
use crate::runtime::task::state::{CallerRef, State, TransitionToIdle, TransitionToRunning};
use crate::runtime::task::waker::waker_ref;
use crate::runtime::task::{Id, JoinError, Runnable, Task};

/// The heap allocation backing a task.
///
/// `Header` has to come first, and the struct has to be `repr(C)`: a task is
/// passed around as a pointer to its header, and cast back to this type
/// whenever the future type is known again.
#[repr(C)]
struct Cell<T: Future> {
    header: Header,

    /// The future, or what it left behind.
    stage: cell::UnsafeCell<Stage<T>>,
}

/// The part of a task that can be reached without knowing its future type.
pub(crate) struct Header {
    /// Lifecycle flags and the reference count.
    pub(super) state: State,

    /// The three operations that need the future type back.
    vtable: &'static Vtable,

    /// The slot this task occupies in the runtime's registry, which doubles as
    /// its public id. Written once, when the task is created.
    pub(super) id: Id,

    /// Where the task was spawned.
    pub(super) spawned_at: &'static Location<'static>,

    /// Waker of whoever is awaiting the `JoinHandle`. The handle owns this
    /// field for as long as `JOIN_INTEREST` is set; the runtime reads it once,
    /// on completion, which cannot overlap because both happen on this thread.
    waker: cell::UnsafeCell<Option<Waker>>,
}

/// The future and its result share a slot: the future is dropped before the
/// result is stored, and the result is moved out when it is read.
enum Stage<T: Future> {
    Running(T),
    Finished(super::Result<T::Output>),
    Consumed,
}

struct Vtable {
    /// Polls the task, or kills it if it has been cancelled. Consumes the
    /// reference that granted the right to poll.
    poll: unsafe fn(NonNull<Header>),

    /// Moves the stored result into a `*mut Poll<Result<T::Output>>`, leaving
    /// it alone if the result has already been taken.
    take_output: unsafe fn(NonNull<Header>, *mut ()),

    /// Frees the allocation.
    dealloc: unsafe fn(NonNull<Header>),
}

fn vtable<T: Future>() -> &'static Vtable {
    &Vtable {
        poll: poll::<T>,
        take_output: take_output::<T>,
        dealloc: dealloc::<T>,
    }
}

/// A pointer to a task that does not own a reference to it.
///
/// Every handle to a task is a `RawTask` plus the obligation to release one
/// reference, which [`Task`] and the public handle types take care of.
#[derive(Clone, Copy)]
pub(crate) struct RawTask {
    ptr: NonNull<Header>,
}

impl RawTask {
    /// Allocates a task holding `future`, with a reference count of three.
    pub(super) fn new<T>(future: T, id: Id, spawned_at: &'static Location<'static>) -> RawTask
    where
        T: Future,
    {
        let ptr = Box::into_raw(Box::new(Cell {
            header: Header {
                state: State::new(),
                vtable: vtable::<T>(),
                id,
                spawned_at,
                waker: cell::UnsafeCell::new(None),
            },
            stage: cell::UnsafeCell::new(Stage::Running(future)),
        }));

        // Safety: `Header` is the first field of a `repr(C)` `Cell`, so the
        // allocation starts with it.
        RawTask {
            ptr: unsafe { NonNull::new_unchecked(ptr.cast()) },
        }
    }

    /// # Safety
    ///
    /// `ptr` must point at the header of a live task.
    pub(super) unsafe fn from_raw(ptr: NonNull<Header>) -> RawTask {
        RawTask { ptr }
    }

    pub(super) fn header_ptr(self) -> NonNull<Header> {
        self.ptr
    }

    pub(super) fn header(self) -> &'static Header {
        // Safety: the caller of every method here holds a reference to the
        // task, which keeps the allocation alive.
        unsafe { self.ptr.as_ref() }
    }

    pub(super) fn state(self) -> &'static State {
        &self.header().state
    }

    /// Polls the task, consuming the reference that granted the right to poll.
    pub(super) fn poll(self) {
        // Safety: the vtable was built for the future this task holds.
        unsafe { (self.header().vtable.poll)(self.ptr) }
    }

    /// Kills the task, consuming a reference.
    ///
    /// Shutting a task down is polling it with cancellation already requested:
    /// the poll entry point takes the lock, sees `CANCELLED`, and kills the
    /// task instead of going near the future.
    pub(super) fn shutdown(self) {
        self.state().set_cancelled();
        self.poll();
    }

    /// Moves the task's result into `dst`, leaving `dst` alone if the result
    /// has already been taken.
    ///
    /// # Safety
    ///
    /// The task must be complete, and `O` must be its output type.
    pub(super) unsafe fn take_output<O>(self, dst: &mut Poll<super::Result<O>>) {
        let dst = std::ptr::from_mut(dst).cast();

        // Safety: the caller guarantees the output type and that a result is
        // there to take.
        unsafe { (self.header().vtable.take_output)(self.ptr, dst) }
    }

    /// Frees the allocation. The caller must have dropped the last reference.
    pub(super) fn dealloc(self) {
        // Safety: nothing else can reach the task any more.
        unsafe { (self.header().vtable.dealloc)(self.ptr) }
    }

    pub(super) fn ref_inc(self) {
        self.state().ref_inc();
    }

    /// Drops one reference, freeing the task if it was the last.
    pub(super) fn drop_reference(self) {
        if self.state().ref_dec() {
            self.dealloc();
        }
    }

    /// Hands a `Runnable` to the run queue, consuming a reference that the
    /// caller guarantees is available for it — either one a transition just
    /// minted, or the caller's own that it is choosing to give up.
    fn schedule(self) {
        // Safety: the caller guarantees a reference is available to transfer.
        let runnable = Runnable(unsafe { Task::from_raw(self.ptr) });

        context::with_handle(|handle| handle.schedule(runnable));
    }

    /// Marks the task notified, queueing it if that is now this caller's job.
    /// No reference is consumed, so the caller must hold one throughout.
    fn notify(self) {
        if self.state().transition_to_notified(CallerRef::Kept) {
            self.schedule();
        }
    }

    /// Wakes the task, consuming a reference.
    ///
    /// If the task is idle, that very reference becomes the new `Runnable`'s
    /// — nothing is minted and nothing is dropped, since nothing else needs
    /// to change. Otherwise there is no `Runnable` to hand it to, so it is
    /// simply dropped.
    pub(super) fn wake_by_val(self) {
        if self.state().transition_to_notified(CallerRef::Consumed) {
            self.schedule();
        } else {
            self.drop_reference();
        }
    }

    /// Wakes the task. The caller keeps its reference.
    pub(super) fn wake_by_ref(self) {
        self.notify();
    }

    /// Asks the runtime to kill the task. The caller keeps its reference.
    ///
    /// Unlike [`RawTask::shutdown`] this only requests cancellation, so a task
    /// aborting itself has its future dropped once its poll returns rather
    /// than underneath the call to `abort`.
    pub(super) fn remote_abort(self) {
        self.state().set_cancelled();
        self.notify();
    }
}

impl Header {
    /// Stores the waker to notify when the task completes.
    ///
    /// # Safety
    ///
    /// Only the `JoinHandle` may call this, and only while `JOIN_INTEREST` is
    /// set.
    pub(super) unsafe fn set_waker(&self, waker: Option<Waker>) {
        // Safety: the caller is the only accessor of the field.
        unsafe { *self.waker.get() = waker };
    }

    /// Returns `true` if a waker is stored that wakes the same task as `waker`.
    ///
    /// # Safety
    ///
    /// See [`Header::set_waker`].
    pub(super) unsafe fn will_wake(&self, waker: &Waker) -> bool {
        // Safety: the caller is the only accessor of the field.
        match unsafe { &*self.waker.get() } {
            Some(stored) => stored.will_wake(waker),
            None => false,
        }
    }

    /// Wakes whoever is awaiting the `JoinHandle`, if anyone is.
    fn wake_join(&self) {
        // Safety: the task is complete, so the `JoinHandle` will not write to
        // the field while it is read here.
        if let Some(waker) = unsafe { &*self.waker.get() } {
            waker.wake_by_ref();
        }
    }
}

impl fmt::Debug for Header {
    fn fmt(&self, fmt: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt.debug_struct("Task")
            .field("id", &self.id)
            .field("state", &self.state)
            .finish()
    }
}

/// Names the task the current thread is running inside, so that [`task::id`]
/// can answer while user code is on the stack.
///
/// [`task::id`]: crate::task::id
pub(super) struct TaskIdGuard {
    parent: Option<Id>,
}

impl TaskIdGuard {
    pub(super) fn enter(id: Id) -> TaskIdGuard {
        TaskIdGuard {
            parent: context::set_current_task_id(Some(id)),
        }
    }
}

impl Drop for TaskIdGuard {
    fn drop(&mut self) {
        context::set_current_task_id(self.parent);
    }
}

// ===== the typed operations =====

/// # Safety
///
/// `ptr` must point at a live task whose future type is `T`.
unsafe fn stage_of<'a, T: Future>(ptr: NonNull<Header>) -> &'a cell::UnsafeCell<Stage<T>> {
    // Safety: the caller guarantees the future type, and `Header` is the first
    // field of a `repr(C)` `Cell`.
    unsafe { &ptr.cast::<Cell<T>>().as_ref().stage }
}

/// Puts `stage` in the task's slot and returns what was there.
///
/// The new value is written before the old one is dropped, so a destructor
/// that panics cannot leave a dropped value behind for the next caller to drop
/// a second time.
///
/// # Safety
///
/// The caller must hold the poll lock, or be the `JoinHandle` of a task that
/// has completed. `T` must be the task's future type.
unsafe fn replace_stage<T: Future>(ptr: NonNull<Header>, stage: Stage<T>) -> Stage<T> {
    // Safety: the caller guarantees exclusive access to the slot.
    unsafe { mem::replace(&mut *stage_of::<T>(ptr).get(), stage) }
}

/// Drops whatever the task's slot holds, leaving it empty.
///
/// # Safety
///
/// See [`replace_stage`].
unsafe fn drop_stage<T: Future>(ptr: NonNull<Header>) {
    // Safety: the caller guarantees exclusive access to the slot.
    drop(unsafe { replace_stage::<T>(ptr, Stage::Consumed) });
}

fn id_of(ptr: NonNull<Header>) -> Id {
    // Safety: the caller holds a reference, so the task is live.
    unsafe { ptr.as_ref() }.id
}

/// Polls the task, or kills it if it has been cancelled, consuming the
/// reference that granted the right to poll.
///
/// # Safety
///
/// `ptr` must point at a live task whose future type is `T`, and the caller
/// must own a reference that carries the right to poll it.
unsafe fn poll<T: Future>(ptr: NonNull<Header>) {
    // Safety: the caller holds a reference to the task.
    let raw = unsafe { RawTask::from_raw(ptr) };

    // Everything below can reach user code: the future's `poll`, and its
    // destructor on the cancellation and completion paths.
    let _guard = TaskIdGuard::enter(raw.header().id);

    match raw.state().transition_to_running() {
        TransitionToRunning::Polled => {
            // Safety: this reference to the task outlives the borrow.
            let waker = unsafe { waker_ref(ptr) };
            let cx = Context::from_waker(&waker);

            // Safety: the poll lock is held and the future type matches.
            if unsafe { poll_future::<T>(ptr, cx) }.is_ready() {
                unsafe { complete::<T>(ptr) };
                return;
            }

            match raw.state().transition_to_idle() {
                TransitionToIdle::Idle => raw.drop_reference(),
                TransitionToIdle::Notified => raw.schedule(),
                // Safety: the poll lock is held and the future type matches.
                TransitionToIdle::Cancelled => unsafe {
                    cancel::<T>(ptr);
                    complete::<T>(ptr);
                },
            }
        }
        // Safety: the poll lock is held and the future type matches.
        TransitionToRunning::Cancelled => unsafe {
            cancel::<T>(ptr);
            complete::<T>(ptr);
        },
        TransitionToRunning::Dead => raw.drop_reference(),
    }
}

/// Polls the future and, if it finished, stores its result. Returns whether the
/// task is ready to be completed.
///
/// # Safety
///
/// The poll lock must be held, and `T` must be the task's future type.
unsafe fn poll_future<T: Future>(ptr: NonNull<Header>, mut cx: Context<'_>) -> Poll<()> {
    let polled = panic::catch_unwind(AssertUnwindSafe(|| {
        // Safety: the poll lock gives exclusive access to the slot, and the
        // task is heap allocated and never moved.
        let output = unsafe {
            let Stage::Running(future) = &mut *stage_of::<T>(ptr).get() else {
                #[cfg(debug_assertions)]
                unreachable!("polled a task that holds no future");
                #[cfg(not(debug_assertions))]
                hint::unreachable_unchecked();
            };

            Pin::new_unchecked(future).poll(&mut cx)
        };

        if output.is_ready() {
            // The future has to go before its result can be stored, since the
            // two share a slot. The result is on the stack until then.
            unsafe { drop_stage::<T>(ptr) };
        }

        output
    }));

    let output = match polled {
        Ok(Poll::Pending) => return Poll::Pending,
        Ok(Poll::Ready(output)) => Ok(output),
        Err(panic) => {
            // The future panicked part way through, so it is still in the slot
            // and has to come out before its result goes in. Its destructor may
            // panic as well, but there is already a panic to report, so that
            // one is dropped.
            // Safety: the poll lock is still held.
            let _ = panic::catch_unwind(AssertUnwindSafe(|| unsafe { drop_stage::<T>(ptr) }));

            Err(JoinError::panic(id_of(ptr), panic))
        }
    };

    // Safety: the slot was emptied above, on either path that reaches here.
    unsafe { replace_stage::<T>(ptr, Stage::Finished(output)) };

    Poll::Ready(())
}

/// Kills a task that has not finished: drops its future and stores a
/// `JoinError` in its place.
///
/// # Safety
///
/// The poll lock must be held, and `T` must be the task's future type.
unsafe fn cancel<T: Future>(ptr: NonNull<Header>) {
    // Safety: the poll lock gives exclusive access to the slot.
    let dropped = panic::catch_unwind(AssertUnwindSafe(|| unsafe { drop_stage::<T>(ptr) }));

    let error = match dropped {
        Ok(()) => JoinError::cancelled(id_of(ptr)),
        Err(panic) => JoinError::panic(id_of(ptr), panic),
    };

    // Safety: the slot was emptied just above.
    unsafe { replace_stage::<T>(ptr, Stage::Finished(Err(error))) };
}

/// Finishes a task whose result is already stored, releasing the reference the
/// poll consumed along with the registry's.
///
/// # Safety
///
/// The poll lock must be held, and `T` must be the task's future type.
unsafe fn complete<T: Future>(ptr: NonNull<Header>) {
    // Safety: the caller holds a reference to the task.
    let raw = unsafe { RawTask::from_raw(ptr) };
    let snapshot = raw.state().transition_to_complete();

    // Waking the joiner and dropping an unwanted result both reach user code.
    let _ = panic::catch_unwind(AssertUnwindSafe(|| {
        if snapshot.is_join_interested() {
            // The `JoinHandle` takes the result from here.
            raw.header().wake_join();
        } else {
            // Nobody will read the result, so drop it now rather than leave it
            // to whichever reference happens to outlive the others.
            // Safety: the task is complete and has no `JoinHandle`.
            unsafe { drop_stage::<T>(ptr) };
        }
    }));

    if raw.state().transition_to_terminal(release(ptr)) {
        raw.dealloc();
    }
}

/// Takes the task out of the runtime's registry, returning how many references
/// completing it should release: the one the poll consumed, plus the registry's
/// if the task was still in there.
fn release(ptr: NonNull<Header>) -> usize {
    // Safety: the caller holds a reference, so the task is live.
    if !unsafe { ptr.as_ref() }.state.load().is_owned() {
        return 1;
    }

    context::with_handle(|handle| match handle.release(ptr) {
        // Released together with the poll's reference, so that the task cannot
        // be freed half way through completing it.
        Some(task) => {
            mem::forget(task);
            2
        }
        None => 1,
    })
}

/// Moves the task's result into `dst`. The slot is left empty afterwards, so a
/// second call leaves `dst` alone.
///
/// # Safety
///
/// `dst` must point at a `Poll<Result<T::Output>>`, and `ptr` at a complete
/// task whose future type is `T`.
unsafe fn take_output<T: Future>(ptr: NonNull<Header>, dst: *mut ()) {
    // Safety: the caller guarantees the type behind `dst`.
    let dst = unsafe { &mut *dst.cast::<Poll<super::Result<T::Output>>>() };

    // Safety: the task is complete, so the `JoinHandle` owns the slot.
    if let Stage::Finished(output) = unsafe { replace_stage::<T>(ptr, Stage::Consumed) } {
        *dst = Poll::Ready(output);
    }
}

/// # Safety
///
/// `ptr` must point at a task whose last reference has just been dropped and
/// whose future type is `T`.
unsafe fn dealloc<T: Future>(ptr: NonNull<Header>) {
    // Safety: nothing else can reach the allocation any more.
    drop(unsafe { Box::from_raw(ptr.cast::<Cell<T>>().as_ptr()) });
}

#[test]
fn header_fits_in_a_cache_line() {
    assert!(size_of::<Header>() <= 8 * size_of::<*const ()>());
}

#[test]
fn vtable_is_three_words() {
    assert_eq!(size_of::<Vtable>(), 3 * size_of::<*const ()>());
}
