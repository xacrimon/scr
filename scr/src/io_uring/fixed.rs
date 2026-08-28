//! Direct descriptors: the ring's own file table, and who owns each slot in it.
//!
//! A direct descriptor is an index into a table registered with the ring rather
//! than a process-wide file descriptor. An operation reaches one by putting the
//! index in [`sys::Sqe::fd`](super::sys::Sqe::fd) and setting
//! [`sys::SqeFlags::FIXED_FILE`](super::sys::SqeFlags::FIXED_FILE):
//!
//! ```ignore
//! op::Read::new()
//!     .fd(slot as i32)
//!     .sqe_flags(|f| f | sys::SqeFlags::FIXED_FILE)
//! ```
//!
//! The point is what the kernel skips. A descriptor argument costs an
//! `fdget`/`fdput` pair per operation — a lookup in the process file table plus
//! a reference count round trip on a cacheline other threads share. A direct
//! descriptor is resolved once, at registration, and every later operation is an
//! array index into memory the ring owns. On a socket workload that is the
//! difference the fixed-file path exists for, so a runtime that can keep its
//! sockets direct should keep all of them direct.
//!
//! # The table is fixed size
//!
//! [`Ring::register_files_sparse`] fixes the table's length. There is no
//! register operation that grows it, and registering over a live table fails
//! with `EBUSY`; growing means unregistering, which closes every descriptor the
//! table holds. So the capacity is a startup decision — size it for the peak.
//! Slots are cheap (a tagged pointer each, plus a bit in the kernel's own free
//! bitmap) but they are charged against `RLIMIT_NOFILE` at registration even
//! while empty.
//!
//! # Two ways to pick a slot
//!
//! The kernel can choose: an operation that installs a descriptor
//! ([`Openat`](super::op::Openat), [`Accept`](super::op::Accept),
//! [`Socket`](super::op::Socket)) accepts [`sys::FILE_INDEX_ALLOC`](super::sys::FILE_INDEX_ALLOC) as its
//! output slot and reports the slot it took in `cqe.res`.
//!
//! Or the caller can: the same operations take a specific slot, and
//! [`Ring::update_files`] installs an existing descriptor into one.
//!
//! Caller-side allocation is what a reactor normally wants, because the slot is
//! known at submission time rather than at completion time. That is what makes
//! [`sys::SqeFlags::IO_LINK`](super::sys::SqeFlags::IO_LINK) usable — a socket, connect and send chain can be
//! submitted in one go only if the later entries can already name the descriptor
//! the first one will produce. It is also the only option for a descriptor that
//! came from outside the ring.
//!
//! Kernel-side allocation is not redundant, though: a multishot accept produces
//! many descriptors from one submission, so it cannot have a slot chosen for it.
//!
//! [`FixedFiles`] runs both at once by splitting the table. It allocates from
//! `[0, managed)` itself and calls [`Ring::set_file_alloc_range`] to confine the
//! kernel's allocator to the rest, so the two never hand out the same slot.
//! Slots in the kernel's region need no bookkeeping here at all: the kernel
//! marks one used when it installs a descriptor and free when the descriptor is
//! closed.
//!
//! # Allocating slots
//!
//! The managed region is one `u32` per slot, holding either a sentinel marking
//! the slot as handed out or the index of the next free slot — an intrusive
//! free chain, linked in full at construction. `alloc` pops its head and `free`
//! pushes onto it, so both are constant time regardless of how large or how
//! fragmented the table is, and the sentinel doubles as the record of which
//! slots are live, so nothing has to be kept in step with anything else.
//!
//! The chain hands back the most recently freed slot first. That suits a server,
//! where a slot freed by a closing connection is about to be taken by the next
//! accept and its bookkeeping is still warm — but it also means a slot freed
//! too early is the very next one handed out, which is why the discipline below
//! matters.
//!
//! # Slot lifetime
//!
//! A slot is in use from the moment an operation naming it is submitted until
//! the completion of the operation that empties it. Closing is asynchronous —
//! [`Close::slot`](super::op::Close::slot) — so a slot must go back to the
//! allocator when that close completes, never when it is submitted. Reusing it
//! earlier hands the next operation a descriptor that is still open, and because
//! installing into an occupied slot *replaces* the occupant instead of failing,
//! nothing reports the mistake.
//!
//! [`Ring`]: super::ring::Ring

#![allow(dead_code)]

use std::ops::Range;

use super::ring::Ring;
use super::syscall::Errno;

