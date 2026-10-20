//! MCP command-loop handlers for [`DaemonState`] — the `/mcp` status, reconnect,
//! reload, and trust operations, the per-session overlay resolution/reuse, and
//! the tool-catalogue rebuild.
//!
//! These methods run on the daemon command loop (dispatched from the
//! `DaemonCommand` arms in `handle_command`) and are split out of `daemon.rs`
//! so that file stays navigable as it grows. This is a child module of
//! `crate::daemon`, so the `impl DaemonState` block below reaches the state's
//! private fields directly; the methods are `pub(super)` because the parent
//! `handle_command` and `daemon/tests.rs` are their only callers.

#[cfg(feature = "mcp")]
use super::DaemonCommand;
use super::{DaemonState, SessionMcpProject};
use crate::mcp::trust::McpTrustStore;
use crate::mcp::{
    McpReloadOutcome, McpServerStatus, McpStatusReport, McpTrustOutcome, SessionMcpOverlay,
};
use crate::sessions::SessionCommand;
use std::path::{Path, PathBuf};
use tracing::{debug, info, warn};

impl DaemonState {
    /// Rebuild and swap the tool catalogue after an MCP server reported a
    /// list change.
    ///
    /// The daemon command loop is the SOLE writer of `tool_registry` (the
    /// sanctioned `ArcSwap` single-writer rule): it reconstructs the whole
    /// registry (core tools + platform bridge + the current MCP tools) and
    /// stores it, so every session and request worker — which share the same
    /// `Arc<ArcSwap<…>>` — observes the new catalogue on its next load, without
    /// a daemon restart. A request already holding the previous registry keeps
    /// using it safely; the swap is atomic and never tears a live load.
    ///
    /// Only the server named by the event is re-listed
    /// (`McpManager::refresh_server`, bounded by the short catalogue-refresh
    /// deadline, not the per-server request timeout); every other server's
    /// cached tool set is reused. The rebuilt catalogue is therefore identical
    /// to a full sweep, but the command loop pays one round-trip per event
    /// instead of one per connected server. This runs on the command loop
    /// because the catalogue has a single writer, and a slow server must not
    /// freeze every session (a server that misses the deadline keeps its
    /// previous registration).
    ///
    /// A server can also be a session's PRIVATE project/per-session server,
    /// whose tools live only in that session's overlay and are NOT touched by
    /// the daemon-tier rebuild. The sessions holding a private server with this
    /// slug therefore have their overlay re-resolved too (re-ensured, not
    /// released, so their connections are reused).
    pub(super) fn handle_mcp_list_changed(&mut self, slug: &str, tools_changed: bool) {
        // A resources-only change needs no action: the resource catalogue is
        // read live by the wrapper tools, so there is nothing cached to
        // refresh. Only a TOOLS change can add or withdraw a registered tool.
        if !tools_changed {
            debug!(server = %slug, "MCP resource list changed; nothing to re-register");
            return;
        }
        info!(server = %slug, "MCP tool list changed; rebuilding the tool catalogue");
        self.mcp_manager.refresh_server(slug);
        self.rebuild_tool_catalogue_cached();
        let affected = self.mcp_manager.sessions_for_slug(slug);
        self.refresh_session_overlays(affected);
        info!(server = %slug, "MCP tool catalogue refreshed");
    }

    /// Rebuild the whole tool catalogue from the current `McpManager`, reusing
    /// every server's CACHED tool set, and swap it into the shared registry.
    ///
    /// The command loop is the single writer of `tool_registry` (the sanctioned
    /// `ArcSwap` rule); this is the one place the swap happens. Every MCP event —
    /// a background connect landing, a list change (whose one changed server the
    /// caller re-listed via `McpManager::refresh_server` first), a reload
    /// dropping a server — leaves the manager's cached tool sets current, so the
    /// rebuild produces the right catalogue without a single network round-trip
    /// on the command loop. The swap is atomic and never tears a live load: a
    /// request already holding the previous registry keeps using it safely while
    /// the next turn sees the new one.
    pub(super) fn rebuild_tool_catalogue_cached(&mut self) {
        let registry = super::open::build_tool_registry_cached(
            self.tool_policy,
            self.platform_tool_bridge.as_ref(),
            &self.mcp_manager,
        );
        self.tool_registry.store(registry);
    }

