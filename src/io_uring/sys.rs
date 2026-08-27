//! Raw io_uring UAPI bindings.
//!
//! Transcribed from liburing's vendored `src/include/liburing/io_uring.h`
//! (at `27255f4c`), which is the source of truth for this module. That copy is not
//! identical to the distro's `/usr/include/linux/io_uring.h`; where they differ,
//! liburing wins and the divergence is noted on the item.
//!
//! Everything here is a 1:1 mirror of the C definitions: plain `repr(C)` structs
//! with public fields, no accessors, no invariants. The ergonomic layer lives on
//! top of this.
//!
//! Where the C header uses an anonymous union, this module picks **one field per
//! byte range** and documents the other arms in a comment. Callers select an arm
//! by writing the right integer into the slot. All such punning assumes a
//! little-endian target, which io_uring effectively requires anyway.

#![allow(dead_code)]

use bitflags::bitflags;

// ---------------------------------------------------------------------------
// Opcodes
// ---------------------------------------------------------------------------

/// `enum io_uring_op`, written into [`Sqe::opcode`].
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Opcode {
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
}

impl Opcode {
    /// `IORING_OP_LAST` for the header this was generated from. Prefer probing
    /// the running kernel over comparing against this.
    pub const LAST: u8 = Opcode::UringCmd128 as u8 + 1;
}

// ---------------------------------------------------------------------------
// Submission queue entry
// ---------------------------------------------------------------------------

/// `struct io_uring_sqe`, the 64-byte submission queue entry.
///
/// This is the element type of the SQE array in every mode *except*
/// [`SetupFlags::SQE128`], where the stride doubles to 128 and the element type
/// is [`Sqe128`]. Under [`SetupFlags::SQE_MIXED`] the stride stays 64 and a
/// 128-byte submission instead occupies two adjacent slots — see [`Sqe128`].
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct Sqe {
    pub opcode: Opcode,
    pub flags: SqeFlags,
    /// Doubles as the op-specific flag word for several opcodes:
    /// [`RecvSendFlags`], [`AcceptFlags`].
    pub ioprio: u16,
    pub fd: i32,
    /// Union slot at offset 8: `off` | `addr2` | (`cmd_op`, `__pad1`).
    ///
    /// For `cmd_op`, write it in the low 32 bits and leave the high 32 zero.
    pub addr2: u64,
    /// Union slot at offset 16: `addr` | `splice_off_in` | (`level`, `optname`).
    ///
    /// For the socket-option pair, write `level | (optname << 32)`.
    pub addr: u64,
    /// Buffer size or iovec count. Doubles as the flag word for `PollAdd`
    /// ([`PollFlags`]) and as `mode` for `Openat`.
    pub len: u32,
    /// Union slot at offset 28: every `*_flags` arm of the C union —
    /// `rw_flags`, `fsync_flags`, `poll32_events`, `timeout_flags`,
    /// `msg_flags`, `cancel_flags`, `open_flags`, `nop_flags`, and the rest.
    ///
    /// The `rw_flags` arm is `__kernel_rwf_t`, i.e. a signed `int`; the `RWF_*`
    /// values are all small and positive, so they round-trip through `u32`.
    pub op_flags: u32,
    pub user_data: u64,
    /// Union slot at offset 40: `buf_index` | `buf_group`.
    pub buf_index: u16,
    pub personality: u16,
    /// Union slot at offset 44: `file_index` | `splice_fd_in` | `zcrx_ifq_idx`
    /// | `optlen` | (`addr_len`, `__pad3`).
    ///
    /// For the narrower arms, write the value into the low bits and leave the
    /// rest zero: `addr_len as u32`.
    ///
    /// As `file_index` this is a *1-based* direct descriptor slot; 0 means "not
    /// a fixed file". [`FILE_INDEX_ALLOC`] asks the kernel to pick a free slot.
    pub file_index: u32,
    /// Union slot at offset 48: `addr3` | `attr_ptr` | `optval`.
    pub addr3: u64,
    /// Union slot at offset 56: `__pad2[0]` | `attr_type_mask`.
    ///
    /// Must be zero unless the opcode reads it. As `attr_type_mask` it holds
    /// [`RwAttrFlags`] and pairs with [`Sqe::addr3`] as `attr_ptr`.
    pub addr3_hi: u64,
}

impl Sqe {
    /// An all-zero SQE, which is a well-formed `Nop`.
    pub const ZEROED: Sqe = Sqe {
        opcode: Opcode::Nop,
        flags: SqeFlags::empty(),
        ioprio: 0,
        fd: 0,
        addr2: 0,
        addr: 0,
        len: 0,
        op_flags: 0,
        user_data: 0,
        buf_index: 0,
        personality: 0,
        file_index: 0,
        addr3: 0,
        addr3_hi: 0,
    };

    /// A zeroed SQE with `opcode` set. Every other field is left for the caller.
    pub const fn new(opcode: Opcode) -> Sqe {
        Sqe {
            opcode,
            ..Sqe::ZEROED
        }
    }
}

impl Default for Sqe {
    fn default() -> Sqe {
        Sqe::ZEROED
    }
}

/// A 128-byte submission queue entry.
///
/// Under [`SetupFlags::SQE128`] this is the SQE array element type and the ring
/// stride is 128. Under [`SetupFlags::SQE_MIXED`] the stride stays 64 and this
/// overlays *two adjacent slots*, so it cannot straddle the ring wrap: a
/// 128-byte entry landing on the last slot needs a padding entry first.
///
/// The C union places `cmd[]` at offset 48, so the 80-byte command payload
/// overlaps [`Sqe::addr3`] and [`Sqe::addr3_hi`]. Use [`Sqe128::cmd`] to get the
/// full 80-byte region rather than writing `tail` directly.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct Sqe128 {
    pub sqe: Sqe,
    /// Bytes 64..128. The command payload proper starts 16 bytes earlier.
    pub tail: [u8; 64],
}

impl Sqe128 {
    pub const ZEROED: Sqe128 = Sqe128 {
        sqe: Sqe::ZEROED,
        tail: [0; 64],
    };

    pub const fn new(opcode: Opcode) -> Sqe128 {
        Sqe128 {
            sqe: Sqe::new(opcode),
            tail: [0; 64],
        }
    }

