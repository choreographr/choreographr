use crate::daemon::{DaemonCommand, DaemonState};
use crate::sessions::SessionCommand;
use choreo_transport::key::TransportSecretKey;
// The signal constants are consumed by the Unix iterator thread; the Windows
// flag thread imports them locally (signal-hook's iterator module is unix-only).
#[cfg(unix)]
use signal_hook::consts::{SIGINT, SIGTERM};
use std::io;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, TcpListener, TcpStream};
#[cfg(unix)]
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};
use tracing::{error, info, warn};
#[cfg(windows)]
use uds_windows::{UnixListener, UnixStream};

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

/// Join a thread with a deadline, giving up once `deadline` passes. Returns
/// whether the thread exited before the deadline.
///
/// The bounded join is the shared primitive behind every "wait for a thread
/// but do not hang on it" site in the daemon:
///
/// * the shutdown drain joins each connection thread against one shared
///   deadline, so N wedged clients cost ~one grace period instead of N ×
///   grace. A connection thread owns its writer: `cleanup_client` joins the
///   writer thread after the socket EOF, so joining the connection thread
///   transitively waits for the writer to flush `ShuttingDown` and close its
///   own socket — this is what makes notify-before-EOF observable even when
///   `run_server` is embedded in-process (no process exit to reap threads).
/// * `run_server` joins the TCP accept thread before the broadcast so no
///   connection handle can be spawned after the drain.
/// * `cleanup_client` joins a connection's writer thread with a short grace
///   so a writer wedged in a blocking socket write cannot wedge its
///   connection thread's cleanup.
pub(crate) fn join_thread_bounded(handle: thread::JoinHandle<()>, deadline: Instant) -> bool {
    while !handle.is_finished() {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            warn!("thread did not exit before shutdown deadline; abandoning join");
            return false;
        }
        thread::sleep(remaining.min(Duration::from_millis(10)));
    }
    if let Err(e) = handle.join() {
        error!("thread panicked during shutdown: {e:?}");
    }
    true
}

/// Prune `client_threads` once it grows past this many retained handles.
/// Handles are kept so shutdown can bound-join every live connection thread;
/// pruning finished ones eagerly stops a long-running daemon from
/// accumulating one handle per connection ever accepted.
const CLIENT_THREAD_PRUNE_THRESHOLD: usize = 64;

/// Maximum concurrently-connected clients (both transports combined). Each
/// connection holds two threads (connection + writer) and a socket FD, so an
/// unbounded number of wedged-but-open clients (connected, not reading) could
/// exhaust thread/FD resources even though each is individually harmless
/// (per-connection backpressure never blocks the command loop). The cap turns
/// that unbounded accumulation into a bounded one: once it is hit, a new
/// connection is accepted and immediately dropped (the client sees a bare
/// EOF rather than hanging in the accept backlog) and the event is logged.
/// Generous for a personal daemon (TUI + GUI + IM bridge + a handful of
/// mobile clients).
const MAX_CONCURRENT_CONNECTIONS: usize = 256;

/// RAII live-connection slot: decrements the daemon-wide connection counter
/// when a connection thread exits — including on panic — so a connection can
/// never leak its slot and slowly eat into the cap. Owns an `Arc` clone so it
/// can be moved into the spawned connection thread.
struct ConnectionSlot(Arc<AtomicUsize>);

impl Drop for ConnectionSlot {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::Relaxed);
    }
}

/// Try to take a connection slot under [`MAX_CONCURRENT_CONNECTIONS`].
/// Atomic `fetch_add` makes the check-and-take race-free across the two
/// accept paths (Unix main thread + TCP accept thread); on rejection the
/// increment is undone and `None` is returned.
fn try_take_connection_slot(count: &Arc<AtomicUsize>) -> Option<ConnectionSlot> {
    if count.fetch_add(1, Ordering::Relaxed) >= MAX_CONCURRENT_CONNECTIONS {
        count.fetch_sub(1, Ordering::Relaxed);
        return None;
    }
    Some(ConnectionSlot(Arc::clone(count)))
}

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

