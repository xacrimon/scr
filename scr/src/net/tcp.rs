//! TCP listeners and streams, over direct descriptors only.
//!
//! Neither type ever holds a process file descriptor. A listener is created,
//! bound and put into listening state entirely inside the ring, landing in a
//! slot of the registered file table; the connections it accepts are installed
//! into slots of their own. Nothing here can be passed to `read(2)`, and that is
//! the point — every operation is an array index into the ring's own table
//! rather than a lookup in the process file table.
//!
//! # Slot lifetime
//!
//! A slot is reserved before the operation that fills it is submitted, and
//! returned only when the operation that empties it *completes*. Closing is
//! asynchronous, so returning a slot at submission time would hand the next
//! connection a descriptor that is still open. Everything that gives a slot back
//! does so from a completion, which is what [`close_slot`] is for.

#[cfg(debug_assertions)]
use std::cell::Cell;
use std::fmt;
use std::io;
use std::net::SocketAddr;
use std::ptr::NonNull;
use std::rc::Rc;

use crate::buf::{BufResult, IoBuf, IoBufMut};
use crate::io::{AsyncRead, AsyncWrite};
use crate::io_uring::{op, sys};
use crate::runtime::context;
use crate::runtime::driver::Driver;
use crate::runtime::driver::ledger::OnComplete;
use crate::runtime::driver::op::{Chain, ChainCompletable, Completable, Op};

use super::addr::SockAddr;

/// Connections the kernel may queue before we accept them.
const BACKLOG: u32 = 1024;

// ---------------------------------------------------------------------------
// Slots
// ---------------------------------------------------------------------------

/// Empty a direct descriptor slot, returning it to the table once that lands.
///
/// Safe to call on a slot that was never filled: the close fails with `EBADF`
/// and the slot comes back regardless, which is what lets the cleanup paths be
/// unconditional instead of having to work out how far a chain got.
fn close_slot(driver: &Driver, slot: u32) {
    driver.submit_detached(
        sys::SqeFlags::empty(),
        |s| op::prep_close_direct(s, slot),
        Box::new(FreeSlot(slot)),
    );
}

/// Returns a slot to the table when the close that emptied it completes.
struct FreeSlot(u32);

impl OnComplete for FreeSlot {
    fn on_complete(self: Box<Self>, driver: &Driver, _res: i32, _flags: sys::CqeFlags) {
        driver.free_slot(self.0);
    }
}

/// Ask the kernel for one end's address, as `getsockname` or `getpeername`.
///
/// There is no plain opcode for this; it is a socket passthrough command, which
/// is why it takes a `cmd_op` rather than an operation of its own.
async fn sock_name(driver: &Driver, slot: u32, peer: bool) -> io::Result<SocketAddr> {
    let mut addr = SockAddr::zeroed();
    // Going in this is the capacity; the kernel overwrites it with the real
    // length. A `sockaddr_storage` is the largest address there is, so the
    // truncation this protocol allows for cannot happen here.
    let (name, namelen) = addr.ptrs();

    Op::submit(
        driver,
        sys::SqeFlags::FIXED_FILE,
        |s| op::prep_cmd_getsockname(s, slot as i32, name, namelen, peer as u32),
        GetName { addr },
    )
    .await
}

/// The payload of a [`sock_name`] command.
struct GetName {
    addr: Box<SockAddr>,
}

impl Completable for GetName {
    type Output = io::Result<SocketAddr>;

    fn complete(self, _driver: &Driver, res: i32, _flags: sys::CqeFlags) -> io::Result<SocketAddr> {
        // Unlike most operations this reports success as a plain zero; the
        // address came back through the buffer.
        result(res)?;
        self.addr.to_socket_addr()
    }

    fn cleanup(self, _driver: &Driver, _res: i32, _flags: sys::CqeFlags) {
        // The address buffer outlived the kernel's pointer to it, which was the
        // only thing that had to happen.
    }
}

