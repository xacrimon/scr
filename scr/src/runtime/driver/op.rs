//! Futures over submitted operations.
//!
//! An [`Op`] owns whatever the kernel was given a pointer to — a buffer, an
//! address structure — for as long as the operation is running. Dropping it
//! does not stop the operation, so it cannot simply free that memory: instead it
//! hands the payload to the ledger, which holds it until the completion arrives.
//! That is why [`Completable::cleanup`] exists and why it is not optional.

use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};

use super::Driver;
use super::ledger::{Discard, OnComplete, OpKey};
use crate::io_uring::{op as uring_op, sys};

// ---------------------------------------------------------------------------
// A single operation
// ---------------------------------------------------------------------------

/// What an operation does with its result, and with itself if nobody wants one.
///
/// `Unpin` because the payload is moved out of the future to be delivered or
/// boxed into the ledger — which is fine for the buffers this ever holds, since
/// [`IoBuf`](crate::buf::IoBuf) demands an address that survives a move anyway.
pub(crate) trait Completable: Sized + Unpin + 'static {
    type Output;

    /// Turn a completion into the operation's result.
    fn complete(self, driver: &Driver, res: i32, flags: sys::CqeFlags) -> Self::Output;

    /// Run instead of [`complete`](Completable::complete) when the future was
    /// dropped before the completion arrived.
    ///
    /// Dropping `self` releases the buffers, which is usually the whole job. It
    /// is not always: an operation that *produces* something — a direct
    /// descriptor from an accept, a socket from a chain — may have succeeded
    /// after nobody was left to take it, and unless this closes what it made,
    /// the resource is leaked with no handle left to reach it by.
    fn cleanup(self, driver: &Driver, res: i32, flags: sys::CqeFlags);
}

/// A submitted operation, and the memory the kernel is using for it.
pub(crate) struct Op<'a, D: Completable> {
    driver: &'a Driver,
    key: OpKey,
    /// Taken when the result is delivered, so that [`Drop`] can tell a finished
    /// operation from an abandoned one.
    data: Option<D>,
}

impl<'a, D: Completable> Op<'a, D> {
    pub(crate) fn submit(
        driver: &'a Driver,
        prep: impl FnOnce(uring_op::Slot<'_>),
        data: D,
    ) -> Op<'a, D> {
        let key = driver.submit(prep);

        Op {
            driver,
            key,
            data: Some(data),
        }
    }
}

impl<D: Completable> Future for Op<'_, D> {
    type Output = D::Output;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<D::Output> {
        // Nothing here is self-referential; the pin is only what `Future` asks
        // for.
        let this = self.get_mut();

        // Bound rather than matched on directly, so that the ledger is not
        // borrowed while `complete` runs — it is free to submit more work.
        let done = this.driver.poll_op(this.key, cx.local_waker());

        match done {
            Poll::Pending => Poll::Pending,
            Poll::Ready((res, flags)) => {
                let data = this
                    .data
                    .take()
                    .expect("an operation was polled after it completed");
                Poll::Ready(data.complete(this.driver, res, flags))
            }
        }
    }
}

impl<D: Completable> Drop for Op<'_, D> {
    fn drop(&mut self) {
        let Some(data) = self.data.take() else {
            // The result was delivered; the entry is already gone.
            return;
        };
        let driver = self.driver;

        if let Some((res, flags)) = driver.take_completed(self.key) {
            // It finished while nobody was looking, so there is nothing for the
            // kernel to give back and no need to allocate an action.
            data.cleanup(driver, res, flags);
            return;
        }

        driver.detach(self.key, Box::new(Abandoned(data)));
        driver.cancel(self.key);
    }
}

/// Holds a dropped operation's payload until the kernel is finished with it,
/// then runs the operation's own cleanup.
struct Abandoned<D>(D);

impl<D: Completable> OnComplete for Abandoned<D> {
    fn on_complete(self: Box<Self>, driver: &Driver, res: i32, flags: sys::CqeFlags) {
        self.0.cleanup(driver, res, flags);
    }
}

