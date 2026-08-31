#![feature(local_waker)]

//! An HTTP/1.1 server, served by hyper on top of scr.
//!
//! ```text
//! $ cargo run --example hyper
//! hyper on scr, listening on http://127.0.0.1:38273
//! $ curl http://127.0.0.1:38273/
//! ```
//!
//! Pass an address to pin it somewhere specific: `cargo run --example hyper --
//! 127.0.0.1:8080`.
//!
//! hyper does not know about any particular runtime: it asks for one through
//! the traits in [`hyper::rt`], and everything below is those traits implemented
//! for this one — an [executor](ScrExecutor), a [transport](ScrIo) and a
//! [timer](ScrTimer). Only the transport is real work.
//!
//! # The gap this example bridges
//!
//! [`hyper::rt::Read`] and [`hyper::rt::Write`] are *readiness* interfaces: a
//! poll hands over a borrowed buffer and expects the transfer to have happened
//! by the time it returns. scr is *completion*-based, and [`scr::io::AsyncRead`]
//! takes the buffer by value precisely because the kernel keeps hold of it past
//! the end of any borrow — see [`scr::buf`].
//!
//! The two cannot be wired together directly, so [`ScrIo`] owns a buffer on each
//! side and copies at the boundary:
//!
//! - a read fills our buffer through the ring, and later polls copy out of it
//!   into whatever cursor hyper brings, so one syscall can serve several polls;
//! - a write copies hyper's slice into our buffer, submits it, and returns
//!   `Pending` until the completion arrives — so the count we report is one the
//!   kernel actually accepted, never a promise.
//!
//! That copy is the price of the adaptation, and it is why a native scr server
//! (see the `echo` example) hands its buffer to the kernel and never touches the
//! bytes at all.
//!
//! # The other gap: wakers
//!
//! scr wakes tasks through a [`LocalWaker`] and leaves a panicking stub in the
//! [`Waker`] slot, on the grounds that a future on a single-threaded runtime has
//! no business cloning a thread-safe waker. hyper is not written to that rule —
//! nor is anything else in the wider ecosystem — and the `futures-channel` it
//! carries request bodies over clones `cx.waker()` and wakes it later.
//!
//! [`ScrExecutor`] is where hyper's futures enter the runtime, so it is where
//! that is patched up: every future it spawns is wrapped in [`Bridged`], which
//! polls it under a `Waker` that forwards to the task's `LocalWaker`.

use std::cell::Cell;
use std::convert::Infallible;
use std::io;
use std::mem::{self, ManuallyDrop};
use std::net::SocketAddr;
use std::pin::Pin;
use std::rc::Rc;
use std::task::{
    Context, ContextBuilder, LocalWaker, Poll, RawWaker, RawWakerVTable, Waker, ready,
};
use std::thread::{self, ThreadId};
use std::time::{Duration, Instant};

use hyper::body::{Body, Bytes, Frame, Incoming, SizeHint};
use hyper::rt::{Executor, ReadBufCursor};
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Request, Response, StatusCode};

use scr::io::{AsyncRead, AsyncWrite};
use scr::net::TcpListener;
use scr::{Runtime, spawn};

const CONN_BUF_SIZE: usize = 4 * 1024;
const HEADER_TIMEOUT: Duration = Duration::from_secs(15);

fn main() -> io::Result<()> {
    let addr: SocketAddr = match std::env::args().nth(1) {
        Some(arg) => arg.parse().map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("`{arg}` is not an address; try 127.0.0.1:8080"),
            )
        })?,
        // Port zero, so a second copy can run alongside the first.
        None => "127.0.0.1:0".parse().expect("a literal address"),
    };

    Runtime::new()?.block_on(serve(addr))
}

async fn serve(addr: SocketAddr) -> io::Result<()> {
    let listener = TcpListener::bind(addr).await?;
    println!(
        "hyper on scr, listening on http://{}",
        listener.local_addr().await?
    );

    // Everything runs on one thread, so the counter shared by every connection
    // needs neither an atomic nor a lock.
    let counter = Rc::new(Cell::new(0u64));

    loop {
        let (stream, peer) = listener.accept().await?;
        let counter = Rc::clone(&counter);

        // http1 never spawns anything itself, so this is the one place the
        // executor gets used; the h2 and auto builders take one and drive their
        // own background tasks through it.
        ScrExecutor.execute(async move {
            let service = service_fn(move |req| {
                let count = counter.get() + 1;
                counter.set(count);

                handle(req, count)
            });

            let conn = http1::Builder::new()
                .timer(ScrTimer)
                .header_read_timeout(HEADER_TIMEOUT)
                .serve_connection(ScrIo::new(stream), service);

            if let Err(e) = conn.await {
                eprintln!("{peer}: {e}");
            }
        });
    }
}

