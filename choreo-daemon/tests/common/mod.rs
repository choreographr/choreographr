//! Shared scaffolding for daemon-level integration tests.
//!
//! End-to-end tests run the REAL daemon (`choreo_daemon::run_server`) with
//! both listeners (Unix socket + TCP/Noise) and drive it with the real
//! client library (`choreo_client_core::run_daemon_connection` /
//! `run_daemon_tcp_connection`). Every such test needs the same scaffolding:
//! a temp DB, a minimal [`DaemonState`], a temp socket path, an ACL TOML, a
//! free TCP port, and graceful shutdown via SIGINT. That scaffolding lives
//! here so the per-test files stay focused on what they exercise.
//!
//! Each integration-test binary that declares `mod common;` compiles this
//! module standalone, so an item unused by a particular binary (e.g. `test_db`
//! in the lifecycle test, or `SpawnedDaemon` until the daemon_client_* tests
//! land) would otherwise trip `dead_code`. The harness is intentionally a
//! superset of what any single test file uses.
#![allow(dead_code)]

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use choreo_daemon::accounts::AccountManager;
use choreo_daemon::broadcast::LagLimits;
use choreo_daemon::server::acl::Acl;
use choreo_daemon::{DaemonState, run_server};
use std::collections::HashMap;
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::Arc;
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

/// A throwaway redb database for tests that need one directly.
pub fn test_db() -> redb::Database {
    let dir = tempfile::tempdir().unwrap();
    redb::Database::create(dir.path().join("state.redb")).unwrap()
}

/// Build a minimal [`DaemonState`] suitable for running the real server
/// under test. The `daemon_tx` channel is a dummy — `run_server` overwrites
/// it with the real command channel before serving.
pub fn test_daemon_state() -> DaemonState {
    test_daemon_state_with_limits(LagLimits::default())
}

/// Like [`test_daemon_state`] but with injectable lag thresholds. The
/// default caps (64 MiB / 512 MiB) are far too large for a test to cross with
/// a few KiB of streamed output, so the eviction integration test builds its
/// daemon state through this seam with tiny caps (see
/// `tests/stream_integrity.rs`).
pub fn test_daemon_state_with_limits(limits: LagLimits) -> DaemonState {
    let (daemon_tx, _daemon_rx) = mpsc::channel();

    let dir = tempfile::tempdir().expect("tempdir");
    let db =
        Arc::new(redb::Database::create(dir.path().join("state.redb")).expect("test database"));
    let tool_registry = choreo_daemon::tools::ToolRegistry::new().build();
    let config_dir = tempfile::tempdir().expect("tempdir for config");
    let accounts_path = config_dir.path().join("accounts.toml");

    DaemonState {
        next_session_id: 1,
        max_turns: 10,
        active_sessions: HashMap::new(),
        session_metadata: HashMap::new(),
        deleted_sessions: std::collections::HashSet::new(),
        children: HashMap::new(),
        accounts: AccountManager::load(&accounts_path).unwrap(),
        providers: HashMap::new(),
        credentials: HashMap::new(),
        x_credentials: None,
        db,
        tool_registry,
        daemon_tx,
        summary_subscribers: HashMap::new(),
        client_writers: HashMap::new(),
        activity_subscribers: HashMap::new(),
        client_subscribed_sessions: HashMap::new(),
        global_lag: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        lag_limits: limits,
        model_cache: HashMap::new(),
        mcp_manager: choreo_daemon::mcp::McpManager::empty(),
        // The integration harness runs the real `run_server`, which spawns the
        // catalog-maintenance thread and fills this in; a dummy value here is
        // overwritten before any client connects.
        maintenance_tx: None,
        catalog_paths: choreo_daemon::catalog::CatalogPaths::default(),
    }
}

/// Poll `cond` every 10 ms until it returns `true`, panicking with a
/// descriptive message after ~5 s. Readiness checks must be bounded so a
/// wedged daemon fails the test loudly instead of hanging it forever.
/// Integration tests exercise the system boundary, so short sleeps are
/// acceptable here — but never unbounded ones.
fn wait_until(what: &str, mut cond: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !cond() {
        assert!(
            Instant::now() < deadline,
            "timed out after 5s waiting for {what}"
        );
        thread::sleep(Duration::from_millis(10));
    }
}

