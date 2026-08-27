//! Buffers that can be handed to the kernel for the length of an operation.
//!
//! Submitting an operation gives the kernel an address to read from or write
//! to, and the kernel keeps that address until the completion arrives —
//! dropping the future does not take it back. A borrowed slice cannot express
//! that, so operations here take a buffer *by value* and give it back with the
//! result:
//!
//! ```ignore
//! let (n, buf) = stream.read(vec![0u8; 4096]).await;
//! ```
//!
//! [`IoBuf`] and [`IoBufMut`] are the contract a type has to meet to be handed
//! over that way. Both are `unsafe` to implement, and the condition is the same
//! for both: the address must survive a move of the buffer value itself, which
//! it does for a heap allocation or a `'static` borrow, and does not for an
//! inline array. That restriction is what keeps an operation from having to
//! heap-allocate its own state just to pin a buffer down.
//!
//! # Reads overwrite from the start
//!
//! A read writes at offset zero, not at [`IoBuf::bytes_init`], and it does not
//! grow the buffer — it fills up to [`IoBufMut::bytes_total`] and reports how
//! far it got. Reading into a buffer that already holds data overwrites it. To
//! append, hand over a [`Slice`] instead:
//!
//! ```ignore
//! let (n, slice) = stream.read(buf.slice(len..)).await;
//! let buf = slice.into_inner();
//! ```

#![allow(dead_code)]

use std::io;
use std::ops::{Bound, RangeBounds};

/// The result of an operation, alongside the buffer it borrowed.
///
/// The buffer comes back whether the operation succeeded or failed, because the
/// kernel is finished with it either way and there is nowhere else for it to
/// go.
pub type BufResult<T, B> = (io::Result<T>, B);

/// A buffer the kernel may read from while an operation is in flight.
///
/// # Safety
///
/// [`read_ptr`](IoBuf::read_ptr) must return the same address for the whole
/// life of the value, *including across a move of it*, and the first
/// [`bytes_init`](IoBuf::bytes_init) bytes at that address must be initialised
/// and stay valid until the value is dropped.
///
/// That rules out anything storing its bytes inline: `[u8; N]` moves its
/// contents with it, so its address is only stable until the operation's future
/// is moved. `Vec<u8>`, `Box<[u8]>` and `&'static [u8]` all point away from
/// themselves and are fine.
pub unsafe trait IoBuf: Unpin + 'static {
    /// The address the kernel reads from.
    fn read_ptr(&self) -> *const u8;

    /// How many bytes at that address are initialised.
    fn bytes_init(&self) -> usize;

    /// Take a sub-range of this buffer by value.
    ///
    /// The owned equivalent of `&buf[range]`, which is unavailable here because
    /// the operation needs the buffer itself rather than a borrow of it.
    ///
    /// An open end stays open rather than being pinned to the length now, so
    /// `buf.slice(n..)` means "the initialised bytes past `n`" when read from
    /// and "the writable bytes past `n`" when written into. That is what makes
    /// it the append idiom.
    ///
    /// # Panics
    ///
    /// Panics if the range runs backwards, or if it starts past
    /// [`IoBuf::bytes_init`] — a gap there would leave uninitialised bytes
    /// below whatever [`IoBufMut::set_init`] later records.
    fn slice(self, range: impl RangeBounds<usize>) -> Slice<Self>
    where
        Self: Sized,
    {
        let (begin, end) = resolve(range);
        assert!(
            begin <= self.bytes_init(),
            "slice starts at {begin}, past the {} initialised bytes",
            self.bytes_init()
        );
        Slice {
            buf: self,
            begin,
            end,
        }
    }
}

