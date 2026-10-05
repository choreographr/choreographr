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
    McpReloadOutcome, RECONNECT_BUDGET, RECONNECT_TOTAL_BUDGET, RELOAD_BUDGET, RELOAD_TOTAL_BUDGET,
    ServerSlot, SharedSlot, connect_and_list, join_with_budget,
};
#[cfg(feature = "mcp")]
use choreo_mcp::{McpListChange, McpServer, McpServerConfig, McpTool};
#[cfg(feature = "mcp")]
use std::collections::{HashMap, HashSet};
#[cfg(feature = "mcp")]
use std::path::PathBuf;
#[cfg(feature = "mcp")]
use std::time::{Duration, Instant};

/// One live connection a reconnect can target, addressed by its pool key.
///
/// Lets [`super::McpManager::reconnect`] walk every connection a slug names — a
/// daemon shared server, each project-shared connection, and each per-session
/// slot — through one rebuild path, so the three categories cannot drift.
#[cfg(feature = "mcp")]
#[derive(Debug, Clone, PartialEq, Eq)]
enum SlotKey {
    /// A daemon-tier shared connection, keyed by slug.
    Daemon(String),
    /// A project-shared connection, keyed by `(project_root, slug)`.
    Project(PathBuf, String),
    /// A per-session connection, keyed by `(session_id, root or None, slug)`.
    Session(u64, Option<PathBuf>, String),
}
#[cfg(feature = "mcp")]
use tracing::{info, warn};

/// The classification of one daemon-tier entry reconciled by a reload, tallied
/// into `McpReloadOutcome::summary`, plus the sessions a change reached.
///
/// Shared by the two per-entry reconcile paths (`reconcile_shared_entry` /
/// `reconcile_per_session_entry`) so the summary and the re-resolve set are
/// accumulated through one place and cannot drift between the tiers.
#[cfg(feature = "mcp")]
#[derive(Default)]
struct ReloadTally {
    /// Newly configured since the last reload.
    added: Vec<String>,
    /// Already configured and reconnected because the entry changed.
    restarted: Vec<String>,
    /// Already configured and left untouched.
    unchanged: Vec<String>,
    /// A shared (re)connect that failed; the slug stays in `failures`.
    failed: Vec<String>,
    /// The sessions whose overlay referenced a changed/removed daemon
    /// per-session server, to be re-resolved.
    affected: HashSet<u64>,
}

#[cfg(feature = "mcp")]
impl super::McpManager {
    /// Rebuild the connection(s) to `slug`, re-registering their tools.
    ///
    /// A slug can name a daemon-tier shared server, one or more project-shared
    /// connections, and/or one or more per-session connections (a daemon
    /// `shared = false` server has a per-session slot with `root = None`; a
    /// project `shared = false` server has one with `root = Some(..)`). Every
    /// matching connection is rebuilt in place: a failed rebuild is collected,
    /// not fatal, so one bad connection does not skip the rest. The WHOLE walk
    /// is bounded by [`RECONNECT_TOTAL_BUDGET`], so a slug referenced from many
    /// projects cannot stall the command loop for one budget per connection.
    ///
    /// Each connection is rebuilt the SAME way — the replacement slot is built
    /// first and swapped in only on success — so a transient connect failure
    /// never discards a working connection.
    ///
    /// # Errors
    ///
    /// Returns a message when `slug` matches nothing, or when every matching
    /// connection failed to rebuild.
    pub fn reconnect(&mut self, slug: &str) -> Result<(), String> {
        let keys = self.matching_slot_keys(slug);
        if keys.is_empty() {
            return Err(format!("unknown MCP server {slug:?}"));
        }

        let deadline = Instant::now() + RECONNECT_TOTAL_BUDGET;
        let mut reconnected = 0usize;
        let mut errors: Vec<String> = Vec::new();
        for key in keys {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                errors.push(format!(
                    "reconnect budget exhausted before rebuilding {key:?}"
                ));
                break;
            }
            match self.rebuild_slot(&key, remaining.min(RECONNECT_BUDGET)) {
                Ok(()) => {
                    info!(server = %slug, slot = ?key, "reconnected MCP server");
                    reconnected += 1;
                }
                Err(e) => errors.push(e),
            }
        }