/// A running daemon under test with both the Unix-socket and TCP/Noise
/// listeners active. Spawns `run_server` in a background thread and waits
/// until both listeners are ready. Dropping it triggers graceful shutdown.
pub struct SpawnedDaemon {
    pub socket_path: std::path::PathBuf,
    pub tcp_addr: SocketAddr, // 127.0.0.1:<free port>
    pub server_pk: [u8; 32],  // X25519 public key of the server
    handle: Option<thread::JoinHandle<std::io::Result<()>>>,
    _tmp: tempfile::TempDir, // keeps socket + ACL file alive
    /// Objects the daemon depends on for its lifetime (e.g. a
    /// `MockProvider` whose serve thread must outlive the daemon). Kept in
    /// the same struct so dropping the daemon also tears them down, in the
    /// right order (the `Drop` impl shuts the daemon down first).
    _keepalive: Vec<Box<dyn std::any::Any + Send>>,
}

impl SpawnedDaemon {
    /// Start a daemon whose Noise ACL authorizes exactly `authorized_pks`
    /// (an empty slice => empty ACL => every Noise connection rejected).
    ///
    /// Retries up to `START_ATTEMPTS` times: the ephemeral TCP port is chosen
    /// by binding `127.0.0.1:0`, reading the port, and *dropping* the probe so
    /// `run_server` can bind it. Under parallel load another process can grab
    /// that port in the window; `run_server` then fails fast on the bind error
    /// and the daemon dies before serving — which we detect as an early
    /// `handle` exit while waiting for readiness and recover from with a fresh
    /// tempdir/port/socket/state.
    pub fn start(authorized_pks: &[[u8; 32]]) -> Self {
        Self::start_with_state(|| (test_daemon_state(), Vec::new()), authorized_pks)
    }

    /// Like [`Self::start`] but builds each attempt's [`DaemonState`] from
    /// `build`, which may also hand back keep-alive objects (e.g. a
    /// `choreo_ai_protocols::test_utils::MockProvider` whose serve thread
    /// must outlive the daemon — see the `_keepalive` field). The factory is
    /// re-invoked per retry attempt because `run_server` consumes the state.
    pub fn start_with_state(
        build: impl Fn() -> (DaemonState, Vec<Box<dyn std::any::Any + Send>>) + Send + 'static,
        authorized_pks: &[[u8; 32]],
    ) -> Self {
        const START_ATTEMPTS: usize = 5;

        for attempt in 1..=START_ATTEMPTS {
            let (state, keepalive) = build();
            if let Some(daemon) = Self::try_start(state, keepalive, authorized_pks, attempt) {
                return daemon;
            }
        }
        panic!("failed to start test daemon after {START_ATTEMPTS} attempts");
    }

