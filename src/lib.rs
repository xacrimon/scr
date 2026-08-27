#![feature(local_waker)]

mod io_uring;
mod runtime;
mod util;

pub mod task;

pub use crate::runtime::Runtime;
pub use crate::task::spawn;
