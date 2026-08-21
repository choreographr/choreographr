//! End-to-end daemon ↔ client integration tests over the local Unix socket.
//!
//! Each test runs the REAL daemon (`common::SpawnedDaemon` →
//! `choreo_daemon::run_server`) and drives it with the REAL client
//! connection library (`choreo_client_core::run_daemon_connection`), so the
//! full wire path is exercised: client writer thread → Unix socket → daemon
//! `client_thread` → daemon command loop → session thread → per-client
//! writer channel → socket → client reader loop → handler channel.
//!
//! These tests belong to the `#[ignore]` integration suite (run via
//! `cargo test-integration` / nextest `--run-ignored only`): they bind real
//! sockets and spawn real threads, so they are excluded from the fast unit
//! suite. The only time-based primitive used is the bounded `recv_timeout`
//! on the message channel, so a wedged daemon fails loudly instead of
//! hanging the suite.

use choreo_client_core::error::ClientError;
use choreo_client_core::run_daemon_connection;
use choreo_proto::{ClientMessage, DaemonMessage, SessionEvent};
use std::io::{self, Read};
use std::os::unix::net::UnixStream;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

mod common;

/// Bounded receive timeout for daemon replies: short enough that a wedged
/// daemon fails the test loudly, long enough that a loaded CI box doesn't
/// flake.
const TIMEOUT: Duration = Duration::from_secs(5);

/// A connected client driven through the real `run_daemon_connection`.
///
/// The connection lives on its own thread: `run_daemon_connection` spawns a
/// writer thread that drains `from_ui` onto the socket, while the calling
/// thread blocks in the reader loop and forwards every decoded
/// `DaemonMessage` into `rx`.
struct Client {
    from_ui: mpsc::Sender<ClientMessage>,
    rx: mpsc::Receiver<DaemonMessage>,
    /// Sender half of `run_daemon_connection`'s optional shutdown channel.
    /// Sending on it makes the connection's shutdown thread call
    /// `shutdown(Shutdown::Both)` on the socket — the only way to sever the
    /// connection from the client side (see [`Client::disconnect`]).
    shutdown_tx: mpsc::Sender<()>,
    handle: thread::JoinHandle<Result<(), ClientError>>,
}

impl Client {
    fn connect(socket: &str) -> Self {
        let (from_ui, to_daemon) = mpsc::channel::<ClientMessage>();
        // The shutdown channel is wired for every client even though most
        // tests never use it: `disconnect()` needs it, and an unused one is
        // inert (its thread just blocks on `recv` until the test process
        // exits).
        let (shutdown_tx, shutdown_rx) = mpsc::channel::<()>();
        let (tx, rx) = mpsc::channel::<DaemonMessage>();
        let socket = socket.to_string();
        let handle = thread::spawn(move || {
            run_daemon_connection(
                &socket,
                |m| {
                    let _ = tx.send(m);
                },
                to_daemon,
                Some(shutdown_rx),
            )
        });
        Client {
            from_ui,
            rx,
            shutdown_tx,
            handle,
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

    fn assert_closed_ok(self) {
        self.handle
            .join()
            .expect("client thread panicked")
            .expect("clean close");
    }

    /// Sever the connection from the client side and join the connection
    /// thread.
    ///
    /// Dropping `from_ui` alone is NOT enough to disconnect: it only stops
    /// the writer thread, while the reader loop keeps its own clone of the
    /// socket open, so the join would block forever. Triggering the shutdown
    /// channel calls `shutdown(Shutdown::Both)` on the socket, which
    /// produces the EOF both sides observe — the client reader exits cleanly
    /// and the daemon's `client_thread` runs its disconnect cleanup path
    /// (detach, `ClientDisconnected`, writer drain).
    fn disconnect(self) {
        // Destructure up front so the fields can be moved independently
        // (mpsc::Sender is not Copy — `drop(self.from_ui)` would otherwise
        // partially move `self` and block the later `assert_closed_ok`).
        let Self {
            from_ui,
            shutdown_tx,
            handle,
            ..
        } = self;
        drop(from_ui); // writer thread sees Disconnected and stops
        let _ = shutdown_tx.send(()); // sever the socket
        handle
            .join()
            .expect("client thread panicked")
            .expect("clean close");
    }
}

/// The CreateSession request used throughout: every optional field unset, so
/// the tests exercise the default session-creation path.
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

#[test]
#[ignore]
fn unix_ping_pong_round_trip() {
    let mut daemon = common::SpawnedDaemon::start(&[]);
    let client = Client::connect(&daemon.socket_str());

    // One round trip through the real wire path: the client writer thread
    // puts Ping on the socket, the daemon's `client_thread` replies Pong,
    // and the reader loop delivers it to the handler channel.
    client.send(ClientMessage::Ping);
    assert_eq!(client.recv(), DaemonMessage::Pong);

    // Graceful shutdown: SIGINT makes the daemon notify every connected
    // Unix client and close the connection, so the reader must exit cleanly
    // on EOF (Ok(()), not an I/O error).
    daemon.shutdown();
    client.assert_closed_ok();
}

#[test]
#[ignore]
fn unix_list_sessions_round_trip() {
    let mut daemon = common::SpawnedDaemon::start(&[]);
    let client = Client::connect(&daemon.socket_str());

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
            session_id: Some(session_id),
            event: SessionEvent::SessionCreated { .. },
        } => assert_eq!(session_id, 1),
        other => panic!("expected SessionCreated, got {other:?}"),
    }

