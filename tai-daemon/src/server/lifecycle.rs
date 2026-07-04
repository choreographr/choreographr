use crate::daemon::{DaemonCommand, DaemonState};
use crate::sessions::SessionCommand;
use signal_hook::consts::{SIGINT, SIGTERM};
use signal_hook::flag;
use std::io::{self, BufWriter};
use std::net::Shutdown;
use std::os::unix::net::UnixListener;
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
    listener.set_nonblocking(true)?;
    info!(%socket_path, "tai-daemon listening");

    let (daemon_tx, daemon_rx) = mpsc::channel::<DaemonCommand>();
    state.daemon_tx = daemon_tx.clone();

    let shutdown = Arc::new(AtomicBool::new(false));

    flag::register(SIGINT, Arc::clone(&shutdown))
        .map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;
    flag::register(SIGTERM, Arc::clone(&shutdown))
        .map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;

    let accept_shutdown = Arc::clone(&shutdown);
    let accept_tx = daemon_tx.clone();
    let accept_handle = thread::spawn(move || -> io::Result<()> {
        let mut client_streams: Vec<std::os::unix::net::UnixStream> = Vec::new();
        loop {
            if accept_shutdown.load(Ordering::SeqCst) {
                break;
            }
            match listener.accept() {
                Ok((stream, _)) => {
                    if let Ok(ctrl) = stream.try_clone() {
                        client_streams.push(ctrl);
                    }
                    let tx = accept_tx.clone();
                    thread::spawn(move || {
                        if let Err(e) = crate::server::connection::client_thread(stream, tx) {
                            error!(error = %e, "client error");
                        }
                    });
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(50));
                    continue;
                }
                Err(e) => {
                    if accept_shutdown.load(Ordering::SeqCst) {
                        break;
                    }
                    error!(error = %e, "accept error");
                    return Err(e);
                }
            }
        }
        for stream in client_streams.iter() {
            if let Ok(writer) = stream.try_clone() {
                let mut writer = BufWriter::new(writer);
                let _ = tai_proto::write_message_sync(&mut writer, &DaemonMessage::ShuttingDown);
            }
            let _ = stream.shutdown(Shutdown::Both);
        }
        Ok(())
    });

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

    while !shutdown.load(Ordering::SeqCst) {
        thread::sleep(Duration::from_millis(200));
    }
    info!("received shutdown signal, shutting down");

    if let Err(e) = accept_handle.join() {
        error!("accept thread panicked: {e:?}");
    }

    drop(daemon_tx);
    cmd_handle.join().unwrap_or_else(|e| {
        error!("command thread panicked: {e:?}");
    });

    if Path::new(socket_path).exists() {
        std::fs::remove_file(socket_path)?;
    }
    Ok(())
}
