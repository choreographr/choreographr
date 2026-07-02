use crate::DaemonState;
use crate::server::connection::handle_client;
use std::{io, path::Path, sync::Arc, time::Duration};
use tokio::{
    net::UnixListener,
    signal::unix::{signal, SignalKind},
    task::JoinSet,
};
use tracing::{debug, error, info, warn};

const SHUTDOWN_DRAIN_SECS: u64 = 10;

async fn wait_for_shutdown() {
    let mut sigint = signal(SignalKind::interrupt()).expect("failed to install SIGINT handler");
    let mut sigterm = signal(SignalKind::terminate()).expect("failed to install SIGTERM handler");
    tokio::select! {
        _ = sigint.recv() => info!("received SIGINT, shutting down tai-daemon"),
        _ = sigterm.recv() => info!("received SIGTERM, shutting down tai-daemon"),
    }
}

pub async fn run_server(socket_path: &str, state: DaemonState) -> io::Result<()> {
    if Path::new(socket_path).exists() {
        info!(%socket_path, "removing stale socket");
        std::fs::remove_file(socket_path)?;
    }

    let listener = UnixListener::bind(socket_path)?;
    info!(%socket_path, "tai-daemon listening");

    let mut client_handles = JoinSet::new();

    let result = loop {
        tokio::select! {
            accept_result = listener.accept() => {
                let (stream, _) = accept_result?;
                debug!("accepted client connection");
                let state = Arc::clone(&state);
                client_handles.spawn(async move {
                    if let Err(error) = handle_client(stream, state).await {
                        error!(error = %error, "client error");
                    }
                });
            }
            _ = wait_for_shutdown() => break Ok(()),
        }
    };

    info!(
        "draining active client connections ({}s timeout)...",
        SHUTDOWN_DRAIN_SECS
    );
    let drained = tokio::time::timeout(
        Duration::from_secs(SHUTDOWN_DRAIN_SECS),
        async {
            while let Some(result) = client_handles.join_next().await {
                if let Err(e) = result && !e.is_cancelled() {
                    warn!(error = %e, "client handler panicked during drain");
                }
            }
        },
    )
    .await;

    if drained.is_err() {
        warn!("shutdown drain timed out, aborting remaining client handlers");
        client_handles.abort_all();
    }

    if Path::new(socket_path).exists() {
        info!(%socket_path, "removing socket");
        std::fs::remove_file(socket_path)?;
    }

    result
}
