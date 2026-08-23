//! A single threaded async runtime.

mod macros;
mod runtime;
mod util;

pub mod task;

pub use crate::runtime::Runtime;
pub use crate::task::spawn;