    /// Reconnect one MCP server and refresh the catalogue, reporting the
    /// outcome to the requester.
    ///
    /// The connect itself runs on a background worker (see
    /// [`Self::spawn_mcp_connect`]) so the command loop is never blocked on a
    /// handshake. The requester's reply is parked in `pending_mcp_reconnects`
    /// and satisfied by [`Self::handle_mcp_connected`] once the connect lands or
    /// fails, preserving the request/reply contract the connection thread is
    /// waiting on.
    pub(super) fn handle_mcp_reconnect(
        &mut self,
        slug: &str,
        reply: &std::sync::mpsc::Sender<Result<(), String>>,
    ) {
        // A daemon-tier server reconnects on a background worker: park the reply
        // and let `handle_mcp_connected` satisfy it when the connect lands, so
        // the command loop is never blocked on the handshake.
        if self.mcp_manager.is_configured(slug) {
            self.pending_mcp_reconnects
                .entry(slug.to_string())
                .or_default()
                .push(reply.clone());
            self.spawn_mcp_connect(slug);
            return;
        }
        // A project / per-session server has no daemon-tier config to background
        // connect: rebuild it in place (bounded, and only on this explicit
        // user action) and re-resolve the sessions holding it so the new tools
        // reach them. A slug matching nothing is an unknown-server error.
        let result = self.mcp_manager.reconnect(slug);
        if result.is_ok() {
            let affected = self.mcp_manager.sessions_for_slug(slug);
            self.refresh_session_overlays(affected);
        }
        match &result {
            Ok(()) => info!(server = %slug, "MCP server reconnected; tool catalogue refreshed"),
            Err(e) => warn!(server = %slug, error = %e, "MCP reconnect failed"),
        }
        let _ = reply.send(result);
    }

    /// Spawn a background connect for the daemon-tier server `slug`.
    ///
    /// No-op when `slug` is not a configured daemon-tier server or a connect is
    /// already in flight (the `in_flight` guard coalesces a duplicate unlock, a
    /// sign-in racing the startup sweep, or a repeated `/mcp reconnect`). The
    /// worker connects and lists the server, then hands the result back to the
    /// command loop as [`DaemonCommand::McpConnected`] — the loop is the sole
    /// writer of the pool and the tool catalogue, so the worker never touches
    /// either.
    ///
    /// [`DaemonCommand::McpConnected`]: super::DaemonCommand::McpConnected
    #[cfg(feature = "mcp")]
    pub(crate) fn spawn_mcp_connect(&mut self, slug: &str) -> bool {
        let Some((config, list_changes)) = self.mcp_manager.connect_inputs(slug) else {
            debug!(server = %slug, "no daemon-tier config for connect; nothing to spawn");
            return false;
        };
        if !self.mcp_manager.mark_in_flight(slug) {
            debug!(server = %slug, "MCP connect already in flight; not spawning a second");
            return false;
        }
        let tx = self.daemon_tx.clone();
        let worker_slug = slug.to_string();
        // A dedicated thread per connect: a cold `npx` handshake is multi-second
        // and must never occupy the command loop or the per-server worker that
        // spawned it. `join_with_budget` bounds the connect so a wedged server
        // cannot pin a thread forever; the detached inner thread's `McpServer`
        // is dropped (shutting its dispatcher) when it eventually returns.
        let spawned = std::thread::Builder::new()
            .name(format!("mcp-connect-{slug}"))
            .spawn(move || {
                let outcome = crate::mcp::join_with_budget(
                    std::thread::spawn(move || {
                        crate::mcp::connect_and_list(&config, Some(list_changes))
                            .map_err(|e| e.to_string())
                    }),
                    crate::mcp::BACKGROUND_CONNECT_BUDGET,
                )
                .unwrap_or_else(|| {
                    Err(format!(
                        "did not connect within {}s",
                        crate::mcp::BACKGROUND_CONNECT_BUDGET.as_secs()
                    ))
                });
                let _ = tx.send(DaemonCommand::McpConnected {
                    slug: worker_slug,
                    outcome,
                });
            });
        if let Err(e) = spawned {
            // The worker never started, so no `McpConnected` will arrive to
            // clear the in-flight marker or satisfy any waiter: do both here.
            warn!(server = %slug, error = %e, "failed to spawn MCP connect worker");
            self.mcp_manager.clear_in_flight(slug);
            self.mcp_manager
                .note_connect_failure(slug, format!("failed to spawn connect worker: {e}"));
            self.complete_pending_mcp_reconnect(slug);
            return false;
        }
        true
    }

    /// Without the `mcp` feature there are no servers to connect.
    #[cfg(not(feature = "mcp"))]
    pub(crate) fn spawn_mcp_connect(&mut self, _slug: &str) -> bool {
        false
    }

