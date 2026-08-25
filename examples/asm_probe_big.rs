#![feature(local_waker)]

//! Not a real example; exists only so codegen can be inspected against a future
//! whose *type* is large, to see how much of the task machinery scales with it.

use std::future::Future;
use std::hint::black_box;
use std::pin::Pin;
use std::task::{Context, Poll};

use scr::Runtime;

/// A future that is 2 KiB on its own, with a trivially droppable payload.
struct BigFuture {
    polled: bool,
    data: [u64; 255],
}

impl Future for BigFuture {
    type Output = ();

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        if self.polled {
            Poll::Ready(())
        } else {
            self.polled = true;
            self.data[0] = self.data[254].wrapping_add(1);
            cx.local_waker().wake_by_ref();
            Poll::Pending
        }
    }
}

/// The same size, but holding something with a real destructor so the drop
/// glue cannot be elided.
struct BigDropFuture {
    polled: bool,
    data: [Option<Box<u64>>; 255],
}

impl Future for BigDropFuture {
    type Output = ();

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        if self.polled {
            Poll::Ready(())
        } else {
            self.polled = true;
            self.data[0] = Some(Box::new(1));
            cx.local_waker().wake_by_ref();
            Poll::Pending
        }
    }
}

fn main() {
    let rt = Runtime::new();

    let a = rt.block_on(async {
        scr::task::spawn(BigFuture {
            polled: false,
            data: [0; 255],
        })
        .await
    });

    let b = rt.block_on(async {
        scr::task::spawn(BigDropFuture {
            polled: false,
            data: [const { None }; 255],
        })
        .await
    });

    black_box((a, b)).0.ok();
}