/// Turn a completion's `res` into a result.
fn result(res: i32) -> io::Result<u32> {
    if res < 0 {
        Err(io::Error::from_raw_os_error(-res))
    } else {
        Ok(res as u32)
    }
}

// ---------------------------------------------------------------------------
// Listener
// ---------------------------------------------------------------------------

/// A TCP socket listening for connections, held as a direct descriptor.
pub struct TcpListener {
    driver: Rc<Driver>,
    slot: u32,
}

impl TcpListener {
    /// Create a socket, bind it to `addr`, and start listening.
    ///
    /// All three run as one linked chain, so the whole thing costs a single
    /// submission rather than three round trips. That is only possible because
    /// the slot is chosen up front: the bind and the listen can name the
    /// descriptor the socket operation has not made yet.
    pub async fn bind(addr: SocketAddr) -> io::Result<TcpListener> {
        let driver = context::driver();
        let slot = driver
            .alloc_slot()
            .ok_or_else(|| io::Error::from_raw_os_error(libc::ENFILE))?;

        let mut sock_addr = SockAddr::from_socket_addr(addr);
        let addr_ptr = sock_addr.addr_ptr();
        let addr_len = sock_addr.len();

        let data = BindChain {
            driver: Rc::clone(&driver),
            slot,
            _addr: sock_addr,
        };

        // The socket operation's `fd` field holds the address family, not a
        // descriptor, and its slot is an output — so no `FIXED_FILE` on it,
        // unlike the two that follow. `submit_chain` adds the links.
        Chain::submit(
            &driver,
            [
                sys::SqeFlags::empty(),
                sys::SqeFlags::FIXED_FILE,
                sys::SqeFlags::FIXED_FILE,
            ],
            |[a, b, c]| {
                op::prep_socket_direct(
                    a,
                    SockAddr::domain(addr),
                    libc::SOCK_STREAM as u64,
                    0,
                    0,
                    slot,
                );
                op::prep_bind(b, slot as i32, addr_ptr, addr_len);
                op::prep_listen(c, slot as i32, BACKLOG);
            },
            data,
        )
        .await
    }

    /// Accept one connection.
    pub async fn accept(&self) -> io::Result<(TcpStream, SocketAddr)> {
        let slot = self
            .driver
            .alloc_slot()
            .ok_or_else(|| io::Error::from_raw_os_error(libc::ENFILE))?;

        let mut peer = SockAddr::zeroed();
        let peer_ptrs = peer.ptrs();

        let data = AcceptOp {
            driver: Rc::clone(&self.driver),
            slot,
            peer,
        };

        Op::submit(
            &self.driver,
            sys::SqeFlags::FIXED_FILE,
            |s| op::prep_accept_direct(s, self.slot as i32, Some(peer_ptrs), 0, slot),
            data,
        )
        .await
    }

    /// The address this listener is bound to.
    ///
    /// Worth asking for even when the bind address was fully specified: binding
    /// to port 0 is how you let the kernel choose one, and this is the only way
    /// to find out which.
    pub async fn local_addr(&self) -> io::Result<SocketAddr> {
        sock_name(&self.driver, self.slot, false).await
    }

    /// The registered file table slot this listener occupies.
    pub fn slot(&self) -> u32 {
        self.slot
    }
}

impl fmt::Debug for TcpListener {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TcpListener")
            .field("slot", &self.slot)
            .finish()
    }
}

impl Drop for TcpListener {
    fn drop(&mut self) {
        self.driver.cancel_slot(self.slot);
        close_slot(&self.driver, self.slot);
    }
}

/// The payload of the socket/bind/listen chain.
struct BindChain {
    driver: Rc<Driver>,
    slot: u32,
    /// Held only so the kernel's pointer to it stays valid for the whole chain.
    _addr: Box<SockAddr>,
}

