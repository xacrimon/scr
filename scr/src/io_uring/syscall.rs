//! Thin wrappers over the syscalls needed to drive a ring.
//!
//! These are a direct translation of the kernel ABI onto the [`sys`] types: no
//! state, no caching, no ring bookkeeping. Errors come back as a raw [`Errno`]
//! rather than `io::Error` so the hot paths can match on `EAGAIN`/`EINTR`/
//! `ETIME` without going through an allocation-shaped error type.
//!
//! Dispatch goes through `libc::syscall`, which reports failure as `-1` plus
//! `errno`; every wrapper converts that back to the negative-errno convention
//! the io_uring ABI is otherwise written in.

#![allow(dead_code)]

use std::ffi::c_void;
use std::ptr::NonNull;
use std::{io, ptr};

use super::sys;

#[cfg(not(all(
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
)))]
compile_error!("the io_uring syscall layer is only supported on linux x86_64 and aarch64");

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// A raw `errno` value.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Errno(pub i32);

impl Errno {
    /// Ring is full, or a non-blocking op would block. On `enter`, means no
    /// completions were ready.
    pub const AGAIN: Errno = Errno(libc::EAGAIN);
    /// Interrupted by a signal; the caller should generally retry.
    pub const INTR: Errno = Errno(libc::EINTR);
    /// The kernel is out of resources for this submission; retry after reaping.
    pub const BUSY: Errno = Errno(libc::EBUSY);
    /// A `Timeout` expired without its count being reached.
    pub const TIME: Errno = Errno(libc::ETIME);
    /// The ring is being torn down.
    pub const NXIO: Errno = Errno(libc::ENXIO);
    /// Operation cancelled.
    pub const CANCELED: Errno = Errno(libc::ECANCELED);
    pub const INVAL: Errno = Errno(libc::EINVAL);
    pub const FAULT: Errno = Errno(libc::EFAULT);
    pub const NOMEM: Errno = Errno(libc::ENOMEM);
    pub const NOSYS: Errno = Errno(libc::ENOSYS);
    /// The kernel does not support this opcode or flag.
    pub const OPNOTSUPP: Errno = Errno(libc::EOPNOTSUPP);
    /// No provided buffer was available. Arrives in `cqe.res`, never from a
    /// syscall; on a multishot operation it is terminal.
    pub const NOBUFS: Errno = Errno(libc::ENOBUFS);

    /// The errno most recently set by a failing libc call.
    fn last() -> Errno {
        let raw = unsafe { libc::__errno_location().read() };
        Errno(raw)
    }

    pub fn raw(self) -> i32 {
        self.0
    }
}

impl From<Errno> for io::Error {
    fn from(e: Errno) -> io::Error {
        io::Error::from_raw_os_error(e.0)
    }
}

impl std::fmt::Debug for Errno {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Errno({}: {})", self.0, io::Error::from(*self))
    }
}

impl std::fmt::Display for Errno {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        io::Error::from(*self).fmt(f)
    }
}

impl std::error::Error for Errno {}

macro_rules! libc_try {
    ($i:ident) => { libc_try!($i, < 0) };
    ($i:ident, $($cmp_rhs:tt)*) => { if $i $($cmp_rhs)* { return Err(Errno::last()) } };
}

/// `io_uring_setup(2)`.
///
/// `params` is updated in place with the negotiated entry counts, the
/// [`sys::Features`] the kernel supports, and the ring offsets needed to map it.
///
/// Returns a file descriptor, *except* under [`sys::SetupFlags::REGISTERED_FD_ONLY`],
/// where the return is an index into the ring's own registered-fd table and must
/// not be passed to `close`.
pub fn io_uring_setup(entries: u32, params: &mut sys::Params) -> Result<u32, Errno> {
    // SAFETY: `params` is a valid, uniquely borrowed `Params` for the call.
    let v = unsafe {
        libc::syscall(
            libc::SYS_io_uring_setup,
            entries as libc::c_long,
            params as *mut sys::Params as libc::c_long,
        )
    };

    libc_try!(v);
    Ok(v as u32)
}