/// One request. The body is ignored, and hyper drains it for us.
async fn handle(req: Request<Incoming>, count: u64) -> Result<Response<Text>, Infallible> {
    let response = match req.uri().path() {
        "/" => Response::new(Text::new(format!("Request #{count}\n"))),
        path => Response::builder()
            .status(StatusCode::NOT_FOUND)
            .body(Text::new(format!("no such path: {path}\n")))
            .expect("a valid response"),
    };

    Ok(response)
}

// ---------------------------------------------------------------------------
// Executor
// ---------------------------------------------------------------------------

/// Spawns hyper's futures onto the current runtime.
///
/// No `Send` bound: the whole runtime is one thread, which is what lets a
/// connection hold an [`Rc`] and hand it to the next request.
#[derive(Clone, Copy)]
struct ScrExecutor;

impl<F> Executor<F> for ScrExecutor
where
    F: Future + 'static,
{
    fn execute(&self, fut: F) {
        // `spawn` wants a task that resolves to nothing, and hyper's futures
        // resolve to something it has already dealt with.
        spawn(async {
            Bridged::new(fut).await;
        });
    }
}

// ---------------------------------------------------------------------------
// Wakers
// ---------------------------------------------------------------------------

/// Polls a future that expects a working [`Waker`].
///
/// scr passes tasks a [`LocalWaker`] and a stub `Waker` that panics if anything
/// touches it, which is a fine rule for futures written against this runtime and
/// no use at all for hyper. This substitutes a `Waker` that forwards to the
/// task's `LocalWaker`, and leaves the `LocalWaker` itself in place — so scr's
/// own futures, further down the same poll, still reach the task directly.
struct Bridged<F> {
    future: F,
    /// The bridge for the local waker last seen, and that waker, to tell
    /// whether the task has been handed to a different one since. A poll is not
    /// worth an allocation, and the answer is almost always no.
    waker: Option<(LocalWaker, Waker)>,
}

impl<F> Bridged<F> {
    fn new(future: F) -> Bridged<F> {
        Bridged {
            future,
            waker: None,
        }
    }
}

impl<F: Future> Future for Bridged<F> {
    type Output = F::Output;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<F::Output> {
        // SAFETY: `future` is never moved out of `self` and nothing else takes a
        // `&mut` to it, so it stays pinned for exactly as long as `self` does.
        let this = unsafe { self.get_unchecked_mut() };
        let future = unsafe { Pin::new_unchecked(&mut this.future) };

        let local = cx.local_waker();

        let stale = match &this.waker {
            Some((cached, _)) => !cached.will_wake(local),
            None => true,
        };

        if stale {
            this.waker = Some((local.clone(), Bridge::waker(local)));
        }

        let (_, waker) = this.waker.as_ref().expect("just filled in");

        future.poll(&mut ContextBuilder::from_waker(waker).local_waker(local).build())
    }
}

/// The far end of a [`Bridged`] waker: a [`LocalWaker`], and the thread it may
/// be used from.
///
/// The same assertion [`ScrSleep`] makes, for the same reason. A `Waker` is
/// `Send + Sync` by definition and this one is not, so it is checked rather than
/// proven: hyper wakes a connection from the connection, on the thread the
/// runtime is turning, and never from anywhere else.
struct Bridge {
    local: LocalWaker,
    owner: ThreadId,
}

impl Bridge {
    fn waker(local: &LocalWaker) -> Waker {
        let bridge = Rc::new(Bridge {
            local: local.clone(),
            owner: thread::current().id(),
        });

        // SAFETY: the pointer is an owned `Rc<Bridge>`, which is what every
        // function in the vtable below expects; the refcount is only ever
        // touched on `owner`, which the assertions insist on.
        unsafe { Waker::from_raw(RawWaker::new(Rc::into_raw(bridge).cast(), &BRIDGE_VTABLE)) }
    }

    fn wake(&self) {
        debug_assert_eq!(
            thread::current().id(),
            self.owner,
            "a bridged waker was woken from a thread other than the runtime's"
        );

        self.local.wake_by_ref();
    }
}