    /// The 80-byte `cmd[]` payload at offset 48.
    pub fn cmd(&self) -> &[u8; 80] {
        // SAFETY: offset 48 + 80 == 128 == size_of::<Sqe128>(), and [u8; 80]
        // has alignment 1, so the projection is in bounds and well aligned.
        unsafe {
            &*core::ptr::from_ref(self)
                .cast::<u8>()
                .add(48)
                .cast::<[u8; 80]>()
        }
    }

    /// Mutable view of the 80-byte `cmd[]` payload at offset 48.
    pub fn cmd_mut(&mut self) -> &mut [u8; 80] {
        // SAFETY: as above; `Sqe128` has no padding and every byte is init.
        unsafe {
            &mut *core::ptr::from_mut(self)
                .cast::<u8>()
                .add(48)
                .cast::<[u8; 80]>()
        }
    }
}

impl Default for Sqe128 {
    fn default() -> Sqe128 {
        Sqe128::ZEROED
    }
}

/// `IORING_FILE_INDEX_ALLOC`: let the kernel pick a free direct descriptor slot
/// for opcodes that install one. The chosen slot comes back in `cqe.res`.
pub const FILE_INDEX_ALLOC: u32 = !0;

/// `IORING_REGISTER_FILES_SKIP`: leave this fd table entry untouched during a
/// files update.
pub const REGISTER_FILES_SKIP: i32 = -2;

// ---------------------------------------------------------------------------
// Completion queue entry
// ---------------------------------------------------------------------------

/// `struct io_uring_cqe`, the 16-byte completion queue entry.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct Cqe {
    pub user_data: u64,
    pub res: i32,
    pub flags: CqeFlags,
}

impl Cqe {
    /// The provided-buffer ID, valid only when [`CqeFlags::BUFFER`] is set.
    pub fn buffer_id(&self) -> u16 {
        (self.flags.bits() >> CQE_BUFFER_SHIFT) as u16
    }
}

/// A 32-byte completion queue entry.
///
/// Under [`SetupFlags::CQE32`] this is the CQE array element type and the ring
/// stride is 32. Under [`SetupFlags::CQE_MIXED`] the stride stays 16 and this
/// overlays two adjacent slots, marked by [`CqeFlags::F32`]; the kernel pads the
/// wrap with a [`CqeFlags::SKIP`] entry when a 32-byte CQE will not fit.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct Cqe32 {
    pub cqe: Cqe,
    pub big_cqe: [u64; 2],
}

/// `IORING_CQE_BUFFER_SHIFT`: bit position of the buffer ID in `cqe.flags`.
pub const CQE_BUFFER_SHIFT: u32 = 16;

/// `IORING_CQE_F_TSTAMP_HW`. Deliberately not part of [`CqeFlags`]: it occupies
/// the same bit as the low bit of the provided-buffer ID and is only meaningful
/// for `SOCKET_URING_OP_TX_TIMESTAMP` completions.
pub const CQE_F_TSTAMP_HW: u32 = 1 << 16;

// ---------------------------------------------------------------------------
// mmap offsets
// ---------------------------------------------------------------------------

/// `IORING_OFF_SQ_RING`
pub const OFF_SQ_RING: u64 = 0;
/// `IORING_OFF_CQ_RING`
pub const OFF_CQ_RING: u64 = 0x8000000;
/// `IORING_OFF_SQES`
pub const OFF_SQES: u64 = 0x10000000;
/// `IORING_OFF_PBUF_RING`. OR in `bgid << OFF_PBUF_SHIFT`.
pub const OFF_PBUF_RING: u64 = 0x80000000;
/// `IORING_OFF_PBUF_SHIFT`
pub const OFF_PBUF_SHIFT: u32 = 16;
/// `IORING_OFF_MMAP_MASK`
pub const OFF_MMAP_MASK: u64 = 0xf8000000;

// ---------------------------------------------------------------------------
// Ring setup
// ---------------------------------------------------------------------------

/// `struct io_sqring_offsets`
#[repr(C)]
#[derive(Debug, Default, Clone, Copy)]
pub struct SqRingOffsets {
    pub head: u32,
    pub tail: u32,
    pub ring_mask: u32,
    pub ring_entries: u32,
    pub flags: u32,
    pub dropped: u32,
    pub array: u32,
    pub resv1: u32,
    pub user_addr: u64,
}

/// `struct io_cqring_offsets`
#[repr(C)]
#[derive(Debug, Default, Clone, Copy)]
pub struct CqRingOffsets {
    pub head: u32,
    pub tail: u32,
    pub ring_mask: u32,
    pub ring_entries: u32,
    pub overflow: u32,
    pub cqes: u32,
    pub flags: u32,
    pub resv1: u32,
    pub user_addr: u64,
}

/// `struct io_uring_params`
#[repr(C)]
#[derive(Debug, Default, Clone, Copy)]
pub struct Params {
    pub sq_entries: u32,
    pub cq_entries: u32,
    pub flags: SetupFlags,
    pub sq_thread_cpu: u32,
    pub sq_thread_idle: u32,
    pub features: Features,
    pub wq_fd: u32,
    pub resv: [u32; 3],
    pub sq_off: SqRingOffsets,
    pub cq_off: CqRingOffsets,
}

/// `struct __kernel_timespec`, as pointed at by `Timeout`, `LinkTimeout`,
/// [`SyncCancelReg::timeout`] and [`RegWait::ts`].
///
/// Note this is *not* libc's `timespec`: `tv_nsec` is 64-bit here on every
/// architecture.
#[repr(C)]
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Timespec {
    pub tv_sec: i64,
    pub tv_nsec: i64,
}

// ---------------------------------------------------------------------------
// Flags
// ---------------------------------------------------------------------------