        if reconnected == 0 {
            return Err(errors.join("; "));
        }
        Ok(())
    }

    /// Every live connection `slug` names, in rebuild order (daemon shared
    /// first, then project-shared, then per-session).
    fn matching_slot_keys(&self, slug: &str) -> Vec<SlotKey> {
        let mut keys = Vec::new();
        // A daemon shared server is reconnectable even when it is not currently
        // connected (a startup/reload failure leaves it out of `servers`), so
        // this is keyed on the CONFIG, not on a live slot.
        if self.configs.get(slug).is_some_and(|e| e.shared) {
            keys.push(SlotKey::Daemon(slug.to_string()));
        }
        for (root, s) in self.project_shared.keys() {
            if s == slug {
                keys.push(SlotKey::Project(root.clone(), s.clone()));
            }
        }
        for (sid, root, s) in self.session_slots.keys() {
            if s == slug {
                keys.push(SlotKey::Session(*sid, root.clone(), s.clone()));
            }
        }
        keys
    }

    /// The resolved config backing `key`, cloned so the caller can rebuild
    /// without holding a borrow of the manager.
    fn slot_config(&self, key: &SlotKey) -> Option<McpServerConfig> {
        match key {
            SlotKey::Daemon(slug) => self.configs.get(slug).map(|e| e.config.clone()),
            SlotKey::Project(root, slug) => self
                .project_shared
                .get(&(root.clone(), slug.clone()))
                .map(|shared| shared.slot.config.clone()),
            SlotKey::Session(id, root, slug) => self
                .session_slots
                .get(&(*id, root.clone(), slug.clone()))
                .map(|slot| slot.config.clone()),
        }
    }

    /// Rebuild the connection for `key` in place, replacing the slot only on
    /// success so a failed rebuild never discards a working connection.
    ///
    /// `timeout` bounds the connect-and-discover worker.
    fn rebuild_slot(&mut self, key: &SlotKey, timeout: Duration) -> Result<(), String> {
        let config = self
            .slot_config(key)
            .ok_or_else(|| format!("no configuration for MCP slot {key:?}"))?;
        let Some(slot) = Self::connect_slot(&self.list_change_tx, &config, timeout) else {
            return Err(match key {
                SlotKey::Daemon(_) => format!("reconnect to {:?} failed", config.slug),
                SlotKey::Project(root, _) => format!(
                    "reconnect to {:?} failed for project root {}",
                    config.slug,
                    root.display()
                ),
                SlotKey::Session(id, _, _) => {
                    format!("reconnect to {:?} failed for session {id}", config.slug)
                }
            });
        };
        match key {
            SlotKey::Daemon(slug) => {
                self.failures.remove(slug);
                self.servers.insert(slug.clone(), slot);
            }
            SlotKey::Project(root, slug) => {
                // Preserve the existing ref-count set (a failed connect would
                // have left it untouched; on success the new slot inherits it).
                let sessions = self
                    .project_shared
                    .get(&(root.clone(), slug.clone()))
                    .map_or_default(|shared| shared.sessions.clone());
                self.project_shared
                    .insert((root.clone(), slug.clone()), SharedSlot { slot, sessions });
            }
            SlotKey::Session(id, root, slug) => {
                self.session_slots
                    .insert((*id, root.clone(), slug.clone()), slot);
            }
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

        // The sessions whose overlay referenced a daemon per-session server this
        // reload changed or removed; the daemon re-resolves exactly these so a
        // `shared = false` config change (or removal) reaches the sessions that
        // hold it. A SHARED server's change reaches every session through the
        // daemon-wide catalogue swap, so it needs no per-session entry here.
        let mut tally = ReloadTally::default();

        let removed: Vec<String> = self
            .order
            .iter()
            .filter(|slug| !new_configs.contains_key(*slug))
            .cloned()
            .collect();
        for slug in &removed {
            // Capture the referencing sessions BEFORE dropping anything.
            tally.affected.extend(self.sessions_for_slug(slug));
            self.servers.remove(slug);
            self.failures.remove(slug);
            self.drop_daemon_session_slots(slug);
            info!(server = %slug, "MCP server removed by reload");
        }

        // One deadline for the WHOLE reload: each (re)connect gets the smaller
        // of its per-server budget and the remaining share, so a config with
        // many slow servers cannot stall the command loop indefinitely.
        let deadline = Instant::now() + RELOAD_TOTAL_BUDGET;

        for entry in &entries {
            let slug = entry.config.slug.clone();
            // `self.configs` still holds the PRE-reload set here, so it tells us
            // whether this slug was configured before this reload. Both reconcile
            // paths are keyed on the shared-vs-per-session ATTRIBUTE of the NEW
            // entry, so a slug that FLIPS tiers is reconciled by its new tier
            // (which drops the old tier's connection).
            let was_configured = self.configs.contains_key(&slug);
            if entry.shared {
                self.reconcile_shared_entry(&slug, entry, was_configured, deadline, &mut tally);
            } else {
                self.reconcile_per_session_entry(&slug, entry, was_configured, &mut tally);
            }
        }

        self.configs = new_configs;
        self.order = new_order;

        let summary = format!(
            "MCP reload: {} added, {} removed, {} restarted, {} unchanged, {} failed",
            tally.added.len(),
            removed.len(),
            tally.restarted.len(),
            tally.unchanged.len(),
            tally.failed.len()
        );
        info!(%summary, "reloaded MCP configuration");
        // Deterministic order so the re-resolve sequence is reproducible.
        let mut affected_sessions: Vec<u64> = tally.affected.into_iter().collect();
        affected_sessions.sort_unstable();
        Ok(McpReloadOutcome {
            summary,
            servers: self.status(),
            affected_sessions,
        })
    }

    /// Reconcile one daemon-tier SHARED entry, reconnecting it when the live
    /// slot's resolved config differs and dropping any stale per-session slots
    /// the slug may have held.
    ///
    /// A shared server's change is visible to every session through the daemon
    /// catalogue rebuild the caller runs afterwards, so no session needs a
    /// per-session re-resolve for it; `tally.affected` therefore only grows from
    /// the stale per-session slots this drops (the `shared = false` -> `true`
    /// flip). A (re)connect that fails is recorded in `failures` AND still
    /// counted as added/restarted, matching the summary's intent (the entry IS
    /// (re)configured; it just did not come up).
    fn reconcile_shared_entry(
        &mut self,
        slug: &str,
        entry: &McpEntry,
        was_configured: bool,
        deadline: Instant,
        tally: &mut ReloadTally,
    ) {
        // An unchanged resolved config keeps the live connection untouched.
        if self
            .servers
            .get(slug)
            .is_some_and(|slot| slot.config == entry.config)
        {
            tally.unchanged.push(slug.to_string());
            return;
        }
        if was_configured {
            tally.restarted.push(slug.to_string());
        } else {
            tally.added.push(slug.to_string());
        }
        // A shared server being (re)connected leaves no per-session slots: drop
        // any (e.g. it was previously `shared = false`).
        tally.affected.extend(self.drop_daemon_session_slots(slug));
        self.servers.remove(slug);
        let remaining = deadline.saturating_duration_since(Instant::now());
        let slot = if remaining.is_zero() {
            warn!(server = %slug, "MCP reload budget exhausted; skipping connect");
            None
        } else {
            Self::connect_slot(
                &self.list_change_tx,
                &entry.config,
                remaining.min(RELOAD_BUDGET),
            )
        };
        if let Some(slot) = slot {
            self.failures.remove(slug);
            self.servers.insert(slug.to_string(), slot);
        } else {
            let e = format!("connect to {slug:?} failed");
            warn!(server = %slug, error = %e, "MCP server failed to connect during reload");
            self.failures.insert(slug.to_string(), e);
            tally.failed.push(slug.to_string());
        }
    }

    /// Reconcile one daemon-tier PER-SESSION (`shared = false`) entry, dropping
    /// the stale connections a config change (or the shared -> per-session flip)
    /// leaves behind so the next resolve reconnects with the new config.
    ///
    /// A daemon per-session server normally lives only in `session_slots`, but a
    /// server FLIPPED from `shared = true` to `false` still has a stale DAEMON
    /// slot in `servers`; that slot is dropped too — leaving it would keep the
    /// server registering its tools under `mcp/<slug>` in the daemon catalogue
    /// while the per-session connection registers them again in each session's
    /// overlay, yielding duplicate tool names. Either kind of stale slot makes
    /// the entry `restarted`; a genuinely unchanged per-session server whose
    /// connection still matches is left untouched.
    fn reconcile_per_session_entry(
        &mut self,
        slug: &str,
        entry: &McpEntry,
        was_configured: bool,
        tally: &mut ReloadTally,
    ) {
        // Drop a stale daemon SHARED slot (the shared -> per-session flip); a
        // per-session server's connection is NOT here, it is in `session_slots`.
        let had_shared = self.servers.remove(slug).is_some();
        if had_shared {
            self.failures.remove(slug);
        }
        let stale = self.daemon_per_session_slot_keys(slug);
        let changed = had_shared
            || stale.iter().any(|key| {
                self.session_slots
                    .get(key)
                    .is_some_and(|slot| slot.config != entry.config)
            });
        if changed {
            // Capture the referencing sessions BEFORE dropping their slots.
            tally.affected.extend(self.sessions_for_slug(slug));
            for key in stale {
                self.session_slots.remove(&key);
            }
            info!(
                server = %slug,
                "daemon per-session MCP server changed; dropped stale connections"
            );
            tally.restarted.push(slug.to_string());
        } else if was_configured {
            tally.unchanged.push(slug.to_string());
        } else {
            // A newly-added daemon `shared = false` server has no stale slot to
            // drop and no live session holds it yet: it reaches existing sessions
            // on their next overlay re-resolve, not at reload.
            tally.added.push(slug.to_string());
        }
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
        let tool_count = Self::enabled_tool_count(tools, &config.disabled_tools);
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
    use super::super::config::{self, McpEntry};
    use super::super::{McpManager, McpServerStatus};
    use super::SlotKey;

    /// A minimal entry with the given slug and pooling attribute.
    fn entry(slug: &str, shared: bool) -> McpEntry {
        McpEntry {
            config: choreo_mcp::McpServerConfig {
                slug: slug.to_string(),
                transport: choreo_mcp::McpTransport::Stdio {
                    command: "true".to_string(),
                    args: Vec::new(),
                    env: std::collections::HashMap::new(),
                    cwd: None,
                    log_path: None,
                },
                enabled: true,
                timeout: None,
                protocol: choreo_mcp::McpProtocolMode::Auto,
                max_concurrent_calls: None,
                max_restarts: None,
                disabled_tools: Vec::new(),
            },
            shared,
        }
    }

    /// A daemon-shared server is reconnectable from its CONFIG even with no live
    /// slot (a failed startup must be retryable), while a per-session server is
    /// reconnectable only where it is actually connected.
    #[test]
    fn matching_slot_keys_targets_shared_by_config() {
        let mut manager = McpManager::empty();
        manager
            .configs
            .insert("shared".to_string(), entry("shared", true));
        manager
            .configs
            .insert("per".to_string(), entry("per", false));
        assert_eq!(
            manager.matching_slot_keys("shared"),
            vec![SlotKey::Daemon("shared".to_string())]
        );
        assert!(
            manager.matching_slot_keys("per").is_empty(),
            "a per-session server with no live slot has nothing to reconnect"
        );
    }

    #[test]
    fn reconnect_unknown_slug_is_an_error() {
        let mut manager = McpManager::empty();
        let err = manager
            .reconnect("nope")
            .expect_err("an unknown slug must error");
        assert!(err.contains("unknown MCP server"), "{err}");
    }

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
