//! Submission queue entry preparation, one `prep_` function per operation.
//!
//! Each `prep_` takes a [`Slot`] — an opaque handle to one entry in its final
//! home, the submission ring or the backlog — followed by the operation's own
//! parameters, named after what they mean rather than the union slots they land
//! in. It writes a fully initialised [`sys::Sqe`] into the slot: every field the
//! operation needs, and zero for every field it does not. The one field it
//! leaves alone is [`Sqe::user_data`](sys::Sqe::user_data), which the submission
//! path stamps.
//!
//! This mirrors liburing's `io_uring_prep_*` helpers. Where one kernel opcode
//! has two layouts — a 32- and a 64-bit length, a direct-descriptor variant, a
//! by-fd versus by-user-data cancel — there is one `prep_` function per layout,
//! so every argument maps to exactly one field.
//!
//! # Field conventions
//!
//! | shape | Rust type |
//! |---|---|
//! | required pointer | `NonNull<T>` |
//! | optional pointer | `Option<NonNull<T>>` |
//! | paired pointer + length pointer | `Option<(NonNull<A>, NonNull<B>)>` |
//! | file descriptor | `i32` |
//! | direct descriptor slot (output) | `u32`, 0-based; encoded 1-based on the wire |
//! | flag word | the matching `sys` bitflags type |
//!
//! `SqeFlags` — `FIXED_FILE`, `IO_LINK` and friends — are *not* arguments here.
//! `Driver::submit` applies them, and `Driver::submit_chain` adds the links a
//! chain needs. A `prep_` sets `Sqe::flags` only when a variant requires it
//! (`BUFFER_SELECT` for a multishot read), and the submission path ORs the
//! caller's flags on top.

use std::ffi::c_void;
use std::marker::PhantomData;
use std::mem;
use std::ptr::NonNull;

use crate::io_uring::sys;

// ---------------------------------------------------------------------------
// Slot
// ---------------------------------------------------------------------------

/// A submission slot handed to a `prep_` function.
///
/// Wraps the pointer to one [`sys::Sqe`] — in the ring or on the backlog — for
/// as long as the submission path holds the ring borrowed. The [`Drop`] impl
/// panics: a slot that was reserved must be filled, so [`Slot::fill`] (which a
/// `prep_` calls) is the only way to discharge one.
pub(crate) struct Slot<'a> {
    sqe: *mut sys::Sqe,
    _borrow: PhantomData<&'a mut sys::Sqe>,
}

impl<'a> Slot<'a> {
    /// # Safety
    ///
    /// `sqe` must point at a writable [`sys::Sqe`] that stays valid for `'a`.
    pub(crate) unsafe fn from_raw(sqe: *mut sys::Sqe) -> Slot<'a> {
        Slot {
            sqe,
            _borrow: PhantomData,
        }
    }

    pub(crate) fn as_raw(&self) -> *mut sys::Sqe {
        self.sqe
    }

    /// Write the fully built entry and discharge the slot.
    pub(crate) fn fill(self, sqe: sys::Sqe) {
        // SAFETY: the constructor's contract makes the pointer valid for writes.
        unsafe { self.sqe.write(sqe) };
        mem::forget(self);
    }
}

impl Drop for Slot<'_> {
    fn drop(&mut self) {
        panic!("a submission slot was reserved but never filled by a prep_ function");
    }
}

// ---------------------------------------------------------------------------
// Field encoders
// ---------------------------------------------------------------------------

fn buf_addr(v: NonNull<[u8]>) -> u64 {
    v.cast::<u8>().as_ptr() as u64
}

fn buf_len(v: NonNull<[u8]>) -> u32 {
    v.len() as u32
}

fn ptr_addr<T>(v: NonNull<T>) -> u64 {
    v.as_ptr() as u64
}

fn optptr_addr<T>(v: Option<NonNull<T>>) -> u64 {
    v.map_or(0, |p| p.as_ptr() as u64)
}

/// An output slot goes on the wire 1-based, so that zero can mean "not a fixed
/// file". [`sys::FILE_INDEX_ALLOC`] is the exception: the kernel compares the
/// raw field against it before subtracting, so it must pass through untouched.
fn slot_encode(v: u32) -> u32 {
    if v == sys::FILE_INDEX_ALLOC {
        v
    } else {
        v.wrapping_add(1)
    }
}

fn peer_addr(v: Option<(NonNull<c_void>, NonNull<u32>)>) -> u64 {
    v.map_or(0, |(a, _)| a.as_ptr() as u64)
}

fn peer_len_addr(v: Option<(NonNull<c_void>, NonNull<u32>)>) -> u64 {
    v.map_or(0, |(_, l)| l.as_ptr() as u64)
}

// ---------------------------------------------------------------------------
// prep! macro
// ---------------------------------------------------------------------------

