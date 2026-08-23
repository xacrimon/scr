use std::fmt;
use std::panic::{RefUnwindSafe, UnwindSafe};

use crate::runtime::task::{Id, RawTask};

/// An owned permission to abort a task, without the permission to await it.
///
/// Dropping an `AbortHandle` gives up that permission; it does not abort the
/// task.
pub struct AbortHandle {
    raw: RawTask,
}

impl UnwindSafe for AbortHandle {}
impl RefUnwindSafe for AbortHandle {}

impl AbortHandle {
    pub(super) fn new(raw: RawTask) -> AbortHandle {
        AbortHandle { raw }
    }

    /// Aborts the task.
    ///
    /// Awaiting an aborted task may still yield its result, if it had already
    /// finished when the abort landed; otherwise it fails with a [cancelled]
    /// `JoinError`. Aborting a task that is already aborted does nothing.
    ///
    /// [cancelled]: method@super::JoinError::is_cancelled
    pub fn abort(&self) {
        self.raw.remote_abort();
    }

    /// Returns whether the task has finished.
    pub fn is_finished(&self) -> bool {
        self.raw.state().load().is_complete()
    }

    /// Returns the [`Id`] of the task.
    pub fn id(&self) -> Id {
        self.raw.header().id
    }
}

impl Clone for AbortHandle {
    fn clone(&self) -> AbortHandle {
        self.raw.ref_inc();
        AbortHandle::new(self.raw)
    }
}

impl Drop for AbortHandle {
    fn drop(&mut self) {
        self.raw.drop_reference();
    }
}

impl fmt::Debug for AbortHandle {
    fn fmt(&self, fmt: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt.debug_struct("AbortHandle")
            .field("id", &self.id())
            .finish()
    }
}
