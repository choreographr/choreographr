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

use super::{DaemonState, SessionMcpProject};
use crate::mcp::trust::McpTrustStore;
use crate::mcp::{McpReloadOutcome, McpStatusReport, McpTrustOutcome, SessionMcpOverlay};
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
        for session_id in affected {
            self.refresh_session_overlay(session_id);
        }
        info!(server = %slug, "MCP tool catalogue refreshed");
    }

    /// Rebuild the whole tool catalogue from the current `McpManager` —
    /// re-listing every connected server — and swap it into the shared
    /// registry.
    ///
    /// The command loop is the single writer of `tool_registry` (the sanctioned
    /// `ArcSwap` rule); this and [`DaemonState::rebuild_tool_catalogue_cached`]
    /// are the two places the swap happens. This full sweep is the
    /// reload/reconnect path, where the whole server set may have shifted (a
    /// server added, removed, reconnected, or renamed) so every server must be
    /// re-listed. A request already holding the previous registry keeps using
    /// it safely; the swap is atomic and never tears a live load.
    pub(super) fn rebuild_tool_catalogue(&mut self) {
        let registry = super::open::build_tool_registry(
            self.tool_policy,
            self.platform_tool_bridge.as_ref(),
            &mut self.mcp_manager,
        );
        self.tool_registry.store(registry);
    }

    /// Rebuild the whole tool catalogue from the current `McpManager`, reusing
    /// every server's CACHED tool set, and swap it into the shared registry.
    ///
    /// The list-change path's counterpart to
    /// [`DaemonState::rebuild_tool_catalogue`]: the caller re-listed exactly the
    /// one changed server first (`McpManager::refresh_server`), so this rebuild
    /// produces the identical catalogue without re-listing the unchanged
    /// servers. The swap is atomic and never tears a live load.
    pub(super) fn rebuild_tool_catalogue_cached(&mut self) {
        let registry = super::open::build_tool_registry_cached(
            self.tool_policy,
            self.platform_tool_bridge.as_ref(),
            &self.mcp_manager,
        );
        self.tool_registry.store(registry);
    }

    /// Reconnect one MCP server and refresh the catalogue.
    ///
    /// On success the whole catalogue is rebuilt (so the reconnected server's
    /// tools — whose sanitized names the manager may have disambiguated
    /// differently from a prior connection — are registered fresh). The outcome
    /// is reported back to the requester.
    pub(super) fn handle_mcp_reconnect(
        &mut self,
        slug: &str,
        reply: &std::sync::mpsc::Sender<Result<(), String>>,
    ) {
        let result = self.mcp_manager.reconnect(slug);
        match &result {
            Ok(()) => {
                self.rebuild_tool_catalogue();
                info!(server = %slug, "MCP server reconnected; tool catalogue refreshed");
            }
            Err(e) => {
                warn!(server = %slug, error = %e, "MCP reconnect failed");
            }
        }
        let _ = reply.send(result);
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
        let result = self.mcp_manager.reload();
        match &result {
            Ok(outcome) => {
                self.rebuild_tool_catalogue();
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
                for sid in &outcome.affected_sessions {
                    self.refresh_session_overlay(*sid);
                }
                info!(summary = %outcome.summary, "MCP config reloaded; tool catalogue refreshed");
            }
            Err(e) => {
                warn!(error = %e, "MCP config reload failed");
            }
        }
        let _ = reply.send(result);
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
        // TRUSTED project: `ensure_session` re-ensures, so pooled project-shared
        // connections are reused rather than torn down and rebuilt. Every other
        // case — leaving or entering a project, or a trust flip in EITHER
        // direction — must release the session's current refs first, so those go
        // through `reload_session` (release + ensure). A trust flip of the same
        // root is exactly such a case: the previous resolve had connected the
        // daemon-tier per-session servers under a key the new resolve no longer
        // wants, so re-using would leave them live instead of releasing them.
        let overlay = if Self::session_overlay_reuse(&previous, root.as_deref(), trusted) {
            self.mcp_manager
                .ensure_session(session_id, root.as_deref(), trusted)
        } else {
            self.mcp_manager
                .reload_session(session_id, root.as_deref(), trusted)
        };
        // Record the freshly-resolved project + trust so the NEXT change knows
        // what it left.
        self.session_mcp_projects
            .insert(session_id, SessionMcpProject { root, trusted });
        self.push_overlay_to_session(session_id, &overlay);
        overlay
    }

    /// Re-resolve ONE session's overlay WITHOUT releasing its existing
    /// connections, and push it to the session thread.
    ///
    /// Used on an MCP list change (a project/per-session server that adds or
    /// withdraws a tool changes the session's private overlay, so re-ensuring
    /// rather than `reload_session`'s release-then-ensure reuses its live
    /// connections while re-listing their tools) and after a daemon-tier
    /// reload that changed or removed a per-session server (whose stale slots
    /// the manager already dropped, so the re-ensure reconnects them with the
    /// new config).
    pub(super) fn refresh_session_overlay(&mut self, session_id: u64) {
        let (root, trusted) = self.session_project_root(session_id);
        let overlay = self
            .mcp_manager
            .ensure_session(session_id, root.as_deref(), trusted);
        self.session_mcp_projects
            .insert(session_id, SessionMcpProject { root, trusted });
        self.push_overlay_to_session(session_id, &overlay);
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
        report
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
            Ok(outcome) => {
                self.rebuild_tool_catalogue();
                // Re-resolve the overlays that held a daemon per-session server
                // this reload changed or removed.
                for sid in &outcome.affected_sessions {
                    self.refresh_session_overlay(*sid);
                }
                info!(summary = %outcome.summary, "MCP daemon-tier config reloaded (watch)");
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
