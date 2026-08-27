//! Typed submission queue entries, one per operation.
//!
//! Each op is a newtype over a [`sys::Sqe`] with builder methods named after the
//! operation's own parameters rather than the union slots they land in — so
//! `Read::offset` writes `addr2` and `Timeout::count` writes it too, without the
//! caller needing to know that. Every setter takes `self` and returns it, and
//! every setter has a matching `get_` reader.
//!
//! ```ignore
//! let sqe = op::Read::new()
//!     .fd(fd)
//!     .buf(buf)
//!     .offset(0)
//!     .user_data(42)
//!     .into_sqe();
//! ```
//!
//! # Field type conventions
//!
//! | shape | Rust type |
//! |---|---|
//! | required pointer | `NonNull<T>` |
//! | optional pointer | `Option<NonNull<T>>` |
//! | pointers that must be set or unset together | `Option<(NonNull<A>, NonNull<B>)>` |
//! | file descriptor | `i32` |
//! | direct descriptor slot | `u32`, 0-based; the wire encoding is 1-based |
//! | flag word | the matching `sys` bitflags type |
//!
//! The tuple case exists because several operations take a buffer alongside a
//! pointer to its length, and the kernel dereferences the second only if the
//! first is non-null. Binding them into one value makes "both or neither" the
//! only representable state. The same applies to values packed into the two
//! halves of one word, such as [`UringCmd::sockopt`].
//!
//! Where two fields deliberately share one SQE word, the docs say so. `buf`
//! writes an address and a length, and `nbytes` then narrows the length alone,
//! so it must come second; [`MsgRing::slot`] and [`MsgRing::cqe_flags`] are
//! alternatives for the same word and only one may be set.
//!
//! Construction always starts from [`sys::Sqe::ZEROED`] and writes the whole
//! 64 bytes. That matters: a submission slot is reused once the ring wraps, so
//! anything left unwritten would inherit the previous request's bytes. liburing
//! solves the same problem by clearing eight fields in
//! `io_uring_initialize_sqe`; starting from zero is simpler and total.

#![allow(dead_code)]

use std::ffi::c_void;
use std::ptr::NonNull;

use super::sys;

