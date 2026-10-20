//! The command loop's half of a session's MCP overlay resolution: plan it from
//! the pool, hand its still-missing servers to a background worker, and install
//! what lands.
//!
//! A child module of `crate::daemon::mcp_commands`, so the `impl DaemonState`
//! block reaches the state's private fields directly, like its parent.
//!
//! Nothing here performs a handshake on the command loop: the overlay built from
//! the already-pooled connections is pushed immediately, and each worker result
//! arrives as its own [`DaemonCommand::McpSessionConnected`], so a fast server's
//! tools appear without waiting for a slow sibling.
//!
//! [`DaemonCommand::McpSessionConnected`]: crate::daemon::DaemonCommand::McpSessionConnected

#[cfg(feature = "mcp")]
use crate::daemon::DaemonCommand;
use crate::daemon::{DaemonState, SessionMcpProject};
#[cfg(feature = "mcp")]
use crate::mcp::PendingSessionConnect;
use crate::mcp::SessionMcpOverlay;
use crate::sessions::SessionCommand;
use std::collections::HashSet;
use std::path::Path;
#[cfg(feature = "mcp")]
use std::path::PathBuf;
use tracing::warn;
#[cfg(feature = "mcp")]
use tracing::{debug, info};

impl DaemonState {
    /// Whether a session's overlay may REUSE its live connections: it must
    /// still resolve to the same project root AND the same trust state it was
    /// last resolved for.
    ///
    /// A resolved overlay's server set is determined by `(project_root,
    /// trusted)`: a TRUSTED root contributes the project's servers plus the
    /// daemon per-session servers, an UNTRUSTED (or absent) root contributes
    /// only the daemon per-session servers — which do not depend on the root at
    /// all. Reuse is therefore correct whenever neither the root nor the trust
    /// state changed, INCLUDING the untrusted/absent case: reusing lets the plan
    /// fold the pooled daemon connections in instead of dropping and reconnecting
    /// them on every re-resolve.
    ///
    /// Leaving the root, or a trust flip in EITHER direction, must release
    /// instead: a revocation that reused would leave the project's connections
    /// live rather than releasing them.
    pub(crate) fn session_overlay_reuse(
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
    /// A further overlay with the newly connected tools is pushed as each entry
    /// lands, so a session-create/attach never blocks on a handshake.
    pub(crate) fn resolve_and_push_session_overlay(
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
        let reuse = Self::session_overlay_reuse(&previous, root.as_deref(), trusted);
        if !reuse {
            self.mcp_manager.release_session(session_id);
        }
        // A resolve that REUSES the session's connections keeps its generation:
        // a connect already in flight for it is still valid and installs on
        // arrival. A resolve that releases them (a root/trust change) bumps the
        // generation so a superseded result is dropped instead of resurrecting a
        // stale connection. `pending` likewise survives a reuse (so the entries a
        // worker is already connecting are not spawned twice) and is cleared when
        // the connections are released.
        let generation = if reuse {
            previous.generation
        } else {
            previous.generation.wrapping_add(1)
        };
        let pending = if reuse {
            previous.pending
        } else {
            HashSet::new()
        };

        #[cfg(feature = "mcp")]
        let (overlay, wanted, pending) = {
            let plan = self
                .mcp_manager
                .plan_session_overlay(session_id, root.as_deref(), trusted);
            self.push_overlay_to_session(session_id, &plan.partial);
            // Everything this resolve wants, pooled and pending alike: an
            // arriving connect is installed only while its entry is in here, so
            // one that lands after the config stopped declaring it is dropped.
            let wanted: HashSet<(Option<PathBuf>, String)> = plan
                .project
                .iter()
                .chain(plan.daemon.iter())
                .map(PendingSessionConnect::key)
                .collect();
            let project: Vec<PendingSessionConnect> = plan
                .project
                .into_iter()
                .filter(|entry| !pending.contains(&entry.key()))
                .collect();
            let daemon: Vec<PendingSessionConnect> = plan
                .daemon
                .into_iter()
                .filter(|entry| !pending.contains(&entry.key()))
                .collect();
            let mut pending = pending;
            pending.extend(
                project
                    .iter()
                    .chain(daemon.iter())
                    .map(PendingSessionConnect::key),
            );
            if !project.is_empty() || !daemon.is_empty() {
                self.spawn_session_overlay_connects(session_id, generation, project, daemon);
            }
            (plan.partial, wanted, pending)
        };
        #[cfg(not(feature = "mcp"))]
        let (overlay, wanted, pending) = {
            // Without the `mcp` feature there are no servers to connect: the
            // overlay is always empty and nothing is ever pending.
            let overlay = SessionMcpOverlay::empty();
            self.push_overlay_to_session(session_id, &overlay);
            (overlay, HashSet::new(), pending)
        };

        self.session_mcp_projects.insert(
            session_id,
            SessionMcpProject {
                root,
                trusted,
                generation,
                wanted,
                pending,
            },
        );
        overlay
    }

    /// Re-resolve each of `sessions`' overlays.
    ///
    /// No shared deadline is needed: every connect runs on a background worker,
    /// so re-resolving many sessions cannot stall the command loop. A session
    /// holding only reused (pooled) connections is a pure in-memory pass, and one
    /// whose connects are still in flight is not re-spawned.
    pub(super) fn refresh_session_overlays(&mut self, sessions: impl IntoIterator<Item = u64>) {
        for session_id in sessions {
            self.resolve_and_push_session_overlay(session_id, false);
        }
    }

    /// Spawn a worker that connects a session overlay's pending entries and
    /// hands each result back as its own [`DaemonCommand::McpSessionConnected`].
    ///
    /// Project-tier entries connect FIRST; a daemon per-session entry whose slug
    /// a project server CONNECTED is skipped (the connected project server
    /// overrides it). One worker per resolve keeps the batch ordered, while each
    /// entry reports as soon as it settles, so its tools land without waiting for
    /// a slow sibling's handshake.
    #[cfg(feature = "mcp")]
    fn spawn_session_overlay_connects(
        &mut self,
        session_id: u64,
        generation: u64,
        project: Vec<PendingSessionConnect>,
        daemon: Vec<PendingSessionConnect>,
    ) {
        let tx = self.daemon_tx.clone();
        let list_changes = self.mcp_manager.list_change_sender();
        let spawned = std::thread::Builder::new()
            .name(format!("mcp-session-{session_id}"))
            .spawn(move || {
                let mut succeeded: HashSet<String> = HashSet::new();
                for entry in project {
                    let result = Self::connect_overlay_entry(&entry, list_changes.clone());
                    if result.is_ok() {
                        succeeded.insert(entry.slug.clone());
                    }
                    if !Self::post_session_result(&tx, session_id, generation, entry, result) {
                        return;
                    }
                }
                for entry in daemon {
                    if succeeded.contains(&entry.slug) {
                        continue;
                    }
                    let result = Self::connect_overlay_entry(&entry, list_changes.clone());
                    if !Self::post_session_result(&tx, session_id, generation, entry, result) {
                        return;
                    }
                }
            });
        if let Err(e) = spawned {
            warn!(session_id, error = %e, "failed to spawn MCP session overlay worker");
        }
    }

    /// Connect one overlay entry, bounded by the background connect budget, and
    /// post its outcome to the command loop.
    ///
    /// Returns `false` when the command channel is gone (the daemon is shutting
    /// down), so the worker abandons the rest of the batch instead of connecting
    /// servers nobody will install.
    #[cfg(feature = "mcp")]
    fn post_session_result(
        tx: &crossbeam_channel::Sender<DaemonCommand>,
        session_id: u64,
        generation: u64,
        entry: PendingSessionConnect,
        result: crate::mcp::McpConnectOutcome,
    ) -> bool {
        let command = match result {
            Ok((server, tools)) => DaemonCommand::McpSessionConnected {
                session_id,
                generation,
                connected: vec![(entry, server, tools)],
                failed: Vec::new(),
            },
            Err(error) => DaemonCommand::McpSessionConnected {
                session_id,
                generation,
                connected: Vec::new(),
                failed: vec![(entry, error)],
            },
        };
        tx.send(command).is_ok()
    }

    /// Connect one overlay entry off the calling thread, bounded by
    /// [`crate::mcp::BACKGROUND_CONNECT_BUDGET`].
    #[cfg(feature = "mcp")]
    fn connect_overlay_entry(
        entry: &PendingSessionConnect,
        list_changes: crossbeam_channel::Sender<choreo_mcp::McpListChange>,
    ) -> crate::mcp::McpConnectOutcome {
        crate::mcp::connect_entry(
            &entry.config,
            Some(list_changes),
            crate::mcp::BACKGROUND_CONNECT_BUDGET,
        )
    }

    /// Install one background overlay connect result and push the rebuilt
    /// overlay.
    ///
    /// A result whose generation no longer matches the session's current resolve
    /// is DROPPED (its freshly connected server is dropped, shutting its
    /// dispatcher): the session was re-resolved against a different root or trust
    /// state, so installing it would resurrect a stale connection. A result whose
    /// entry the current plan no longer wants is dropped for the same reason (the
    /// config stopped declaring it while the connect ran).
    #[cfg(feature = "mcp")]
    pub(crate) fn handle_mcp_session_connected(
        &mut self,
        session_id: u64,
        generation: u64,
        connected: Vec<(
            PendingSessionConnect,
            choreo_mcp::McpServer,
            Vec<choreo_mcp::McpTool>,
        )>,
        failed: Vec<(PendingSessionConnect, String)>,
    ) {
        let Some((current_generation, wanted)) = self
            .session_mcp_projects
            .get(&session_id)
            .map(|state| (state.generation, state.wanted.clone()))
        else {
            debug!(
                session_id,
                "discarding MCP overlay result for a session that is gone"
            );
            return;
        };
        if current_generation != generation {
            debug!(
                session_id,
                generation, "discarding superseded MCP overlay result"
            );
            return;
        }
        // Whatever the outcome, these entries are no longer being connected:
        // clear their in-flight marks so a later resolve may retry a failure (and
        // will not skip a success, which is pooled from now on).
        for (entry, ..) in &connected {
            self.clear_session_pending(session_id, &entry.key());
        }
        for (entry, error) in failed {
            self.clear_session_pending(session_id, &entry.key());
            warn!(session_id, server = %entry.slug, error = %error, "MCP session server connect failed");
        }

        let mut installed = 0usize;
        for (entry, server, tools) in connected {
            if !wanted.contains(&entry.key()) {
                debug!(
                    session_id,
                    server = %entry.slug,
                    "discarding an MCP connection the session's plan no longer wants"
                );
                continue;
            }
            self.mcp_manager
                .install_session_connected(session_id, entry, server, &tools);
            installed += 1;
        }
        if installed == 0 {
            return;
        }
        // Rebuild from the now-larger pool and push it to the session thread.
        let (root, trusted) = self
            .session_mcp_projects
            .get(&session_id)
            .map_or((None, false), |state| (state.root.clone(), state.trusted));
        let overlay =
            self.mcp_manager
                .build_session_overlay_cached(session_id, root.as_deref(), trusted);
        self.push_overlay_to_session(session_id, &overlay);
        info!(
            session_id,
            installed, "MCP session overlay connection installed"
        );
    }

    /// Clear one overlay entry's in-flight mark for `session_id`.
    #[cfg(feature = "mcp")]
    fn clear_session_pending(&mut self, session_id: u64, key: &(Option<PathBuf>, String)) {
        if let Some(state) = self.session_mcp_projects.get_mut(&session_id) {
            state.pending.remove(key);
        }
    }

    /// Push a resolved overlay to the session thread (which stores it for the
    /// request/execution path). Best-effort: a session with no live thread is
    /// skipped with a warning.
    fn push_overlay_to_session(&self, session_id: u64, overlay: &SessionMcpOverlay) {
        if let Some(entry) = self.active_sessions.get(&session_id)
            && entry
                .cmd_tx
                .send(SessionCommand::SetMcpOverlay(Box::new(overlay.clone())))
                .is_err()
        {
            warn!(session_id, "failed to push MCP overlay to session");
        }
    }
}
