//! The per-session overlay resolution: planning the servers behind a session's
//! private project/per-session overlay, installing the connections a worker
//! made, and ref-counting the shared ones.
//!
//! The pure overlay VALUE types (`ProjectToolSet`, `SessionMcpOverlay`) and the
//! project-root walk (`project_root_for`) live in `project.rs`; this module is
//! the `mcp`-feature-gated `impl McpManager` block that reconciles the pool
//! behind those values. It lives here rather than in `mod.rs` so it keeps the
//! manager's private fields and helpers in scope as a child module.
//!
//! Resolution is split in two so it never blocks the command loop: a PLAN folds
//! the already-pooled connections into an overlay and reports the entries a
//! worker must still connect, and an INSTALL folds a worker's finished
//! connection back into the pool. The daemon's command loop drives both; nothing
//! here performs a handshake. The one blocking seam
//! ([`McpManager::ensure_session_blocking`]) exists for tests and embedders that
//! want a populated overlay before proceeding, and is built on the same
//! plan/install pair so it cannot drift from the daemon's path.

use super::McpServerStatus;
#[cfg(feature = "mcp")]
use super::config::{self, McpEntry};
#[cfg(feature = "mcp")]
use super::project::{ProjectToolSet, SessionMcpOverlay};
#[cfg(feature = "mcp")]
use super::slot::ServerSlot;
#[cfg(feature = "mcp")]
use super::tool::build_server_wrappers;
#[cfg(feature = "mcp")]
use crate::tools::ToolDyn;
#[cfg(feature = "mcp")]
use choreo_mcp::{McpListChange, McpServer, McpServerConfig, McpTool};
#[cfg(feature = "mcp")]
use std::collections::HashSet;
#[cfg(feature = "mcp")]
use std::path::{Path, PathBuf};
#[cfg(feature = "mcp")]
use std::sync::Arc;
#[cfg(feature = "test-utils")]
use std::time::{Duration, Instant};
#[cfg(feature = "mcp")]
use tracing::{debug, warn};

#[cfg(feature = "mcp")]
impl super::McpManager {
    /// The daemon-tier `shared = false` entries a session's overlay should
    /// connect, excluding any slug a POOLED project server overrides (its
    /// whole-entry project config replaces the daemon server of the same name).
    ///
    /// `suppressed` is the set of project slugs whose servers are connected (or
    /// being connected); a project entry that failed does not appear in it, so
    /// the daemon server of that slug is still planned.
    ///
    /// Sorted by slug so the collision-suffix assignment for two daemon
    /// per-session servers is deterministic across resolves (matching the
    /// ordering the other registration paths use): `self.configs` is a `HashMap`,
    /// whose iteration order is not stable, and tool names are resolved
    /// first-come-first-served.
    fn daemon_per_session_entries(&self, suppressed: &HashSet<String>) -> Vec<McpEntry> {
        let mut entries: Vec<McpEntry> = self
            .configs
            .values()
            .filter(|e| !e.shared && !suppressed.contains(&e.config.slug))
            .cloned()
            .collect();
        entries.sort_by(|a, b| a.config.slug.cmp(&b.config.slug));
        entries
    }

    /// Release every connection a leaving (or re-resolving) session holds:
    /// decrement the ref-count of each project-shared connection it referenced
    /// (dropping those that reach zero) and drop all its per-session slots.
    pub fn release_session(&mut self, session_id: u64) {
        let mut dropped_shared = Vec::new();
        for (key, shared) in &mut self.project_shared {
            shared.sessions.remove(&session_id);
            if shared.sessions.is_empty() {
                dropped_shared.push(key.clone());
            }
        }
        for key in dropped_shared {
            if self.project_shared.remove(&key).is_some() {
                debug!(root = %key.0.display(), server = %key.1, "dropped unreferenced shared project MCP server");
            }
        }
        let before = self.session_slots.len();
        self.session_slots
            .retain(|(sid, _, _), _| *sid != session_id);
        let dropped = before - self.session_slots.len();
        if dropped > 0 {
            debug!(session_id, dropped, "dropped per-session MCP servers");
        }
    }

