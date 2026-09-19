//! Transport-independent daemon assembly ([`start_daemon_core`]).
//!
//! Step 2 of the embedded-daemon refactor: everything `run_server` used to do
//! that does NOT touch a transport listener (command channel wiring, ACL
//! install, config watchers, catalog maintenance, the shutdown flag, the
//! command-loop thread) now lives here and returns a [`DaemonCore`] bundle.
//! The transport adapters (Unix accept loop, TCP accept thread, signal
//! threads, metrics server, shutdown drain) remain in `lifecycle.rs` as
//! functions over `&DaemonCore` fields, so the shipped binary's behavior is
//! byte-for-byte what it was before the split.

use crate::daemon::{DaemonCommand, DaemonState};
use crate::sessions::SessionCommand;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize};
use std::sync::mpsc;
use std::thread;
use tracing::debug;
use tracing::info;
use tracing::warn;

/// Assembly options for [`start_daemon_core`].
pub(crate) struct CoreOptions {
    /// Shared ACL to install into [`DaemonState::acl`] and hot-reload-watch.
    ///
    /// `None` exists for the future embedded daemon, which may be assembled
    /// without a filesystem-backed ACL at all (or install/own it upstream of
    /// `start_daemon_core`). It is plumbed through and skipped wholesale so
    /// the embedded case needs no half-installed ACL state; the shipped
    /// binary always passes `Some` (see `run_server`).
    pub acl: Option<Arc<crate::server::acl::SharedAcl>>,
    /// Whether to spawn the config-directory watchers (overlay + accounts
    /// auto-reload). The embedded daemon may be assembled with watchers
    /// managed externally (or none at all), so the flag lets the adapter
    /// decide; the shipped binary passes `true`.
    pub config_watchers: bool,
    /// Auto-exit wake socket (`--auto-exit`): when `Some(path)`, the command
    /// loop, upon receiving [`DaemonCommand::LastClientDisconnected`] with a
    /// zero live-connection count, sets the shutdown flag and connects to
    /// `path` to unblock the accept loop — the exact SIGINT shutdown path.
    /// `None` when auto-exit is off, and for the embedded daemon (no socket
    /// to wake; auto-exit is a CLI-daemon feature only).
    pub auto_exit_wake_path: Option<String>,
}

/// Transport-independent daemon core assembled by [`start_daemon_core`]: the
/// pieces the transport adapters (accept loops, signal threads, metrics
/// server, shutdown drain) all share.
pub(crate) struct DaemonCore {
    /// Command channel to the daemon command loop. Adapters clone this per
    /// accepted connection; the shutdown drain sends the shutdown commands
    /// over it and then drops its own clone to close the loop.
    pub daemon_tx: mpsc::Sender<DaemonCommand>,
    /// Cooperative cancellation flag (thread-comm exception #1): every
    /// transport adapter — signal threads, accept loops, TCP accept thread,
    /// metrics server — polls this single-bit flag as a best-effort stop hint;
    /// all actual control flow still travels over channels/sockets.
    pub shutdown: Arc<AtomicBool>,
    /// Daemon-wide delivery-lag byte counter (exception #6). Cloned from
    /// [`DaemonState::global_lag`] so the accept paths hand the SAME counter
    /// to every connection's writer thread that the command loop and session
    /// threads increment on enqueue.
    pub global_lag: Arc<AtomicUsize>,
    /// Daemon-wide live-connection counter backing `MAX_CONCURRENT_CONNECTIONS`.
    /// Created here (not in the adapters) because BOTH accept paths take a
    /// slot per accepted connection — the cap must be enforced atomically
    /// across the Unix main thread and the TCP accept thread, so the Arc must
    /// exist before either adapter is spawned. See ARCHITECTURE.md
    /// (exception #3) for the lock-free rationale.
    pub conn_count: Arc<AtomicUsize>,
    /// The shared redb database handle. The transport adapters hand this
    /// `Arc` to each connection thread so on-demand image reads (`GetImage`)
    /// resolve on the connection's own thread — they never serialize on the
    /// command loop. redb's `Database` handle is explicitly designed for
    /// concurrent readers (each connection opens its own read transaction),
    /// so sharing one `Arc` across every connection thread is safe and needs
    /// no per-connection handle. Cloned from [`DaemonState::db`] BEFORE
    /// `state` moves into the command-loop thread (`start_daemon_core`).
    pub db: Arc<redb::Database>,
    /// `JoinHandle` of the command-loop thread; the shutdown drain joins it
    /// after sending `Shutdown` and dropping `daemon_tx`.
    pub cmd_handle: thread::JoinHandle<()>,
}

