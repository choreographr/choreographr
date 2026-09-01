//! Integration test: the daemon hot-reloads `authorized_clients.toml`.
//!
//! Starts the REAL daemon with one authorized client, connects it (IK over
//! TCP), then APPENDS a second client's key to the ACL file on disk. The
//! config watcher → `AclReload` → `SharedAcl::reload` chain must make the
//! second client's handshake succeed WITHOUT a daemon restart — the whole
//! point of the hot-reload (and the prerequisite for phase 5's `/acl add`
//! and phase 6's `choreographr acl-add`, which rely on the same chain).
//!
//! `#[ignore]` per test discipline (real sockets, spawned daemon). Waiting
//! for the watcher is bounded POLLING: reconnect attempts in a deadline
//! loop — never a fixed sleep — so the test is as deterministic as the
//! filesystem event delivery allows and fails loudly on timeout.

mod common;

use choreo_client_core::error::ClientError;
use choreo_client_core::run_daemon_tcp_connection;
use choreo_transport::key::{ensure_transport_keypair, set_test_config_root};
use std::path::PathBuf;
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

const TIMEOUT: Duration = Duration::from_secs(5);
/// Overall deadline for the watcher to observe the ACL edit and the command
/// loop to apply it. Generous: a healthy system lands in well under a
/// second, but notify latency on loaded CI boxes can spike.
const RELOAD_DEADLINE: Duration = Duration::from_secs(10);

/// Same client harness as `daemon_client_noise.rs` (`NoiseClient`), reduced
/// to what this test needs. The keypair override must be re-installed inside
/// the connection thread (thread-local) — see the doc comment there.
struct Client {
    from_ui: mpsc::Sender<choreo_proto::ClientMessage>,
    rx: mpsc::Receiver<choreo_proto::DaemonMessage>,
    result_rx: mpsc::Receiver<Result<(), ClientError>>,
}

impl Client {
    fn connect(addr: &str, server_pk: &[u8; 32], key_dir: PathBuf) -> Self {
        let (from_ui, to_daemon) = mpsc::channel();
        let (tx, rx) = mpsc::channel();
        let (result_tx, result_rx) = mpsc::channel();
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
                None,
            );
            set_test_config_root(None);
            let _ = result_tx.send(result);
        });
        Client {
            from_ui,
            rx,
            result_rx,
        }
    }

    /// Whether the handshake succeeded and the encrypted channel answers a
    /// Ping. A rejected client (ACL miss) surfaces as a failed connection
    /// result; a live one gets a Pong.
    fn is_alive(&mut self) -> bool {
        use choreo_proto::{ClientMessage, DaemonMessage};
        self.from_ui.send(ClientMessage::Ping).is_ok()
            && matches!(self.rx.recv_timeout(TIMEOUT), Ok(DaemonMessage::Pong))
            && self.result_rx.try_recv().map(|r| r.is_ok()).unwrap_or(true)
    }
}

/// Generate a client keypair into a fresh temp config root; return
/// `(key_dir, client_pk)`.
fn test_keypair() -> (tempfile::TempDir, [u8; 32]) {
    let key_dir = tempfile::tempdir().unwrap();
    set_test_config_root(Some(key_dir.path().to_path_buf()));
    let (_sk, client_pk) = ensure_transport_keypair().unwrap();
    set_test_config_root(None);
    (key_dir, client_pk)
}

#[test]
#[ignore]
fn acl_edit_authorizes_new_client_without_restart() {
    let (key_dir_a, client_pk_a) = test_keypair();
    let (key_dir_b, client_pk_b) = test_keypair();
    let mut daemon = common::SpawnedDaemon::start(&[client_pk_a]);

    // Sanity: the second client is rejected BEFORE the edit (the ACL gates).
    {
        let mut rejected = Client::connect(
            &daemon.tcp_addr.to_string(),
            &daemon.server_pk,
            key_dir_b.path().to_path_buf(),
        );
        assert!(
            !rejected.is_alive(),
            "a client absent from the ACL must be rejected before the edit"
        );
    }

    // THE hot-reload trigger: append client B's key to the ACL file the
    // running daemon loaded. File format per server/acl.rs ([[client]] with
    // base64 pubkey) — the same the daemon wrote at startup.
    use base64::Engine as _;
    let existing = std::fs::read_to_string(&daemon.acl_path).expect("read ACL");
    let addition = format!(
        "[[client]]\npubkey = \"{}\"\n",
        base64::engine::general_purpose::STANDARD.encode(client_pk_b)
    );
    std::fs::write(&daemon.acl_path, format!("{existing}{addition}")).expect("append to ACL");

    // Bounded retry loop: each attempt either connects (the reload landed)
    // or fails fast (handshake rejected); the loop re-attempts until the
    // deadline. No fixed sleep — the connect round trip itself is the pace.
    let deadline = Instant::now() + RELOAD_DEADLINE;
    let mut connected = None;
    while Instant::now() < deadline {
        let mut client = Client::connect(
            &daemon.tcp_addr.to_string(),
            &daemon.server_pk,
            key_dir_b.path().to_path_buf(),
        );
        if client.is_alive() {
            connected = Some(client);
            break;
        }
        // Give the previous attempt's thread a moment to observe its own
        // rejection before recycling the keypair dir handle. The retry
        // cadence is the connect timeout itself, not a sleep.
        thread::yield_now();
    }

    let client_b = connected.expect("the ACL edit must authorize the new client without a restart");

    // Prove the channel is live, then clean shutdown.
    client_b
        .from_ui
        .send(choreo_proto::ClientMessage::Ping)
        .expect("send over hot-authorized connection");
    assert_eq!(
        client_b.rx.recv_timeout(TIMEOUT).expect("Pong"),
        choreo_proto::DaemonMessage::Pong
    );
    drop(client_b);

    // The FIRST client must still work — a reload must never disturb
    // already-authorized clients.
    let mut client_a = Client::connect(
        &daemon.tcp_addr.to_string(),
        &daemon.server_pk,
        key_dir_a.path().to_path_buf(),
    );
    assert!(
        client_a.is_alive(),
        "pre-existing authorized client must survive the reload"
    );

    daemon.shutdown();
    let _ = client_a.result_rx.recv_timeout(TIMEOUT);
}