    /// The `session_slots` keys naming a DAEMON-tier per-session connection
    /// (`root = None`) for `slug`.
    ///
    /// A key with `root = Some(..)` is a PROJECT-tier per-session connection and
    /// is reconciled by the overlay resolution, never by a daemon-tier reload;
    /// only the daemon tier's keys are returned here.
    pub(crate) fn daemon_per_session_slot_keys(
        &self,
        slug: &str,
    ) -> Vec<(u64, Option<PathBuf>, String)> {
        self.session_slots
            .keys()
            .filter(|(_, root, s)| root.is_none() && s == slug)
            .cloned()
            .collect()
    }

    /// Remove every daemon-tier per-session slot (`root = None`) for `slug`,
    /// returning the sessions that held them.
    ///
    /// A stale slot is one whose server's config changed, that no longer exists,
    /// or that was connected before the credentials it needs became available;
    /// leaving it in `session_slots` would make a later resolve reuse the stale
    /// connection under the same key forever. Dropping it is therefore how the
    /// daemon forces the holding sessions to reconnect (their next overlay
    /// resolve plans the entry as pending again).
    pub(crate) fn drop_daemon_session_slots(&mut self, slug: &str) -> HashSet<u64> {
        let mut sessions = HashSet::new();
        for key in self.daemon_per_session_slot_keys(slug) {
            if self.session_slots.remove(&key).is_some() {
                sessions.insert(key.0);
            }
        }
        sessions
    }

    /// Build the tool wrappers for one pooled slot WITHOUT registering them in a
    /// `ToolRegistry`; they are returned to the session as its private overlay.
    ///
    /// Delegates the name resolution, disabled filter, and resource-catalogue
    /// construction to [`build_server_wrappers`] (the same builder the
    /// daemon-wide registration uses), so the two paths cannot drift.
    fn append_cached_tools(
        slug: &str,
        tier: &str,
        slot: &ServerSlot,
        used: &mut HashSet<String>,
        tools: &mut Vec<Box<dyn ToolDyn>>,
        groups: &mut HashSet<String>,
        statuses: &mut Vec<McpServerStatus>,
    ) {
        let built = build_server_wrappers(
            slug,
            &slot.handle,
            &slot.tools,
            &slot.config.disabled_tools,
            used,
        );
        groups.insert(built.group);
        tools.extend(built.tools.into_iter().map(|(_, wrapper)| wrapper));
        statuses.push(McpServerStatus::for_slot(slug, tier, slot));
    }
}

/// One server a session's overlay still has to connect, handed to a background
/// worker and returned (with the connected server) so the command loop can
/// install it into the pool. `pub(crate)` because it rides `DaemonCommand`.
#[cfg(feature = "mcp")]
#[derive(Debug, Clone)]
pub struct PendingSessionConnect {
    /// The server slug (tool-name prefix).
    pub slug: String,
    /// The resolved server config the worker connects with.
    pub config: McpServerConfig,
    /// `Some(root)` for a project-tier entry; `None` for a daemon per-session
    /// (`shared = false`) entry.
    pub root: Option<PathBuf>,
}

impl PendingSessionConnect {
    /// The `(root, slug)` key that identifies this entry in a session's overlay
    /// plan, so a plan, its in-flight markers, and an arriving result agree.
    #[cfg(feature = "mcp")]
    #[must_use]
    pub(crate) fn key(&self) -> (Option<PathBuf>, String) {
        (self.root.clone(), self.slug.clone())
    }
}

/// A session's overlay plan: the overlay built from the connections already in
/// the pool, plus the entries a worker must still connect.
#[cfg(feature = "mcp")]
pub(crate) struct SessionOverlayPlan {
    /// The overlay from the currently pooled connections (no connect).
    pub partial: SessionMcpOverlay,
    /// Project-tier entries to connect first.
    pub project: Vec<PendingSessionConnect>,
    /// Daemon per-session entries; a slug a project entry CONNECTED is skipped
    /// by the worker.
    pub daemon: Vec<PendingSessionConnect>,
}