    /// One attempt at starting a daemon, or `None` when `run_server` exits
    /// early while waiting for readiness — the ephemeral-port re-bind race
    /// under parallel load — so [`Self::start_with_state`] can retry with a
    /// fresh tempdir/port/socket/state (and, via `build`, a fresh keepalive).
    fn try_start(
        state: DaemonState,
        keepalive: Vec<Box<dyn std::any::Any + Send>>,
        authorized_pks: &[[u8; 32]],
        attempt: usize,
    ) -> Option<Self> {
        // Fresh Noise keypair for the server. `rand::rng()` is the
        // thread-local CSPRNG; `random_from_rng` fills the 32-byte secret
        // from it. The public half is what tests hand to the client side;
        // the secret half goes into `run_server` via `TransportSecretKey`.
        let server_sk = x25519_dalek::StaticSecret::random_from_rng(&mut rand::rng());
        let server_pk = x25519_dalek::PublicKey::from(&server_sk).to_bytes();
        let transport_sk = choreo_transport::key::TransportSecretKey::new(server_sk.to_bytes());

        // One temp dir holds the socket and the ACL file — keeping them
        // alive for the daemon's whole lifetime (the DB lives in its own
        // tempdir inside `test_daemon_state`).
        let tmp = tempfile::tempdir().expect("tempdir for daemon test");
        let socket_path = tmp.path().join("daemon.sock");
        let socket_str = socket_path
            .to_str()
            .expect("socket path must be valid UTF-8")
            .to_string();

        // Always write the ACL file — even when there are no authorized keys
        // — so `Acl::load` exercises its real TOML parsing path instead of
        // the file-not-found early return.
        let acl_path = tmp.path().join("authorized_clients.toml");
        let mut acl_toml = String::new();
        for pk in authorized_pks {
            acl_toml.push_str(&format!("[[client]]\npubkey = \"{}\"\n", BASE64.encode(pk)));
        }
        std::fs::write(&acl_path, acl_toml).expect("write ACL file");
        let acl = Arc::new(Acl::load(&acl_path));

        // Reserve a free TCP port: bind :0, read the kernel-assigned port,
        // then drop the probe listener so `run_server` can bind it. There
        // is an inherent race where another process grabs the port in
        // between — exactly what the retry loop recovers from, because
        // `run_server` fails fast on the bind error and exits early.
        let tcp_addr: SocketAddr = {
            let probe = TcpListener::bind("127.0.0.1:0").expect("bind ephemeral port");
            let port = probe.local_addr().expect("local_addr").port();
            SocketAddr::from(([127, 0, 0, 1], port))
        };
        let tcp_addr_str = tcp_addr.to_string();

        let mut handle: Option<thread::JoinHandle<std::io::Result<()>>> =
            Some(thread::spawn(move || {
                run_server(
                    &socket_str,
                    state,
                    None,
                    Some(tcp_addr_str),
                    transport_sk,
                    acl,
                )
            }));

        // Readiness, bounded: first the Unix socket file (created by the
        // bind inside `run_server`), then a TCP connect probe. A stuck
        // daemon panics the test with a clear message instead of hanging.
        wait_until("daemon Unix socket", || socket_path.exists());

        // TCP readiness loop that ALSO watches for early server exit: if
        // the socket file appeared but the server thread has already
        // finished, `run_server` died before serving — the port-steal
        // bind failure — so reclaim the JoinHandle result and signal the
        // caller to retry. `handle` is an Option so the finished branch can
        // `take()` it (join consumes) while the success branch still owns it
        // for the daemon struct.
        let deadline = Instant::now() + Duration::from_secs(5);
        let ready = loop {
            if handle.as_ref().is_some_and(|h| h.is_finished()) {
                let result = handle
                    .take()
                    .expect("server thread panicked")
                    .join()
                    .expect("server thread panicked");
                tracing::warn!(
                    attempt,
                    ?result,
                    "daemon exited early while waiting for readiness — retrying start"
                );
                break false;
            }
            if TcpStream::connect(tcp_addr).is_ok() {
                break true;
            }
            assert!(
                Instant::now() < deadline,
                "timed out after 5s waiting for daemon TCP listener"
            );
            thread::sleep(Duration::from_millis(10));
        };
        if ready {
            return Some(SpawnedDaemon {
                socket_path,
                tcp_addr,
                server_pk,
                handle,
                _tmp: tmp,
                _keepalive: keepalive,
            });
        }
        None
    }

    /// Gracefully shut the daemon down (SIGINT) and join the server thread.
    /// Idempotent. If the server thread already exited, just reclaims the
    /// JoinHandle result without sending a signal (sending SIGINT after the
    /// signal_hook handler is unregistered would kill the test process!).
    pub fn shutdown(&mut self) {
        let handle = match self.handle.take() {
            Some(h) => h,
            None => return, // already shut down (or never started)
        };

        if handle.is_finished() {
            // The server thread exited on its own (e.g. the test triggered
            // the shutdown path itself). Reclaim the JoinHandle result but
            // do NOT signal: run_server's signal_hook handler may already be
            // gone, and a stray SIGINT would then kill the test process.
            let result = handle.join().expect("server thread panicked");
            tracing::debug!(?result, "daemon server thread already finished");
            return;
        }

        // Graceful shutdown via SIGINT — the same pattern the lifecycle test
        // uses. rustix has no dedicated raise(); killing our own pid is the
        // same syscall sequence as raise(2) and delivers SIGINT to this
        // process, which run_server's signal handler is still registered for.
        let _ =
            rustix::process::kill_process(rustix::process::getpid(), rustix::process::Signal::INT);
        let result = handle.join().expect("server thread panicked");
        if let Err(e) = result {
            panic!("run_server exited with an error during shutdown: {e}");
        }
    }

    /// Convenience: the socket path as a String for client APIs.
    pub fn socket_str(&self) -> String {
        self.socket_path
            .to_str()
            .expect("socket path must be valid UTF-8")
            .to_string()
    }
}

impl Drop for SpawnedDaemon {
    fn drop(&mut self) {
        // Tear the daemon down even when the test panics mid-way — a leaked
        // background server would keep the socket/port alive and flake the
        // next test.
        self.shutdown();
    }
}