    /// Install (or record the failure of) a background connect's result, rebuild
    /// the tool catalogue from the cached sets, and satisfy any parked
    /// `/mcp reconnect` reply.
    ///
    /// Runs on the command loop, the sole writer of the tool catalogue: the
    /// swap is atomic and never tears a live load, so a request already holding
    /// the previous registry keeps using it while the next turn sees the server's
    /// tools.
    #[cfg(feature = "mcp")]
    pub(super) fn handle_mcp_connected(
        &mut self,
        slug: &str,
        outcome: crate::mcp::McpConnectOutcome,
    ) {
        match outcome {
            Ok((server, tools)) => {
                // A server that came up needs no post-unlock retry.
                self.mcp_manager.take_resync_request(slug);
                if let Err(e) = self.mcp_manager.install_connected(slug, server, &tools) {
                    // The server was removed from the config while the connect
                    // was in flight; the freshly connected `server` is dropped
                    // here (its dispatcher shut down) and the slug recorded as a
                    // failure so `/mcp` reports it.
                    self.mcp_manager.note_connect_failure(slug, e);
                } else {
                    self.rebuild_tool_catalogue_cached();
                    self.mcp_connects_completed = self.mcp_connects_completed.saturating_add(1);
                    let affected = self.mcp_manager.sessions_for_slug(slug);
                    self.refresh_session_overlays(affected);
                    info!(server = %slug, "MCP server connected; tool catalogue refreshed");
                }
                self.complete_pending_mcp_reconnect(slug);
            }
            Err(e) => {
                warn!(server = %slug, error = %e, "MCP background connect failed");
                self.mcp_manager.note_connect_failure(slug, e);
                // An unlock that landed while this connect was in flight could
                // not start its own retry (the in-flight guard skipped it). Now
                // that the credentials are live, retry ONCE so the OAuth server
                // actually comes up; the parked waiter sees the retry's result.
                if self.mcp_manager.take_resync_request(slug) {
                    info!(server = %slug, "retrying MCP connect after unlock");
                    self.spawn_mcp_connect(slug);
                    return;
                }
                self.complete_pending_mcp_reconnect(slug);
            }
        }
    }

    /// Satisfy every `/mcp reconnect` reply parked for `slug` with the current
    /// connected/failed state (called once a connect settles).
    #[cfg(feature = "mcp")]
    fn complete_pending_mcp_reconnect(&mut self, slug: &str) {
        let Some(waiters) = self.pending_mcp_reconnects.remove(slug) else {
            return;
        };
        let result = if self.mcp_manager.is_connected(slug) {
            Ok(())
        } else {
            Err(self
                .mcp_manager
                .failure(slug)
                .unwrap_or_else(|| format!("MCP server {slug:?} is not connected")))
        };
        for waiter in waiters {
            let _ = waiter.send(result.clone());
        }
    }

    /// Bring up the MCP server a just-stored OAuth grant belongs to.
    ///
    /// A completed sign-in stores the credential (see
    /// `DaemonState::handle_save_credential`) but does NOT, on its own, rebuild
    /// the connection that failed with an auth-required error before the grant
    /// existed — so the server would stay offline until the user ran
    /// `/mcp reconnect`. Reconnecting here (on a background worker, never the
    /// command loop) makes a sign-in self-sufficient.
    ///
    /// `credential_key` is the daemon's target-scoped `mcp:<slug>|<hash>` key;
    /// the slug it encodes is reconnected. A key that names no configured
    /// server (a stale grant, or a project server on an untrusted root) is a
    /// harmless no-op.
    #[cfg(feature = "mcp")]
    pub(super) fn reconnect_mcp_for_credential(&mut self, credential_key: &str) {
        let Some(slug) = choreo_mcp::slug_from_credential_key(credential_key) else {
            debug!(%credential_key, "stored credential is not an MCP key; nothing to reconnect");
            return;
        };
        self.spawn_mcp_connect(slug);
    }

    /// Bring up every MCP server whose OAuth grant just became available after
    /// an unlock (or bind).
    ///
    /// The daemon starts LOCKED and runs its background startup connects while
    /// the grant is still undecryptable, so an OAuth server's first connect
    /// 401s; this retries those servers once the credentials are live. ONLY the
    /// servers that are not already connected are retried — the ones whose
    /// first connect succeeded need no churn — and each retry runs on a
    /// background worker, so the unlock reply is never delayed by a handshake.
    ///
    /// Called from the unlock tail AFTER the acting client's `Unlocked` reply
    /// is enqueued (see `DaemonState::handle_unlock`), so the targeted reply and
    /// the lock-state broadcast always precede this work.
    #[cfg(feature = "mcp")]
    pub(super) fn schedule_mcp_resync_for_credentials(&mut self) {
        let slugs = self.mcp_resync_candidates();
        for slug in slugs {
            if !self.spawn_mcp_connect(&slug) {
                // A connect for this slug is already in flight (a startup or
                // sign-in connect that ran while locked). Remember to retry on
                // its failure, now that the credentials are live.
                self.mcp_manager.request_resync(&slug);
            }
        }
    }

