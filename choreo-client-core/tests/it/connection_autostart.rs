//! Autostart contract tests that cross the filesystem/IPC boundary.
//!
//! AGENTS.md's Test Discipline places tests that bind real sockets or touch
//! the filesystem boundary in crate-level `tests/` directories (marked
//! `#[ignore]`, run via `cargo test-integration`).
//!
//! Both halves of the contract live here, because both cross that boundary: the
//! ABSENT-socket case dials a temp path (a filesystem path no listener holds),
//! and the LIVE-listener case binds a real unix listener.
//!
//! The contract pinned: a LIVE listener means the first dial IS the
//! connection — the autostart hook must never run (a pre-flight probe would
//! have dialed, thrown the stream away, and dialed again — which an
//! `--auto-exit` daemon counts as its last client leaving), and the pump must
//! observe the clean EOF the listener's accept thread produces. When NOTHING is
//! listening, the hook runs exactly once and its failure is the connection
//! result.

use choreo_client_core::{ClientError, run_daemon_connection_with_autostart};
use choreo_proto::ClientMessage;

/// A LIVE listener means the first dial IS the connection: the autostart
/// hook must never run, and the pump must end cleanly on the listener's
/// immediate close.
#[test]
#[ignore = "integration"]
fn autostart_hook_skipped_when_daemon_listens() {
    let dir = std::env::temp_dir().join(format!(
        "choreo-core-autostart-live-{}-{}",
        std::process::id(),
        format!("{:?}", std::thread::current().id()).as_str()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let sock = dir.join("live.sock");
    let listener = std::os::unix::net::UnixListener::bind(&sock).unwrap();
    // Close every accepted stream immediately: EOF for the pump, so this
    // test needs no real daemon messages.
    let accepter = std::thread::spawn(move || {
        for stream in listener.incoming() {
            drop(stream);
        }
    });
    let path = sock.to_string_lossy().into_owned();

    let mut hook_calls = 0;
    let mut ensure_daemon = || -> Result<(), ClientError> {
        hook_calls += 1;
        Ok(())
    };
    let (from_ui_tx, from_ui_rx) = crossbeam_channel::unbounded::<ClientMessage>();
    drop(from_ui_tx);

    let result =
        run_daemon_connection_with_autostart(&path, &mut ensure_daemon, |_| {}, from_ui_rx, None);

    assert!(result.is_ok(), "EOF from the closed accept is clean");
    assert_eq!(hook_calls, 0, "a live daemon must never be autostarted");
    drop(accepter);
    let _ = std::fs::remove_dir_all(&dir);
}

/// An ABSENT socket (`NotFound` at the dial) is the one condition autostart may
/// act on: the hook must run EXACTLY once, and its error must surface as the
/// connection result — the caller sees what the hook failed with, never a
/// synthetic "autostart failed".
#[test]
#[ignore = "integration"]
fn autostart_hook_invoked_when_nothing_listens() {
    let dir = std::env::temp_dir().join(format!(
        "choreo-core-autostart-absent-{}-{}",
        std::process::id(),
        format!("{:?}", std::thread::current().id()).as_str()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    // The socket file itself is never created, so the dial fails with
    // `NotFound` — the classification autostart keys on.
    let sock = dir.join("absent.sock");
    let _ = std::fs::remove_file(&sock);
    let path = sock.to_string_lossy().into_owned();

    let mut hook_calls = 0;
    let mut ensure_daemon = || -> Result<(), ClientError> {
        hook_calls += 1;
        Err(ClientError::DaemonStart("test: no daemon".to_string()))
    };
    let (from_ui_tx, from_ui_rx) = crossbeam_channel::unbounded::<ClientMessage>();
    drop(from_ui_tx); // the pump's writer thread ends immediately

    let error =
        run_daemon_connection_with_autostart(&path, &mut ensure_daemon, |_| {}, from_ui_rx, None)
            .expect_err("the failing hook must fail the connection");

    assert!(matches!(error, ClientError::DaemonStart(_)));
    assert_eq!(hook_calls, 1, "the hook must run exactly once");
    let _ = std::fs::remove_dir_all(&dir);
}