/// A buffer the kernel may write into while an operation is in flight.
///
/// # Safety
///
/// As [`IoBuf`], for [`write_ptr`](IoBufMut::write_ptr): the address must
/// survive a move, and the whole [`bytes_total`](IoBufMut::bytes_total) bytes
/// at it must be writable for the life of the value — not merely the
/// initialised prefix, since that is the point.
pub unsafe trait IoBufMut: IoBuf {
    /// The address the kernel writes to. The same address
    /// [`IoBuf::read_ptr`] reports.
    fn write_ptr(&mut self) -> *mut u8;

    /// How many bytes may be written there, initialised or not.
    fn bytes_total(&self) -> usize;

    /// Record that the first `n` bytes are now initialised.
    ///
    /// # Safety
    ///
    /// `n` bytes at [`write_ptr`](IoBufMut::write_ptr) must actually have been
    /// written, and `n` must not exceed
    /// [`bytes_total`](IoBufMut::bytes_total).
    unsafe fn set_init(&mut self, n: usize);
}

// ---------------------------------------------------------------------------
// Slice
// ---------------------------------------------------------------------------

/// A sub-range of a buffer, owning the buffer it was taken from.
///
/// Produced by [`IoBuf::slice`]. Hand it to an operation the way you would the
/// buffer itself, then [`into_inner`](Slice::into_inner) to get the whole
/// buffer back.
#[derive(Debug)]
pub struct Slice<B> {
    buf: B,
    begin: usize,
    /// The exclusive end, or `None` for "as far as the buffer goes" — which is
    /// [`IoBuf::bytes_init`] when read from and [`IoBufMut::bytes_total`] when
    /// written into, so one open-ended slice serves both.
    end: Option<usize>,
}

impl<B> Slice<B> {
    /// Where this range starts in the underlying buffer.
    pub fn begin(&self) -> usize {
        self.begin
    }

    /// Where this range ends, or `None` if it runs to the end of the buffer.
    pub fn end(&self) -> Option<usize> {
        self.end
    }

    /// The whole buffer, range discarded.
    pub fn into_inner(self) -> B {
        self.buf
    }

    /// The whole buffer, borrowed.
    pub fn get_ref(&self) -> &B {
        &self.buf
    }
}

// SAFETY: an offset into a stable address is itself stable, and `begin` is
// within the buffer by construction, so the pointer stays inside it.
unsafe impl<B: IoBuf> IoBuf for Slice<B> {
    fn read_ptr(&self) -> *const u8 {
        // SAFETY: `begin <= bytes_init() <= ` the allocation's length, so this
        // is in bounds or one past the end.
        unsafe { self.buf.read_ptr().add(self.begin) }
    }

    fn bytes_init(&self) -> usize {
        // Initialised bytes of the underlying buffer that fall inside the range.
        self.buf
            .bytes_init()
            .min(self.end.unwrap_or(usize::MAX))
            .saturating_sub(self.begin)
    }
}

// SAFETY: as the `IoBuf` impl. `bytes_total` is clamped to the underlying
// buffer's capacity, so the window never runs past what is writable.
unsafe impl<B: IoBufMut> IoBufMut for Slice<B> {
    fn write_ptr(&mut self) -> *mut u8 {
        let begin = self.begin;
        // SAFETY: as `read_ptr`.
        unsafe { self.buf.write_ptr().add(begin) }
    }

    fn bytes_total(&self) -> usize {
        self.buf
            .bytes_total()
            .min(self.end.unwrap_or(usize::MAX))
            .saturating_sub(self.begin)
    }

    unsafe fn set_init(&mut self, n: usize) {
        // The underlying buffer counts from its own start, and everything below
        // the window had to be initialised for the window to exist.
        // SAFETY: forwarded to the caller's contract; `begin + n` is within the
        // buffer because `n <= bytes_total()`.
        unsafe { self.buf.set_init(self.begin + n) };
    }
}

