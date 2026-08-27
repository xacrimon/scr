//! A TCP echo server.
//!
//! Run it, and it prints the address it landed on:
//!
//! ```text
//! $ cargo run --example echo
//! echo server listening on 127.0.0.1:39421
//! $ nc 127.0.0.1 39421
//! ```
//!
//! Pass an address to pin it somewhere specific: `cargo run --example echo --
//! 127.0.0.1:8080`.
//!
//! Everything here runs on one thread. A connection is a task, and the accept
//! loop is another; what makes that enough is that a task waiting on a read
//! costs nothing but its buffer while the ring holds the operation.

use std::io;
use std::net::SocketAddr;

use scr::buf::IoBuf;
use scr::io::{AsyncRead, AsyncWrite};
use scr::net::{TcpListener, TcpStream};
use scr::{Runtime, spawn};

/// Per connection, so this is the memory a connection costs while idle.
const BUF_SIZE: usize = 16 * 1024;

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
    println!("echo server listening on {}", listener.local_addr().await?);

    loop {
        let (stream, peer) = listener.accept().await?;

        // Spawned rather than awaited, so a slow client cannot hold up the
        // next accept.
        spawn(async move {
            if let Err(e) = echo(stream).await {
                eprintln!("{peer}: {e}");
            }
        });
    }
}

/// Read and write back until the client hangs up.
async fn echo(stream: TcpStream) -> io::Result<()> {
    // Uninitialised capacity, not `vec![0; N]`: a read writes into the whole
    // capacity and reports how much it filled, so zeroing first is wasted work.
    let mut buf = Vec::with_capacity(BUF_SIZE);

    loop {
        // The buffer goes to the kernel and comes back, whether or not the read
        // worked — which is why it is rebound before the error is looked at.
        let (result, returned) = stream.read(buf).await;
        buf = returned;

        let read = result?;
        if read == 0 {
            // A clean close from the far end.
            return Ok(());
        }

        // A send is under no obligation to take everything, so keep going until
        // it has. `slice` is what makes that expressible: the unsent tail is
        // handed over without copying it anywhere.
        let mut sent = 0;
        while sent < read {
            let (result, slice) = stream.write(buf.slice(sent..read)).await;
            buf = slice.into_inner();
            sent += result?;
        }
    }
}