/// Marks a slot in [`FixedFiles::slots`] as handed out. Above
/// `IORING_MAX_FIXED_FILES`, so it cannot collide with a slot index.
const OCCUPIED: u32 = u32::MAX;

/// Closes the free chain in [`FixedFiles::slots`].
const END: u32 = u32::MAX - 1;

/// A registered direct descriptor table, with an allocator for the slots the
/// caller manages.
///
/// This does not own the registration: the table belongs to the [`Ring`] and
/// dies with it. Dropping a `FixedFiles` only discards the bookkeeping.
#[derive(Debug)]
pub struct FixedFiles {
    /// One entry per managed slot: [`OCCUPIED`] when the slot is handed out,
    /// otherwise the next free slot, with [`END`] closing the chain. The
    /// sentinel doubles as the record of which slots are live, so there is no
    /// second structure to keep in step with this one.
    slots: Box<[u32]>,
    /// First slot of the free chain, or [`END`] when the region is full.
    head: u32,
    /// Slots registered with the kernel, across both regions.
    capacity: u32,
    /// Slots currently handed out.
    used: u32,
}

impl FixedFiles {
    /// Register a sparse table of `capacity` slots on `ring` and take ownership
    /// of the first `capacity - kernel_reserved` of them.
    ///
    /// The last `kernel_reserved` slots are left to the kernel's own allocator
    /// for [`sys::FILE_INDEX_ALLOC`](super::sys::FILE_INDEX_ALLOC), for multishot accepts. Pass zero to manage
    /// the whole table.
    ///
    /// Fails with [`Errno::INVAL`] on an empty table or a reservation larger
    /// than it, with `EBUSY` if a table is already registered, and with `EMFILE`
    /// if `capacity` exceeds `RLIMIT_NOFILE`.
    pub fn register(ring: &Ring, capacity: u32, kernel_reserved: u32) -> Result<Self, Errno> {
        if capacity == 0 || kernel_reserved > capacity {
            return Err(Errno::INVAL);
        }
        ring.register_files_sparse(capacity)?;
        if kernel_reserved != 0 {
            let managed = capacity - kernel_reserved;
            // Undo the registration rather than leave the caller a table whose
            // two regions are not actually separated.
            if let Err(e) = ring.set_file_alloc_range(managed, kernel_reserved) {
                ring.unregister_files().ok();
                return Err(e);
            }
        }
        Ok(FixedFiles::with_capacity(capacity, kernel_reserved))
    }

    /// The bookkeeping alone, for a table registered by other means.
    ///
    /// The whole chain is linked up front, so every managed slot is written
    /// once here rather than on first use.
    fn with_capacity(capacity: u32, kernel_reserved: u32) -> Self {
        let managed = (capacity - kernel_reserved) as usize;
        let mut slots: Box<[u32]> = (1..=managed as u32).collect();
        if let Some(last) = slots.last_mut() {
            *last = END;
        }
        FixedFiles {
            slots,
            head: if managed == 0 { END } else { 0 },
            capacity,
            used: 0,
        }
    }

    /// Slots registered with the kernel, across both regions.
    pub fn capacity(&self) -> u32 {
        self.capacity
    }

    /// Slots this allocator hands out, `[0, managed)`.
    pub fn managed(&self) -> u32 {
        self.slots.len() as u32
    }

    /// The region left to the kernel's allocator, if any.
    pub fn kernel_range(&self) -> Option<Range<u32>> {
        (self.managed() < self.capacity).then_some(self.managed()..self.capacity)
    }

    /// Whether the kernel, rather than this allocator, accounts for `slot`.
    pub fn is_kernel_owned(&self, slot: u32) -> bool {
        (self.managed()..self.capacity).contains(&slot)
    }

    /// Allocated slots in the managed region.
    pub fn used(&self) -> u32 {
        self.used
    }

    /// Free slots in the managed region.
    pub fn available(&self) -> u32 {
        self.managed() - self.used
    }

    /// Whether `slot` is a managed slot that is not currently handed out.
    pub fn is_free(&self, slot: u32) -> bool {
        self.slots
            .get(slot as usize)
            .is_some_and(|&s| s != OCCUPIED)
    }

    /// Reserve a slot, or `None` when the managed region is full.
    ///
    /// Unlinks the head of the free chain, so this is a constant-time pop
    /// whatever the table's size or how fragmented it has become.
    ///
    /// The slot is only reserved here — nothing is installed in it until an
    /// operation naming it completes.
    pub fn alloc(&mut self) -> Option<u32> {
        if self.head == END {
            return None;
        }
        let slot = self.head;
        self.head = self.slots[slot as usize];
        self.slots[slot as usize] = OCCUPIED;
        self.used += 1;
        Some(slot)
    }

