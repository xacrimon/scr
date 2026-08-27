//! An owned io_uring ring and typed views over its two halves.
//!
//! `io_uring_setup` reports where each control word lives as a byte offset into
//! the mapped region rather than fixing a layout, because the kernel's ring
//! header has changed shape across versions. [`Ring::new`] resolves those
//! offsets once and then drops [`sys::Params`] — it is 120 bytes of setup-time
//! scaffolding, and only `features` and `flags` matter afterwards.
//!
//! [`SqRing`] and [`CqRing`] hold raw pointers into the mapping rather than
//! borrows. They are reachable only through [`Ring::sq`] and [`Ring::cq`], which
//! borrow the [`Ring`] that owns the mapping, so the pointers cannot outlive it.
//!
//! # Memory ordering
//!
//! The two tail stores are the only synchronisation points; everything else
//! rides on them.
//!
//! | field | ordering |
//! |---|---|
//! | [`SqRing::head`] | `Acquire` under SQPOLL, else `Relaxed` |
//! | [`SqRing::set_tail`] | `Release` under SQPOLL, else `Relaxed` |
//! | [`CqRing::tail`] | `Acquire` always |
//! | [`CqRing::advance`] | `Release` always |
//! | flags, dropped, overflow | `Relaxed` |
//!
//! Without SQPOLL the kernel only touches the SQ indices inside your
//! `io_uring_enter` call, which is an opaque call and a full barrier, so
//! `Relaxed` suffices. The CQ side needs real ordering in every mode because
//! completions are posted from interrupt context and other CPUs.
//!
//! The SQ index array is not covered by any of this: it is written once at
//! construction as an identity map and never touched again.

#![allow(dead_code)]

use std::ffi::c_void;
use std::ptr::NonNull;
use std::sync::atomic::{AtomicU32, Ordering};

use super::sys;
use super::syscall::{self, Errno};

/// Borrow the `AtomicU32` at `off` bytes into a mapped ring.
///
/// # Safety
///
/// `base + off` must be a live, 4-byte aligned word inside a mapping that
/// outlives the returned pointer's use.
unsafe fn word_at(base: NonNull<c_void>, off: u32) -> NonNull<AtomicU32> {
    // SAFETY: forwarded to the caller's contract. Ring offsets are always
    // 4-byte aligned and the base of an mmap is page aligned.
    unsafe { NonNull::new_unchecked(base.as_ptr().byte_add(off as usize).cast()) }
}

// ---------------------------------------------------------------------------
// Ring
// ---------------------------------------------------------------------------

/// An io_uring instance: the ring file descriptor and both mapped queues.
///
/// Dropping unmaps the rings and closes the descriptor.
#[derive(Debug)]
pub struct Ring {
    sq: SqRing,
    cq: CqRing,
    /// The descriptor from `io_uring_setup`, or `-1` when the ring was created
    /// with [`sys::SetupFlags::REGISTERED_FD_ONLY`] and no descriptor exists.
    ring_fd: i32,
    /// What to pass as the `fd` argument of `io_uring_enter`. Equal to
    /// [`Ring::ring_fd`] until the ring's own descriptor is registered with
    /// [`Ring::register_ring_fd`], after which it is a registered-ring index.
    enter_ring_fd: u32,
    /// Enter flags implied by the ring's state, to be OR'd into every
    /// `io_uring_enter`. Carries [`sys::EnterFlags::REGISTERED_RING`] once
    /// [`Ring::enter_ring_fd`] is an index rather than a descriptor.
    enter_flags: sys::EnterFlags,
    features: sys::Features,
    flags: sys::SetupFlags,
}

// SAFETY: a `Ring` owns its mappings and descriptor outright, and every field
// is either plain data or a pointer into those mappings. Moving the whole thing
// between threads hands over the entire ring at once.
//
// It is deliberately not `Sync`: two threads sharing one `&Ring` would race on
// the SQ tail, which no ordering in `SqRing` guards against — that is what
// `IORING_SETUP_SINGLE_ISSUER` exists to enforce on the kernel side.
unsafe impl Send for Ring {}

impl Ring {
    /// Create a ring, passing `params` to `io_uring_setup`.
    ///
    /// `params` is updated in place with what the kernel negotiated, but the
    /// [`Ring`] keeps only `features` and `flags`; the caller may inspect the
    /// rest and then discard it.
    pub fn with_params(entries: u32, params: &mut sys::Params) -> Result<Ring, Errno> {
        let fd = syscall::io_uring_setup(entries, params)?;
        let registered_only = params.flags.contains(sys::SetupFlags::REGISTERED_FD_ONLY);

        // SAFETY: `fd` names the ring `params` was just filled in for, and the
        // sizes below are derived from `params`. On any failure every mapping
        // made so far is undone before returning.
        let mapped = unsafe { Ring::map(fd as i32, params) };
        let (sq, cq) = match mapped {
            Ok(v) => v,
            Err(e) => {
                if !registered_only {
                    // SAFETY: `fd` is a live descriptor we own and have not
                    // handed out.
                    unsafe { syscall::close(fd as i32) }.ok();
                }
                return Err(e);
            }
        };

        Ok(Ring {
            sq,
            cq,
            ring_fd: if registered_only { -1 } else { fd as i32 },
            enter_ring_fd: fd,
            enter_flags: if registered_only {
                sys::EnterFlags::REGISTERED_RING
            } else {
                sys::EnterFlags::empty()
            },
            features: params.features,
            flags: params.flags,
        })
    }

