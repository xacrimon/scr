//! Networking over the ring's registered file table.

mod addr;
mod tcp;

pub use self::tcp::{TcpListener, TcpStream};
