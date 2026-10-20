//! MCP command-loop handlers for [`DaemonState`] — the `/mcp` status, reload,
//! trust, and list-change operations plus the tool-catalogue rebuild.
//!
//! These methods run on the daemon command loop (dispatched from the
//! `DaemonCommand` arms in `handle_command`). This is a child module of
//! `crate::daemon`, so the `impl DaemonState` block below reaches the state's
//! private fields directly; the methods are `pub(super)` because the parent
//! `daemon` module and its `daemon/tests.rs` are their only callers.
//!
//! The handlers are split by concern, each a child module that keeps the same
//! private-field access:
//!
//! - `reconnect` — bringing one server up (background connect, its reply
//!   parking, the post-unlock/post-sign-in resync, and the in-place rebuild of a
//!   project/per-session server).
//! - `overlay` — planning a session's overlay, spawning its connect worker, and
//!   installing what lands.
//!
//! What stays here is the catalogue rebuild every MCP event funnels into, the
//! config reloads, and the status/trust surface.

mod overlay;
mod reconnect;

use super::DaemonState;
use crate::mcp::trust::McpTrustStore;
use crate::mcp::{McpReloadOutcome, McpServerStatus, McpStatusReport, McpTrustOutcome};
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
    /// instead of one per connected server.
    ///
    /// A server can also be a session's PRIVATE project/per-session server,
    /// whose tools live only in that session's overlay and are NOT touched by
    /// the daemon-tier rebuild. The sessions holding a private server with this
    /// slug therefore have their overlay re-resolved too (planned against the
    /// pool, so their connections are reused rather than rebuilt).
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

    /// Reload the MCP configuration and refresh the catalogue.
    ///
    /// The manager re-reads the daemon-tier `mcp.json` and reconciles the
    /// running servers with it — dropping the slots of removed/changed servers
    /// and reporting the shared ones whose connect the caller must spawn — then
    /// reconciles the active session's project `.mcp.json` (when one is
    /// attached). On success the catalogue is rebuilt from cache (a reload can
    /// remove or rename servers' tools; the deferred connects register theirs as
    /// they land), and the outcome is reported back to the requester. On a
    /// config read/parse failure nothing is changed and the error is reported.
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
