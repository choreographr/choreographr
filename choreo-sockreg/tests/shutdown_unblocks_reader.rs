//! Integration test: `shutdown_all` must un-block a thread that is blocked in
//! a blocking `read()` on a registered socket — the core reason the registry
//! exists. No sleeps anywhere: the shutdown itself is what makes the read
//! return, so a plain blocking `recv()` on the result channel is the
//! event-driven assertion.

#![cfg(unix)]
// Spawns a real reader thread against a real loopback socket — integration
// territory, and marked #[ignore] per the workspace test discipline
// (run with `cargo test-integration` / `cargo nextest run -- --ignored`).

use std::io::Read;
use std::net::{TcpListener, TcpStream};
use std::os::fd::OwnedFd;

use choreo_sockreg::SocketRegistry;

#[test]
#[ignore = "integration test: binds a real loopback socket and spawns a thread"]
fn shutdown_all_unblocks_blocking_reader() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
    let addr = listener.local_addr().expect("local addr");
    let client = TcpStream::connect(addr).expect("connect");
    let (_server, _) = listener.accept().expect("accept");

    let registry = SocketRegistry::new();
    // The registry gets a duplicate; the test thread keeps the original
    // client stream. shutdown_all closes the dup — the ORIGINAL stream is
    // the one whose blocked read must un-block (same underlying socket).
    let dup = client.try_clone().expect("try_clone");
    registry.register(OwnedFd::from(dup));

    // Reader thread blocks in read() until shutdown_all forces it out.
    let (tx, rx) = std::sync::mpsc::channel::<std::io::Result<usize>>();
    let mut reader_client = client;
    let reader = std::thread::spawn(move || {
        let mut buf = [0u8; 64];
        tx.send(reader_client.read(&mut buf))
    });

    // Give the reader a moment to actually reach the blocking read: NOT a
    // sleep — we assert on the channel after shutdown, and the read is
    // guaranteed to return once shutdown fires regardless of whether the
    // reader had entered read() yet (a read after shutdown returns
    // immediately with EOF/ECONNRESET, so there is no race in the verdict).
    registry.shutdown_all();

    // Blocking recv(): returns as soon as the reader's read() returned.
    let sent = rx.recv().expect("reader thread sent its read result");
    match sent {
        // Linux: local SHUT_RD+WR typically surfaces as ECONNRESET or EOF.
        Ok(0) => {}
        Err(e)
            if matches!(
                e.kind(),
                std::io::ErrorKind::ConnectionReset
                    | std::io::ErrorKind::ConnectionAborted
                    | std::io::ErrorKind::UnexpectedEof
                    | std::io::ErrorKind::BrokenPipe
            ) => {}
        other => panic!("expected EOF/connection error after shutdown, got {other:?}"),
    }
    let _ = reader.join().expect("reader thread joined");
}
