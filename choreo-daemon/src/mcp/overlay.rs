//! The per-session MCP overlay: the private tool set a session carries for ITS
//! project (plus any per-session `shared = false` daemon server), the root walk
//! that identifies that project, and the resolution that (re)connects the
//! servers behind it.
//!
//! The overlay value types ([`ProjectToolSet`], [`SessionMcpOverlay`]) and
//! [`project_root_for`] are pure and compile unconditionally (so a session can
//! hold its overlay in any build). The resolution methods on the manager are
//! `mcp`-feature-gated like the manager itself; their `impl` block lives here
//! rather than in `mod.rs` so they keep the manager's private fields and helpers
//! in scope as a child module.

use super::McpServerStatus;
#[cfg(feature = "mcp")]
use super::config::{self, McpEntry};
#[cfg(feature = "mcp")]
use super::tool::{McpListResourcesTool, McpReadResourceTool, McpToolWrapper};
#[cfg(feature = "mcp")]
use super::{CATALOGUE_REFRESH_BUDGET, RELOAD_BUDGET, SharedSlot};
use crate::tools::{ToolDyn, ToolError, ToolOutput, ToolOutputFormat};
use choreo_ai_protocols::openai::ChatToolDefinition;
#[cfg(feature = "mcp")]
use choreo_mcp::{McpServerConfig, McpServerHandle};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
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

/// The set of tool wrappers a session carries for ITS project's MCP servers
/// (and any per-session, `shared = false` daemon-tier servers).
///
/// This is the session's private overlay: it never enters the daemon-wide
/// `ToolRegistry` (which holds only core + daemon-tier shared servers + static
/// groups). The request path merges this set's definitions on top of the
/// registry's (with every daemon-tier group the session's project shadows
/// removed), and the execution path consults it BEFORE the shared registry.
///
/// Compiled unconditionally (it wraps `dyn ToolDyn`, which is always present)
/// so a session's `SessionState` can hold an `Arc<ProjectToolSet>` in every
/// build; without the `mcp` feature the set is simply always empty.
#[derive(Default)]
pub struct ProjectToolSet {
    /// The wrapped tools, in registration order.
    tools: Vec<Box<dyn ToolDyn>>,
    /// `name -> index into tools`, so `has`/`describe_invocation_json`/`execute_*`
    /// are O(1) rather than a linear scan. Names are unique within the set
    /// (`resolve_name` dedupes against the resolve's shared `used` set), so each
    /// tool appears exactly once; kept in lockstep with `tools`, which preserves
    /// registration order for `definitions`.
    index: HashMap<String, usize>,
    /// The `mcp/<slug>` groups these tools belong to — the groups whose
    /// daemon-tier counterparts the session must shadow.
    groups: HashSet<String>,
}

impl ProjectToolSet {
    /// An empty tool set.
    #[must_use]
    pub fn empty() -> Self {
        Self {
            tools: Vec::new(),
            index: HashMap::new(),
            groups: HashSet::new(),
        }
    }

    /// Build a set from a session's collected overlay tools, indexing every
    /// tool by name for O(1) lookup. The `Vec` keeps registration order for
    /// `definitions`; the index is derived from it so the two cannot drift.
    /// Gated with the resolution methods that call it (only the `mcp` feature
    /// builds a non-empty set).
    #[cfg(feature = "mcp")]
    fn new(tools: Vec<Box<dyn ToolDyn>>, groups: HashSet<String>) -> Self {
        let index: HashMap<String, usize> = tools
            .iter()
            .enumerate()
            .map(|(i, tool)| (tool.name().to_string(), i))
            .collect();
        Self {
            tools,
            index,
            groups,
        }
    }

    /// The tool named `name`, if the set holds one, via the name index.
    fn find(&self, name: &str) -> Option<&dyn ToolDyn> {
        // `index` is `name -> index into tools` and is built from the same
        // `Vec`, so the lookup is total; `get` keeps it panic-free either way.
        let index = *self.index.get(name)?;
        self.tools.get(index).map(AsRef::as_ref)
    }