impl ChainCompletable<3> for BindChain {
    type Output = io::Result<TcpListener>;

    fn complete(self, _driver: &Driver, res: [i32; 3]) -> io::Result<TcpListener> {
        // Once a member fails the rest report `ECANCELED`, so the first error is
        // the one worth reporting.
        if let Some(&failed) = res.iter().find(|&&r| r < 0) {
            close_slot(&self.driver, self.slot);
            return Err(io::Error::from_raw_os_error(-failed));
        }

        Ok(TcpListener {
            driver: Rc::clone(&self.driver),
            slot: self.slot,
        })
    }

    fn cleanup(self, driver: &Driver) {
        // The chain may have got as far as creating the socket, and there is now
        // nobody to close it; closing an empty slot is harmless.
        close_slot(driver, self.slot);
    }
}

/// The payload of an accept.
struct AcceptOp {
    driver: Rc<Driver>,
    slot: u32,
    peer: Box<SockAddr>,
}

impl Completable for AcceptOp {
    type Output = io::Result<(TcpStream, SocketAddr)>;

    fn complete(self, _driver: &Driver, res: i32, _flags: sys::CqeFlags) -> Self::Output {
        // With a slot named, a successful accept reports zero rather than a
        // descriptor — the connection is already in the table.
        if let Err(e) = result(res) {
            self.driver.free_slot(self.slot);
            return Err(e);
        }

        match self.peer.to_socket_addr() {
            Ok(addr) => Ok((TcpStream::new(Rc::clone(&self.driver), self.slot), addr)),
            Err(e) => {
                // A connection we cannot describe is still a connection.
                close_slot(&self.driver, self.slot);
                Err(e)
            }
        }
    }

    fn cleanup(self, driver: &Driver, res: i32, _flags: sys::CqeFlags) {
        if res < 0 {
            driver.free_slot(self.slot);
        } else {
            // It succeeded after the caller stopped caring. Without this the
            // connection stays open in a slot no handle names.
            close_slot(driver, self.slot);
        }
    }
}

// ---------------------------------------------------------------------------
// Stream
// ---------------------------------------------------------------------------

/// A TCP connection, held as a direct descriptor.
///
/// Reads and writes take `&self`, so two tasks sharing an `Rc<TcpStream>` can
/// run one of each at the same time without splitting it. Two concurrent reads,
/// or two concurrent writes, are a bug, and are caught by a debug assertion.
pub struct TcpStream {
    driver: Rc<Driver>,
    slot: u32,
    #[cfg(debug_assertions)]
    reading: Cell<bool>,
    #[cfg(debug_assertions)]
    writing: Cell<bool>,
}

impl TcpStream {
    /// Open a connection to `addr`.
    ///
    /// The socket and the connect go out as one linked chain, so a connection
    /// costs a single submission rather than two round trips. As in
    /// [`TcpListener::bind`], that works because the slot is reserved before
    /// either entry is built: the connect can name a descriptor the socket
    /// operation has not created yet.
    pub async fn connect(addr: SocketAddr) -> io::Result<TcpStream> {
        let driver = context::driver();
        let slot = driver
            .alloc_slot()
            .ok_or_else(|| io::Error::from_raw_os_error(libc::ENFILE))?;

        let mut peer = SockAddr::from_socket_addr(addr);
        let addr_ptr = peer.addr_ptr();
        let addr_len = peer.len();

        let data = ConnectChain {
            driver: Rc::clone(&driver),
            slot,
            _addr: peer,
        };

        // As in `bind`, the socket operation's `fd` field holds the address
        // family rather than a descriptor and its slot is an output, so it takes
        // neither `FIXED_FILE` nor the slot in `fd`. `submit_chain` adds the link.
        Chain::submit(
            &driver,
            [sys::SqeFlags::empty(), sys::SqeFlags::FIXED_FILE],
            |[a, b]| {
                op::prep_socket_direct(
                    a,
                    SockAddr::domain(addr),
                    libc::SOCK_STREAM as u64,
                    0,
                    0,
                    slot,
                );
                op::prep_connect(b, slot as i32, addr_ptr, addr_len);
            },
            data,
        )
        .await
    }

