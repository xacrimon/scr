#![feature(local_waker)]
#![feature(macro_metavar_expr_concat)]

mod io_uring;
mod runtime;
mod util;

pub mod buf;
pub mod io;
pub mod net;
pub mod task;
pub mod time;
pub mod macros;

pub use crate::runtime::Runtime;
pub use crate::task::spawn;

#[doc(hidden)]
pub use scr_macros::{select_priv_declare_output_enum, select_priv_clean_pattern};