    /// Without the `mcp` feature there are no MCP servers to bring up.
    #[cfg(not(feature = "mcp"))]
    pub(super) fn schedule_mcp_resync_for_credentials(&mut self) {}

    /// The daemon-tier servers a post-unlock resync should (re)connect: every
    /// configured server with an `mcp:` credential. Sorted and deduplicated so
    /// the spawn order is deterministic.
    #[cfg(feature = "mcp")]
    fn mcp_resync_candidates(&self) -> Vec<String> {
        let mut slugs: Vec<String> = self
            .credentials
            .keys()
            .filter(|key| choreo_mcp::is_mcp_credential_key(key))
            .filter_map(|key| choreo_mcp::slug_from_credential_key(key))
            .filter(|slug| self.mcp_manager.is_configured(slug))
            .map(str::to_string)
            .collect();
        slugs.sort();
        slugs.dedup();
        slugs
    }

    /// Reload the MCP configuration and refresh the catalogue.
    ///
    /// The manager re-reads the daemon-tier `mcp.json` and reconciles the
    /// running servers with it — connecting added servers, disconnecting removed
    /// ones, and rebuilding changed ones — then reconciles the active session's
    /// project `.mcp.json` (when one is attached). On success the
    /// whole catalogue is rebuilt (a reload can add, remove, or rename
    /// servers' tools), and the outcome is reported back to the requester. On
    /// a config read/parse failure nothing is changed and the error is
    /// reported.
    pub(super) fn handle_mcp_reload(
        &mut self,
        session_id: Option<u64>,
        reply: &std::sync::mpsc::Sender<Result<McpReloadOutcome, String>>,
    ) {
        match self.mcp_manager.reload() {
            Ok((outcome, to_connect)) => {
                // Rebuild from the CACHED tool sets (a dropped/changed server's
                // tools vanish; unchanged servers keep theirs) — never re-list
                // every server on the command loop. Each deferred connect
                // registers its own tools when it lands (`handle_mcp_connected`).
                self.rebuild_tool_catalogue_cached();
                for slug in &to_connect {
                    self.spawn_mcp_connect(slug);
                }
                // Reconcile the active session's project `.mcp.json` too (the
                // daemon-tier `mcp.json` was just reloaded above; `trust.toml`
                // is watcher-driven). Only the project file needs this explicit
                // nudge — per-session project roots are unbounded, so they are
                // never watched.
                if let Some(session_id) = session_id {
                    let _ = self.resolve_and_push_session_overlay(session_id, false);
                }
                // Re-resolve the overlays that held a daemon per-session server
                // this reload changed or removed, so they pick up the new
                // config (the manager already dropped their stale slots).
                self.refresh_session_overlays(outcome.affected_sessions.iter().copied());
                info!(
                    summary = %outcome.summary,
                    connecting = to_connect.len(),
                    "MCP config reloaded; reconnecting changed servers in background"
                );
                let _ = reply.send(Ok(outcome));
            }
            Err(e) => {
                warn!(error = %e, "MCP config reload failed");
                let _ = reply.send(Err(e));
            }
        }
    }

    /// Compute the project root + trust state for a session from its recorded
    /// working directory, returning `(root, trusted)`.
    pub(super) fn session_project_root(&self, session_id: u64) -> (Option<PathBuf>, bool) {
        let root = self
            .session_metadata
            .get(&session_id)
            .and_then(|m| m.working_dir.as_ref())
            .and_then(|wd| crate::mcp::project_root_for(Path::new(wd)));
        let trusted = root
            .as_deref()
            .is_some_and(|r| self.mcp_trust.is_trusted(r));
        (root, trusted)
    }

