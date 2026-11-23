//! The Unix connection's writer bounds, against a hand-rolled fake daemon.
//!
//! Two failure modes, both of which used to leave the connection hanging:
//!
//! 1. The daemon is GONE (it accepted, then dropped the socket). A queued
//!    message must fail the writer, which closes the transport so the reader's
//!    blocking read unblocks — the whole connection returns instead of waiting
//!    on a peer that will never speak again.
//! 2. The daemon is ALIVE BUT NEVER READS. A message larger than the socket
//!    buffer cannot be written at all, and the writer's per-syscall stall must
//!    end the attempt rather than blocking until the whole size-scaled budget
//!    expires.
//!
//! The first is fast and join-driven (no sleeps, no timers). The second has to
//! wait out the real stall: the 1 MiB write blocks for one stall window, is
//! partly accepted, then blocks for a second one and fails — roughly ten seconds
//! — which is why it lives in the `#[ignore]`d suite.

use std::fs;
use std::os::unix::net::UnixListener;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use choreo_client_core::run_daemon_connection;
use choreo_proto::{ClientMessage, ClientMessageType};

/// A socket path short enough for the platform's `sun_path` limit (macOS caps it
/// near 104 bytes, and the temp dir already spends much of that), unique enough
/// within a test run.
fn socket_path(tag: &str) -> std::path::PathBuf {
    std::env::temp_dir().join(format!("cuww-{tag}-{}.sock", std::process::id()))
}

/// A generous wall-clock cap for a test whose expected teardown is driven by a
/// five-second stall. It only has to distinguish "returned" from "hung".
const TEARDOWN_CAP: Duration = Duration::from_secs(20);

/// The daemon accepted, then dropped the connection. One queued message must be
/// enough to tear the whole connection down: the writer fails, closes the
/// transport, and the reader (blocked on a read that would otherwise wait
/// forever) returns.
#[test]
#[ignore = "integration"]
fn a_dropped_daemon_socket_tears_the_connection_down() {
    let path = socket_path("drop");
    let _ = fs::remove_file(&path);
    let listener = UnixListener::bind(&path).expect("bind listener");
    let path_string = path.to_string_lossy().into_owned();

    // The fake daemon accepts and immediately drops: from the client's side the
    // peer is gone, which is the condition the writer must survive.
    let (accepted_tx, accepted_rx) = mpsc::channel();
    let daemon = thread::spawn(move || {
        let (stream, _) = listener.accept().expect("accept");
        drop(stream);
        let _ = accepted_tx.send(());
    });

    let (from_ui_tx, from_ui_rx) = crossbeam_channel::unbounded::<ClientMessage>();
    // The connection result travels over its own channel so the test can bound
    // how long it waits without polling.
    let (done_tx, done_rx) = mpsc::channel();
    let connection = thread::spawn(move || {
        let result = run_daemon_connection(&path_string, |_| {}, from_ui_rx, None);
        let _ = done_tx.send(result);
    });

    accepted_rx.recv().expect("the fake daemon accepted");
    // Queue one message AFTER the daemon has gone. The writer either fails the
    // send (the socket is closed) or has its stop honoured — either way the
    // connection must end, which is what the assertion pins.
    from_ui_tx
        .send(ClientMessage::request(0, ClientMessageType::Ping))
        .expect("from_ui open");

    let result = done_rx
        .recv_timeout(TEARDOWN_CAP)
        .expect("a dropped daemon socket must not hang the connection");
    // A peer that vanished is a clean EOF once the transport is closed, so the
    // reader reports success; the point of the test is that it DID return.
    assert!(
        result.is_ok(),
        "the connection must end cleanly, got {result:?}"
    );

    // Keep `from_ui` open until now: the connection ended on the daemon's
    // disappearance (and the writer's teardown), not on the UI closing.
    drop(from_ui_tx);
    connection.join().expect("connection thread joins");
    daemon.join().expect("fake daemon joins");
    let _ = fs::remove_file(&path);
}

/// The daemon accepts and then never reads. A message larger than the socket
/// buffer cannot be written; the writer's stall must end it, close the
/// transport, and let the connection return well before the message's own
/// size-scaled budget would have expired.
#[test]
#[ignore = "integration"]
fn a_daemon_that_never_reads_stalls_the_writer_out() {
    let path = socket_path("wedged");
    let _ = fs::remove_file(&path);
    let listener = UnixListener::bind(&path).expect("bind listener");
    let path_string = path.to_string_lossy().into_owned();

    // The fake daemon holds the accepted stream for the whole test WITHOUT
    // reading it, so the socket's receive buffer fills and stays full. It is
    // released only after the client's teardown is observed.
    let (accepted_tx, accepted_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel::<()>();
    let daemon = thread::spawn(move || {
        let (stream, _) = listener.accept().expect("accept");
        let _ = accepted_tx.send(());
        let _ = release_rx.recv();
        drop(stream);
    });

    let (from_ui_tx, from_ui_rx) = crossbeam_channel::unbounded::<ClientMessage>();
    let (done_tx, done_rx) = mpsc::channel();
    let connection = thread::spawn(move || {
        let result = run_daemon_connection(&path_string, |_| {}, from_ui_rx, None);
        let _ = done_tx.send(result);
    });

    accepted_rx.recv().expect("the fake daemon accepted");
    // One mebibyte: far past any default unix-socket buffer (~208 KiB), so the
    // write cannot complete while the daemon refuses to read.
    from_ui_tx
        .send(ClientMessage::request(
            1,
            ClientMessageType::RunInput {
                input: vec![0u8; 1 << 20],
            },
        ))
        .expect("from_ui open");

    let result = done_rx
        .recv_timeout(TEARDOWN_CAP)
        .expect("a daemon that never reads must not hang the connection past the writer's bounds");
    // The writer's stall fired → it closed the transport → the reader saw EOF,
    // so the connection ends cleanly rather than reporting a write error.
    assert!(
        result.is_ok(),
        "the connection must end cleanly once the writer stalls out, got {result:?}"
    );

    // `from_ui` was held open for the whole test: the teardown came from the
    // writer's bound, not from the UI closing.
    drop(from_ui_tx);
    let _ = release_tx.send(());
    connection.join().expect("connection thread joins");
    daemon.join().expect("fake daemon joins");
    let _ = fs::remove_file(&path);
}
