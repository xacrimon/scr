mod abort;
mod error;
mod id;
mod join;
mod owned;
mod raw;
mod state;
mod waker;

pub use self::abort::AbortHandle;
pub use self::error::JoinError;
pub use self::id::{Id, id, try_id};
pub use self::join::JoinHandle;

pub(crate) use self::owned::OwnedTasks;
pub(crate) use self::raw::{Header, RawTask};

use std::fmt;
use std::mem;
use std::panic::Location;
use std::ptr::NonNull;

pub(crate) type Result = std::result::Result<(), JoinError>;

#[repr(transparent)]
pub(crate) struct Task {
    raw: RawTask,
}

#[repr(transparent)]
pub(crate) struct Runnable(Task);

fn new_task<T>(
    future: T,
    id: Id,
    spawned_at: &'static Location<'static>,
) -> (Task, Runnable, JoinHandle)
where
    T: Future<Output = ()> + 'static,
{
    let raw = RawTask::new(future, id, spawned_at);
    (Task { raw }, Runnable(Task { raw }), JoinHandle::new(raw))
}

impl Task {
    unsafe fn from_raw(ptr: NonNull<Header>) -> Task {
        Task {
            raw: unsafe { RawTask::from_raw(ptr) },
        }
    }

    fn header(&self) -> &Header {
        self.raw.header()
    }

    fn header_ptr(&self) -> NonNull<Header> {
        self.raw.header_ptr()
    }

    pub(crate) fn shutdown(self) {
        let raw = self.raw;
        mem::forget(self);

        raw.shutdown();
    }
}

impl Runnable {
    pub(crate) fn run(self) {
        let raw = self.0.raw;
        mem::forget(self);

        raw.poll();
    }

    pub(crate) fn header_ptr(&self) -> NonNull<Header> {
        self.0.header_ptr()
    }
}

impl Drop for Task {
    fn drop(&mut self) {
        self.raw.drop_reference();
    }
}

impl fmt::Debug for Task {
    fn fmt(&self, fmt: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.header().fmt(fmt)
    }
}

impl fmt::Debug for Runnable {
    fn fmt(&self, fmt: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(fmt)
    }
}
