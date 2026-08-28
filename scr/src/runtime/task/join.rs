use std::fmt;
use std::panic::{Location, RefUnwindSafe, UnwindSafe};
use std::pin::Pin;
use std::task::{Context, Poll};

use crate::runtime::task::{AbortHandle, Id, JoinError, RawTask};

pub struct JoinHandle {
    raw: RawTask,
}

impl UnwindSafe for JoinHandle {}
impl RefUnwindSafe for JoinHandle {}
impl Unpin for JoinHandle {}

impl JoinHandle {
    pub(super) fn new(raw: RawTask) -> JoinHandle {
        JoinHandle { raw }
    }

    pub fn abort(&self) {
        self.raw.remote_abort();
    }

    #[must_use = "abort handles do nothing unless `.abort` is called"]
    pub fn abort_handle(&self) -> AbortHandle {
        self.raw.ref_inc();
        AbortHandle::new(self.raw)
    }

    pub fn is_finished(&self) -> bool {
        self.raw.state().load().is_complete()
    }

    pub fn id(&self) -> Id {
        self.raw.header().id
    }

    pub fn spawned_at(&self) -> &'static Location<'static> {
        self.raw.header().spawned_at
    }
}

impl Future for JoinHandle {
    type Output = super::Result;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let header = self.raw.header();
        let snapshot = header.state.load();
        debug_assert!(snapshot.is_join_interested());

        if !snapshot.is_complete() {
            unsafe {
                if !header.will_wake(cx.local_waker()) {
                    header.set_waker(Some(cx.local_waker().clone()));
                }
            }

            return Poll::Pending;
        }

        assert!(
            header.state.take_output(),
            "`JoinHandle` polled after its result was taken"
        );

        Poll::Ready(match header.take_panic() {
            Some(panic) => Err(JoinError::panic(header.id, panic)),
            None if snapshot.is_cancelled() => Err(JoinError::cancelled(header.id)),
            None => Ok(()),
        })
    }
}

impl Drop for JoinHandle {
    fn drop(&mut self) {
        self.raw.state().unset_join_interested();

        unsafe { self.raw.header().set_waker(None) };

        self.raw.drop_reference();
    }
}

impl fmt::Debug for JoinHandle {
    fn fmt(&self, fmt: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt.debug_struct("JoinHandle")
            .field("id", &self.id())
            .finish()
    }
}
