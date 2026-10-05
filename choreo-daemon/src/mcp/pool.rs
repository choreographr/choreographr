//! Reconciliation of the live MCP connection pool with the configured server
//! set: the user-initiated reconnect and the daemon-tier config reload, plus
//! the shared connect helper they and the overlay resolution build slots
//! through.
//!
//! Everything here drives a live `McpManager` connection, so the module body is
//! `mcp`-feature-gated like the manager itself. The `impl` block lives here
//! rather than in `mod.rs` so it keeps the manager's private fields and helpers
//! in scope as a child module.

#[cfg(feature = "mcp")]
use super::config::{self, McpEntry};
#[cfg(feature = "mcp")]
use super::{
    McpReloadOutcome, RECONNECT_BUDGET, RELOAD_BUDGET, ServerSlot, SharedSlot, connect_and_list,
    join_with_budget,
};
#[cfg(feature = "mcp")]
use choreo_mcp::{McpListChange, McpServer, McpServerConfig, McpTool};
#[cfg(feature = "mcp")]
use std::collections::{HashMap, HashSet};
#[cfg(feature = "mcp")]
use std::path::PathBuf;
#[cfg(feature = "mcp")]
use std::time::Duration;
#[cfg(feature = "mcp")]
use tracing::{info, warn};

#[cfg(feature = "mcp")]
impl super::McpManager {
    /// Rebuild the connection(s) to `slug`, re-registering their tools.
    ///
    /// A slug can name a daemon-tier shared server, one or more project-shared
    /// connections, and/or one or more per-session connections (a daemon
    /// `shared = false` server has a per-session slot with `root = None`; a
    /// project `shared = false` server has one with `root = Some(..)`). Every
    /// matching connection is rebuilt in place: a failed rebuild is collected,
    /// not fatal, so one bad connection does not skip the rest.
    ///
    /// # Errors
    ///
    /// Returns a message when `slug` matches nothing, or when every matching
    /// connection failed to rebuild.
    pub fn reconnect(&mut self, slug: &str) -> Result<(), String> {
        let mut reconnected = 0usize;
        let mut errors: Vec<String> = Vec::new();

        // Daemon-tier shared server (at most one, keyed by slug).
        if let Some(config) = self
            .configs
            .get(slug)
            .filter(|e| e.shared)
            .map(|e| e.config.clone())
        {
            self.servers.remove(slug);
            if let Some(slot) = Self::connect_slot(&self.list_change_tx, &config, RECONNECT_BUDGET)
            {
                self.servers.insert(slug.to_string(), slot);
                self.failures.remove(slug);
                info!(server = %slug, "reconnected MCP server");
                reconnected += 1;
            } else {
                let msg = format!("reconnect to {slug:?} failed");
                self.failures.insert(slug.to_string(), msg.clone());
                errors.push(msg);
            }
        }

        // Project-shared connections (best-effort, by slug). Rebuild EVERY
        // match rather than bailing on the first failure, so a shared server
        // referenced from several projects is fully refreshed.
        let matching_shared: Vec<(PathBuf, String)> = self
            .project_shared
            .keys()
            .filter(|(_, s)| s == slug)
            .cloned()
            .collect();
        for key in matching_shared {
            if let Some(shared) = self.project_shared.get(&key) {
                let config = shared.slot.config.clone();
                let sessions = shared.sessions.clone();
                if let Some(slot) =
                    Self::connect_slot(&self.list_change_tx, &config, RECONNECT_BUDGET)
                {
                    self.project_shared
                        .insert(key.clone(), SharedSlot { slot, sessions });
                    info!(server = %slug, root = %key.0.display(), "reconnected shared project MCP server");
                    reconnected += 1;
                } else {
                    errors.push(format!(
                        "reconnect to {slug:?} failed for project root {}",
                        key.0.display()
                    ));
                }
            }
        }

        // Per-session connections (daemon `shared = false`, and project
        // `shared = false`), each rebuilt in place from its own config.
        let matching_session: Vec<(u64, Option<PathBuf>, String)> = self
            .session_slots
            .keys()
            .filter(|(_, _, s)| s == slug)
            .cloned()
            .collect();
        for key in matching_session {
            if let Some(slot) = self.session_slots.get(&key) {
                let config = slot.config.clone();
                if let Some(new_slot) =
                    Self::connect_slot(&self.list_change_tx, &config, RECONNECT_BUDGET)
                {
                    self.session_slots.insert(key.clone(), new_slot);
                    info!(
                        server = %slug,
                        session_id = key.0,
                        root = ?key.1,
                        "reconnected per-session MCP server"
                    );
                    reconnected += 1;
                } else {
                    errors.push(format!(
                        "reconnect to {slug:?} failed for session {}",
                        key.0
                    ));
                }
            }
        }

        if reconnected == 0 {
            if errors.is_empty() {
                return Err(format!("unknown MCP server {slug:?}"));
            }
            return Err(errors.join("; "));
        }
        Ok(())
    }

