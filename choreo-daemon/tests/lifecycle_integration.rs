use choreo_proto::{ClientMessage, DaemonMessage, read_message, write_message};
use choreographr::accounts::AccountManager;
use choreographr::{DaemonState, run_server};
use std::collections::HashMap;
use std::io::{BufReader, BufWriter, Write};
use std::os::unix::net::UnixStream;
use std::sync::Arc;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

/// Build a minimal [`DaemonState`] suitable for testing the server lifecycle.
fn test_daemon_state() -> DaemonState {
    let (daemon_tx, _daemon_rx) = mpsc::channel();

    let dir = tempfile::tempdir().expect("tempdir");
    let db =
        Arc::new(redb::Database::create(dir.path().join("state.redb")).expect("test database"));
    let tool_registry = choreographr::tools::ToolRegistry::new().build();
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
        client_streams: Vec::new(),
        summary_subscribers: HashMap::new(),
        activity_subscribers: HashMap::new(),
        client_subscribed_sessions: HashMap::new(),
        model_cache: HashMap::new(),
        mcp_manager: choreographr::mcp::McpManager::empty(),
    }
}

#[test]
#[ignore]
fn server_accepts_ping_and_shuts_down_on_signal() {
    let dir = tempfile::tempdir().expect("tempdir for socket");
    let socket_path = dir.path().join("test.sock");
    let socket_str = socket_path.to_str().expect("valid socket path").to_string();

    let state = test_daemon_state();

    // Dummy transport key and empty ACL (no TCP listener needed for this test).
    let transport_sk = choreo_transport::key::TransportSecretKey::new([0u8; 32]);
    let acl = Arc::new(choreographr::server::acl::Acl::load(std::path::Path::new(
        "/nonexistent",
    )));

    // Run the server in a background thread.
    let handle = thread::spawn(move || {
        run_server(&socket_str, state, None, None, transport_sk, acl).expect("run_server");
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

    // The server thread should exit cleanly within a reasonable timeout.
    handle.join().expect("server thread panicked");
}
