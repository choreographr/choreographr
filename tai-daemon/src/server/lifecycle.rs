use crate::daemon::{DaemonCommand, DaemonState};
use crate::sessions::SessionCommand;
use signal_hook::consts::{SIGINT, SIGTERM};
use std::io::{self, BufWriter};
use std::net::Shutdown;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::Arc;
use std::thread;
use std::time::Duration;
use tai_proto::DaemonMessage;
use tracing::{error, info};

pub fn run_server(socket_path: &str, mut state: DaemonState) -> io::Result<()> {
    if Path::new(socket_path).exists() {
        std::fs::remove_file(socket_path)?;
    }
    let listener = UnixListener::bind(socket_path)?;
    info!(%socket_path, "tai-daemon listening");

    let (daemon_tx, daemon_rx) = mpsc::channel::<DaemonCommand>();
    state.daemon_tx = daemon_tx.clone();

    let shutdown = Arc::new(AtomicBool::new(false));

    // Signal handler thread: sets the shutdown flag and connects to our own
    // socket to unblock the blocking accept() call on the main thread.
    let sig_shutdown = Arc::clone(&shutdown);
    let sig_path = socket_path.to_string();
    thread::spawn(move || {
        let mut signals = match signal_hook::iterator::Signals::new(&[SIGINT, SIGTERM]) {
            Ok(s) => s,
            Err(e) => {
                error!("failed to register signal handlers: {e}");
                return;
            }
        };
        for _ in signals.forever() {
            sig_shutdown.store(true, Ordering::SeqCst);
            // Wake the accept loop by connecting to our own socket.
            // The pending connection causes the next blocking accept()
            // to return immediately so the shutdown flag is checked.
            if let Ok(stream) = UnixStream::connect(&sig_path) {
                drop(stream);
            }
        }
    });

    // Daemon command handler thread.
    let cmd_handle = thread::spawn(move || {
        loop {
            match daemon_rx.recv() {
                Ok(DaemonCommand::Shutdown) => break,
                Ok(cmd) => state.handle_command(cmd),
                Err(mpsc::RecvError) => break,
            }
        }
        let active_sessions = std::mem::take(&mut state.active_sessions);
        for entry in active_sessions.values() {
            let _ = entry.cmd_tx.send(SessionCommand::Shutdown);
        }
        for (_, entry) in active_sessions {
            let _ = entry.handle.join();
        }
    });

    // Main thread accept loop — blocking accept() is event-driven
    // (the kernel deschedules us until a connection arrives).
    let mut client_streams: Vec<UnixStream> = Vec::new();
    loop {
        if shutdown.load(Ordering::SeqCst) {
            break;
        }
        match listener.accept() {
            Ok((stream, _)) => {
                if shutdown.load(Ordering::SeqCst) {
                    // Wakeup from the signal handler — shut down.
                    break;
                }
                if let Ok(ctrl) = stream.try_clone() {
                    client_streams.push(ctrl);
                }
                let tx = daemon_tx.clone();
                thread::spawn(move || {
                    if let Err(e) = crate::server::connection::client_thread(stream, tx) {
                        error!(error = %e, "client error");
                    }
                });
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {
                // Blocking accept was interrupted by a signal
                // (signal_hook does not use SA_RESTART).  Retry.
                continue;
            }
            Err(e) => {
                if shutdown.load(Ordering::SeqCst) {
                    break;
                }
                // Transient or resource-exhaustion errors
                // (ECONNABORTED, EMFILE, ENFILE, etc.) — log
                // and retry with backoff.
                error!(error = %e, "accept error, retrying");
                thread::sleep(Duration::from_millis(100));
                continue;
            }
        }
    }

    info!("shutting down");

    // Notify connected clients.
    for stream in client_streams.iter() {
        if let Ok(writer) = stream.try_clone() {
            let mut writer = BufWriter::new(writer);
            let _ = tai_proto::write_message_sync(&mut writer, &DaemonMessage::ShuttingDown);
        }
        let _ = stream.shutdown(Shutdown::Both);
    }

    // Signal the daemon command handler to stop and wait for it.
    let _ = daemon_tx.send(DaemonCommand::Shutdown);
    drop(daemon_tx);
    cmd_handle.join().unwrap_or_else(|e| {
        error!("command thread panicked: {e:?}");
    });

    if Path::new(socket_path).exists() {
        std::fs::remove_file(socket_path)?;
    }
    Ok(())
}