/// Asynchronous overlay resolution: the `McpManager` half of the daemon's
/// off-loop session connect. The command loop plans (cheap, pool-only) and the
/// worker connects; the loop then installs the results and rebuilds the overlay
/// from the cache. None of these methods block on a handshake.
#[cfg(feature = "mcp")]
impl super::McpManager {
    /// A clone of the shared list-change sender a background overlay worker
    /// re-subscribes rebuilt transports through.
    pub(crate) fn list_change_sender(&self) -> crossbeam_channel::Sender<McpListChange> {
        self.list_change_tx.clone()
    }

    /// Plan a session's overlay WITHOUT connecting anything.
    ///
    /// Folds every already-pooled project-shared / per-session connection into
    /// the partial overlay, adds the session to an existing project-shared
    /// connection's reference set, and hands back the entries a worker must
    /// connect. Reused connections cost nothing — their tools come straight from
    /// the cached tool set, so the common re-resolve is a pure in-memory pass.
    pub(crate) fn plan_session_overlay(
        &mut self,
        session_id: u64,
        project_root: Option<&Path>,
        trusted: bool,
    ) -> SessionOverlayPlan {
        // Project tier: join an existing shared connection (ref-count only) or
        // mark the entry pending.
        let mut pooled_project: HashSet<String> = HashSet::new();
        let mut project: Vec<PendingSessionConnect> = Vec::new();
        if let (Some(root), true) = (project_root, trusted)
            && let Ok(Some(entries)) = config::load_project_config(root, true)
        {
            for entry in entries {
                let slug = entry.config.slug.clone();
                if let Some(shared) = self
                    .project_shared
                    .get_mut(&(root.to_path_buf(), slug.clone()))
                {
                    shared.sessions.insert(session_id);
                    pooled_project.insert(slug);
                } else {
                    project.push(PendingSessionConnect {
                        slug,
                        config: entry.config,
                        root: Some(root.to_path_buf()),
                    });
                }
            }
        }
        // Daemon per-session tier: a not-yet-pooled entry is pending. A slug a
        // POOLED project server provides is already excluded by
        // `daemon_per_session_entries` (project override).
        let mut daemon: Vec<PendingSessionConnect> = Vec::new();
        for entry in self.daemon_per_session_entries(&pooled_project) {
            let slug = entry.config.slug.clone();
            if !self
                .session_slots
                .contains_key(&(session_id, None, slug.clone()))
            {
                daemon.push(PendingSessionConnect {
                    slug,
                    config: entry.config,
                    root: None,
                });
            }
        }
        let partial = self.build_session_overlay_cached(session_id, project_root, trusted);
        SessionOverlayPlan {
            partial,
            project,
            daemon,
        }
    }