    /// Whether a session's overlay may REUSE its live connections: it must
    /// still resolve to the same project root AND the same trust state it was
    /// last resolved for.
    ///
    /// A resolved overlay's server set is determined by `(project_root,
    /// trusted)`: a TRUSTED root contributes the project's servers plus the
    /// daemon per-session servers, an UNTRUSTED (or absent) root contributes
    /// only the daemon per-session servers — which do not depend on the root at
    /// all. Reuse is therefore correct whenever neither the root nor the trust
    /// state changed, INCLUDING the untrusted/absent case: reusing lets
    /// `ensure_session` reuse the pooled daemon connections instead of dropping
    /// and reconnecting them on every re-resolve.
    ///
    /// Leaving the root, or a trust flip in EITHER direction, must release
    /// instead: a revocation that reused would leave the project's connections
    /// live rather than releasing them.
    pub(super) fn session_overlay_reuse(
        previous: &SessionMcpProject,
        root: Option<&Path>,
        trusted: bool,
    ) -> bool {
        let leaving = previous.root.as_deref() != root;
        let trust_flip = previous.trusted != trusted;
        !leaving && !trust_flip
    }

    /// Resolve a session's MCP overlay and push it to the session thread (which
    /// stores it for the request/execution path). Returns the overlay so a
    /// caller that needs it (the ensure handler) can reply with it directly.
    ///
    /// The resolve is CLOCK-FREE: the overlay built from the already-pooled
    /// connections is pushed immediately, and any servers still to connect are
    /// handed to a background worker (see [`Self::spawn_session_overlay_connects`]).
    /// A second overlay with the newly connected tools is pushed when the batch
    /// lands, so a session-create/attach never blocks on a handshake.
    pub(super) fn resolve_and_push_session_overlay(
        &mut self,
        session_id: u64,
        cancel_inflight: bool,
    ) -> SessionMcpOverlay {
        // The project this session's overlay was last resolved for, so a
        // working-directory change (or a trust flip) that LEAVES that project
        // can cancel exactly its servers' in-flight calls.
        let previous = self
            .session_mcp_projects
            .get(&session_id)
            .cloned()
            .unwrap_or_default();
        let (root, trusted) = self.session_project_root(session_id);
        // The change LEFT the previous project when the resolved root differs
        // (including entering or leaving the project tier entirely).
        let leaving = previous.root != root;
        if cancel_inflight || leaving {
            // Stop the session's in-flight calls to the project it is leaving or
            // revoking — and ONLY those. Its daemon-tier calls keep running: a
            // working-directory change must not disturb an unrelated in-flight
            // call.
            if let Some(old_root) = previous.root.as_deref() {
                self.mcp_manager
                    .cancel_session_project(session_id, old_root);
            }
        }
        // Reuse the live connections only when the session stays in the same
        // TRUSTED project: the plan folds pooled connections in and only
        // connects what is genuinely new. Every other case — leaving or
        // entering a project, or a trust flip in EITHER direction — must
        // release the session's current refs first. A trust flip of the same
        // root is exactly such a case: the previous resolve had connected the
        // daemon-tier per-session servers under a key the new resolve no longer
        // wants, so reusing would leave them live instead of releasing them.
        if !Self::session_overlay_reuse(&previous, root.as_deref(), trusted) {
            self.mcp_manager.release_session(session_id);
        }
        // A new resolve generation: any overlay connect still in flight for this
        // session carries the OLD generation and is dropped when it lands.
        let generation = previous.generation.wrapping_add(1);
        self.session_mcp_projects.insert(
            session_id,
            SessionMcpProject {
                root: root.clone(),
                trusted,
                generation,
            },
        );
        #[cfg(feature = "mcp")]
        let overlay = {
            let plan = self
                .mcp_manager
                .plan_session_overlay(session_id, root.as_deref(), trusted);
            self.push_overlay_to_session(session_id, &plan.partial);
            if !plan.project.is_empty() || !plan.daemon.is_empty() {
                self.spawn_session_overlay_connects(
                    session_id,
                    generation,
                    plan.project,
                    plan.daemon,
                );
            }
            plan.partial
        };
        #[cfg(not(feature = "mcp"))]
        let overlay = {
            // Without the `mcp` feature there are no servers to connect: the
            // overlay is always empty.
            let overlay = SessionMcpOverlay::empty();
            self.push_overlay_to_session(session_id, &overlay);
            overlay
        };
        let _ = generation;
        overlay
    }

    /// Re-resolve each of `sessions`' overlays.
    ///
    /// No shared deadline is needed any more: every connect runs on a background
    /// worker, so re-resolving many sessions cannot stall the command loop. A
    /// session holding only reused (pooled) connections is a pure in-memory
    /// pass.
    pub(super) fn refresh_session_overlays(&mut self, sessions: impl IntoIterator<Item = u64>) {
        for session_id in sessions {
            self.resolve_and_push_session_overlay(session_id, false);
        }
    }

