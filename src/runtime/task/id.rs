use std::fmt;
use std::num::NonZeroU32;

use crate::runtime::context;

/// An opaque id that identifies a task among those currently live on its
/// runtime.
///
/// An id names the slot the task holds in its runtime's registry, so it is
/// handed to a new task once the task that held it completes. Two tasks that
/// are alive at the same time never share one.
#[derive(Clone, Copy, Debug, Hash, Eq, PartialEq, PartialOrd, Ord)]
pub struct Id {
    v: NonZeroU32,
}

impl Id {
    /// Returns the id naming registry slot `slot`.
    pub(crate) fn from_slot(slot: usize) -> Id {
        // Ids are one based so that `Option<Id>` is the size of an `Id`. The
        // cast is the only part that can fail, and only with more live tasks
        // than a `u32` can count.
        let v = u32::try_from(slot)
            .ok()
            .and_then(|slot| NonZeroU32::new(slot.wrapping_add(1)))
            .expect("too many live tasks");

        Id { v }
    }

    /// Returns the registry slot this id names.
    pub(crate) fn slot(self) -> usize {
        self.v.get() as usize - 1
    }
}

impl fmt::Display for Id {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.v.fmt(f)
    }
}

/// Returns the [`Id`] of the currently running task.
///
/// # Panics
///
/// Panics if called from outside a task. The future passed to `block_on` is not
/// a task, so this panics there too; use [`try_id`] for a version that does
/// not.
#[track_caller]
pub fn id() -> Id {
    try_id().expect("called `task::id` from outside of a task")
}

/// Returns the [`Id`] of the currently running task, or `None` if called from
/// outside a task.
pub fn try_id() -> Option<Id> {
    context::current_task_id()
}