    fn new(driver: Rc<Driver>, slot: u32) -> TcpStream {
        TcpStream {
            driver,
            slot,
            #[cfg(debug_assertions)]
            reading: Cell::new(false),
            #[cfg(debug_assertions)]
            writing: Cell::new(false),
        }
    }

    /// This end's address.
    pub async fn local_addr(&self) -> io::Result<SocketAddr> {
        sock_name(&self.driver, self.slot, false).await
    }

    /// The address of the far end.
    pub async fn peer_addr(&self) -> io::Result<SocketAddr> {
        sock_name(&self.driver, self.slot, true).await
    }

    /// The registered file table slot this connection occupies.
    pub fn slot(&self) -> u32 {
        self.slot
    }

    #[cfg(debug_assertions)]
    fn claim_read(&self) -> Busy<'_> {
        Busy::claim(&self.reading, "reads")
    }

    #[cfg(not(debug_assertions))]
    fn claim_read(&self) {}

    #[cfg(debug_assertions)]
    fn claim_write(&self) -> Busy<'_> {
        Busy::claim(&self.writing, "writes")
    }

    #[cfg(not(debug_assertions))]
    fn claim_write(&self) {}
}

impl AsyncRead for TcpStream {
    async fn read<B: IoBufMut>(&self, mut buf: B) -> BufResult<usize, B> {
        let _busy = self.claim_read();

        let ptr = NonNull::new(buf.write_ptr()).expect("a buffer address is never null");
        let window = NonNull::slice_from_raw_parts(ptr, buf.bytes_total());

        Op::submit(
            &self.driver,
            sys::SqeFlags::FIXED_FILE,
            |s| op::prep_recv(s, self.slot as i32, window, 0),
            Recv { buf },
        )
        .await
    }
}

impl AsyncWrite for TcpStream {
    async fn write<B: IoBuf>(&self, buf: B) -> BufResult<usize, B> {
        let _busy = self.claim_write();

        let ptr = NonNull::new(buf.read_ptr().cast_mut()).expect("a buffer address is never null");
        let window = NonNull::slice_from_raw_parts(ptr, buf.bytes_init());

        Op::submit(
            &self.driver,
            sys::SqeFlags::FIXED_FILE,
            |s| op::prep_send(s, self.slot as i32, window, 0),
            Send { buf },
        )
        .await
    }

    async fn flush(&self) -> io::Result<()> {
        Ok(())
    }

    async fn shutdown(&self) -> io::Result<()> {
        Op::submit(
            &self.driver,
            sys::SqeFlags::FIXED_FILE,
            |s| op::prep_shutdown(s, self.slot as i32, libc::SHUT_WR as u32),
            Shutdown,
        )
        .await
    }
}

impl fmt::Debug for TcpStream {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TcpStream")
            .field("slot", &self.slot)
            .finish()
    }
}

impl Drop for TcpStream {
    fn drop(&mut self) {
        // One entry stops everything still running on this slot; without it a
        // read on an idle connection would hold its ledger entry, and the buffer
        // in it, until the peer happened to send something.
        self.driver.cancel_slot(self.slot);
        close_slot(&self.driver, self.slot);
    }
}

/// Guards against two reads, or two writes, running on one stream at once.
///
/// This is the bug `&mut self` on the IO traits would have made impossible, and
/// the reason giving it up is affordable: it is caught here in debug builds,
/// costs nothing in release, and — unlike the borrow checker — it keeps working
/// once the stream is shared between tasks, which is when it can actually
/// happen.
#[cfg(debug_assertions)]
struct Busy<'a> {
    flag: &'a Cell<bool>,
}

