use crate::daemon::{DaemonCommand, DaemonState};
use crate::server::core::{CoreOptions, start_daemon_core};

use choreo_transport::key::TransportSecretKey;
// The signal constants are consumed by the Unix iterator thread; the Windows
// flag thread imports them locally (signal-hook's iterator module is unix-only).
#[cfg(unix)]
use signal_hook::consts::{SIGINT, SIGTERM};
use std::io;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, TcpListener, TcpStream};
#[cfg(unix)]
use std::os::unix::net::UnixListener;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};
use tracing::{error, info, warn};
#[cfg(windows)]
use uds_windows::UnixListener;

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
pub(crate) const CONNECTION_DRAIN_GRACE: Duration = Duration::from_secs(5);

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
pub(crate) const MAX_CONCURRENT_CONNECTIONS: usize = 256;

/// RAII live-connection slot: decrements the daemon-wide connection counter
/// when a connection thread exits — including on panic — so a connection can
/// never leak its slot and slowly eat into the cap. Owns an `Arc` clone so it
/// can be moved into the spawned connection thread.
///
/// `pub(crate)`: the embedded daemon's `connect()` takes the same slot type
/// so `MAX_CONCURRENT_CONNECTIONS` applies uniformly across all three
/// transports.
pub(crate) struct ConnectionSlot(Arc<AtomicUsize>);

impl Drop for ConnectionSlot {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::Relaxed);
    }
}

/// Try to take a connection slot under [`MAX_CONCURRENT_CONNECTIONS`].
/// Atomic `fetch_add` makes the check-and-take race-free across the two
/// accept paths (Unix main thread + TCP accept thread); on rejection the
/// increment is undone and `None` is returned.
pub(crate) fn try_take_connection_slot(count: &Arc<AtomicUsize>) -> Option<ConnectionSlot> {
    if count.fetch_add(1, Ordering::Relaxed) >= MAX_CONCURRENT_CONNECTIONS {
        count.fetch_sub(1, Ordering::Relaxed);
        return None;
    }
    Some(ConnectionSlot(Arc::clone(count)))
}

/// Track a connection thread's `JoinHandle` for the shutdown drain, pruning
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
        crate::metrics::serve_metrics(addr, &shutdown_flag);
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

/// Handle an `accept()` error the way both accept loops do: a transient error
/// (interrupted syscall — `signal_hook` does not use `SA_RESTART` — or a
/// connection aborted before accept completed, which consumed no FD) is
/// retried immediately; a resource-exhaustion error (EMFILE/ENFILE/…) is
/// logged and backed off so other threads can close FDs. Both loops continue
/// after this, so it returns nothing.
fn handle_accept_error(e: &io::Error) {
    match e.kind() {
        io::ErrorKind::Interrupted | io::ErrorKind::ConnectionAborted => {}
        _ => {
            error!(error = %e, "accept error, retrying");
            thread::sleep(Duration::from_millis(100));
        }
    }
}

/// Refuse to delete a live daemon's socket; clean up a stale one.
///
/// When the socket path exists, probe it with [`choreo_proto::socket_listening`]
/// (the shared cross-platform dial): a SUCCESSFUL connect means a live daemon
/// is listening, so removing the file would orphan a working daemon — return
/// an actionable error instead. A failed connect (ENOENT, ECONNREFUSED, or a
/// regular file at the path) means nothing is listening: remove the leftover
/// and proceed (the same dial the CLIENT side uses for autostart — here ANY
/// failed connect is stale, because this path's action on "stale" is cleanup,
/// not a spawn). The connect has no timeout by design — a connect to a local
/// listener resolves immediately.
///
/// Honest limitation: probe-then-remove-then-bind is three separate syscalls,
/// not one atomic operation, so two daemons starting SIMULTANEOUSLY can both
/// pass the probe (the socket file may even be recreated between the probe
/// and the remove by the other starter). The guarantee is therefore
/// best-effort ordering, not exclusivity: the loser of such a race always
/// fails loudly at the bind (or at the removal error above), and the ordinary
/// autostart flow — a client dialing an existing socket — is fully covered,
/// since a live listener makes the probe connect succeed.
pub(crate) fn remove_stale_socket(socket_path: &str) -> io::Result<()> {
    if !Path::new(socket_path).exists() {
        return Ok(());
    }
    // Probe before removing: the path may be a LIVE daemon's socket, not a
    // stale leftover from a crash.
    if choreo_proto::socket_listening(socket_path) {
        return Err(io::Error::other(format!(
            "another daemon is already listening at {socket_path}; it must be \
             stopped before starting a new one"
        )));
    }
    // Stale: same removal as before, with the path carried in the error — a
    // bare "Permission denied (os error 13)" (the Termux /tmp failure mode)
    // with no hint WHICH path failed is undiagnosable.
    std::fs::remove_file(socket_path).map_err(|e| {
        io::Error::new(
            e.kind(),
            format!("removing the stale socket at {socket_path}: {e}"),
        )
    })
}