    /// Map the three ring segments and resolve the offsets in `params`.
    ///
    /// # Safety
    ///
    /// `fd` must be the descriptor `params` was filled in by.
    unsafe fn map(fd: i32, params: &sys::Params) -> Result<(SqRing, CqRing), Errno> {
        let cqe_shift = params.flags.contains(sys::SetupFlags::CQE32) as u32;
        let sqe_shift = params.flags.contains(sys::SetupFlags::SQE128) as u32;

        // Matches `io_uring_mmap`: the SQ index array sits at the tail of the
        // SQ region, and the CQE array at the tail of the CQ region.
        //
        // Under `NO_SQARRAY` there is no index array and the kernel reports
        // `array = 0`, so that formula would size the SQ region at nothing.
        // Fall back to the header itself: the highest control word the kernel
        // placed, plus its own width.
        let off = &params.sq_off;
        let mut sq_sz = if params.flags.contains(sys::SetupFlags::NO_SQARRAY) {
            let last = [
                off.head,
                off.tail,
                off.ring_mask,
                off.ring_entries,
                off.flags,
                off.dropped,
            ]
            .into_iter()
            .max()
            .expect("the array is not empty");
            last as usize + size_of::<u32>()
        } else {
            off.array as usize + params.sq_entries as usize * 4
        };
        let mut cq_sz = params.cq_off.cqes as usize
            + ((params.cq_entries as usize * size_of::<sys::Cqe>()) << cqe_shift);
        let single = params.features.contains(sys::Features::SINGLE_MMAP);
        if single {
            sq_sz = sq_sz.max(cq_sz);
            cq_sz = sq_sz;
        }
        let sqes_sz = (params.sq_entries as usize * size_of::<sys::Sqe>()) << sqe_shift;

        // SAFETY: forwarded to the caller's contract; each offset is one of the
        // documented `IORING_OFF_*` values and each size is derived above.
        unsafe {
            let sq_ptr = syscall::map_ring(sq_sz, fd, sys::OFF_SQ_RING)?;
            let cq_ptr = if single {
                sq_ptr
            } else {
                match syscall::map_ring(cq_sz, fd, sys::OFF_CQ_RING) {
                    Ok(p) => p,
                    Err(e) => {
                        syscall::munmap(sq_ptr, sq_sz).ok();
                        return Err(e);
                    }
                }
            };
            let sqes = match syscall::map_ring(sqes_sz, fd, sys::OFF_SQES) {
                Ok(p) => p,
                Err(e) => {
                    if !single {
                        syscall::munmap(cq_ptr, cq_sz).ok();
                    }
                    syscall::munmap(sq_ptr, sq_sz).ok();
                    return Err(e);
                }
            };

            let sq = SqRing::new(sq_ptr, sq_sz, sqes, sqes_sz, params);
            // A shared mapping is owned by the SQ side, so the CQ records a
            // zero length and leaves it alone at teardown.
            let cq = CqRing::new(cq_ptr, if single { 0 } else { cq_sz }, params);
            Ok((sq, cq))
        }
    }

    /// The submission queue.
    pub fn sq(&self) -> &SqRing {
        &self.sq
    }

    /// The completion queue.
    pub fn cq(&self) -> &CqRing {
        &self.cq
    }

    /// What the kernel reported it supports.
    pub fn features(&self) -> sys::Features {
        self.features
    }

    /// The flags this ring was created with, as negotiated.
    pub fn flags(&self) -> sys::SetupFlags {
        self.flags
    }

    /// The ring's file descriptor, or `None` under
    /// [`sys::SetupFlags::REGISTERED_FD_ONLY`], where the ring has no
    /// descriptor and exists only as a registered index.
    pub fn fd(&self) -> Option<i32> {
        (self.ring_fd != -1).then_some(self.ring_fd)
    }

    /// The `fd` argument for `io_uring_enter`, to be paired with
    /// [`Ring::enter_flags`].
    pub fn enter_fd(&self) -> u32 {
        self.enter_ring_fd
    }

    /// Enter flags the ring's state requires. OR these into the flags for every
    /// `io_uring_enter`.
    pub fn enter_flags(&self) -> sys::EnterFlags {
        self.enter_flags
    }

    /// Whether completions are waiting that an `io_uring_enter` would deliver.
    ///
    /// When the completion ring fills, the kernel parks further completions in
    /// an internal list and raises [`sys::SqRingFlags::CQ_OVERFLOW`]; they only
    /// reach the ring on the next enter with
    /// [`sys::EnterFlags::GETEVENTS`]. [`sys::SqRingFlags::TASKRUN`] means
    /// pending task work would post more.
    ///
    /// Both live in the *submission* ring's flags even though they describe the
    /// completion side. An empty [`CqRing`] with this set is not an idle ring.
    ///
    /// # This is not a test for "are there completions"
    ///
    /// [`sys::SqRingFlags::TASKRUN`] is only ever raised by the kernel's
    /// *normal* task-work path, under [`sys::SetupFlags::TASKRUN_FLAG`]. A ring
    /// created with [`sys::SetupFlags::DEFER_TASKRUN`] takes the local path
    /// instead and never sets it, so this reports overflow alone and a `false`
    /// says nothing about work waiting to be run. There is no cheap userspace
    /// test in that mode: completions become visible only inside an
    /// `io_uring_enter` carrying [`sys::EnterFlags::GETEVENTS`].
    pub fn needs_flush(&self) -> bool {
        self.sq
            .flags()
            .intersects(sys::SqRingFlags::CQ_OVERFLOW | sys::SqRingFlags::TASKRUN)
    }