    // The new session is now visible to ListSessions — exactly one entry,
    // carrying the id the daemon assigned at creation. This proves the
    // create path updated the same daemon-side store the list reads from.
    client.send(ClientMessage::ListSessions);
    match client.recv() {
        DaemonMessage::Sessions { sessions } => {
            assert_eq!(sessions.len(), 1);
            assert_eq!(sessions[0].session_id, 1);
        }
        other => panic!("expected Sessions, got {other:?}"),
    }

    daemon.shutdown();
    client.assert_closed_ok();
}

#[test]
#[ignore]
fn unix_create_session_then_attach() {
    let mut daemon = common::SpawnedDaemon::start(&[]);
    let client = Client::connect(&daemon.socket_str());

    client.send(create_session());
    match client.recv() {
        DaemonMessage::Session {
            session_id: Some(session_id),
            event: SessionEvent::SessionCreated { .. },
        } => assert_eq!(session_id, 1),
        other => panic!("expected SessionCreated, got {other:?}"),
    }

    // Attach to the session we just created. The daemon deliberately sends
    // SessionAttached BEFORE the session thread's SessionState snapshot: the
    // TUI sets its `attached_session_id` on SessionAttached and silently
    // drops SessionState messages for sessions it is not attached to. The
    // ordering is load-bearing, so assert it strictly in order.
    client.send(ClientMessage::AttachSession { session_id: 1 });
    match client.recv() {
        DaemonMessage::Session {
            session_id: Some(session_id),
            event: SessionEvent::SessionAttached,
        } => assert_eq!(session_id, 1),
        other => panic!("expected SessionAttached, got {other:?}"),
    }
    match client.recv() {
        DaemonMessage::Session {
            session_id: Some(session_id),
            event: SessionEvent::SessionState { .. },
        } => assert_eq!(session_id, 1),
        other => panic!("expected SessionState, got {other:?}"),
    }

    daemon.shutdown();
    client.assert_closed_ok();
}

#[test]
#[ignore]
fn unix_shutdown_notifies_client() {
    let mut daemon = common::SpawnedDaemon::start(&[]);
    let client = Client::connect(&daemon.socket_str());

    // Prove the connection is live before shutting the daemon down.
    client.send(ClientMessage::Ping);
    assert_eq!(client.recv(), DaemonMessage::Pong);

    // SIGINT: the server's shutdown path writes ShuttingDown to every
    // connected Unix client, then closes the connection.
    daemon.shutdown();
    assert_eq!(client.recv(), DaemonMessage::ShuttingDown);

    // The reader must exit cleanly on the EOF that follows the notification
    // — a clean close (Ok(())), not an I/O error.
    client.assert_closed_ok();
}