/// Dedicated forwarder thread for platform suspend/wake events: blocks on
/// the power monitor's crossbeam receiver (a dedicated thread blocking in
/// `recv()` is fine — the house rule only forbids blocking the command loop)
/// and translates each [`SuspendEvent`] into a [`DaemonCommand::PowerEvent`].
/// The command loop's policy (force-close on sleep, log on wake) lives in
/// `handle_suspend_event`; this thread is transport only. Exits when the
/// command channel closes (daemon shutting down); the power monitor's own
/// thread is daemon-like and reaps itself when its sender fails.
fn spawn_power_event_forwarder(
    daemon_tx: mpsc::Sender<DaemonCommand>,
    power_rx: crossbeam_channel::Receiver<choreo_power_events::SuspendEvent>,
) {
    let _ = thread::Builder::new()
        .name("power-events".into())
        .spawn(move || {
            for event in &power_rx {
                debug!(?event, "power event received; forwarding to command loop");
                if daemon_tx.send(DaemonCommand::PowerEvent(event)).is_err() {
                    info!("daemon command loop gone; stopping power-event forwarder");
                    break;
                }
            }
        });
}

/// Assemble the transport-independent daemon core: command channel, ACL
/// install + watcher, config watchers, catalog-maintenance thread, shutdown
/// flag, connection counter, and the command-loop thread.
///
/// Everything a transport adapter needs afterwards comes back in
/// [`DaemonCore`]; no listener is created or touched here, so the Unix and
/// (future) embedded daemon share this exact assembly.
// The assembly cannot fail: every fallible step degrades gracefully (no ACL
// dir → no hot-reload, no config dir → never-delivering receivers), so the
// function returns `DaemonCore` directly instead of a transparent Ok wrapper.
pub(crate) fn start_daemon_core(state: DaemonState, opts: CoreOptions) -> DaemonCore {
    let mut state = state;
    let (daemon_tx, daemon_rx) = mpsc::channel::<DaemonCommand>();
    state.daemon_tx = daemon_tx.clone();

    // Install the shared ACL into the state BEFORE the command loop takes
    // ownership: the command loop becomes its single WRITER (AclReload),
    // while the transport adapters keep a clone for lock-free reads. One Arc,
    // two roles — see the SharedAcl docs for the exception-#4 rationale.
    // Skipped entirely when no ACL was provided (the embedded-daemon case:
    // it owns its ACL policy upstream, and a half-installed ACL — present in
    // state but unwatched — would silently diverge from the shipped
    // binary's semantics).
    if let Some(acl) = opts.acl {
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
    }

    // Shared config-file watching transport: ONE notify watcher on the config
    // directory, fanned out per-basename to consumers (the catalog overlay,
    // accounts, and future files). Spawned before the consumers that react to
    // its events. Degrades gracefully to no transport (and no auto-reload)
    // when the config dir cannot be resolved, or when `config_watchers` is
    // false (the embedded-daemon case: watchers are managed externally or not
    // at all — the maintenance thread still runs, it just has no overlay
    // events to react to).
    let overlay_rx = match state.catalog_paths.overlay.parent().map(Path::to_path_buf) {
        _ if !opts.config_watchers => {
            warn!("config watchers disabled; config-file auto-reload disabled");
            // A never-delivering receiver keeps the maintenance thread running
            // (see the no-config-dir fallback for the same pattern).
            crossbeam_channel::never()
        }
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

    // Best-effort platform power monitoring: on Linux this subscribes to
    // logind's PrepareForSleep (a monitor thread inside choreo-power-events
    // owns the subscription); on any platform where the notification
    // mechanism is unavailable it degrades to an inert monitor that logs
    // once and never fires. Suspend/wake events reach the command loop via
    // `DaemonCommand::PowerEvent` — the same forwarder-into-command-channel
    // pattern the config/ACL watchers use — instead of a select! arm on the
    // command channel (a std mpsc receiver, which crossbeam's select! cannot
    // accept without converting every DaemonCommand sender).
    let power_monitor = choreo_power_events::PowerMonitor::best_effort();
    let power_rx = power_monitor.events().clone();
    spawn_power_event_forwarder(daemon_tx.clone(), power_rx);

    let shutdown = Arc::new(AtomicBool::new(false));

    // Clone the daemon-wide lag counter for the transport adapters BEFORE
    // `state` moves into the command-loop thread below: every connection's
    // writer thread gets a clone of the SAME counter the command loop and
    // session threads increment on enqueue (see DaemonCore::global_lag).
    let global_lag = Arc::clone(&state.global_lag);

    // Clone the shared DB handle for the transport adapters BEFORE `state`
    // moves into the command-loop thread: each connection thread reads the
    // `session_attachments` store directly (see DaemonCore::db).
    let db = Arc::clone(&state.db);

    // Daemon-wide live-connection counter backing MAX_CONCURRENT_CONNECTIONS.
    // Both accept paths take a slot per accepted connection, so the cap is
    // enforced across the Unix main thread and the TCP accept thread. Created
    // here so the shared Arc exists before any adapter is spawned.
    let conn_count = Arc::new(AtomicUsize::new(0));

    // Auto-exit state for the command loop: the decision must be made on ONE
    // thread (the command loop — same thread that owns every other shutdown
    // decision), so it needs its own clone of the connection counter, a clone
    // of the shutdown flag to set, and the wake path. `opts` moves into the
    // closure below, so extract these before the move.
    let auto_exit_wake_path = opts.auto_exit_wake_path.clone();
    let auto_exit_conn_count = Arc::clone(&conn_count);
    // The SAME flag the accept loop polls — setting a distinct flag would
    // never reach run_server's drain.
    let auto_exit_shutdown = Arc::clone(&shutdown);

    // Daemon command handler thread.
    let cmd_handle = thread::spawn(move || {
        loop {
            match daemon_rx.recv() {
                Ok(DaemonCommand::LastClientDisconnected) => {
                    // Auto-exit decision. A connection thread reports its
                    // disconnect AFTER releasing its slot, so a zero count
                    // here means NO client is connected anywhere (Unix or
                    // TCP). Anything else — an earlier disconnect while other
                    // clients remain, or a spurious delivery — is a no-op.
                    // There is deliberately no idle timer: a daemon that has
                    // never had a client runs forever.
                    //
                    // Known benign race: the zero-check and a NEW client's
                    // slot acquisition are not serialized (the counter is the
                    // sanctioned lock-free bookkeeping exception, not a
                    // mutex-guarded decision), so a client dialing in the
                    // window between "count hits 0" and "this arm reads 0"
                    // can have the daemon shut down underneath it — that
                    // client sees a bare EOF (or the notify-before-EOF
                    // `ShuttingDown`) and, for the TUI autostart flow,
                    // simply retries. Closing that window would require the
                    // accept path to take the decision mutex per connection,
                    // which the message-passing rules forbid for a bookkeeping
                    // counter; the tiny retry cost is the accepted trade-off.
                    if let Some(wake_path) = &auto_exit_wake_path
                        && auto_exit_conn_count.load(std::sync::atomic::Ordering::Relaxed) == 0
                    {
                        info!("auto-exit: last client disconnected; beginning graceful shutdown");
                        auto_exit_shutdown.store(true, std::sync::atomic::Ordering::SeqCst);
                        // Wake the blocking accept() with a self-connect so
                        // run_server observes the flag and runs its normal
                        // drain (BroadcastShuttingDown → Shutdown → unlink),
                        // exactly as on SIGINT. The dial lives in choreo-proto
                        // (the one cross-platform primitive); we only need it
                        // to land in the accept backlog, so the stream is
                        // dropped immediately.
                        let _ = choreo_proto::connect_unix(wake_path);
                    }
                }
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

    DaemonCore {
        daemon_tx,
        shutdown,
        global_lag,
        conn_count,
        db,
        cmd_handle,
    }
}