    /// Whether `io_uring_register` should address this ring by registered index
    /// rather than by descriptor.
    ///
    /// Registering the ring fd speeds up `io_uring_enter` on every kernel, but
    /// only kernels advertising [`sys::Features::REG_REG_RING`] also accept the
    /// index for `io_uring_register`. Under
    /// [`sys::SetupFlags::REGISTERED_FD_ONLY`] there is no descriptor, so the
    /// index is the only option.
    fn register_by_index(&self) -> bool {
        self.enter_flags.contains(sys::EnterFlags::REGISTERED_RING)
            && (self.ring_fd == -1 || self.features.contains(sys::Features::REG_REG_RING))
    }

    /// Issue `io_uring_register` against this ring, choosing the descriptor or
    /// the registered index as the kernel allows.
    ///
    /// # Safety
    ///
    /// As [`syscall::io_uring_register`]: `arg` and `nr_args` must match `op`, and any
    /// registered memory must stay valid until unregistered.
    pub unsafe fn register(
        &self,
        op: sys::RegisterOp,
        arg: *const c_void,
        nr_args: u32,
    ) -> Result<u32, Errno> {
        let (fd, op) = if self.register_by_index() {
            (
                self.enter_ring_fd,
                op as u32 | sys::REGISTER_USE_REGISTERED_RING,
            )
        } else {
            (self.ring_fd as u32, op as u32)
        };
        // SAFETY: forwarded to the caller's contract.
        unsafe { syscall::io_uring_register(fd, op, arg, nr_args) }
    }

    /// Register the ring's own descriptor with itself, so that subsequent
    /// `io_uring_enter` calls pass a small index instead of a file descriptor.
    ///
    /// This skips the `fdget`/`fdput` the kernel would otherwise do on every
    /// enter. After this returns, [`Ring::enter_fd`] is an index and
    /// [`Ring::enter_flags`] carries [`sys::EnterFlags::REGISTERED_RING`].
    ///
    /// Fails with [`Errno::INVAL`] if the ring fd is already registered.
    pub fn register_ring_fd(&mut self) -> Result<(), Errno> {
        if self.enter_flags.contains(sys::EnterFlags::REGISTERED_RING) {
            return Err(Errno(libc::EEXIST));
        }
        let mut up = sys::RsrcUpdate {
            offset: !0,
            resv: 0,
            data: self.ring_fd as u64,
        };
        // SAFETY: `up` is a live, correctly typed argument for RING_FDS, and
        // `nr_args` of 1 matches the single entry it describes.
        let n = unsafe {
            self.register(
                sys::RegisterOp::RingFds,
                std::ptr::from_mut(&mut up).cast(),
                1,
            )?
        };
        if n != 1 {
            return Err(Errno::INVAL);
        }
        self.enter_ring_fd = up.offset;
        self.enter_flags |= sys::EnterFlags::REGISTERED_RING;
        Ok(())
    }

    /// Register `nr` empty direct descriptor slots.
    ///
    /// The table's size is fixed here for the life of the registration: there is
    /// no register operation that grows it, and re-registering over a live table
    /// fails with `EBUSY`. Growing means [`Ring::unregister_files`] first, which
    /// drops every descriptor in it, so pick a capacity that covers the peak.
    ///
    /// `nr` counts against `RLIMIT_NOFILE` even though the slots are empty, so a
    /// table larger than the soft limit fails with `EMFILE`. liburing raises the
    /// limit and retries; this does not, because the limit is process-wide state
    /// a library has no business editing.
    ///
    /// See [`fixed::FixedFiles`](super::fixed::FixedFiles), which pairs this
    /// with a slot allocator.
    pub fn register_files_sparse(&self, nr: u32) -> Result<(), Errno> {
        let reg = sys::RsrcRegister {
            nr,
            flags: sys::RsrcRegisterFlags::SPARSE,
            ..Default::default()
        };
        // SAFETY: `reg` is a live, correctly typed argument for FILES2, and the
        // kernel takes its size in `nr_args` rather than an element count. It
        // borrows nothing: the table is allocated from `nr` alone.
        unsafe {
            self.register(
                sys::RegisterOp::Files2,
                std::ptr::from_ref(&reg).cast(),
                size_of::<sys::RsrcRegister>() as u32,
            )?
        };
        Ok(())
    }

    /// Register `fds` as the direct descriptor table, slot `i` naming `fds[i]`.
    ///
    /// A `-1` entry leaves the slot empty. As with [`Ring::register_files_sparse`]
    /// the length fixes the table size.
    pub fn register_files(&self, fds: &[i32]) -> Result<(), Errno> {
        // SAFETY: `fds` is live for the call and `nr_args` is its true length.
        // The kernel resolves every descriptor before returning and keeps no
        // reference to the array itself.
        unsafe {
            self.register(
                sys::RegisterOp::Files,
                fds.as_ptr().cast(),
                fds.len() as u32,
            )?
        };
        Ok(())
    }

    /// Install `fds` into consecutive slots starting at `offset`, returning how
    /// many were updated.
    ///
    /// A `-1` entry empties the slot; [`sys::REGISTER_FILES_SKIP`] leaves it as
    /// it was. Installing over an occupied slot replaces it silently, so the
    /// caller is responsible for knowing the slot is free.
    pub fn update_files(&self, offset: u32, fds: &[i32]) -> Result<u32, Errno> {
        let up = sys::RsrcUpdate {
            offset,
            resv: 0,
            data: fds.as_ptr() as u64,
        };
        // SAFETY: `up` points at `fds`, which outlives the call, and `nr_args`
        // matches its length. As with registration the kernel copies out during
        // the call.
        unsafe {
            self.register(
                sys::RegisterOp::FilesUpdate,
                std::ptr::from_ref(&up).cast(),
                fds.len() as u32,
            )
        }
    }