bitflags! {
    /// `IOSQE_*`, [`Sqe::flags`].
    #[repr(transparent)]
    #[derive(Default, Debug, Clone, Copy, PartialEq, Eq)]
    pub struct SqeFlags : u8 {
        const FIXED_FILE = 1 << 0;
        const IO_DRAIN = 1 << 1;
        const IO_LINK = 1 << 2;
        const IO_HARDLINK = 1 << 3;
        const ASYNC = 1 << 4;
        const BUFFER_SELECT = 1 << 5;
        const SKIP_SUCCESS = 1 << 6;
    }

    /// `IORING_SETUP_*`, [`Params::flags`].
    #[repr(transparent)]
    #[derive(Default, Debug, Clone, Copy, PartialEq, Eq)]
    pub struct SetupFlags : u32 {
        const IOPOLL = 1 << 0;
        const SQPOLL = 1 << 1;
        const SQ_AFF = 1 << 2;
        const CQSIZE = 1 << 3;
        const CLAMP = 1 << 4;
        const ATTACH_WQ = 1 << 5;
        const R_DISABLED = 1 << 6;
        const SUBMIT_ALL = 1 << 7;
        const COOP_TASKRUN = 1 << 8;
        const TASKRUN_FLAG = 1 << 9;
        const SQE128 = 1 << 10;
        const CQE32 = 1 << 11;
        const SINGLE_ISSUER = 1 << 12;
        const DEFER_TASKRUN = 1 << 13;
        const NO_MMAP = 1 << 14;
        const REGISTERED_FD_ONLY = 1 << 15;
        const NO_SQARRAY = 1 << 16;
        const HYBRID_IOPOLL = 1 << 17;
        const CQE_MIXED = 1 << 18;
        const SQE_MIXED = 1 << 19;
        /// Requires [`SetupFlags::NO_SQARRAY`]; incompatible with `SQPOLL`.
        const SQ_REWIND = 1 << 20;
    }

    /// `IORING_ENTER_*`, the `flags` argument to `io_uring_enter(2)`.
    #[repr(transparent)]
    #[derive(Default, Debug, Clone, Copy, PartialEq, Eq)]
    pub struct EnterFlags : u32 {
        const GETEVENTS = 1 << 0;
        const SQ_WAKEUP = 1 << 1;
        const SQ_WAIT = 1 << 2;
        /// `arg` is a [`GeteventsArg`].
        const EXT_ARG = 1 << 3;
        const REGISTERED_RING = 1 << 4;
        const ABS_TIMER = 1 << 5;
        /// `arg` is an index into a registered [`RegWait`] region.
        const EXT_ARG_REG = 1 << 6;
        const NO_IOWAIT = 1 << 7;
    }

    /// `IORING_FEAT_*`, [`Params::features`]. Filled in by the kernel.
    #[repr(transparent)]
    #[derive(Default, Debug, Clone, Copy, PartialEq, Eq)]
    pub struct Features : u32 {
        const SINGLE_MMAP = 1 << 0;
        const NODROP = 1 << 1;
        const SUBMIT_STABLE = 1 << 2;
        const RW_CUR_POS = 1 << 3;
        const CUR_PERSONALITY = 1 << 4;
        const FAST_POLL = 1 << 5;
        const POLL_32BITS  = 1 << 6;
        const SQPOLL_NONFIXED = 1 << 7;
        const EXT_ARG = 1 << 8;
        const NATIVE_WORKERS = 1 << 9;
        const RSRC_TAGS = 1 << 10;
        const CQE_SKIP = 1 << 11;
        const LINKED_FILE = 1 << 12;
        const REG_REG_RING = 1 << 13;
        const RECVSEND_BUNDLE = 1 << 14;
        const MIN_TIMEOUT = 1 << 15;
        const RW_ATTR = 1 << 16;
        const NO_IOWAIT = 1 << 17;
    }

    /// `IORING_CQE_F_*`, [`Cqe::flags`].
    ///
    /// Bits 16..32 carry the provided-buffer ID; see [`Cqe::buffer_id`]. They
    /// show up as unknown bits here, which `bitflags` preserves.
    #[repr(transparent)]
    #[derive(Default, Debug, Clone, Copy, PartialEq, Eq)]
    pub struct CqeFlags : u32 {
        /// Upper 16 bits hold the buffer ID.
        const BUFFER = 1 << 0;
        /// The submission will produce further completions; keep it alive.
        const MORE = 1 << 1;
        const SOCK_NONEMPTY = 1 << 2;
        /// A zero-copy notification CQE rather than the send completion.
        const NOTIF = 1 << 3;
        /// Buffer was only partially consumed (`IOU_PBUF_RING_INC` rings).
        const BUF_MORE = 1 << 4;
        /// Padding CQE filling a wrap gap. Must be ignored entirely.
        const SKIP = 1 << 5;
        /// This is a 32-byte CQE on a [`SetupFlags::CQE_MIXED`] ring.
        const F32 = 1 << 15;
    }

    /// `IORING_SQ_*`, read from the SQ ring's `flags` word.
    #[repr(transparent)]
    #[derive(Default, Debug, Clone, Copy, PartialEq, Eq)]
    pub struct SqRingFlags : u32 {
        /// SQPOLL thread is idle; `io_uring_enter` with `SQ_WAKEUP` to restart it.
        const NEED_WAKEUP = 1 << 0;
        const CQ_OVERFLOW = 1 << 1;
        /// Task work is pending; enter the kernel to run it. Only set with
        /// [`SetupFlags::TASKRUN_FLAG`].
        const TASKRUN = 1 << 2;
    }

    /// `IORING_CQ_*`, read from the CQ ring's `flags` word.
    #[repr(transparent)]
    #[derive(Default, Debug, Clone, Copy, PartialEq, Eq)]
    pub struct CqRingFlags : u32 {
        const EVENTFD_DISABLED = 1 << 0;
    }

    /// `IORING_RSRC_REGISTER_*`, [`RsrcRegister::flags`].
    #[repr(transparent)]
    #[derive(Default, Debug, Clone, Copy, PartialEq, Eq)]
    pub struct RsrcRegisterFlags : u32 {
        const SPARSE = 1 << 0;
    }

    /// `IORING_REG_WAIT_*`, [`RegWait::flags`].
    #[repr(transparent)]
    #[derive(Default, Debug, Clone, Copy, PartialEq, Eq)]
    pub struct RegWaitFlags : u32 {
        const TS = 1 << 0;
    }

    /// `IOU_PBUF_RING_*`, [`BufReg::flags`].
    #[repr(transparent)]
    #[derive(Default, Debug, Clone, Copy, PartialEq, Eq)]
    pub struct PbufRingFlags : u16 {
        /// Kernel allocates the ring; mmap it at
        /// `OFF_PBUF_RING | (bgid << OFF_PBUF_SHIFT)`.
        const MMAP = 1;
        /// Buffers are consumed incrementally. Pairs with [`CqeFlags::BUF_MORE`].
        const INC = 2;
    }
}