    /// Whether the set holds no tools.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.tools.is_empty()
    }

    /// How many tools the set holds.
    #[must_use]
    pub fn len(&self) -> usize {
        self.tools.len()
    }

    /// The `mcp/<slug>` groups this set provides.
    #[must_use]
    pub fn groups(&self) -> &HashSet<String> {
        &self.groups
    }

    /// Whether a tool with `name` lives in this set.
    #[must_use]
    pub fn has(&self, name: &str) -> bool {
        self.index.contains_key(name)
    }

    /// A tool definition for every tool in the set (Text/JSON-compatible).
    #[must_use]
    pub fn definitions(&self) -> Vec<ChatToolDefinition> {
        self.tools
            .iter()
            .map(|t| ChatToolDefinition::function(t.name(), t.description(), t.schema()))
            .collect()
    }

    /// Describe a call against a tool in this set, or `None` if unknown.
    #[must_use]
    pub fn describe_invocation_json(&self, name: &str, args_json: &str) -> Option<String> {
        self.find(name)
            .map(|t| t.describe_invocation_json(args_json))
    }

    /// Execute a JSON tool call against this set, or `None` when the tool is
    /// not held here (so the caller falls back to the shared registry).
    #[must_use]
    pub fn execute_json(
        &self,
        tool_call: &choreo_ai_protocols::ChatToolCall,
        format: ToolOutputFormat,
        x_credentials: Option<&choreo_keystore::ServiceCredential>,
        working_dir: Option<&Path>,
        ctx: Option<&crate::tools::context::ToolContext>,
        image_tx: Option<crossbeam_channel::Sender<crate::tools::PreparedImage>>,
    ) -> Option<Result<ToolOutput, ToolError>> {
        self.find(&tool_call.name).map(|t| {
            t.execute_json(
                &tool_call.arguments_json,
                format,
                x_credentials,
                working_dir,
                ctx,
                image_tx,
            )
        })
    }

    /// Execute a streaming JSON tool call against this set, or `None` when the
    /// tool is not held here.
    #[must_use]
    #[expect(
        clippy::too_many_arguments,
        reason = "mirrors the ToolDyn::execute_streaming_json signature field-for-field so the set is a drop-in front for the registry"
    )]
    pub fn execute_streaming_json(
        &self,
        tool_call: &choreo_ai_protocols::ChatToolCall,
        format: ToolOutputFormat,
        output_tx: crossbeam_channel::Sender<Vec<u8>>,
        x_credentials: Option<&choreo_keystore::ServiceCredential>,
        working_dir: Option<&Path>,
        ctx: Option<&crate::tools::context::ToolContext>,
        image_tx: Option<crossbeam_channel::Sender<crate::tools::PreparedImage>>,
    ) -> Option<Result<ToolOutput, ToolError>> {
        self.find(&tool_call.name).map(|t| {
            t.execute_streaming_json(
                &tool_call.arguments_json,
                format,
                x_credentials,
                working_dir,
                output_tx,
                ctx,
                image_tx,
            )
        })
    }

    /// Execute a postcard tool call against this set, or `None` when the tool
    /// is not held here.
    #[must_use]
    pub fn execute_postcard(
        &self,
        name: &str,
        args_bytes: &[u8],
        x_credentials: Option<&choreo_keystore::ServiceCredential>,
        working_dir: Option<&Path>,
        ctx: Option<&crate::tools::context::ToolContext>,
    ) -> Option<Vec<u8>> {
        self.find(name)
            .map(|t| t.execute_postcard(args_bytes, x_credentials, working_dir, ctx))
    }
}

/// A session's MCP overlay, produced by the daemon when a session's project is
/// (re)resolved: the private project/per-session tools plus the daemon-tier
/// groups the session's project shadows.
#[derive(Default, Clone)]
pub struct SessionMcpOverlay {
    /// The session's private tool set (project servers plus `shared = false`
    /// per-session servers).
    pub tools: Arc<ProjectToolSet>,
    /// `mcp/<slug>` groups to remove from the session's registry view (their
    /// project counterpart replaces them).
    pub shadowed_groups: HashSet<String>,
    /// The session's project root, if any.
    pub project_root: Option<PathBuf>,
    /// Whether that root is trusted.
    pub project_trusted: bool,
    /// Slugs declared by an untrusted `.mcp.json` (ignored, never spawned).
    pub ignored_project_servers: Vec<String>,
    /// The project/per-session server statuses (tier-tagged).
    pub statuses: Vec<McpServerStatus>,
}

impl SessionMcpOverlay {
    /// An empty overlay (no project, no per-session servers).
    #[must_use]
    pub fn empty() -> Self {
        Self {
            tools: Arc::new(ProjectToolSet::empty()),
            shadowed_groups: HashSet::new(),
            project_root: None,
            project_trusted: false,
            ignored_project_servers: Vec::new(),
            statuses: Vec::new(),
        }
    }
}