    /// Drop the direct descriptor table, closing every descriptor in it.
    ///
    /// Fails with [`Errno::NXIO`] if no table is registered.
    pub fn unregister_files(&self) -> Result<(), Errno> {
        // SAFETY: this operation takes no argument.
        unsafe { self.register(sys::RegisterOp::UnregisterFiles, std::ptr::null(), 0)? };
        Ok(())
    }

    /// Confine the kernel's own slot allocator to `[off, off + len)`.
    ///
    /// This bounds [`sys::FILE_INDEX_ALLOC`] only; an operation naming a slot
    /// outright may still target any slot in the table. Partitioning this way
    /// lets a multishot accept allocate for itself out of one region while the
    /// caller hands out slots from the other without the two colliding.
    ///
    /// The range is validated against the registered table, so it must be set
    /// after [`Ring::register_files_sparse`] and is undone by
    /// [`Ring::unregister_files`].
    pub fn set_file_alloc_range(&self, off: u32, len: u32) -> Result<(), Errno> {
        let range = sys::FileIndexRange {
            off,
            len,
            ..Default::default()
        };
        // SAFETY: `range` is a live, correctly typed argument for
        // FILE_ALLOC_RANGE, which takes no element count.
        unsafe {
            self.register(
                sys::RegisterOp::FileAllocRange,
                std::ptr::from_ref(&range).cast(),
                0,
            )?
        };
        Ok(())
    }
}

