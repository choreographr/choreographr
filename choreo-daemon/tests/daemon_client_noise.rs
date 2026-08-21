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
use choreo_proto::{ClientMessage, DaemonMessage, SessionEvent};
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
        DaemonMessage::Session {
            session_id,
            event: SessionEvent::SessionCreated { .. },
        } => assert_eq!(session_id, 1),
        other => panic!("expected SessionCreated, got {other:?}"),
    }

    // The new session is visible to ListSessions — exactly one entry,
    // carrying the id the daemon assigned at creation, proving the create
    // path updated the same daemon-side store the list reads from.
    //
    // This client never sent SubscribeSessionsSummary, and summary
    // broadcasts are an explicit opt-in on BOTH transports (tcp_client_thread
    // no longer auto-registers), so no SessionCreated/SessionStatusChanged
    // broadcast arrives between the create reply and the list reply — the
    // next message is exactly the Sessions reply. This is the regression pin
    // for the removed TCP auto-registration.
    client.send(ClientMessage::ListSessions);
    match client.recv() {
        DaemonMessage::Sessions { sessions } => {
            assert_eq!(sessions.len(), 1);
            assert_eq!(sessions[0].session_id, 1);
        }
        other => panic!("expected Sessions, got {other:?}"),
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
        DaemonMessage::Session {
            session_id,
            event: SessionEvent::SessionCreated { .. },
        } => assert_eq!(session_id, 1),
        other => panic!("expected SessionCreated, got {other:?}"),
    }

    // ...and the Unix client sees it: both listeners serve the same
    // DaemonState, so a session created through one transport is visible
    // through the other.
    //
    // Neither client has subscribed to the session summary (the Unix client
    // never sends SubscribeSessionsSummary and the Noise client was not
    // auto-registered), so no broadcast lands on the Unix channel before its
    // reply — the next message must be exactly the Sessions reply.
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
fn noise_subscribe_receives_session_broadcasts() {
    // Two authorized Noise clients with independent keypairs: A opts into
    // session-summary broadcasts, B stays unsubscribed. This is the
    // regression pin for the removed TCP auto-registration — summary
    // broadcasts must now be earned with an explicit
    // SubscribeSessionsSummary on the Noise transport, exactly as on the
    // Unix path.
    let (key_dir_a, client_pk_a) = test_keypair();
    let (key_dir_b, client_pk_b) = test_keypair();
    let mut daemon = common::SpawnedDaemon::start(&[client_pk_a, client_pk_b]);

    let client_a = NoiseClient::connect(
        &daemon.tcp_addr.to_string(),
        &daemon.server_pk,
        key_dir_a.path().to_path_buf(),
    );
    let client_b = NoiseClient::connect(
        &daemon.tcp_addr.to_string(),
        &daemon.server_pk,
        key_dir_b.path().to_path_buf(),
    );

    // A opts in to summary broadcasts; B never sends the message.
    client_a.send(ClientMessage::SubscribeSessionsSummary);

    // Synchronize: A's connection thread forwards SubscribeSessionsSummary
    // and ListSessions to the daemon command loop in order, so when A
    // receives the Sessions reply the RegisterSummarySubscriber has been
    // processed. Without this barrier, B's CreateSession below could win the
    // race to the daemon loop and session 1's broadcast would miss A
    // (zero subscribers at broadcast time) — flaking the test.
    client_a.send(ClientMessage::ListSessions);
    match client_a.recv() {
        DaemonMessage::Sessions { sessions } => assert!(sessions.is_empty()),
        other => panic!("expected empty Sessions, got {other:?}"),
    }

    // B creates a session. An unsubscribed client must receive only the
    // DIRECT SessionCreated reply — not the duplicate SessionCreated and
    // SessionStatusChanged broadcasts that the old auto-registration pushed
    // onto every TCP client's writer channel. (Under auto-registration this
    // single recv could have returned either broadcast first, which is why
    // the old tests drained; the exact-one-message assert below is the pin.)
    client_b.send(create_session());
    match client_b.recv() {
        DaemonMessage::Session {
            session_id,
            event: SessionEvent::SessionCreated { .. },
        } => assert_eq!(session_id, 1),
        other => panic!("expected SessionCreated, got {other:?}"),
    }

    // A, subscribed, sees B's creation as two summary broadcasts: a
    // SessionCreated and a SessionStatusChanged. The daemon loop emits them
    // back-to-back onto A's writer channel, but the direct-reply thread and
    // the daemon loop both write to that channel, so their relative order is
    // not load-bearing — accept either order; both must arrive.
    let mut saw_created = false;
    let mut saw_status = false;
    for _ in 0..2 {
        match client_a.recv() {
            DaemonMessage::Session {
                session_id,
                event: SessionEvent::SessionCreated { .. },
            } => {
                assert_eq!(session_id, 1);
                saw_created = true;
            }
            DaemonMessage::Session {
                session_id,
                event: SessionEvent::SessionStatusChanged { .. },
            } => {
                assert_eq!(session_id, 1);
                saw_status = true;
            }
            other => panic!("expected summary broadcast, got {other:?}"),
        }
    }
    assert!(
        saw_created,
        "subscribed client must receive the SessionCreated broadcast"
    );
    assert!(
        saw_status,
        "subscribed client must receive the SessionStatusChanged broadcast"
    );

    // A creates a session too. A receives three messages for it — the
    // direct SessionCreated reply plus its own broadcast SessionCreated and
    // SessionStatusChanged — in racy order, so count rather than sequence
    // them. B (unsubscribed) must stay quiet: a Ping's Pong has to be B's
    // very next message, which it could not be if any broadcast about A's
    // session had leaked onto B's writer channel.
    client_a.send(create_session());
    let mut created_2 = 0;
    let mut status_2 = 0;
    for _ in 0..3 {
        match client_a.recv() {
            DaemonMessage::Session {
                session_id,
                event: SessionEvent::SessionCreated { .. },
            } => {
                assert_eq!(session_id, 2);
                created_2 += 1;
            }
            DaemonMessage::Session {
                session_id,
                event: SessionEvent::SessionStatusChanged { .. },
            } => {
                assert_eq!(session_id, 2);
                status_2 += 1;
            }
            other => panic!("expected session 2 traffic, got {other:?}"),
        }
    }
    assert_eq!(created_2, 2, "direct reply + broadcast SessionCreated");
    assert_eq!(status_2, 1, "broadcast SessionStatusChanged");

    client_b.send(ClientMessage::Ping);
    assert_eq!(client_b.recv(), DaemonMessage::Pong);

    daemon.shutdown();
    client_a.finish().expect("clean close after shutdown");
    client_b.finish().expect("clean close after shutdown");
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

/// Test that a >64 KiB message survives the FULL daemon round trip. The
/// client's 1 MiB AddCredential payload must fragment on the wire (snow's
/// single-fragment cap is 65518 plaintext bytes); the daemon's
/// `tcp_client_thread` reassembles it via `recv_client_message`, the command
/// loop stores the blob (`handle_add_credential_sync`), and the CredentialAdded
/// reply travels back through the same encrypted channel. This proves the
/// framing change is invisible above the transport: typed proto messages can
/// now be as large as the codec's 32 MiB `MAX_FRAME_SIZE`, not just 65518
/// bytes.
#[test]
#[ignore]
fn noise_large_message_through_daemon() {
    let (key_dir, client_pk) = test_keypair();
    let mut daemon = common::SpawnedDaemon::start(&[client_pk]);
    let client = NoiseClient::connect(
        &daemon.tcp_addr.to_string(),
        &daemon.server_pk,
        key_dir.path().to_path_buf(),
    );

    // 1 MiB encrypted credential blob — 17 wire fragments — far beyond the
    // 65518-byte single-fragment cap, so the sender must split it and
    // the daemon's reader must glue the fragments back before the command
    // loop ever sees the message.
    client.send(ClientMessage::AddCredential {
        service: "big-blob".into(),
        encrypted_payload: vec![0x42u8; 1024 * 1024],
        unlock_key: None,
    });
    match client.recv() {
        DaemonMessage::CredentialAdded { service } => assert_eq!(service, "big-blob"),
        other => panic!("expected CredentialAdded, got {other:?}"),
    }

    daemon.shutdown();
    client.finish().expect("clean close after shutdown");
}

/// Test that a >64 KiB message travels daemon → client through the FULL
/// daemon stack — the reverse direction of `noise_large_message_through_daemon`
/// (which is client → daemon). The client creates sessions with multi-KiB
/// titles, so the aggregate ListSessions reply exceeds snow's single-message
/// cap and must fragment on the wire: the daemon's writer thread splits it
/// (send_daemon_message → send_message) and the client's reader thread
/// reassembles it (recv_daemon_message → recv_message). The reply must
/// arrive intact.
#[test]
#[ignore]
fn noise_large_message_daemon_to_client() {
    let (key_dir, client_pk) = test_keypair();
    let mut daemon = common::SpawnedDaemon::start(&[client_pk]);
    let client = NoiseClient::connect(
        &daemon.tcp_addr.to_string(),
        &daemon.server_pk,
        key_dir.path().to_path_buf(),
    );

    // Titles large enough that the aggregate Sessions reply exceeds the
    // 65518-byte single-fragment cap: 12 × 8 KiB of title bytes
    // (~96 KiB) fragments into at least two wire fragments.
    const SESSIONS: usize = 12;
    let big_title = "x".repeat(8 * 1024);
    for i in 0..SESSIONS {
        client.send(ClientMessage::CreateSession {
            title: Some(big_title.clone()),
            parent_session_id: None,
            working_dir: None,
            context_config: None,
            account_name: None,
            selected_model: None,
            reasoning_effort: None,
        });
        match client.recv() {
            DaemonMessage::Session {
                session_id,
                event: SessionEvent::SessionCreated { .. },
            } => {
                assert_eq!(session_id, (i + 1) as u64)
            }
            other => panic!("expected SessionCreated, got {other:?}"),
        }
    }

    // This client never subscribed to the session summary, so the next
    // message is exactly the Sessions reply — and it must be intact after
    // reassembly.
    client.send(ClientMessage::ListSessions);
    match client.recv() {
        DaemonMessage::Sessions { sessions } => {
            assert_eq!(sessions.len(), SESSIONS);
            for s in &sessions {
                assert_eq!(s.title.as_deref(), Some(big_title.as_str()));
            }
            // Sanity: the reply really was large enough to fragment.
            let total: usize = sessions
                .iter()
                .map(|s| s.title.as_deref().map_or(0, |t| t.len()))
                .sum();
            assert!(
                total > 65518,
                "test premise broken: aggregated reply is not >64 KiB ({total} bytes)"
            );
        }
        other => panic!("expected Sessions, got {other:?}"),
    }

    daemon.shutdown();
    client.finish().expect("clean close after shutdown");
}
