//! End-to-end daemon ↔ client integration tests over TCP + Noise IK.
//!
//! Each test runs the REAL daemon (`common::SpawnedDaemon` →
//! `choreo_daemon::run_server`) and drives it with the REAL client
//! connection library (`choreo_client_core::run_daemon_tcp_connection`), so
//! the full encrypted wire path is exercised: client writer thread → Noise IK
//! handshake → TCP → daemon `handshake_responder` → ACL check →
//! `tcp_client_thread` → daemon command loop → per-client writer channel →
//! Noise encryption → TCP → client reader loop → handler channel.
//!
//! The client's transport keypair would normally live in the user's real
//! config directory; `choreo_transport::key::set_test_config_root` redirects
//! it to a per-test temp dir. The override is THREAD-LOCAL, so the test
//! thread generates the keypair (to learn its public half for the daemon
//! ACL) and the connection thread re-installs the override before
//! `run_daemon_tcp_connection` loads it — see [`NoiseClient::connect`].
//!
//! These tests belong to the `#[ignore]` integration suite (run via
//! `cargo test-integration` / nextest `--run-ignored only`): they bind real
//! sockets and spawn real threads, so they are excluded from the fast unit
//! suite. The only time-based primitive used is the bounded `recv_timeout`
//! on the message channel, so a wedged daemon fails loudly instead of
//! hanging the suite.

use choreo_client_core::error::ClientError;
use choreo_client_core::run_daemon_connection;
use choreo_client_core::run_daemon_tcp_connection;
use choreo_proto::{ClientMessage, DaemonMessage};
use choreo_transport::key::{ensure_transport_keypair, set_test_config_root};
use std::path::PathBuf;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

mod common;

/// Bounded receive timeout for daemon replies: short enough that a wedged
/// daemon fails the test loudly, long enough that a loaded CI box doesn't
/// flake.
const TIMEOUT: Duration = Duration::from_secs(5);

/// A connected Noise client driven through the real
/// `run_daemon_tcp_connection`.
///
/// The connection lives on its own thread: `run_daemon_tcp_connection`
/// performs the Noise IK handshake and spawns a writer thread that drains
/// `from_ui` into the encrypted channel, while the calling thread blocks in
/// the reader loop and forwards every decoded `DaemonMessage` into `rx`.
/// The thread's `Result<(), ClientError>` is relayed over `result_rx` so the
/// test can join it with a bounded `recv_timeout` instead of a blocking
/// `thread::join`.
struct NoiseClient {
    from_ui: mpsc::Sender<ClientMessage>,
    rx: mpsc::Receiver<DaemonMessage>,
    shutdown_tx: mpsc::Sender<()>,
    result_rx: mpsc::Receiver<Result<(), ClientError>>,
}

impl NoiseClient {
    /// Connect to `addr` over TCP/Noise, presenting the keypair stored in
    /// `key_dir` (the temp config root the TEST thread generated it into).
    ///
    /// The keypair override must be re-installed INSIDE the spawned thread:
    /// `set_test_config_root` is thread-local, and
    /// `run_daemon_tcp_connection` calls `ensure_transport_keypair` on the
    /// thread it runs on. Without the re-install, the connection thread
    /// would generate a fresh keypair in the user's real config directory —
    /// one the daemon's ACL does not authorize. The override is reset before
    /// the thread's result is sent; if the thread panics first, the
    /// thread-local dies with the thread and leaks nothing.
    fn connect(addr: &str, server_pk: &[u8; 32], key_dir: PathBuf) -> Self {
        let (from_ui, to_daemon) = mpsc::channel::<ClientMessage>();
        let (shutdown_tx, shutdown_rx) = mpsc::channel::<()>();
        let (tx, rx) = mpsc::channel::<DaemonMessage>();
        let (result_tx, result_rx) = mpsc::channel::<Result<(), ClientError>>();
        let addr = addr.to_string();
        let server_pk = *server_pk;
        thread::spawn(move || {
            set_test_config_root(Some(key_dir));
            let result = run_daemon_tcp_connection(
                &addr,
                &server_pk,
                |m| {
                    let _ = tx.send(m);
                },
                to_daemon,
                Some(shutdown_rx),
            );
            set_test_config_root(None);
            let _ = result_tx.send(result);
        });
        NoiseClient {
            from_ui,
            rx,
            shutdown_tx,
            result_rx,
        }
    }