#[test]
#[ignore]
fn unix_two_clients_isolated_and_shared_state() {
    let mut daemon = common::SpawnedDaemon::start(&[]);
    let client_a = Client::connect(&daemon.socket_str());
    let client_b = Client::connect(&daemon.socket_str());

    // Both connections are live and independent: each Ping gets its own
    // Pong on the right connection.
    client_a.send(ClientMessage::Ping);
    assert_eq!(client_a.recv(), DaemonMessage::Pong);
    client_b.send(ClientMessage::Ping);
    assert_eq!(client_b.recv(), DaemonMessage::Pong);

    // A creates a session...
    client_a.send(create_session());
    match client_a.recv() {
        DaemonMessage::Session {
            session_id: Some(session_id),
            event: SessionEvent::SessionCreated { .. },
        } => assert_eq!(session_id, 1),
        other => panic!("expected SessionCreated, got {other:?}"),
    }

    // ...and B sees it: daemon state is shared across connections even
    // though the connections themselves are independent.
    client_b.send(ClientMessage::ListSessions);
    match client_b.recv() {
        DaemonMessage::Sessions { sessions } => {
            assert_eq!(sessions.len(), 1);
            assert_eq!(sessions[0].session_id, 1);
        }
        other => panic!("expected Sessions, got {other:?}"),
    }

    // Disconnect A from the client side (drops the sender, severs the
    // socket, joins the connection thread). This drives the daemon's
    // `client_thread` to observe the reset and run its disconnect cleanup
    // path — the exact code that would otherwise leak state for a client
    // that vanished.
    client_a.disconnect();

    // The daemon keeps serving remaining clients after a disconnect.
    client_b.send(ClientMessage::Ping);
    assert_eq!(client_b.recv(), DaemonMessage::Pong);

    daemon.shutdown();
    client_b.assert_closed_ok();
}

/// The concurrent-connection cap (`MAX_CONCURRENT_CONNECTIONS = 256` in
/// `server/lifecycle.rs`) bounds how many wedged-but-open clients the daemon
/// keeps alive. This pins it end-to-end over the Unix socket: 256 silent
/// clients (socket open, nothing sent) each hold a live-connection slot; the
/// 257th is accepted and immediately dropped, so its reader sees a bare EOF
/// while the first 256 stay connected.
///
/// Direct `UnixStream` usage (rather than the `Client` helper) keeps the
/// test minimal: the point is the accept-side cap, not the client protocol.
#[test]
#[ignore]
fn unix_connection_cap_rejects_over_limit_with_eof() {
    // Must match MAX_CONCURRENT_CONNECTIONS in server/lifecycle.rs (the
    // constant is private to the crate, so the integration test hardcodes it
    // and this comment is the coupling point).
    const CAP: usize = 256;
    let daemon = common::SpawnedDaemon::start(&[]);

    let mut streams = Vec::with_capacity(CAP);
    for _ in 0..CAP {
        let stream = UnixStream::connect(daemon.socket_str()).expect("connect under the cap");
        streams.push(stream);
        // Pace the connects so the daemon's single-threaded accept loop keeps
        // up: its Unix listener backlog is 128 (smaller than the cap), so
        // connecting 256 sockets faster than it accepts would block on a full
        // backlog instead of exercising the cap.
        thread::sleep(Duration::from_millis(2));
    }

    // The cap is 256: the 257th connection is accepted and dropped
    // immediately (the client sees a bare EOF, never a protocol message).
    let mut over_limit = UnixStream::connect(daemon.socket_str()).expect("connect over the cap");
    over_limit
        .set_read_timeout(Some(TIMEOUT))
        .expect("set read timeout");
    let mut buf = [0u8; 1];
    match over_limit.read(&mut buf) {
        Ok(0) => {} // EOF — the expected rejection
        other => panic!("expected EOF from over-cap connection, got {other:?}"),
    }

    // FIFO accept order guarantees the daemon accepted the first 256 before
    // the 257th (each connect was issued strictly after the previous one), so
    // once the 257th saw EOF every slot is held. Verify none of the 256 was
    // dropped: a still-connected silent socket reads WouldBlock (no data),
    // never EOF.
    for (i, mut stream) in streams.iter().enumerate() {
        stream.set_nonblocking(true).expect("set nonblocking");
        let mut buf = [0u8; 1];
        match stream.read(&mut buf) {
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {}
            other => panic!("connection {i} under the cap was dropped: {other:?}"),
        }
    }

    // Dropping the streams before the daemon's Drop runs shutdown lets the
    // daemon's readers see EOF and exit cleanly, so shutdown does not wait
    // out the bounded drain grace for 256 wedged connections.
    drop(streams);
}
