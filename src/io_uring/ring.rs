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
        let mut sq_sz = params.sq_off.array as usize + params.sq_entries as usize * 4;
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
