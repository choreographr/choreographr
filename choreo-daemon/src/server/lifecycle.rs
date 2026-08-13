use crate::daemon::{DaemonCommand, DaemonState};
use crate::sessions::SessionCommand;
use choreo_transport::key::TransportSecretKey;
use signal_hook::consts::{SIGINT, SIGTERM};
use std::io;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, TcpListener, TcpStream};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};
use tracing::{error, info, warn};

/// Grace period for joining connection threads during shutdown. After
/// `BroadcastShuttingDown`, every healthy writer flushes the notification and
/// closes its own socket, the reader sees EOF, and the connection thread's
/// cleanup joins the writer — so the join completes in microseconds. The
/// bound exists for a client that stopped reading: its writer stays stuck in
/// a blocking socket write, the writer never processes `ShuttingDown`, and
/// the connection thread blocks joining the writer. Shutdown must not hang
/// on that, so the join is abandoned after the grace period (the daemon
/// process exits and the OS closes the socket anyway). Mirrors
/// `sessions::SESSION_SHUTDOWN_GRACE`.
const CONNECTION_DRAIN_GRACE: Duration = Duration::from_secs(5);

/// Bound for the shutdown wake-probe's connect to the TCP accept thread. The
/// probe only needs to land in the accept queue; a healthy listener accepts it
/// in microseconds. Without a bound, a full accept backlog (a connection burst
/// coinciding with shutdown) would make the blocking connect wait out the
/// kernel's SYN-retry period (~130 s), stalling shutdown — so the probe itself
/// is bounded, and a listener the probe cannot reach is simply left to the
/// drain grace below.
const ACCEPT_PROBE_CONNECT_TIMEOUT: Duration = Duration::from_secs(1);

/// Join a connection thread, giving up once `deadline` passes. Returns
/// whether the thread exited before the deadline.
///
/// A connection thread is the owner of its writer: `cleanup_client` joins
/// the writer thread after the socket EOF, so joining the connection thread
/// transitively waits for the writer to flush `ShuttingDown` and close its
/// own socket — this is what makes notify-before-EOF observable even when
/// `run_server` is embedded in-process (no process exit to reap threads).
/// All connection threads share one deadline (see the shutdown path below) so
/// N wedged clients cost ~one grace period instead of N × grace.
fn join_connection_thread(handle: thread::JoinHandle<()>, deadline: Instant) -> bool {
    while !handle.is_finished() {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            warn!("connection thread did not exit before shutdown deadline; abandoning join");
            return false;
        }
        thread::sleep(remaining.min(Duration::from_millis(10)));
    }
    if let Err(e) = handle.join() {
        error!("connection thread panicked during shutdown: {e:?}");
    }
    true
}

/// Prune `client_threads` once it grows past this many retained handles.
/// Handles are kept so shutdown can bound-join every live connection thread;
/// pruning finished ones eagerly stops a long-running daemon from
/// accumulating one handle per connection ever accepted.
const CLIENT_THREAD_PRUNE_THRESHOLD: usize = 64;

/// Track a connection thread's JoinHandle for the shutdown drain, pruning
/// handles of already-finished threads once the Vec grows past
/// [`CLIENT_THREAD_PRUNE_THRESHOLD`]. A finished thread's handle can be
/// dropped without joining (the OS thread is already reaped); a still-running
/// handle must be retained — dropping it would detach the thread and lose the
/// shutdown join — so only finished ones are pruned.
fn push_client_thread(
    client_threads: &mut Vec<thread::JoinHandle<()>>,
    handle: thread::JoinHandle<()>,
) {
    if client_threads.len() >= CLIENT_THREAD_PRUNE_THRESHOLD {
        client_threads.retain(|h| !h.is_finished());
    }
    client_threads.push(handle);
}

