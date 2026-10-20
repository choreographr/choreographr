//! The per-session overlay resolution: connecting the servers behind a session's
//! private project/per-session overlay and ref-counting the shared connections.
//!
//! The pure overlay VALUE types (`ProjectToolSet`, `SessionMcpOverlay`) and the
//! project-root walk (`project_root_for`) live in `project.rs`; this module is
//! the `mcp`-feature-gated `impl McpManager` block that (re)connects the servers
//! behind those values. It lives here rather than in `mod.rs` so it keeps the
//! manager's private fields and helpers in scope as a child module.

use super::McpServerStatus;
#[cfg(feature = "mcp")]
use super::config::{self, McpEntry};
#[cfg(feature = "mcp")]
use super::project::{ProjectToolSet, SessionMcpOverlay};
#[cfg(feature = "mcp")]
use super::tool::build_server_wrappers;
#[cfg(feature = "mcp")]
use super::{CATALOGUE_REFRESH_BUDGET, RELOAD_BUDGET, ServerSlot, SharedSlot};
#[cfg(feature = "mcp")]
use crate::tools::ToolDyn;
#[cfg(feature = "mcp")]
use choreo_mcp::{McpListChange, McpServer, McpServerConfig, McpServerHandle, McpTool};
#[cfg(feature = "mcp")]
use std::collections::HashSet;
#[cfg(feature = "mcp")]
use std::path::{Path, PathBuf};
#[cfg(feature = "mcp")]
use std::sync::Arc;
#[cfg(feature = "mcp")]
use std::time::{Duration, Instant};
#[cfg(feature = "mcp")]
use tracing::{debug, info, warn};

/// Aggregate budget for the connect portion of ONE session's overlay
/// resolution (see [`super::McpManager::ensure_session`]).
///
/// Unlike [`super::RELOAD_BUDGET`], which is per server, this bounds the WHOLE
/// set of project/per-session servers a single resolve connects.
/// `ensure_session` runs on the command-loop thread, so a project declaring
/// several slow or unresponsive servers would otherwise stall every session and
/// client behind it for one budget PER server. The budget matches the
/// single-server reload budget, so one legitimately slow start (e.g. a cold
/// `npx`) still connects in the common case, while a batch of them cannot
/// compound into an unbounded stall; a server that misses the deadline is
/// skipped from this resolve and retried on the next one.
#[cfg(feature = "mcp")]
const SESSION_CONNECT_BUDGET: Duration = Duration::from_secs(10);

#[cfg(feature = "mcp")]
impl super::McpManager {
    /// Ensure a session's overlay: connect/create the project and per-session
    /// servers it needs, ref-counting shared project connections, and return
    /// the private tool set plus the groups to shadow.
    ///
    /// `project_root`/`trusted` come from the daemon (the session's working
    /// directory walked up to a `.mcp.json` and checked against the trust
    /// store). An UNTRUSTED project root yields no project connections: the
    /// declaration is read only to report the ignored slugs, and — because the
    /// project is not honoured — its slugs must NOT suppress a daemon-tier
    /// per-session server of the same name.
    ///
    /// A TRUSTED project's servers are connected FIRST; only a slug whose server
    /// actually CONNECTED suppresses the daemon-tier `shared = false` server of
    /// the same name (and shadows the daemon-tier `mcp/<slug>` group). A project
    /// entry that failed to connect, or was skipped on the connect deadline,
    /// therefore leaves the daemon server of that slug in place.
    ///
    /// The connect portion of the whole resolve is bounded by
    /// `SESSION_CONNECT_BUDGET`, so a project whose servers are slow to start
    /// cannot stall the command loop for one budget per server; a server that
    /// misses the deadline is skipped from this resolve and retried on the
    /// next. Connections already pooled for this session/server are reused and
    /// consume no budget.
    pub fn ensure_session(
        &mut self,
        session_id: u64,
        project_root: Option<&Path>,
        trusted: bool,
    ) -> SessionMcpOverlay {
        self.ensure_session_within(
            session_id,
            project_root,
            trusted,
            Instant::now() + SESSION_CONNECT_BUDGET,
        )
    }

