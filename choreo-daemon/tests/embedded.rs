//! Integration tests for the embedded (in-process) transport
//! (`crate::embedded`). `#[ignore = "integration"]`-marked per the repo's test discipline
//! (they spawn threads, databases, and exercise the full handler pipeline);
//! run with `cargo nextest run -p choreo-daemon --ignored embedded` or the
//! `cargo test-integration` alias.
//!
//! Determinism: NO sleep-based waits anywhere. Every wait is a channel
//! operation — `recv()` blocks on the kernel, and channel close IS the EOF,
//! so "wait for `ShuttingDown` then observe close" is two blocking recvs.

// AGENTS.md permits unwrap/expect/panic in tests/ files, but clippy's
// allow-*-in-tests config only recognizes #[test]-annotated functions —
// helper fns in this file need this file-level allowance.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::panic_in_result_fn,
    clippy::indexing_slicing
)]
use choreo_daemon::daemon::OpenOptions;
use choreo_daemon::{DaemonState, EmbeddedOptions, ToolPolicy, spawn_embedded};
use choreo_proto::{ClientMessage, DaemonMessage, SessionEvent};

/// Open a `DaemonState` rooted in `dir` (temp sandbox: explicit paths, no env
/// overrides) with the unrestricted tool policy.
fn open_state(dir: &tempfile::TempDir) -> DaemonState {
    DaemonState::open(OpenOptions {
        db_path: dir.path().join("state.redb"),
        accounts_path: dir.path().join("accounts.toml"),
        catalog_paths: choreo_daemon::catalog::CatalogPaths {
            bin: dir.path().join("catalog.bin"),
            overlay: dir.path().join("models-overlay.toml"),
        },
        tool_policy: ToolPolicy::Full,
        max_turns: 0,
        // No platform bridge in these tests — the ios group stays unregistered.
        platform_tool_bridge: None,
    })
    .unwrap()
}

/// Create a session and wait for the connection-level reply (the
/// `SessionCreated` event comes back to the CREATING client directly, via
/// the same single-writer channel broadcasts ride on).
fn create_session(link: &choreo_daemon::EmbeddedLink) -> u64 {
    link.client_tx
        .send(ClientMessage::CreateSession {
            title: Some("embedded test".to_string()),
            parent_session_id: None,
            working_dir: None,
            context_config: None,
            account_name: None,
            selected_model: None,
            reasoning_effort: None,
        })
        .unwrap();
    loop {
        // Session-status broadcasts and other traffic are expected here;
        // keep waiting for the connection-level creation reply.
        if let DaemonMessage::Session {
            session_id: Some(sid),
            event: SessionEvent::SessionCreated { .. },
        } = link.daemon_rx.recv().unwrap()
        {
            return sid;
        }
    }
}

/// A full value round-trip: no codec anywhere. `CreateSession` then
/// `ListSessions`, expecting the reply events and broadcasts as VALUES on
/// `daemon_rx`; then a second link must work after the first is dropped,
/// proving the daemon stayed healthy across the disconnect.
#[test]
#[ignore = "integration"]
fn embedded_transport_round_trips_values() {
    let dir = tempfile::tempdir().unwrap();
    let daemon = spawn_embedded(open_state(&dir), EmbeddedOptions::default()).unwrap();

    let link = daemon.connect().unwrap();
    let sid = create_session(&link);
    assert!(sid > 0);

    // A request/response round trip over the SAME value channel.
    link.client_tx.send(ClientMessage::ListSessions).unwrap();
    let saw_list = loop {
        if let DaemonMessage::Sessions { sessions } = link.daemon_rx.recv().unwrap() {
            break sessions;
        }
    };
    assert!(
        saw_list.iter().any(|s| s.session_id == sid),
        "ListSessions must return the session created over this link"
    );

    // The session state must be observable too: attach and read it back.
    link.client_tx
        .send(ClientMessage::AttachSession { session_id: sid })
        .unwrap();
    loop {
        if let DaemonMessage::Session {
            session_id: Some(id),
            event: SessionEvent::SessionAttached,
        } = link.daemon_rx.recv().unwrap()
        {
            assert_eq!(id, sid);
            break;
        }
    }

    // Drop the link (channel close IS the EOF for the embedded transport);
    // the daemon must stay healthy — a second link answers a Ping.
    drop(link);
    let link2 = daemon.connect().unwrap();
    link2.client_tx.send(ClientMessage::Ping).unwrap();
    assert!(matches!(
        link2.daemon_rx.recv().unwrap(),
        DaemonMessage::Pong
    ));

    daemon.shutdown();
}

/// The shutdown contract: the GUI receives `DaemonMessage::ShuttingDown` as
/// a VALUE, and only THEN does the channel close (the next recv returns
/// Err/None) — notify-before-close with no bytes involved.
#[test]
#[ignore = "integration"]
fn shutdown_delivers_shutting_down_before_channel_close() {
    let dir = tempfile::tempdir().unwrap();
    let daemon = spawn_embedded(open_state(&dir), EmbeddedOptions::default()).unwrap();
    let link = daemon.connect().unwrap();

    // Sanity: the link is live before the drain.
    link.client_tx.send(ClientMessage::Ping).unwrap();
    assert!(matches!(
        link.daemon_rx.recv().unwrap(),
        DaemonMessage::Pong
    ));

    // Drain the daemon on a separate thread: the GUI-side observations below
    // must happen WHILE the drain is in flight (that is exactly the ordering
    // under test — notification before close). The link is kept alive here
    // (no drop), so the connection thread stays blocked on `client_rx` until
    // the bounded join; that join is the drain's backstop, not the test's.
    let drain = std::thread::spawn(move || daemon.shutdown());

    // Ordered exactly as the socket transports deliver it: the notification
    // value first, then the EOF (blocking recvs — both are deterministic:
    // the writer thread closes the channel right after the flush).
    assert!(
        matches!(link.daemon_rx.recv().unwrap(), DaemonMessage::ShuttingDown),
        "ShuttingDown must arrive as a value before the channel closes"
    );
    assert!(
        link.daemon_rx.recv().is_err(),
        "after ShuttingDown the channel must be closed"
    );
    // Let the GUI side go so the drain's bounded join of the (now blocked)
    // connection thread doesn't wait out the whole grace period needlessly.
    drop(link);
    drain.join().unwrap();
}