    /// Return `slot` to the allocator, at the head of the free chain.
    ///
    /// Call this when the operation that emptied the slot *completes*, not when
    /// it is submitted; see the module docs. Slots in the kernel's region are
    /// ignored, so a caller that mixes the two regions can call this
    /// unconditionally.
    ///
    /// Freeing a slot that is already free would splice the chain into a cycle,
    /// so it is refused rather than allowed to corrupt the chain — in debug
    /// builds loudly, otherwise as a no-op.
    pub fn free(&mut self, slot: u32) {
        debug_assert!(slot < self.capacity, "slot {slot} is outside the table");
        if slot >= self.managed() {
            return;
        }
        debug_assert!(
            self.slots[slot as usize] == OCCUPIED,
            "slot {slot} freed twice"
        );
        if self.slots[slot as usize] == OCCUPIED {
            self.slots[slot as usize] = self.head;
            self.head = slot;
            self.used -= 1;
        }
    }

    /// Reserve a slot and install `fd` into it, returning the slot.
    ///
    /// This is the synchronous path, for descriptors that arrive from outside
    /// the ring — a listener, or a standard stream. Descriptors the ring itself
    /// produces should be installed by the operation that creates them, which
    /// costs no syscall at all.
    ///
    /// `fd` stays the caller's to close: registering dups the underlying file,
    /// it does not consume the descriptor.
    pub fn install(&mut self, ring: &Ring, fd: i32) -> Result<u32, Errno> {
        let slot = self.alloc().ok_or(Errno(libc::ENFILE))?;
        match ring.update_files(slot, &[fd]) {
            Ok(_) => Ok(slot),
            Err(e) => {
                self.free(slot);
                Err(e)
            }
        }
    }

    /// Empty `slot` and return it to the allocator.
    ///
    /// The synchronous counterpart to [`FixedFiles::install`]. A reactor with a
    /// ring to hand should prefer [`Close::slot`](super::op::Close::slot), which
    /// does the same thing without leaving the submission path.
    pub fn remove(&mut self, ring: &Ring, slot: u32) -> Result<(), Errno> {
        ring.update_files(slot, &[-1])?;
        self.free(slot);
        Ok(())
    }

    /// Drop the whole table, closing every descriptor in it.
    pub fn unregister(self, ring: &Ring) -> Result<(), Errno> {
        ring.unregister_files()
    }
}

#[cfg(test)]
mod tests {
    use std::ptr::NonNull;

    use super::*;
    use crate::io_uring::op;
    use crate::io_uring::ring::CqeResult;
    use crate::io_uring::sys;
    use crate::io_uring::syscall;

    /// Submit one entry, wait for its completion, and report the result.
    fn run(ring: &Ring, sqe: sys::Sqe) -> CqeResult {
        let sq = ring.sq();
        let tail = sq.tail();
        // SAFETY: the ring is idle in these tests, so this slot is ours.
        unsafe { sq.sqe(tail).write(sqe) };
        sq.set_tail(tail + 1);

        // SAFETY: no argument is passed, and every SQE these tests build names
        // a live descriptor or slot and a buffer that outlives the call.
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

        let mut completions = ring.cq().completions();
        let c = completions.next().expect("one completion");
        assert!(completions.next().is_none());
        c.result()
    }

    fn ring() -> Ring {
        let mut params = sys::Params::default();
        Ring::with_params(8, &mut params).expect("Ring::with_params")
    }