/// `io_uring_enter(2)`, submitting `to_submit` entries and optionally waiting
/// for `min_complete` completions. Returns the number of SQEs consumed.
///
/// `arg` depends on `flags`: null for a plain enter, a `*const libc::sigset_t`
/// by default, a [`sys::GeteventsArg`] under [`sys::EnterFlags::EXT_ARG`], or an
/// index into a registered wait region under [`sys::EnterFlags::EXT_ARG_REG`].
/// `argsz` is the size of the pointee, or the index for `EXT_ARG_REG`.
///
/// # Safety
///
/// The caller must ensure that `arg`/`argsz` describe a valid argument for
/// `flags`, and that every submitted SQE is well formed: the kernel will
/// dereference the addresses in them and read from or write to the registered
/// buffers and mapped rings for as long as those operations are in flight.
pub unsafe fn io_uring_enter(
    fd: u32,
    to_submit: u32,
    min_complete: u32,
    flags: sys::EnterFlags,
    arg: *const c_void,
    argsz: usize,
) -> Result<u32, Errno> {
    // SAFETY: forwarded to the caller's contract.
    let v = unsafe {
        libc::syscall(
            libc::SYS_io_uring_enter,
            fd as libc::c_long,
            to_submit as libc::c_long,
            min_complete as libc::c_long,
            flags.bits() as libc::c_long,
            arg as libc::c_long,
            argsz as libc::c_long,
        )
    };

    libc_try!(v);
    Ok(v as u32)
}

/// `io_uring_register(2)`.
///
/// `op` is a [`sys::RegisterOp`], optionally OR'd with
/// [`sys::REGISTER_USE_REGISTERED_RING`] to pass a registered ring index in `fd`
/// instead of a file descriptor.
///
/// # Safety
///
/// `arg` and `nr_args` must match what `op` expects, and any memory registered
/// with the kernel must stay valid and unmoved until it is unregistered.
pub unsafe fn io_uring_register(
    fd: u32,
    op: u32,
    arg: *const c_void,
    nr_args: u32,
) -> Result<u32, Errno> {
    // SAFETY: forwarded to the caller's contract.
    let v = unsafe {
        libc::syscall(
            libc::SYS_io_uring_register,
            fd as libc::c_long,
            op as libc::c_long,
            arg as libc::c_long,
            nr_args as libc::c_long,
        )
    };

    libc_try!(v);
    Ok(v as u32)
}

/// `mmap(2)` wrapper with the protection and flags every io_uring ring segment uses.
///
/// `offset` must be one of [`sys::OFF_SQ_RING`], [`sys::OFF_CQ_RING`],
/// [`sys::OFF_SQES`], or [`sys::OFF_PBUF_RING`] OR'd with a buffer group id.
///
/// # Safety
///
/// As `fd` must be a live ring file descriptor and `len` must match
/// the segment size implied by the ring's [`sys::Params`].
pub unsafe fn map_ring(len: usize, fd: i32, offset: u64) -> Result<NonNull<c_void>, Errno> {
    let prot = libc::PROT_READ | libc::PROT_WRITE;
    let flags = libc::MAP_SHARED | libc::MAP_POPULATE;

    // SAFETY: forwarded to the caller's contract.
    let p = unsafe { libc::mmap(ptr::null_mut(), len, prot, flags, fd, offset as libc::off_t) };
    libc_try!(p, == libc::MAP_FAILED);

    // SAFETY: mmap returns either MAP_FAILED or a non-null mapping.
    Ok(unsafe { NonNull::new_unchecked(p) })
}

/// `munmap(2)`.
///
/// # Safety
///
/// `addr` and `len` must describe a mapping made by this process, and nothing
/// may reference it afterwards.
pub unsafe fn munmap(addr: NonNull<c_void>, len: usize) -> Result<(), Errno> {
    // SAFETY: forwarded to the caller's contract.
    let v = unsafe { libc::munmap(addr.as_ptr(), len) };
    libc_try!(v);
    Ok(())
}

/// `close(2)`.
///
/// # Safety
///
/// `fd` must be owned by the caller and not used again afterwards. Note that a
/// ring created with [`sys::SetupFlags::REGISTERED_FD_ONLY`] yields a registered
/// index, not a descriptor, and must not be passed here.
pub unsafe fn close(fd: i32) -> Result<(), Errno> {
    // SAFETY: forwarded to the caller's contract.
    let v = unsafe { libc::close(fd) };
    libc_try!(v);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The kernel reports back a feature set and honours `Opcode::LAST`, which
    /// only works if `Features` and the probe path agree with the header.
    #[test]
    #[cfg_attr(miri, ignore = "issues real syscalls and mmaps a ring")]
    fn setup_reports_features() {
        let mut p = sys::Params::default();
        let fd = io_uring_setup(4, &mut p).expect("io_uring_setup") as i32;
        assert!(
            p.features.contains(sys::Features::NODROP),
            "features = {:?}",
            p.features
        );
        unsafe { close(fd).unwrap() };
    }
}
