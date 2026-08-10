use crate::daemon::{DaemonCommand, DaemonState};
use crate::sessions::SessionCommand;
use choreo_proto::DaemonMessage;
use choreo_transport::key::TransportSecretKey;
use signal_hook::consts::{SIGINT, SIGTERM};
use std::io::{self, BufWriter};
use std::net::{Shutdown, SocketAddr, TcpListener};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;
use tracing::{error, info};

/// Resolve and spawn the `/metrics` HTTP server thread.
///
/// Feature-on build: parse the socket address (rejecting garbage with an
/// actionable message) and serve on it until the shutdown flag is set.
#[cfg(feature = "metrics")]
fn start_metrics_server(addr_str: &str, shutdown: &Arc<AtomicBool>) -> io::Result<()> {
    let addr: SocketAddr = addr_str.parse().map_err(|e| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("invalid --metrics-addr: {e}"),
        )
    })?;
    let shutdown_flag = Arc::clone(shutdown);
    thread::spawn(move || {
        crate::metrics::serve_metrics(addr, shutdown_flag);
    });
    Ok(())
}

/// Refuse startup when `--metrics-addr` is passed to a feature-off build.
///
/// The flag is still parsed by clap (so scripts that pass it get this clear,
/// actionable error instead of clap's confusing "unexpected argument"), but
/// the daemon refuses to start rather than silently ignoring the requested
/// endpoint.
#[cfg(not(feature = "metrics"))]
fn start_metrics_server(addr_str: &str, _shutdown: &Arc<AtomicBool>) -> io::Result<()> {
    Err(io::Error::other(format!(
        "--metrics-addr {addr_str}: this build was compiled without the \
         `metrics` feature; rebuild with `--features metrics` to serve /metrics"
    )))
}