/// Resolve a session's project MCP root from its working directory: walk UP
/// from `working_dir` to the git root (inclusive), and return the directory of
/// the first `.mcp.json` found. The owning directory is the project's identity
/// AND its trust key.
///
/// A session without a working directory has no project tier (`None`).
/// Compiled unconditionally (pure path logic) so the daemon can resolve a
/// root regardless of the `mcp` feature.
#[must_use]
pub fn project_root_for(working_dir: &Path) -> Option<PathBuf> {
    let git_root = crate::context::find_git_root(working_dir);
    // The boundary is the git root when there is one (the walk never climbs
    // above it — a `.mcp.json` outside the repository is not this project's),
    // else the filesystem root.
    let boundary = git_root.unwrap_or_else(|| PathBuf::from("/"));
    let mut current = Some(working_dir.to_path_buf());
    while let Some(dir) = current {
        if dir.join(super::PROJECT_CONFIG_FILE).is_file() {
            return Some(dir);
        }
        if dir == boundary {
            break;
        }
        let parent = dir.parent().map(Path::to_path_buf);
        if parent.as_deref() == Some(dir.as_path()) {
            break;
        }
        current = parent;
    }
    None
}

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

        // Resolve the project entries. Their slugs suppress a same-slug daemon
        // per-session server (the project override is whole-entry) — but ONLY
        // for a TRUSTED project: an untrusted `.mcp.json` is merely reported,
        // so its slugs must not suppress a daemon per-session server.
        let mut project_slugs: HashSet<String> = HashSet::new();
        let mut project_entries: Vec<McpEntry> = Vec::new();
        if let Some(root) = project_root {
            match config::load_project_config(root, trusted) {
                Ok(Some(entries)) => {
                    if trusted {
                        project_slugs.extend(entries.iter().map(|e| e.config.slug.clone()));
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

        // Daemon-tier per-session (shared=false) servers, unless a trusted
        // project overrides that slug.
        let daemon_per_session: Vec<McpEntry> = self
            .configs
            .values()
            .filter(|e| !e.shared && !project_slugs.contains(&e.config.slug))
            .cloned()
            .collect();

        // Track which project slugs the session references, for release.
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
            // A project slug shadows the daemon-tier `mcp/<slug>` group of the
            // same name ONLY when its server actually connected (its tools were
            // built): a project entry that failed to connect, or was skipped on
            // the connect deadline, must NOT remove the daemon-tier group from
            // the catalogue.
            for slug in connected {
                shadowed.insert(choreo_mcp::group_name(&slug));
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

    /// Connect/ensure the slots for `entries`, appending their wrappers to
    /// `tools`. `root` is `None` for daemon-tier per-session servers. Returns
    /// the slugs whose tools were built successfully — a server that failed to
    /// connect/list, or that ran out of the shared `deadline`, is skipped and
    /// does not appear.
    #[expect(clippy::too_many_arguments)]
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
    fn build_server_tools(
        slug: &str,
        handle: &McpServerHandle,
        disabled: &[String],
        used: &mut HashSet<String>,
        out: &mut Vec<Box<dyn ToolDyn>>,
        groups: &mut HashSet<String>,
    ) -> Result<usize, choreo_mcp::McpError> {
        let tools = handle.list_tools_with_deadline(CATALOGUE_REFRESH_BUDGET)?;
        let group = choreo_mcp::group_name(slug);
        groups.insert(group.clone());
        let disabled: HashSet<&str> = disabled.iter().map(String::as_str).collect();

        if handle.supports_resources() {
            for suffix in ["list_resources", "read_resource"] {
                let name = Self::resolve_name(slug, suffix, used);
                used.insert(name);
            }
        }

        let mut count = 0usize;
        for mcp_tool in tools {
            if disabled.contains(mcp_tool.name.as_str()) {
                debug!(server = %slug, tool = %mcp_tool.name, "MCP tool disabled by config");
                continue;
            }
            let description = mcp_tool.description.unwrap_or_default();
            let name = Self::resolve_name(slug, &mcp_tool.name, used);
            used.insert(name.clone());
            out.push(Box::new(McpToolWrapper::with_name(
                name.clone(),
                group.clone(),
                format!("[MCP {slug}] {description}"),
                mcp_tool.name,
                mcp_tool.input_schema,
                mcp_tool.output_schema,
                handle.clone(),
            )));
            count += 1;
        }

        if handle.supports_resources() {
            out.push(Box::new(McpListResourcesTool::new(slug, handle.clone())));
            out.push(Box::new(McpReadResourceTool::new(slug, handle.clone())));
        }
        Ok(count)
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
    use super::project_root_for;

    #[test]
    fn project_root_for_finds_nearest_dot_mcp_json() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join(".git")).unwrap();
        let nested = root.join("sub").join("deep");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::write(root.join(".mcp.json"), "{}").unwrap();
        // The walk climbs to the directory holding `.mcp.json`.
        assert_eq!(project_root_for(&nested), Some(root.to_path_buf()));
    }

    #[test]
    fn project_root_for_returns_none_without_a_file() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join(".git")).unwrap();
        assert_eq!(project_root_for(dir.path()), None);
    }
}
