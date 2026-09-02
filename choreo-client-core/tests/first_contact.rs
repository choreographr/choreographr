//! Integration tests for the TCP first-contact trust flow (phase 3).
//!
//! These drive the real client connection library
//! (`choreo_client_core::probe_server_key`, `run_daemon_connection_with_mode`
//! with `ConnectionMode::TcpPinned`) against hand-rolled responder threads
//! built on the real `choreo_transport` handshakes — the same shape the
//! daemon's accept path uses (preamble read → responder → ACL closure). No
//! full daemon is needed here because the behavior under test is entirely
//! client-side: the probe contract, the pinned-mode error guidance, and the
//! pin-driven IK connection.
//!
//! `#[ignore]` per AGENTS.md test discipline (real sockets, spawned
//! threads). The client's transport keypair and the known-servers store are
//! redirected to temp dirs; BOTH overrides are thread-local, so any spawned
//! thread that touches them re-installs them first (see
//! [`spawn_connection_thread`] — the same discipline as choreo-daemon's
//! `daemon_client_noise.rs`).

use std::io::Read;
use std::net::TcpListener;
use std::path::PathBuf;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use choreo_client_core::{ConnectionMode, probe_server_key, run_daemon_connection_with_mode};
use choreo_proto::{ClientMessage, DaemonMessage};
use x25519_dalek::{PublicKey, StaticSecret};

/// A server keypair: raw secret + public bytes, generated from the workspace
/// CSPRNG (same pattern as choreo-transport's and choreo-daemon's noise
/// tests).
struct ServerKeys {
    sk: [u8; 32],
    pk: [u8; 32],
}

impl ServerKeys {
    fn generate() -> Self {
        let sk = StaticSecret::random_from_rng(&mut rand::rng());
        let pk = PublicKey::from(&sk);
        ServerKeys {
            sk: sk.to_bytes(),
            pk: pk.to_bytes(),
        }
    }
}

/// Redirect BOTH test config roots (transport's for the client transport
/// keypair, the keystore's for `known_servers.toml`) to fresh temp dirs and
/// generate the client keypair under the redirect.
///
/// The overrides are THREAD-LOCAL: the main test thread installs them at
/// construction, and [`spawn_connection_thread`] re-installs both inside the
/// connection thread before it dials. Without the re-install, the connection
/// thread would read the user's REAL config directory — a real keypair the
/// responder's ACL does not authorize, and a real known_servers store the
/// pin is not in.
struct TestRoots {
    /// Held so the temp dirs outlive the test body.
    _dirs: Vec<tempfile::TempDir>,
    /// Held so the keystore's thread-local override stays installed on the
    /// main test thread for the whole body (it resets on drop).
    _keystore_guard: choreo_keystore::paths::TestConfigGuard,
    transport_dir: PathBuf,
    keystore_dir: PathBuf,
    client_pk: [u8; 32],
}

impl TestRoots {
    fn install() -> Self {
        let transport_dir = tempfile::TempDir::new().expect("transport tempdir");
        let keystore_dir = tempfile::TempDir::new().expect("keystore tempdir");
        // choreo-transport's override is #[doc(hidden)] but pub — the exact
        // seam choreo-daemon's integration tests use. The guard type is
        // `()`; thread-locals die with the thread, and these test threads
        // are the only users.
        choreo_transport::key::set_test_config_root(Some(transport_dir.path().to_path_buf()));
        let keystore_guard = choreo_keystore::paths::TestConfigGuard::set_root(Some(
            keystore_dir.path().to_path_buf(),
        ));
        // Generate the client keypair NOW, on this thread, under the
        // overrides — so we know its public half for the responder ACLs.
        let (_sk, client_pk) =
            choreo_transport::key::ensure_transport_keypair().expect("client keypair");
        // Capture the paths BEFORE moving the TempDirs into `_dirs` (they
        // must outlive the test body, and their paths are what spawned
        // threads re-install).
        let transport_path = transport_dir.path().to_path_buf();
        let keystore_path = keystore_dir.path().to_path_buf();
        TestRoots {
            _dirs: vec![transport_dir, keystore_dir],
            _keystore_guard: keystore_guard,
            transport_dir: transport_path,
            keystore_dir: keystore_path,
            client_pk,
        }
    }
}

/// Run `run_daemon_connection_with_mode` on a thread with BOTH config-root
/// overrides re-installed (see [`TestRoots`] for why).
#[allow(clippy::too_many_arguments)]
fn spawn_connection_thread(
    roots: &TestRoots,
    mode: ConnectionMode,
    handle_daemon_message: impl FnMut(DaemonMessage) + Send + 'static,
    to_daemon: mpsc::Receiver<ClientMessage>,
) -> thread::JoinHandle<Result<(), choreo_client_core::ClientError>> {
    let transport_dir = roots.transport_dir.clone();
    let keystore_dir = roots.keystore_dir.clone();
    thread::spawn(move || {
        choreo_transport::key::set_test_config_root(Some(transport_dir));
        let _guard = choreo_keystore::paths::TestConfigGuard::set_root(Some(keystore_dir));
        run_daemon_connection_with_mode(mode, handle_daemon_message, to_daemon, None)
    })
}