/// Turn a range into a `[begin, end)` pair, leaving an open end open.
fn resolve(range: impl RangeBounds<usize>) -> (usize, Option<usize>) {
    let begin = match range.start_bound() {
        Bound::Included(&n) => n,
        Bound::Excluded(&n) => n.checked_add(1).expect("slice start overflowed"),
        Bound::Unbounded => 0,
    };
    let end = match range.end_bound() {
        Bound::Included(&n) => Some(n.checked_add(1).expect("slice end overflowed")),
        Bound::Excluded(&n) => Some(n),
        Bound::Unbounded => None,
    };

    if let Some(end) = end {
        assert!(begin <= end, "slice starts at {begin} but ends at {end}");
    }

    (begin, end)
}

// ---------------------------------------------------------------------------
// Implementations
// ---------------------------------------------------------------------------

// SAFETY: the bytes live in a heap allocation the `Vec` owns, so moving the
// `Vec` moves the three words and not the buffer. `len` bytes are initialised.
unsafe impl IoBuf for Vec<u8> {
    fn read_ptr(&self) -> *const u8 {
        self.as_ptr()
    }

    fn bytes_init(&self) -> usize {
        self.len()
    }
}

// SAFETY: as above, and the whole capacity is owned and writable.
unsafe impl IoBufMut for Vec<u8> {
    fn write_ptr(&mut self) -> *mut u8 {
        self.as_mut_ptr()
    }

    fn bytes_total(&self) -> usize {
        self.capacity()
    }

    unsafe fn set_init(&mut self, n: usize) {
        debug_assert!(n <= self.capacity());
        // SAFETY: forwarded to the caller's contract — those bytes were written.
        unsafe { self.set_len(n) };
    }
}

// SAFETY: a boxed slice is a heap allocation; every byte of it is initialised.
unsafe impl IoBuf for Box<[u8]> {
    fn read_ptr(&self) -> *const u8 {
        self.as_ptr()
    }

    fn bytes_init(&self) -> usize {
        self.len()
    }
}

// SAFETY: as above. The length is fixed, so `set_init` has nothing to record.
unsafe impl IoBufMut for Box<[u8]> {
    fn write_ptr(&mut self) -> *mut u8 {
        self.as_mut_ptr()
    }

    fn bytes_total(&self) -> usize {
        self.len()
    }

    unsafe fn set_init(&mut self, n: usize) {
        debug_assert!(n <= self.len());
    }
}

// SAFETY: the array is behind the box, so the address is the allocation's and
// not the `Box` value's.
unsafe impl<const N: usize> IoBuf for Box<[u8; N]> {
    fn read_ptr(&self) -> *const u8 {
        self.as_ptr()
    }

    fn bytes_init(&self) -> usize {
        N
    }
}

// SAFETY: as above.
unsafe impl<const N: usize> IoBufMut for Box<[u8; N]> {
    fn write_ptr(&mut self) -> *mut u8 {
        self.as_mut_ptr()
    }

    fn bytes_total(&self) -> usize {
        N
    }

    unsafe fn set_init(&mut self, n: usize) {
        debug_assert!(n <= N);
    }
}

// SAFETY: a `'static` slice points at memory that outlives every operation, and
// copying the fat pointer does not move it.
unsafe impl IoBuf for &'static [u8] {
    fn read_ptr(&self) -> *const u8 {
        self.as_ptr()
    }

    fn bytes_init(&self) -> usize {
        self.len()
    }
}

// SAFETY: as above, and the borrow is exclusive.
unsafe impl IoBuf for &'static mut [u8] {
    fn read_ptr(&self) -> *const u8 {
        self.as_ptr()
    }

    fn bytes_init(&self) -> usize {
        self.len()
    }
}