impl Drop for Bridge {
    fn drop(&mut self) {
        // Dropping the `LocalWaker` releases a reference to the task, so it is
        // as thread-bound as waking it is.
        debug_assert_eq!(
            thread::current().id(),
            self.owner,
            "a bridged waker was dropped on a thread other than the runtime's"
        );
    }
}

static BRIDGE_VTABLE: RawWakerVTable =
    RawWakerVTable::new(bridge_clone, bridge_wake, bridge_wake_by_ref, bridge_drop);

/// # Safety
///
/// `ptr` must be an `Rc<Bridge>` handed over by [`Bridge::waker`], as it is for
/// every entry here.
unsafe fn bridge_clone(ptr: *const ()) -> RawWaker {
    unsafe { Rc::increment_strong_count(ptr.cast::<Bridge>()) };

    RawWaker::new(ptr, &BRIDGE_VTABLE)
}

/// # Safety
///
/// As [`bridge_clone`]. Consumes the reference.
unsafe fn bridge_wake(ptr: *const ()) {
    let bridge = unsafe { Rc::from_raw(ptr.cast::<Bridge>()) };

    bridge.wake();
}

/// # Safety
///
/// As [`bridge_clone`]. Leaves the reference alone.
unsafe fn bridge_wake_by_ref(ptr: *const ()) {
    let bridge = ManuallyDrop::new(unsafe { Rc::from_raw(ptr.cast::<Bridge>()) });

    bridge.wake();
}

/// # Safety
///
/// As [`bridge_clone`]. Consumes the reference.
unsafe fn bridge_drop(ptr: *const ()) {
    drop(unsafe { Rc::from_raw(ptr.cast::<Bridge>()) });
}

// ---------------------------------------------------------------------------
// IO
// ---------------------------------------------------------------------------

/// A [`scr::net::TcpStream`] behind hyper's [`Read`](hyper::rt::Read) and
/// [`Write`](hyper::rt::Write).
///
/// The stream is held in an [`Rc`] so that the in-flight operation's future can
/// own a handle to it: an operation outlives the poll that started it, and
/// borrowing `&self` across that would not typecheck. Reads and writes take
/// `&self`, so both halves work from the one handle at once.
struct ScrIo<S> {
    stream: Rc<S>,
    read: ReadState,
    write: WriteState,
}

/// A read operation, boxed because it is an anonymous `async` type that has to
/// live in the struct between polls. The `'static` that `Box<dyn Future>`
/// implies is what the [`Rc`] inside it buys.
type ReadOp = Pin<Box<dyn Future<Output = (io::Result<usize>, Vec<u8>)>>>;
type WriteOp = ReadOp;
type ShutdownOp = Pin<Box<dyn Future<Output = io::Result<()>>>>;

enum ReadState {
    /// Nothing in flight, holding the buffer for the next read.
    Idle(Vec<u8>),
    /// A `recv` is in the ring, and the kernel has the buffer.
    Busy(ReadOp),
    /// Bytes are waiting, of which the first `usize` have been copied out.
    Filled(Vec<u8>, usize),
    /// The peer closed its end.
    Eof,
}

enum WriteState {
    Idle(Vec<u8>),
    Busy(WriteOp),
    /// A write completed during a flush, so its count has not been reported to
    /// hyper yet. The next `poll_write` hands it over.
    Done(usize, Vec<u8>),
    Closing(ShutdownOp),
    Closed,
}

impl<S> ScrIo<S> {
    fn new(stream: S) -> ScrIo<S> {
        ScrIo {
            stream: Rc::new(stream),
            // Uninitialised capacity: a read fills from the start and reports
            // how far it got, so zeroing first would be wasted work.
            read: ReadState::Idle(Vec::with_capacity(CONN_BUF_SIZE)),
            write: WriteState::Idle(Vec::with_capacity(CONN_BUF_SIZE)),
        }
    }
}