/// First contact through `probe_server_key`: the learned key must be exactly
/// the server's static.
#[test]
#[ignore]
fn probe_server_key_learns_server_static() {
    let roots = TestRoots::install();
    let _ = &roots; // overrides live for the whole test body
    let server = ServerKeys::generate();

    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("local addr").to_string();

    // Responder: preamble + XX responder authorizing this test's client key.
    let client_pk = roots.client_pk;
    let (result_tx, result_rx) = mpsc::channel();
    thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept");
        let mut preamble = [0u8; 1];
        stream.read_exact(&mut preamble).expect("read preamble");
        assert_eq!(preamble[0], choreo_transport::handshake::PREAMBLE_XX);
        let result =
            choreo_transport::handshake::handshake_responder_xx(stream, &server.sk, |pk| {
                pk == &client_pk
            })
            .map(|_| ());
        let _ = result_tx.send(result);
    });

    let learned = probe_server_key(&addr).expect("probe succeeds against an ACL'd responder");
    assert_eq!(
        learned, server.pk,
        "the probe must learn the server's real static"
    );

    result_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("responder result")
        .expect("responder handshake succeeded");
}

/// THE enrollment preflight, accepted side: `verify_daemon_authorization`
/// against a responder that authorizes this client (the daemon's shape:
/// preamble + IK responder with an ACL closure) must succeed without any
/// protocol message being exchanged — the preflight drops the transport.
#[test]
#[ignore]
fn verify_daemon_authorization_accepts_enrolled_client() {
    let roots = TestRoots::install();
    let _ = &roots;
    let server = ServerKeys::generate();

    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("local addr").to_string();

    let client_pk = roots.client_pk;
    let (result_tx, result_rx) = mpsc::channel();
    thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept");
        let mut preamble = [0u8; 1];
        stream.read_exact(&mut preamble).expect("read preamble");
        assert_eq!(preamble[0], choreo_transport::handshake::PREAMBLE_IK);
        let result = choreo_transport::handshake::handshake_responder(stream, &server.sk, |pk| {
            pk == &client_pk
        })
        .map(|_| ());
        let _ = result_tx.send(result);
    });

    choreo_client_core::verify_daemon_authorization(&addr, &server.pk)
        .expect("an enrolled client must pass the preflight");

    // The responder completed the handshake too (the preflight is a real
    // full IK handshake, then drops the transport).
    result_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("responder result")
        .expect("responder handshake succeeded");
}

/// THE enrollment preflight, rejected side — the exact bug this closes: a
/// client NOT in the daemon's ACL must be refused HERE, before any UI
/// starts, with a `Rejected` classification (not an unreachable/network
/// error). The daemon's IK responder aborts before message 2 when the ACL
/// misses, so the client's handshake fails mid-read; the XX probe cannot
/// detect this (it completes client-side before the daemon's check), which
/// is why the preflight speaks IK.
#[test]
#[ignore]
fn verify_daemon_authorization_rejects_unenrolled_client() {
    let roots = TestRoots::install();
    let _ = &roots;
    let server = ServerKeys::generate();

    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("local addr").to_string();

    // The daemon's ACL closure rejects EVERYONE — this client is not
    // enrolled.
    let (result_tx, result_rx) = mpsc::channel();
    thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept");
        let mut preamble = [0u8; 1];
        stream.read_exact(&mut preamble).expect("read preamble");
        assert_eq!(preamble[0], choreo_transport::handshake::PREAMBLE_IK);
        let result =
            choreo_transport::handshake::handshake_responder(stream, &server.sk, |_| false)
                .map(|_| ());
        let _ = result_tx.send(result);
    });

    let outcome = choreo_client_core::verify_daemon_authorization(&addr, &server.pk);
    match &outcome {
        Err(choreo_client_core::PreflightError::Rejected(_)) => {}
        other => panic!("an un-enrolled client must be classified Rejected, got {other:?}"),
    }

    // The daemon-side handshake reports the ACL rejection.
    let server_result = result_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("responder result");
    assert!(
        server_result.is_err(),
        "the responder must have rejected the un-enrolled client"
    );
}

