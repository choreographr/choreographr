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
use choreo_daemon::run_server;
use choreo_proto::{ClientMessage, DaemonMessage, read_message, write_message};
use std::io::{BufReader, BufWriter, Write};
use std::os::unix::net::UnixStream;
use std::thread;
use std::time::Duration;

use crate::common;

/// `run_server` must refuse to start over a LIVE daemon's socket: the probe
/// in `remove_stale_socket` must detect the listening peer via a successful
/// connect and return an error naming the path, WITHOUT unlinking the socket
/// (a live daemon keeps working). A regular file at the path, by contrast, is
/// stale and is removed.
#[test]
#[ignore = "integration"]
fn remove_stale_socket_refuses_live_listener() {
    let dir = tempfile::tempdir().expect("tempdir for socket");
    let path = dir.path().join("live.sock");
    let listener = std::os::unix::net::UnixListener::bind(&path).expect("bind listener");

    // The helper is pub(crate), so go through the same decision via a real
    // `run_server` start: it must fail before binding (the bind below in the
    // second server would otherwise just overwrite the path).
    let state = common::test_daemon_state();
    let transport_sk = choreo_transport::key::TransportSecretKey::new([0u8; 32]);
    let acl = choreo_daemon::server::acl::SharedAcl::load(std::path::Path::new("/nonexistent"));
    let socket_str = path.to_str().expect("valid socket path").to_string();

    let err = run_server(&socket_str, state, None, None, transport_sk, &acl, false)
        .expect_err("run_server over a live socket must fail");
    let msg = err.to_string();
    assert!(
        msg.contains("already listening") && msg.contains("live.sock"),
        "error must name the path and the conflict: {msg}"
    );

    // The live daemon's socket file survived the refused start and the
    // listener is still accepting.
    assert!(path.exists(), "a live daemon's socket must not be removed");
    assert!(std::os::unix::net::UnixStream::connect(&path).is_ok());
    drop(listener);
}

#[test]
#[ignore = "integration"]
fn server_accepts_ping_and_shuts_down_on_signal() {
    let dir = tempfile::tempdir().expect("tempdir for socket");
    let socket_path = dir.path().join("test.sock");
    let socket_str = socket_path.to_str().expect("valid socket path").to_string();

    let state = common::test_daemon_state();

    // Dummy transport key and empty ACL (no TCP listener needed for this test).
    let transport_sk = choreo_transport::key::TransportSecretKey::new([0u8; 32]);
    let acl = choreo_daemon::server::acl::SharedAcl::load(std::path::Path::new("/nonexistent"));

    // Run the server in a background thread.
    let handle = thread::spawn(move || {
        run_server(&socket_str, state, None, None, transport_sk, &acl, false)
    });

    // Wait for the socket to appear (server is ready).
    while !socket_path.exists() {
        thread::sleep(Duration::from_millis(10));
    }
    // Give the accept loop a moment to start blocking.
    thread::sleep(Duration::from_millis(50));

    // Connect a client and verify the server responds to Ping.
    let client = UnixStream::connect(&socket_path).expect("connect");
    let mut reader = BufReader::new(client.try_clone().expect("clone for reader"));
    let mut writer = BufWriter::new(client);

    write_message(&mut writer, &ClientMessage::Ping).expect("write Ping");
    writer.flush().expect("flush Ping");

    let response: DaemonMessage = read_message(&mut reader).expect("read response");
    assert_eq!(response, DaemonMessage::Pong);

    // Trigger graceful shutdown by sending SIGINT.
    // The signal handler thread sets the shutdown flag and self-connects
    // to the socket, which unblocks the accept loop.
    // rustix has no dedicated raise(); killing our own pid is the same
    // syscall sequence as raise(2) and delivers SIGINT to this process.
    let _ = rustix::process::kill_process(rustix::process::getpid(), rustix::process::Signal::INT);

    // The server thread should exit cleanly within a reasonable timeout, so
    // reclaim its `run_server` result and assert it did not error out (the
    // same pattern as common/mod.rs and ctrlc_after_connection.rs).
    let result = handle.join().expect("server thread panicked");
    if let Err(e) = result {
        panic!("run_server exited with an error during shutdown: {e}");
    }
}
