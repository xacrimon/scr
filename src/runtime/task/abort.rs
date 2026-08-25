use std::fmt;
use std::panic::{RefUnwindSafe, UnwindSafe};

use crate::runtime::task::{Id, RawTask};

pub struct AbortHandle {
    raw: RawTask,
}

impl UnwindSafe for AbortHandle {}
impl RefUnwindSafe for AbortHandle {}

impl AbortHandle {
    pub(super) fn new(raw: RawTask) -> AbortHandle {
        AbortHandle { raw }
    }

    pub fn abort(&self) {
        self.raw.remote_abort();
    }

    pub fn is_finished(&self) -> bool {
        self.raw.state().load().is_complete()
    }

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
