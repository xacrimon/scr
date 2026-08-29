#![feature(local_waker)]

mod io_uring;
mod runtime;
mod util;

pub mod buf;
pub mod io;
pub mod macros;
pub mod net;
pub mod task;
pub mod time;

pub use crate::runtime::Runtime;
pub use crate::task::spawn;

#[doc(hidden)]
pub use scr_macros::{select_priv_clean_pattern, select_priv_declare_output_enum};