    /// Build a session's overlay from the CURRENTLY POOLED connections only —
    /// no connect and no round-trip. Each pooled slot's cached tool set is turned
    /// into wrappers; an entry not yet pooled is simply omitted (it arrives via a
    /// later install + rebuild). This is both the "partial" overlay a plan
    /// returns and the "full" overlay a worker's install rebuilds.
    pub(crate) fn build_session_overlay_cached(
        &self,
        session_id: u64,
        project_root: Option<&Path>,
        trusted: bool,
    ) -> SessionMcpOverlay {
        let mut used: HashSet<String> = HashSet::new();
        let mut tools: Vec<Box<dyn ToolDyn>> = Vec::new();
        let mut groups: HashSet<String> = HashSet::new();
        let mut statuses: Vec<McpServerStatus> = Vec::new();
        let mut shadowed: HashSet<String> = HashSet::new();
        let mut ignored: Vec<String> = Vec::new();

        let mut pooled_project: HashSet<String> = HashSet::new();
        if let Some(root) = project_root {
            match config::load_project_config(root, trusted) {
                Ok(Some(entries)) => {
                    if trusted {
                        for entry in entries {
                            let slug = entry.config.slug.clone();
                            if let Some(shared) =
                                self.project_shared.get(&(root.to_path_buf(), slug.clone()))
                                && shared.sessions.contains(&session_id)
                            {
                                pooled_project.insert(slug.clone());
                                Self::append_cached_tools(
                                    &slug,
                                    "project",
                                    &shared.slot,
                                    &mut used,
                                    &mut tools,
                                    &mut groups,
                                    &mut statuses,
                                );
                            }
                        }
                    } else {
                        ignored = entries.iter().map(|e| e.config.slug.clone()).collect();
                        ignored.sort();
                    }
                }
                Ok(None) => {}
                Err(e) => {
                    warn!(root = %root.display(), error = %e, "failed to read project .mcp.json");
                }
            }
        }
        // Only a CONNECTED (pooled) project server shadows the daemon group of
        // the same slug.
        for slug in &pooled_project {
            shadowed.insert(choreo_mcp::group_name(slug));
        }

        for entry in self.daemon_per_session_entries(&pooled_project) {
            let slug = entry.config.slug.clone();
            if let Some(slot) = self.session_slots.get(&(session_id, None, slug.clone())) {
                Self::append_cached_tools(
                    &slug,
                    "daemon",
                    slot,
                    &mut used,
                    &mut tools,
                    &mut groups,
                    &mut statuses,
                );
            }
        }

        SessionMcpOverlay {
            tools: Arc::new(ProjectToolSet::new(tools, groups)),
            shadowed_groups: shadowed,
            project_root: project_root.map(Path::to_path_buf),
            project_trusted: trusted,
            ignored_project_servers: ignored,
            statuses,
        }
    }

    /// Install a worker-connected overlay entry into the pool: a project entry
    /// into `project_shared` (ref-counted for `session_id`), a daemon per-session
    /// entry into `session_slots`.
    ///
    /// Installing REPLACES any connection already pooled under the same key, so
    /// a reconnect that raced another one is settled by whichever lands last —
    /// both are legitimate connections to the same server.
    pub(crate) fn install_session_connected(
        &mut self,
        session_id: u64,
        entry: PendingSessionConnect,
        server: McpServer,
        tools: &[McpTool],
    ) {
        let slot = Self::slot_from_server(server, tools, entry.config);
        match entry.root {
            Some(root) => {
                let key = (root, entry.slug);
                // Preserve any other sessions already referencing this shared
                // connection; this session is now one of them.
                let mut sessions = self
                    .project_shared
                    .get(&key)
                    .map_or_else(HashSet::new, |s| s.sessions.clone());
                sessions.insert(session_id);
                self.project_shared
                    .insert(key, super::SharedSlot { slot, sessions });
            }
            None => {
                self.session_slots
                    .insert((session_id, None, entry.slug), slot);
            }
        }
    }
}

