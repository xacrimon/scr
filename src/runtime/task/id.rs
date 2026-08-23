use crate::runtime::context;

use std::cell::Cell;
use std::fmt;
use std::num::NonZeroU32;

/// An opaque ID that uniquely identifies a task relative to all other currently
/// running tasks.
///
/// A task's ID may be re-used for another task only once *both* of the
/// following happen:
/// 1. The task itself exits.
/// 2. There is no active [`JoinHandle`] associated with this task.
///
/// [`JoinHandle`]: crate::task::JoinHandle
#[derive(Clone, Copy, Debug, Hash, Eq, PartialEq, PartialOrd, Ord)]
pub struct Id {
    v: NonZeroU32,
}

/// Returns the [`Id`] of the currently running task.
///
/// # Panics
///
/// This function panics if called from outside a task. Note that calls to
/// `block_on` do not have task IDs, so the method will panic if called from
/// within a call to `block_on`. For a version of this function that doesn't
/// panic, see [`try_id`].
#[track_caller]
pub fn id() -> Id {
    context::current_task_id().expect("Can't get a task id when not inside a task")
}

/// Returns the [`Id`] of the currently running task, or `None` if called
/// outside of a task.
#[track_caller]
pub fn try_id() -> Option<Id> {
    context::current_task_id()
}

impl Id {
    /// Returns the next task ID.
    ///
    /// The counter is thread local: every runtime lives on exactly one thread,
    /// so a plain `Cell` is enough and no atomic RMW is needed on the spawn
    /// path. IDs are therefore only unique within a thread.
    pub(crate) fn next() -> Self {
        thread_local! {
            static NEXT_ID: Cell<u32> = const { Cell::new(1) };
        }

        NEXT_ID.with(|next| {
            loop {
                let id = next.get();
                next.set(id.wrapping_add(1));

                if let Some(v) = NonZeroU32::new(id) {
                    return Id { v };
                }
            }
        })
    }
}

impl fmt::Display for Id {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.v.fmt(f)
    }
}