impl Drop for Ring {
    fn drop(&mut self) {
        // SAFETY: these mappings and this descriptor were created by
        // `Ring::map`/`syscall::io_uring_setup` and are owned solely by `self`, which is
        // being destroyed. `cq.ring_sz` is zero when the CQ shares the SQ's
        // mapping, so it is never unmapped twice.
        unsafe {
            syscall::munmap(self.sq.sqes.cast(), self.sq.sqes_sz).ok();
            syscall::munmap(self.sq.ring_ptr, self.sq.ring_sz).ok();
            if self.cq.ring_sz != 0 {
                syscall::munmap(self.cq.ring_ptr, self.cq.ring_sz).ok();
            }
            if self.ring_fd != -1 {
                syscall::close(self.ring_fd).ok();
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Submission ring
// ---------------------------------------------------------------------------

/// The submission queue: control words plus the SQE array.
///
/// Obtained from [`Ring::sq`]; the pointers within are valid only for as long
/// as the owning [`Ring`].
#[derive(Debug)]
pub struct SqRing {
    head: NonNull<AtomicU32>,
    tail: NonNull<AtomicU32>,
    flags: NonNull<AtomicU32>,
    dropped: NonNull<AtomicU32>,
    sqes: NonNull<sys::Sqe>,
    ring_ptr: NonNull<c_void>,
    ring_sz: usize,
    sqes_sz: usize,
    mask: u32,
    entries: u32,
    /// SQE array stride shift: 1 for [`sys::SetupFlags::SQE128`], else 0. Note
    /// that [`sys::SetupFlags::SQE_MIXED`] keeps a 64-byte stride and spends two
    /// adjacent slots on a 128-byte entry, so it does not shift.
    sqe_shift: u32,
    /// Under SQPOLL the kernel walks the ring on its own thread, so the index
    /// handoff needs real acquire/release ordering rather than relying on the
    /// `io_uring_enter` barrier.
    sqpoll: bool,
}

impl SqRing {
    /// Resolve the SQ offsets in `params` against a mapped ring.
    ///
    /// Unless the ring was created with [`sys::SetupFlags::NO_SQARRAY`], this
    /// also fills the SQ index array with the identity map, which is the only
    /// time that array is ever written.
    ///
    /// # Safety
    ///
    /// `ring` must be a mapping of `IORING_OFF_SQ_RING` of `ring_sz` bytes and
    /// `sqes` one of `IORING_OFF_SQES` of `sqes_sz` bytes, both for the ring
    /// `params` was filled in by, and both at least as large as `params`
    /// implies.
    unsafe fn new(
        ring: NonNull<c_void>,
        ring_sz: usize,
        sqes: NonNull<c_void>,
        sqes_sz: usize,
        params: &sys::Params,
    ) -> SqRing {
        let off = &params.sq_off;
        // SAFETY: forwarded to the caller's contract.
        unsafe {
            let entries = word_at(ring, off.ring_entries)
                .as_ref()
                .load(Ordering::Relaxed);

            if !params.flags.contains(sys::SetupFlags::NO_SQARRAY) {
                // Map SQ slot n directly onto SQE n. The kernel reads this only
                // below the published tail, and nothing rewrites it afterwards,
                // so plain stores before the ring goes live are sufficient.
                let array = ring.as_ptr().byte_add(off.array as usize).cast::<u32>();
                for i in 0..entries {
                    array.add(i as usize).write(i);
                }
            }

            SqRing {
                head: word_at(ring, off.head),
                tail: word_at(ring, off.tail),
                flags: word_at(ring, off.flags),
                dropped: word_at(ring, off.dropped),
                sqes: sqes.cast(),
                ring_ptr: ring,
                ring_sz,
                sqes_sz,
                mask: word_at(ring, off.ring_mask)
                    .as_ref()
                    .load(Ordering::Relaxed),
                entries,
                sqe_shift: params.flags.contains(sys::SetupFlags::SQE128) as u32,
                sqpoll: params.flags.contains(sys::SetupFlags::SQPOLL),
            }
        }
    }

    /// Number of SQEs the ring holds.
    pub fn entries(&self) -> u32 {
        self.entries
    }

    /// Mask that turns a running index into a slot index.
    pub fn mask(&self) -> u32 {
        self.mask
    }

    /// The kernel's consumer index.
    ///
    /// Acquires under SQPOLL: without it we could overwrite an SQE the kernel
    /// has not finished reading.
    pub fn head(&self) -> u32 {
        let order = if self.sqpoll {
            Ordering::Acquire
        } else {
            Ordering::Relaxed
        };
        // SAFETY: the pointer is into the mapping owned by our `Ring`, which
        // outlives every borrow that can reach us.
        unsafe { self.head.as_ref() }.load(order)
    }

    /// Our producer index, as the kernel last saw it.
    pub fn tail(&self) -> u32 {
        // SAFETY: as `head`.
        unsafe { self.tail.as_ref() }.load(Ordering::Relaxed)
    }

    /// Publish `tail`, releasing every SQE written below it.
    pub fn set_tail(&self, tail: u32) {
        let order = if self.sqpoll {
            Ordering::Release
        } else {
            Ordering::Relaxed
        };
        // SAFETY: as `head`.
        unsafe { self.tail.as_ref() }.store(tail, order);
    }

    /// Ring flags: [`sys::SqRingFlags::NEED_WAKEUP`] and friends.
    pub fn flags(&self) -> sys::SqRingFlags {
        // SAFETY: as `head`.
        let bits = unsafe { self.flags.as_ref() }.load(Ordering::Relaxed);
        sys::SqRingFlags::from_bits_retain(bits)
    }

    /// Count of SQ entries the kernel rejected as invalid.
    pub fn dropped(&self) -> u32 {
        // SAFETY: as `head`.
        unsafe { self.dropped.as_ref() }.load(Ordering::Relaxed)
    }

    /// Submissions written but not yet consumed by the kernel.
    pub fn pending(&self) -> u32 {
        self.tail().wrapping_sub(self.head())
    }

    /// Free slots in the ring.
    pub fn space_left(&self) -> u32 {
        self.entries - self.pending()
    }

    /// The SQE slot for running index `index`, which is masked for you.
    ///
    /// # Safety
    ///
    /// The slot must be free — that is, `index` must be at or above the last
    /// published tail and within [`SqRing::space_left`] of it. Writing a slot
    /// the kernel is still reading is a data race.
    pub unsafe fn sqe(&self, index: u32) -> *mut sys::Sqe {
        let slot = (index & self.mask) << self.sqe_shift;
        // SAFETY: `slot` is within the SQE array by construction; freeness is
        // forwarded to the caller's contract.
        unsafe { self.sqes.as_ptr().add(slot as usize) }
    }
}

// ---------------------------------------------------------------------------
// Completion ring
// ---------------------------------------------------------------------------

/// The completion queue: control words plus the CQE array.
///
/// Obtained from [`Ring::cq`]; the pointers within are valid only for as long
/// as the owning [`Ring`].
#[derive(Debug)]
pub struct CqRing {
    head: NonNull<AtomicU32>,
    tail: NonNull<AtomicU32>,
    overflow: NonNull<AtomicU32>,
    /// `None` on kernels that predate the field, which report offset 0 for it.
    flags: Option<NonNull<AtomicU32>>,
    cqes: NonNull<sys::Cqe>,
    ring_ptr: NonNull<c_void>,
    /// Zero when the CQ shares the SQ's mapping, which owns it.
    ring_sz: usize,
    mask: u32,
    entries: u32,
    /// CQE array stride shift: 1 for [`sys::SetupFlags::CQE32`], else 0.
    /// [`sys::SetupFlags::CQE_MIXED`] keeps a 16-byte stride, so it does not
    /// shift; a 32-byte completion there occupies two slots and is marked with
    /// [`sys::CqeFlags::F32`].
    cqe_shift: u32,
}

impl CqRing {
    /// Resolve the CQ offsets in `params` against a mapped ring.
    ///
    /// # Safety
    ///
    /// `ring` must be a mapping of `IORING_OFF_CQ_RING` for the ring `params`
    /// was filled in by — which is the `IORING_OFF_SQ_RING` mapping itself when
    /// [`sys::Features::SINGLE_MMAP`] is set — and at least as large as `params`
    /// implies. `ring_sz` must be zero when that mapping is owned elsewhere.
    unsafe fn new(ring: NonNull<c_void>, ring_sz: usize, params: &sys::Params) -> CqRing {
        let off = &params.cq_off;
        // SAFETY: forwarded to the caller's contract.
        unsafe {
            CqRing {
                head: word_at(ring, off.head),
                tail: word_at(ring, off.tail),
                overflow: word_at(ring, off.overflow),
                // Offset 0 is where the SQ head lives, so no real field can sit
                // there; the kernel uses it to mean "not supported".
                flags: (off.flags != 0).then(|| word_at(ring, off.flags)),
                cqes: NonNull::new_unchecked(
                    ring.as_ptr().byte_add(off.cqes as usize).cast::<sys::Cqe>(),
                ),
                ring_ptr: ring,
                ring_sz,
                mask: word_at(ring, off.ring_mask)
                    .as_ref()
                    .load(Ordering::Relaxed),
                entries: word_at(ring, off.ring_entries)
                    .as_ref()
                    .load(Ordering::Relaxed),
                cqe_shift: params.flags.contains(sys::SetupFlags::CQE32) as u32,
            }
        }
    }

    /// Number of CQEs the ring holds.
    pub fn entries(&self) -> u32 {
        self.entries
    }

    /// Mask that turns a running index into a slot index.
    pub fn mask(&self) -> u32 {
        self.mask
    }

    /// Our consumer index.
    pub fn head(&self) -> u32 {
        // SAFETY: the pointer is into the mapping owned by our `Ring`, which
        // outlives every borrow that can reach us.
        unsafe { self.head.as_ref() }.load(Ordering::Relaxed)
    }

    /// The kernel's producer index.
    ///
    /// Always acquires: completions are posted from interrupt context and other
    /// CPUs regardless of how the ring was set up, and this load is what orders
    /// the CQE reads that follow it.
    pub fn tail(&self) -> u32 {
        // SAFETY: as `head`.
        unsafe { self.tail.as_ref() }.load(Ordering::Acquire)
    }

    /// Completions available to read.
    pub fn ready(&self) -> u32 {
        self.tail().wrapping_sub(self.head())
    }

    /// Count of completions the kernel could not fit in the ring.
    pub fn overflow(&self) -> u32 {
        // SAFETY: as `head`.
        unsafe { self.overflow.as_ref() }.load(Ordering::Relaxed)
    }

    /// Ring flags, or `None` if this kernel does not expose them.
    pub fn flags(&self) -> Option<sys::CqRingFlags> {
        // SAFETY: as `head`.
        let bits = self
            .flags
            .map(|f| unsafe { f.as_ref() }.load(Ordering::Relaxed));
        bits.map(sys::CqRingFlags::from_bits_retain)
    }

    /// The CQE at running index `index`, which is masked for you.
    ///
    /// # Safety
    ///
    /// `index` must be below the tail last returned by [`CqRing::tail`] and at
    /// or above [`CqRing::head`], so the kernel is not still writing it.
    pub unsafe fn cqe(&self, index: u32) -> *const sys::Cqe {
        let slot = (index & self.mask) << self.cqe_shift;
        // SAFETY: `slot` is within the CQE array by construction; the rest is
        // forwarded to the caller's contract.
        unsafe { self.cqes.as_ptr().add(slot as usize) }
    }

    /// Iterate the completions that are ready right now.
    ///
    /// Samples the tail once, so the batch is fixed at the point of the call.
    /// Entries are released back to the kernel when the iterator is dropped.
    pub fn completions(&self) -> Completions<'_> {
        Completions {
            cq: self,
            head: self.head(),
            tail: self.tail(),
            consumed: 0,
        }
    }

    /// Release `n` completions back to the kernel.
    ///
    /// Always releases, so the kernel only observes the new head after the CQEs
    /// below it have been read.
    pub fn advance(&self, n: u32) {
        if n != 0 {
            let new = self.head().wrapping_add(n);
            // SAFETY: as `head`.
            unsafe { self.head.as_ref() }.store(new, Ordering::Release);
        }
    }
}

// ---------------------------------------------------------------------------
// Completions
// ---------------------------------------------------------------------------

/// The outcome encoded in a completion's `res`, read according to its flags.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CqeResult {
    /// Success. The meaning is operation-defined: bytes transferred, a file
    /// descriptor, a direct descriptor slot, a poll event mask, and so on.
    Value(u32),
    /// The operation failed.
    Error(Errno),
    /// A zero-copy notification, not a result at all — the send's own outcome
    /// arrived on a separate completion. `copied` reports that the kernel fell
    /// back to copying, and is only meaningful when the submission carried
    /// [`sys::RecvSendFlags::SEND_ZC_REPORT_USAGE`].
    Notification { copied: bool },
}

impl sys::Cqe {
    /// Split this completion into its result or its `-errno`.
    ///
    /// Checks [`sys::CqeFlags::NOTIF`] first: on a notification `res` is a flag
    /// word whose only defined bit is `1 << 31`, which read as a signed integer
    /// would otherwise look like an enormous negative errno.
    pub fn result(&self) -> CqeResult {
        if self.flags.contains(sys::CqeFlags::NOTIF) {
            let copied = self.res as u32 & sys::NOTIF_USAGE_ZC_COPIED != 0;
            CqeResult::Notification { copied }
        } else if self.res < 0 {
            CqeResult::Error(Errno(-self.res))
        } else {
            CqeResult::Value(self.res as u32)
        }
    }
}

/// One completion, copied out of the ring.
///
/// Taken by value rather than borrowed: the slot it came from is released back
/// to the kernel when the iterator that produced it is dropped, so a reference
/// into the ring could be overwritten while still held.
#[derive(Debug, Clone, Copy)]
pub struct Completion {
    cqe: sys::Cqe,
    big: Option<[u64; 2]>,
}

impl Completion {
    /// The token echoed back from the submission.
    pub fn user_data(&self) -> u64 {
        self.cqe.user_data
    }

    /// The result, interpreted according to the flags.
    pub fn result(&self) -> CqeResult {
        self.cqe.result()
    }

    pub fn flags(&self) -> sys::CqeFlags {
        self.cqe.flags
    }

    /// The provided buffer this completion consumed.
    /// Valid only when [`sys::CqeFlags::BUFFER`] is set.
    pub fn buffer_id(&self) -> u16 {
        self.cqe.buffer_id()
    }

    /// The extra 16 bytes of a 32-byte completion, on rings that post them.
    pub fn big(&self) -> Option<[u64; 2]> {
        self.big
    }

    /// The raw entry.
    pub fn raw(&self) -> &sys::Cqe {
        &self.cqe
    }
}

/// An iterator over ready completions, releasing them as it goes.
///
/// The tail is sampled once, so the batch is whatever was ready when iteration
/// started. Slots are released on drop, and only those actually yielded — so
/// breaking out early leaves the rest for the next batch.
#[derive(Debug)]
pub struct Completions<'cq> {
    cq: &'cq CqRing,
    head: u32,
    tail: u32,
    consumed: u32,
}

impl Iterator for Completions<'_> {
    type Item = Completion;