#[cfg(debug_assertions)]
impl<'a> Busy<'a> {
    fn claim(flag: &'a Cell<bool>, what: &str) -> Busy<'a> {
        assert!(
            !flag.replace(true),
            "two concurrent {what} on one TcpStream; they would race for the same bytes"
        );
        Busy { flag }
    }
}

#[cfg(debug_assertions)]
impl Drop for Busy<'_> {
    fn drop(&mut self) {
        self.flag.set(false);
    }
}

// ---------------------------------------------------------------------------
// Stream operations
// ---------------------------------------------------------------------------

/// The payload of the socket/connect chain.
struct ConnectChain {
    driver: Rc<Driver>,
    slot: u32,
    /// Held only so the kernel's pointer to it stays valid for the whole chain.
    _addr: Box<SockAddr>,
}

impl ChainCompletable<2> for ConnectChain {
    type Output = io::Result<TcpStream>;

    fn complete(self, _driver: &Driver, res: [i32; 2]) -> io::Result<TcpStream> {
        // A failed socket leaves the connect reporting `ECANCELED`, so the first
        // error is the one that says what actually went wrong.
        if let Some(&failed) = res.iter().find(|&&r| r < 0) {
            close_slot(&self.driver, self.slot);
            return Err(io::Error::from_raw_os_error(-failed));
        }

        Ok(TcpStream::new(Rc::clone(&self.driver), self.slot))
    }

    fn cleanup(self, driver: &Driver) {
        // The chain may have got as far as an open — possibly connected — socket
        // that nothing now names. Closing a slot that was never filled is
        // harmless, so this needs no idea of how far it got.
        close_slot(driver, self.slot);
    }
}

struct Recv<B: IoBufMut> {
    buf: B,
}

impl<B: IoBufMut> Completable for Recv<B> {
    type Output = BufResult<usize, B>;

    fn complete(mut self, _driver: &Driver, res: i32, _flags: sys::CqeFlags) -> Self::Output {
        match result(res) {
            Err(e) => (Err(e), self.buf),
            Ok(n) => {
                // SAFETY: the kernel reports what it wrote, and it was given a
                // window of exactly `bytes_total`.
                unsafe { self.buf.set_init(n as usize) };
                (Ok(n as usize), self.buf)
            }
        }
    }

    fn cleanup(self, _driver: &Driver, _res: i32, _flags: sys::CqeFlags) {
        // Holding the buffer until now was the whole job; dropping it here is
        // the first moment the kernel is provably finished with it.
    }
}

struct Send<B: IoBuf> {
    buf: B,
}

impl<B: IoBuf> Completable for Send<B> {
    type Output = BufResult<usize, B>;

    fn complete(self, _driver: &Driver, res: i32, _flags: sys::CqeFlags) -> Self::Output {
        match result(res) {
            Err(e) => (Err(e), self.buf),
            Ok(n) => (Ok(n as usize), self.buf),
        }
    }

    fn cleanup(self, _driver: &Driver, _res: i32, _flags: sys::CqeFlags) {}
}

struct Shutdown;

impl Completable for Shutdown {
    type Output = io::Result<()>;

    fn complete(self, _driver: &Driver, res: i32, _flags: sys::CqeFlags) -> io::Result<()> {
        result(res).map(|_| ())
    }

    fn cleanup(self, _driver: &Driver, _res: i32, _flags: sys::CqeFlags) {}
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::io::{Read as _, Write as _};
    use std::net::TcpStream as StdStream;
    use std::task::{ContextBuilder, Poll, Waker};
    use std::thread;

    use crate::Runtime;
    use crate::runtime::driver::noop_local_waker;

    /// Loopback, port unspecified — the kernel picks one and `local_addr`
    /// reports it, which is how every test here gets an address nobody else can
    /// take out from under it.
    fn any() -> SocketAddr {
        "127.0.0.1:0".parse().expect("a literal address")
    }

