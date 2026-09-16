//! Integration tests for the Windows (Winsock) socket-registry path.
//!
//! Windows-only: `#![cfg(windows)]` compiles this to an empty test crate on
//! every other platform, and each test is `#[ignore]`d (it binds a real
//! loopback socket) per the workspace test discipline — run with
//! `cargo test-integration` on a Windows host.

#![cfg(windows)]
// AGENTS.md permits expect()/panic!() in tests/ files, but clippy's
// allow-expect-in-tests config only recognizes #[test]-annotated functions —
// the `loopback_pair` helper below needs this file-level allowance (same
// pattern as choreo-daemon/tests/common/mod.rs).
#![allow(clippy::expect_used)]

use std::io::Read;
use std::net::{TcpListener, TcpStream};
use std::os::windows::io::OwnedSocket;

use choreo_sockreg::SocketRegistry;

/// A connected loopback TCP pair: (client, server). The client is the
/// socket the registry registers.
fn loopback_pair() -> (TcpStream, TcpStream) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
    let addr = listener.local_addr().expect("local addr");
    let client = TcpStream::connect(addr).expect("connect");
    let (server, _) = listener.accept().expect("accept");
    (client, server)
}

#[test]
#[ignore = "integration test: binds a real loopback socket and spawns a thread"]
fn shutdown_all_unblocks_blocking_reader() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
    let addr = listener.local_addr().expect("local addr");
    let client = TcpStream::connect(addr).expect("connect");
    let (_server, _) = listener.accept().expect("accept");

    let registry = SocketRegistry::new();
    // The registry owns a DUPLICATE; the original stays with the test so the
    // blocked read is on a stream whose twin was shut down (same socket).
    let dup = client.try_clone().expect("try_clone");
    let _id = registry.register(OwnedSocket::from(dup));

    // crossbeam per the workspace house rule (never std::sync::mpsc for
    // cross-thread messaging, even in tests):
    // - unbounded like the production event paths;
    // - `send` returning an Err on a dropped receiver keeps this a fair
    //   handshake (a `try_send`-style embedding would lose that).
    let (tx, rx) = crossbeam_channel::bounded::<std::io::Result<usize>>(1);
    let mut reader_client = client;
    let reader = std::thread::spawn(move || {
        let mut buf = [0u8; 64];
        tx.send(reader_client.read(&mut buf))
    });

    registry.shutdown_all();

    // Blocking `recv` is fine here: the reader thread's send is the only
    // sender and the runtime is the (single) consumer — a plain handshake.
    let sent = rx.recv().expect("reader thread sent its read result");
    match sent {
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

#[test]
#[ignore = "integration test: binds a real loopback socket"]
fn prune_dead_removes_only_dead_sockets() {
    let registry = SocketRegistry::new();
    // A live connection (peer kept alive) ...
    let (live, _peer) = loopback_pair();
    let live_id = registry.register(OwnedSocket::from(live.try_clone().expect("try_clone")));
    // ... and a doomed one: dropping its peer makes the probe's MSG_PEEK
    // see EOF, so prune must remove exactly it.
    let (doomed, doomed_peer) = loopback_pair();
    let doomed_id = registry.register(OwnedSocket::from(doomed.try_clone().expect("try_clone")));
    drop(doomed_peer);

    let pruned = registry.prune_dead();
    assert_eq!(pruned, 1, "exactly the dead entry must be pruned");
    assert_eq!(registry.registered_count(), 1);

    // The pruned id must NO-OP (ownership already transferred to prune);
    // the surviving (live) id must still unregister cleanly.
    registry.unregister(doomed_id);
    assert_eq!(registry.registered_count(), 1);
    registry.unregister(live_id);
    assert_eq!(registry.registered_count(), 0);
}
