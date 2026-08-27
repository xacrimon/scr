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
use std::io;
use std::ptr::NonNull;

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

    /// The errno most recently set by a failing libc call.
    fn last() -> Errno {
        Errno(io::Error::last_os_error().raw_os_error().unwrap_or(0))
    }

    pub const fn raw(self) -> i32 {
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

/// Map a `libc::syscall` return value onto the negative-errno convention.
fn ret(r: libc::c_long) -> Result<u32, Errno> {
    if r < 0 {
        Err(Errno::last())
    } else {
        Ok(r as u32)
    }
}

// ---------------------------------------------------------------------------
// io_uring syscalls
// ---------------------------------------------------------------------------

/// `io_uring_setup(2)`.
///
/// `params` is updated in place with the negotiated entry counts, the
/// [`sys::Features`] the kernel supports, and the ring offsets needed to map it.
///
/// Returns a file descriptor, *except* under [`sys::SetupFlags::REGISTERED_FD_ONLY`],
/// where the return is an index into the ring's own registered-fd table and must
/// not be passed to `close`.
pub fn setup(entries: u32, params: &mut sys::Params) -> Result<u32, Errno> {
    // SAFETY: `params` is a valid, uniquely borrowed `Params` for the call.
    ret(unsafe {
        libc::syscall(
            libc::SYS_io_uring_setup,
            entries as libc::c_long,
            params as *mut sys::Params as libc::c_long,
        )
    })
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
pub unsafe fn enter(
    fd: u32,
    to_submit: u32,
    min_complete: u32,
    flags: sys::EnterFlags,
    arg: *const c_void,
    argsz: usize,
) -> Result<u32, Errno> {
    // SAFETY: forwarded to the caller's contract.
    ret(unsafe {
        libc::syscall(
            libc::SYS_io_uring_enter,
            fd as libc::c_long,
            to_submit as libc::c_long,
            min_complete as libc::c_long,
            flags.bits() as libc::c_long,
            arg as libc::c_long,
            argsz as libc::c_long,
        )
    })
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
pub unsafe fn register(fd: u32, op: u32, arg: *const c_void, nr_args: u32) -> Result<u32, Errno> {
    // SAFETY: forwarded to the caller's contract.
    ret(unsafe {
        libc::syscall(
            libc::SYS_io_uring_register,
            fd as libc::c_long,
            op as libc::c_long,
            arg as libc::c_long,
            nr_args as libc::c_long,
        )
    })
}

// ---------------------------------------------------------------------------
// Memory
// ---------------------------------------------------------------------------

/// `mmap(2)`.
///
/// For ring segments this is `PROT_READ | PROT_WRITE` with
/// `MAP_SHARED | MAP_POPULATE`, `fd` the ring fd, and `offset` one of the
/// `sys::OFF_*` constants. See [`map_ring`].
///
/// # Safety
///
/// Standard `mmap` contract: the caller owns the returned mapping and must not
/// unmap or alias it in a way that invalidates outstanding references.
pub unsafe fn mmap(
    addr: *mut c_void,
    len: usize,
    prot: i32,
    flags: i32,
    fd: i32,
    offset: u64,
) -> Result<NonNull<c_void>, Errno> {
    // SAFETY: forwarded to the caller's contract.
    let p = unsafe { libc::mmap(addr, len, prot, flags, fd, offset as libc::off_t) };
    if p == libc::MAP_FAILED {
        return Err(Errno::last());
    }
    // SAFETY: mmap returns either MAP_FAILED, handled above, or a non-null
    // page-aligned mapping.
    Ok(unsafe { NonNull::new_unchecked(p) })
}

/// [`mmap`] with the protection and flags every io_uring ring segment uses.
///
/// `offset` must be one of [`sys::OFF_SQ_RING`], [`sys::OFF_CQ_RING`],
/// [`sys::OFF_SQES`], or [`sys::OFF_PBUF_RING`] OR'd with a buffer group id.
///
/// # Safety
///
/// As [`mmap`]. `fd` must be a live ring file descriptor and `len` must match
/// the segment size implied by the ring's [`sys::Params`].
pub unsafe fn map_ring(len: usize, fd: i32, offset: u64) -> Result<NonNull<c_void>, Errno> {
    // SAFETY: forwarded to the caller's contract.
    unsafe {
        mmap(
            std::ptr::null_mut(),
            len,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_SHARED | libc::MAP_POPULATE,
            fd,
            offset,
        )
    }
}

/// `munmap(2)`.
///
/// # Safety
///
/// `addr` and `len` must describe a mapping made by this process, and nothing
/// may reference it afterwards.
pub unsafe fn munmap(addr: NonNull<c_void>, len: usize) -> Result<(), Errno> {
    // SAFETY: forwarded to the caller's contract.
    if unsafe { libc::munmap(addr.as_ptr(), len) } < 0 {
        return Err(Errno::last());
    }
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
    if unsafe { libc::close(fd) } < 0 {
        return Err(Errno::last());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    /// End-to-end smoke test: set up a ring, map it, submit a `Nop`, reap the
    /// completion. Exercises the `Params`/`Sqe`/`Cqe` layouts and the ring
    /// offsets against the running kernel.
    #[test]
    #[cfg_attr(miri, ignore = "issues real syscalls and mmaps a ring")]
    fn nop_round_trip() {
        const ENTRIES: u32 = 8;
        const USER_DATA: u64 = 0xdead_beef_1234_5678;

        let mut p = sys::Params::default();
        let fd = setup(ENTRIES, &mut p).expect("io_uring_setup") as i32;

        assert!(p.sq_entries >= ENTRIES, "sq_entries = {}", p.sq_entries);
        assert!(p.cq_entries >= ENTRIES, "cq_entries = {}", p.cq_entries);

        // These are byte offsets into the mapped ring region, chosen by the
        // kernel. We can't predict them, but a zeroed `cqes`/`array` would mean
        // `Params` is misaligned and the kernel wrote into the wrong fields.
        assert_ne!(p.sq_off.array, 0, "sq_off not filled in");
        assert_ne!(p.cq_off.cqes, 0, "cq_off not filled in");

        let mut sq_sz = p.sq_off.array as usize + p.sq_entries as usize * 4;
        let mut cq_sz =
            p.cq_off.cqes as usize + p.cq_entries as usize * std::mem::size_of::<sys::Cqe>();
        let single = p.features.contains(sys::Features::SINGLE_MMAP);
        if single {
            sq_sz = sq_sz.max(cq_sz);
            cq_sz = sq_sz;
        }

        unsafe {
            let sq = map_ring(sq_sz, fd, sys::OFF_SQ_RING).expect("map sq");
            let cq = if single {
                sq
            } else {
                map_ring(cq_sz, fd, sys::OFF_CQ_RING).expect("map cq")
            };
            let sqes_sz = p.sq_entries as usize * std::mem::size_of::<sys::Sqe>();
            let sqes = map_ring(sqes_sz, fd, sys::OFF_SQES).expect("map sqes");

            let at = |base: NonNull<c_void>, off: u32| -> *const AtomicU32 {
                base.as_ptr().byte_add(off as usize).cast()
            };
            let sq_tail = at(sq, p.sq_off.tail);
            let sq_mask = (*at(sq, p.sq_off.ring_mask)).load(Ordering::Relaxed);
            let cq_head = at(cq, p.cq_off.head);
            let cq_tail = at(cq, p.cq_off.tail);
            let cq_mask = (*at(cq, p.cq_off.ring_mask)).load(Ordering::Relaxed);

            // Fill slot 0 with a Nop and point the SQ array at it.
            let tail = (*sq_tail).load(Ordering::Relaxed);
            let idx = tail & sq_mask;
            let sqe = sqes.as_ptr().cast::<sys::Sqe>().add(idx as usize);
            sqe.write({
                let mut s = sys::Sqe {
                    opcode: sys::Opcode::Nop,
                    ..sys::Sqe::ZEROED
                };

                s.user_data = USER_DATA;
                s
            });
            assert!(
                !p.flags.contains(sys::SetupFlags::NO_SQARRAY),
                "test assumes the SQ index array is present"
            );
            let array = sq.as_ptr().byte_add(p.sq_off.array as usize).cast::<u32>();
            array.add(idx as usize).write(idx);
            (*sq_tail).store(tail + 1, Ordering::Release);

            let submitted = enter(
                fd as u32,
                1,
                1,
                sys::EnterFlags::GETEVENTS,
                std::ptr::null(),
                0,
            )
            .expect("io_uring_enter");
            assert_eq!(submitted, 1, "kernel consumed {submitted} sqes");

            let head = (*cq_head).load(Ordering::Relaxed);
            assert_eq!(
                (*cq_tail).load(Ordering::Acquire),
                head + 1,
                "expected exactly one completion"
            );
            let cqe = &*cq
                .as_ptr()
                .byte_add(p.cq_off.cqes as usize)
                .cast::<sys::Cqe>()
                .add((head & cq_mask) as usize);

            assert_eq!(cqe.user_data, USER_DATA, "user_data did not round-trip");
            assert_eq!(cqe.res, 0, "nop failed: {}", Errno(-cqe.res));
            (*cq_head).store(head + 1, Ordering::Release);

            munmap(sqes, sqes_sz).unwrap();
            if !single {
                munmap(cq, cq_sz).unwrap();
            }
            munmap(sq, sq_sz).unwrap();
            close(fd).unwrap();
        }
    }

    /// The kernel reports back a feature set and honours `Opcode::LAST`, which
    /// only works if `Features` and the probe path agree with the header.
    #[test]
    #[cfg_attr(miri, ignore = "issues real syscalls and mmaps a ring")]
    fn setup_reports_features() {
        let mut p = sys::Params::default();
        let fd = setup(4, &mut p).expect("io_uring_setup") as i32;
        assert!(
            p.features.contains(sys::Features::NODROP),
            "features = {:?}",
            p.features
        );
        unsafe { close(fd).unwrap() };
    }
}
