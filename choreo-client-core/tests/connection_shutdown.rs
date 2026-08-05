use std::fs;
use std::os::unix::net::UnixListener;
use std::sync::mpsc;
use std::thread;

use choreo_client_core::run_daemon_connection;

#[ignore]
#[test]
fn local_shutdown_unblocks_daemon_connection_without_eof() {
    // Keep the socket name short: macOS caps Unix socket paths at ~104 bytes,
    // and the temp dir itself is already long (/var/folders/.../T/). A
    // long unique suffix would overflow that limit and make bind() fail with
    // EINVAL. pid + a short prefix is unique enough within a test run.
    let socket_path = std::env::temp_dir().join(format!("ccs-{}.sock", std::process::id()));
    let _ = fs::remove_file(&socket_path);

    let listener = UnixListener::bind(&socket_path).expect("bind listener");
    let (client_tx, client_rx) = mpsc::channel();
    let (shutdown_tx, shutdown_rx) = mpsc::channel();
    let (done_tx, done_rx) = mpsc::channel();
    let socket_path_string = socket_path.to_string_lossy().to_string();

    let handle = thread::spawn(move || {
        let result =
            run_daemon_connection(&socket_path_string, |_| {}, client_rx, Some(shutdown_rx));
        let _ = done_tx.send(result.is_ok());
    });

    let _server_stream = listener.accept().expect("accept client");
    drop(client_tx);
    shutdown_tx.send(()).expect("signal shutdown");

    assert!(done_rx.recv().expect("connection result"));
    handle.join().expect("join client thread");
    let _ = fs::remove_file(&socket_path);
}