impl<S: AsyncRead + 'static> hyper::rt::Read for ScrIo<S> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        mut cursor: ReadBufCursor<'_>,
    ) -> Poll<io::Result<()>> {
        // Every field is `Unpin` — the boxed operations are pinned inside their
        // own allocation — so `ScrIo` is, and the pin can be dropped here.
        let this = self.get_mut();

        loop {
            match &mut this.read {
                ReadState::Filled(buf, copied) => {
                    let n = cursor.remaining().min(buf.len() - *copied);
                    cursor.put_slice(&buf[*copied..*copied + n]);
                    *copied += n;

                    if *copied == buf.len() {
                        let mut buf = mem::take(buf);
                        buf.clear();
                        this.read = ReadState::Idle(buf);
                    }

                    return Poll::Ready(Ok(()));
                }

                ReadState::Idle(buf) => {
                    // The buffer goes to the kernel for the length of the
                    // operation, so it leaves the state machine with it.
                    let buf = mem::take(buf);
                    let stream = Rc::clone(&this.stream);

                    this.read = ReadState::Busy(Box::pin(async move { stream.read(buf).await }));
                }

                ReadState::Busy(op) => {
                    let (result, mut buf) = ready!(op.as_mut().poll(cx));

                    match result {
                        // A clean close from the far end. hyper reads that from
                        // an untouched cursor rather than from a sentinel.
                        Ok(0) => this.read = ReadState::Eof,
                        Ok(_) => this.read = ReadState::Filled(buf, 0),
                        Err(e) => {
                            buf.clear();
                            this.read = ReadState::Idle(buf);

                            return Poll::Ready(Err(e));
                        }
                    }
                }

                ReadState::Eof => return Poll::Ready(Ok(())),
            }
        }
    }
}

impl<S: AsyncWrite + 'static> hyper::rt::Write for ScrIo<S> {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        data: &[u8],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();

        loop {
            match &mut this.write {
                WriteState::Idle(buf) => {
                    if data.is_empty() {
                        return Poll::Ready(Ok(0));
                    }

                    // The copy hyper's borrowed slice forces on us: the kernel
                    // needs an address that outlives this poll.
                    let mut buf = mem::take(buf);
                    buf.clear();
                    buf.extend_from_slice(data);

                    let stream = Rc::clone(&this.stream);

                    this.write = WriteState::Busy(Box::pin(async move { stream.write(buf).await }));
                }

                WriteState::Busy(op) => {
                    let (result, mut buf) = ready!(op.as_mut().poll(cx));
                    buf.clear();
                    this.write = WriteState::Idle(buf);

                    // Returning `Pending` until the completion lands is what
                    // makes this honest: hyper is told about bytes the kernel
                    // has taken, and a short write is simply a smaller count.
                    return Poll::Ready(result);
                }

                WriteState::Done(n, buf) => {
                    let n = *n;
                    let mut buf = mem::take(buf);
                    buf.clear();
                    this.write = WriteState::Idle(buf);

                    return Poll::Ready(Ok(n));
                }

                WriteState::Closing(_) | WriteState::Closed => {
                    return Poll::Ready(Err(io::ErrorKind::BrokenPipe.into()));
                }
            }
        }
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();

        match &mut this.write {
            // Nothing is held back on this side: a write is submitted straight
            // to the ring and `poll_write` does not report it until the kernel
            // has accepted it, so there is nothing left to push.
            WriteState::Idle(_) | WriteState::Done(..) | WriteState::Closed => Poll::Ready(Ok(())),

            // hyper only flushes once its own writes have come back ready, so
            // this arm is defensive. Finish the operation and keep its count for
            // the `poll_write` that has yet to hear about it.
            WriteState::Busy(op) => {
                let (result, mut buf) = ready!(op.as_mut().poll(cx));
                buf.clear();

                match result {
                    Ok(n) => {
                        this.write = WriteState::Done(n, buf);
                        Poll::Ready(Ok(()))
                    }
                    Err(e) => {
                        this.write = WriteState::Idle(buf);
                        Poll::Ready(Err(e))
                    }
                }
            }

            // A flush during shutdown waits for the close to land. The state
            // has to move on with it: polling a completed operation again is a
            // panic, not a second answer.
            WriteState::Closing(op) => {
                let result = ready!(op.as_mut().poll(cx));
                this.write = WriteState::Closed;

                Poll::Ready(result)
            }
        }
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();

        loop {
            match &mut this.write {
                // Let an in-flight write land before closing the half it is
                // using. Its count is dropped along with the state: after a
                // shutdown there is no `poll_write` left to tell.
                WriteState::Busy(op) => {
                    let (result, _) = ready!(op.as_mut().poll(cx));
                    if let Err(e) = result {
                        this.write = WriteState::Closed;
                        return Poll::Ready(Err(e));
                    }

                    this.write = WriteState::Idle(Vec::new());
                }

                WriteState::Idle(_) | WriteState::Done(..) => {
                    let stream = Rc::clone(&this.stream);

                    this.write =
                        WriteState::Closing(Box::pin(async move { stream.shutdown().await }));
                }

                WriteState::Closing(op) => {
                    let result = ready!(op.as_mut().poll(cx));
                    this.write = WriteState::Closed;

                    return Poll::Ready(result);
                }

                WriteState::Closed => return Poll::Ready(Ok(())),
            }
        }
    }

    // Left at the default `false`. A vectored write would have to gather into
    // one owned buffer anyway, and hyper's own flattening does that better than
    // this adapter could — it knows which slices are worth coalescing.
}