    /// Re-read the daemon-tier `mcp.json` and reconcile the daemon-tier shared
    /// server set with it. Project-tier servers are reconciled per session
    /// (see [`super::McpManager::reload_session`]).
    ///
    /// Daemon-tier `shared = false` servers are per-session, so their
    /// reconciliation drops the stale per-session connections of a server
    /// whose config changed or that was removed (a stale slot would otherwise
    /// be returned by `ensure_session_slot` forever); the sessions that held
    /// them are reported in the outcome so the daemon re-resolves their
    /// overlays.
    ///
    /// # Errors
    ///
    /// Returns an error when the config cannot be read or parsed. A server
    /// that fails to (re)connect is recorded in `failures` (and appears in the
    /// outcome's status list), not as a hard error.
    pub fn reload(&mut self) -> Result<McpReloadOutcome, String> {
        let mut entries: Vec<McpEntry> =
            config::load_daemon_config().map_err(|e| format!("failed to load MCP config: {e}"))?;
        entries.sort_by(|a, b| a.config.slug.cmp(&b.config.slug));
        let new_order: Vec<String> = entries.iter().map(|c| c.config.slug.clone()).collect();
        let new_configs: HashMap<String, McpEntry> = entries
            .iter()
            .map(|c| (c.config.slug.clone(), c.clone()))
            .collect();

        // The sessions whose overlay referenced a daemon per-session server
        // this reload changed or removed; the daemon re-resolves exactly these
        // so a `shared = false` config change (or removal) reaches the sessions
        // that hold it.
        let mut affected: HashSet<u64> = HashSet::new();

        let removed: Vec<String> = self
            .order
            .iter()
            .filter(|slug| !new_configs.contains_key(*slug))
            .cloned()
            .collect();
        for slug in &removed {
            // Capture the referencing sessions BEFORE dropping anything.
            affected.extend(self.sessions_for_slug(slug));
            self.servers.remove(slug);
            self.failures.remove(slug);
            self.drop_daemon_session_slots(slug);
            info!(server = %slug, "MCP server removed by reload");
        }

        let mut added: Vec<String> = Vec::new();
        let mut restarted: Vec<String> = Vec::new();
        let mut unchanged: Vec<String> = Vec::new();
        let mut failed: Vec<String> = Vec::new();

        for entry in &entries {
            let slug = entry.config.slug.clone();
            if !entry.shared {
                // Daemon-tier per-session server: not in `self.servers`; its
                // connection lives in `session_slots` keyed with root `None`.
                // A changed config must drop those stale slots — else
                // `ensure_session_slot` returns the old connection under the
                // same key and never picks up the new one.
                let stale: Vec<(u64, Option<PathBuf>, String)> = self
                    .session_slots
                    .keys()
                    .filter(|(_, root, s)| root.is_none() && s == &slug)
                    .cloned()
                    .collect();
                let changed = stale.iter().any(|key| {
                    self.session_slots
                        .get(key)
                        .is_some_and(|slot| slot.config != entry.config)
                });
                if changed {
                    affected.extend(self.sessions_for_slug(&slug));
                    for key in stale {
                        self.session_slots.remove(&key);
                    }
                    info!(
                        server = %slug,
                        "daemon per-session MCP server config changed; dropped stale per-session connections"
                    );
                    restarted.push(slug);
                } else {
                    unchanged.push(slug);
                }
                continue;
            }

            let was_configured = self.configs.contains_key(&slug);
            if self
                .servers
                .get(&slug)
                .is_some_and(|slot| slot.config == entry.config)
            {
                unchanged.push(slug);
                continue;
            }
            if was_configured {
                restarted.push(slug.clone());
            } else {
                added.push(slug.clone());
            }
            // A shared server being (re)connected leaves no per-session slots:
            // drop any (e.g. it was previously `shared = false`).
            affected.extend(self.drop_daemon_session_slots(&slug));
            self.servers.remove(&slug);
            if let Some(slot) =
                Self::connect_slot(&self.list_change_tx, &entry.config, RELOAD_BUDGET)
            {
                self.failures.remove(&slug);
                self.servers.insert(slug, slot);
            } else {
                let e = format!("connect to {slug:?} failed");
                warn!(server = %slug, error = %e, "MCP server failed to connect during reload");
                self.failures.insert(slug.clone(), e);
                failed.push(slug);
            }
        }

        self.configs = new_configs;
        self.order = new_order;

        let summary = format!(
            "MCP reload: {} added, {} removed, {} restarted, {} unchanged, {} failed",
            added.len(),
            removed.len(),
            restarted.len(),
            unchanged.len(),
            failed.len()
        );
        info!(%summary, "reloaded MCP configuration");
        // Deterministic order so the re-resolve sequence is reproducible.
        let mut affected_sessions: Vec<u64> = affected.into_iter().collect();
        affected_sessions.sort_unstable();
        Ok(McpReloadOutcome {
            summary,
            servers: self.status(),
            affected_sessions,
        })
    }