    /// An address nothing is listening on, borrowed from the OS and handed
    /// straight back. A listener that never accepted leaves no `TIME_WAIT`
    /// behind, so the port really is free again. Only a test that needs a
    /// *closed* port wants this; everything else binds to zero and asks.
    fn free_addr() -> SocketAddr {
        let probe = std::net::TcpListener::bind("127.0.0.1:0").expect("probe bind");
        probe.local_addr().expect("local_addr")
    }

    /// Turn the reactor until `done` holds. Each `yield_now` costs the executor
    /// one pass, and a pass is one `io_uring_enter`.
    async fn settle(mut done: impl FnMut() -> bool) {
        for _ in 0..1000 {
            if done() {
                return;
            }
            crate::task::yield_now().await;
        }
        panic!("the driver never settled");
    }

    #[test]
    fn a_connection_arrives_with_its_peer_address() {
        let rt = Runtime::new().expect("Runtime::new");

        rt.block_on(async move {
            let listener = TcpListener::bind(any()).await.expect("bind");
            let addr = listener.local_addr().await.expect("local_addr");

            let client = thread::spawn(move || {
                let mut sock = StdStream::connect(addr).expect("connect");
                sock.write_all(b"hello").expect("write");
                sock.local_addr().expect("local_addr")
            });

            let (stream, peer) = listener.accept().await.expect("accept");
            let (n, buf) = stream.read(vec![0u8; 16]).await;

            assert_eq!(n.expect("read"), 5);
            assert_eq!(&buf[..5], b"hello");
            assert_eq!(peer, client.join().expect("client thread"));
        });
    }

    #[test]
    fn a_stream_writes_back() {
        let rt = Runtime::new().expect("Runtime::new");

        rt.block_on(async move {
            let listener = TcpListener::bind(any()).await.expect("bind");
            let addr = listener.local_addr().await.expect("local_addr");

            let client = thread::spawn(move || {
                let mut sock = StdStream::connect(addr).expect("connect");
                sock.write_all(b"ping").expect("write");
                let mut got = [0u8; 4];
                sock.read_exact(&mut got).expect("read");
                got
            });

            let (stream, _) = listener.accept().await.expect("accept");
            let (n, buf) = stream.read(vec![0u8; 16]).await;
            assert_eq!(n.expect("read"), 4);

            let (written, _) = stream.write(b"pong".to_vec()).await;
            assert_eq!(written.expect("write"), 4);
            drop(buf);

            assert_eq!(&client.join().expect("client thread"), b"pong");
        });
    }

    #[test]
    fn a_read_past_the_peers_close_reports_end_of_file() {
        let rt = Runtime::new().expect("Runtime::new");

        rt.block_on(async move {
            let listener = TcpListener::bind(any()).await.expect("bind");
            let addr = listener.local_addr().await.expect("local_addr");
            let client = thread::spawn(move || drop(StdStream::connect(addr).expect("connect")));

            let (stream, _) = listener.accept().await.expect("accept");
            client.join().expect("client thread");

            let (n, _) = stream.read(vec![0u8; 16]).await;
            assert_eq!(n.expect("read"), 0, "a clean close reads as zero bytes");
        });
    }

    /// Appending rather than overwriting, which is what an open-ended slice is
    /// for — and the thing most likely to be got wrong at a call site.
    #[test]
    fn reading_into_a_slice_appends() {
        let rt = Runtime::new().expect("Runtime::new");

        rt.block_on(async move {
            let listener = TcpListener::bind(any()).await.expect("bind");
            let addr = listener.local_addr().await.expect("local_addr");
            let client = thread::spawn(move || {
                let mut sock = StdStream::connect(addr).expect("connect");
                sock.write_all(b"world").expect("write");
            });

            let (stream, _) = listener.accept().await.expect("accept");

            let mut buf = Vec::with_capacity(32);
            buf.extend_from_slice(b"hello ");

            let (n, slice) = stream.read(buf.slice(6..)).await;
            assert_eq!(n.expect("read"), 5);
            assert_eq!(slice.into_inner(), b"hello world");

            client.join().expect("client thread");
        });
    }