/// `run_daemon_connection_with_mode(TcpPinned)` against a pinned key: full
/// encrypted Ping → Pong round trip over the pinned-IK connection, with the
/// responder replicating the daemon's preamble + IK accept path.
#[test]
#[ignore]
fn pinned_mode_connects_and_round_trips() {
    let roots = TestRoots::install();
    let server = ServerKeys::generate();

    // Pin the server's REAL key for this address (main thread — the
    // keystore override is installed here).
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("local addr").to_string();
    {
        let mut known = choreo_client_core::KnownServers::load().expect("load store");
        known.pin(&addr, &server.pk).expect("pin");
    }

    // Responder half: preamble + IK responder with the same ACL shape the
    // daemon uses (authorize this test's client key), serving one round trip.
    let client_pk = roots.client_pk;
    thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept");
        let mut preamble = [0u8; 1];
        stream.read_exact(&mut preamble).expect("read preamble");
        assert_eq!(preamble[0], choreo_transport::handshake::PREAMBLE_IK);
        let mut server_stream =
            choreo_transport::handshake::handshake_responder(stream, &server.sk, |pk| {
                pk == &client_pk
            })
            .expect("IK responder");
        let msg = server_stream.recv_client_message().expect("recv Ping");
        assert_eq!(msg, ClientMessage::Ping);
        server_stream
            .send_daemon_message(&DaemonMessage::Pong)
            .expect("send Pong");
    });

    let (from_ui, to_daemon) = mpsc::channel::<ClientMessage>();
    let (tx, rx) = mpsc::channel::<DaemonMessage>();
    let handle = spawn_connection_thread(
        &roots,
        ConnectionMode::TcpPinned(addr),
        move |m| {
            let _ = tx.send(m);
        },
        to_daemon,
    );

    from_ui.send(ClientMessage::Ping).expect("send Ping");
    assert_eq!(
        rx.recv_timeout(Duration::from_secs(5)).expect("Pong"),
        DaemonMessage::Pong,
        "pinned-mode connection must carry encrypted traffic"
    );

    drop(from_ui);
    handle
        .join()
        .expect("connection thread")
        .expect("clean close");
}

/// THE known_hosts pin: connecting in pinned mode when the server's key has
/// CHANGED must fail, and the error must carry the pinned fingerprint and
/// the explicit re-pair guidance — not a bare ConnectionRefused.
#[test]
#[ignore]
fn pinned_mode_key_change_fails_loud() {
    let roots = TestRoots::install();

    // The pin names key A; the responder presents key B.
    let key_a = ServerKeys::generate();
    let key_b = ServerKeys::generate();

    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("local addr").to_string();
    {
        let mut known = choreo_client_core::KnownServers::load().expect("load store");
        known.pin(&addr, &key_a.pk).expect("pin");
    }

    let client_pk = roots.client_pk;
    thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept");
        let mut preamble = [0u8; 1];
        stream.read_exact(&mut preamble).expect("read preamble");
        // The client still speaks IK (it has a pin) — the changed server
        // simply cannot complete the handshake and closes the socket.
        assert_eq!(preamble[0], choreo_transport::handshake::PREAMBLE_IK);
        let _ = choreo_transport::handshake::handshake_responder(stream, &key_b.sk, |pk| {
            pk == &client_pk
        });
    });

    let (from_ui, to_daemon) = mpsc::channel::<ClientMessage>();
    let (tx, _rx) = mpsc::channel::<DaemonMessage>();
    let result = spawn_connection_thread(
        &roots,
        ConnectionMode::TcpPinned(addr.clone()),
        move |m| {
            let _ = tx.send(m);
        },
        to_daemon,
    )
    .join()
    .expect("connection thread");

    let err = result.expect_err("pinned handshake against a changed key must fail");
    let rendered = err.to_string();
    assert!(
        rendered.contains("pinned fingerprint"),
        "the error must surface the pinned fingerprint for comparison, got: {rendered}"
    );
    assert!(
        rendered.contains(&choreo_client_core::fingerprint(&key_a.pk)),
        "the error must contain the PINNED key's fingerprint, got: {rendered}"
    );
    assert!(
        rendered.contains("known_servers.toml"),
        "the error must include the re-pair guidance, got: {rendered}"
    );
    drop(from_ui);
}

/// The store must gate pinned mode: `TcpPinned` with NO pin errors with
/// guidance instead of dialing (the caller skipped first contact).
#[test]
#[ignore]
fn pinned_mode_without_pin_errors() {
    let roots = TestRoots::install();

    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("local addr").to_string();
    // No pin written — deliberately. (The bound listener only proves the
    // client COULD have dialed: the error must come from the store check.)

    let (from_ui, to_daemon) = mpsc::channel::<ClientMessage>();
    let (tx, _rx) = mpsc::channel::<DaemonMessage>();
    let result = spawn_connection_thread(
        &roots,
        ConnectionMode::TcpPinned(addr.clone()),
        move |m| {
            let _ = tx.send(m);
        },
        to_daemon,
    )
    .join()
    .expect("connection thread");

    let err = result.expect_err("pinned mode without a pin must error before dialing");
    let rendered = err.to_string();
    assert!(
        rendered.contains("no pinned server key"),
        "error must explain the missing pin, got: {rendered}"
    );
    assert!(
        rendered.contains("first contact"),
        "error must point at the first-contact flow, got: {rendered}"
    );
    drop(from_ui);
}