/// Collect every connection-thread handle the TCP accept thread has ferried
/// over the channel since the last drain, routing them through
/// [`push_client_thread`].
fn drain_tcp_handles(
    rx: &mpsc::Receiver<thread::JoinHandle<()>>,
    client_threads: &mut Vec<thread::JoinHandle<()>>,
) {
    while let Ok(handle) = rx.try_recv() {
        push_client_thread(client_threads, handle);
    }
}

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

    // Connection threads are tracked so shutdown can wait for them — and,
    // through them, their writer threads — to flush `ShuttingDown` and close
    // their own sockets before `run_server` returns. Unix connection threads
    // are spawned directly below and pushed here; TCP connection threads are
    // spawned inside the accept thread, so their JoinHandles are ferried back
    // over a channel.
    let mut client_threads: Vec<thread::JoinHandle<()>> = Vec::new();
    let (tcp_client_tx, tcp_client_rx) = mpsc::channel::<thread::JoinHandle<()>>();

    // TCP listener for Noise IK clients. The `ShuttingDown` notification is
    // routed through the daemon's client_writers registry, exactly as on the
    // Unix path; the TCP accept thread just spawns a per-connection
    // `tcp_client_thread`, whose writer thread owns and closes its own socket.
    let tcp_shutdown = Arc::clone(&shutdown);
    // The accept thread itself is tracked (and later woken + bounded-joined)
    // so shutdown can close the spawn/drain race: once the thread has exited,
    // every connection handle it ever spawned has been sent over
    // `tcp_client_tx` (handles are ferried from the accept thread immediately
    // after each spawn), so the final drain below captures all of them.
    let mut tcp_accept_handle: Option<thread::JoinHandle<()>> = None;
    let mut tcp_accept_addr: Option<SocketAddr> = None;
    if let Some(ref tcp_addr_str) = tcp_addr {
        let addr: SocketAddr = tcp_addr_str.parse().map_err(|e| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("invalid --tcp-addr: {e}"),
            )
        })?;
        tcp_accept_addr = Some(addr);
        let listener = TcpListener::bind(addr)
            .map_err(|e| io::Error::other(format!("failed to bind TCP listener on {addr}: {e}")))?;
        info!("TCP (Noise IK) listening on {addr}");

        let daemon_tx = daemon_tx.clone();
        let acl = Arc::clone(&acl);
        let tcp_client_tx = tcp_client_tx.clone();
        tcp_accept_handle = Some(thread::spawn(move || {
            loop {
                if tcp_shutdown.load(std::sync::atomic::Ordering::SeqCst) {
                    break;
                }
                match listener.accept() {
                    Ok((tcp, _)) => {
                        if tcp_shutdown.load(std::sync::atomic::Ordering::SeqCst) {
                            // Shutdown wake-up probe (see the shutdown path
                            // below): the main thread connected to unblock this
                            // accept; drop the probe WITHOUT spawning a client
                            // thread for it.
                            drop(tcp);
                            break;
                        }
                        let tx = daemon_tx.clone();
                        let sk_bytes = *transport_sk.as_bytes();
                        let acl = Arc::clone(&acl);
                        // Register the writer channel BEFORE the handshake (see
                        // register_client_writer): the main thread joins this
                        // accept thread before sending the shutdown broadcast,
                        // so a register issued here is always ordered before
                        // the broadcast — a connection accepted concurrently
                        // with shutdown cannot miss ShuttingDown.
                        let (client_id, writer_tx, writer_rx) =
                            crate::server::connection::register_client_writer(&tx);
                        let handle = thread::spawn(move || {
                            let noise = match choreo_transport::noise::handshake_responder(
                                tcp,
                                &sk_bytes,
                                |pk| acl.contains(pk),
                            ) {
                                Ok(ns) => ns,
                                Err(e) => {
                                    error!(error = %e, "Noise IK handshake rejected");
                                    // The writer channel was registered at accept
                                    // time; unregister so a failed handshake does
                                    // not leave a stale writer entry in the
                                    // daemon's client_writers registry.
                                    let _ =
                                        tx.send(DaemonCommand::ClientDisconnected { client_id });
                                    return;
                                }
                            };
                            if let Err(e) = crate::server::connection::tcp_client_thread(
                                noise, tx, client_id, writer_tx, writer_rx,
                            ) {
                                error!(error = %e, "TCP client error");
                            }
                        });
                        // Ferry the handle back to the main thread so shutdown
                        // can wait for this connection thread too.
                        let _ = tcp_client_tx.send(handle);
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
        }));
    }

    // Main thread accept loop — blocking accept() is event-driven
    // (the kernel deschedules us until a connection arrives).
    loop {
        if shutdown.load(Ordering::SeqCst) {
            break;
        }
        // Collect TCP connection threads whose handshakes completed since the
        // last iteration so shutdown can wait for them too.
        drain_tcp_handles(&tcp_client_rx, &mut client_threads);
        match listener.accept() {
            Ok((stream, _)) => {
                if shutdown.load(Ordering::SeqCst) {
                    // Wakeup from the signal handler — shut down.
                    break;
                }
                crate::metrics::record_connection_accepted();
                let tx = daemon_tx.clone();
                // Register the writer channel with the daemon BEFORE spawning
                // the connection thread — see register_client_writer for why
                // this closes the "connection accepted concurrently with
                // shutdown misses ShuttingDown" race.
                let (client_id, writer_tx, writer_rx) =
                    crate::server::connection::register_client_writer(&daemon_tx);
                push_client_thread(
                    &mut client_threads,
                    thread::spawn(move || {
                        if let Err(e) = crate::server::connection::client_thread(
                            stream, tx, client_id, writer_tx, writer_rx,
                        ) {
                            error!(error = %e, "client error");
                        }
                    }),
                );
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

    // Wake and bounded-join the TCP accept thread BEFORE the broadcast and
    // the connection drain: a spurious probe connect below makes its blocked
    // accept() return, the shutdown-flag check drops the probe, and the thread
    // exits. Because handles are ferried from the accept thread itself (right
    // after each spawn), every connection thread it ever started is already on
    // `tcp_client_rx` once it has exited — so joining it here closes the race
    // where the accept thread could spawn a connection after the drain, whose
    // handle would never be joined.
    if let Some(addr) = tcp_accept_addr {
        // Wake the accept thread: probe the address the listener is actually
        // bound to when it is concrete — a specific non-loopback bind (e.g.
        // 192.168.1.10:9000) is unreachable via loopback probes. An
        // unspecified bind (0.0.0.0 / ::) cannot be connected back to, so
        // probe the matching loopback address instead.
        let probe = match addr.ip() {
            IpAddr::V4(ip) if ip.is_unspecified() => {
                SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), addr.port())
            }
            IpAddr::V6(ip) if ip.is_unspecified() => {
                SocketAddr::new(IpAddr::V6(Ipv6Addr::LOCALHOST), addr.port())
            }
            _ => addr,
        };
        let _ = TcpStream::connect_timeout(&probe, ACCEPT_PROBE_CONNECT_TIMEOUT);
    }
    if let Some(handle) = tcp_accept_handle.take() {
        join_connection_thread(handle, Instant::now() + CONNECTION_DRAIN_GRACE);
    }

    // Collect TCP connection threads spawned concurrently with shutdown so
    // they are not missed by the bounded join below.
    drain_tcp_handles(&tcp_client_rx, &mut client_threads);

    // Route the shutdown notification through each connection's single writer
    // thread (via the command loop's client_writers registry), then stop the
    // command loop. Each writer thread flushes ShuttingDown and closes its own
    // socket, so a client observes the notification before the EOF. The main
    // thread writes nothing to client sockets — that is what guarantees the
    // notification cannot be lost to a race with a socket close.
    let _ = daemon_tx.send(DaemonCommand::BroadcastShuttingDown);
    let _ = daemon_tx.send(DaemonCommand::Shutdown);
    drop(daemon_tx);
    cmd_handle.join().unwrap_or_else(|e| {
        error!("command thread panicked: {e:?}");
    });

    // Collect any stragglers, then wait (bounded) for each connection thread
    // — and, through it, its writer thread — to finish. Every healthy writer
    // flushes ShuttingDown and closes its own socket after the broadcast
    // above, the reader then sees EOF and cleanup joins the writer, so the
    // join completes promptly. The deadline (CONNECTION_DRAIN_GRACE) covers a
    // client that stopped reading, which would otherwise wedge its writer in
    // a blocking socket write and hang shutdown; all connections share one
    // deadline so N wedged clients cost ~one grace period, not N × grace.
    drain_tcp_handles(&tcp_client_rx, &mut client_threads);
    let drain_deadline = Instant::now() + CONNECTION_DRAIN_GRACE;
    for handle in client_threads {
        join_connection_thread(handle, drain_deadline);
    }

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

    /// Finished connection threads are pruned once the retained Vec grows past
    /// the threshold, so a long-running daemon does not accumulate one
    /// JoinHandle per connection ever accepted; a still-running handle is
    /// retained for the shutdown join.
    #[test]
    fn push_client_thread_prunes_finished_handles() {
        let mut handles = Vec::new();
        // Fill past the prune threshold with threads that have already exited.
        // Spinning on `is_finished` is deterministic — the closure is empty,
        // so the thread cannot fail to finish — and avoids any sleep.
        for _ in 0..=CLIENT_THREAD_PRUNE_THRESHOLD {
            let h = thread::spawn(|| {});
            while !h.is_finished() {
                std::hint::spin_loop();
            }
            handles.push(h);
        }

        // A live thread: blocked on a channel until released, so it is
        // guaranteed to be running when pushed and must survive the prune.
        let (tx, rx) = mpsc::channel::<()>();
        let live = thread::spawn(move || {
            let _ = rx.recv();
        });
        push_client_thread(&mut handles, live);

        assert_eq!(
            handles.len(),
            1,
            "only the still-running handle must survive the prune"
        );
        tx.send(()).unwrap();
        handles.pop().unwrap().join().unwrap();
    }
}
