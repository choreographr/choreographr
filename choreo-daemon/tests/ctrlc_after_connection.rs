//! Reproduction tests for the "Ctrl+C stops working after client
//! connections" bug report.
//!
//! Scenario from the bug report: a freshly started daemon exits cleanly on
//! SIGINT, but after one or more clients have connected and disconnected the
//! same SIGINT no longer terminates the process (the operator has to force
//! kill it).
//!
//! These tests run the REAL `run_server` and deliver a real SIGINT to the
//! test process (the same delivery path as pressing Ctrl+C in a terminal,
//! since the test process IS the daemon's process — the signal_hook handler
//! is registered in-process). They then POLL the server thread's exit with a
//! hard deadline instead of joining it, so a wedged shutdown fails the test
//! loudly instead of hanging the suite forever.
//!
//! Belongs to the `#[ignore]` integration suite (real sockets, real signal
//! delivery, real threads).

use choreo_client_core::run_daemon_connection;
use choreo_proto::{ClientMessage, DaemonMessage, SessionEvent};
use std::io::Write;
use std::os::unix::net::UnixStream;
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

mod common;

/// How long to wait for the daemon's server thread to exit after SIGINT.
/// A healthy shutdown takes well under a second (empty daemon); the drain
/// graces sum to a few seconds at most. Anything past this is the wedge.
const SHUTDOWN_DEADLINE: Duration = Duration::from_secs(20);

/// Deliver SIGINT to our own process (the daemon's signal_hook handler is
/// registered in-process) and poll — never join — the server thread until it
/// exits or the deadline passes. Returns `true` when it exited in time.
fn sigint_and_wait(daemon: &mut common::SpawnedDaemon) -> bool {
    // Take the handle out so `SpawnedDaemon::drop` doesn't ALSO signal/join
    // (and hang the test teardown) if the shutdown wedged.
    let handle = daemon.take_handle();
    let Some(handle) = handle else {
        return true; // already exited
    };
    // The same syscall sequence as raise(2)/Ctrl+C in the foreground group:
    // the daemon's in-process signal_hook iterator is registered for SIGINT.
    let _ = rustix::process::kill_process(rustix::process::getpid(), rustix::process::Signal::INT);
    let deadline = Instant::now() + SHUTDOWN_DEADLINE;
    while !handle.is_finished() {
        if Instant::now() >= deadline {
            // Detach: the test process exits when the test binary finishes,
            // so a wedged server thread doesn't block the runner.
            eprintln!("DAEMON DID NOT EXIT within {SHUTDOWN_DEADLINE:?} after SIGINT");
            return false;
        }
        thread::sleep(Duration::from_millis(50));
    }
    let result = handle.join().expect("server thread panicked");
    if let Err(e) = result {
        panic!("run_server exited with an error during shutdown: {e}");
    }
    true
}

/// CONTROL: a fresh daemon with zero connections must exit promptly on
/// SIGINT (the bug report confirms this works in the field).
#[test]
#[ignore]
fn sigint_exits_fresh_daemon_with_no_connections() {
    let mut daemon = common::SpawnedDaemon::start(&[]);
    assert!(
        sigint_and_wait(&mut daemon),
        "fresh daemon must exit on SIGINT"
    );
}

/// A client that connects, exchanges a Ping/Pong, and disconnects cleanly —
/// the minimal "opened and closed a connection" from the bug report.
#[test]
#[ignore]
fn sigint_exits_after_ping_pong_connect_and_disconnect() {
    let mut daemon = common::SpawnedDaemon::start(&[]);
    {
        let (tx, rx) = mpsc::channel::<DaemonMessage>();
        let (from_ui, to_daemon) = mpsc::channel::<ClientMessage>();
        let (shutdown_tx, shutdown_rx) = mpsc::channel::<()>();
        let socket = daemon.socket_str();
        let handle = thread::spawn(move || {
            run_daemon_connection(
                &socket,
                move |m| {
                    let _ = tx.send(m);
                },
                to_daemon,
                Some(shutdown_rx),
            )
        });
        from_ui.send(ClientMessage::Ping).expect("send ping");
        assert_eq!(
            rx.recv_timeout(Duration::from_secs(5)).expect("pong"),
            DaemonMessage::Pong
        );
        // Clean disconnect: stop the writer, sever the socket, join.
        drop(from_ui);
        let _ = shutdown_tx.send(());
        handle.join().expect("client thread").expect("clean close");
    }

    // Give the daemon a moment to run its disconnect cleanup (detach,
    // ClientDisconnected, writer drain) before the signal — the bug is
    // about the state left behind AFTER a completed disconnect.
    thread::sleep(Duration::from_millis(500));

    assert!(
        sigint_and_wait(&mut daemon),
        "daemon must exit on SIGINT after a completed client connection"
    );
}

