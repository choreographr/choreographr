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
use std::io;
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
    /// Daemon-wide live-connection counter backing MAX_CONCURRENT_CONNECTIONS.
    /// Created here (not in the adapters) because BOTH accept paths take a
    /// slot per accepted connection — the cap must be enforced atomically
    /// across the Unix main thread and the TCP accept thread, so the Arc must
    /// exist before either adapter is spawned. See ARCHITECTURE.md
    /// (exception #3) for the lock-free rationale.
    pub conn_count: Arc<AtomicUsize>,
    /// JoinHandle of the command-loop thread; the shutdown drain joins it
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
            for event in power_rx.iter() {
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
pub(crate) fn start_daemon_core(state: DaemonState, opts: CoreOptions) -> io::Result<DaemonCore> {
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

    // Daemon-wide live-connection counter backing MAX_CONCURRENT_CONNECTIONS.
    // Both accept paths take a slot per accepted connection, so the cap is
    // enforced across the Unix main thread and the TCP accept thread. Created
    // here so the shared Arc exists before any adapter is spawned.
    let conn_count = Arc::new(AtomicUsize::new(0));

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

    Ok(DaemonCore {
        daemon_tx,
        shutdown,
        global_lag,
        conn_count,
        cmd_handle,
    })
}