    /// Spawn a worker that connects a session overlay's pending entries and
    /// hands the results back as [`DaemonCommand::McpSessionConnected`].
    ///
    /// Project-tier entries connect FIRST; a daemon per-session entry whose slug
    /// a project server CONNECTED is skipped (the connected project server
    /// overrides it). One worker per resolve keeps the batch ordered, and its
    /// results are installed only if the session's generation is unchanged when
    /// they land.
    ///
    /// [`DaemonCommand::McpSessionConnected`]: super::DaemonCommand::McpSessionConnected
    #[cfg(feature = "mcp")]
    fn spawn_session_overlay_connects(
        &mut self,
        session_id: u64,
        generation: u64,
        project: Vec<crate::mcp::PendingSessionConnect>,
        daemon: Vec<crate::mcp::PendingSessionConnect>,
    ) {
        let tx = self.daemon_tx.clone();
        let list_changes = self.mcp_manager.list_change_sender();
        let spawned = std::thread::Builder::new()
            .name(format!("mcp-session-{session_id}"))
            .spawn(move || {
                let mut connected = Vec::new();
                let mut failed = Vec::new();
                let mut succeeded: std::collections::HashSet<String> =
                    std::collections::HashSet::new();
                for entry in project {
                    match Self::connect_overlay_entry(&entry, list_changes.clone()) {
                        Ok((server, tools)) => {
                            succeeded.insert(entry.slug.clone());
                            connected.push((entry, server, tools));
                        }
                        Err(e) => failed.push((entry, e)),
                    }
                }
                for entry in daemon {
                    if succeeded.contains(&entry.slug) {
                        continue;
                    }
                    match Self::connect_overlay_entry(&entry, list_changes.clone()) {
                        Ok((server, tools)) => connected.push((entry, server, tools)),
                        Err(e) => failed.push((entry, e)),
                    }
                }
                let _ = tx.send(DaemonCommand::McpSessionConnected {
                    session_id,
                    generation,
                    connected,
                    failed,
                });
            });
        if let Err(e) = spawned {
            warn!(session_id, error = %e, "failed to spawn MCP session overlay worker");
        }
    }

    /// Connect one overlay entry, bounded by [`BACKGROUND_CONNECT_BUDGET`],
    /// returning the connected server plus its listed tools.
    ///
    /// [`BACKGROUND_CONNECT_BUDGET`]: crate::mcp::BACKGROUND_CONNECT_BUDGET
    #[cfg(feature = "mcp")]
    fn connect_overlay_entry(
        entry: &crate::mcp::PendingSessionConnect,
        list_changes: crossbeam_channel::Sender<choreo_mcp::McpListChange>,
    ) -> Result<(choreo_mcp::McpServer, Vec<choreo_mcp::McpTool>), String> {
        let config = entry.config.clone();
        crate::mcp::join_with_budget(
            std::thread::spawn(move || {
                crate::mcp::connect_and_list(&config, Some(list_changes)).map_err(|e| e.to_string())
            }),
            crate::mcp::BACKGROUND_CONNECT_BUDGET,
        )
        .unwrap_or_else(|| {
            Err(format!(
                "did not connect within {}s",
                crate::mcp::BACKGROUND_CONNECT_BUDGET.as_secs()
            ))
        })
    }

    /// Install a background session overlay batch and push the rebuilt overlay.
    ///
    /// A batch whose generation no longer matches the session's current resolve
    /// is DROPPED (its freshly connected servers are dropped, shutting their
    /// dispatchers): the session was re-resolved or released while it was
    /// connecting, so installing it would resurrect a stale connection.
    #[cfg(feature = "mcp")]
    pub(super) fn handle_mcp_session_connected(
        &mut self,
        session_id: u64,
        generation: u64,
        connected: Vec<(
            crate::mcp::PendingSessionConnect,
            choreo_mcp::McpServer,
            Vec<choreo_mcp::McpTool>,
        )>,
        failed: Vec<(crate::mcp::PendingSessionConnect, String)>,
    ) {
        let current = self
            .session_mcp_projects
            .get(&session_id)
            .map(|p| p.generation);
        if current != Some(generation) {
            debug!(
                session_id,
                generation, "dropping stale MCP session overlay batch"
            );
            return;
        }
        for (entry, server, tools) in connected {
            self.mcp_manager
                .install_session_connected(session_id, entry, server, &tools);
        }
        for (entry, error) in failed {
            warn!(session_id, server = %entry.slug, error = %error, "MCP session server connect failed");
        }
        // Rebuild from the now-complete pool and push it to the session thread.
        let (root, trusted) = self
            .session_mcp_projects
            .get(&session_id)
            .map_or((None, false), |p| (p.root.clone(), p.trusted));
        let overlay =
            self.mcp_manager
                .build_session_overlay_cached(session_id, root.as_deref(), trusted);
        self.push_overlay_to_session(session_id, &overlay);
        info!(session_id, "MCP session overlay connected; tools pushed");
    }