    fn next(&mut self) -> Option<Completion> {
        while self.head != self.tail {
            // SAFETY: `head` is below the tail sampled at construction and at
            // or above the ring's head, so the kernel has finished writing it
            // and will not reuse it until we advance.
            let cqe = unsafe { *self.cq.cqe(self.head) };
            let wide = cqe.flags.contains(sys::CqeFlags::F32);

            // A big completion spends two 16-byte slots on a mixed ring; on a
            // dedicated CQE32 ring the stride already covers it.
            let slots = 1 + u32::from(wide && self.cq.cqe_shift == 0);
            let big = (wide || self.cq.cqe_shift == 1).then(|| {
                // SAFETY: the kernel never lets a big completion straddle the
                // wrap — it pads with a skip entry instead — so the trailing 16
                // bytes are contiguous with the ones just read.
                unsafe { (*self.cq.cqe(self.head).cast::<sys::Cqe32>()).big_cqe }
            });

            self.head = self.head.wrapping_add(slots);
            self.consumed += slots;

            // Padding posted to fill a wrap gap. It occupies a slot but is not
            // a completion, so consume it and keep going.
            if cqe.flags.contains(sys::CqeFlags::SKIP) {
                continue;
            }
            return Some(Completion { cqe, big });
        }
        None
    }
}