    /// A pipe, as a pair of owned descriptors.
    fn pipe() -> (i32, i32) {
        let mut fds = [0i32; 2];
        // SAFETY: `fds` is a live array of the two descriptors `pipe` writes.
        assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0);
        (fds[0], fds[1])
    }

    #[test]
    fn alloc_hands_out_every_managed_slot_once() {
        let mut files = FixedFiles::with_capacity(200, 0);
        assert_eq!(files.managed(), 200);
        assert_eq!(files.available(), 200);

        let mut seen: Vec<u32> = (0..200).map(|_| files.alloc().expect("a slot")).collect();
        assert_eq!(files.alloc(), None, "the table is full");
        assert_eq!(files.used(), 200);
        assert_eq!(files.available(), 0);

        seen.sort_unstable();
        assert_eq!(seen, (0..200).collect::<Vec<_>>());
    }

    /// A slot past the managed region belongs to the kernel, or to nothing at
    /// all; neither is this allocator's to report on.
    #[test]
    fn slots_outside_the_managed_region_are_not_free() {
        let files = FixedFiles::with_capacity(70, 0);
        assert!(files.is_free(69));
        assert!(!files.is_free(70));
        assert!(!files.is_free(127));
    }

    /// Freeing twice would link a slot to itself and orphan every slot behind
    /// it in the chain, so the sentinel has to catch it.
    #[test]
    #[should_panic(expected = "freed twice")]
    fn a_double_free_is_caught() {
        let mut files = FixedFiles::with_capacity(4, 0);
        let slot = files.alloc().unwrap();
        files.free(slot);
        files.free(slot);
    }

    /// The chain is only correct if every managed slot appears in it exactly
    /// once, and a corrupted chain is otherwise silent: it either loops or
    /// loses slots. Churn it, checking no slot is ever handed out twice, then
    /// drain and confirm the whole region is still accounted for.
    #[test]
    fn churn_leaves_the_chain_intact() {
        const N: u32 = 300;
        let mut files = FixedFiles::with_capacity(N, 0);
        let mut live: Vec<u32> = Vec::new();
        let mut x = 0x9e3779b9u32;

        for step in 0..20_000 {
            x ^= x << 13;
            x ^= x >> 17;
            x ^= x << 5;
            if (x & 1 == 0 && !live.is_empty()) || live.len() == N as usize {
                let slot = live.swap_remove((x >> 8) as usize % live.len());
                files.free(slot);
                assert!(files.is_free(slot));
            } else {
                let slot = files.alloc().expect("the region is not full");
                assert!(
                    !live.contains(&slot),
                    "slot {slot} handed out twice at step {step}"
                );
                live.push(slot);
            }
            assert_eq!(files.used() as usize, live.len());
        }

        let mut all = live.clone();
        while let Some(slot) = files.alloc() {
            all.push(slot);
        }
        all.sort_unstable();
        assert_eq!(all, (0..N).collect::<Vec<_>>());
    }

    #[test]
    fn freed_slots_are_reused() {
        let mut files = FixedFiles::with_capacity(4, 0);
        let slots: Vec<u32> = (0..4).map(|_| files.alloc().unwrap()).collect();
        assert_eq!(files.alloc(), None);

        files.free(slots[2]);
        assert!(files.is_free(slots[2]));
        assert_eq!(files.used(), 3);
        assert_eq!(files.alloc(), Some(slots[2]));
        assert_eq!(files.alloc(), None);
    }

    /// The kernel accounts for its own region, so this allocator must never
    /// hand out a slot from it, and must tolerate being told about one.
    #[test]
    fn the_kernel_region_is_never_handed_out() {
        let mut files = FixedFiles::with_capacity(16, 6);
        assert_eq!(files.managed(), 10);
        assert_eq!(files.capacity(), 16);
        assert_eq!(files.kernel_range(), Some(10..16));
        assert!(files.is_kernel_owned(10));
        assert!(!files.is_kernel_owned(9));

        for _ in 0..10 {
            assert!(files.alloc().unwrap() < 10);
        }
        assert_eq!(files.alloc(), None);

        // A slot the kernel allocated is not ours to reclaim, but a caller that
        // mixes the regions should not have to branch.
        files.free(12);
        assert_eq!(files.used(), 10);
        assert_eq!(files.alloc(), None);
    }

    #[test]
    fn a_reservation_larger_than_the_table_is_rejected() {
        let ring = ring();
        assert_eq!(FixedFiles::register(&ring, 8, 9).unwrap_err(), Errno::INVAL);
        assert_eq!(FixedFiles::register(&ring, 0, 0).unwrap_err(), Errno::INVAL);
    }

    /// The whole point: an operation reaches an installed descriptor by index,
    /// with `IOSQE_FIXED_FILE` and no descriptor of its own.
    #[test]
    #[cfg_attr(miri, ignore = "issues real syscalls and mmaps a ring")]
    fn a_slot_reads_what_its_descriptor_reads() {
        let ring = ring();
        let mut files = FixedFiles::register(&ring, 8, 0).expect("register");
        let (r, w) = pipe();

        let slot = files.install(&ring, r).expect("install");
        assert_eq!(slot, 0);
        assert_eq!(files.used(), 1);

        const MSG: [u8; 5] = *b"fixed";
        // SAFETY: `w` is the live write end of the pipe.
        assert_eq!(unsafe { libc::write(w, MSG.as_ptr().cast(), MSG.len()) }, 5);

        let mut got = [0u8; 5];
        let sqe = op::Read::new()
            .fd(slot as i32)
            .sqe_flags(|f| f | sys::SqeFlags::FIXED_FILE)
            .buf(NonNull::from(&mut got))
            .offset(!0)
            .into_sqe();
        assert_eq!(run(&ring, sqe), CqeResult::Value(5));
        assert_eq!(&got, &MSG);

        files.remove(&ring, slot).expect("remove");
        assert!(files.is_free(slot));

        // SAFETY: both ends are still ours; registering dupped the file, so
        // closing `r` did not disturb the slot while it was installed.
        unsafe {
            libc::close(r);
            libc::close(w);
        }
    }

    /// A slot that was never filled is not a usable descriptor. This is the
    /// failure mode of reusing a slot too early, and the kernel reports it.
    #[test]
    #[cfg_attr(miri, ignore = "issues real syscalls and mmaps a ring")]
    fn an_empty_slot_is_ebadf() {
        let ring = ring();
        FixedFiles::register(&ring, 8, 0).expect("register");

        let mut got = [0u8; 4];
        let sqe = op::Read::new()
            .fd(3)
            .sqe_flags(|f| f | sys::SqeFlags::FIXED_FILE)
            .buf(NonNull::from(&mut got))
            .into_sqe();
        assert_eq!(run(&ring, sqe), CqeResult::Error(Errno(libc::EBADF)));
    }

    /// `FILE_INDEX_ALLOC` picks from the region handed to the kernel, which is
    /// what keeps it from colliding with the slots handed out here.
    #[test]
    #[cfg_attr(miri, ignore = "issues real syscalls and mmaps a ring")]
    fn the_kernel_allocator_stays_inside_its_range() {
        let ring = ring();
        let files = FixedFiles::register(&ring, 16, 6).expect("register");
        let range = files.kernel_range().expect("a kernel region");

        for _ in 0..range.len() {
            let sqe = op::Socket::new()
                .domain(libc::AF_INET)
                .kind(libc::SOCK_STREAM as u64)
                .slot(sys::FILE_INDEX_ALLOC)
                .into_sqe();
            match run(&ring, sqe) {
                CqeResult::Value(slot) => assert!(
                    range.contains(&slot),
                    "kernel chose {slot}, outside {range:?}"
                ),
                other => panic!("socket: {other:?}"),
            }
        }

        // Exhausted: the kernel will not spill into the managed region.
        let sqe = op::Socket::new()
            .domain(libc::AF_INET)
            .kind(libc::SOCK_STREAM as u64)
            .slot(sys::FILE_INDEX_ALLOC)
            .into_sqe();
        assert_eq!(run(&ring, sqe), CqeResult::Error(Errno(libc::ENFILE)));
    }

    /// The table cannot be resized in place, which is why its capacity is a
    /// startup decision.
    #[test]
    #[cfg_attr(miri, ignore = "issues real syscalls and mmaps a ring")]
    fn a_second_registration_is_ebusy() {
        let ring = ring();
        assert_eq!(ring.unregister_files().unwrap_err(), Errno::NXIO);

        FixedFiles::register(&ring, 8, 0).expect("register");
        assert_eq!(
            FixedFiles::register(&ring, 16, 0).unwrap_err(),
            Errno(libc::EBUSY)
        );

        ring.unregister_files().expect("unregister");
        FixedFiles::register(&ring, 16, 0).expect("re-register");
    }

    /// A direct close empties the slot without a syscall, and is the path a
    /// reactor takes; the slot only goes back to the allocator once it lands.
    #[test]
    #[cfg_attr(miri, ignore = "issues real syscalls and mmaps a ring")]
    fn a_direct_close_empties_the_slot() {
        let ring = ring();
        let mut files = FixedFiles::register(&ring, 8, 0).expect("register");
        let (r, w) = pipe();
        let slot = files.install(&ring, r).expect("install");

        assert_eq!(
            run(&ring, op::Close::new().slot(slot).into_sqe()),
            CqeResult::Value(0)
        );
        files.free(slot);
        assert!(files.is_free(slot));

        let mut got = [0u8; 4];
        let sqe = op::Read::new()
            .fd(slot as i32)
            .sqe_flags(|f| f | sys::SqeFlags::FIXED_FILE)
            .buf(NonNull::from(&mut got))
            .into_sqe();
        assert_eq!(run(&ring, sqe), CqeResult::Error(Errno(libc::EBADF)));

        // SAFETY: both ends are still ours.
        unsafe {
            libc::close(r);
            libc::close(w);
        }
    }
}