    /// Connect one server from its resolved config and return a ready slot,
    /// bounding the whole connect by `timeout`.
    pub(super) fn connect_slot(
        list_change_tx: &crossbeam_channel::Sender<McpListChange>,
        config: &McpServerConfig,
        timeout: Duration,
    ) -> Option<ServerSlot> {
        let list_changes = Some(list_change_tx.clone());
        let cfg = config.clone();
        // Connect AND list inside the worker, so `timeout` bounds discovery as
        // well as the handshake: a server that handshakes fast but never answers
        // `tools/list` is dropped at the budget instead of stalling the (reload
        // or command-loop) caller for the per-server request timeout.
        let handle = std::thread::spawn(move || {
            connect_and_list(&cfg, list_changes).map_err(anyhow::Error::from)
        });
        match join_with_budget(handle, timeout) {
            Some(Ok((server, tools))) => {
                Some(Self::slot_from_server(server, &tools, config.clone()))
            }
            Some(Err(e)) => {
                warn!(server = %config.slug, error = %e, "MCP server failed to connect");
                None
            }
            None => {
                warn!(server = %config.slug, "MCP server connect/discovery timed out");
                None
            }
        }
    }

    /// Build a `ServerSlot` from a freshly connected `server` and its
    /// already-listed `tools` (the listing happened inside the connect budget).
    fn slot_from_server(
        server: McpServer,
        tools: &[McpTool],
        config: McpServerConfig,
    ) -> ServerSlot {
        let handle = server.handle();
        let disabled: HashSet<&str> = config.disabled_tools.iter().map(String::as_str).collect();
        let tool_count = tools
            .iter()
            .filter(|t| !disabled.contains(t.name.as_str()))
            .count();
        ServerSlot {
            handle,
            server,
            config,
            tool_count,
            tools: tools.to_vec(),
        }
    }
}

#[cfg(test)]
#[cfg(feature = "mcp")]
mod tests {
    use super::super::config;
    use super::super::{McpManager, McpServerStatus};

    #[test]
    fn reload_with_no_config_reports_all_zero() {
        let dir = tempfile::tempdir().unwrap();
        config::set_test_config_root(Some(dir.path().to_path_buf()));
        let mut manager = McpManager::empty();
        let outcome = manager.reload().expect("empty reload succeeds");
        config::set_test_config_root(None);
        assert_eq!(
            outcome.summary,
            "MCP reload: 0 added, 0 removed, 0 restarted, 0 unchanged, 0 failed"
        );
        assert_eq!(outcome.servers, [] as [McpServerStatus; 0]);
    }

    #[test]
    fn reload_with_malformed_config_is_a_hard_error() {
        let dir = tempfile::tempdir().unwrap();
        let cfg_dir = dir.path().join("choreographr");
        std::fs::create_dir_all(&cfg_dir).unwrap();
        std::fs::write(cfg_dir.join("mcp.json"), "{ not json").unwrap();
        config::set_test_config_root(Some(dir.path().to_path_buf()));
        let mut manager = McpManager::empty();
        let err = manager.reload().expect_err("malformed config fails");
        config::set_test_config_root(None);
        assert!(err.contains("failed to load MCP config"), "got: {err}");
    }
}