    /// Push a resolved overlay to the session thread (which stores it for the
    /// request/execution path). Best-effort: a session with no live thread is
    /// skipped with a warning.
    pub(super) fn push_overlay_to_session(&self, session_id: u64, overlay: &SessionMcpOverlay) {
        if let Some(entry) = self.active_sessions.get(&session_id)
            && entry
                .cmd_tx
                .send(SessionCommand::SetMcpOverlay(Box::new(overlay.clone())))
                .is_err()
        {
            warn!(session_id, "failed to push MCP overlay to session");
        }
    }

    /// Build a full MCP status report for a session (or the daemon tier only
    /// when `session_id` is `None`).
    pub(super) fn handle_mcp_status(&self, session_id: Option<u64>) -> McpStatusReport {
        let mut servers = self.mcp_manager.status();
        let mut report = McpStatusReport {
            servers: Vec::new(),
            project_root: None,
            project_trusted: false,
            ignored_project_servers: Vec::new(),
        };
        if let Some(session_id) = session_id {
            servers.extend(self.mcp_manager.session_status(session_id));
            let (root, trusted) = self.session_project_root(session_id);
            if let Some(root) = root {
                if !trusted {
                    report.ignored_project_servers = Self::untrusted_project_slugs(root.as_path());
                }
                report.project_root = Some(root);
                report.project_trusted = trusted;
            }
        }
        report.servers = servers;
        // Refine each server's auth state from the daemon's credential store
        // (the manager cannot see it): a stored grant is `OAuth`, a quarantined
        // key is `Reauthentication`, otherwise the baseline (`NotLoggedIn` for
        // an oauth server, `BearerToken`/`NotApplicable` otherwise).
        for server in &mut report.servers {
            server.auth = self.mcp_auth_state(server);
        }
        report
    }

    /// The auth state of one server, refined against the daemon's credential
    /// store. A server with no `oauth` block keeps its baseline (it is
    /// authenticated by a static header, or not at all).
    pub(super) fn mcp_auth_state(&self, status: &McpServerStatus) -> choreo_proto::McpAuthState {
        let Some(key) = status.credential_key.as_deref() else {
            return status.auth;
        };
        if status.oauth.is_none() {
            return status.auth;
        }
        if self.dead_oauth.contains(key) {
            choreo_proto::McpAuthState::Reauthentication
        } else if self.credentials.contains_key(key) {
            choreo_proto::McpAuthState::OAuth
        } else {
            choreo_proto::McpAuthState::NotLoggedIn
        }
    }

    /// The slugs an untrusted project root's `.mcp.json` declares (read so
    /// status can name what is being ignored; never spawned).
    #[cfg(feature = "mcp")]
    pub(super) fn untrusted_project_slugs(root: &Path) -> Vec<String> {
        match crate::mcp::config::load_project_config(root, false) {
            Ok(Some(entries)) => {
                let mut slugs: Vec<String> = entries.into_iter().map(|e| e.config.slug).collect();
                slugs.sort();
                slugs
            }
            _ => Vec::new(),
        }
    }

    /// Without the `mcp` feature no project file is ever read, so nothing is
    /// ever ignored.
    #[cfg(not(feature = "mcp"))]
    pub(super) fn untrusted_project_slugs(_root: &Path) -> Vec<String> {
        Vec::new()
    }

    /// Trust or untrust the active session's project root, then re-resolve the
    /// session's overlay and reply with the outcome.
    pub(super) fn handle_mcp_trust_set(
        &mut self,
        session_id: u64,
        trusted: bool,
    ) -> McpTrustOutcome {
        let (root, _) = self.session_project_root(session_id);
        let Some(root) = root else {
            return McpTrustOutcome {
                root: None,
                trusted: false,
                message: "no project root for this session's working directory".to_string(),
            };
        };
        let result = if trusted {
            self.mcp_trust.trust(&root)
        } else {
            self.mcp_trust.untrust(&root)
        };
        match result {
            Ok(canonical) => {
                // A trust flip changes which project servers are spawned: push
                // the fresh overlay to the session (releasing the old refs).
                let _ = self.resolve_and_push_session_overlay(session_id, !trusted);
                McpTrustOutcome {
                    root: Some(canonical.clone()),
                    trusted,
                    message: format!(
                        "{} project MCP root {}",
                        if trusted {
                            "trusted"
                        } else {
                            "revoked trust for"
                        },
                        canonical.display()
                    ),
                }
            }
            Err(e) => McpTrustOutcome {
                root: Some(root),
                trusted: false,
                message: format!("failed to update trust: {e}"),
            },
        }
    }