/// A client that connects, CREATES a session (the default tool-group set),
/// and disconnects — leaving a live session thread behind, as the phone/local
/// TUI flow does. The session thread persists after the client is gone.
#[test]
#[ignore]
fn sigint_exits_after_create_session_connect_and_disconnect() {
    let mut daemon = common::SpawnedDaemon::start(&[]);
    {
        let (tx, rx) = mpsc::channel::<DaemonMessage>();
        let (from_ui, to_daemon) = mpsc::channel::<ClientMessage>();
        let (shutdown_tx, shutdown_rx) = mpsc::channel::<()>();
        let socket = daemon.socket_str();
        let handle = thread::spawn(move || {
            run_daemon_connection(
                &socket,
                move |m| {
                    let _ = tx.send(m);
                },
                to_daemon,
                Some(shutdown_rx),
            )
        });
        from_ui
            .send(ClientMessage::CreateSession {
                title: Some("repro".into()),
                parent_session_id: None,
                working_dir: None,
                context_config: None,
                account_name: None,
                selected_model: None,
                reasoning_effort: None,
            })
            .expect("send create session");
        // Wait for the SessionCreated broadcast (it follows the direct
        // reply), so the session thread is up before the client goes away.
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut created = false;
        while Instant::now() < deadline {
            match rx.recv_timeout(Duration::from_millis(100)) {
                Ok(DaemonMessage::Session {
                    event: SessionEvent::SessionCreated { .. },
                    ..
                }) => {
                    created = true;
                    break;
                }
                Ok(_) => continue,
                Err(_) => break,
            }
        }
        assert!(created, "session was never created");
        // Clean disconnect: stop the writer, sever the socket, join.
        drop(from_ui);
        let _ = shutdown_tx.send(());
        handle.join().expect("client thread").expect("clean close");
    }

    // Give the daemon's disconnect cleanup time to run, then signal.
    thread::sleep(Duration::from_millis(500));

    assert!(
        sigint_and_wait(&mut daemon),
        "daemon must exit on SIGINT after a session was created and the client left"
    );
}

/// A raw connect + close with NO protocol traffic at all: the accept path
/// runs, the connection thread starts and immediately sees EOF.
#[test]
#[ignore]
fn sigint_exits_after_bare_connect_and_close() {
    let mut daemon = common::SpawnedDaemon::start(&[]);
    {
        let stream = UnixStream::connect(&daemon.socket_path).expect("connect");
        // Close immediately — just the accept + EOF, no messages.
        drop(stream);
    }
    thread::sleep(Duration::from_millis(500));
    assert!(
        sigint_and_wait(&mut daemon),
        "daemon must exit on SIGINT after a bare connect/close"
    );
}

/// A connect that never sends a valid preamble (the TCP-path failure mode,
/// driven over the Unix socket here as the cheapest equivalent: connect and
/// write one garbage byte, then close).
#[test]
#[ignore]
fn sigint_exits_after_garbage_byte_connect_and_close() {
    let mut daemon = common::SpawnedDaemon::start(&[]);
    {
        let mut stream = UnixStream::connect(&daemon.socket_path).expect("connect");
        stream.write_all(&[0xFF]).expect("write garbage");
        drop(stream);
    }
    thread::sleep(Duration::from_millis(500));
    assert!(
        sigint_and_wait(&mut daemon),
        "daemon must exit on SIGINT after a garbage-byte connect/close"
    );
}