/// Generate a `prep_` function.
///
/// ```ignore
/// prep! {
///     /// doc
///     prep_read => Read { };
///     fd: i32 => fd;
///     buf: NonNull<[u8]> => addr = buf_addr, len = buf_len;
///     offset: u64 => addr2;
/// }
/// ```
///
/// The brace list after the opcode presets `Sqe` fields the kernel requires a
/// fixed value in. Each `arg : type => slot [= conv], ...;` line writes one or
/// more SQE fields from one argument, applying `conv` if given. Fields no
/// argument touches stay zero, from [`sys::Sqe::ZEROED`].
macro_rules! prep {
    (
        $(#[$doc:meta])*
        $fn:ident => $op:ident { $($pk:ident = $pv:expr),* $(,)? } ;
        $( $arg:ident : $argty:ty => $( $slot:ident $(= $conv:expr)? ),+ ; )*
    ) => {
        $(#[$doc])*
        #[allow(dead_code)]
        pub(crate) fn $fn(dst: Slot<'_>, $($arg: $argty),*) {
            let mut sqe = sys::Sqe::ZEROED;
            sqe.opcode = sys::Opcode::$op;
            $( sqe.$pk = $pv; )*
            $( $( sqe.$slot = prep!(@conv $arg $(, $conv)?); )+ )*
            dst.fill(sqe);
        }
    };

    (@conv $arg:ident) => { $arg };
    (@conv $arg:ident, $conv:expr) => { ($conv)($arg) };
}

// ---------------------------------------------------------------------------
// Basic file I/O
// ---------------------------------------------------------------------------

prep! {
    /// Do nothing. Useful for waking a ring and for padding a wrap.
    prep_nop => Nop { };
}

prep! {
    /// A `Nop` that reports `result` instead of zero.
    prep_nop_inject => Nop { op_flags = sys::NopFlags::INJECT_RESULT.bits() };
    result: i32 => len = |v: i32| v as u32;
}

prep! {
    /// A no-op occupying two submission slots, for padding a 128-byte ring.
    prep_nop128 => Nop128 { fd = -1 };
}

prep! {
    /// Read from a file descriptor at an offset. The length is the buffer's.
    prep_read => Read { };
    fd: i32 => fd;
    buf: NonNull<[u8]> => addr = buf_addr, len = buf_len;
    offset: u64 => addr2;
    rw_flags: u32 => op_flags;
}

prep! {
    /// Write to a file descriptor at an offset. The length is the buffer's.
    prep_write => Write { };
    fd: i32 => fd;
    buf: NonNull<[u8]> => addr = buf_addr, len = buf_len;
    offset: u64 => addr2;
    rw_flags: u32 => op_flags;
}

prep! {
    /// Vectored read, as `preadv2`.
    prep_readv => Readv { };
    fd: i32 => fd;
    iovecs: NonNull<libc::iovec> => addr = ptr_addr;
    nr_vecs: u32 => len;
    offset: u64 => addr2;
    rw_flags: u32 => op_flags;
}

prep! {
    /// Vectored write, as `pwritev2`.
    prep_writev => Writev { };
    fd: i32 => fd;
    iovecs: NonNull<libc::iovec> => addr = ptr_addr;
    nr_vecs: u32 => len;
    offset: u64 => addr2;
    rw_flags: u32 => op_flags;
}

prep! {
    /// Read into a registered buffer.
    prep_read_fixed => ReadFixed { };
    fd: i32 => fd;
    buf: NonNull<[u8]> => addr = buf_addr, len = buf_len;
    offset: u64 => addr2;
    buf_index: u16 => buf_index;
}

prep! {
    /// Write from a registered buffer.
    prep_write_fixed => WriteFixed { };
    fd: i32 => fd;
    buf: NonNull<[u8]> => addr = buf_addr, len = buf_len;
    offset: u64 => addr2;
    buf_index: u16 => buf_index;
}

prep! {
    /// Vectored read into registered buffers.
    prep_readv_fixed => ReadvFixed { };
    fd: i32 => fd;
    iovecs: NonNull<libc::iovec> => addr = ptr_addr;
    nr_vecs: u32 => len;
    offset: u64 => addr2;
    rw_flags: u32 => op_flags;
    buf_index: u16 => buf_index;
}

prep! {
    /// Vectored write from registered buffers.
    prep_writev_fixed => WritevFixed { };
    fd: i32 => fd;
    iovecs: NonNull<libc::iovec> => addr = ptr_addr;
    nr_vecs: u32 => len;
    offset: u64 => addr2;
    rw_flags: u32 => op_flags;
    buf_index: u16 => buf_index;
}

prep! {
    /// Multishot read into provided buffers. Presets `BUFFER_SELECT`.
    prep_read_multishot => ReadMultishot { flags = sys::SqeFlags::BUFFER_SELECT };
    fd: i32 => fd;
    nbytes: u32 => len;
    offset: u64 => addr2;
    buf_group: u16 => buf_index;
}

prep! {
    /// Close a file descriptor.
    prep_close => Close { };
    fd: i32 => fd;
}

prep! {
    /// Close a direct descriptor slot.
    prep_close_direct => Close { };
    slot: u32 => file_index = slot_encode;
}

// ---------------------------------------------------------------------------
// File state
// ---------------------------------------------------------------------------

prep! {
    /// Flush a file to storage.
    prep_fsync => Fsync { };
    fd: i32 => fd;
    flags: sys::FsyncFlags => op_flags = |v: sys::FsyncFlags| v.bits();
}

prep! {
    /// Flush a byte range, as `sync_file_range`.
    prep_sync_file_range => SyncFileRange { };
    fd: i32 => fd;
    nbytes: u32 => len;
    offset: u64 => addr2;
    flags: u32 => op_flags;
}

prep! {
    /// Manipulate file space, as `fallocate`.
    prep_fallocate => Fallocate { };
    fd: i32 => fd;
    mode: u32 => len;
    offset: u64 => addr2;
    length: u64 => addr;
}

prep! {
    /// Truncate a file, as `ftruncate`.
    prep_ftruncate => Ftruncate { };
    fd: i32 => fd;
    length: u64 => addr2;
}

prep! {
    /// Declare a file access pattern, as `posix_fadvise` (32-bit length).
    prep_fadvise => Fadvise { };
    fd: i32 => fd;
    offset: u64 => addr2;
    length: u32 => len;
    advice: u32 => op_flags;
}

prep! {
    /// Declare a file access pattern with a 64-bit length.
    prep_fadvise64 => Fadvise { };
    fd: i32 => fd;
    offset: u64 => addr2;
    length: u64 => addr;
    advice: u32 => op_flags;
}

prep! {
    /// Declare a memory access pattern, as `madvise` (32-bit length).
    prep_madvise => Madvise { fd = -1 };
    addr: NonNull<u8> => addr = ptr_addr;
    length: u32 => len;
    advice: u32 => op_flags;
}

prep! {
    /// Declare a memory access pattern with a 64-bit length.
    prep_madvise64 => Madvise { fd = -1 };
    addr: NonNull<u8> => addr = ptr_addr;
    length: u64 => addr2;
    advice: u32 => op_flags;
}

prep! {
    /// Query file metadata, as `statx`.
    prep_statx => Statx { };
    dirfd: i32 => fd;
    path: NonNull<u8> => addr = ptr_addr;
    mask: u32 => len;
    statxbuf: NonNull<c_void> => addr2 = ptr_addr;
    flags: u32 => op_flags;
}

// ---------------------------------------------------------------------------
// Namespace operations
// ---------------------------------------------------------------------------

prep! {
    /// Open a file relative to a directory descriptor.
    prep_openat => Openat { };
    dirfd: i32 => fd;
    path: NonNull<u8> => addr = ptr_addr;
    mode: u32 => len;
    flags: u32 => op_flags;
}

prep! {
    /// Open a file into a direct descriptor slot.
    prep_openat_direct => Openat { };
    dirfd: i32 => fd;
    path: NonNull<u8> => addr = ptr_addr;
    mode: u32 => len;
    flags: u32 => op_flags;
    slot: u32 => file_index = slot_encode;
}

prep! {
    /// Open with the extended `open_how` description. Presets `len = 24`.
    prep_openat2 => Openat2 { len = 24 };
    dirfd: i32 => fd;
    path: NonNull<u8> => addr = ptr_addr;
    how: NonNull<c_void> => addr2 = ptr_addr;
}

prep! {
    /// `openat2` into a direct descriptor slot.
    prep_openat2_direct => Openat2 { len = 24 };
    dirfd: i32 => fd;
    path: NonNull<u8> => addr = ptr_addr;
    how: NonNull<c_void> => addr2 = ptr_addr;
    slot: u32 => file_index = slot_encode;
}

prep! {
    /// Remove a name, as `unlinkat`.
    prep_unlinkat => Unlinkat { };
    dirfd: i32 => fd;
    path: NonNull<u8> => addr = ptr_addr;
    flags: u32 => op_flags;
}

prep! {
    /// Create a directory, as `mkdirat`.
    prep_mkdirat => Mkdirat { };
    dirfd: i32 => fd;
    path: NonNull<u8> => addr = ptr_addr;
    mode: u32 => len;
}

prep! {
    /// Rename a file, as `renameat2`.
    prep_renameat => Renameat { };
    olddirfd: i32 => fd;
    oldpath: NonNull<u8> => addr = ptr_addr;
    newdirfd: i32 => len = |v: i32| v as u32;
    newpath: NonNull<u8> => addr2 = ptr_addr;
    flags: u32 => op_flags;
}

prep! {
    /// Create a symbolic link, as `symlinkat`.
    prep_symlinkat => Symlinkat { };
    newdirfd: i32 => fd;
    target: NonNull<u8> => addr = ptr_addr;
    linkpath: NonNull<u8> => addr2 = ptr_addr;
}

prep! {
    /// Create a hard link, as `linkat`.
    prep_linkat => Linkat { };
    olddirfd: i32 => fd;
    oldpath: NonNull<u8> => addr = ptr_addr;
    newdirfd: i32 => len = |v: i32| v as u32;
    newpath: NonNull<u8> => addr2 = ptr_addr;
    flags: u32 => op_flags;
}

prep! {
    /// Create a pipe pair.
    prep_pipe => Pipe { };
    fds: NonNull<i32> => addr = ptr_addr;
    flags: u32 => op_flags;
}

prep! {
    /// Create a pipe pair into consecutive direct descriptor slots.
    prep_pipe_direct => Pipe { };
    fds: NonNull<i32> => addr = ptr_addr;
    flags: u32 => op_flags;
    slot: u32 => file_index = slot_encode;
}

// ---------------------------------------------------------------------------
// Extended attributes
// ---------------------------------------------------------------------------

prep! {
    /// Read an extended attribute by path.
    prep_getxattr => Getxattr { };
    name: NonNull<u8> => addr = ptr_addr;
    value: NonNull<u8> => addr2 = ptr_addr;
    path: NonNull<u8> => addr3 = ptr_addr;
    len: u32 => len;
}

prep! {
    /// Write an extended attribute by path.
    prep_setxattr => Setxattr { };
    name: NonNull<u8> => addr = ptr_addr;
    value: NonNull<u8> => addr2 = ptr_addr;
    path: NonNull<u8> => addr3 = ptr_addr;
    len: u32 => len;
    flags: u32 => op_flags;
}

prep! {
    /// Read an extended attribute by descriptor.
    prep_fgetxattr => Fgetxattr { };
    fd: i32 => fd;
    name: NonNull<u8> => addr = ptr_addr;
    value: NonNull<u8> => addr2 = ptr_addr;
    len: u32 => len;
}

prep! {
    /// Write an extended attribute by descriptor.
    prep_fsetxattr => Fsetxattr { };
    fd: i32 => fd;
    name: NonNull<u8> => addr = ptr_addr;
    value: NonNull<u8> => addr2 = ptr_addr;
    len: u32 => len;
    flags: u32 => op_flags;
}

// ---------------------------------------------------------------------------
// Sockets
// ---------------------------------------------------------------------------

prep! {
    /// Create a socket.
    prep_socket => Socket { };
    domain: i32 => fd;
    kind: u64 => addr2;
    protocol: u32 => len;
    flags: u32 => op_flags;
}

prep! {
    /// Create a socket into a direct descriptor slot.
    prep_socket_direct => Socket { };
    domain: i32 => fd;
    kind: u64 => addr2;
    protocol: u32 => len;
    flags: u32 => op_flags;
    slot: u32 => file_index = slot_encode;
}

prep! {
    /// Accept a connection, returning a file descriptor.
    prep_accept => Accept { };
    fd: i32 => fd;
    peer: Option<(NonNull<c_void>, NonNull<u32>)> => addr = peer_addr, addr2 = peer_len_addr;
    flags: u32 => op_flags;
}

prep! {
    /// Accept a connection into a direct descriptor slot.
    prep_accept_direct => Accept { };
    fd: i32 => fd;
    peer: Option<(NonNull<c_void>, NonNull<u32>)> => addr = peer_addr, addr2 = peer_len_addr;
    flags: u32 => op_flags;
    slot: u32 => file_index = slot_encode;
}

prep! {
    /// Multishot accept into direct descriptor slots. Presets `MULTISHOT`.
    prep_multishot_accept_direct => Accept { ioprio = sys::AcceptFlags::MULTISHOT.bits() };
    fd: i32 => fd;
    peer: Option<(NonNull<c_void>, NonNull<u32>)> => addr = peer_addr, addr2 = peer_len_addr;
    flags: u32 => op_flags;
    slot: u32 => file_index = slot_encode;
}

prep! {
    /// Connect a socket. The address length is passed by value.
    prep_connect => Connect { };
    fd: i32 => fd;
    addr: NonNull<c_void> => addr = ptr_addr;
    addrlen: u32 => addr2 = |v: u32| v as u64;
}

prep! {
    /// Bind a socket to an address.
    prep_bind => Bind { };
    fd: i32 => fd;
    addr: NonNull<c_void> => addr = ptr_addr;
    addrlen: u32 => addr2 = |v: u32| v as u64;
}

prep! {
    /// Mark a socket as listening.
    prep_listen => Listen { };
    fd: i32 => fd;
    backlog: u32 => len;
}

prep! {
    /// Shut down part of a full-duplex connection.
    prep_shutdown => Shutdown { };
    fd: i32 => fd;
    how: u32 => len;
}

prep! {
    /// Receive from a socket. The length is the buffer's.
    prep_recv => Recv { };
    fd: i32 => fd;
    buf: NonNull<[u8]> => addr = buf_addr, len = buf_len;
    flags: u32 => op_flags;
}

prep! {
    /// Multishot receive into provided buffers.
    prep_recv_multishot => Recv { ioprio = sys::RecvSendFlags::RECV_MULTISHOT.bits() };
    fd: i32 => fd;
    flags: u32 => op_flags;
    buf_group: u16 => buf_index;
}

prep! {
    /// Send on a socket. The length is the buffer's.
    prep_send => Send { };
    fd: i32 => fd;
    buf: NonNull<[u8]> => addr = buf_addr, len = buf_len;
    flags: u32 => op_flags;
}

prep! {
    /// Receive a message with ancillary data. Presets `len = 1`.
    prep_recvmsg => Recvmsg { len = 1 };
    fd: i32 => fd;
    msg: NonNull<libc::msghdr> => addr = ptr_addr;
    flags: u32 => op_flags;
}

prep! {
    /// Send a message with ancillary data. Presets `len = 1`.
    prep_sendmsg => Sendmsg { len = 1 };
    fd: i32 => fd;
    msg: NonNull<libc::msghdr> => addr = ptr_addr;
    flags: u32 => op_flags;
}

prep! {
    /// Zero-copy send. The length is the buffer's.
    prep_send_zc => SendZc { };
    fd: i32 => fd;
    buf: NonNull<[u8]> => addr = buf_addr, len = buf_len;
    flags: u32 => op_flags;
}

prep! {
    /// Zero-copy `sendmsg`. Presets `len = 1`.
    prep_sendmsg_zc => SendmsgZc { len = 1 };
    fd: i32 => fd;
    msg: NonNull<libc::msghdr> => addr = ptr_addr;
    flags: u32 => op_flags;
}

prep! {
    /// Zero-copy receive into a registered zcrx area.
    prep_recv_zc => RecvZc { };
    fd: i32 => fd;
    nbytes: u32 => len;
    flags: u32 => op_flags;
    ifq_idx: u32 => file_index;
}

// ---------------------------------------------------------------------------
// Polling, timers and cancellation
// ---------------------------------------------------------------------------

prep! {
    /// Wait for readiness on a descriptor.
    prep_poll_add => PollAdd { };
    fd: i32 => fd;
    events: u32 => op_flags;
}

prep! {
    /// Multishot poll: one completion per readiness edge.
    prep_poll_multishot => PollAdd { len = sys::PollFlags::ADD_MULTI.bits() };
    fd: i32 => fd;
    events: u32 => op_flags;
}

prep! {
    /// Remove a pending poll, named by its `user_data`.
    prep_poll_remove => PollRemove { fd = -1 };
    target: u64 => addr;
}

prep! {
    /// Update a pending poll's `user_data` and/or event mask.
    prep_poll_update => PollRemove { fd = -1 };
    target: u64 => addr;
    new_user_data: u64 => addr2;
    events: u32 => op_flags;
    flags: sys::PollFlags => len = |v: sys::PollFlags| v.bits();
}

prep! {
    /// Wait for a duration, or for `count` completions. Presets `len = 1`.
    prep_timeout => Timeout { len = 1, fd = -1 };
    ts: NonNull<sys::Timespec> => addr = ptr_addr;
    count: u32 => addr2 = |v: u32| v as u64;
    flags: sys::TimeoutFlags => op_flags = |v: sys::TimeoutFlags| v.bits();
}

prep! {
    /// Remove a pending timeout, named by its `user_data`.
    prep_timeout_remove => TimeoutRemove { fd = -1 };
    target: u64 => addr;
    flags: sys::TimeoutFlags => op_flags = |v: sys::TimeoutFlags| v.bits();
}

prep! {
    /// Update a pending timeout's deadline.
    prep_timeout_update => TimeoutRemove { fd = -1 };
    target: u64 => addr;
    ts: Option<NonNull<sys::Timespec>> => addr2 = optptr_addr;
    flags: sys::TimeoutFlags => op_flags = |v: sys::TimeoutFlags| v.bits();
}

prep! {
    /// Cancel the previous linked operation if it outlives the timeout.
    /// Presets `len = 1`; must follow an SQE carrying `IO_LINK`.
    prep_link_timeout => LinkTimeout { len = 1, fd = -1 };
    ts: NonNull<sys::Timespec> => addr = ptr_addr;
    flags: sys::TimeoutFlags => op_flags = |v: sys::TimeoutFlags| v.bits();
}

prep! {
    /// Cancel a pending operation, named by its `user_data`.
    prep_cancel => AsyncCancel { fd = -1 };
    target: u64 => addr;
    flags: sys::AsyncCancelFlags => op_flags = |v: sys::AsyncCancelFlags| v.bits();
}

prep! {
    /// Cancel pending operations on a descriptor.
    prep_cancel_fd => AsyncCancel { };
    fd: i32 => fd;
    flags: sys::AsyncCancelFlags => op_flags = |v: sys::AsyncCancelFlags| v.bits();
}

// ---------------------------------------------------------------------------
// Futexes
// ---------------------------------------------------------------------------

prep! {
    /// Wait on a futex.
    prep_futex_wait => FutexWait { };
    futex: NonNull<u32> => addr = ptr_addr;
    val: u64 => addr2;
    mask: u64 => addr3;
    futex_flags: i32 => fd;
    flags: u32 => op_flags;
}

prep! {
    /// Wake waiters on a futex.
    prep_futex_wake => FutexWake { };
    futex: NonNull<u32> => addr = ptr_addr;
    val: u64 => addr2;
    mask: u64 => addr3;
    futex_flags: i32 => fd;
    flags: u32 => op_flags;
}

prep! {
    /// Wait on any of several futexes.
    prep_futex_waitv => FutexWaitv { };
    futexv: NonNull<c_void> => addr = ptr_addr;
    nr_futex: u32 => len;
    flags: u32 => op_flags;
}

// ---------------------------------------------------------------------------
// Process and epoll
// ---------------------------------------------------------------------------

prep! {
    /// Wait for a process to change state, as `waitid`.
    prep_waitid => Waitid { };
    id: i32 => fd;
    idtype: u32 => len;
    infop: NonNull<c_void> => addr2 = ptr_addr;
    options: u32 => file_index;
    flags: u32 => op_flags;
}

prep! {
    /// Modify an epoll set, as `epoll_ctl`.
    prep_epoll_ctl => EpollCtl { };
    epfd: i32 => fd;
    target_fd: u64 => addr2;
    op: u32 => len;
    event: NonNull<libc::epoll_event> => addr = ptr_addr;
}

prep! {
    /// Wait on an epoll set.
    prep_epoll_wait => EpollWait { };
    epfd: i32 => fd;
    events: NonNull<libc::epoll_event> => addr = ptr_addr;
    maxevents: u32 => len;
    flags: u32 => op_flags;
}

// ---------------------------------------------------------------------------
// Splicing
// ---------------------------------------------------------------------------

prep! {
    /// Move data between descriptors without copying through userspace.
    prep_splice => Splice { };
    fd_out: i32 => fd;
    off_out: u64 => addr2;
    fd_in: i32 => file_index = |v: i32| v as u32;
    off_in: u64 => addr;
    nbytes: u32 => len;
    flags: u32 => op_flags;
}

prep! {
    /// Duplicate pipe contents, as `tee`.
    prep_tee => Tee { };
    fd_out: i32 => fd;
    fd_in: i32 => file_index = |v: i32| v as u32;
    nbytes: u32 => len;
    flags: u32 => op_flags;
}

// ---------------------------------------------------------------------------
// Ring resources
// ---------------------------------------------------------------------------

prep! {
    /// Hand a block of buffers to the kernel, the pre-buffer-ring interface.
    prep_provide_buffers => ProvideBuffers { };
    addr: NonNull<u8> => addr = ptr_addr;
    buf_len: u32 => len;
    nr: i32 => fd;
    bgid: u16 => buf_index;
    bid: u64 => addr2;
}

prep! {
    /// Take buffers back from a group.
    prep_remove_buffers => RemoveBuffers { };
    nr: i32 => fd;
    bgid: u16 => buf_index;
}

prep! {
    /// Update the registered file table from inside the ring.
    prep_files_update => FilesUpdate { fd = -1 };
    fds: NonNull<i32> => addr = ptr_addr;
    nr_fds: u32 => len;
    offset: u64 => addr2;
}

prep! {
    /// Turn a direct descriptor back into a regular file descriptor.
    /// Presets `FIXED_FILE`.
    prep_fixed_fd_install => FixedFdInstall { flags = sys::SqeFlags::FIXED_FILE };
    fd: i32 => fd;
    install_flags: sys::InstallFdFlags => op_flags = |v: sys::InstallFdFlags| v.bits();
}

prep! {
    /// Post a data completion to another ring.
    prep_msg_ring => MsgRing { addr = sys::MsgRingOp::Data as u64 };
    fd: i32 => fd;
    result: u32 => len;
    data: u64 => addr2;
    flags: sys::MsgRingFlags => op_flags = |v: sys::MsgRingFlags| v.bits();
}

prep! {
    /// Send a descriptor to another ring, into a direct slot.
    prep_msg_ring_fd => MsgRing { addr = sys::MsgRingOp::SendFd as u64 };
    fd: i32 => fd;
    source_fd: u64 => addr3;
    data: u64 => addr2;
    slot: u32 => file_index = slot_encode;
}

// ---------------------------------------------------------------------------
// Passthrough commands
// ---------------------------------------------------------------------------

prep! {
    /// A file-specific passthrough command with no inline payload.
    prep_uring_cmd => UringCmd { };
    fd: i32 => fd;
    cmd_op: u32 => addr2 = |v: u32| v as u64;
    flags: sys::UringCmdFlags => op_flags = |v: sys::UringCmdFlags| v.bits();
}

prep! {
    /// A passthrough command carrying the full 80-byte payload (two slots).
    prep_uring_cmd128 => UringCmd128 { };
    fd: i32 => fd;
    cmd_op: u32 => addr2 = |v: u32| v as u64;
    flags: sys::UringCmdFlags => op_flags = |v: sys::UringCmdFlags| v.bits();
}

prep! {
    /// `getsockopt`/`setsockopt` over a socket, `(level, optname)` packed into
    /// one word.
    prep_cmd_sock => UringCmd { };
    cmd_op: u32 => addr2 = |v: u32| v as u64;
    fd: i32 => fd;
    sockopt: (u32, u32) => addr = |v: (u32, u32)| v.0 as u64 | ((v.1 as u64) << 32);
    optval: u64 => addr3;
    optlen: u32 => file_index;
}

prep! {
    /// `getsockname`/`getpeername` over a socket. `peer` is 0 for this end, 1
    /// for the far end.
    prep_cmd_getsockname => UringCmd { addr2 = sys::SocketOp::Getsockname as u64 };
    fd: i32 => fd;
    name: NonNull<c_void> => addr = ptr_addr;
    namelen: NonNull<u32> => addr3 = ptr_addr;
    peer: u32 => file_index;
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::io_uring::ring::Ring;
    use crate::io_uring::syscall;

    /// Run `prep` against a fresh zeroed SQE and hand it back.
    fn built(prep: impl FnOnce(Slot<'_>)) -> sys::Sqe {
        let mut sqe = sys::Sqe::ZEROED;
        // SAFETY: `sqe` outlives the call.
        prep(unsafe { Slot::from_raw(&mut sqe) });
        sqe
    }

    fn buf_of(v: &mut [u8]) -> NonNull<[u8]> {
        NonNull::from(v)
    }

    /// The encoded SQE must land where `io_uring_prep_read` puts it: opcode, fd,
    /// addr, len, off — and nothing else touched.
    #[test]
    fn read_encodes_like_prep_rw() {
        let mut store = [0u8; 64];
        let buf = buf_of(&mut store);
        let sqe = built(|s| prep_read(s, 5, buf, 9, 0));

        assert_eq!(sqe.opcode, sys::Opcode::Read);
        assert_eq!(sqe.fd, 5);
        assert_eq!(sqe.addr, buf.cast::<u8>().as_ptr() as u64);
        assert_eq!(sqe.len, 64);
        assert_eq!(sqe.addr2, 9);
        assert_eq!(
            (
                sqe.ioprio,
                sqe.op_flags,
                sqe.buf_index,
                sqe.file_index,
                sqe.addr3,
                sqe.addr3_hi,
                sqe.user_data,
                sqe.flags.bits(),
            ),
            (0, 0, 0, 0, 0, 0, 0, 0)
        );
    }

    /// Presets the kernel insists on, in one place so a regression anywhere is
    /// visible.
    #[test]
    fn kernel_required_presets() {
        let ts = NonNull::dangling();
        assert_eq!(
            built(|s| prep_timeout(s, ts, 0, sys::TimeoutFlags::empty())).len,
            1
        );
        assert_eq!(
            built(|s| prep_timeout(s, ts, 0, sys::TimeoutFlags::empty())).fd,
            -1
        );
        assert_eq!(
            built(|s| prep_link_timeout(s, ts, sys::TimeoutFlags::empty())).len,
            1
        );

        let msg = NonNull::dangling();
        assert_eq!(built(|s| prep_recvmsg(s, 0, msg, 0)).len, 1);
        assert_eq!(built(|s| prep_sendmsg(s, 0, msg, 0)).len, 1);
        assert_eq!(built(|s| prep_sendmsg_zc(s, 0, msg, 0)).len, 1);

        let how = NonNull::dangling();
        let path = NonNull::dangling();
        assert_eq!(built(|s| prep_openat2(s, 0, path, how)).len, 24);

        assert_eq!(
            built(|s| prep_read_multishot(s, 0, 0, 0, 0)).flags,
            sys::SqeFlags::BUFFER_SELECT
        );
        assert_eq!(
            built(|s| prep_fixed_fd_install(s, 0, sys::InstallFdFlags::empty())).flags,
            sys::SqeFlags::FIXED_FILE
        );
    }

    /// Output slots go on the wire 1-based; the allocate sentinel passes
    /// through untouched.
    #[test]
    fn slots_are_encoded_one_based() {
        let path = NonNull::dangling();
        assert_eq!(
            built(|s| prep_openat_direct(s, 0, path, 0, 0, 0)).file_index,
            1
        );
        assert_eq!(
            built(|s| prep_openat_direct(s, 0, path, 0, 0, 41)).file_index,
            42
        );
        assert_eq!(
            built(|s| prep_openat_direct(s, 0, path, 0, 0, sys::FILE_INDEX_ALLOC)).file_index,
            sys::FILE_INDEX_ALLOC
        );
    }

    /// `None` must leave both wire fields zero, so the kernel never gets a
    /// length pointer without an address.
    #[test]
    fn accept_peer_is_all_or_nothing() {
        let none = built(|s| prep_accept(s, 3, None, 0));
        assert_eq!((none.addr, none.addr2), (0, 0));

        let mut sa = [0u8; 128];
        let mut salen = sa.len() as u32;
        let peer = (
            NonNull::from(&mut sa).cast::<c_void>(),
            NonNull::from(&mut salen),
        );
        let set = built(|s| prep_accept(s, 3, Some(peer), 0));
        assert_eq!(set.addr, peer.0.as_ptr() as u64);
        assert_eq!(set.addr2, peer.1.as_ptr() as u64);
    }

    /// `Splice` spreads two descriptors and two offsets across four different
    /// union slots — the easiest encoding to get wrong.
    #[test]
    fn splice_encodes_both_ends() {
        let sqe = built(|s| prep_splice(s, 4, 100, 5, 200, 4096, sys::SPLICE_F_FD_IN_FIXED));
        assert_eq!(sqe.fd, 4, "fd_out lands in fd");
        assert_eq!(sqe.addr2, 100, "off_out lands in addr2");
        assert_eq!(sqe.file_index, 5, "fd_in lands in the splice_fd_in slot");
        assert_eq!(sqe.addr, 200, "off_in lands in the splice_off_in slot");
        assert_eq!(sqe.len, 4096);
        assert_eq!(sqe.op_flags, sys::SPLICE_F_FD_IN_FIXED);
    }

    /// The `GETSOCKNAME` fields land where the kernel reads them, and the words
    /// it insists stay zero — `ioprio`, the high half of `cmd_op`, `len` — are
    /// not written on the way.
    #[test]
    fn uring_cmd_lays_out_a_getsockname() {
        let mut storage = [0u8; 128];
        let mut len = 128u32;
        let name = NonNull::new(storage.as_mut_ptr()).unwrap().cast::<c_void>();
        let namelen = NonNull::new(&raw mut len).unwrap();

        let sqe = built(|s| prep_cmd_getsockname(s, 3, name, namelen, 1));

        assert_eq!(sqe.addr, name.as_ptr() as u64, "the address buffer");
        assert_eq!(sqe.addr3, namelen.as_ptr() as u64, "the in/out length");
        assert_eq!(sqe.file_index, 1, "1 asks for the peer's address");
        assert_eq!(sqe.addr2, sys::SocketOp::Getsockname as u64);
        assert_eq!(
            sqe.addr2 >> 32,
            0,
            "__pad1 shares this word and must stay 0"
        );
        assert_eq!((sqe.ioprio, sqe.len, sqe.op_flags), (0, 0, 0));
    }

    /// `prep_cmd_sock` packs level and optname into the two halves of one word.
    #[test]
    fn cmd_sock_packs_the_sockopt_pair() {
        let sqe = built(|s| prep_cmd_sock(s, 3, 0, (1, 2), 0, 0));
        assert_eq!(sqe.addr, 1 | (2u64 << 32));
        assert_eq!(sqe.addr2, 3);
    }

    /// Every `prep_` used to build a real connection encodes its own opcode.
    #[test]
    fn representative_opcodes() {
        let p = NonNull::dangling();
        let mut b = [0u8; 4];
        let buf = buf_of(&mut b);
        assert_eq!(built(prep_nop).opcode, sys::Opcode::Nop);
        assert_eq!(
            built(|s| prep_read(s, 0, buf, 0, 0)).opcode,
            sys::Opcode::Read
        );
        assert_eq!(
            built(|s| prep_write(s, 0, buf, 0, 0)).opcode,
            sys::Opcode::Write
        );
        assert_eq!(built(|s| prep_recv(s, 0, buf, 0)).opcode, sys::Opcode::Recv);
        assert_eq!(built(|s| prep_send(s, 0, buf, 0)).opcode, sys::Opcode::Send);
        assert_eq!(built(|s| prep_close(s, 0)).opcode, sys::Opcode::Close);
        assert_eq!(
            built(|s| prep_close_direct(s, 0)).opcode,
            sys::Opcode::Close
        );
        assert_eq!(
            built(|s| prep_socket_direct(s, 0, 0, 0, 0, 0)).opcode,
            sys::Opcode::Socket
        );
        assert_eq!(
            built(|s| prep_connect(s, 0, p, 0)).opcode,
            sys::Opcode::Connect
        );
        assert_eq!(built(|s| prep_bind(s, 0, p, 0)).opcode, sys::Opcode::Bind);
        assert_eq!(built(|s| prep_listen(s, 0, 0)).opcode, sys::Opcode::Listen);
        assert_eq!(
            built(|s| prep_shutdown(s, 0, 0)).opcode,
            sys::Opcode::Shutdown
        );
        assert_eq!(
            built(|s| prep_accept_direct(s, 0, None, 0, 0)).opcode,
            sys::Opcode::Accept
        );
        assert_eq!(
            built(|s| prep_cancel(s, 0, sys::AsyncCancelFlags::empty())).opcode,
            sys::Opcode::AsyncCancel
        );
        assert_eq!(
            built(|s| prep_cmd_getsockname(s, 0, p.cast(), p.cast(), 0)).opcode,
            sys::Opcode::UringCmd
        );
    }

    /// A `Slot` that is dropped without being filled panics.
    #[test]
    #[should_panic(expected = "never filled")]
    fn an_unfilled_slot_panics() {
        let mut sqe = sys::Sqe::ZEROED;
        // SAFETY: `sqe` outlives the slot.
        let slot = unsafe { Slot::from_raw(&mut sqe) };
        drop(slot);
    }

    /// Submit one operation and return its completion result.
    fn run(ring: &Ring, prep: impl FnOnce(Slot<'_>)) -> i32 {
        let sq = ring.sq();
        let tail = sq.tail();
        // SAFETY: the ring is idle, so this slot is ours to write.
        prep(unsafe { Slot::from_raw(sq.sqe(tail)) });
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
        // SAFETY: head is below the tail we just observed.
        let res = unsafe { &*cq.cqe(cq.head()) }.res;
        cq.advance(1);
        res
    }

    /// Drive real I/O: write a message into a pipe, read it back, close both.
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
        let obuf = buf_of(&mut out);
        let n = run(&ring, |s| prep_write(s, wr, obuf, u64::MAX, 0));
        assert_eq!(n, MSG.len() as i32, "short write: {n}");

        let mut back = [0u8; 32];
        let bbuf = buf_of(&mut back);
        let n = run(&ring, |s| prep_read(s, rd, bbuf, u64::MAX, 0));
        assert_eq!(n, MSG.len() as i32, "short read: {n}");
        assert_eq!(back[..MSG.len()], MSG);

        assert_eq!(run(&ring, |s| prep_close(s, wr)), 0);
        assert_eq!(run(&ring, |s| prep_close(s, rd)), 0);
    }

    /// A `Nop` with an injected result comes back carrying it.
    #[test]
    #[cfg_attr(miri, ignore = "issues real syscalls and mmaps a ring")]
    fn nop_injects_its_result() {
        let mut params = sys::Params::default();
        let ring = Ring::with_params(8, &mut params).expect("Ring::with_params");
        assert_eq!(run(&ring, prep_nop), 0);
        assert_eq!(run(&ring, |s| prep_nop_inject(s, 42)), 42);
    }
}