// SAFETY: as above.
unsafe impl IoBufMut for &'static mut [u8] {
    fn write_ptr(&mut self) -> *mut u8 {
        self.as_mut_ptr()
    }

    fn bytes_total(&self) -> usize {
        self.len()
    }

    unsafe fn set_init(&mut self, n: usize) {
        debug_assert!(n <= self.len());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_vec_reports_its_length_and_capacity_separately() {
        let mut v = Vec::with_capacity(16);
        v.extend_from_slice(b"abc");

        assert_eq!(v.bytes_init(), 3, "three bytes are initialised");
        assert_eq!(v.bytes_total(), 16, "but the kernel may use all sixteen");
        assert_eq!(v.read_ptr(), v.write_ptr());
    }

    #[test]
    fn a_slice_offsets_the_pointer_and_narrows_the_window() {
        let v = b"hello world".to_vec();
        let base = v.read_ptr();
        let s = v.slice(6..);

        assert_eq!(s.read_ptr(), unsafe { base.add(6) });
        assert_eq!(s.bytes_init(), 5, "\"world\"");
        assert_eq!((s.begin(), s.end()), (6, None));
    }

    /// An explicit end pins the window even when the buffer holds more.
    #[test]
    fn a_closed_range_clamps_both_ends() {
        let v = b"hello world".to_vec();
        let s = v.slice(2..5);

        assert_eq!(s.bytes_init(), 3, "\"llo\"");
        assert_eq!(s.bytes_total(), 3, "and no more is writable");
        assert_eq!(s.end(), Some(5));
    }

    /// The append idiom: slice past the initialised prefix and the window is
    /// the free capacity, with nothing initialised in it yet.
    #[test]
    fn slicing_past_the_end_gives_the_free_capacity() {
        let mut v = Vec::with_capacity(16);
        v.extend_from_slice(b"abc");

        let mut s = v.slice(3..);
        assert_eq!(s.bytes_init(), 0, "nothing past the prefix is initialised");
        assert_eq!(s.bytes_total(), 13, "but thirteen bytes are writable");

        // Stand in for a read of four bytes.
        unsafe {
            s.write_ptr().copy_from_nonoverlapping(b"defg".as_ptr(), 4);
            s.set_init(4);
        }

        let v = s.into_inner();
        assert_eq!(v, b"abcdefg", "set_init counts from the buffer's own start");
    }

    #[test]
    fn a_slice_of_a_full_buffer_can_still_be_written() {
        let v = vec![0u8; 8];
        let mut s = v.slice(4..8);

        assert_eq!(s.bytes_total(), 4);
        unsafe {
            s.write_ptr().write_bytes(0xff, 4);
            s.set_init(4);
        }

        assert_eq!(s.into_inner(), [0, 0, 0, 0, 0xff, 0xff, 0xff, 0xff]);
    }

    /// A gap between the initialised prefix and the window would let `set_init`
    /// claim bytes nothing ever wrote.
    #[test]
    #[should_panic(expected = "past the 3 initialised bytes")]
    fn a_range_starting_past_the_initialised_bytes_is_rejected() {
        let mut v = Vec::with_capacity(16);
        v.extend_from_slice(b"abc");
        let _ = v.slice(4..);
    }

    /// An end past the buffer is not an error: it is clamped, so
    /// `buf.slice(..8192)` on a shorter buffer just means "all of it".
    #[test]
    fn a_range_ending_past_the_buffer_is_clamped() {
        let mut v = Vec::with_capacity(16);
        v.extend_from_slice(b"abc");
        let s = v.slice(..8192);

        assert_eq!(s.bytes_init(), 3);
        assert_eq!(s.bytes_total(), 16);
    }

    #[test]
    #[should_panic(expected = "starts at 4 but ends at 2")]
    fn a_backwards_range_is_rejected() {
        // Through bindings, because a literal backwards range is a lint of its
        // own and never reaches the assertion under test.
        let (begin, end) = (4, 2);
        let _ = b"abcdef".to_vec().slice(begin..end);
    }

    #[test]
    fn an_empty_vec_still_has_a_usable_pointer() {
        let mut v: Vec<u8> = Vec::new();
        assert_eq!(v.bytes_total(), 0);
        assert!(!v.write_ptr().is_null(), "dangling, but not null");
    }
}