// ---------------------------------------------------------------------------
// Per-opcode flags
// ---------------------------------------------------------------------------

bitflags! {
    /// `IORING_TIMEOUT_*` and `IORING_LINK_TIMEOUT_UPDATE`, in [`Sqe::op_flags`]
    /// for `Timeout`, `TimeoutRemove` and `LinkTimeout`.
    #[repr(transparent)]
    #[derive(Default, Debug, Clone, Copy, PartialEq, Eq)]
    pub struct TimeoutFlags : u32 {
        const ABS = 1 << 0;
        const UPDATE = 1 << 1;
        const BOOTTIME = 1 << 2;
        const REALTIME = 1 << 3;
        const LINK_TIMEOUT_UPDATE = 1 << 4;
        /// Complete with 0 rather than `-ETIME` when the timer expires.
        const ETIME_SUCCESS = 1 << 5;
        const MULTISHOT = 1 << 6;
        /// [`Sqe::addr`] holds the timeout directly, in nanoseconds, instead of
        /// pointing at a [`Timespec`].
        ///
        /// Newer than this machine's `/usr/include/linux/io_uring.h`, so a
        /// kernel matching the distro headers will reject it. Probe first.
        const IMMEDIATE_ARG = 1 << 7;

        /// `IORING_TIMEOUT_CLOCK_MASK`
        const CLOCK_MASK = Self::BOOTTIME.bits() | Self::REALTIME.bits();
        /// `IORING_TIMEOUT_UPDATE_MASK`
        const UPDATE_MASK = Self::UPDATE.bits() | Self::LINK_TIMEOUT_UPDATE.bits();
    }

    /// `IORING_ASYNC_CANCEL_*`, in [`Sqe::op_flags`] for `AsyncCancel` and in
    /// [`SyncCancelReg::flags`].
    #[repr(transparent)]
    #[derive(Default, Debug, Clone, Copy, PartialEq, Eq)]
    pub struct AsyncCancelFlags : u32 {
        const ALL = 1 << 0;
        const FD = 1 << 1;
        const ANY = 1 << 2;
        const FD_FIXED = 1 << 3;
        const USERDATA = 1 << 4;
        const OP = 1 << 5;
    }

    /// `IORING_POLL_*`, in [`Sqe::len`] for `PollAdd` — *not* `op_flags`, which
    /// `PollAdd` uses for the poll event mask.
    #[repr(transparent)]
    #[derive(Default, Debug, Clone, Copy, PartialEq, Eq)]
    pub struct PollFlags : u32 {
        /// Multishot poll; completions carry [`CqeFlags::MORE`].
        const ADD_MULTI = 1 << 0;
        const UPDATE_EVENTS = 1 << 1;
        const UPDATE_USER_DATA = 1 << 2;
        const ADD_LEVEL = 1 << 3;
    }

    /// `IORING_RECVSEND_*` / `IORING_RECV_*` / `IORING_SEND_*`, in
    /// [`Sqe::ioprio`] for the send and recv family.
    #[repr(transparent)]
    #[derive(Default, Debug, Clone, Copy, PartialEq, Eq)]
    pub struct RecvSendFlags : u16 {
        /// Arm poll upfront instead of trying the transfer first.
        const POLL_FIRST = 1 << 0;
        const RECV_MULTISHOT = 1 << 1;
        const FIXED_BUF = 1 << 2;
        const SEND_ZC_REPORT_USAGE = 1 << 3;
        /// With [`SqeFlags::BUFFER_SELECT`], consume several buffers at once.
        const BUNDLE = 1 << 4;
        /// `addr` points at an iovec array rather than a flat buffer.
        const SEND_VECTORIZED = 1 << 5;
    }

    /// `IORING_ACCEPT_*`, in [`Sqe::ioprio`] for `Accept`.
    #[repr(transparent)]
    #[derive(Default, Debug, Clone, Copy, PartialEq, Eq)]
    pub struct AcceptFlags : u16 {
        const MULTISHOT = 1 << 0;
        const DONTWAIT = 1 << 1;
        const POLL_FIRST = 1 << 2;
    }

    /// `IORING_FSYNC_*`, in [`Sqe::op_flags`] for `Fsync`.
    #[repr(transparent)]
    #[derive(Default, Debug, Clone, Copy, PartialEq, Eq)]
    pub struct FsyncFlags : u32 {
        const DATASYNC = 1 << 0;
    }

    /// `IORING_MSG_RING_*`, in [`Sqe::op_flags`] for `MsgRing`.
    #[repr(transparent)]
    #[derive(Default, Debug, Clone, Copy, PartialEq, Eq)]
    pub struct MsgRingFlags : u32 {
        /// Don't post a CQE on the target ring. Not valid with [`MsgRingOp::Data`].
        const CQE_SKIP = 1 << 0;
        /// Pass [`Sqe::file_index`] through to the target `cqe.flags`.
        const FLAGS_PASS = 1 << 1;
    }

    /// `IORING_RW_ATTR_FLAG_*`, in [`Sqe::addr3_hi`] as `attr_type_mask`.
    /// Gated by [`Features::RW_ATTR`].
    #[repr(transparent)]
    #[derive(Default, Debug, Clone, Copy, PartialEq, Eq)]
    pub struct RwAttrFlags : u64 {
        const PI = 1 << 0;
    }
}

/// `enum io_uring_msg_ring_flags`, written into [`Sqe::addr`] for `MsgRing`.
#[repr(u64)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MsgRingOp {
    /// Pass `sqe.len` as the target `res` and `sqe.addr2` as its `user_data`.
    Data = 0,
    SendFd = 1,
}

/// `SPLICE_F_FD_IN_FIXED`, in [`Sqe::op_flags`] for `Splice` and `Tee`.
pub const SPLICE_F_FD_IN_FIXED: u32 = 1 << 31;