    #[test]
    fn dropping_a_stream_returns_its_slot() {
        let rt = Runtime::new().expect("Runtime::new");

        rt.block_on(async move {
            let listener = TcpListener::bind(any()).await.expect("bind");
            let addr = listener.local_addr().await.expect("local_addr");
            let driver = context::driver();
            let before = driver.slots_used();

            let client = thread::spawn(move || StdStream::connect(addr).expect("connect"));
            let (stream, _) = listener.accept().await.expect("accept");
            assert_eq!(driver.slots_used(), before + 1, "the connection took one");

            drop(stream);
            settle(|| driver.slots_used() == before).await;

            drop(client.join().expect("client thread"));
        });
    }

    /// The trap [`Completable::cleanup`] exists for. An accept that succeeds
    /// after its future is gone has produced a connection in a slot that no
    /// handle names; unless the cleanup closes it, both are lost for good.
    #[test]
    fn an_abandoned_accept_returns_its_slot() {
        let rt = Runtime::new().expect("Runtime::new");

        rt.block_on(async move {
            let listener = TcpListener::bind(any()).await.expect("bind");
            let addr = listener.local_addr().await.expect("local_addr");
            let driver = context::driver();
            let before = driver.slots_used();

            let local = noop_local_waker();
            let mut cx = ContextBuilder::from_waker(Waker::noop())
                .local_waker(&local)
                .build();

            // Poll once so the accept is actually submitted and holds a slot,
            // then walk away from it. `Box::pin` rather than `pin!` because this
            // has to own the future: dropping a `Pin<&mut F>` drops the pointer
            // and leaves the operation running.
            let mut accept = Box::pin(listener.accept());
            assert!(matches!(accept.as_mut().poll(&mut cx), Poll::Pending));
            assert_eq!(driver.slots_used(), before + 1, "it reserved a slot");
            drop(accept);

            // Race a connection against the cancellation: whichever wins, the
            // slot has to come back.
            let client = thread::spawn(move || StdStream::connect(addr).ok());

            settle(|| driver.slots_used() == before).await;
            drop(client.join().expect("client thread"));
        });
    }

    /// The whole point of `local_addr`: bind to port 0 and find out what the
    /// kernel picked. This is also the first check that the socket passthrough
    /// command works at all, and works on a direct descriptor.
    #[test]
    fn a_listener_reports_the_port_the_kernel_chose() {
        let rt = Runtime::new().expect("Runtime::new");

        rt.block_on(async move {
            let listener = TcpListener::bind("127.0.0.1:0".parse().unwrap())
                .await
                .expect("bind");
            let bound = listener.local_addr().await.expect("local_addr");

            assert_eq!(bound.ip(), "127.0.0.1".parse::<std::net::IpAddr>().unwrap());
            assert_ne!(bound.port(), 0, "the kernel chose a real port");
        });
    }

    /// Both ends inside the runtime, with no `std::net` anywhere. The connect
    /// resolves before anyone accepts because the kernel finishes the handshake
    /// out of the listen backlog, which is what lets a single-threaded test
    /// drive both halves in sequence.
    #[test]
    fn a_connection_made_and_accepted_carries_bytes_both_ways() {
        let rt = Runtime::new().expect("Runtime::new");

        rt.block_on(async move {
            let listener = TcpListener::bind(any()).await.expect("bind");
            let addr = listener.local_addr().await.expect("local_addr");

            let client = TcpStream::connect(addr).await.expect("connect");
            let (server, _) = listener.accept().await.expect("accept");

            let (n, _) = client.write(b"ping".to_vec()).await;
            assert_eq!(n.expect("write"), 4);

            let (n, buf) = server.read(vec![0u8; 16]).await;
            assert_eq!(n.expect("read"), 4);
            assert_eq!(&buf[..4], b"ping");

            let (n, _) = server.write(b"pong".to_vec()).await;
            assert_eq!(n.expect("write"), 4);

            let (n, buf) = client.read(vec![0u8; 16]).await;
            assert_eq!(n.expect("read"), 4);
            assert_eq!(&buf[..4], b"pong");
        });
    }

