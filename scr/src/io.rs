//! Traits for endpoints that can be read from and written to.
//!
//! These take the buffer by value, because the kernel keeps hold of it past the
//! end of any borrow we could hand it — see [`crate::buf`]. They take `&self`
//! rather than `&mut self`, which is the one place this deviates from the shape
//! `std::io` and the other completion-based runtimes use.
//!
//! # Why `&self`
//!
//! A socket has no shared cursor: a read and a write on one are independent
//! kernel paths, and running them at once from two tasks is exactly what a
//! server does. Under `&mut self` that cannot be said directly, so the other
//! runtimes bolt on a split — and their splits are an unsafe marker trait plus
//! an `Rc<UnsafeCell<T>>` around the socket, which is to say they hand the
//! unsafety to the caller to get back what `&self` gives away for free. Here
//! `Rc<TcpStream>` shared between two tasks just works.
//!
//! What `&mut self` would have bought is a compile error on two concurrent
//! *reads* of one stream, which is a genuine bug. [`crate::net::TcpStream`]
//! catches that with a debug assertion instead — which keeps catching it after a
//! split, where `&mut self` no longer can.

use std::io;

use crate::buf::{BufResult, IoBuf, IoBufMut};

/// An endpoint bytes can be read from.
pub trait AsyncRead {
    /// Read into `buf`, returning how many bytes arrived along with the buffer.
    ///
    /// Writes at the start of the buffer and does not grow it: it fills up to
    /// [`IoBufMut::bytes_total`] and reports how far it got. To append to what
    /// the buffer already holds, pass `buf.slice(n..)`.
    ///
    /// A return of `Ok(0)` means the peer has closed its end.
    fn read<B: IoBufMut>(&self, buf: B) -> impl Future<Output = BufResult<usize, B>>;
}

/// An endpoint bytes can be written to.
pub trait AsyncWrite {
    /// Write the initialised bytes of `buf`, returning how many were accepted
    /// along with the buffer.
    ///
    /// A short write is not an error. To write everything, resubmit the
    /// remainder as `buf.slice(n..)`.
    fn write<B: IoBuf>(&self, buf: B) -> impl Future<Output = BufResult<usize, B>>;

    /// Push out anything buffered on this side.
    ///
    /// An endpoint that submits each write straight to the kernel — every one
    /// in this crate — has nothing to push, and this is a no-op for it. It is
    /// here so that a buffered wrapper can be swapped in without the code
    /// writing through it changing.
    fn flush(&self) -> impl Future<Output = io::Result<()>>;

    /// Close the writing half, so the peer reads end-of-file.
    fn shutdown(&self) -> impl Future<Output = io::Result<()>>;
}