/// `IORING_NOTIF_USAGE_ZC_COPIED`: set in `cqe.res` of a [`CqeFlags::NOTIF`]
/// completion when the zero-copy send fell back to copying.
pub const NOTIF_USAGE_ZC_COPIED: u32 = 1 << 31;

// ---------------------------------------------------------------------------
// io_uring_register
// ---------------------------------------------------------------------------

/// `enum io_uring_register_op`.
#[repr(u32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RegisterOp {
    Buffers = 0,
    UnregisterBuffers = 1,
    Files = 2,
    UnregisterFiles = 3,
    Eventfd = 4,
    UnregisterEventfd = 5,
    FilesUpdate = 6,
    EventfdAsync = 7,
    Probe = 8,
    Personality = 9,
    UnregisterPersonality = 10,
    Restrictions = 11,
    EnableRings = 12,
    /// Tagged variant of [`RegisterOp::Files`]; takes a [`RsrcRegister`].
    Files2 = 13,
    /// Takes a [`RsrcUpdate2`].
    FilesUpdate2 = 14,
    Buffers2 = 15,
    BuffersUpdate = 16,
    IowqAff = 17,
    UnregisterIowqAff = 18,
    IowqMaxWorkers = 19,
    RingFds = 20,
    UnregisterRingFds = 21,
    /// Takes a [`BufReg`].
    PbufRing = 22,
    UnregisterPbufRing = 23,
    /// Takes a [`SyncCancelReg`].
    SyncCancel = 24,
    /// Takes a [`FileIndexRange`].
    FileAllocRange = 25,
    /// Takes a [`BufStatus`].
    PbufStatus = 26,
    Napi = 27,
    UnregisterNapi = 28,
    Clock = 29,
    CloneBuffers = 30,
    SendMsgRing = 31,
    ZcrxIfq = 32,
    ResizeRings = 33,
    MemRegion = 34,
    /// See `linux/io_uring/query.h`.
    Query = 35,
    ZcrxCtrl = 36,
    BpfFilter = 37,
}

/// `IORING_REGISTER_USE_REGISTERED_RING`: OR into the [`RegisterOp`] value to
/// pass a registered ring index instead of an fd.
pub const REGISTER_USE_REGISTERED_RING: u32 = 1 << 31;

/// `struct io_uring_rsrc_register`
#[repr(C)]
#[derive(Debug, Default, Clone, Copy)]
pub struct RsrcRegister {
    pub nr: u32,
    pub flags: RsrcRegisterFlags,
    pub resv: u64,
    pub data: u64,
    pub tags: u64,
}

/// `struct io_uring_rsrc_update`
#[repr(C)]
#[derive(Debug, Default, Clone, Copy)]
pub struct RsrcUpdate {
    pub offset: u32,
    pub resv: u32,
    pub data: u64,
}

/// `struct io_uring_rsrc_update2`
#[repr(C)]
#[derive(Debug, Default, Clone, Copy)]
pub struct RsrcUpdate2 {
    pub offset: u32,
    pub resv: u32,
    pub data: u64,
    pub tags: u64,
    pub nr: u32,
    pub resv2: u32,
}

/// `struct io_uring_file_index_range`, for [`RegisterOp::FileAllocRange`].
#[repr(C)]
#[derive(Debug, Default, Clone, Copy)]
pub struct FileIndexRange {
    pub off: u32,
    pub len: u32,
    pub resv: u64,
}

/// `IO_URING_OP_SUPPORTED`, [`ProbeOp::flags`].
pub const PROBE_OP_SUPPORTED: u16 = 1 << 0;

/// `struct io_uring_probe_op`
#[repr(C)]
#[derive(Debug, Default, Clone, Copy)]
pub struct ProbeOp {
    pub op: u8,
    pub resv: u8,
    pub flags: u16,
    pub resv2: u32,
}

/// `struct io_uring_probe`, for [`RegisterOp::Probe`].
///
/// The C type ends in a flexible `ops[]` array; allocate
/// `size_of::<Probe>() + n * size_of::<ProbeOp>()` and index past the header.
#[repr(C)]
#[derive(Debug, Default, Clone, Copy)]
pub struct Probe {
    pub last_op: u8,
    pub ops_len: u8,
    pub resv: u16,
    pub resv2: [u32; 3],
}

/// `struct io_uring_sync_cancel_reg`, for [`RegisterOp::SyncCancel`].
#[repr(C)]
#[derive(Debug, Default, Clone, Copy)]
pub struct SyncCancelReg {
    pub addr: u64,
    pub fd: i32,
    pub flags: AsyncCancelFlags,
    pub timeout: Timespec,
    pub opcode: u8,
    pub pad: [u8; 7],
    pub pad2: [u64; 3],
}

// ---------------------------------------------------------------------------
// Provided buffer rings
// ---------------------------------------------------------------------------

/// `struct io_uring_buf`, one entry in a provided-buffer ring.
#[repr(C)]
#[derive(Debug, Default, Clone, Copy)]
pub struct Buf {
    pub addr: u64,
    pub len: u32,
    pub bid: u16,
    pub resv: u16,
}

/// `struct io_uring_buf_ring` header.
///
/// The ring is an array of [`Buf`]; this header is overlaid on entry 0, with
/// `tail` sharing storage with that entry's `resv` field. Entry 0 is therefore
/// usable as a buffer only once the tail has been read.
#[repr(C)]
#[derive(Debug, Default, Clone, Copy)]
pub struct BufRing {
    pub resv1: u64,
    pub resv2: u32,
    pub resv3: u16,
    pub tail: u16,
}

/// `struct io_uring_buf_reg`, for [`RegisterOp::PbufRing`].
#[repr(C)]
#[derive(Debug, Default, Clone, Copy)]
pub struct BufReg {
    pub ring_addr: u64,
    pub ring_entries: u32,
    pub bgid: u16,
    pub flags: PbufRingFlags,
    pub min_left: u32,
    pub resv: [u32; 5],
}

/// `struct io_uring_buf_status`, for [`RegisterOp::PbufStatus`].
#[repr(C)]
#[derive(Debug, Default, Clone, Copy)]
pub struct BufStatus {
    /// Input: the buffer group to query.
    pub buf_group: u32,
    /// Output: the kernel's current head index.
    pub head: u32,
    pub resv: [u32; 8],
}

