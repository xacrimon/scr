//! Not a real example. Exists so `cargo asm` has concrete monomorphisations of
//! the operation futures to inspect: a plain [`Op`] (a `read`) and a linked
//! [`Chain`] (a `connect`), each behind its own spawned task so the poll
//! function is a symbol of its own rather than being inlined into `main`.

use std::hint::black_box;
use std::net::SocketAddr;

use scr::buf::IoBuf;
use scr::io::AsyncRead;
use scr::net::TcpStream;
use scr::{Runtime, spawn};

fn main() {
    let rt = Runtime::new().expect("Runtime::new");
    let addr: SocketAddr = "127.0.0.1:9".parse().expect("a literal address");

    rt.block_on(async move {
        // The chain: socket + connect, submitted as one linked run.
        let chain_task = spawn(async move {
            black_box(TcpStream::connect(black_box(addr)).await).ok();
        });

        // The single op: a recv into an owned buffer.
        let read_task = spawn(async move {
            if let Ok(stream) = TcpStream::connect(addr).await {
                let (n, buf) = stream.read(Vec::with_capacity(black_box(4096))).await;
                black_box((n.is_ok(), buf.bytes_init()));
            }
        });

        chain_task.await.ok();
        read_task.await.ok();
    });
}
