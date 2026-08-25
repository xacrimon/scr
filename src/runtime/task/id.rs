use std::fmt;
use std::num::NonZeroU32;

use crate::runtime::context;

#[derive(Clone, Copy, Debug, Hash, Eq, PartialEq, PartialOrd, Ord)]
pub struct Id {
    v: NonZeroU32,
}

impl Id {
    pub(crate) fn from_slot(slot: usize) -> Id {
        let v = u32::try_from(slot)
            .ok()
            .and_then(|slot| NonZeroU32::new(slot.wrapping_add(1)))
            .expect("too many live tasks");

        Id { v }
    }

    pub(crate) fn slot(self) -> usize {
        self.v.get() as usize - 1
    }
}

impl fmt::Display for Id {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.v.fmt(f)
    }
}

pub fn id() -> Id {
    try_id().expect("called `task::id` from outside of a task")
}

pub fn try_id() -> Option<Id> {
    context::current_task_id()
}
