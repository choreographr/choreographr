//! Transport-independent daemon assembly ([`start_daemon_core`]).
//!
//! Step 2 of the embedded-daemon refactor: everything `run_server` used to do
//! that does NOT touch a transport listener (command channel wiring, ACL
//! install, config watchers, catalog maintenance, the command-loop thread) now
//! lives here and returns a [`DaemonCore`] bundle. The transport adapters (Unix
//! accept loop, TCP accept thread, signal threads, metrics server, shutdown
//! drain) remain in `lifecycle.rs` as functions over `&DaemonCore` fields, so
//! the shipped binary's behavior is byte-for-byte what it was before the split.
//!
//! The shutdown flag and the trigger that sets it are adapter-owned
//! ([`CoreOptions::shutdown`] and [`CoreOptions::request_shutdown`]): only the
//! adapter knows how to wake its own blocking `accept()`, and every stop
//! request — a signal, a latched store handle, auto-exit — has to reach the
//! accept loop, not just the command loop. The core takes the flag and the
//! trigger as inputs and calls the trigger; it assembles no stop sequence of
//! its own.

use crate::daemon::{DaemonCommand, DaemonState};
use crate::db::Store;
use crate::sessions::SessionCommand;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize};
use std::thread;
use std::time::Duration;
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
    /// Whether the auto-exit decision is armed: on
    /// [`DaemonCommand::LastClientDisconnected`] with a zero live-connection
    /// count, the command loop asks for a shutdown through
    /// [`Self::request_shutdown`] — the exact SIGINT shutdown path. The
    /// CLI/TUI daemon passes `true` (its connection threads report
    /// disconnects and its accept loop can be woken); the embedded daemon
    /// passes `false`, because it has no accept loop to wake.
    pub auto_exit: bool,
    /// The flag the adapter's accept loops poll, and the one
    /// [`Self::request_shutdown`] sets. Created by the adapter so the trigger
    /// can capture it: one `Arc` shared by the trigger, the core, and the
    /// accept loops is what makes a stop request reach the blocking
    /// `accept()` at all.
    pub shutdown: Arc<AtomicBool>,
    /// The daemon's single shutdown trigger: set the flag and wake the
    /// blocking accept. Every stop request routes through it — a signal, a
    /// latched store handle, auto-exit — so all of them run the same drain.
    /// `Some` for the socket daemons; `None` for the embedded daemon, which
    /// has no accept loop and whose core therefore stops its own command
    /// loop.
    pub request_shutdown: Option<Arc<dyn Fn() + Send + Sync>>,
}

