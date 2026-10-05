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
use super::{CATALOGUE_REFRESH_BUDGET, RELOAD_BUDGET, SharedSlot};
#[cfg(feature = "mcp")]
use crate::tools::ToolDyn;
#[cfg(feature = "mcp")]
use choreo_mcp::{McpServerConfig, McpServerHandle};
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
    /// [`SESSION_CONNECT_BUDGET`], so a project whose servers are slow to start
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

        // One connect deadline shared by every server this resolve ensures, so
        // the aggregate is bounded (not one budget per server).
        let deadline = Instant::now() + SESSION_CONNECT_BUDGET;

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

    /// Remove every daemon-tier per-session slot (`root = None`) for `slug`,
    /// returning the sessions that held them.
    ///
    /// A stale slot is one whose server's config changed or that no longer
    /// exists; leaving it in `session_slots` would make `ensure_session_slot`
    /// return the old connection under the same key forever.
    pub(super) fn drop_daemon_session_slots(&mut self, slug: &str) -> HashSet<u64> {
        let stale: Vec<(u64, Option<PathBuf>, String)> = self
            .session_slots
            .keys()
            .filter(|(_, root, s)| root.is_none() && s == slug)
            .cloned()
            .collect();
        let mut sessions = HashSet::new();
        for key in stale {
            if self.session_slots.remove(&key).is_some() {
                sessions.insert(key.0);
            }
        }
        sessions
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