/// Serve both accept loops (Unix socket and optional TCP listener plus the
/// optional metrics server) until shutdown.
///
/// # Errors
///
/// Returns Err if the Unix socket path cannot be prepared/bound, the TCP
/// or metrics listener cannot be bound, or a fatal accept error occurs.
pub fn run_server(
    socket_path: &str,
    state: DaemonState,
    metrics_addr: Option<&String>,
    tcp_addr: Option<&String>,
    transport_sk: TransportSecretKey,
    acl: &std::sync::Arc<crate::server::acl::SharedAcl>,
    // `--auto-exit`: shut down gracefully when the last client disconnects.
    // Connection threads then report their disconnect to the command loop
    // (see `DaemonCommand::LastClientDisconnected`); with this off the send
    // in the connection-spawn closures below is skipped and behavior is
    // unchanged.
    auto_exit: bool,
) -> io::Result<()> {
    // ── Signal-handler registration ────────────────────────────────────────
    // Register the SIGINT/SIGTERM handler SYNCHRONOUSLY here, on the
    // `run_server` thread, BEFORE the socket is bound — hence before any
    // readiness a caller can observe (the socket file appearing, a TCP
    // connect succeeding). `Signals::new` installs the self-pipe handler as
    // a side effect of returning, and the pipe buffers any signal that
    // arrives before the consumer thread starts draining it — so the
    // guarantee is: once the socket exists at all, SIGINT is caught rather
    // than taking the kernel's default action and killing the process.
    //
    // This ordering is load-bearing. The previous shape spawned a thread
    // whose FIRST action was `Signals::new`, which runs only once the
    // scheduler reaches that thread: the accept loop (and the readiness a
    // client observes) could come up first, so a SIGINT landing in that
    // window hit an uninstalled handler and terminated the process by the
    // default action. Registering on the `run_server` thread before the bind
    // closes that window entirely.
    //
    // Failure to register is log-and-continue, not a startup error: the
    // daemon still serves, it just cannot be Ctrl+C'd (matching the old
    // behavior). The registered iterator is moved into its consumer thread
    // below; when registration fails the local is `None` and no thread is
    // spawned. An early `bind` failure returns with the local dropping,
    // which unregisters the handler (`signal_hook` unregisters on drop) —
    // i.e. a failed startup leaves no handler behind, as before.
    #[cfg(unix)]
    let signals: Option<signal_hook::iterator::Signals> =
        match signal_hook::iterator::Signals::new([SIGINT, SIGTERM]) {
            Ok(s) => Some(s),
            Err(e) => {
                error!("failed to register signal handlers: {e}");
                None
            }
        };
    // Windows: `low_level::register` installs the CRT console handler (the
    // same primitive `flag::register` is built on) synchronously and returns
    // once it is live; the channel receivers are moved into the consumer
    // thread below, which just blocks in `recv()`. Registering here — rather
    // than inside the spawned thread — closes the same startup race as the
    // Unix path above.
    #[cfg(windows)]
    let windows_signal_rx: Option<mpsc::Receiver<()>> = {
        use signal_hook::consts::{SIGINT, SIGTERM};
        let (sig_tx, sig_rx) = mpsc::channel::<()>();
        let int_tx = sig_tx.clone();
        let term_tx = sig_tx;
        // SAFETY: on Windows the registered action runs on the CRT's
        // console-handler thread, where an mpsc send is safe (no POSIX
        // async-signal restrictions apply); the senders are moved into the
        // registrations and outlive them.
        match unsafe {
            signal_hook::low_level::register(SIGINT, move || {
                let _ = int_tx.send(());
            })
        }
        .and_then(|_| unsafe {
            signal_hook::low_level::register(SIGTERM, move || {
                let _ = term_tx.send(());
            })
        }) {
            Ok(()) => Some(sig_rx),
            Err(e) => {
                error!("failed to register signal handlers: {e}");
                None
            }
        }
    };

    // Probe-then-remove, with the socket path carried in every error: a
    // bind/removal failure otherwise surfaces as a context-free "Permission
    // denied (os error 13)" (the Termux /tmp failure mode) with no hint WHICH
    // path was the problem. The probe refuses to unlink a socket a live
    // daemon is still listening on — a second daemon must not orphan the
    // first one.
    remove_stale_socket(socket_path)?;
    let listener = UnixListener::bind(socket_path).map_err(|e| {
        io::Error::new(
            e.kind(),
            format!("binding the Unix socket at {socket_path}: {e}"),
        )
    })?;
    info!(%socket_path, "choreographr listening");

    // Transport-independent assembly (command channel, ACL install +
    // watcher, config watchers, catalog-maintenance thread, shutdown flag,
    // connection counter, command-loop thread). The shipped binary always
    // passes the ACL and enables the config watchers; the Option/flag exist
    // for the future embedded daemon (see CoreOptions). Behavior is identical
    // to the pre-split inline assembly — the moved code is verbatim.
    let core = start_daemon_core(
        state,
        CoreOptions {
            acl: Some(Arc::clone(acl)),
            config_watchers: true,
            // Auto-exit only makes sense for a socket-listening daemon: the
            // decision arm wakes THIS socket's accept loop. The embedded
            // daemon passes `None` (no socket, no auto-exit).
            auto_exit_wake_path: auto_exit.then(|| socket_path.to_string()),
        },
    );
    // Local clone of the core's command sender: the accept paths below clone
    // per connection and the shutdown drain sends over this one, then drops
    // it to close the command loop. `core.daemon_tx` itself stays in `core`.
    let daemon_tx = core.daemon_tx.clone();
    // Local clones of the core's shared state for the adapter code below,
    // keeping the moved bodies verbatim (`shutdown`/`global_lag`/`conn_count`
    // names as before the split).
    let shutdown = Arc::clone(&core.shutdown);
    let global_lag = Arc::clone(&core.global_lag);
    let conn_count = Arc::clone(&core.conn_count);
    // The shared DB handle, cloned per connection below so each connection
    // thread reads on-demand images from its own redb handle instead of
    // round-tripping through the command loop (see DaemonCore::db).
    let db = Arc::clone(&core.db);
    // Socket write timeout for each accepted connection's writer thread
    // (`Duration` is `Copy`, so the accept closures below capture a copy
    // without disturbing this binding). See DaemonCore::writer_write_timeout.
    let writer_write_timeout = core.writer_write_timeout;

    // Signal-consumer thread: drains the handler ALREADY REGISTERED above,
    // setting the shutdown flag and connecting to our own socket to unblock
    // the blocking accept() call on the main thread.
    //
    // Unix: blocking iterator over the self-pipe. Registration happened
    // synchronously on this thread before the socket was bound; if it failed
    // (`None`) no consumer is spawned and the daemon runs without a handler.
    #[cfg(unix)]
    if let Some(mut signals) = signals {
        let sig_shutdown = Arc::clone(&shutdown);
        let sig_path = socket_path.to_string();
        thread::spawn(move || {
            for _ in signals.forever() {
                sig_shutdown.store(true, Ordering::SeqCst);
                // Wake the accept loop by connecting to our own socket.
                // The pending connection causes the next blocking accept()
                // to return immediately so the shutdown flag is checked.
                let _ = choreo_proto::connect_unix(&sig_path);
            }
        });
    }

    // Windows: the CRT console handler (installed synchronously above)
    // forwards each signal as a channel message; this thread blocks in
    // `recv()` with zero CPU instead of polling a flag, then wakes the accept
    // loop the same way as Unix (a connect to our own socket unblocks
    // accept()). The senders are held by the registrations, so recv never
    // returns Err and the loop runs until the process exits.
    #[cfg(windows)]
    if let Some(sig_rx) = windows_signal_rx {
        let sig_shutdown = Arc::clone(&shutdown);
        let sig_path = socket_path.to_string();
        thread::spawn(move || {
            while sig_rx.recv().is_ok() {
                sig_shutdown.store(true, Ordering::SeqCst);
                let _ = choreo_proto::connect_unix(&sig_path);
            }
        });
    }

    // (Cloned from the core above — see DaemonCore::global_lag for why the
    // SAME counter must reach the accept paths.)

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
    if let Some(addr_str) = metrics_addr {
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

    // Daemon-wide live-connection counter backing MAX_CONCURRENT_CONNECTIONS
    // (created in start_daemon_core — see DaemonCore::conn_count for why it
    // must exist before either accept path is spawned).

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
    if let Some(tcp_addr_str) = tcp_addr {
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
        let acl = Arc::clone(acl);
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
        // Same pattern for the DB handle: the main thread keeps its `db`
        // clone for the Unix accept path, the TCP accept thread takes its own
        // and hands a clone to each connection it spawns.
        let db_tcp = Arc::clone(&db);
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
                        // Auto-exit report sender: `tx` is consumed by the
                        // handshake/thread below, so the post-disconnect
                        // report needs its own clone (dropped untouched when
                        // auto-exit is off — an unused-but-captured clone is
                        // cheaper than branching the spawn plumbing).
                        let auto_exit_tx = if auto_exit { Some(tx.clone()) } else { None };
                        let sk_bytes = *transport_sk.as_bytes();
                        let acl = Arc::clone(&acl);
                        let global_lag = Arc::clone(&global_lag_tcp);
                        // Each TCP connection thread reads images from its own
                        // clone of the shared DB handle.
                        let db = Arc::clone(&db_tcp);
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
                            let conn_slot = slot;
                            // The preamble read + handshake-mode dispatch +
                            // responder handshake all live in
                            // tcp_handshake_and_client_thread (server/connection.rs)
                            // so the accept thread stays a pure spawn loop; it
                            // also unregisters the writer channel on every
                            // pre-transport failure path.
                            if let Err(e) =
                                crate::server::connection::tcp_handshake_and_client_thread(
                                    tcp,
                                    sk_bytes,
                                    &acl,
                                    tx,
                                    client_id,
                                    writer_tx,
                                    writer_rx,
                                    global_lag,
                                    db,
                                    writer_write_timeout,
                                )
                            {
                                error!(error = %e, "TCP client error");
                            }
                            // Auto-exit: the connection has fully ended and
                            // `conn_slot` is about to drop. Release it FIRST so
                            // the command loop's count is accurate when it
                            // processes the report, then send the event (see
                            // DaemonCommand::LastClientDisconnected).
                            drop(conn_slot);
                            if let Some(tx) = auto_exit_tx {
                                let _ = tx.send(DaemonCommand::LastClientDisconnected);
                            }
                        });
                        // Ferry the handle back to the main thread so shutdown
                        // can wait for this connection thread too.
                        let _ = tcp_client_tx.send(handle);
                    }
                    Err(e) => {
                        handle_accept_error(&e);
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
                // Auto-exit report sender: `tx` is consumed by client_thread,
                // so the post-disconnect report needs its own clone (see the
                // TCP path above for why the slot drop precedes the send).
                let auto_exit_tx = if auto_exit { Some(tx.clone()) } else { None };
                let global_lag = Arc::clone(&global_lag);
                // Each Unix connection thread reads images from its own clone
                // of the shared DB handle.
                let db = Arc::clone(&db);
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
                        let conn_slot = slot;
                        let result = crate::server::connection::client_thread(
                            stream,
                            tx,
                            client_id,
                            writer_tx,
                            writer_rx,
                            global_lag,
                            db,
                            writer_write_timeout,
                        );
                        if let Err(e) = result {
                            error!(error = %e, "client error");
                        }
                        // Auto-exit: release the slot BEFORE the report so the
                        // command loop's zero-check sees the true live count.
                        drop(conn_slot);
                        if let Some(tx) = auto_exit_tx {
                            let _ = tx.send(DaemonCommand::LastClientDisconnected);
                        }
                    }),
                );
            }
            Err(e) => {
                handle_accept_error(&e);
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
    core.cmd_handle.join().unwrap_or_else(|e| {
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
    /// `JoinHandle` per connection ever accepted; a still-running handle is
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

    // ── stale-socket probe (remove_stale_socket) ─────────────────────

    /// A regular file at the socket path (crash leftover after the file was
    /// clobbered, or simply garbage) is stale — connect fails, so the helper
    /// removes it and proceeds.
    #[test]
    fn remove_stale_socket_removes_regular_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sock");
        std::fs::write(&path, b"not a socket").unwrap();

        remove_stale_socket(path.to_str().unwrap()).unwrap();
        assert!(!path.exists(), "stale leftover must be removed");
    }

    /// A nonexistent path is trivially fine (fresh start) — no error, no
    /// creation.
    #[test]
    fn remove_stale_socket_ignores_missing_path() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("absent");
        remove_stale_socket(path.to_str().unwrap()).unwrap();
        assert!(!path.exists());
    }

    // The live-listener refusal case needs a real Unix socket at a real
    // path — that is a filesystem/IPC boundary test, so it lives in
    // tests/it/lifecycle_integration.rs (remove_stale_socket_refuses_live_listener).
}