pub fn run_server(
    socket_path: &str,
    mut state: DaemonState,
    metrics_addr: Option<String>,
    tcp_addr: Option<String>,
    transport_sk: TransportSecretKey,
    acl: std::sync::Arc<crate::server::acl::Acl>,
) -> io::Result<()> {
    if Path::new(socket_path).exists() {
        std::fs::remove_file(socket_path)?;
    }
    let listener = UnixListener::bind(socket_path)?;
    info!(%socket_path, "choreographr listening");

    let (daemon_tx, daemon_rx) = mpsc::channel::<DaemonCommand>();
    state.daemon_tx = daemon_tx.clone();

    let shutdown = Arc::new(AtomicBool::new(false));

    // Signal handler thread: sets the shutdown flag and connects to our own
    // socket to unblock the blocking accept() call on the main thread.
    let sig_shutdown = Arc::clone(&shutdown);
    let sig_path = socket_path.to_string();
    thread::spawn(move || {
        let mut signals = match signal_hook::iterator::Signals::new([SIGINT, SIGTERM]) {
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
        // Join each session thread with a bounded grace period: a request
        // worker stuck in an LLM provider read (which a cancel cannot
        // interrupt promptly) must not hang the daemon's shutdown.  The
        // graceful path exits promptly because the worker responds to the
        // cancel; only pathological cases hit the grace deadline.
        //
        // Join the session threads concurrently (bounded by
        // SESSION_SHUTDOWN_GRACE per session) so N stuck sessions cost ~one
        // grace period instead of N × grace.
        let joiners: Vec<_> = active_sessions
            .into_iter()
            .map(|(session_id, entry)| {
                std::thread::spawn(move || {
                    crate::sessions::join_session_shutdown(entry.handle, session_id)
                })
            })
            .collect();
        for joiner in joiners {
            let _ = joiner.join();
        }
        // Shut down MCP servers after all sessions have exited.
        state.mcp_manager.shutdown_all();
    });

    // Initialize the metrics registry so that instrumented code throughout
    // the daemon can safely call record_* functions (they no-op when
    // uninitialized, and are compiled-out no-ops when the `metrics` feature
    // is disabled).  This must happen before the accept loop starts.
    crate::metrics::init().map_err(io::Error::other)?;

    // Metrics HTTP server thread (if `--metrics-addr` was provided).
    // Spawned before the accept loop so it's reachable immediately.
    //
    // When the `metrics` feature is disabled the flag is still parsed (so a
    // script that passes it gets a clear, actionable error instead of clap's
    // confusing "unexpected argument"), but the daemon refuses to start
    // rather than silently ignoring the requested endpoint.
    if let Some(ref addr_str) = metrics_addr {
        start_metrics_server(addr_str, &shutdown)?;
    }

    // TCP listener for Noise IK clients.
    let tcp_shutdown = Arc::clone(&shutdown);
    if let Some(ref tcp_addr_str) = tcp_addr {
        let addr: SocketAddr = tcp_addr_str.parse().map_err(|e| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("invalid --tcp-addr: {e}"),
            )
        })?;
        let listener = TcpListener::bind(addr)
            .map_err(|e| io::Error::other(format!("failed to bind TCP listener on {addr}: {e}")))?;
        info!("TCP (Noise IK) listening on {addr}");

        let daemon_tx = daemon_tx.clone();
        let acl = Arc::clone(&acl);
        thread::spawn(move || {
            loop {
                if tcp_shutdown.load(std::sync::atomic::Ordering::SeqCst) {
                    break;
                }
                match listener.accept() {
                    Ok((tcp, _)) => {
                        let tx = daemon_tx.clone();
                        let sk_bytes = *transport_sk.as_bytes();
                        let acl = Arc::clone(&acl);
                        thread::spawn(move || {
                            let noise = match choreo_transport::noise::handshake_responder(
                                tcp,
                                &sk_bytes,
                                |pk| acl.contains(pk),
                            ) {
                                Ok(ns) => ns,
                                Err(e) => {
                                    error!(error = %e, "Noise IK handshake rejected");
                                    return;
                                }
                            };
                            if let Err(e) = crate::server::connection::tcp_client_thread(noise, tx)
                            {
                                error!(error = %e, "TCP client error");
                            }
                        });
                    }
                    Err(e) if e.kind() == io::ErrorKind::Interrupted => {
                        // Blocking accept was interrupted by a signal.
                        // Retry immediately.
                        continue;
                    }
                    Err(e) if e.kind() == io::ErrorKind::ConnectionAborted => {
                        // The connection was aborted before accept completed.
                        // No FD was consumed, retry immediately.
                        continue;
                    }
                    Err(e) => {
                        // Transient or resource-exhaustion errors
                        // (EMFILE, ENFILE, etc.) — log and retry with
                        // backoff so other threads can close FDs.
                        error!(error = %e, "TCP accept error, retrying");
                        thread::sleep(Duration::from_millis(100));
                    }
                }
            }
        });
    }

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
                crate::metrics::record_connection_accepted();
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
            Err(e) if e.kind() == io::ErrorKind::ConnectionAborted => {
                // The connection was aborted before accept completed.
                // No FD was consumed, retry immediately.
                continue;
            }
            Err(e) => {
                // Transient or resource-exhaustion errors
                // (ECONNABORTED, EMFILE, ENFILE, etc.) — log
                // and retry with backoff so other threads can
                // close file descriptors.
                error!(error = %e, "accept error, retrying");
                thread::sleep(Duration::from_millis(100));
            }
        }
    }

    info!("shutting down");

    // Notify connected clients.
    for stream in client_streams.iter() {
        if let Ok(writer) = stream.try_clone() {
            let mut writer = BufWriter::new(writer);
            let _ = choreo_proto::write_message(&mut writer, &DaemonMessage::ShuttingDown);
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Feature-on builds parse the address before spawning the server thread,
    /// so a malformed `--metrics-addr` must be a startup error — and the
    /// message must say why (no server is spawned for a bad address).
    #[cfg(feature = "metrics")]
    #[test]
    fn metrics_addr_rejects_malformed_socket() {
        let shutdown = Arc::new(AtomicBool::new(false));
        let err = start_metrics_server("not-a-socket-address", &shutdown).unwrap_err();
        assert!(
            err.to_string().contains("invalid --metrics-addr"),
            "unexpected error: {err}"
        );
    }

    /// Feature-off builds refuse startup entirely and must point the operator
    /// at the opt-in feature.  This is the path `cargo test-lean` keeps honest:
    /// the `--all-features` test aliases never compile it.
    #[cfg(not(feature = "metrics"))]
    #[test]
    fn metrics_addr_refused_when_feature_off() {
        let shutdown = Arc::new(AtomicBool::new(false));
        let err = start_metrics_server("127.0.0.1:9464", &shutdown).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("--metrics-addr"), "unexpected error: {msg}");
        assert!(
            msg.contains("--features metrics"),
            "error must point at the opt-in feature: {msg}"
        );
    }
}