    /// Handle a daemon-tier `mcp.json` watcher event: re-read and reconcile the
    /// shared servers, then rebuild the catalogue.
    pub(super) fn handle_mcp_tier_reload(&mut self) {
        match self.mcp_manager.reload() {
            Ok((outcome, to_connect)) => {
                // Cached rebuild only: deferred connects register their tools as
                // they land, and an unchanged server keeps its cached tool set.
                self.rebuild_tool_catalogue_cached();
                for slug in &to_connect {
                    self.spawn_mcp_connect(slug);
                }
                // Re-resolve the overlays that held a daemon per-session server
                // this reload changed or removed.
                self.refresh_session_overlays(outcome.affected_sessions.iter().copied());
                info!(
                    summary = %outcome.summary,
                    connecting = to_connect.len(),
                    "MCP daemon-tier config reloaded (watch); reconnecting in background"
                );
            }
            Err(e) => warn!(error = %e, "MCP daemon-tier config reload failed"),
        }
    }

    /// Handle a `trust.toml` watcher event: re-read the trust store and
    /// re-resolve every active session's overlay (a trust flip changes which
    /// project servers are spawned).
    ///
    /// A save that does not change the trust set is a no-op: re-resolving
    /// every active session is expensive (it can connect servers), so it is
    /// gated on the sorted root lists actually differing.
    pub(super) fn handle_mcp_trust_reload(&mut self) {
        let before = self.mcp_trust.list();
        self.mcp_trust = McpTrustStore::load(self.mcp_trust.path().to_path_buf());
        let after = self.mcp_trust.list();
        let changed: Vec<PathBuf> = Self::trust_root_diff(&before, &after);
        if changed.is_empty() {
            info!("MCP trust store reloaded (watch); trust set unchanged, no re-resolve");
            return;
        }
        // Re-resolve only the sessions whose project root is one of the roots
        // whose trust actually flipped: re-resolving can connect servers, so a
        // change to one root must not disturb every unrelated session. Roots are
        // canonicalized on both sides (the store holds canonical paths; a
        // session's resolved root may be a symlinked spelling).
        let changed: std::collections::HashSet<PathBuf> = changed
            .iter()
            .map(|p| crate::mcp::trust::canonicalize_root(p))
            .collect();
        // A root that is no longer trusted is a REVOCATION: the sessions at it
        // must have their in-flight calls to that project's servers cancelled,
        // matching the command path (`/mcp untrust`). A root that became trusted
        // is a grant and cancels nothing.
        let revoked: std::collections::HashSet<PathBuf> = before
            .iter()
            .filter(|r| !after.contains(*r))
            .map(|p| crate::mcp::trust::canonicalize_root(p))
            .collect();
        let sessions: Vec<u64> = self.active_sessions.keys().copied().collect();
        let mut affected = 0usize;
        for session_id in sessions {
            let (root, _) = self.session_project_root(session_id);
            let canonical = root.as_deref().map(crate::mcp::trust::canonicalize_root);
            let matches = canonical.as_ref().is_some_and(|r| changed.contains(r));
            if matches {
                affected += 1;
                let cancel = canonical.as_ref().is_some_and(|r| revoked.contains(r));
                let _ = self.resolve_and_push_session_overlay(session_id, cancel);
            }
        }
        info!(
            changed_roots = changed.len(),
            affected_sessions = affected,
            "MCP trust store reloaded (watch)"
        );
    }

    /// The roots whose trust differs between `before` and `after` (the
    /// symmetric difference), order-insensitively. Used to scope the
    /// trust-reload re-resolve to the sessions a real change can affect.
    pub(super) fn trust_root_diff(before: &[PathBuf], after: &[PathBuf]) -> Vec<PathBuf> {
        let mut changed: Vec<PathBuf> = Vec::new();
        for root in before.iter().chain(after.iter()) {
            let in_both = before.contains(root) && after.contains(root);
            if !in_both && !changed.contains(root) {
                changed.push(root.clone());
            }
        }
        changed
    }
}
