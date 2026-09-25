//! Autostart contract tests that cross the filesystem/IPC boundary.
//!
//! AGENTS.md's Test Discipline places tests that bind real sockets or touch
//! the filesystem boundary in crate-level `tests/` directories (marked
//! `#[ignore]`, run via `cargo test-integration`). The absent-socket half of
//! the autostart contract stays a unit test in `connection.rs` (a dial to a
//! path that is never created); everything that binds a REAL unix listener
//! lives here.
//!
//! The contract pinned: a LIVE listener means the first dial IS the
//! connection — the autostart hook must never run (a pre-flight probe would
//! have dialed, thrown the stream away, and dialed again — which an
//! `--auto-exit` daemon counts as its last client leaving), and the pump must
//! observe the clean EOF the listener's accept thread produces.

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