// ---------------------------------------------------------------------------
// Timer
// ---------------------------------------------------------------------------

/// Timers for hyper's connection deadlines.
#[derive(Clone, Copy)]
struct ScrTimer;

impl hyper::rt::Timer for ScrTimer {
    fn sleep(&self, duration: Duration) -> Pin<Box<dyn hyper::rt::Sleep>> {
        Box::pin(ScrSleep::new(scr::time::sleep(duration)))
    }

    fn sleep_until(&self, deadline: Instant) -> Pin<Box<dyn hyper::rt::Sleep>> {
        Box::pin(ScrSleep::new(scr::time::sleep_until(deadline)))
    }

    fn reset(&self, sleep: &mut Pin<Box<dyn hyper::rt::Sleep>>, deadline: Instant) {
        match sleep.as_mut().downcast_mut_pin::<ScrSleep>() {
            // Re-aims the existing entry rather than dropping it and arming
            // another, which is the whole reason this method exists.
            Some(ours) => ours.get_mut().sleep.reset(deadline),
            None => *sleep = self.sleep_until(deadline),
        }
    }
}

/// A [`scr::time::Sleep`] wearing the `Send + Sync` that [`hyper::rt::Sleep`]
/// demands.
///
/// scr's timers are per-thread and reached through an [`Rc`], so a `Sleep` is
/// neither `Send` nor `Sync` and cannot honestly be made so. hyper's trait
/// asks for both because a multi-threaded runtime may move a connection between
/// threads mid-flight; on a runtime that never does, the bound is asking for a
/// guarantee nothing here needs.
///
/// So it is asserted rather than earned, and then checked: every poll and the
/// drop assert, in debug builds, that they are on the thread that armed the
/// timer. That is the same trade [`scr::net::TcpStream`] makes for concurrent
/// reads — a bound the type system cannot express, caught by an assertion
/// instead of by the compiler.
struct ScrSleep {
    sleep: scr::time::Sleep,
    owner: ThreadId,
}

// SAFETY: this is a lie the assertions below catch. It holds for as long as the
// value stays on the thread that made it, which is true of every timer hyper
// arms for an http1 connection: the connection is a single task, the runtime is
// one thread, and neither hands the timer to anything else.
unsafe impl Send for ScrSleep {}
// SAFETY: as above, and nothing here is reachable through a shared reference —
// polling and dropping both need `&mut`.
unsafe impl Sync for ScrSleep {}

impl ScrSleep {
    fn new(sleep: scr::time::Sleep) -> ScrSleep {
        ScrSleep {
            sleep,
            owner: thread::current().id(),
        }
    }

    fn check_thread(&self, what: &str) {
        debug_assert_eq!(
            thread::current().id(),
            self.owner,
            "a scr timer was {what} on a thread other than the one that armed it"
        );
    }
}

impl Future for ScrSleep {
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        let this = self.get_mut();
        this.check_thread("polled");

        Pin::new(&mut this.sleep).poll(cx)
    }
}

impl Drop for ScrSleep {
    fn drop(&mut self) {
        // Dropping is what removes the entry from the thread's timer wheel, so
        // it is as thread-bound as the poll is.
        self.check_thread("dropped");
    }
}

impl hyper::rt::Sleep for ScrSleep {}

// ---------------------------------------------------------------------------
// Body
// ---------------------------------------------------------------------------

/// A response body of exactly one chunk.
///
/// `http-body-util` has this as `Full`, and pulling that in for an example that
/// answers with a line of text is more dependency than it is worth.
struct Text(Option<Bytes>);

impl Text {
    fn new(text: impl Into<Bytes>) -> Text {
        Text(Some(text.into()))
    }
}

impl Body for Text {
    type Data = Bytes;
    type Error = Infallible;

    fn poll_frame(
        self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, Infallible>>> {
        Poll::Ready(self.get_mut().0.take().map(|data| Ok(Frame::data(data))))
    }

    fn is_end_stream(&self) -> bool {
        self.0.is_none()
    }

    /// Exact, so hyper sends a `Content-Length` rather than chunking.
    fn size_hint(&self) -> SizeHint {
        SizeHint::with_exact(self.0.as_ref().map_or(0, Bytes::len) as u64)
    }
}
