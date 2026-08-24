//! Not a real example; exists only so `cargo asm` has a concrete future type
//! to monomorphize the task machinery against for codegen inspection.

use std::future::Future;
use std::hint::black_box;
use std::pin::Pin;
use std::task::{Context, Poll};

use scr::Runtime;

struct DummyFuture {
    polled: bool,
}

impl Future for DummyFuture {
    type Output = ();

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        if self.polled {
            Poll::Ready(())
        } else {
            self.polled = true;
            cx.waker().wake_by_ref();
            Poll::Pending
        }
    }
}

fn main() {
    let rt = Runtime::new();
    let out = rt.block_on(async {
        let h = scr::task::spawn(DummyFuture { polled: false });
        h.await
    });
    black_box(out).ok();
}
