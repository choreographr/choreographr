use std::fs;
use std::os::unix::net::UnixListener;
use std::sync::mpsc;
use std::thread;
use std::time::{SystemTime, UNIX_EPOCH};

use choreo_client_core::run_daemon_connection;

#[ignore]
#[test]
fn local_shutdown_unblocks_daemon_connection_without_eof() {
    let socket_path = std::env::temp_dir().join(format!(
        "choreo-client-core-shutdown-{}-{}.sock",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock before unix epoch")
            .as_nanos()
    ));
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