/// Handle an accept() error the way both accept loops do: a transient error
/// (interrupted syscall — `signal_hook` does not use SA_RESTART — or a
/// connection aborted before accept completed, which consumed no FD) is
/// retried immediately; a resource-exhaustion error (EMFILE/ENFILE/…) is
/// logged and backed off so other threads can close FDs. Both loops continue
/// after this, so it returns nothing.
fn handle_accept_error(e: io::Error) {
    match e.kind() {
        io::ErrorKind::Interrupted | io::ErrorKind::ConnectionAborted => {}
        _ => {
            error!(error = %e, "accept error, retrying");
            thread::sleep(Duration::from_millis(100));
        }
    }
}

pub fn run_server(
    socket_path: &str,
    mut state: DaemonState,
    metrics_addr: Option<String>,
    tcp_addr: Option<String>,
    transport_sk: TransportSecretKey,
    acl: std::sync::Arc<crate::server::acl::SharedAcl>,
) -> io::Result<()> {
    // Both operations carry the socket path in the error: a bind/removal
    // failure otherwise surfaces as a context-free "Permission denied (os
    // error 13)" (the Termux /tmp failure mode) with no hint WHICH path
    // was the problem — the path is the entire diagnosis.
    if Path::new(socket_path).exists() {
        std::fs::remove_file(socket_path).map_err(|e| {
            io::Error::new(
                e.kind(),
                format!("removing the stale socket at {socket_path}: {e}"),
            )
        })?;
    }
    let listener = UnixListener::bind(socket_path).map_err(|e| {
        io::Error::new(
            e.kind(),
            format!("binding the Unix socket at {socket_path}: {e}"),
        )
    })?;
    info!(%socket_path, "choreographr listening");

    let (daemon_tx, daemon_rx) = mpsc::channel::<DaemonCommand>();
    state.daemon_tx = daemon_tx.clone();

    // Install the shared ACL into the state BEFORE the command loop takes
    // ownership: the command loop becomes its single WRITER (AclReload),
    // while the TCP accept path below keeps a clone for lock-free reads.
    // One Arc, two roles — see the SharedAcl docs for the exception-#4
    // rationale.
    let acl_path = acl.path().to_path_buf();
    state.acl = Some(acl.clone());

    // Dedicated config watcher for the ACL file. It watches the ACL's OWN
    // parent directory, not the general config dir: in production they are
    // the same directory, but tests (and any future --acl-path override)
    // place the ACL elsewhere — and the watcher must follow the file the
    // SharedAcl actually holds, not where the catalog overlay happens to
    // live. The basename subscription keeps unrelated files in that dir
    // from triggering reloads.
    if let Some(acl_dir) = acl_path.parent() {
        let mut acl_watcher = crate::config_watch::ConfigWatcher::new(acl_dir.to_path_buf());
        let acl_rx = acl_watcher.subscribe(
            acl_path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .as_deref()
                .unwrap_or("authorized_clients.toml"),
        );
        acl_watcher.spawn();
        crate::server::acl::spawn_acl_watcher(daemon_tx.clone(), acl_rx);
    } else {
        warn!(
            path = %acl_path.display(),
            "ACL path has no parent directory; ACL hot-reload disabled"
        );
    }

    // Shared config-file watching transport: ONE notify watcher on the config
    // directory, fanned out per-basename to consumers (the catalog overlay,
    // accounts, and future files). Spawned before the consumers that react to
    // its events. Degrades gracefully to no transport (and no auto-reload)
    // when the config dir cannot be resolved.
    let overlay_rx = match state.catalog_paths.overlay.parent().map(Path::to_path_buf) {
        Some(config_dir) => {
            let mut config_watcher = crate::config_watch::ConfigWatcher::new(config_dir);
            // The catalog maintenance thread reacts to overlay edits; the
            // accounts watcher reacts to accounts.toml edits. Each consumer
            // owns its reload policy (see `handle_accounts_reload` and the
            // maintenance loop's overlay arm).
            let overlay_rx = config_watcher.subscribe(crate::catalog::USER_OVERLAY_NAME);
            let accounts_rx = config_watcher.subscribe(crate::accounts::ACCOUNTS_TOML_NAME);
            config_watcher.spawn();
            crate::accounts::spawn_accounts_watcher(daemon_tx.clone(), accounts_rx);
            overlay_rx
        }
        None => {
            warn!("config directory not resolvable; config-file auto-reload disabled");
            // A never-delivering receiver so the maintenance thread still runs
            // (it just has no overlay events to react to).
            crossbeam_channel::never()
        }
    };

    // Spawn the ONE background catalog-maintenance thread (S4) before the
    // command loop is moved into its own thread: it loads the cache, does the
    // startup models.dev conditional GET, reacts to user-overlay edits from
    // the config transport, and serves `/refresh-models` requests — all over
    // channels, and never mutating the catalog itself (every change goes
    // through `DaemonCommand::CatalogBaseChanged` back to the command loop,
    // the single writer of the catalog ArcSwap). Spawned before the accept
    // loop so the startup swap lands promptly.
    let maintenance_tx = crate::catalog::spawn_catalog_maintenance(
        daemon_tx.clone(),
        state.db.clone(),
        state.catalog_paths.clone(),
        overlay_rx,
    );
    state.maintenance_tx = Some(maintenance_tx);

    let shutdown = Arc::new(AtomicBool::new(false));

    // Signal handler thread: sets the shutdown flag and connects to our own
    // socket to unblock the blocking accept() call on the main thread.
    //
    // Unix: blocking iterator over the self-pipe (unchanged behavior).
    #[cfg(unix)]
    {
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
    }

    // Windows: no sigwait and no iterator module — `low_level::register`
    // installs the CRT console handler (the same primitive `flag::register`
    // is built on), forwarding each signal as a channel message; the thread
    // then blocks in `recv()` with zero CPU instead of polling a flag, then
    // wakes the accept loop the same way as Unix (a connect to our own socket
    // unblocks accept()).
    #[cfg(windows)]
    {
        let sig_shutdown = Arc::clone(&shutdown);
        let sig_path = socket_path.to_string();
        thread::spawn(move || {
            use signal_hook::consts::{SIGINT, SIGTERM};
            let (sig_tx, sig_rx) = mpsc::channel::<()>();
            let int_tx = sig_tx.clone();
            let term_tx = sig_tx;
            // SAFETY: on Windows the registered action runs on the CRT's
            // console-handler thread, where an mpsc send is safe (no POSIX
            // async-signal restrictions apply); the senders are moved into
            // the registrations and outlive them.
            if let Err(e) = unsafe {
                signal_hook::low_level::register(SIGINT, move || {
                    let _ = int_tx.send(());
                })
            }
            .and_then(|_| unsafe {
                signal_hook::low_level::register(SIGTERM, move || {
                    let _ = term_tx.send(());
                })
            }) {
                error!("failed to register signal handlers: {e}");
                return;
            }
            // Block until a signal arrives (channel recv — no polling), then
            // wake the accept loop: the pending connection causes the next
            // blocking accept() to return immediately so the shutdown flag is
            // checked. The senders are held by the registrations, so recv
            // never returns Err and the loop runs until the process exits.
            while sig_rx.recv().is_ok() {
                sig_shutdown.store(true, Ordering::SeqCst);
                if let Ok(stream) = UnixStream::connect(&sig_path) {
                    drop(stream);
                }
            }
        });
    }

    // Clone the daemon-wide lag counter for the accept paths BEFORE `state`
    // moves into the command-loop thread below: `register_client_writer`
    // hands it to every connection's writer thread so dequeue-side accounting
    // decrements the SAME counter the command loop and session threads
    // increment on enqueue.
    let global_lag = Arc::clone(&state.global_lag);

    // Daemon command handler thread.
    let cmd_handle = thread::spawn(move || {
        loop {
            match daemon_rx.recv() {
                Ok(DaemonCommand::Shutdown) => {
                    // Announce the stage: everything after this line is
                    // teardown (session joins, MCP shutdown), and each stage
                    // below logs its completion — the last line printed
                    // under a wedged Ctrl+C identifies the culprit.
                    info!("command loop: shutdown command received; beginning teardown");
                    break;
                }
                Ok(cmd) => state.handle_command(cmd),
                Err(mpsc::RecvError) => {
                    info!("command loop: all daemon command senders dropped");
                    break;
                }
            }
        }
        let active_sessions = std::mem::take(&mut state.active_sessions);
        info!(
            active_sessions = active_sessions.len(),
            "command loop teardown: signalling session threads"
        );
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
        info!("command loop teardown: session threads drained");
        // Shut down MCP servers after all sessions have exited.
        // `shutdown_all` logs its begin/end; a wedge between those two lines
        // means an MCP client lock is held by a stuck tool call.
        state.mcp_manager.shutdown_all();
        info!("command loop teardown: complete");
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

    // Daemon-wide live-connection counter backing MAX_CONCURRENT_CONNECTIONS.
    // Both accept paths take a slot per accepted connection, so the cap is
    // enforced across the Unix main thread and the TCP accept thread.
    let conn_count = Arc::new(AtomicUsize::new(0));

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
        // Clone the connection counter into this accept thread (same pattern
        // as the daemon_tx/acl clones above): the main thread keeps its own
        // Arc for the Unix accept path, so the shared cap is enforced across
        // both transports.
        let conn_count = Arc::clone(&conn_count);
        // Clone the lag counter for the TCP accept thread too — the main
        // thread keeps the original for the Unix accept path, and each
        // connection thread gets its own clone from here.
        let global_lag_tcp = Arc::clone(&global_lag);
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
                        // Enforce the concurrent-connection cap: at the cap,
                        // a new connection is accepted and immediately dropped
                        // (the client sees a bare EOF) instead of letting
                        // wedged-but-open clients accumulate threads and FDs
                        // without bound.
                        let Some(slot) = try_take_connection_slot(&conn_count) else {
                            warn!(
                                "connection rejected: at the {MAX_CONCURRENT_CONNECTIONS} concurrent-connection cap"
                            );
                            continue;
                        };
                        // Count the accept toward connections_total — mirrors
                        // the Unix accept path below.
                        crate::metrics::record_connection_accepted();
                        let tx = daemon_tx.clone();
                        let sk_bytes = *transport_sk.as_bytes();
                        let acl = Arc::clone(&acl);
                        let global_lag = Arc::clone(&global_lag_tcp);
                        // Register the writer channel BEFORE the handshake (see
                        // register_client_writer): the main thread joins this
                        // accept thread before sending the shutdown broadcast,
                        // so a register issued here is always ordered before
                        // the broadcast — a connection accepted concurrently
                        // with shutdown cannot miss ShuttingDown.
                        let (client_id, writer_tx, writer_rx) =
                            crate::server::connection::register_client_writer(&tx);
                        let handle = thread::spawn(move || {
                            // Held through the preamble + handshake AND the
                            // connection thread: released when this thread
                            // exits (handshake failure or connection end),
                            // even on panic.
                            let _slot = slot;
                            // The preamble read + handshake-mode dispatch +
                            // responder handshake all live in
                            // tcp_handshake_and_client_thread (server/connection.rs)
                            // so the accept thread stays a pure spawn loop; it
                            // also unregisters the writer channel on every
                            // pre-transport failure path.
                            if let Err(e) =
                                crate::server::connection::tcp_handshake_and_client_thread(
                                    tcp, sk_bytes, acl, tx, client_id, writer_tx, writer_rx,
                                    global_lag,
                                )
                            {
                                error!(error = %e, "TCP client error");
                            }
                        });
                        // Ferry the handle back to the main thread so shutdown
                        // can wait for this connection thread too.
                        let _ = tcp_client_tx.send(handle);
                    }
                    Err(e) => {
                        handle_accept_error(e);
                    }
                }
            }
        }));
    }

    // Main thread accept loop — blocking accept() is event-driven
    // (the kernel deschedules us until a connection arrives).
    loop {
        if shutdown.load(Ordering::SeqCst) {
            info!("accept loop: shutdown flag observed (pre-accept check)");
            break;
        }
        // Collect TCP connection threads whose handshakes completed since the
        // last iteration so shutdown can wait for them too.
        drain_tcp_handles(&tcp_client_rx, &mut client_threads);
        match listener.accept() {
            Ok((stream, _)) => {
                if shutdown.load(Ordering::SeqCst) {
                    // Wakeup from the signal handler — shut down.
                    info!("accept loop: woken by shutdown signal");
                    break;
                }
                // Enforce the concurrent-connection cap: at the cap, the
                // accepted stream is dropped right here (the client sees a
                // bare EOF) rather than letting wedged-but-open clients
                // accumulate threads and FDs without bound.
                let Some(slot) = try_take_connection_slot(&conn_count) else {
                    warn!(
                        "connection rejected: at the {MAX_CONCURRENT_CONNECTIONS} concurrent-connection cap"
                    );
                    continue; // the accepted stream is dropped here; the client sees EOF
                };
                crate::metrics::record_connection_accepted();
                let tx = daemon_tx.clone();
                let global_lag = Arc::clone(&global_lag);
                // Register the writer channel with the daemon BEFORE spawning
                // the connection thread — see register_client_writer for why
                // this closes the "connection accepted concurrently with
                // shutdown misses ShuttingDown" race.
                let (client_id, writer_tx, writer_rx) =
                    crate::server::connection::register_client_writer(&daemon_tx);
                push_client_thread(
                    &mut client_threads,
                    thread::spawn(move || {
                        // Held for this thread's whole lifetime: released
                        // (decrementing the counter) when the connection
                        // thread exits, even on panic.
                        let _slot = slot;
                        if let Err(e) = crate::server::connection::client_thread(
                            stream, tx, client_id, writer_tx, writer_rx, global_lag,
                        ) {
                            error!(error = %e, "client error");
                        }
                    }),
                );
            }
            Err(e) => {
                handle_accept_error(e);
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
        let exited = join_thread_bounded(handle, Instant::now() + CONNECTION_DRAIN_GRACE);
        info!(exited, "TCP accept thread drained");
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
    info!(
        tracked_connection_threads = client_threads.len(),
        "queueing shutdown broadcast + command-loop stop"
    );
    let _ = daemon_tx.send(DaemonCommand::BroadcastShuttingDown);
    let _ = daemon_tx.send(DaemonCommand::Shutdown);
    drop(daemon_tx);
    cmd_handle.join().unwrap_or_else(|e| {
        error!("command thread panicked: {e:?}");
    });
    info!("command loop thread joined");

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
    info!(
        connection_threads = client_threads.len(),
        "draining connection threads (bounded)"
    );
    for handle in client_threads {
        join_thread_bounded(handle, drain_deadline);
    }
    info!("connection threads drained");

    if Path::new(socket_path).exists() {
        std::fs::remove_file(socket_path)?;
    }
    info!("shutdown complete");
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

    /// A connection slot decrements the daemon-wide counter when dropped, so
    /// a connection thread can never leak its slot and slowly eat into the
    /// cap — the decrement also runs on panic, via `Drop`. The slot is taken
    /// through [`try_take_connection_slot`] because that is where the counter
    /// is incremented (atomically, to make check-and-take race-free); the
    /// `ConnectionSlot` constructor itself never touches the counter.
    #[test]
    fn connection_slot_decrements_counter_on_drop() {
        let count = Arc::new(AtomicUsize::new(0));
        {
            let _slot = try_take_connection_slot(&count).expect("under the cap");
            assert_eq!(count.load(Ordering::Relaxed), 1);
        }
        assert_eq!(count.load(Ordering::Relaxed), 0);
    }

    /// At the cap, further connections are rejected (None) and every taken
    /// slot is released when its connection exits, so the counter returns to
    /// zero rather than leaking slots into the cap.
    #[test]
    fn connection_cap_rejects_over_limit_and_releases_on_drop() {
        let count = Arc::new(AtomicUsize::new(0));
        let mut slots = Vec::new();
        for _ in 0..MAX_CONCURRENT_CONNECTIONS {
            let slot = try_take_connection_slot(&count).expect("under the cap");
            slots.push(slot);
        }
        assert!(
            try_take_connection_slot(&count).is_none(),
            "at-cap connection must be rejected"
        );
        drop(slots);
        assert_eq!(
            count.load(Ordering::Relaxed),
            0,
            "every slot must be released when its connection exits"
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