/// The blocking overlay seam: a fully connected overlay in one call.
///
/// This exists for tests and for an embedder that must have a session's tools
/// before it proceeds. It is built ON the daemon's own plan/install pair, so it
/// exercises the same resolution the command loop does and cannot drift from it;
/// `budget` bounds the WHOLE resolve (each entry gets the remaining share), the
/// way the daemon bounds a sweep. The daemon itself never calls it — every
/// connect runs on a background worker.
#[cfg(feature = "test-utils")]
impl super::McpManager {
    /// Ensure a session's overlay synchronously: connect every entry the plan
    /// reports (project tier first, then daemon per-session), install each, and
    /// return the overlay rebuilt from the pool.
    pub fn ensure_session_blocking(
        &mut self,
        session_id: u64,
        project_root: Option<&Path>,
        trusted: bool,
        budget: Duration,
    ) -> SessionMcpOverlay {
        let deadline = Instant::now() + budget;
        let plan = self.plan_session_overlay(session_id, project_root, trusted);
        let list_changes = self.list_change_tx.clone();
        // Project servers connect first so a CONNECTED project server suppresses
        // the daemon per-session server of the same slug, exactly as the
        // background worker orders the two tiers.
        let mut connected_project: HashSet<String> = HashSet::new();
        for entry in plan.project {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                warn!(server = %entry.slug, "overlay connect budget exhausted");
                break;
            }
            let outcome =
                super::connect_entry(&entry.config, Some(list_changes.clone()), remaining);
            match outcome {
                Ok((server, tools)) => {
                    connected_project.insert(entry.slug.clone());
                    self.install_session_connected(session_id, entry, server, &tools);
                }
                Err(e) => {
                    warn!(server = %entry.slug, error = %e, "project MCP server failed to connect")
                }
            }
        }
        for entry in plan.daemon {
            if connected_project.contains(&entry.slug) {
                continue;
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                warn!(server = %entry.slug, "overlay connect budget exhausted");
                break;
            }
            let outcome =
                super::connect_entry(&entry.config, Some(list_changes.clone()), remaining);
            match outcome {
                Ok((server, tools)) => {
                    self.install_session_connected(session_id, entry, server, &tools);
                }
                Err(e) => {
                    warn!(server = %entry.slug, error = %e, "per-session MCP server failed to connect")
                }
            }
        }
        self.build_session_overlay_cached(session_id, project_root, trusted)
    }
}

#[cfg(test)]
#[cfg(feature = "mcp")]
mod tests {
    use super::super::McpManager;
    use crate::mcp::config::McpEntry;
    use choreo_mcp::McpServerConfig;
    use std::collections::HashSet;

    /// A minimal daemon-tier entry with the given slug and pooling attribute.
    fn entry(slug: &str, shared: bool) -> McpEntry {
        McpEntry {
            config: McpServerConfig {
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

    #[test]
    fn plan_with_no_servers_is_empty() {
        // A session with no project and no daemon per-session servers plans an
        // empty overlay with nothing to connect.
        let mut manager = McpManager::empty();
        let plan = manager.plan_session_overlay(1, None, false);
        assert!(plan.project.is_empty());
        assert!(plan.daemon.is_empty());
        assert!(plan.partial.tools.is_empty());
    }

    #[test]
    fn plan_reports_unpooled_daemon_per_session_entries_as_pending() {
        // A daemon `shared = false` server that is not pooled is pending; a
        // shared one never reaches the per-session tier at all.
        let mut manager = McpManager::empty();
        manager
            .configs
            .insert("per".to_string(), entry("per", false));
        manager
            .configs
            .insert("shared".to_string(), entry("shared", true));

        let plan = manager.plan_session_overlay(1, None, false);
        let slugs: Vec<&str> = plan.daemon.iter().map(|e| e.slug.as_str()).collect();
        assert_eq!(slugs, ["per"], "only the per-session entry is pending");
        assert!(plan.project.is_empty(), "no project tier without a root");
    }

    #[test]
    fn daemon_per_session_entries_are_slug_sorted() {
        let mut manager = McpManager::empty();
        // Insert in an order that is NOT sorted, and with a mix of shared and
        // per-session servers, so the filter and the sort are both exercised.
        for (slug, shared) in [
            ("gamma", false),
            ("alpha", false),
            ("shared", true),
            ("beta", false),
        ] {
            manager
                .configs
                .insert(slug.to_string(), entry(slug, shared));
        }

        let entries = manager.daemon_per_session_entries(&HashSet::new());
        let slugs: Vec<&str> = entries.iter().map(|e| e.config.slug.as_str()).collect();
        assert_eq!(
            slugs,
            ["alpha", "beta", "gamma"],
            "per-session slugs, sorted"
        );

        // A suppressed (project-overridden) slug is excluded.
        let suppressed = HashSet::from(["beta".to_string()]);
        let entries = manager.daemon_per_session_entries(&suppressed);
        let slugs: Vec<&str> = entries.iter().map(|e| e.config.slug.as_str()).collect();
        assert_eq!(slugs, ["alpha", "gamma"]);
    }
}