/// Define an operation newtype over [`sys::Sqe`].
///
/// The header names the struct — which must match a [`sys::Opcode`] variant —
/// followed by a brace list of SQE fields to preset in `new`, for opcodes the
/// kernel requires a fixed value from.
///
/// Fields come in two forms. A direct field names the SQE field it maps to and
/// derives both directions:
///
/// ```ignore
/// offset: u64 => addr2;
/// ```
///
/// A mapped field gives one expression per SQE field it writes, then `<=` and
/// an expression taking `&sys::Sqe` for the reverse:
///
/// ```ignore
/// buf: NonNull<[u8]>
///     => addr = |v| v.cast::<u8>().as_ptr() as u64,
///        len = |v| v.len() as u32;
///     <= |s| /* rebuild it */;
/// ```
///
/// Field types must be `Copy`: a mapped field applies `value` once per SQE
/// field it writes.
macro_rules! opcode {
    (
        $(#[$attr:meta])*
        $name:ident { $($init:ident = $iv:expr),* $(,)? };
        $($fields:tt)*
    ) => {
        $(#[$attr])*
        #[derive(Debug, Clone, Copy)]
        pub struct $name(sys::Sqe);

        impl $name {
            /// A zeroed SQE for this operation.
            pub fn new() -> $name {
                $name(sys::Sqe {
                    opcode: sys::Opcode::$name,
                    $($init: $iv,)*
                    ..sys::Sqe::ZEROED
                })
            }

            /// The completion token echoed back in `cqe.user_data`.
            pub fn user_data(mut self, value: u64) -> $name {
                self.0.user_data = value;
                self
            }

            pub fn get_user_data(&self) -> u64 {
                self.0.user_data
            }

            /// Submission flags — linking, drain, fixed file, buffer select.
            ///
            /// Named `sqe_flags` because several operations have a `flags` of
            /// their own that lands in a different field.
            pub fn sqe_flags(mut self, value: sys::SqeFlags) -> $name {
                self.0.flags = value;
                self
            }

            pub fn get_sqe_flags(&self) -> sys::SqeFlags {
                self.0.flags
            }

            /// Registered credentials to issue this operation under.
            pub fn personality(mut self, value: u16) -> $name {
                self.0.personality = value;
                self
            }

            pub fn get_personality(&self) -> u16 {
                self.0.personality
            }

            /// The encoded submission queue entry.
            pub fn into_sqe(self) -> sys::Sqe {
                self.0
            }

            pub fn as_sqe(&self) -> &sys::Sqe {
                &self.0
            }

            opcode!(@fields $($fields)*);
        }

        impl From<$name> for sys::Sqe {
            fn from(op: $name) -> sys::Sqe {
                op.0
            }
        }
    };

    (@fields) => {};

    // Mapped field: explicit expressions in both directions.
    (@fields
        $(#[$m:meta])*
        $n:ident : $t:ty => $($bf:ident = $set:expr),+ ; <= $get:expr ;
        $($rest:tt)*
    ) => {
        $(#[$m])*
        pub fn $n(mut self, value: $t) -> Self {
            $( self.0.$bf = ($set)(value); )+
            self
        }

        $(#[$m])*
        pub fn ${concat(get_, $n)}(&self) -> $t {
            ($get)(&self.0)
        }

        opcode!(@fields $($rest)*);
    };

    // Direct field: both directions derived.
    (@fields
        $(#[$m:meta])*
        $n:ident : $t:ty => $bf:ident ;
        $($rest:tt)*
    ) => {
        opcode!(@fields
            $(#[$m])*
            $n : $t => $bf = |v: $t| v ; <= |s: &sys::Sqe| s.$bf ;
            $($rest)*
        );
    };
}

/// Rebuild a `NonNull<[u8]>` from an `addr`/`len` pair.
///
/// A zeroed SQE has no buffer, so this reports a dangling pointer of the
/// recorded length rather than pretending the address is valid.
fn buf_from(addr: u64, len: u32) -> NonNull<[u8]> {
    let ptr = NonNull::new(addr as *mut u8).unwrap_or(NonNull::dangling());
    NonNull::slice_from_raw_parts(ptr, len as usize)
}

/// Encode a pointer into an SQE word.
fn ptr_enc<T>(v: NonNull<T>) -> u64 {
    v.as_ptr() as u64
}

/// Rebuild a pointer from an SQE word, reporting a dangling pointer when the
/// field was never set.
fn ptr_dec<T>(raw: u64) -> NonNull<T> {
    NonNull::new(raw as *mut T).unwrap_or(NonNull::dangling())
}

/// Encode an optional pointer, where zero means absent.
fn optptr_enc<T>(v: Option<NonNull<T>>) -> u64 {
    v.map_or(0, |p| p.as_ptr() as u64)
}

fn optptr_dec<T>(raw: u64) -> Option<NonNull<T>> {
    NonNull::new(raw as *mut T)
}

/// Encode a direct descriptor slot, which the kernel takes 1-based so that zero
/// can mean "not a fixed file".
fn slot_encode(slot: u32) -> u32 {
    slot.wrapping_add(1)
}

fn slot_decode(raw: u32) -> u32 {
    raw.wrapping_sub(1)
}

opcode! {
    /// Do nothing. Useful for waking a ring and for padding a wrap.
    Nop {};

    /// Behavioural flags, including injecting a result.
    flags: sys::NopFlags => op_flags = |v: sys::NopFlags| v.bits();
        <= |s: &sys::Sqe| sys::NopFlags::from_bits_retain(s.op_flags);

    /// The result to report when [`sys::NopFlags::INJECT_RESULT`] is set.
    result: i32 => len = |v: i32| v as u32;
        <= |s: &sys::Sqe| s.len as i32;
}

opcode! {
    /// Read from a file descriptor at an offset.
    Read {};

    fd: i32 => fd;

    /// The destination buffer. Writes both the address and the length; use
    /// [`Read::nbytes`] afterwards to read less than the whole buffer.
    buf: NonNull<[u8]>
        => addr = |v: NonNull<[u8]>| v.cast::<u8>().as_ptr() as u64,
           len = |v: NonNull<[u8]>| v.len() as u32;
        <= |s: &sys::Sqe| buf_from(s.addr, s.len);

    /// Bytes to read, if fewer than the buffer holds.
    nbytes: u32 => len;

    /// File offset, or `u64::MAX` to use the file's current position.
    offset: u64 => addr2;

    /// `RWF_*` flags, as taken by `preadv2`.
    rw_flags: u32 => op_flags;
}

opcode! {
    /// Write to a file descriptor at an offset.
    Write {};

    fd: i32 => fd;

    /// The source buffer. Writes both the address and the length; use
    /// [`Write::nbytes`] afterwards to write less than the whole buffer.
    buf: NonNull<[u8]>
        => addr = |v: NonNull<[u8]>| v.cast::<u8>().as_ptr() as u64,
           len = |v: NonNull<[u8]>| v.len() as u32;
        <= |s: &sys::Sqe| buf_from(s.addr, s.len);

    /// Bytes to write, if fewer than the buffer holds.
    nbytes: u32 => len;

    /// File offset, or `u64::MAX` to use the file's current position.
    offset: u64 => addr2;

    /// `RWF_*` flags, as taken by `pwritev2`.
    rw_flags: u32 => op_flags;
}

opcode! {
    /// Close a file descriptor.
    Close {};

    fd: i32 => fd;

    /// Close a direct descriptor slot instead of `fd`, which must then be zero.
    slot: u32 => file_index = |v: u32| slot_encode(v);
        <= |s: &sys::Sqe| slot_decode(s.file_index);
}

opcode! {
    /// Open a file relative to a directory descriptor.
    Openat {};

    /// The directory to resolve against, or `libc::AT_FDCWD`.
    dirfd: i32 => fd;

    /// A null-terminated path.
    path: NonNull<u8> => addr = |v: NonNull<u8>| v.as_ptr() as u64;
        <= |s: &sys::Sqe| NonNull::new(s.addr as *mut u8).unwrap_or(NonNull::dangling());

    /// Creation mode, as the third argument of `openat`.
    mode: u32 => len;

    /// `O_*` open flags.
    flags: u32 => op_flags;

    /// Install into this direct descriptor slot rather than returning an fd.
    /// [`sys::FILE_INDEX_ALLOC`] lets the kernel choose.
    slot: u32 => file_index = |v: u32| slot_encode(v);
        <= |s: &sys::Sqe| slot_decode(s.file_index);
}

opcode! {
    /// Accept a connection on a listening socket.
    Accept {};

    fd: i32 => fd;

    /// Where to write the peer address, or `None` to discard it.
    ///
    /// The second pointer is in-out: set it to the size of the first before
    /// submitting, and the kernel overwrites it with the length it actually
    /// wrote. `accept4` requires both or neither, so they travel together.
    ///
    /// With [`sys::AcceptFlags::MULTISHOT`] every accepted connection writes
    /// through the same pointer, so a connection can be overwritten before the
    /// previous one has been read. Pass `None` unless you know that cannot
    /// happen.
    peer: Option<(NonNull<c_void>, NonNull<u32>)>
        => addr = |v: Option<(NonNull<c_void>, NonNull<u32>)>| {
               v.map_or(0, |(a, _)| a.as_ptr() as u64)
           },
           addr2 = |v: Option<(NonNull<c_void>, NonNull<u32>)>| {
               v.map_or(0, |(_, l)| l.as_ptr() as u64)
           };
        <= |s: &sys::Sqe| {
            NonNull::new(s.addr as *mut c_void).zip(NonNull::new(s.addr2 as *mut u32))
        };

    /// `SOCK_*` flags, as taken by `accept4`.
    flags: u32 => op_flags;

    /// Accept behaviour, including [`sys::AcceptFlags::MULTISHOT`].
    accept_flags: sys::AcceptFlags => ioprio = |v: sys::AcceptFlags| v.bits();
        <= |s: &sys::Sqe| sys::AcceptFlags::from_bits_retain(s.ioprio);

    /// Install into this direct descriptor slot rather than returning an fd.
    slot: u32 => file_index = |v: u32| slot_encode(v);
        <= |s: &sys::Sqe| slot_decode(s.file_index);
}

opcode! {
    /// Receive from a socket.
    Recv {};

    fd: i32 => fd;

    /// The destination buffer. Leave unset when selecting a provided buffer
    /// with [`sys::SqeFlags::BUFFER_SELECT`].
    buf: NonNull<[u8]>
        => addr = |v: NonNull<[u8]>| v.cast::<u8>().as_ptr() as u64,
           len = |v: NonNull<[u8]>| v.len() as u32;
        <= |s: &sys::Sqe| buf_from(s.addr, s.len);

    /// Bytes to receive, if fewer than the buffer holds.
    nbytes: u32 => len;

    /// `MSG_*` flags.
    flags: u32 => op_flags;

    /// Receive behaviour: [`sys::RecvSendFlags::RECV_MULTISHOT`],
    /// [`sys::RecvSendFlags::BUNDLE`], and friends.
    recv_flags: sys::RecvSendFlags => ioprio = |v: sys::RecvSendFlags| v.bits();
        <= |s: &sys::Sqe| sys::RecvSendFlags::from_bits_retain(s.ioprio);

    /// The provided buffer group to select from.
    buf_group: u16 => buf_index;
}

opcode! {
    /// Send on a socket.
    Send {};

    fd: i32 => fd;

    /// The source buffer.
    buf: NonNull<[u8]>
        => addr = |v: NonNull<[u8]>| v.cast::<u8>().as_ptr() as u64,
           len = |v: NonNull<[u8]>| v.len() as u32;
        <= |s: &sys::Sqe| buf_from(s.addr, s.len);

    /// Bytes to send, if fewer than the buffer holds.
    nbytes: u32 => len;

    /// `MSG_*` flags.
    flags: u32 => op_flags;

    /// Send behaviour: [`sys::RecvSendFlags::BUNDLE`],
    /// [`sys::RecvSendFlags::POLL_FIRST`], and friends.
    send_flags: sys::RecvSendFlags => ioprio = |v: sys::RecvSendFlags| v.bits();
        <= |s: &sys::Sqe| sys::RecvSendFlags::from_bits_retain(s.ioprio);

    /// The provided buffer group to select from.
    buf_group: u16 => buf_index;
}

opcode! {
    /// Wait for a duration, or for a number of completions.
    ///
    /// `len` is preset to 1: the kernel reads exactly one timespec, and a zero
    /// there is rejected.
    Timeout { len = 1, fd = -1 };

    /// The timeout value. Must stay alive until the operation completes, unless
    /// [`sys::TimeoutFlags::IMMEDIATE_ARG`] is set.
    ts: NonNull<sys::Timespec>
        => addr = |v: NonNull<sys::Timespec>| v.as_ptr() as u64;
        <= |s: &sys::Sqe| {
            NonNull::new(s.addr as *mut sys::Timespec).unwrap_or(NonNull::dangling())
        };

    /// Complete early once this many completions have been posted. Zero waits
    /// for the full duration.
    count: u32 => addr2 = |v: u32| v as u64;
        <= |s: &sys::Sqe| s.addr2 as u32;

    /// Clock selection and behaviour.
    flags: sys::TimeoutFlags => op_flags = |v: sys::TimeoutFlags| v.bits();
        <= |s: &sys::Sqe| sys::TimeoutFlags::from_bits_retain(s.op_flags);
}

// ---------------------------------------------------------------------------
// Vectored and fixed-buffer I/O
// ---------------------------------------------------------------------------

opcode! {
    /// Vectored read, as `preadv2`.
    Readv {};
    fd: i32 => fd;
    /// The iovec array.
    iovecs: NonNull<libc::iovec> => addr = ptr_enc; <= |s: &sys::Sqe| ptr_dec(s.addr);
    /// Number of entries in `iovecs`.
    nr_vecs: u32 => len;
    offset: u64 => addr2;
    /// `RWF_*` flags.
    rw_flags: u32 => op_flags;
}

opcode! {
    /// Vectored write, as `pwritev2`.
    Writev {};
    fd: i32 => fd;
    iovecs: NonNull<libc::iovec> => addr = ptr_enc; <= |s: &sys::Sqe| ptr_dec(s.addr);
    nr_vecs: u32 => len;
    offset: u64 => addr2;
    rw_flags: u32 => op_flags;
}

opcode! {
    /// Read into a registered buffer.
    ReadFixed {};
    fd: i32 => fd;
    /// A subrange of the registered buffer named by [`ReadFixed::buf_index`].
    buf: NonNull<[u8]>
        => addr = |v: NonNull<[u8]>| v.cast::<u8>().as_ptr() as u64,
           len = |v: NonNull<[u8]>| v.len() as u32;
        <= |s: &sys::Sqe| buf_from(s.addr, s.len);
    nbytes: u32 => len;
    offset: u64 => addr2;
    /// Index into the registered buffer table.
    buf_index: u16 => buf_index;
}

opcode! {
    /// Write from a registered buffer.
    WriteFixed {};
    fd: i32 => fd;
    buf: NonNull<[u8]>
        => addr = |v: NonNull<[u8]>| v.cast::<u8>().as_ptr() as u64,
           len = |v: NonNull<[u8]>| v.len() as u32;
        <= |s: &sys::Sqe| buf_from(s.addr, s.len);
    nbytes: u32 => len;
    offset: u64 => addr2;
    buf_index: u16 => buf_index;
}

opcode! {
    /// Vectored read into registered buffers.
    ReadvFixed {};
    fd: i32 => fd;
    iovecs: NonNull<libc::iovec> => addr = ptr_enc; <= |s: &sys::Sqe| ptr_dec(s.addr);
    nr_vecs: u32 => len;
    offset: u64 => addr2;
    rw_flags: u32 => op_flags;
    buf_index: u16 => buf_index;
}

opcode! {
    /// Vectored write from registered buffers.
    WritevFixed {};
    fd: i32 => fd;
    iovecs: NonNull<libc::iovec> => addr = ptr_enc; <= |s: &sys::Sqe| ptr_dec(s.addr);
    nr_vecs: u32 => len;
    offset: u64 => addr2;
    rw_flags: u32 => op_flags;
    buf_index: u16 => buf_index;
}

opcode! {
    /// Multishot read into provided buffers.
    ///
    /// Requires [`sys::SqeFlags::BUFFER_SELECT`], which `new` presets.
    ReadMultishot { flags = sys::SqeFlags::BUFFER_SELECT };
    fd: i32 => fd;
    nbytes: u32 => len;
    offset: u64 => addr2;
    /// The provided buffer group to draw from.
    buf_group: u16 => buf_index;
}

// ---------------------------------------------------------------------------
// File state
// ---------------------------------------------------------------------------

opcode! {
    /// Flush a file to storage.
    Fsync {};
    fd: i32 => fd;
    /// [`sys::FsyncFlags::DATASYNC`] for `fdatasync` semantics.
    flags: sys::FsyncFlags => op_flags = |v: sys::FsyncFlags| v.bits();
        <= |s: &sys::Sqe| sys::FsyncFlags::from_bits_retain(s.op_flags);
}

opcode! {
    /// Flush a byte range, as `sync_file_range`.
    SyncFileRange {};
    fd: i32 => fd;
    nbytes: u32 => len;
    offset: u64 => addr2;
    /// `SYNC_FILE_RANGE_*` flags.
    flags: u32 => op_flags;
}

opcode! {
    /// Manipulate file space, as `fallocate`.
    Fallocate {};
    fd: i32 => fd;
    /// `FALLOC_FL_*` mode.
    mode: u32 => len;
    offset: u64 => addr2;
    /// Length of the range.
    nbytes: u64 => addr;
}

opcode! {
    /// Truncate a file, as `ftruncate`.
    Ftruncate {};
    fd: i32 => fd;
    /// The new length.
    nbytes: u64 => addr2;
}

opcode! {
    /// Declare an access pattern, as `posix_fadvise`.
    Fadvise {};
    fd: i32 => fd;
    offset: u64 => addr2;
    /// Length of the range. Leave [`Fadvise::nbytes64`] unset when using this.
    nbytes: u32 => len;
    /// 64-bit length, for ranges beyond 4GiB. Leave [`Fadvise::nbytes`] unset.
    nbytes64: u64 => addr;
    /// `POSIX_FADV_*` advice.
    advice: u32 => op_flags;
}

opcode! {
    /// Declare an access pattern for a memory range, as `madvise`.
    Madvise { fd = -1 };
    /// Start of the range.
    addr: NonNull<u8> => addr = ptr_enc; <= |s: &sys::Sqe| ptr_dec(s.addr);
    /// Length of the range. Leave [`Madvise::nbytes64`] unset when using this.
    nbytes: u32 => len;
    /// 64-bit length. Leave [`Madvise::nbytes`] unset.
    nbytes64: u64 => addr2;
    /// `MADV_*` advice.
    advice: u32 => op_flags;
}

opcode! {
    /// Query file metadata, as `statx`.
    Statx {};
    /// The directory to resolve against, or `libc::AT_FDCWD`.
    dirfd: i32 => fd;
    /// A null-terminated path.
    path: NonNull<u8> => addr = ptr_enc; <= |s: &sys::Sqe| ptr_dec(s.addr);
    /// `STATX_*` field mask.
    mask: u32 => len;
    /// Where to write the `struct statx`.
    statxbuf: NonNull<c_void> => addr2 = ptr_enc; <= |s: &sys::Sqe| ptr_dec(s.addr2);
    /// `AT_*` resolution flags.
    flags: u32 => op_flags;
}

// ---------------------------------------------------------------------------
// Namespace operations
// ---------------------------------------------------------------------------

opcode! {
    /// Open a file with the extended `open_how` description.
    ///
    /// `len` is preset to the size of `struct open_how`, which the kernel
    /// validates.
    Openat2 { len = 24 };
    dirfd: i32 => fd;
    path: NonNull<u8> => addr = ptr_enc; <= |s: &sys::Sqe| ptr_dec(s.addr);
    /// Pointer to a `struct open_how`.
    how: NonNull<c_void> => addr2 = ptr_enc; <= |s: &sys::Sqe| ptr_dec(s.addr2);
    /// Install into this direct descriptor slot rather than returning an fd.
    slot: u32 => file_index = slot_encode; <= |s: &sys::Sqe| slot_decode(s.file_index);
}

opcode! {
    /// Remove a name, as `unlinkat`.
    Unlinkat {};
    dirfd: i32 => fd;
    path: NonNull<u8> => addr = ptr_enc; <= |s: &sys::Sqe| ptr_dec(s.addr);
    /// `AT_REMOVEDIR` to remove a directory.
    flags: u32 => op_flags;
}

opcode! {
    /// Create a directory, as `mkdirat`.
    Mkdirat {};
    dirfd: i32 => fd;
    path: NonNull<u8> => addr = ptr_enc; <= |s: &sys::Sqe| ptr_dec(s.addr);
    mode: u32 => len;
}

opcode! {
    /// Rename a file, as `renameat2`.
    Renameat {};
    /// Directory to resolve [`Renameat::oldpath`] against.
    olddirfd: i32 => fd;
    oldpath: NonNull<u8> => addr = ptr_enc; <= |s: &sys::Sqe| ptr_dec(s.addr);
    /// Directory to resolve [`Renameat::newpath`] against.
    newdirfd: i32 => len = |v: i32| v as u32; <= |s: &sys::Sqe| s.len as i32;
    newpath: NonNull<u8> => addr2 = ptr_enc; <= |s: &sys::Sqe| ptr_dec(s.addr2);
    /// `RENAME_*` flags.
    flags: u32 => op_flags;
}

opcode! {
    /// Create a symbolic link, as `symlinkat`.
    Symlinkat {};
    /// Directory to resolve [`Symlinkat::linkpath`] against.
    newdirfd: i32 => fd;
    /// What the link points at.
    target: NonNull<u8> => addr = ptr_enc; <= |s: &sys::Sqe| ptr_dec(s.addr);
    /// Where to create the link.
    linkpath: NonNull<u8> => addr2 = ptr_enc; <= |s: &sys::Sqe| ptr_dec(s.addr2);
}

opcode! {
    /// Create a hard link, as `linkat`.
    Linkat {};
    olddirfd: i32 => fd;
    oldpath: NonNull<u8> => addr = ptr_enc; <= |s: &sys::Sqe| ptr_dec(s.addr);
    newdirfd: i32 => len = |v: i32| v as u32; <= |s: &sys::Sqe| s.len as i32;
    newpath: NonNull<u8> => addr2 = ptr_enc; <= |s: &sys::Sqe| ptr_dec(s.addr2);
    /// `AT_*` flags.
    flags: u32 => op_flags;
}

opcode! {
    /// Create a pipe pair.
    Pipe {};
    /// Where to write the two descriptors.
    fds: NonNull<i32> => addr = ptr_enc; <= |s: &sys::Sqe| ptr_dec(s.addr);
    /// `O_*` flags.
    flags: u32 => op_flags;
    /// Install into consecutive direct descriptor slots starting here.
    slot: u32 => file_index = slot_encode; <= |s: &sys::Sqe| slot_decode(s.file_index);
}

// ---------------------------------------------------------------------------
// Sockets
// ---------------------------------------------------------------------------

opcode! {
    /// Connect a socket.
    Connect {};
    fd: i32 => fd;
    /// The peer address.
    addr: NonNull<c_void> => addr = ptr_enc; <= |s: &sys::Sqe| ptr_dec(s.addr);
    /// Size of [`Connect::addr`]. Passed by value, unlike `Accept`.
    addrlen: u32 => addr2 = |v: u32| v as u64; <= |s: &sys::Sqe| s.addr2 as u32;
}

opcode! {
    /// Bind a socket to an address.
    Bind {};
    fd: i32 => fd;
    addr: NonNull<c_void> => addr = ptr_enc; <= |s: &sys::Sqe| ptr_dec(s.addr);
    addrlen: u32 => addr2 = |v: u32| v as u64; <= |s: &sys::Sqe| s.addr2 as u32;
}

opcode! {
    /// Mark a socket as listening.
    Listen {};
    fd: i32 => fd;
    backlog: u32 => len;
}

opcode! {
    /// Shut down part of a full-duplex connection.
    Shutdown {};
    fd: i32 => fd;
    /// `SHUT_RD`, `SHUT_WR` or `SHUT_RDWR`.
    how: u32 => len;
}

opcode! {
    /// Create a socket.
    Socket {};
    /// Address family, in the `fd` slot.
    domain: i32 => fd;
    /// `SOCK_*` type.
    kind: u64 => addr2;
    protocol: u32 => len;
    flags: u32 => op_flags;
    /// Install into this direct descriptor slot rather than returning an fd.
    slot: u32 => file_index = slot_encode; <= |s: &sys::Sqe| slot_decode(s.file_index);
}

opcode! {
    /// Receive a message with ancillary data.
    ///
    /// `len` is preset to 1: the kernel reads exactly one `msghdr`.
    Recvmsg { len = 1 };
    fd: i32 => fd;
    msg: NonNull<libc::msghdr> => addr = ptr_enc; <= |s: &sys::Sqe| ptr_dec(s.addr);
    /// `MSG_*` flags.
    flags: u32 => op_flags;
    /// [`sys::RecvSendFlags::RECV_MULTISHOT`] and friends.
    recv_flags: sys::RecvSendFlags => ioprio = |v: sys::RecvSendFlags| v.bits();
        <= |s: &sys::Sqe| sys::RecvSendFlags::from_bits_retain(s.ioprio);
    buf_group: u16 => buf_index;
}

opcode! {
    /// Send a message with ancillary data.
    Sendmsg { len = 1 };
    fd: i32 => fd;
    msg: NonNull<libc::msghdr> => addr = ptr_enc; <= |s: &sys::Sqe| ptr_dec(s.addr);
    flags: u32 => op_flags;
    send_flags: sys::RecvSendFlags => ioprio = |v: sys::RecvSendFlags| v.bits();
        <= |s: &sys::Sqe| sys::RecvSendFlags::from_bits_retain(s.ioprio);
    buf_index: u16 => buf_index;
}

opcode! {
    /// Zero-copy send.
    ///
    /// Produces two completions: the send result, then a
    /// [`sys::CqeFlags::NOTIF`] entry once the buffer may be reused.
    SendZc {};
    fd: i32 => fd;
    buf: NonNull<[u8]>
        => addr = |v: NonNull<[u8]>| v.cast::<u8>().as_ptr() as u64,
           len = |v: NonNull<[u8]>| v.len() as u32;
        <= |s: &sys::Sqe| buf_from(s.addr, s.len);
    nbytes: u32 => len;
    flags: u32 => op_flags;
    /// [`sys::RecvSendFlags::FIXED_BUF`], [`sys::RecvSendFlags::SEND_ZC_REPORT_USAGE`].
    zc_flags: sys::RecvSendFlags => ioprio = |v: sys::RecvSendFlags| v.bits();
        <= |s: &sys::Sqe| sys::RecvSendFlags::from_bits_retain(s.ioprio);
    buf_index: u16 => buf_index;
    /// Destination address for unconnected sockets.
    dest_addr: Option<NonNull<c_void>> => addr2 = optptr_enc;
        <= |s: &sys::Sqe| optptr_dec(s.addr2);
    /// Size of [`SendZc::dest_addr`], in the low half of the `file_index` slot.
    dest_addr_len: u16 => file_index = |v: u16| v as u32;
        <= |s: &sys::Sqe| s.file_index as u16;
}

opcode! {
    /// Zero-copy `sendmsg`.
    SendmsgZc { len = 1 };
    fd: i32 => fd;
    msg: NonNull<libc::msghdr> => addr = ptr_enc; <= |s: &sys::Sqe| ptr_dec(s.addr);
    flags: u32 => op_flags;
    zc_flags: sys::RecvSendFlags => ioprio = |v: sys::RecvSendFlags| v.bits();
        <= |s: &sys::Sqe| sys::RecvSendFlags::from_bits_retain(s.ioprio);
    buf_index: u16 => buf_index;
}

opcode! {
    /// Zero-copy receive into a registered zcrx area.
    ///
    /// liburing ships no prep helper for this; the mapping comes from the UAPI
    /// header, where the interface queue index lives in the `file_index` slot.
    RecvZc {};
    fd: i32 => fd;
    nbytes: u32 => len;
    flags: u32 => op_flags;
    recv_flags: sys::RecvSendFlags => ioprio = |v: sys::RecvSendFlags| v.bits();
        <= |s: &sys::Sqe| sys::RecvSendFlags::from_bits_retain(s.ioprio);
    /// The registered zcrx interface queue to receive from.
    ifq_idx: u32 => file_index;
}

// ---------------------------------------------------------------------------
// Polling, timers and cancellation
// ---------------------------------------------------------------------------

opcode! {
    /// Wait for readiness on a descriptor.
    PollAdd {};
    fd: i32 => fd;
    /// `EPOLL*` event mask. Little-endian only; big-endian needs a word swap.
    events: u32 => op_flags;
    /// [`sys::PollFlags::ADD_MULTI`] for multishot. Lives in `len`, not
    /// `op_flags`, which the event mask occupies.
    flags: sys::PollFlags => len = |v: sys::PollFlags| v.bits();
        <= |s: &sys::Sqe| sys::PollFlags::from_bits_retain(s.len);
}

opcode! {
    /// Remove or update a pending poll.
    PollRemove { fd = -1 };
    /// `user_data` of the poll to act on.
    target: u64 => addr;
    /// Replacement `user_data`, with [`sys::PollFlags::UPDATE_USER_DATA`].
    new_user_data: u64 => addr2;
    /// Replacement event mask, with [`sys::PollFlags::UPDATE_EVENTS`].
    events: u32 => op_flags;
    flags: sys::PollFlags => len = |v: sys::PollFlags| v.bits();
        <= |s: &sys::Sqe| sys::PollFlags::from_bits_retain(s.len);
}

opcode! {
    /// Cancel a pending operation.
    AsyncCancel { fd = -1 };
    /// `user_data` of the operation to cancel.
    target: u64 => addr;
    /// Cancel by descriptor, with [`sys::AsyncCancelFlags::FD`].
    fd: i32 => fd;
    flags: sys::AsyncCancelFlags => op_flags = |v: sys::AsyncCancelFlags| v.bits();
        <= |s: &sys::Sqe| sys::AsyncCancelFlags::from_bits_retain(s.op_flags);
    /// Cancel by opcode, with [`sys::AsyncCancelFlags::OP`].
    opcode: u16 => buf_index;
}

opcode! {
    /// Remove or update a pending timeout.
    TimeoutRemove { fd = -1 };
    /// `user_data` of the timeout to act on.
    target: u64 => addr;
    /// Replacement value, with [`sys::TimeoutFlags::UPDATE`].
    ts: Option<NonNull<sys::Timespec>> => addr2 = optptr_enc;
        <= |s: &sys::Sqe| optptr_dec(s.addr2);
    flags: sys::TimeoutFlags => op_flags = |v: sys::TimeoutFlags| v.bits();
        <= |s: &sys::Sqe| sys::TimeoutFlags::from_bits_retain(s.op_flags);
}

opcode! {
    /// Cancel the previous linked operation if it outlives the timeout.
    ///
    /// Must directly follow an SQE carrying [`sys::SqeFlags::IO_LINK`].
    LinkTimeout { len = 1, fd = -1 };
    ts: NonNull<sys::Timespec> => addr = ptr_enc; <= |s: &sys::Sqe| ptr_dec(s.addr);
    flags: sys::TimeoutFlags => op_flags = |v: sys::TimeoutFlags| v.bits();
        <= |s: &sys::Sqe| sys::TimeoutFlags::from_bits_retain(s.op_flags);
}

opcode! {
    /// Wait on a futex.
    FutexWait {};
    /// The futex word.
    futex: NonNull<u32> => addr = ptr_enc; <= |s: &sys::Sqe| ptr_dec(s.addr);
    /// Value to compare against.
    val: u64 => addr2;
    /// Bitset of which waiters this matches.
    mask: u64 => addr3;
    /// `FUTEX2_*` flags, in the `fd` slot.
    futex_flags: i32 => fd;
    flags: u32 => op_flags;
}

opcode! {
    /// Wake waiters on a futex.
    FutexWake {};
    futex: NonNull<u32> => addr = ptr_enc; <= |s: &sys::Sqe| ptr_dec(s.addr);
    /// How many waiters to wake.
    val: u64 => addr2;
    mask: u64 => addr3;
    futex_flags: i32 => fd;
    flags: u32 => op_flags;
}

opcode! {
    /// Wait on any of several futexes.
    FutexWaitv {};
    /// Array of `struct futex_waitv`.
    futexv: NonNull<c_void> => addr = ptr_enc; <= |s: &sys::Sqe| ptr_dec(s.addr);
    /// Number of entries in `futexv`.
    nr_futex: u32 => len;
    flags: u32 => op_flags;
}

opcode! {
    /// Wait for a process to change state, as `waitid`.
    Waitid {};
    /// The id to wait on, in the `fd` slot.
    id: i32 => fd;
    /// `P_PID`, `P_PGID` or `P_ALL`.
    idtype: u32 => len;
    /// Where to write the `siginfo_t`.
    infop: NonNull<c_void> => addr2 = ptr_enc; <= |s: &sys::Sqe| ptr_dec(s.addr2);
    /// `WEXITED` and friends.
    options: u32 => file_index;
    flags: u32 => op_flags;
}

// ---------------------------------------------------------------------------
// epoll
// ---------------------------------------------------------------------------

opcode! {
    /// Modify an epoll set, as `epoll_ctl`.
    EpollCtl {};
    /// The epoll descriptor.
    epfd: i32 => fd;
    /// The descriptor being registered, in the `addr2` slot.
    target_fd: u64 => addr2;
    /// `EPOLL_CTL_ADD`, `_MOD` or `_DEL`.
    op: u32 => len;
    event: NonNull<libc::epoll_event> => addr = ptr_enc;
        <= |s: &sys::Sqe| ptr_dec(s.addr);
}

opcode! {
    /// Wait on an epoll set.
    EpollWait {};
    epfd: i32 => fd;
    /// Where to write the ready events.
    events: NonNull<libc::epoll_event> => addr = ptr_enc;
        <= |s: &sys::Sqe| ptr_dec(s.addr);
    /// Capacity of `events`.
    maxevents: u32 => len;
    flags: u32 => op_flags;
}

// ---------------------------------------------------------------------------
// Splicing
// ---------------------------------------------------------------------------

opcode! {
    /// Move data between descriptors without copying through userspace.
    Splice {};
    /// Destination descriptor.
    fd_out: i32 => fd;
    /// Offset in `fd_out`, or `u64::MAX` for the current position.
    off_out: u64 => addr2;
    /// Source descriptor. OR [`sys::SPLICE_F_FD_IN_FIXED`] into `flags` to use
    /// a direct descriptor here.
    fd_in: i32 => file_index = |v: i32| v as u32; <= |s: &sys::Sqe| s.file_index as i32;
    /// Offset in `fd_in`, or `u64::MAX` for the current position.
    off_in: u64 => addr;
    nbytes: u32 => len;
    /// `SPLICE_F_*` flags.
    flags: u32 => op_flags;
}

opcode! {
    /// Duplicate pipe contents, as `tee`.
    Tee {};
    fd_out: i32 => fd;
    fd_in: i32 => file_index = |v: i32| v as u32; <= |s: &sys::Sqe| s.file_index as i32;
    nbytes: u32 => len;
    flags: u32 => op_flags;
}

// ---------------------------------------------------------------------------
// Ring resources
// ---------------------------------------------------------------------------

opcode! {
    /// Hand a block of buffers to the kernel, the pre-buffer-ring interface.
    ProvideBuffers {};
    /// Start of the buffer block.
    addr: NonNull<u8> => addr = ptr_enc; <= |s: &sys::Sqe| ptr_dec(s.addr);
    /// Size of each buffer.
    buf_len: u32 => len;
    /// How many buffers the block holds, in the `fd` slot.
    nr: i32 => fd;
    /// The buffer group these join.
    bgid: u16 => buf_index;
    /// Buffer id of the first buffer; the rest count up from it.
    bid: u64 => addr2;
}

opcode! {
    /// Take buffers back from a group.
    RemoveBuffers {};
    /// How many to remove, in the `fd` slot.
    nr: i32 => fd;
    bgid: u16 => buf_index;
}

opcode! {
    /// Update the registered file table from inside the ring.
    FilesUpdate { fd = -1 };
    /// Array of descriptors; [`sys::REGISTER_FILES_SKIP`] leaves a slot alone.
    fds: NonNull<i32> => addr = ptr_enc; <= |s: &sys::Sqe| ptr_dec(s.addr);
    nr_fds: u32 => len;
    /// First slot to update.
    offset: u64 => addr2;
}

opcode! {
    /// Turn a direct descriptor back into a regular file descriptor.
    ///
    /// Requires [`sys::SqeFlags::FIXED_FILE`], which `new` presets.
    FixedFdInstall { flags = sys::SqeFlags::FIXED_FILE };
    /// The direct descriptor to install.
    fd: i32 => fd;
    /// [`sys::InstallFdFlags::NO_CLOEXEC`].
    flags: sys::InstallFdFlags => op_flags = |v: sys::InstallFdFlags| v.bits();
        <= |s: &sys::Sqe| sys::InstallFdFlags::from_bits_retain(s.op_flags);
}

opcode! {
    /// Post a completion, or send a descriptor, to another ring.
    MsgRing {};
    /// The target ring's descriptor.
    fd: i32 => fd;
    /// [`sys::MsgRingOp::Data`] or [`sys::MsgRingOp::SendFd`].
    cmd: sys::MsgRingOp => addr = |v: sys::MsgRingOp| v as u64;
        <= |s: &sys::Sqe| if s.addr == sys::MsgRingOp::SendFd as u64 {
            sys::MsgRingOp::SendFd
        } else {
            sys::MsgRingOp::Data
        };
    /// Value delivered as the target completion's `res`.
    result: u32 => len;
    /// Value delivered as the target completion's `user_data`.
    data: u64 => addr2;
    flags: sys::MsgRingFlags => op_flags = |v: sys::MsgRingFlags| v.bits();
        <= |s: &sys::Sqe| sys::MsgRingFlags::from_bits_retain(s.op_flags);
    /// Descriptor to send, for [`sys::MsgRingOp::SendFd`].
    source_fd: u64 => addr3;
    /// Target slot for the sent descriptor. Shares a field with
    /// [`MsgRing::cqe_flags`]; set only one.
    slot: u32 => file_index = slot_encode; <= |s: &sys::Sqe| slot_decode(s.file_index);
    /// Flags to pass through to the target completion, with
    /// [`sys::MsgRingFlags::FLAGS_PASS`]. Shares a field with [`MsgRing::slot`].
    cqe_flags: u32 => file_index;
}

// ---------------------------------------------------------------------------
// Extended attributes
// ---------------------------------------------------------------------------

opcode! {
    /// Read an extended attribute by path.
    Getxattr {};
    /// Null-terminated attribute name.
    name: NonNull<u8> => addr = ptr_enc; <= |s: &sys::Sqe| ptr_dec(s.addr);
    /// Where to write the value.
    value: NonNull<u8> => addr2 = ptr_enc; <= |s: &sys::Sqe| ptr_dec(s.addr2);
    /// Null-terminated file path.
    path: NonNull<u8> => addr3 = ptr_enc; <= |s: &sys::Sqe| ptr_dec(s.addr3);
    /// Capacity of `value`.
    len: u32 => len;
}

opcode! {
    /// Write an extended attribute by path.
    Setxattr {};
    name: NonNull<u8> => addr = ptr_enc; <= |s: &sys::Sqe| ptr_dec(s.addr);
    value: NonNull<u8> => addr2 = ptr_enc; <= |s: &sys::Sqe| ptr_dec(s.addr2);
    path: NonNull<u8> => addr3 = ptr_enc; <= |s: &sys::Sqe| ptr_dec(s.addr3);
    /// Length of `value`.
    len: u32 => len;
    /// `XATTR_CREATE` or `XATTR_REPLACE`.
    flags: u32 => op_flags;
}

opcode! {
    /// Read an extended attribute by descriptor.
    Fgetxattr {};
    fd: i32 => fd;
    name: NonNull<u8> => addr = ptr_enc; <= |s: &sys::Sqe| ptr_dec(s.addr);
    value: NonNull<u8> => addr2 = ptr_enc; <= |s: &sys::Sqe| ptr_dec(s.addr2);
    len: u32 => len;
}

opcode! {
    /// Write an extended attribute by descriptor.
    Fsetxattr {};
    fd: i32 => fd;
    name: NonNull<u8> => addr = ptr_enc; <= |s: &sys::Sqe| ptr_dec(s.addr);
    value: NonNull<u8> => addr2 = ptr_enc; <= |s: &sys::Sqe| ptr_dec(s.addr2);
    len: u32 => len;
    flags: u32 => op_flags;
}

// ---------------------------------------------------------------------------
// Passthrough commands
// ---------------------------------------------------------------------------

opcode! {
    /// A file-specific passthrough command.
    ///
    /// The 80-byte payload lives past the end of a 64-byte SQE, so anything
    /// needing it must use [`UringCmd128`] on a ring that supports it.
    UringCmd {};
    fd: i32 => fd;
    /// The command selector, in the low half of the `addr2` slot.
    cmd_op: u32 => addr2 = |v: u32| v as u64; <= |s: &sys::Sqe| s.addr2 as u32;
    /// Socket option `(level, optname)`, for `SOCKET_URING_OP_[GS]ETSOCKOPT`.
    ///
    /// Both halves share one 64-bit word, so they are set together rather than
    /// as two fields that would overwrite each other.
    sockopt: (u32, u32)
        => addr = |v: (u32, u32)| v.0 as u64 | ((v.1 as u64) << 32);
        <= |s: &sys::Sqe| (s.addr as u32, (s.addr >> 32) as u32);
    /// Option value buffer.
    optval: u64 => addr3;
    /// Size of `optval`.
    optlen: u32 => file_index;
    flags: sys::UringCmdFlags => op_flags = |v: sys::UringCmdFlags| v.bits();
        <= |s: &sys::Sqe| sys::UringCmdFlags::from_bits_retain(s.op_flags);
}

opcode! {
    /// A passthrough command with the full 80-byte payload.
    ///
    /// Occupies two 64-byte submission slots. [`Self::into_sqe`] yields only the
    /// first; the payload must be written into the second slot separately, and
    /// the ring needs [`sys::SetupFlags::SQE128`] or
    /// [`sys::SetupFlags::SQE_MIXED`].
    UringCmd128 {};
    fd: i32 => fd;
    cmd_op: u32 => addr2 = |v: u32| v as u64; <= |s: &sys::Sqe| s.addr2 as u32;
    flags: sys::UringCmdFlags => op_flags = |v: sys::UringCmdFlags| v.bits();
        <= |s: &sys::Sqe| sys::UringCmdFlags::from_bits_retain(s.op_flags);
}

opcode! {
    /// A no-op occupying two submission slots, for padding a 128-byte ring.
    Nop128 { fd = -1 };
    flags: sys::NopFlags => op_flags = |v: sys::NopFlags| v.bits();
        <= |s: &sys::Sqe| sys::NopFlags::from_bits_retain(s.op_flags);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::io_uring::ring::Ring;
    use crate::io_uring::syscall;

    fn buf_of(v: &mut [u8]) -> NonNull<[u8]> {
        NonNull::from(v)
    }

    /// Every setter must be readable back through its `get_`, including the
    /// mapped fields whose reverse expression is written by hand.
    #[test]
    fn fields_round_trip() {
        let mut store = [0u8; 32];
        let buf = buf_of(&mut store);
        let mut ts = sys::Timespec {
            tv_sec: 3,
            tv_nsec: 4,
        };
        let ts_ptr = NonNull::from(&mut ts);

        let r = Read::new()
            .fd(7)
            .buf(buf)
            .nbytes(16)
            .offset(4096)
            .rw_flags(9)
            .user_data(11)
            .sqe_flags(sys::SqeFlags::IO_LINK)
            .personality(3);
        assert_eq!(r.get_fd(), 7);
        assert_eq!(r.get_buf().cast::<u8>(), buf.cast::<u8>());
        assert_eq!(r.get_nbytes(), 16, "nbytes must override the buf length");
        assert_eq!(r.get_offset(), 4096);
        assert_eq!(r.get_rw_flags(), 9);
        assert_eq!(r.get_user_data(), 11);
        assert_eq!(r.get_sqe_flags(), sys::SqeFlags::IO_LINK);
        assert_eq!(r.get_personality(), 3);

        let mut sa = [0u8; 128];
        let mut salen = sa.len() as u32;
        let peer = (
            NonNull::from(&mut sa).cast::<c_void>(),
            NonNull::from(&mut salen),
        );
        let a = Accept::new()
            .fd(4)
            .peer(Some(peer))
            .flags(1)
            .accept_flags(sys::AcceptFlags::MULTISHOT)
            .slot(sys::FILE_INDEX_ALLOC);
        assert_eq!(a.get_peer(), Some(peer));
        assert_eq!(a.get_accept_flags(), sys::AcceptFlags::MULTISHOT);
        assert_eq!(a.get_slot(), sys::FILE_INDEX_ALLOC);

        let t = Timeout::new()
            .ts(ts_ptr)
            .count(5)
            .flags(sys::TimeoutFlags::ABS | sys::TimeoutFlags::ETIME_SUCCESS);
        assert_eq!(t.get_ts(), ts_ptr);
        assert_eq!(t.get_count(), 5);
        assert_eq!(
            t.get_flags(),
            sys::TimeoutFlags::ABS | sys::TimeoutFlags::ETIME_SUCCESS
        );

        let n = Nop::new().flags(sys::NopFlags::INJECT_RESULT).result(-5);
        assert_eq!(n.get_flags(), sys::NopFlags::INJECT_RESULT);
        assert_eq!(n.get_result(), -5);

        let s = Recv::new()
            .recv_flags(sys::RecvSendFlags::BUNDLE)
            .buf_group(9);
        assert_eq!(s.get_recv_flags(), sys::RecvSendFlags::BUNDLE);
        assert_eq!(s.get_buf_group(), 9);
    }

    /// The encoded SQE must land in the same slots `io_uring_prep_read` uses:
    /// `prep_rw(READ, fd, addr, len, off)` with nothing else touched.
    #[test]
    fn read_encodes_like_prep_rw() {
        let mut store = [0u8; 64];
        let buf = buf_of(&mut store);
        let sqe = Read::new().fd(5).buf(buf).offset(9).into_sqe();

        assert_eq!(sqe.opcode, sys::Opcode::Read);
        assert_eq!(sqe.fd, 5);
        assert_eq!(sqe.addr, buf.cast::<u8>().as_ptr() as u64);
        assert_eq!(sqe.len, 64);
        assert_eq!(sqe.addr2, 9);
        // Everything prep_rw leaves alone must still be zero.
        assert_eq!(sqe.ioprio, 0);
        assert_eq!(sqe.op_flags, 0);
        assert_eq!(sqe.buf_index, 0);
        assert_eq!(sqe.file_index, 0);
        assert_eq!(sqe.addr3, 0);
        assert_eq!(sqe.addr3_hi, 0);
    }

    /// `prep_timeout` submits `len = 1` and `fd = -1`; a zero `len` is rejected.
    #[test]
    fn timeout_presets_are_applied() {
        let sqe = Timeout::new().into_sqe();
        assert_eq!(sqe.len, 1);
        assert_eq!(sqe.fd, -1);
        assert_eq!(sqe.opcode, sys::Opcode::Timeout);
    }

    /// Direct descriptor slots go on the wire 1-based so that zero can mean
    /// "not a fixed file", and must read back 0-based.
    #[test]
    fn slots_are_encoded_one_based() {
        assert_eq!(Openat::new().slot(0).into_sqe().file_index, 1);
        assert_eq!(Openat::new().slot(0).get_slot(), 0);
        assert_eq!(Openat::new().slot(41).into_sqe().file_index, 42);
        // The allocate sentinel has to survive the round trip untouched.
        let alloc = Openat::new().slot(sys::FILE_INDEX_ALLOC);
        assert_eq!(alloc.into_sqe().file_index, 0, "!0 + 1 wraps to 0");
        assert_eq!(alloc.get_slot(), sys::FILE_INDEX_ALLOC);
    }

    /// `None` must leave both wire fields zero, so the kernel is never handed
    /// a length pointer without an address to go with it.
    #[test]
    fn accept_peer_is_all_or_nothing() {
        let none = Accept::new().fd(3).into_sqe();
        assert_eq!(none.addr, 0);
        assert_eq!(none.addr2, 0);
        assert_eq!(Accept::new().fd(3).get_peer(), None);

        let mut sa = [0u8; 128];
        let mut salen = sa.len() as u32;
        let peer = (
            NonNull::from(&mut sa).cast::<c_void>(),
            NonNull::from(&mut salen),
        );
        let set = Accept::new().peer(Some(peer)).into_sqe();
        assert_eq!(set.addr, peer.0.as_ptr() as u64);
        assert_eq!(set.addr2, peer.1.as_ptr() as u64);

        // Clearing it must clear both halves, not leave a dangling length.
        let cleared = Accept::new().peer(Some(peer)).peer(None).into_sqe();
        assert_eq!(cleared.addr, 0);
        assert_eq!(cleared.addr2, 0);
    }

    /// Every [`sys::Opcode`] must have a type, and each type must encode its
    /// own opcode. Catches both a missing op and a copy-paste in the header.
    #[test]
    fn every_opcode_has_a_type() {
        macro_rules! check {
            ($($op:ident),* $(,)?) => {{
                let mut seen = Vec::new();
                $( seen.push(($op::new().into_sqe().opcode, stringify!($op))); )*
                seen
            }};
        }
        let seen = check![
            Nop,
            Readv,
            Writev,
            Fsync,
            ReadFixed,
            WriteFixed,
            PollAdd,
            PollRemove,
            SyncFileRange,
            Sendmsg,
            Recvmsg,
            Timeout,
            TimeoutRemove,
            Accept,
            AsyncCancel,
            LinkTimeout,
            Connect,
            Fallocate,
            Openat,
            Close,
            FilesUpdate,
            Statx,
            Read,
            Write,
            Fadvise,
            Madvise,
            Send,
            Recv,
            Openat2,
            EpollCtl,
            Splice,
            ProvideBuffers,
            RemoveBuffers,
            Tee,
            Shutdown,
            Renameat,
            Unlinkat,
            Mkdirat,
            Symlinkat,
            Linkat,
            MsgRing,
            Fsetxattr,
            Setxattr,
            Fgetxattr,
            Getxattr,
            Socket,
            UringCmd,
            SendZc,
            SendmsgZc,
            ReadMultishot,
            Waitid,
            FutexWait,
            FutexWake,
            FutexWaitv,
            FixedFdInstall,
            Ftruncate,
            Bind,
            Listen,
            RecvZc,
            EpollWait,
            ReadvFixed,
            WritevFixed,
            Pipe,
            Nop128,
            UringCmd128,
        ];
        assert_eq!(seen.len(), sys::Opcode::LAST as usize, "op count");
        for (i, (opcode, name)) in seen.iter().enumerate() {
            assert_eq!(*opcode as u8, i as u8, "{name} encodes the wrong opcode");
        }
    }

    /// Presets the kernel requires, gathered in one place so a regression in
    /// any single op is visible.
    #[test]
    fn kernel_required_presets() {
        // A timespec count of zero is rejected.
        assert_eq!(Timeout::new().into_sqe().len, 1);
        assert_eq!(LinkTimeout::new().into_sqe().len, 1);
        // One msghdr.
        assert_eq!(Sendmsg::new().into_sqe().len, 1);
        assert_eq!(Recvmsg::new().into_sqe().len, 1);
        assert_eq!(SendmsgZc::new().into_sqe().len, 1);
        // sizeof(struct open_how), which the kernel validates.
        assert_eq!(Openat2::new().into_sqe().len, 24);
        // These two only work with the matching sqe flag set.
        assert_eq!(
            ReadMultishot::new().get_sqe_flags(),
            sys::SqeFlags::BUFFER_SELECT
        );
        assert_eq!(
            FixedFdInstall::new().get_sqe_flags(),
            sys::SqeFlags::FIXED_FILE
        );
    }

    /// `Splice` spreads its two descriptors and two offsets across four
    /// different union slots, which is the easiest encoding to get wrong.
    #[test]
    fn splice_encodes_both_ends() {
        let sqe = Splice::new()
            .fd_out(4)
            .off_out(100)
            .fd_in(5)
            .off_in(200)
            .nbytes(4096)
            .flags(sys::SPLICE_F_FD_IN_FIXED)
            .into_sqe();
        assert_eq!(sqe.fd, 4, "fd_out lands in fd");
        assert_eq!(sqe.addr2, 100, "off_out lands in addr2");
        assert_eq!(sqe.file_index, 5, "fd_in lands in the splice_fd_in slot");
        assert_eq!(sqe.addr, 200, "off_in lands in the splice_off_in slot");
        assert_eq!(sqe.len, 4096);
        assert_eq!(sqe.op_flags, sys::SPLICE_F_FD_IN_FIXED);
    }

    /// `UringCmd` packs level and optname into the two halves of one word, so
    /// they must be set as a pair or each would clobber the other.
    #[test]
    fn uring_cmd_packs_the_sockopt_pair() {
        let c = UringCmd::new().sockopt((1, 2));
        assert_eq!(c.get_sockopt(), (1, 2));
        assert_eq!(c.as_sqe().addr, 1 | (2u64 << 32));
    }

    /// `buf` writes the address and the length; `nbytes` then narrows the
    /// length alone. The order matters, and the reverse is a no-op.
    #[test]
    fn nbytes_narrows_buf_only_when_applied_after() {
        let mut store = [0u8; 64];
        let b = buf_of(&mut store);
        assert_eq!(Read::new().buf(b).nbytes(8).into_sqe().len, 8);
        assert_eq!(Read::new().nbytes(8).buf(b).into_sqe().len, 64);
    }

    /// Submit one operation and return its completion result.
    fn run(ring: &Ring, sqe: sys::Sqe) -> i32 {
        let sq = ring.sq();
        let tail = sq.tail();
        // SAFETY: the ring is idle, so this slot is ours to write.
        unsafe { sq.sqe(tail).write(sqe) };
        sq.set_tail(tail + 1);
        // SAFETY: no argument is passed and the SQE above is well formed.
        let n = unsafe {
            syscall::io_uring_enter(
                ring.enter_fd(),
                1,
                1,
                ring.enter_flags() | sys::EnterFlags::GETEVENTS,
                std::ptr::null(),
                0,
            )
        }
        .expect("io_uring_enter");
        assert_eq!(n, 1);

        let cq = ring.cq();
        // SAFETY: head is below the tail we just observed via `ready`.
        let res = unsafe { &*cq.cqe(cq.head()) }.res;
        cq.advance(1);
        res
    }

    /// Drive real I/O through the typed ops: write a message into a pipe with
    /// `Write`, read it back with `Read`, and close both ends with `Close`.
    #[test]
    #[cfg_attr(miri, ignore = "issues real syscalls and mmaps a ring")]
    fn write_then_read_through_a_pipe() {
        const MSG: [u8; 14] = *b"hello io_uring";
        let mut params = sys::Params::default();
        let ring = Ring::with_params(8, &mut params).expect("Ring::with_params");

        let mut fds = [0i32; 2];
        // SAFETY: `fds` is a live two-element array, as pipe(2) requires.
        assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0);
        let [rd, wr] = fds;

        let mut out = MSG;
        let n = run(
            &ring,
            Write::new()
                .fd(wr)
                .buf(buf_of(&mut out))
                .offset(u64::MAX)
                .user_data(1)
                .into_sqe(),
        );
        assert_eq!(n, MSG.len() as i32, "short write: {n}");

        let mut back = [0u8; 32];
        let n = run(
            &ring,
            Read::new()
                .fd(rd)
                .buf(buf_of(&mut back))
                .offset(u64::MAX)
                .user_data(2)
                .into_sqe(),
        );
        assert_eq!(n, MSG.len() as i32, "short read: {n}");
        assert_eq!(back[..MSG.len()], MSG);

        assert_eq!(run(&ring, Close::new().fd(wr).into_sqe()), 0);
        assert_eq!(run(&ring, Close::new().fd(rd).into_sqe()), 0);
    }

    /// A `Nop` with an injected result must come back carrying it.
    #[test]
    #[cfg_attr(miri, ignore = "issues real syscalls and mmaps a ring")]
    fn nop_injects_its_result() {
        let mut params = sys::Params::default();
        let ring = Ring::with_params(8, &mut params).expect("Ring::with_params");
        assert_eq!(run(&ring, Nop::new().into_sqe()), 0);
        let injected = Nop::new()
            .flags(sys::NopFlags::INJECT_RESULT)
            .result(42)
            .into_sqe();
        assert_eq!(run(&ring, injected), 42);
    }
}
