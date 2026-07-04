use crate::daemon::{DaemonCommand, DaemonState};
use crate::sessions::SessionCommand;
use std::io::{self, BufWriter};
use std::net::Shutdown;
use std::path::Path;
use tai_proto::DaemonMessage;
use tokio::net::UnixListener;
use tokio::signal::unix::{SignalKind, signal};
use tokio::sync::mpsc;
use tokio::task;
use tracing::{error, info};

fn wait_for_shutdown() -> impl std::future::Future<Output = ()> {
    async {
        let mut sigint = signal(SignalKind::interrupt()).expect("failed to install SIGINT handler");
        let mut sigterm =
            signal(SignalKind::terminate()).expect("failed to install SIGTERM handler");
        tokio::select! {
            _ = sigint.recv() => info!("received SIGINT, shutting down tai-daemon"),
            _ = sigterm.recv() => info!("received SIGTERM, shutting down tai-daemon"),
        }
    }
}

pub async fn run_server(socket_path: &str, mut state: DaemonState) -> io::Result<()> {
    if Path::new(socket_path).exists() {
        std::fs::remove_file(socket_path)?;
    }
    let listener = UnixListener::bind(socket_path)?;
    info!(%socket_path, "tai-daemon listening");

    let (daemon_tx, mut daemon_rx) = mpsc::unbounded_channel::<DaemonCommand>();
    state.daemon_tx = daemon_tx.clone();

    let result = loop {
        tokio::select! {
            accept_result = listener.accept() => {
                let (stream, _) = accept_result?;
                let stream = stream.into_std()?;
                stream.set_nonblocking(false)?;
                if let Ok(ctrl) = stream.try_clone() {
                    state.client_streams.push(ctrl);
                }
                let tx = daemon_tx.clone();
                task::spawn_blocking(move || {
                    if let Err(e) = crate::server::connection::client_thread(stream, tx) {
                        error!(error = %e, "client error");
                    }
                });
            }
            Some(cmd) = daemon_rx.recv() => {
                state.handle_command(cmd);
            }
            _ = wait_for_shutdown() => break Ok(()),
        }
    };

    for stream in state.client_streams.drain(..) {
        if let Ok(writer) = stream.try_clone() {
            let mut writer = BufWriter::new(writer);
            let _ = tai_proto::write_message_sync(&mut writer, &DaemonMessage::ShuttingDown);
        }
        let _ = stream.shutdown(Shutdown::Both);
    }

    let active_sessions = std::mem::take(&mut state.active_sessions);
    for entry in active_sessions.values() {
        let _ = entry.cmd_tx.send(SessionCommand::Shutdown);
    }

    for (_, entry) in active_sessions {
        let _ = entry.handle.await;
    }

    if Path::new(socket_path).exists() {
        std::fs::remove_file(socket_path)?;
    }
    result
}