// ---------------------------------------------------------------------------
// A chain of linked operations
// ---------------------------------------------------------------------------

/// What a linked chain does with the results of all of its members.
pub(crate) trait ChainCompletable<const N: usize>: Sized + Unpin + 'static {
    type Output;

    /// Turn the members' results, in submission order, into one result.
    ///
    /// Once a member fails the rest are cancelled, so the array will hold that
    /// member's error followed by `-ECANCELED`.
    fn complete(self, driver: &Driver, res: [i32; N]) -> Self::Output;

    /// Run instead of [`complete`](ChainCompletable::complete) when the future
    /// was dropped first.
    ///
    /// No results are offered, because a chain can be abandoned halfway and
    /// there is no honest way to report what did and did not happen. Cleanup
    /// has to be unconditional: undo whatever the chain *might* have done.
    fn cleanup(self, driver: &Driver);
}

/// A chain of operations that run in order, submitted together.
pub(crate) struct Chain<'a, const N: usize, D: ChainCompletable<N>> {
    driver: &'a Driver,
    keys: [OpKey; N],
    results: [i32; N],
    collected: u8,
    data: Option<D>,
}

impl<'a, const N: usize, D: ChainCompletable<N>> Chain<'a, N, D> {
    /// Submit `N` linked operations as one chain.
    pub(crate) fn submit(
        driver: &'a Driver,
        prep: impl FnOnce([uring_op::Slot<'_>; N]),
        data: D,
    ) -> Chain<'a, N, D> {
        Chain {
            driver,
            keys: driver.submit_chain(prep),
            results: [0; N],
            collected: 0,
            data: Some(data),
        }
    }
}

impl<const N: usize, D: ChainCompletable<N>> Future for Chain<'_, N, D> {
    type Output = D::Output;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<D::Output> {
        let this = self.get_mut();

        // Walk backwards. A linked entry does not start until the one before it
        // has finished, so the last member is the last to complete — if it has,
        // every earlier one has too, and if it has not, registering there is
        // enough to be woken exactly once instead of N times.
        while (this.collected as usize) < N {
            let i = N - 1 - this.collected as usize;
            match this.driver.poll_op(this.keys[i], cx.local_waker()) {
                Poll::Pending => return Poll::Pending,
                Poll::Ready((res, _)) => {
                    this.results[i] = res;
                    this.collected += 1;
                }
            }
        }

        let results = this.results;
        let data = this
            .data
            .take()
            .expect("a chain was polled after it completed");

        Poll::Ready(data.complete(this.driver, results))
    }
}

impl<const N: usize, D: ChainCompletable<N>> Drop for Chain<'_, N, D> {
    fn drop(&mut self) {
        let Some(data) = self.data.take() else {
            return;
        };
        let driver = self.driver;

        // Again backwards, so the first still-running member found is the
        // highest-numbered one — the one that lands last, and therefore the only
        // one that can safely carry the cleanup.
        let mut carrier: Option<usize> = None;

        for i in (0..N).rev() {
            let in_hand = i >= N - self.collected as usize;
            if in_hand || driver.take_completed(self.keys[i]).is_some() {
                continue;
            }
            if carrier.is_none() {
                carrier = Some(i);
            } else {
                driver.detach(self.keys[i], Box::new(Discard));
            }
            driver.cancel(self.keys[i]);
        }

        match carrier {
            Some(i) => driver.detach(self.keys[i], Box::new(AbandonedChain::<N, D>(data))),
            // The whole chain had already finished.
            None => data.cleanup(driver),
        }
    }
}

/// As [`Abandoned`], for a chain: held by whichever member completes last.
struct AbandonedChain<const N: usize, D: ChainCompletable<N>>(D);

impl<const N: usize, D: ChainCompletable<N>> OnComplete for AbandonedChain<N, D> {
    fn on_complete(self: Box<Self>, driver: &Driver, _res: i32, _flags: sys::CqeFlags) {
        self.0.cleanup(driver);
    }
}