    fn send(&self, msg: ClientMessage) {
        self.from_ui.send(msg).expect("send to daemon");
    }

    fn recv(&self) -> DaemonMessage {
        self.rx
            .recv_timeout(TIMEOUT)
            .unwrap_or_else(|e| panic!("timed out waiting for daemon message: {e:?}"))
    }

    /// Bounded join: signals shutdown (severs the socket), waits up to
    /// [`TIMEOUT`] for the connection thread's result, and returns it.
    ///
    /// The shutdown signal is a belt-and-braces path — most tests shut the
    /// daemon down first, which already closes the connection and lets the
    /// reader exit cleanly on its own; the signal only matters for severing
    /// a connection that the daemon has not closed (not used by these
    /// tests, but kept symmetric with the Unix `Client::disconnect`).
    /// Sending on a shutdown channel whose receiver already exited is a
    /// harmless no-op (`let _`).
    fn finish(self) -> Result<(), ClientError> {
        let Self {
            from_ui,
            shutdown_tx,
            result_rx,
            ..
        } = self;
        drop(from_ui); // writer thread sees Disconnected and stops
        let _ = shutdown_tx.send(()); // sever the socket
        result_rx
            .recv_timeout(TIMEOUT)
            .unwrap_or_else(|e| panic!("timed out waiting for connection result: {e:?}"))
    }
}

/// The CreateSession request used throughout: every optional field unset, so
/// the tests exercise the default session-creation path (mirrors the Unix
/// test file's helper).
fn create_session() -> ClientMessage {
    ClientMessage::CreateSession {
        title: None,
        parent_session_id: None,
        working_dir: None,
        context_config: None,
        account_name: None,
        selected_model: None,
        reasoning_effort: None,
    }
}

/// Generate a client transport keypair into a fresh temp config root and
/// return `(key_dir, client_pk)`. The keypair files land in
/// `<key_dir>/choreographr/`, which the connection thread later re-uses.
///
/// `set_test_config_root` is thread-local and set unconditionally here, so a
/// previous test that leaked the override (only possible under libtest's
/// serial fallback, where tests share a process) gets overwritten. It is
/// reset before returning so its scope stays tight — never set across
/// `SpawnedDaemon::start`.
fn test_keypair() -> (tempfile::TempDir, [u8; 32]) {
    let key_dir = tempfile::tempdir().unwrap();
    set_test_config_root(Some(key_dir.path().to_path_buf()));
    let (_sk, client_pk) = ensure_transport_keypair().unwrap();
    set_test_config_root(None);
    (key_dir, client_pk)
}

#[test]
#[ignore]
fn noise_ping_pong_over_tcp() {
    let (key_dir, client_pk) = test_keypair();
    let mut daemon = common::SpawnedDaemon::start(&[client_pk]);
    let client = NoiseClient::connect(
        &daemon.tcp_addr.to_string(),
        &daemon.server_pk,
        key_dir.path().to_path_buf(),
    );

    // The first-ever full-path TCP test: real daemon `handshake_responder`
    // + ACL + `tcp_client_thread` on one side, real client
    // `run_daemon_tcp_connection` on the other, one encrypted round trip
    // through the Noise transport state.
    client.send(ClientMessage::Ping);
    assert_eq!(client.recv(), DaemonMessage::Pong);

    // Graceful shutdown: SIGINT makes the daemon notify every connected
    // Noise client and close the connection, so the reader must exit
    // cleanly on EOF (Ok(()), not an I/O error).
    daemon.shutdown();
    client.finish().expect("clean close after shutdown");
}