/// Transport-independent daemon core assembled by [`start_daemon_core`]: the
/// pieces the transport adapters (accept loops, signal threads, metrics
/// server, shutdown drain) all share.
pub(crate) struct DaemonCore {
    /// Command channel to the daemon command loop. Adapters clone this per
    /// accepted connection; the shutdown drain sends the shutdown commands
    /// over it and then drops its own clone to close the loop.
    pub daemon_tx: crossbeam_channel::Sender<DaemonCommand>,
    /// Cooperative cancellation flag (thread-comm exception #1): every
    /// transport adapter — signal threads, accept loops, TCP accept thread,
    /// metrics server — polls this single-bit flag as a best-effort stop hint;
    /// all actual control flow still travels over channels/sockets. Created by
    /// the adapter and handed in through [`CoreOptions::shutdown`], so the
    /// daemon's shutdown trigger ([`CoreOptions::request_shutdown`]) captures
    /// the very flag the accept loops poll.
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
    pub db: Arc<Store>,
    /// Socket write timeout handed to each accepted connection's writer thread
    /// (cloned from [`DaemonState::peer_stall`] before `state` moves
    /// into the command loop). Injectable so a wedged-writer test can use a
    /// tiny value instead of the 5 s production default.
    pub peer_stall: Duration,
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
    daemon_tx: crossbeam_channel::Sender<DaemonCommand>,
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

/// Dedicated forwarder for one config-dir file: blocks on the file's watcher
/// receiver, re-reads it on every change, and forwards the reload command only
/// when the content actually changed (a content fingerprint gate — a rewrite
/// with identical bytes is a no-op). The `last` view is seeded from the file's
/// current content at spawn, so the first genuine edit triggers exactly one
/// reload. Exits when the daemon command channel closes.
fn spawn_config_change_consumer(
    daemon_tx: crossbeam_channel::Sender<DaemonCommand>,
    rx: crossbeam_channel::Receiver<crate::config_watch::ConfigChange>,
    path: std::path::PathBuf,
    name: &str,
    make: fn() -> DaemonCommand,
) {
    let spawned = thread::Builder::new()
        .name(name.to_string())
        .spawn(move || {
            let mut last = std::fs::read(&path).ok();
            for change in &rx {
                let content = std::fs::read(&change.path).ok();
                if content == last {
                    continue;
                }
                last = content;
                if daemon_tx.send(make()).is_err() {
                    break;
                }
            }
        });
    if let Err(e) = spawned {
        warn!(error = %e, %name, "failed to spawn config-change consumer");
    }
}

/// The ACL file's basename, for a config-watcher subscription. Falls back to
/// the canonical `authorized_clients.toml` when the path has no final
/// component.
fn acl_basename(path: &Path) -> String {
    path.file_name().map_or_else(
        || "authorized_clients.toml".to_string(),
        |n| n.to_string_lossy().into_owned(),
    )
}

/// Watch an ACL file that lives OUTSIDE the shared config directory with its
/// own `notify` transport (the shared watcher cannot see a foreign directory).
/// A path with no parent disables ACL hot-reload with a warning; a directory
/// that does not exist yet is created and armed, and a creation failure is
/// retried by the transport's re-arm cadence.
fn spawn_dedicated_acl_watcher(
    daemon_tx: crossbeam_channel::Sender<DaemonCommand>,
    acl_path: &Path,
) {
    let Some(acl_dir) = acl_path.parent() else {
        warn!(
            path = %acl_path.display(),
            "ACL path has no parent directory; ACL hot-reload disabled"
        );
        return;
    };
    let mut acl_watcher = crate::config_watch::ConfigWatcher::new(acl_dir.to_path_buf());
    let acl_rx = acl_watcher.subscribe(&acl_basename(acl_path));
    acl_watcher.spawn();
    crate::server::acl::spawn_acl_watcher(daemon_tx, acl_rx);
}

/// Dedicated forwarder thread for MCP list-changed events: blocks on the
/// servers' shared list-change receiver (a dedicated thread blocking in
/// `recv()` is fine — the house rule only forbids blocking the command loop)
/// and translates each into a [`DaemonCommand::McpListChanged`]. The command
/// loop then rebuilds the tool catalogue (single writer of the shared
/// `ArcSwap`); this thread is transport only. Exits when every server's
/// subscription sender drops (daemon shutdown) or the command channel closes.
#[cfg(feature = "mcp")]
fn spawn_mcp_list_change_forwarder(
    state: &mut DaemonState,
    daemon_tx: crossbeam_channel::Sender<DaemonCommand>,
) {
    let Some(list_rx) = state.mcp_manager.take_list_change_rx() else {
        return;
    };
    let spawned = thread::Builder::new()
        .name("mcp-list-change".into())
        .spawn(move || {
            for change in &list_rx {
                debug!(server = %change.slug, ?change.kind, "MCP list change received; forwarding to command loop");
                if daemon_tx
                    .send(DaemonCommand::McpListChanged {
                        slug: change.slug,
                        tools_changed: change.kind == choreo_mcp::McpListKind::Tools,
                    })
                    .is_err()
                {
                    info!("daemon command loop gone; stopping MCP list-change forwarder");
                    break;
                }
            }
        });
    if let Err(e) = spawned {
        warn!(error = %e, "failed to spawn MCP list-change forwarder");
    }
}

#[cfg(feature = "mcp")]
fn spawn_mcp_startup_connects(state: &mut DaemonState) {
    // Drain the daemon-tier shared entries gathered at construction and start a
    // background connect for each. This is the whole of the daemon's MCP startup
    // cost: the connects run on worker threads that hand their results back to
    // the command loop as `DaemonCommand::McpConnected`, so the socket is bound
    // and the first client is served long before any handshake completes.
    let queue = state.mcp_manager.take_startup_queue();
    for entry in queue {
        let slug = entry.config.slug.clone();
        info!(
            server = %slug,
            transport = entry.config.transport.label(),
            target = entry.config.transport.target(),
            "scheduling MCP server connect (background)"
        );
        state.spawn_mcp_connect(&slug);
    }
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
pub(crate) fn start_daemon_core(state: DaemonState, opts: &CoreOptions) -> DaemonCore {
    let mut state = state;
    let (daemon_tx, daemon_rx) = crossbeam_channel::unbounded::<DaemonCommand>();
    state.daemon_tx = daemon_tx.clone();

    // Install the MCP OAuth runtime (the read snapshot + command sender the
    // connect path uses) now that the real command channel exists. Before this
    // the snapshot is absent, so an MCP server connects unauthenticated.
    #[cfg(feature = "mcp")]
    crate::mcp::oauth::install(daemon_tx.clone());

    // The daemon's single shutdown trigger (see `CoreOptions::request_shutdown`),
    // cloned once: the store escalation below and the command loop's auto-exit
    // arm both fire it.
    let request_shutdown = opts.request_shutdown.clone();

    // Route a store handle that latches a permanent I/O failure to a shutdown.
    // `redb` rejects every later operation on a latched handle and only a
    // close-and-reopen clears it, but every session thread holds a clone of this
    // one, so it cannot be re-opened in place. Once the handle is latched the
    // honest response is to stop — nothing can be persisted any more, and the
    // next launch re-opens (and repairs) the store.
    //
    // The stop has to go through the shutdown TRIGGER, not straight into the
    // command loop. Stopping the command loop alone leaves the accept loop
    // waiting for a flag nobody set: the process keeps running with the socket
    // bound and redb's exclusive file lock held, serving a store that cannot be
    // written. That is also what makes the latch unrecoverable — the next launch
    // finds a live listener at the socket path, `remove_stale_socket` refuses to
    // start over it, and the store stays unreachable. The trigger instead runs
    // the very drain SIGINT runs: set the flag, wake the blocking `accept()`, and
    // let the adapter broadcast `ShuttingDown` (so each client sees the
    // notification rather than a bare EOF), stop the sessions, and unlink the
    // socket. The store's own one-shot fence (see `db::store`) guarantees this
    // fires at most once however many sessions trip over the latched handle.
    state.db.set_escalation({
        let request_shutdown = request_shutdown.clone();
        // The embedded fallback: with no accept loop to wake, the command loop
        // is the thing that must stop.
        let daemon_tx = daemon_tx.clone();
        move || {
            if let Some(request) = &request_shutdown {
                request();
            } else {
                // No accept loop to wake: stop the command loop from inside. The
                // embedded host learns of the stop from the closed link;
                // surfacing the reason to its UI is a follow-up.
                let _ = daemon_tx.send(DaemonCommand::BroadcastShuttingDown);
                let _ = daemon_tx.send(DaemonCommand::Shutdown);
            }
        }
    });

    // Pump the MCP servers' list-changed events into the command loop. The
    // servers' `subscriptions/listen` streams feed one shared channel (see
    // `McpManager::take_list_change_rx`); a dedicated forwarder thread
    // translates each into a `DaemonCommand::McpListChanged`, keeping every
    // external event source on the same forwarder-into-command-channel shape
    // the config/ACL watchers and the power monitor use. Compiled out entirely
    // without the `mcp` feature (no manager, no channel).
    #[cfg(feature = "mcp")]
    spawn_mcp_list_change_forwarder(&mut state, daemon_tx.clone());

    // Kick off the daemon-tier shared-server connects on background workers,
    // now that the OAuth runtime snapshot (installed above) and the command
    // channel exist. This never blocks: every connect reports back through
    // `DaemonCommand::McpConnected`, which the command loop installs.
    #[cfg(feature = "mcp")]
    spawn_mcp_startup_connects(&mut state);

    // Install the shared ACL into the state BEFORE the command loop takes
    // ownership: the command loop becomes its single WRITER (AclReload),
    // while the transport adapters keep a clone for lock-free reads. One Arc,
    // two roles — see the SharedAcl docs for the exception-#4 rationale.
    // Skipped entirely when no ACL was provided (the embedded-daemon case:
    // it owns its ACL policy upstream, and a half-installed ACL — present in
    // state but unwatched — would silently diverge from the shipped
    // binary's semantics). The file itself is watched below, folded into the
    // shared config watcher when it lives in the config directory.
    let acl_path = opts.acl.as_ref().map(|acl| {
        let path = acl.path().to_path_buf();
        state.acl = Some(Arc::clone(acl));
        path
    });

    // Shared config-file watching transport: ONE notify watcher on the config
    // directory, fanned out per-basename to consumers (the catalog overlay,
    // accounts, the MCP trust store, and — under the `mcp` feature — the
    // daemon-tier config). Each consumer owns its reload policy; the transport
    // only routes events. Degrades gracefully to a never-delivering receiver
    // (and no auto-reload) when the config dir cannot be resolved or when
    // `config_watchers` is false (the embedded-daemon case: watchers are
    // managed externally or not at all — the maintenance thread still runs, it
    // just has no overlay events to react to).
    //
    // The ACL file joins this transport when it lives in the config directory
    // (the production layout), so the whole directory is watched by ONE
    // `notify` instance rather than two; a dedicated watcher covers the case
    // where the ACL is resolved elsewhere (see below).
    let config_dir = state.catalog_paths.overlay.parent().map(Path::to_path_buf);
    let mut config_watcher = match (config_dir.as_ref(), opts.config_watchers) {
        (Some(dir), true) => Some(crate::config_watch::ConfigWatcher::new(dir.clone())),
        (Some(_), false) => {
            warn!("config watchers disabled; config-file auto-reload disabled");
            None
        }
        (None, _) => {
            warn!("config directory not resolvable; config-file auto-reload disabled");
            None
        }
    };

    let overlay_rx = match (config_watcher.as_mut(), config_dir.as_ref()) {
        (Some(watcher), Some(config_dir)) => {
            // The catalog maintenance thread reacts to overlay edits; the
            // accounts watcher reacts to accounts.toml edits. Each consumer
            // owns its reload policy (see `handle_accounts_reload` and the
            // maintenance loop's overlay arm).
            let overlay_rx = watcher.subscribe(crate::catalog::USER_OVERLAY_NAME);
            let accounts_rx = watcher.subscribe(crate::accounts::ACCOUNTS_TOML_NAME);
            // The MCP trust store is always watched (it compiles without the
            // `mcp` feature): an edit re-resolves every session's overlay. The
            // daemon-tier `mcp.json` is watched only when the `mcp` feature is
            // built in (there is no manager without it). Per-session PROJECT
            // `.mcp.json` files are deliberately NOT watched — their roots are
            // unbounded, so they reload via `/mcp reload`.
            let trust_rx = watcher.subscribe(crate::mcp::TRUST_FILE);
            #[cfg(feature = "mcp")]
            let mcp_rx = watcher.subscribe(crate::mcp::config::DAEMON_CONFIG_FILE);

            crate::accounts::spawn_accounts_watcher(daemon_tx.clone(), accounts_rx);
            spawn_config_change_consumer(
                daemon_tx.clone(),
                trust_rx,
                config_dir.join(crate::mcp::TRUST_FILE),
                "mcp-trust-watch",
                || DaemonCommand::McpTrustReload,
            );
            #[cfg(feature = "mcp")]
            spawn_config_change_consumer(
                daemon_tx.clone(),
                mcp_rx,
                config_dir.join(crate::mcp::config::DAEMON_CONFIG_FILE),
                "mcp-config-watch",
                || DaemonCommand::McpTierReload,
            );
            overlay_rx
        }
        // A never-delivering receiver keeps the maintenance thread running (it
        // just has no overlay events to react to).
        _ => crossbeam_channel::never(),
    };

    // ACL hot-reload. Reuse the shared config watcher when it watches the ACL's
    // own directory (the production layout); otherwise — an ACL resolved
    // outside the config dir (the `--base-dir` layout keeps it at the base
    // root, not under `<base>/config/choreographr`), or config watching
    // disabled/unresolved — fall back to a dedicated watcher so ACL hot-reload
    // never depends on where the catalog overlay happens to live. The basename
    // subscription keeps unrelated files in the directory from triggering
    // reloads.
    if let Some(acl_path) = acl_path.as_deref() {
        let shared_acl_rx = match (config_watcher.as_mut(), config_dir.as_deref()) {
            (Some(watcher), Some(config_dir)) if acl_path.parent() == Some(config_dir) => {
                Some(watcher.subscribe(&acl_basename(acl_path)))
            }
            _ => None,
        };
        match shared_acl_rx {
            Some(acl_rx) => crate::server::acl::spawn_acl_watcher(daemon_tx.clone(), acl_rx),
            None => spawn_dedicated_acl_watcher(daemon_tx.clone(), acl_path),
        }
    }

    // Arm the shared watcher only now that every subscription — the config
    // basenames and any shared ACL basename — is registered, so no event can
    // slip past an unregistered consumer.
    if let Some(watcher) = config_watcher {
        watcher.spawn();
    }

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
    // `DaemonCommand::PowerEvent` through the same forwarder-into-command-
    // channel pattern the config/ACL watchers use, keeping every external
    // event source on one uniform delivery shape rather than special-casing
    // the command loop with an extra `select!` arm.
    let power_monitor = choreo_power_events::PowerMonitor::best_effort();
    let power_rx = power_monitor.events().clone();
    spawn_power_event_forwarder(daemon_tx.clone(), power_rx);

    // The SAME flag the transport adapters poll: it is created by the adapter
    // and handed in through `opts`, so the shutdown trigger — which captures its
    // own clone — sets exactly the flag the accept loops check. A distinct flag
    // here would never reach the drain.
    let shutdown = Arc::clone(&opts.shutdown);

    // Clone the daemon-wide lag counter for the transport adapters BEFORE
    // `state` moves into the command-loop thread below: every connection's
    // writer thread gets a clone of the SAME counter the command loop and
    // session threads increment on enqueue (see DaemonCore::global_lag).
    let global_lag = Arc::clone(&state.global_lag);

    // Clone the shared DB handle for the transport adapters BEFORE `state`
    // moves into the command-loop thread: each connection thread reads the
    // `session_attachments` store directly (see DaemonCore::db).
    let db = Arc::clone(&state.db);

    // Connection writer write timeout (see DaemonCore::peer_stall);
    // `Duration` is `Copy`, so reading it before the move is trivial.
    let peer_stall = state.peer_stall;

    // Daemon-wide live-connection counter backing MAX_CONCURRENT_CONNECTIONS.
    // Both accept paths take a slot per accepted connection, so the cap is
    // enforced across the Unix main thread and the TCP accept thread. Created
    // here so the shared Arc exists before any adapter is spawned.
    let conn_count = Arc::new(AtomicUsize::new(0));

    // Auto-exit state for the command loop: the decision must be made on ONE
    // thread (the command loop — same thread that owns every other shutdown
    // decision), so it needs its own clone of the connection counter and a clone
    // of the shutdown trigger to fire. `opts` is a borrow and the closure below
    // is `move` and `'static`, so every value it needs is extracted before the
    // move.
    let auto_exit = opts.auto_exit;
    let auto_exit_conn_count = Arc::clone(&conn_count);
    let auto_exit_request_shutdown = request_shutdown.clone();

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
                    if auto_exit
                        && auto_exit_conn_count.load(std::sync::atomic::Ordering::Relaxed) == 0
                    {
                        info!("auto-exit: last client disconnected; beginning graceful shutdown");
                        // Same trigger a latched store and the signal path use:
                        // set the flag and wake the accept loop, which then runs
                        // the normal drain (BroadcastShuttingDown → Shutdown →
                        // session joins → socket unlink).
                        if let Some(request) = &auto_exit_request_shutdown {
                            request();
                        } else {
                            // Embedded: no accept loop exists to wake, so the
                            // command loop is the thing that stops.
                            break;
                        }
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
                Err(crossbeam_channel::RecvError) => {
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
        peer_stall,
        cmd_handle,
    }
}

#[cfg(test)]
mod tests {
    use super::acl_basename;
    use std::path::Path;

    #[test]
    fn acl_basename_uses_the_final_component_or_a_fallback() {
        assert_eq!(
            acl_basename(Path::new("/etc/choreographr/authorized_clients.toml")),
            "authorized_clients.toml"
        );
        // A path with no final component falls back to the canonical name.
        assert_eq!(acl_basename(Path::new("/")), "authorized_clients.toml");
        assert_eq!(acl_basename(Path::new("")), "authorized_clients.toml");
    }
}