    /// Like [`ensure_session`](Self::ensure_session), but bounding the whole
    /// resolve by an explicit `deadline` rather than a fresh per-resolve budget.
    ///
    /// The command loop's overlay re-resolve sweeps (a list change, a reload, and
    /// a trust-watch reload) call this with a SHARED deadline, so re-resolving
    /// many sessions cannot compound into one budget per session; a server
    /// reached after the deadline is deferred to the next resolve.
    /// `SESSION_CONNECT_BUDGET` is the budget a lone resolve gets.
    pub fn ensure_session_within(
        &mut self,
        session_id: u64,
        project_root: Option<&Path>,
        trusted: bool,
        deadline: Instant,
    ) -> SessionMcpOverlay {
        let mut used: HashSet<String> = HashSet::new();
        let mut tools: Vec<Box<dyn ToolDyn>> = Vec::new();
        let mut groups: HashSet<String> = HashSet::new();
        let mut statuses: Vec<McpServerStatus> = Vec::new();
        let mut shadowed: HashSet<String> = HashSet::new();
        let mut ignored: Vec<String> = Vec::new();

        // Resolve the project entries. A trusted project's slugs override a
        // same-slug daemon per-session server (the project override is
        // whole-entry) — but ONLY when the project server actually connects
        // (see below); an untrusted `.mcp.json` is merely reported.
        let mut project_entries: Vec<McpEntry> = Vec::new();
        if let Some(root) = project_root {
            match config::load_project_config(root, trusted) {
                Ok(Some(entries)) => {
                    if trusted {
                        project_entries = entries;
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

        // `deadline` (the caller's shared one, or a fresh per-resolve budget) is
        // used by every server this resolve ensures, so the aggregate is bounded
        // (not one budget per server).

        // Connect the project's servers FIRST, so we learn which slugs actually
        // connected before deciding what the daemon tier contributes: only a
        // project server that CONNECTED overrides the daemon server of the same
        // name. A project slug whose server failed (or timed out) does NOT
        // suppress the daemon-tier server — matching the group-shadow rule.
        let mut connected_project_slugs: HashSet<String> = HashSet::new();
        if let Some(root) = project_root
            && trusted
        {
            let connected = self.ensure_entries(
                session_id,
                Some(root),
                &project_entries,
                deadline,
                &mut used,
                &mut tools,
                &mut groups,
                &mut statuses,
            );
            for slug in connected {
                shadowed.insert(choreo_mcp::group_name(&slug));
                connected_project_slugs.insert(slug);
            }
        }

        // Daemon-tier per-session (shared=false) servers, excluding any slug a
        // CONNECTED project server overrode.
        let daemon_per_session = self.daemon_per_session_entries(&connected_project_slugs);
        self.ensure_entries(
            session_id,
            None,
            &daemon_per_session,
            deadline,
            &mut used,
            &mut tools,
            &mut groups,
            &mut statuses,
        );

        SessionMcpOverlay {
            tools: Arc::new(ProjectToolSet::new(tools, groups)),
            shadowed_groups: shadowed,
            project_root: project_root.map(Path::to_path_buf),
            project_trusted: trusted,
            ignored_project_servers: ignored,
            statuses,
        }
    }

    /// The daemon-tier `shared = false` entries a session's overlay should
    /// connect, excluding any slug a CONNECTED project server overrides (its
    /// whole-entry project config replaces the daemon server of the same name).
    ///
    /// `suppressed` is the set of project slugs whose servers actually
    /// connected; a project entry that failed to connect does not appear, so the
    /// daemon server of that slug is still connected here.
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

    /// Connect/ensure the slots for `entries`, appending their wrappers to
    /// `tools`. `root` is `None` for daemon-tier per-session servers. Returns
    /// the slugs whose tools were built successfully — a server that failed to
    /// connect/list, or that ran out of the shared `deadline`, is skipped and
    /// does not appear.
    #[expect(
        clippy::too_many_arguments,
        reason = "the resolve accumulates into several caller-owned out-params (used/tools/groups/statuses); bundling them into a struct would add indirection without clarifying anything"
    )]
    fn ensure_entries(
        &mut self,
        session_id: u64,
        root: Option<&Path>,
        entries: &[McpEntry],
        deadline: Instant,
        used: &mut HashSet<String>,
        tools: &mut Vec<Box<dyn ToolDyn>>,
        groups: &mut HashSet<String>,
        statuses: &mut Vec<McpServerStatus>,
    ) -> Vec<String> {
        let mut connected: Vec<String> = Vec::new();
        for entry in entries {
            let slug = entry.config.slug.clone();
            // Bound each connect by the remaining share of the overlay's total
            // budget; once it is spent, defer the rest to the next resolve
            // rather than stalling the command loop further.
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                warn!(
                    server = %slug,
                    "MCP overlay connect budget exhausted; deferring remaining servers to the next resolve"
                );
                continue;
            }
            let timeout = remaining.min(RELOAD_BUDGET);
            let handle = if entry.shared {
                let Some(root) = root else {
                    continue;
                };
                self.ensure_shared(session_id, root, &entry.config, timeout)
            } else {
                self.ensure_session_slot(session_id, root, &entry.config, timeout)
            };
            let Some((handle, config)) = handle else {
                continue;
            };
            match Self::build_server_tools(
                &slug,
                &handle,
                &config.disabled_tools,
                used,
                tools,
                groups,
            ) {
                Ok(count) => {
                    let (credential_key, oauth, auth) = crate::mcp::oauth::status_auth(&config);
                    statuses.push(McpServerStatus {
                        slug: slug.clone(),
                        tier: if root.is_some() {
                            "project".to_string()
                        } else {
                            "daemon".to_string()
                        },
                        transport: config.transport.label().to_string(),
                        target: config.transport.target().to_string(),
                        connected: true,
                        tool_count: count,
                        server_name: Some(handle.name().to_string()),
                        server_version: Some(handle.version().to_string()),
                        last_error: None,
                        credential_key,
                        oauth,
                        auth,
                    });
                    connected.push(slug);
                }
                Err(e) => warn!(server = %slug, error = %e, "failed to list project MCP tools"),
            }
        }
        connected
    }

    /// Ensure a project-shared connection for `(root, slug)`, adding the
    /// session to its reference set. Returns the handle + config. A new
    /// connection is given at most `timeout`.
    fn ensure_shared(
        &mut self,
        session_id: u64,
        root: &Path,
        config: &McpServerConfig,
        timeout: Duration,
    ) -> Option<(McpServerHandle, McpServerConfig)> {
        let key = (root.to_path_buf(), config.slug.clone());
        if let Some(shared) = self.project_shared.get_mut(&key) {
            shared.sessions.insert(session_id);
            return Some((shared.slot.handle.clone(), shared.slot.config.clone()));
        }
        let slot = Self::connect_slot(&self.list_change_tx, config, timeout)?;
        let handle = slot.handle.clone();
        let cfg = slot.config.clone();
        self.project_shared.insert(
            key,
            SharedSlot {
                slot,
                sessions: HashSet::from([session_id]),
            },
        );
        info!(server = %config.slug, root = %root.display(), "connected shared project MCP server");
        Some((handle, cfg))
    }

    /// Ensure a per-session connection for `(session_id, root, slug)`. A new
    /// connection is given at most `timeout`.
    fn ensure_session_slot(
        &mut self,
        session_id: u64,
        root: Option<&Path>,
        config: &McpServerConfig,
        timeout: Duration,
    ) -> Option<(McpServerHandle, McpServerConfig)> {
        let key = (session_id, root.map(Path::to_path_buf), config.slug.clone());
        if let Some(slot) = self.session_slots.get(&key) {
            return Some((slot.handle.clone(), slot.config.clone()));
        }
        let slot = Self::connect_slot(&self.list_change_tx, config, timeout)?;
        let handle = slot.handle.clone();
        let cfg = slot.config.clone();
        self.session_slots.insert(key, slot);
        info!(server = %config.slug, session_id, "connected per-session MCP server");
        Some((handle, cfg))
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

    /// Re-resolve a session's overlay: release its current connections, then
    /// ensure against the (possibly new) project/trust.
    pub fn reload_session(
        &mut self,
        session_id: u64,
        project_root: Option<&Path>,
        trusted: bool,
    ) -> SessionMcpOverlay {
        self.release_session(session_id);
        self.ensure_session(session_id, project_root, trusted)
    }

    /// Build the tool wrappers for one server WITHOUT registering them in a
    /// `ToolRegistry`; they are returned to the session as its private overlay.
    ///
    /// Delegates the name resolution, disabled filter, and resource-catalogue
    /// construction to [`build_server_wrappers`] (the same builder the
    /// daemon-wide registration uses), so the two paths cannot drift.
    fn build_server_tools(
        slug: &str,
        handle: &McpServerHandle,
        disabled: &[String],
        used: &mut HashSet<String>,
        out: &mut Vec<Box<dyn ToolDyn>>,
        groups: &mut HashSet<String>,
    ) -> Result<usize, choreo_mcp::McpError> {
        let tools = handle.list_tools_with_deadline(CATALOGUE_REFRESH_BUDGET)?;
        let built = build_server_wrappers(slug, handle, &tools, disabled, used);
        groups.insert(built.group);
        out.extend(built.tools.into_iter().map(|(_, wrapper)| wrapper));
        Ok(built.tool_count)
    }

    /// The `session_slots` keys naming a DAEMON-tier per-session connection
    /// (`root = None`) for `slug`.
    ///
    /// A key with `root = Some(..)` is a PROJECT-tier per-session connection and
    /// is reconciled by the overlay resolution, never by a daemon-tier reload;
    /// only the daemon tier's keys are returned here.
    pub(super) fn daemon_per_session_slot_keys(
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
    /// A stale slot is one whose server's config changed or that no longer
    /// exists; leaving it in `session_slots` would make `ensure_session_slot`
    /// return the old connection under the same key forever.
    pub(super) fn drop_daemon_session_slots(&mut self, slug: &str) -> HashSet<u64> {
        let mut sessions = HashSet::new();
        for key in self.daemon_per_session_slot_keys(slug) {
            if self.session_slots.remove(&key).is_some() {
                sessions.insert(key.0);
            }
        }
        sessions
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
    /// the returned overlay, adds the session to an existing project-shared
    /// connection's reference set (the cheap half of `ensure_shared`), and hands
    /// back the entries a worker must connect. Reused connections cost nothing —
    /// their tools come straight from the cached tool set, so the common
    /// re-resolve is a pure in-memory pass.
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

    /// Append one pooled slot's CACHED tools to an overlay build (no re-list).
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
        let count = built.tool_count;
        tools.extend(built.tools.into_iter().map(|(_, wrapper)| wrapper));
        let (credential_key, oauth, auth) = crate::mcp::oauth::status_auth(&slot.config);
        statuses.push(McpServerStatus {
            slug: slug.to_string(),
            tier: tier.to_string(),
            transport: slot.config.transport.label().to_string(),
            target: slot.config.transport.target().to_string(),
            connected: true,
            tool_count: count,
            server_name: Some(slot.handle.name().to_string()),
            server_version: Some(slot.handle.version().to_string()),
            last_error: None,
            credential_key,
            oauth,
            auth,
        });
    }

    /// Install a worker-connected overlay entry into the pool: a project entry
    /// into `project_shared` (ref-counted for `session_id`), a daemon per-session
    /// entry into `session_slots`.
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
                    .insert(key, SharedSlot { slot, sessions });
            }
            None => {
                self.session_slots
                    .insert((session_id, None, entry.slug), slot);
            }
        }
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