#[test]
#[ignore]
fn noise_list_sessions_round_trip() {
    let (key_dir, client_pk) = test_keypair();
    let mut daemon = common::SpawnedDaemon::start(&[client_pk]);
    let client = NoiseClient::connect(
        &daemon.tcp_addr.to_string(),
        &daemon.server_pk,
        key_dir.path().to_path_buf(),
    );

    // Fresh daemon: the session list starts empty.
    client.send(ClientMessage::ListSessions);
    match client.recv() {
        DaemonMessage::Sessions { sessions } => assert!(sessions.is_empty()),
        other => panic!("expected empty Sessions, got {other:?}"),
    }

    // Create a session with all optional fields unset.
    client.send(create_session());
    match client.recv() {
        DaemonMessage::SessionCreated { session_id, .. } => assert_eq!(session_id, 1),
        other => panic!("expected SessionCreated, got {other:?}"),
    }

    // The new session is visible to ListSessions — exactly one entry,
    // carrying the id the daemon assigned at creation, proving the create
    // path updated the same daemon-side store the list reads from.
    //
    // Unlike the Unix client_thread, tcp_client_thread auto-registers every
    // Noise client as a SUMMARY SUBSCRIBER, so session creation also pushes
    // a SessionCreated broadcast (duplicating the direct reply) and a
    // SessionStatusChanged broadcast onto the writer channel — the interleave
    // of the connection thread's reply with the daemon loop's broadcasts is
    // racy, so drain both until the ListSessions reply arrives (the drain is
    // bounded by `recv`'s recv_timeout, so a wedged daemon fails loudly).
    client.send(ClientMessage::ListSessions);
    loop {
        match client.recv() {
            DaemonMessage::SessionCreated { .. } | DaemonMessage::SessionStatusChanged { .. } => {
                continue;
            }
            DaemonMessage::Sessions { sessions } => {
                assert_eq!(sessions.len(), 1);
                assert_eq!(sessions[0].session_id, 1);
                break;
            }
            other => panic!("expected Sessions, got {other:?}"),
        }
    }

    daemon.shutdown();
    client.finish().expect("clean close after shutdown");
}

#[test]
#[ignore]
fn noise_and_unix_share_daemon_state() {
    let (key_dir, client_pk) = test_keypair();
    let mut daemon = common::SpawnedDaemon::start(&[client_pk]);

    // Authorized Noise client.
    let noise_client = NoiseClient::connect(
        &daemon.tcp_addr.to_string(),
        &daemon.server_pk,
        key_dir.path().to_path_buf(),
    );

    // Minimal inline Unix client: spawn `run_daemon_connection` in a
    // thread. Only send + recv + join are needed here — the full Client
    // helper lives in daemon_client_unix.rs.
    let (from_ui, to_daemon) = mpsc::channel::<ClientMessage>();
    let (tx, rx) = mpsc::channel::<DaemonMessage>();
    let (shutdown_tx, shutdown_rx) = mpsc::channel::<()>();
    let socket = daemon.socket_str();
    let unix_handle = thread::spawn(move || {
        run_daemon_connection(
            &socket,
            |m| {
                let _ = tx.send(m);
            },
            to_daemon,
            Some(shutdown_rx),
        )
    });

    // Both connections are live on their own transports.
    noise_client.send(ClientMessage::Ping);
    assert_eq!(noise_client.recv(), DaemonMessage::Pong);
    from_ui.send(ClientMessage::Ping).expect("send to daemon");
    match rx
        .recv_timeout(TIMEOUT)
        .unwrap_or_else(|e| panic!("timed out waiting for Unix daemon message: {e:?}"))
    {
        DaemonMessage::Pong => {}
        other => panic!("expected Pong on Unix connection, got {other:?}"),
    }

    // The Noise client creates a session...
    noise_client.send(create_session());
    match noise_client.recv() {
        DaemonMessage::SessionCreated { session_id, .. } => assert_eq!(session_id, 1),
        other => panic!("expected SessionCreated, got {other:?}"),
    }

    // ...and the Unix client sees it: both listeners serve the same
    // DaemonState, so a session created through one transport is visible
    // through the other.
    from_ui
        .send(ClientMessage::ListSessions)
        .expect("send to daemon");
    match rx
        .recv_timeout(TIMEOUT)
        .unwrap_or_else(|e| panic!("timed out waiting for Unix daemon message: {e:?}"))
    {
        DaemonMessage::Sessions { sessions } => {
            assert_eq!(sessions.len(), 1);
            assert_eq!(sessions[0].session_id, 1);
        }
        other => panic!("expected Sessions on Unix connection, got {other:?}"),
    }

    daemon.shutdown();

    // Both clients must observe the clean close on the EOF that follows.
    noise_client.finish().expect("clean close after shutdown");
    drop(from_ui);
    let _ = shutdown_tx.send(());
    unix_handle
        .join()
        .expect("unix client thread panicked")
        .expect("clean close after shutdown");
}