impl Drop for Completions<'_> {
    fn drop(&mut self) {
        self.cq.advance(self.consumed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Push `n` nops carrying `user_data = i`, submit, and return the results
    /// in completion order.
    fn nops(ring: &Ring, n: u32) -> Vec<(u64, i32)> {
        let sq = ring.sq();
        let tail = sq.tail();
        for i in 0..n {
            // SAFETY: the ring is idle and `n` is within `space_left`, so these
            // slots are ours to write.
            unsafe {
                sq.sqe(tail + i).write(sys::Sqe {
                    opcode: sys::Opcode::Nop,
                    user_data: i as u64,
                    ..sys::Sqe::ZEROED
                })
            };
        }
        sq.set_tail(tail + n);

        // SAFETY: no argument is passed, so `arg`/`argsz` are trivially valid,
        // and every submitted SQE is a well-formed nop.
        let submitted = unsafe {
            syscall::io_uring_enter(
                ring.enter_fd(),
                n,
                n,
                ring.enter_flags() | sys::EnterFlags::GETEVENTS,
                std::ptr::null(),
                0,
            )
        }
        .expect("io_uring_enter");
        assert_eq!(submitted, n, "kernel consumed {submitted} of {n} sqes");

        let cq = ring.cq();
        assert_eq!(cq.ready(), n);
        let head = cq.head();
        let out = (0..n)
            .map(|i| {
                // SAFETY: `i` is below the observed tail and at or above head.
                let cqe = unsafe { &*cq.cqe(head + i) };
                (cqe.user_data, cqe.res)
            })
            .collect();
        cq.advance(n);
        assert_eq!(cq.ready(), 0);
        out
    }

    #[test]
    #[cfg_attr(miri, ignore = "issues real syscalls and mmaps a ring")]
    fn nop_round_trip() {
        let mut params = sys::Params::default();
        let ring = Ring::with_params(8, &mut params).expect("Ring::new");

        assert_eq!(ring.sq().entries(), 8);
        assert_eq!(ring.sq().mask(), 7);
        assert_eq!(ring.sq().space_left(), 8);
        assert_eq!(ring.cq().ready(), 0);
        assert!(ring.features().contains(sys::Features::NODROP));
        assert_eq!(ring.fd(), Some(ring.enter_fd() as i32));
        assert!(ring.enter_flags().is_empty());

        assert_eq!(nops(&ring, 1), vec![(0, 0)]);
        assert_eq!(ring.sq().pending(), 0);
        assert_eq!(ring.sq().dropped(), 0);
        assert_eq!(ring.cq().overflow(), 0);
    }

    /// Every SQ slot must map to the SQE of the same index. If the identity map
    /// were missing, all three submissions would resolve to slot 0 and come
    /// back carrying the same `user_data`.
    #[test]
    #[cfg_attr(miri, ignore = "issues real syscalls and mmaps a ring")]
    fn sq_array_maps_each_slot_to_its_own_sqe() {
        let mut params = sys::Params::default();
        let ring = Ring::with_params(8, &mut params).expect("Ring::new");
        assert!(!ring.flags().contains(sys::SetupFlags::NO_SQARRAY));
        assert_eq!(nops(&ring, 3), vec![(0, 0), (1, 0), (2, 0)]);
    }

    /// Registering the ring's own fd swaps `enter_fd` for an index and makes
    /// every later enter carry `REGISTERED_RING`.
    #[test]
    #[cfg_attr(miri, ignore = "issues real syscalls and mmaps a ring")]
    fn register_ring_fd_switches_the_enter_path() {
        let mut params = sys::Params::default();
        let mut ring = Ring::with_params(8, &mut params).expect("Ring::new");
        let fd = ring.fd().expect("ring should own a descriptor");

        ring.register_ring_fd().expect("register_ring_fd");
        assert_eq!(ring.fd(), Some(fd), "the real fd should be unchanged");
        assert_eq!(ring.enter_fd(), 0, "first registered slot");
        assert!(
            ring.enter_flags()
                .contains(sys::EnterFlags::REGISTERED_RING)
        );

        // The ring still works, now entered by index.
        assert_eq!(nops(&ring, 2), vec![(0, 0), (1, 0)]);

        assert_eq!(
            ring.register_ring_fd().unwrap_err(),
            Errno(libc::EEXIST),
            "registering twice should be rejected"
        );
    }

    /// `res` is only an errno when the completion is not a zero-copy
    /// notification. `IORING_NOTIF_USAGE_ZC_COPIED` is `1 << 31`, which as a
    /// signed int is negative and would otherwise read as a huge errno.
    #[test]
    fn notification_res_is_not_an_errno() {
        let notif = sys::Cqe {
            user_data: 1,
            res: sys::NOTIF_USAGE_ZC_COPIED as i32,
            flags: sys::CqeFlags::NOTIF,
        };
        assert!(
            notif.res < 0,
            "the trap only exists because this is negative"
        );
        assert_eq!(notif.result(), CqeResult::Notification { copied: true });

        let plain = sys::Cqe {
            user_data: 1,
            res: 0,
            flags: sys::CqeFlags::NOTIF,
        };
        assert_eq!(plain.result(), CqeResult::Notification { copied: false });

        let ok = sys::Cqe {
            user_data: 1,
            res: 17,
            flags: sys::CqeFlags::empty(),
        };
        assert_eq!(ok.result(), CqeResult::Value(17));

        let err = sys::Cqe {
            user_data: 1,
            res: -libc::ENOBUFS,
            flags: sys::CqeFlags::empty(),
        };
        assert_eq!(err.result(), CqeResult::Error(Errno::NOBUFS));
    }

    /// The iterator must yield every completion, release exactly what it
    /// yielded, and leave the ring empty afterwards.
    #[test]
    #[cfg_attr(miri, ignore = "issues real syscalls and mmaps a ring")]
    fn completions_drain_the_ring() {
        let mut params = sys::Params::default();
        let ring = Ring::with_params(8, &mut params).expect("Ring::with_params");
        let _ = nops(&ring, 0);

        let sq = ring.sq();
        let tail = sq.tail();
        for i in 0..4u32 {
            // SAFETY: the ring is idle, so these slots are ours.
            unsafe {
                sq.sqe(tail + i).write(sys::Sqe {
                    opcode: sys::Opcode::Nop,
                    user_data: i as u64,
                    ..sys::Sqe::ZEROED
                })
            };
        }
        sq.set_tail(tail + 4);
        // SAFETY: no argument, and the SQEs above are well formed nops.
        unsafe {
            syscall::io_uring_enter(
                ring.enter_fd(),
                4,
                4,
                ring.enter_flags() | sys::EnterFlags::GETEVENTS,
                std::ptr::null(),
                0,
            )
        }
        .expect("io_uring_enter");

        let cq = ring.cq();
        assert_eq!(cq.ready(), 4);
        let seen: Vec<_> = cq
            .completions()
            .map(|c| {
                (
                    c.user_data(),
                    c.result(),
                    c.flags().contains(sys::CqeFlags::MORE),
                )
            })
            .collect();
        assert_eq!(
            seen,
            (0..4)
                .map(|i| (i as u64, CqeResult::Value(0), false))
                .collect::<Vec<_>>()
        );
        assert_eq!(cq.ready(), 0, "the iterator must release what it yielded");
    }

    /// Breaking out early must release only what was actually taken, leaving
    /// the rest for the next batch.
    #[test]
    #[cfg_attr(miri, ignore = "issues real syscalls and mmaps a ring")]
    fn a_partial_drain_leaves_the_rest() {
        let mut params = sys::Params::default();
        let ring = Ring::with_params(8, &mut params).expect("Ring::with_params");

        let sq = ring.sq();
        let tail = sq.tail();
        for i in 0..4u32 {
            // SAFETY: the ring is idle, so these slots are ours.
            unsafe {
                sq.sqe(tail + i).write(sys::Sqe {
                    opcode: sys::Opcode::Nop,
                    user_data: i as u64,
                    ..sys::Sqe::ZEROED
                })
            };
        }
        sq.set_tail(tail + 4);
        // SAFETY: as above.
        unsafe {
            syscall::io_uring_enter(
                ring.enter_fd(),
                4,
                4,
                ring.enter_flags() | sys::EnterFlags::GETEVENTS,
                std::ptr::null(),
                0,
            )
        }
        .expect("io_uring_enter");

        let cq = ring.cq();
        let first: Vec<_> = cq.completions().take(2).map(|c| c.user_data()).collect();
        assert_eq!(first, vec![0, 1]);
        assert_eq!(cq.ready(), 2, "only the taken entries should be released");

        let rest: Vec<_> = cq.completions().map(|c| c.user_data()).collect();
        assert_eq!(rest, vec![2, 3]);
        assert_eq!(cq.ready(), 0);
    }

    /// An idle ring with nothing pending needs no flush.
    #[test]
    #[cfg_attr(miri, ignore = "issues real syscalls and mmaps a ring")]
    fn an_idle_ring_needs_no_flush() {
        let mut params = sys::Params::default();
        let ring = Ring::with_params(8, &mut params).expect("Ring::with_params");
        assert!(!ring.needs_flush());
        assert_eq!(nops(&ring, 1), vec![(0, 0)]);
        assert!(!ring.needs_flush());
    }

    /// Two rings can exist at once and tear down independently.
    #[test]
    #[cfg_attr(miri, ignore = "issues real syscalls and mmaps a ring")]
    fn rings_are_independent() {
        let mut params = sys::Params::default();
        let a = Ring::with_params(8, &mut params).expect("Ring::new");
        let mut params = sys::Params::default();
        let b = Ring::with_params(16, &mut params).expect("Ring::new");
        assert_ne!(a.fd(), b.fd());
        assert_eq!(a.sq().entries(), 8);
        assert_eq!(b.sq().entries(), 16);
        assert_eq!(nops(&a, 1), vec![(0, 0)]);
        drop(a);
        assert_eq!(nops(&b, 1), vec![(0, 0)]);
    }
}