// ---------------------------------------------------------------------------
// io_uring_enter extended arguments
// ---------------------------------------------------------------------------

/// `struct io_uring_getevents_arg`, the `arg` for `io_uring_enter(2)` with
/// [`EnterFlags::EXT_ARG`]. Gated by [`Features::EXT_ARG`].
///
/// `ts` is a userspace pointer to a [`Timespec`].
#[repr(C)]
#[derive(Debug, Default, Clone, Copy)]
pub struct GeteventsArg {
    pub sigmask: u64,
    pub sigmask_sz: u32,
    /// Gated by [`Features::MIN_TIMEOUT`].
    pub min_wait_usec: u32,
    pub ts: u64,
}

/// `struct io_uring_reg_wait`, an entry in a registered wait region.
///
/// Register the region with [`RegisterOp::MemRegion`] and
/// [`MEM_REGION_REG_WAIT_ARG`], then pass an *index* into it as the `arg` to
/// `io_uring_enter(2)` with [`EnterFlags::EXT_ARG_REG`]. This avoids copying the
/// wait arguments into the kernel on every enter.
#[repr(C)]
#[derive(Debug, Default, Clone, Copy)]
pub struct RegWait {
    pub ts: Timespec,
    pub min_wait_usec: u32,
    pub flags: RegWaitFlags,
    pub sigmask: u64,
    pub sigmask_sz: u32,
    pub pad: [u32; 3],
    pub pad2: [u64; 2],
}

/// `struct io_uring_region_desc`
#[repr(C)]
#[derive(Debug, Default, Clone, Copy)]
pub struct RegionDesc {
    pub user_addr: u64,
    pub size: u64,
    /// [`MEM_REGION_TYPE_USER`] to use caller-provided memory.
    pub flags: u32,
    pub id: u32,
    pub mmap_offset: u64,
    pub resv: [u64; 4],
}

/// `struct io_uring_mem_region_reg`, for [`RegisterOp::MemRegion`].
#[repr(C)]
#[derive(Debug, Default, Clone, Copy)]
pub struct MemRegionReg {
    /// Pointer to a [`RegionDesc`].
    pub region_uptr: u64,
    pub flags: u64,
    pub resv: [u64; 2],
}

/// `IORING_MEM_REGION_TYPE_USER`, [`RegionDesc::flags`].
pub const MEM_REGION_TYPE_USER: u32 = 1;

/// `IORING_MEM_REGION_REG_WAIT_ARG`, [`MemRegionReg::flags`]: expose the region
/// as the [`RegWait`] array for [`EnterFlags::EXT_ARG_REG`].
pub const MEM_REGION_REG_WAIT_ARG: u64 = 1;

// ---------------------------------------------------------------------------
// Remaining per-opcode flags
// ---------------------------------------------------------------------------

bitflags! {
    /// `IORING_URING_CMD_*`, in [`Sqe::op_flags`] for `UringCmd`.
    ///
    /// The top 8 bits of the field are reserved for the kernel.
    #[repr(transparent)]
    #[derive(Default, Debug, Clone, Copy, PartialEq, Eq)]
    pub struct UringCmdFlags : u32 {
        /// Use a registered buffer; set [`Sqe::buf_index`] alongside this.
        const FIXED = 1 << 0;

        /// `IORING_URING_CMD_MASK`
        const MASK = Self::FIXED.bits();
    }

    /// `IORING_NOP_*`, in [`Sqe::op_flags`] for `Nop`.
    #[repr(transparent)]
    #[derive(Default, Debug, Clone, Copy, PartialEq, Eq)]
    pub struct NopFlags : u32 {
        /// Complete with the result taken from [`Sqe::len`].
        const INJECT_RESULT = 1 << 0;
        const CQE32 = 1 << 5;
    }

    /// `IORING_FIXED_FD_*`, in [`Sqe::op_flags`] for `FixedFdInstall`.
    #[repr(transparent)]
    #[derive(Default, Debug, Clone, Copy, PartialEq, Eq)]
    pub struct InstallFdFlags : u32 {
        const NO_CLOEXEC = 1 << 0;
    }

    /// `IORING_REGISTER_SRC_REGISTERED` / `IORING_REGISTER_DST_REPLACE`,
    /// [`CloneBuffers::flags`].
    #[repr(transparent)]
    #[derive(Default, Debug, Clone, Copy, PartialEq, Eq)]
    pub struct CloneBuffersFlags : u32 {
        /// `src_fd` is a registered ring index rather than an fd.
        const SRC_REGISTERED = 1 << 0;
        /// Replace existing entries in the destination table.
        const DST_REPLACE = 1 << 1;
    }
}

/// `struct io_uring_attr_pi`, pointed at by [`Sqe::addr3`] as `attr_ptr` when
/// [`RwAttrFlags::PI`] is set in [`Sqe::addr3_hi`].
#[repr(C)]
#[derive(Debug, Default, Clone, Copy)]
pub struct AttrPi {
    pub flags: u16,
    pub app_tag: u16,
    pub len: u32,
    pub addr: u64,
    pub seed: u64,
    pub rsvd: u64,
}

// ---------------------------------------------------------------------------
// Registration: restrictions, workers, clocks, buffer cloning, napi
// ---------------------------------------------------------------------------

/// `enum io_wq_type`. Indexes the two-element array passed to
/// [`RegisterOp::IowqMaxWorkers`].
#[repr(u32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WqType {
    Bound = 0,
    Unbound = 1,
}

/// `enum io_uring_register_restriction_op`, [`Restriction::opcode`].
#[repr(u16)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RestrictionOp {
    /// Allow one [`RegisterOp`]; [`Restriction::arg`] is its value.
    Register = 0,
    /// Allow one [`Opcode`]; [`Restriction::arg`] is its value.
    SqeOp = 1,
    /// [`Restriction::arg`] is a [`SqeFlags`] mask of permitted flags.
    SqeFlagsAllowed = 2,
    /// [`Restriction::arg`] is a [`SqeFlags`] mask required on every submission.
    SqeFlagsRequired = 3,
}