#[test]
#[ignore]
fn noise_rejects_client_not_in_acl() {
    // Two independent keypairs: dir_a holds the key the daemon authorizes,
    // dir_b holds the key the connecting client actually presents. They
    // must differ, so each gets its own temp config root. Only pk_a is
    // needed after generation (dir_a's files are never re-read).
    let (_key_dir_a, client_pk_a) = test_keypair();
    let (key_dir_b, _client_pk_b) = test_keypair();

    let mut daemon = common::SpawnedDaemon::start(&[client_pk_a]);
    let client = NoiseClient::connect(
        &daemon.tcp_addr.to_string(),
        &daemon.server_pk,
        key_dir_b.path().to_path_buf(),
    );

    // The daemon's `handshake_responder` checks pk_b against the ACL,
    // rejects it, and closes the socket WITHOUT sending its second
    // handshake message; the client's `handshake_initiator` read then hits
    // EOF and the connection returns Err. (The keypairs are necessarily
    // distinct — a collision would make this test flake by authorizing the
    // wrong key, which is vanishingly improbable for X25519.)
    let result = client.finish();
    assert!(result.is_err(), "unauthenticated client must not connect");

    daemon.shutdown();
}

#[test]
#[ignore]
fn noise_wrong_server_public_key_fails() {
    let (key_dir, client_pk) = test_keypair();
    let mut daemon = common::SpawnedDaemon::start(&[client_pk]);

    // The client is authorized, but is handed a bogus server public key.
    // Its first handshake message is then encrypted for the WRONG responder
    // key: the real server cannot decrypt it (the derived shared secret
    // differs), never sends its reply, and closes the socket — so the
    // client's handshake read fails and `run_daemon_tcp_connection` returns
    // Err. (If snow rejects the all-zero key at build time instead, the
    // handshake errors out before any socket I/O — `finish()` still returns
    // Err either way, which is all this test asserts.)
    let wrong_server_pk = [0u8; 32];
    let client = NoiseClient::connect(
        &daemon.tcp_addr.to_string(),
        &wrong_server_pk,
        key_dir.path().to_path_buf(),
    );

    let result = client.finish();
    assert!(result.is_err(), "handshake with wrong server key must fail");

    daemon.shutdown();
}

#[test]
#[ignore]
fn noise_shutdown_notifies_client() {
    let (key_dir, client_pk) = test_keypair();
    let mut daemon = common::SpawnedDaemon::start(&[client_pk]);
    let client = NoiseClient::connect(
        &daemon.tcp_addr.to_string(),
        &daemon.server_pk,
        key_dir.path().to_path_buf(),
    );

    // Prove the encrypted channel is live before shutting the daemon down.
    client.send(ClientMessage::Ping);
    assert_eq!(client.recv(), DaemonMessage::Pong);

    // SIGINT: the shutdown path writes ShuttingDown to every connected
    // Noise client THROUGH the encrypted channel (the Task-2 fix), then
    // closes the connection. Asserting the notification here is what pins
    // that fix — TCP clients previously got only a bare EOF.
    daemon.shutdown();
    assert_eq!(client.recv(), DaemonMessage::ShuttingDown);

    // The EOF that follows the notification must be a clean close — Ok(()),
    // not an I/O error.
    client.finish().expect("clean close after shutdown");
}