    /// `getsockname` and `getpeername` from both sides of one connection, which
    /// is the only arrangement that catches the two being swapped.
    #[test]
    fn both_ends_agree_on_who_is_where() {
        let rt = Runtime::new().expect("Runtime::new");

        rt.block_on(async move {
            let listener = TcpListener::bind(any()).await.expect("bind");
            let addr = listener.local_addr().await.expect("local_addr");
            let client = TcpStream::connect(addr).await.expect("connect");
            let (server, peer) = listener.accept().await.expect("accept");

            // The address the accept reported is the one the client is really
            // using, ephemeral port and all.
            assert_eq!(peer, client.local_addr().await.expect("client local"));

            // And the two ends agree in both directions. Checking only one
            // direction would not catch `peer` being wired up backwards.
            assert_eq!(
                server.local_addr().await.expect("server local"),
                client.peer_addr().await.expect("client peer"),
            );
            assert_eq!(
                server.peer_addr().await.expect("server peer"),
                client.local_addr().await.expect("client local"),
            );
        });
    }

    #[test]
    fn a_shutdown_write_is_read_as_end_of_file() {
        let rt = Runtime::new().expect("Runtime::new");

        rt.block_on(async move {
            let listener = TcpListener::bind(any()).await.expect("bind");
            let addr = listener.local_addr().await.expect("local_addr");
            let client = TcpStream::connect(addr).await.expect("connect");
            let (server, _) = listener.accept().await.expect("accept");

            client.shutdown().await.expect("shutdown");

            let (n, _) = server.read(vec![0u8; 16]).await;
            assert_eq!(n.expect("read"), 0);
        });
    }

    #[test]
    fn a_refused_connection_reports_it_and_returns_the_slot() {
        let rt = Runtime::new().expect("Runtime::new");
        // Nothing ever listens here, so the first SYN is answered with a reset.
        let addr = free_addr();

        rt.block_on(async move {
            let driver = context::driver();
            let before = driver.slots_used();

            let err = TcpStream::connect(addr)
                .await
                .expect_err("nothing is listening");
            assert_eq!(err.raw_os_error(), Some(libc::ECONNREFUSED));

            settle(|| driver.slots_used() == before).await;
        });
    }

    /// The chain counterpart of [`an_abandoned_accept_returns_its_slot`]: a
    /// chain dropped mid-flight has a socket somewhere in it that only
    /// [`ChainCompletable::cleanup`] will ever close.
    #[test]
    fn an_abandoned_connect_returns_its_slot() {
        let rt = Runtime::new().expect("Runtime::new");

        rt.block_on(async move {
            let listener = TcpListener::bind(any()).await.expect("bind");
            let addr = listener.local_addr().await.expect("local_addr");
            let driver = context::driver();
            let before = driver.slots_used();

            let local = noop_local_waker();
            let mut cx = ContextBuilder::from_waker(Waker::noop())
                .local_waker(&local)
                .build();

            // As in the accept case, `Box::pin` so that the drop below drops the
            // future rather than a pointer to it.
            let mut connect = Box::pin(TcpStream::connect(addr));
            assert!(matches!(connect.as_mut().poll(&mut cx), Poll::Pending));
            assert_eq!(driver.slots_used(), before + 1, "it reserved a slot");
            drop(connect);

            // Whether the connect beat the cancellation or lost to it, the slot
            // comes back.
            settle(|| driver.slots_used() == before).await;
            drop(listener);
        });
    }
}