/// `struct io_uring_restriction`, for [`RegisterOp::Restrictions`].
#[repr(C)]
#[derive(Debug, Default, Clone, Copy)]
pub struct Restriction {
    /// A [`RestrictionOp`].
    pub opcode: u16,
    /// Union slot at offset 2: `register_op` | `sqe_op` | `sqe_flags`, selected
    /// by [`Restriction::opcode`].
    pub arg: u8,
    pub resv: u8,
    pub resv2: [u32; 3],
}

/// `struct io_uring_task_restriction` header.
///
/// The C type ends in a flexible `restrictions[]` array; allocate
/// `size_of::<TaskRestriction>() + nr_res * size_of::<Restriction>()`.
#[repr(C)]
#[derive(Debug, Default, Clone, Copy)]
pub struct TaskRestriction {
    pub flags: u16,
    pub nr_res: u16,
    pub resv: [u32; 3],
}

/// `struct io_uring_clock_register`, for [`RegisterOp::Clock`]. Selects the
/// clock source used by timeouts on this ring.
#[repr(C)]
#[derive(Debug, Default, Clone, Copy)]
pub struct ClockRegister {
    pub clockid: u32,
    pub resv: [u32; 3],
}

/// `struct io_uring_clone_buffers`, for [`RegisterOp::CloneBuffers`].
#[repr(C)]
#[derive(Debug, Default, Clone, Copy)]
pub struct CloneBuffers {
    pub src_fd: u32,
    pub flags: CloneBuffersFlags,
    pub src_off: u32,
    pub dst_off: u32,
    pub nr: u32,
    pub pad: [u32; 3],
}

/// `struct io_uring_napi`, for [`RegisterOp::Napi`].
///
/// liburing's copy predates the kernel's static-tracking rework: the distro
/// header replaces `pad`/`resv` with `opcode`, `pad[2]`, `op_param` and `resv`,
/// and adds `io_uring_napi_op` / `io_uring_napi_tracking_strategy`. The size is
/// unchanged, so a newer kernel reads the trailing bytes as `opcode == 0`
/// (register) with `op_param == 0` (dynamic tracking).
#[repr(C)]
#[derive(Debug, Default, Clone, Copy)]
pub struct Napi {
    pub busy_poll_to: u32,
    pub prefer_busy_poll: u8,
    pub pad: [u8; 3],
    pub resv: u64,
}

/// `struct io_uring_files_update`. Deprecated in favour of [`RsrcUpdate`].
#[repr(C)]
#[derive(Debug, Default, Clone, Copy)]
pub struct FilesUpdate {
    pub offset: u32,
    pub resv: u32,
    /// Pointer to an array of `i32`.
    pub fds: u64,
}

// ---------------------------------------------------------------------------
// Socket commands
// ---------------------------------------------------------------------------

/// `enum io_uring_socket_op`, written into [`Sqe::addr2`] as `cmd_op` for
/// `UringCmd` on a socket.
#[repr(u32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SocketOp {
    Siocinq = 0,
    Siocoutq = 1,
    /// [`Sqe::addr`] holds `level | (optname << 32)`, [`Sqe::addr3`] is
    /// `optval` and [`Sqe::file_index`] is `optlen`.
    Getsockopt = 2,
    Setsockopt = 3,
    TxTimestamp = 4,
    Getsockname = 5,
}

/// `struct io_uring_recvmsg_out`, the header written into the buffer by a
/// `Recvmsg` completion.
#[repr(C)]
#[derive(Debug, Default, Clone, Copy)]
pub struct RecvmsgOut {
    pub namelen: u32,
    pub controllen: u32,
    pub payloadlen: u32,
    pub flags: u32,
}

/// `struct io_timespec`, as returned by [`SocketOp::TxTimestamp`].
///
/// Unlike [`Timespec`] both fields are unsigned.
#[repr(C)]
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct IoTimespec {
    pub tv_sec: u64,
    pub tv_nsec: u64,
}

/// `IORING_TIMESTAMP_HW_SHIFT`: see [`CQE_F_TSTAMP_HW`].
pub const TIMESTAMP_HW_SHIFT: u32 = 16;

/// `IORING_TIMESTAMP_TYPE_SHIFT`: bit of `cqe.flags` holding the timestamp type.
pub const TIMESTAMP_TYPE_SHIFT: u32 = TIMESTAMP_HW_SHIFT + 1;

// ---------------------------------------------------------------------------
// Zero-copy receive
// ---------------------------------------------------------------------------

bitflags! {
    /// `enum io_uring_zcrx_area_flags`, [`ZcrxAreaReg::flags`].
    #[repr(transparent)]
    #[derive(Default, Debug, Clone, Copy, PartialEq, Eq)]
    pub struct ZcrxAreaFlags : u32 {
        /// The area is backed by the dmabuf in [`ZcrxAreaReg::dmabuf_fd`].
        const DMABUF = 1;
    }

    /// `enum zcrx_reg_flags`, [`ZcrxIfqReg::flags`].
    #[repr(transparent)]
    #[derive(Default, Debug, Clone, Copy, PartialEq, Eq)]
    pub struct ZcrxRegFlags : u32 {
        const IMPORT = 1;
        /// Register without a net device; all data is copied and the refill
        /// queue may need an explicit [`ZcrxCtrlOp::FlushRq`].
        const NODEV = 2;
    }

    /// `enum zcrx_features`, reported by the query interface.
    #[repr(transparent)]
    #[derive(Default, Debug, Clone, Copy, PartialEq, Eq)]
    pub struct ZcrxFeatures : u32 {
        /// [`ZcrxIfqReg::rx_buf_len`] is honoured as a page-size request.
        const RX_PAGE_SIZE = 1 << 0;
    }
}

/// `struct io_uring_zcrx_rqe`, an entry in the refill queue.
#[repr(C)]
#[derive(Debug, Default, Clone, Copy)]
pub struct ZcrxRqe {
    pub off: u64,
    pub len: u32,
    pub pad: u32,
}

/// `struct io_uring_zcrx_cqe`, the 16-byte extension of a `RecvZc` completion
/// on a [`SetupFlags::CQE32`] ring.
#[repr(C)]
#[derive(Debug, Default, Clone, Copy)]
pub struct ZcrxCqe {
    pub off: u64,
    pub pad: u64,
}

/// `IORING_ZCRX_AREA_SHIFT`: bit from which the area id is encoded into offsets.
pub const ZCRX_AREA_SHIFT: u64 = 48;

/// `IORING_ZCRX_AREA_MASK`
pub const ZCRX_AREA_MASK: u64 = !((1u64 << ZCRX_AREA_SHIFT) - 1);

/// `struct io_uring_zcrx_offsets`
#[repr(C)]
#[derive(Debug, Default, Clone, Copy)]
pub struct ZcrxOffsets {
    pub head: u32,
    pub tail: u32,
    pub rqes: u32,
    pub resv2: u32,
    pub resv: [u64; 2],
}

/// `struct io_uring_zcrx_area_reg`
#[repr(C)]
#[derive(Debug, Default, Clone, Copy)]
pub struct ZcrxAreaReg {
    pub addr: u64,
    pub len: u64,
    pub rq_area_token: u64,
    pub flags: ZcrxAreaFlags,
    pub dmabuf_fd: u32,
    pub resv2: [u64; 2],
}

/// `struct io_uring_zcrx_ifq_reg`, for [`RegisterOp::ZcrxIfq`].
#[repr(C)]
#[derive(Debug, Default, Clone, Copy)]
pub struct ZcrxIfqReg {
    pub if_idx: u32,
    pub if_rxq: u32,
    pub rq_entries: u32,
    pub flags: ZcrxRegFlags,
    /// Pointer to a [`ZcrxAreaReg`].
    pub area_ptr: u64,
    /// Pointer to a [`RegionDesc`].
    pub region_ptr: u64,
    pub offsets: ZcrxOffsets,
    pub zcrx_id: u32,
    /// Requested rx page size; honoured only with [`ZcrxFeatures::RX_PAGE_SIZE`].
    pub rx_buf_len: u32,
    pub resv: [u64; 3],
}

/// `enum zcrx_ctrl_op`, [`ZcrxCtrl::op`].
#[repr(u32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ZcrxCtrlOp {
    FlushRq = 0,
    Export = 1,
}

/// `struct zcrx_ctrl`, for [`RegisterOp::ZcrxCtrl`].
///
/// The trailing 48 bytes are a union of `zcrx_ctrl_export` and
/// `zcrx_ctrl_flush_rq`; only the export arm carries data, so it is named here.
/// For [`ZcrxCtrlOp::FlushRq`] leave the whole tail zeroed.
#[repr(C)]
#[derive(Debug, Default, Clone, Copy)]
pub struct ZcrxCtrl {
    pub zcrx_id: u32,
    /// A [`ZcrxCtrlOp`].
    pub op: u32,
    pub resv: [u64; 2],
    /// Union slot at offset 24: `zcrx_ctrl_export::zcrx_fd`.
    pub zcrx_fd: u32,
    /// Union slot at offset 28: `zcrx_ctrl_export::__resv1`.
    pub payload_resv: [u32; 11],
}

// ---------------------------------------------------------------------------
// Layout assertions against the C header
// ---------------------------------------------------------------------------

const _: () = {
    use core::mem::{align_of, offset_of, size_of};

    assert!(size_of::<Sqe>() == 64 && align_of::<Sqe>() == 8);
    assert!(offset_of!(Sqe, fd) == 4);
    assert!(offset_of!(Sqe, addr2) == 8);
    assert!(offset_of!(Sqe, addr) == 16);
    assert!(offset_of!(Sqe, len) == 24);
    assert!(offset_of!(Sqe, op_flags) == 28);
    assert!(offset_of!(Sqe, user_data) == 32);
    assert!(offset_of!(Sqe, buf_index) == 40);
    assert!(offset_of!(Sqe, personality) == 42);
    assert!(offset_of!(Sqe, file_index) == 44);
    assert!(offset_of!(Sqe, addr3) == 48);
    assert!(offset_of!(Sqe, addr3_hi) == 56);
    assert!(size_of::<Sqe128>() == 128 && align_of::<Sqe128>() == 8);

    assert!(size_of::<Cqe>() == 16 && align_of::<Cqe>() == 8);
    assert!(size_of::<Cqe32>() == 32);

    assert!(size_of::<Params>() == 120);
    assert!(size_of::<SqRingOffsets>() == 40);
    assert!(size_of::<CqRingOffsets>() == 40);
    assert!(size_of::<Timespec>() == 16);

    assert!(size_of::<RsrcRegister>() == 32);
    assert!(size_of::<RsrcUpdate>() == 16);
    assert!(size_of::<RsrcUpdate2>() == 32);
    assert!(size_of::<FileIndexRange>() == 16);
    assert!(size_of::<ProbeOp>() == 8);
    assert!(size_of::<Probe>() == 16);
    assert!(size_of::<SyncCancelReg>() == 64);

    assert!(size_of::<Buf>() == 16);
    assert!(size_of::<BufRing>() == 16);
    assert!(offset_of!(BufRing, tail) == 14);
    assert!(size_of::<BufReg>() == 40);
    assert!(size_of::<BufStatus>() == 40);

    assert!(size_of::<GeteventsArg>() == 24);
    assert!(size_of::<RegWait>() == 64);
    assert!(size_of::<RegionDesc>() == 64);
    assert!(size_of::<MemRegionReg>() == 32);

    assert!(size_of::<AttrPi>() == 32);
    assert!(size_of::<Restriction>() == 16);
    assert!(size_of::<TaskRestriction>() == 16);
    assert!(size_of::<ClockRegister>() == 16);
    assert!(size_of::<CloneBuffers>() == 32);
    assert!(size_of::<Napi>() == 16);
    assert!(size_of::<FilesUpdate>() == 16);
    assert!(size_of::<RecvmsgOut>() == 16);
    assert!(size_of::<IoTimespec>() == 16);

    assert!(size_of::<ZcrxRqe>() == 16);
    assert!(size_of::<ZcrxCqe>() == 16);
    assert!(size_of::<ZcrxOffsets>() == 32);
    assert!(size_of::<ZcrxAreaReg>() == 48);
    assert!(size_of::<ZcrxIfqReg>() == 96);
    assert!(offset_of!(ZcrxIfqReg, offsets) == 32);
    assert!(size_of::<ZcrxCtrl>() == 72);
    assert!(offset_of!(ZcrxCtrl, zcrx_fd) == 24);
};
