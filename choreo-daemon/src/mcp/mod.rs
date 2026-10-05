// Real implementation (connect/handshake/discover/shutdown over stdio) is
// compiled only with the `mcp` feature. Without it, the module below degrades
// to a no-op stub (see the `#[cfg(not(feature = "mcp"))]` block) so the
// manager's call sites in cli.rs / daemon.rs / server/lifecycle.rs compile
// unchanged in both configurations.
#[cfg(feature = "mcp")]
pub mod config;
#[cfg(feature = "mcp")]
pub mod tool;
// The trust store has no `choreo-mcp` dependency (only serde/toml/std), so it
// compiles unconditionally: the daemon's trust field then needs no feature
// cfgs, and the `mcp`-off stub still reports an empty trust set.
pub mod trust;
// The manager is split across cohesive child modules: `overlay` owns the
// per-session overlay resolution plus the overlay value types and project-root
// walk, `pool` owns the pool reconciliation (reconnect/reload) and the shared
// slot connect helper. Their `impl McpManager` blocks reach the manager's
// private fields as child modules; the items other modules use are re-exported
// here so every `crate::mcp::…` path is unchanged.
mod overlay;
mod pool;

pub use overlay::{ProjectToolSet, SessionMcpOverlay, project_root_for};

#[cfg(feature = "mcp")]
use crate::tools::ToolDyn;
#[cfg(feature = "mcp")]
use crate::tools::ToolRegistry;
#[cfg(feature = "mcp")]
use choreo_mcp::{McpListChange, McpServer, McpServerConfig, McpServerHandle, McpTool};
#[cfg(feature = "mcp")]
use config::McpEntry;
#[cfg(feature = "mcp")]
use std::collections::HashMap;
#[cfg(feature = "mcp")]
use std::collections::HashSet;
#[cfg(feature = "mcp")]
use std::path::Path;
use std::path::PathBuf;
#[cfg(feature = "mcp")]
use std::time::{Duration, Instant};
#[cfg(feature = "mcp")]
use tool::{McpListResourcesTool, McpReadResourceTool, McpToolWrapper};
#[cfg(feature = "mcp")]
use tracing::{debug, error, info, warn};

/// A background connect's join handle: the connected server plus its listed
/// tools, or the connect/list error.
///
/// The listing runs inside the same bounded worker as the connect (see
/// [`connect_and_list`]), so the caller's budget covers connect AND discovery.
#[cfg(feature = "mcp")]
type PendingConnect = (
    String,
    std::thread::JoinHandle<anyhow::Result<(McpServer, Vec<McpTool>)>>,
);

/// The project-tier MCP config file name (a checkout's own server set).
///
/// The MCP-ecosystem convention for a repository-local server declaration. The
/// daemon-tier counterpart lives beside this crate's other config files
/// (`<config>/choreographr/mcp.json`).
pub const PROJECT_CONFIG_FILE: &str = ".mcp.json";

/// The MCP trust-store file name (`<config>/choreographr/trust.toml`).
pub const TRUST_FILE: &str = "trust.toml";

thread_local! {
    /// Test-only override for the base config directory. When set,
    /// [`config_dir`] returns `<root>/choreographr` instead of the user's real
    /// config dir.
    ///
    /// Deliberately NOT `#[cfg(test)]`-gated: integration tests in `tests/`
    /// compile the crate without `cfg(test)`, so the hook must exist in normal
    /// builds too (it is a no-op unless explicitly set).
    static TEST_CONFIG_ROOT: std::cell::RefCell<Option<PathBuf>> =
        const { std::cell::RefCell::new(None) };
}

/// Test-only override for the base config directory (see `TEST_CONFIG_ROOT`).
///
/// This is needed because `dirs::config_dir()` honors `XDG_CONFIG_HOME` only
/// on Linux — on macOS it always returns `$HOME/Library/Application Support`,
/// so an integration test cannot redirect the config path via environment
/// variables.
#[doc(hidden)]
pub fn set_test_config_root(root: Option<PathBuf>) {
    TEST_CONFIG_ROOT.with(|cell| cell.replace(root));
}

/// Resolve the choreographr config directory (`<config>/choreographr`).
///
/// # Errors
///
/// Returns an error when the config directory cannot be determined.
pub fn config_dir() -> std::io::Result<PathBuf> {
    if let Some(root) = TEST_CONFIG_ROOT.with(|cell| cell.borrow().clone()) {
        return Ok(root.join("choreographr"));
    }
    choreo_shared::paths::config_dir()
}

/// The path of the MCP trust store (`<config>/choreographr/trust.toml`), or
/// `None` when the config directory cannot be resolved.
#[must_use]
pub fn trust_path() -> Option<PathBuf> {
    config_dir().ok().map(|d| d.join(TRUST_FILE))
}

/// Default budget for connecting all MCP servers during `from_config`.
///
/// A hung server must never stall daemon startup: the manager gives the whole
/// batch this long to connect, then registers whatever is ready and leaves the
/// rest (their threads detach and the server is dropped, so nothing leaks).
#[cfg(feature = "mcp")]
const STARTUP_BUDGET: Duration = Duration::from_secs(2);

/// Budget for a single manual reconnect attempt (the `/mcp` surface's
/// reconnect action). Longer than the per-server startup share because it is
/// user-initiated and can afford to wait for one server.
#[cfg(feature = "mcp")]
const RECONNECT_BUDGET: Duration = Duration::from_secs(10);

/// Per-server budget for a config reload (the `/mcp reload` action). Reload
/// reconciles every configured server, so a (re)connect attempt gets the same
/// generous per-server share as a manual reconnect; servers are attempted one
/// at a time.
#[cfg(feature = "mcp")]
const RELOAD_BUDGET: Duration = Duration::from_secs(10);

/// Budget for a synchronous tool-listing sweep the command loop performs when it
/// refreshes the tool catalogue (a list-changed event, a reload, or a
/// reconnect) or (re)resolves a session overlay.
///
/// Deliberately distinct from, and far shorter than, the per-server request
/// timeout (60 s by default): these sweeps run on the command-loop thread, so a
/// single slow server must not freeze every session and client behind it. A
/// server that misses this deadline is skipped from that refresh with a warning
/// and keeps its previous registration, rather than stalling the loop. A few
/// seconds is ample for a healthy server, whose `tools/list` round-trip is
/// normally milliseconds.
#[cfg(feature = "mcp")]
const CATALOGUE_REFRESH_BUDGET: Duration = Duration::from_secs(3);

/// A read-only snapshot of one configured server's state, for the `/mcp`
/// status surface and `session_inspect`.
///
/// Defined unconditionally (outside the `mcp` feature gate) so a status-returning
/// daemon command and its callers compile no matter how the daemon is built;
/// without the feature the list is simply always empty.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpServerStatus {
    /// The server's config key (tool-name prefix).
    pub slug: String,
    /// The tier this server belongs to: `"daemon"` or `"project"`.
    pub tier: String,
    /// The resolved transport label (`"stdio"` / `"http"`).
    pub transport: String,
    /// The command (stdio) or URL (http) the transport targets.
    pub target: String,
    /// Whether the server is connected and its tools are registered.
    pub connected: bool,
    /// How many tools (excluding the resource catalogue tools) are registered.
    pub tool_count: usize,
    /// The server's self-reported name, once connected.
    pub server_name: Option<String>,
    /// The server's self-reported version, once connected.
    pub server_version: Option<String>,
    /// The last connect/refresh error, when the server is not connected (or a
    /// refresh failed).
    pub last_error: Option<String>,
}

impl McpServerStatus {
    /// A one-line human-readable summary of this server's state.
    #[must_use]
    pub fn summary(&self) -> String {
        if self.connected {
            let name = self.server_name.as_deref().unwrap_or("unknown");
            let version = self.server_version.as_deref().unwrap_or("?");
            format!(
                "{} [{} → {}] connected: {name} {version}, {} tool(s)",
                self.slug, self.transport, self.target, self.tool_count
            )
        } else {
            format!(
                "{} [{} → {}] not connected: {}",
                self.slug,
                self.transport,
                self.target,
                self.last_error.as_deref().unwrap_or("not connected")
            )
        }
    }
}

/// The result of a successful MCP config reload, for the `/mcp reload`
/// surface.
///
/// Defined unconditionally (outside the `mcp` feature gate) so the status-
/// returning daemon command and its reply type compile no matter how the
/// daemon is built; without the feature a reload never succeeds.
#[derive(Debug, Clone)]
pub struct McpReloadOutcome {
    /// One-line human-readable summary of what changed, e.g.
    /// `"MCP reload: 1 added, 0 removed, 1 restarted, 2 unchanged, 0 failed"`.
    pub summary: String,
    /// The refreshed state of every configured server, in stable slug order.
    pub servers: Vec<McpServerStatus>,
    /// The sessions whose overlay referenced a daemon per-session
    /// (`shared = false`) server that this reload changed or removed, in sorted
    /// order. The command loop re-resolves exactly these overlays so they pick
    /// up the new config (or drop the removed one). Daemon-internal bookkeeping:
    /// not carried on the wire.
    #[doc(hidden)]
    pub affected_sessions: Vec<u64>,
}

/// A full MCP status report for one session: every visible server — daemon-tier
/// plus (for an attached session) that session's own project servers — tagged
/// by tier, plus the session's project-root trust context.
#[derive(Debug, Clone)]
pub struct McpStatusReport {
    /// Every visible server, daemon-tier and project-tier, in stable order.
    pub servers: Vec<McpServerStatus>,
    /// The session's resolved project root, if any.
    pub project_root: Option<PathBuf>,
    /// Whether `project_root` (when present) is trusted.
    pub project_trusted: bool,
    /// Slugs declared by an UNTRUSTED `.mcp.json` — read so the operator can see
    /// what is ignored, never spawned.
    pub ignored_project_servers: Vec<String>,
}

impl McpStatusReport {
    /// An empty report (no servers, no project context) — the stub baseline.
    #[must_use]
    pub fn empty() -> Self {
        Self {
            servers: Vec::new(),
            project_root: None,
            project_trusted: false,
            ignored_project_servers: Vec::new(),
        }
    }
}

/// The outcome of a `/mcp trust` / `/mcp untrust` request: the resulting trust
/// state of the target root (`None` when the active session has no resolvable
/// project root) and a one-line human-readable summary.
#[derive(Debug, Clone)]
pub struct McpTrustOutcome {
    /// The canonical project root the decision applied to, if any.
    pub root: Option<PathBuf>,
    /// Whether `root` is now trusted.
    pub trusted: bool,
    /// A one-line human-readable summary of what happened.
    pub message: String,
}

/// One connected server: the live connection (owns the dispatcher thread) plus
/// a cloneable handle shared with that server's tool wrappers.
#[cfg(feature = "mcp")]
struct ServerSlot {
    handle: McpServerHandle,
    /// Dropped on shutdown; its `Drop` sends the shutdown command and joins the
    /// dispatcher with a bounded wait.
    #[expect(
        dead_code,
        reason = "kept only for its Drop (bounded dispatcher shutdown)"
    )]
    server: McpServer,
    /// The resolved config this server was connected from; kept so a manual
    /// reconnect can re-attempt the same server.
    config: McpServerConfig,
    /// How many tools are registered for this server (excluding the resource
    /// catalogue tools).
    tool_count: usize,
    /// The tools registered by the most recent successful listing. Kept so a
    /// catalogue-refresh sweep that misses [`CATALOGUE_REFRESH_BUDGET`] can fall
    /// back to the previous registration instead of dropping the server's tools
    /// from the rebuilt catalogue.
    tools: Vec<McpTool>,
}

/// A pooled, ref-counted project-shared server: one connection shared by every
/// session that references the same `(project_root, slug)`.
#[cfg(feature = "mcp")]
struct SharedSlot {
    slot: ServerSlot,
    /// The sessions currently referencing this connection. The connection is
    /// dropped when the last one leaves the project.
    sessions: HashSet<u64>,
}

/// Manages all MCP server connections and their registered tools.
#[cfg(feature = "mcp")]
pub struct McpManager {
    /// Daemon-tier SHARED servers (one connection, keyed by slug). Daemon-tier
    /// `shared = false` servers are NOT here — they get a per-session
    /// connection, keyed in `session_slots`.
    servers: HashMap<String, ServerSlot>,
    /// The resolved config of EVERY daemon-tier server (connected or not),
    /// keyed by slug, so a failed/skipped server can be retried and reported.
    configs: HashMap<String, McpEntry>,
    /// The configured daemon-tier slugs in a stable (sorted) order.
    order: Vec<String>,
    /// Startup connect failures, keyed by slug, for the status surface.
    failures: HashMap<String, String>,
    /// Project-shared connections, keyed by `(project_root, slug)`, ref-counted
    /// by the sessions referencing them.
    project_shared: HashMap<(PathBuf, String), SharedSlot>,
    /// Per-session private connections (`shared = false`), keyed by
    /// `(session_id, project_root or None, slug)`.
    session_slots: HashMap<(u64, Option<PathBuf>, String), ServerSlot>,
    /// Sender half of the shared list-changed channel, kept so a reconnect can
    /// re-subscribe the rebuilt transport.
    list_change_tx: crossbeam_channel::Sender<McpListChange>,
    /// Receiver end of the shared list-changed channel every server's
    /// `subscriptions/listen` stream feeds. The daemon takes it out (via
    /// [`McpManager::take_list_change_rx`]) and pumps it into the command loop;
    /// `None` once taken, or when no server could open a subscription.
    list_change_rx: Option<crossbeam_channel::Receiver<McpListChange>>,
}

#[cfg(feature = "mcp")]
impl McpManager {
    /// Connect every enabled daemon-tier SHARED server and register its tools
    /// in the `ToolRegistry`.
    ///
    /// `shared = false` daemon-tier servers are NOT connected here: they are
    /// per-session, connected lazily when a session resolves its overlay (see
    /// [`McpManager::ensure_session`]). Servers connect in parallel on
    /// background threads; the whole batch is bounded by [`STARTUP_BUDGET`].
    pub fn from_config(registry: &mut ToolRegistry) -> Self {
        let mut configs: Vec<McpEntry> = match config::load_daemon_config() {
            Ok(configs) => configs,
            Err(e) => {
                warn!("failed to load MCP config: {e}");
                Vec::new()
            }
        };
        // Deterministic registration order (a HashMap-derived Vec is not), so a
        // name collision between two servers always resolves the same way and a
        // reconnect reproduces the same names.
        configs.sort_by(|a, b| a.config.slug.cmp(&b.config.slug));

        let (list_change_tx, list_change_rx) = crossbeam_channel::unbounded::<McpListChange>();

        let mut manager = Self {
            servers: HashMap::new(),
            configs: configs
                .iter()
                .map(|c| (c.config.slug.clone(), c.clone()))
                .collect(),
            order: configs.iter().map(|c| c.config.slug.clone()).collect(),
            failures: HashMap::new(),
            project_shared: HashMap::new(),
            session_slots: HashMap::new(),
            list_change_tx: list_change_tx.clone(),
            list_change_rx: Some(list_change_rx),
        };

        // Spawn only the shared servers up front so they handshake in parallel.
        // Each worker connects AND performs the initial tool listing, so the
        // startup budget below bounds discovery too, not just the handshake.
        let mut pending: Vec<PendingConnect> = Vec::new();
        for cfg in configs {
            if !cfg.shared {
                debug!(server = %cfg.config.slug, "daemon-tier server is not shared; deferred to per-session connect");
                continue;
            }
            let slug = cfg.config.slug.clone();
            info!(
                server = %slug,
                transport = cfg.config.transport.label(),
                target = cfg.config.transport.target(),
                "spawning MCP server"
            );
            let list_changes = Some(list_change_tx.clone());
            let handle = std::thread::spawn(move || {
                connect_and_list(&cfg.config, list_changes).map_err(anyhow::Error::from)
            });
            pending.push((slug, handle));
        }

        let deadline = Instant::now() + STARTUP_BUDGET;
        let mut used: HashSet<String> = HashSet::new();
        for (slug, handle) in pending {
            let remaining = deadline.saturating_duration_since(Instant::now());
            match join_with_budget(handle, remaining) {
                Some(Ok((server, tools))) => {
                    if let Err(e) = Self::register_server(
                        &slug,
                        server,
                        tools,
                        &mut used,
                        registry,
                        &mut manager,
                    ) {
                        error!(server = %slug, error = %e, "failed to register MCP server");
                        manager.failures.insert(slug, e);
                    }
                }
                Some(Err(e)) => {
                    error!(server = %slug, error = %e, "failed to connect MCP server");
                    manager.failures.insert(slug, e.to_string());
                }
                None => {
                    warn!(
                        server = %slug,
                        budget_ms = STARTUP_BUDGET.as_millis(),
                        "MCP server did not connect within the startup budget; skipping"
                    );
                    manager.failures.insert(
                        slug,
                        format!(
                            "did not connect within the {}s startup budget",
                            STARTUP_BUDGET.as_secs()
                        ),
                    );
                }
            }
        }

        manager
    }

    /// Take the shared list-changed receiver, if a server could have opened a
    /// subscription. Called once by the daemon command-loop assembly, which
    /// spawns a forwarder thread pumping it into [`DaemonCommand::McpListChanged`].
    ///
    /// [`DaemonCommand::McpListChanged`]: crate::daemon::DaemonCommand::McpListChanged
    pub fn take_list_change_rx(&mut self) -> Option<crossbeam_channel::Receiver<McpListChange>> {
        self.list_change_rx.take()
    }

    /// Re-register every daemon-tier shared server's tools into `registry`.
    ///
    /// Each server is listed with the short [`CATALOGUE_REFRESH_BUDGET`]
    /// deadline, not the per-server request timeout: this sweep runs on the
    /// command loop, and one slow server must not freeze every session. A server
    /// that misses the deadline keeps its previously-listed tool set (so its
    /// group does not blink out of the catalogue) and is re-listed on the next
    /// refresh.
    pub fn register_all(&mut self, registry: &mut ToolRegistry) {
        let mut used: HashSet<String> = HashSet::new();
        for slug in self.order.clone() {
            let Some(slot) = self.servers.get_mut(&slug) else {
                continue;
            };
            let tools = match slot
                .handle
                .list_tools_with_deadline(CATALOGUE_REFRESH_BUDGET)
            {
                Ok(tools) => {
                    slot.tools.clone_from(&tools);
                    tools
                }
                Err(e) => {
                    warn!(server = %slug, error = %e, "MCP tool listing missed the catalogue-refresh budget; keeping the previous tool set");
                    slot.tools.clone()
                }
            };
            Self::register_server_tools(
                &slug,
                &slot.handle,
                tools,
                &slot.config.disabled_tools,
                &mut used,
                registry,
            );
        }
    }

    fn register_server(
        slug: &str,
        server: McpServer,
        tools: Vec<McpTool>,
        used: &mut HashSet<String>,
        registry: &mut ToolRegistry,
        manager: &mut Self,
    ) -> Result<(), String> {
        let handle = server.handle();
        let config = manager
            .configs
            .get(slug)
            .map(|e| e.config.clone())
            .ok_or_else(|| format!("no config for server {slug:?}"))?;
        let tool_count = Self::register_server_tools(
            slug,
            &handle,
            tools.clone(),
            &config.disabled_tools,
            used,
            registry,
        );
        manager.servers.insert(
            slug.to_string(),
            ServerSlot {
                handle,
                server,
                config,
                tool_count,
                tools,
            },
        );
        manager.failures.remove(slug);
        Ok(())
    }

    /// Register one server's advertised `tools` (and its catalogue group) into
    /// `registry`, returning the number of tools registered.
    ///
    /// The listing itself is done by the caller (inside its connect/refresh
    /// budget), so this is pure registration: it cannot fail.
    fn register_server_tools(
        slug: &str,
        handle: &McpServerHandle,
        tools: Vec<McpTool>,
        disabled: &[String],
        used: &mut HashSet<String>,
        registry: &mut ToolRegistry,
    ) -> usize {
        let server_name = handle.name().to_string();
        let group = choreo_mcp::group_name(slug);
        registry.register_dynamic_group(group.clone(), format!("MCP server: {server_name}"));
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
            let wrapper = McpToolWrapper::with_name(
                name.clone(),
                group.clone(),
                format!("[MCP {slug}] {description}"),
                mcp_tool.name,
                mcp_tool.input_schema,
                mcp_tool.output_schema,
                handle.clone(),
            );
            registry.register_dynamic(name, &group, Box::new(wrapper));
            count += 1;
        }

        if handle.supports_resources() {
            let lister = McpListResourcesTool::new(slug, handle.clone());
            let reader = McpReadResourceTool::new(slug, handle.clone());
            for tool in [
                Box::new(lister) as Box<dyn ToolDyn>,
                Box::new(reader) as Box<dyn ToolDyn>,
            ] {
                let name = tool.name().to_string();
                registry.register_dynamic(name, &group, tool);
            }
            info!(server = %slug, "registered MCP resource tools");
        }
        info!(
            server = %slug,
            name = %server_name,
            tool_count = count,
            "registered MCP server tools"
        );
        count
    }

    /// Compute the provider-safe name for `tool` on `slug`, appending a hash
    /// suffix when another server already claimed the sanitized name.
    fn resolve_name(slug: &str, tool: &str, used: &HashSet<String>) -> String {
        let base = choreo_mcp::build_tool_name(slug, tool);
        if !used.contains(&base) {
            return base;
        }
        choreo_mcp::build_tool_name_with_suffix(slug, tool, &format!("{slug}\u{0}{tool}"))
    }

    /// A snapshot of every daemon-tier SHARED server's state, in stable slug
    /// order.
    ///
    /// Daemon-tier `shared = false` servers are per-session (they live in
    /// `session_slots`, never in `servers`), so they are NOT reported here —
    /// they would appear as a "not connected" duplicate of the per-session row
    /// [`McpManager::session_status`] owns.
    #[must_use]
    pub fn status(&self) -> Vec<McpServerStatus> {
        self.order
            .iter()
            .filter(|slug| self.configs.get(*slug).is_some_and(|e| e.shared))
            .map(|slug| self.daemon_status(slug))
            .collect()
    }

    fn daemon_status(&self, slug: &str) -> McpServerStatus {
        let config = self.configs.get(slug).map(|e| &e.config);
        let transport = config.map_or_else(String::new, |c| c.transport.label().to_string());
        let target = config.map_or_else(String::new, |c| c.transport.target().to_string());
        match self.servers.get(slug) {
            Some(slot) => McpServerStatus {
                slug: slug.to_string(),
                tier: "daemon".to_string(),
                transport,
                target,
                connected: true,
                tool_count: slot.tool_count,
                server_name: Some(slot.handle.name().to_string()),
                server_version: Some(slot.handle.version().to_string()),
                last_error: None,
            },
            None => McpServerStatus {
                slug: slug.to_string(),
                tier: "daemon".to_string(),
                transport,
                target,
                connected: false,
                tool_count: 0,
                server_name: None,
                server_version: None,
                last_error: self.failures.get(slug).cloned(),
            },
        }
    }

    /// Statuses of a session's private (project + per-session) servers, in
    /// stable `(tier, slug)` order.
    #[must_use]
    pub fn session_status(&self, session_id: u64) -> Vec<McpServerStatus> {
        let mut out = Vec::new();
        // Project-shared connections this session references.
        for ((_, slug), shared) in &self.project_shared {
            if shared.sessions.contains(&session_id) {
                out.push(Self::slot_status(slug, "project", &shared.slot));
            }
        }
        // Per-session connections.
        for ((sid, root, slug), slot) in &self.session_slots {
            if *sid == session_id {
                let tier = if root.is_some() { "project" } else { "daemon" };
                out.push(Self::slot_status(slug, tier, slot));
            }
        }
        out.sort_by(|a, b| a.tier.cmp(&b.tier).then_with(|| a.slug.cmp(&b.slug)));
        out
    }

    fn slot_status(slug: &str, tier: &str, slot: &ServerSlot) -> McpServerStatus {
        McpServerStatus {
            slug: slug.to_string(),
            tier: tier.to_string(),
            transport: slot.config.transport.label().to_string(),
            target: slot.config.transport.target().to_string(),
            connected: true,
            tool_count: slot.tool_count,
            server_name: Some(slot.handle.name().to_string()),
            server_version: Some(slot.handle.version().to_string()),
            last_error: None,
        }
    }

    /// Shut down all MCP servers, joining each dispatcher with a bounded wait.
    pub fn shutdown_all(&mut self) {
        let total = self.servers.len() + self.project_shared.len() + self.session_slots.len();
        info!(count = total, "shutting down MCP servers");
        for (slug, slot) in self.servers.drain() {
            debug!(server = %slug, "shutting down MCP server");
            drop(slot);
            debug!(server = %slug, "MCP server shut down");
        }
        for ((root, slug), shared) in self.project_shared.drain() {
            debug!(server = %slug, root = %root.display(), "shutting down shared project MCP server");
            drop(shared.slot);
        }
        for ((session_id, root, slug), slot) in self.session_slots.drain() {
            debug!(server = %slug, session_id, root = ?root, "shutting down per-session MCP server");
            drop(slot);
        }
        info!("all MCP servers shut down");
    }

    /// Cancel every in-flight tool call started by `session_id`.
    pub fn cancel_session(&self, session_id: u64) {
        for slot in self.servers.values() {
            slot.handle.cancel_session(session_id);
        }
        for shared in self.project_shared.values() {
            shared.slot.handle.cancel_session(session_id);
        }
        for ((sid, _, _), slot) in &self.session_slots {
            if *sid == session_id {
                slot.handle.cancel_session(session_id);
            }
        }
    }

    /// Cancel `session_id`'s in-flight tool calls to the servers of ONE project
    /// root: its project-shared connections for that root plus its per-session
    /// project connections under it.
    ///
    /// The session's daemon-tier calls (shared or per-session) are left running:
    /// a working-directory change that leaves a project must not disturb an
    /// unrelated in-flight call.
    pub fn cancel_session_project(&self, session_id: u64, project_root: &Path) {
        for ((root, _slug), shared) in &self.project_shared {
            if root.as_path() == project_root && shared.sessions.contains(&session_id) {
                shared.slot.handle.cancel_session(session_id);
            }
        }
        for ((sid, root, _slug), slot) in &self.session_slots {
            if *sid == session_id && root.as_deref() == Some(project_root) {
                slot.handle.cancel_session(session_id);
            }
        }
    }

    /// The sessions that hold a project-tier or per-session server with `slug`
    /// (project or daemon tier) — i.e. the sessions whose PRIVATE overlay
    /// includes a server with that slug.
    ///
    /// A daemon-tier SHARED server with the same slug is NOT included: its tools
    /// live in the daemon-wide catalogue, which the command loop refreshes
    /// separately (`register_all`). Used to route a list change into exactly the
    /// sessions whose overlay a project server's change affects.
    #[must_use]
    pub fn sessions_for_slug(&self, slug: &str) -> HashSet<u64> {
        let mut out = HashSet::new();
        for ((_root, s), shared) in &self.project_shared {
            if s == slug {
                out.extend(shared.sessions.iter().copied());
            }
        }
        for (sid, _root, s) in self.session_slots.keys() {
            if s == slug {
                out.insert(*sid);
            }
        }
        out
    }

    /// Create an empty `McpManager` with no servers (for testing).
    #[must_use]
    pub fn empty() -> Self {
        let (list_change_tx, list_change_rx) = crossbeam_channel::unbounded::<McpListChange>();
        Self {
            servers: HashMap::new(),
            configs: HashMap::new(),
            order: Vec::new(),
            failures: HashMap::new(),
            project_shared: HashMap::new(),
            session_slots: HashMap::new(),
            list_change_tx,
            list_change_rx: Some(list_change_rx),
        }
    }

    /// Whether no servers are connected (for testing/inspection).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.servers.is_empty() && self.project_shared.is_empty() && self.session_slots.is_empty()
    }

    /// The number of connected daemon-tier shared servers.
    #[must_use]
    pub fn server_count(&self) -> usize {
        self.servers.len()
    }
}

#[cfg(feature = "mcp")]
impl Drop for McpManager {
    fn drop(&mut self) {
        self.shutdown_all();
    }
}

/// Connect to a server and list its tools, both on the caller's worker thread.
///
/// Bundling the initial listing into the connect worker lets `join_with_budget`
/// bound connect **and** discovery together: a server that handshakes quickly
/// but never answers `tools/list` is dropped at the caller's budget rather than
/// stalling it for the full per-server request timeout.
#[cfg(feature = "mcp")]
fn connect_and_list(
    config: &McpServerConfig,
    list_changes: Option<crossbeam_channel::Sender<McpListChange>>,
) -> Result<(McpServer, Vec<McpTool>), choreo_mcp::McpError> {
    let server = McpServer::connect_with_list_changes(config, list_changes)?;
    let tools = server.handle().list_tools()?;
    Ok((server, tools))
}

/// Join a thread, giving up after `timeout` and returning `None` (leaving it
/// detached) when it overruns.
#[cfg(feature = "mcp")]
fn join_with_budget<T: Send + 'static>(
    handle: std::thread::JoinHandle<T>,
    timeout: Duration,
) -> Option<T> {
    let (done_tx, done_rx) = crossbeam_channel::bounded::<T>(1);
    std::thread::spawn(move || {
        if let Ok(value) = handle.join() {
            let _ = done_tx.send(value);
        }
    });
    match done_rx.recv_timeout(timeout) {
        Ok(value) => Some(value),
        Err(crossbeam_channel::RecvTimeoutError::Timeout) => None,
        Err(crossbeam_channel::RecvTimeoutError::Disconnected) => None,
    }
}

// Feature-off stub: mirrors the metrics-module stub convention. The manager
// holds no servers (there is nothing to manage without the choreo-mcp
// dependency), so construction, teardown, and Drop are all no-ops. The API
// surface matches the real manager exactly — same method signatures — so
// callers cannot tell the difference, and no call site needs feature cfgs.
#[cfg(not(feature = "mcp"))]
mod imp {
    use crate::tools::ToolRegistry;

    use super::{McpReloadOutcome, McpServerStatus, SessionMcpOverlay};

    /// No-op stand-in for the real `McpManager` (see the module-level cfg note).
    pub struct McpManager;

    impl McpManager {
        /// Stub: no MCP config is loaded and no servers are spawned.
        pub fn from_config(_registry: &mut ToolRegistry) -> Self {
            Self
        }

        /// Stub: no server has any tools to (re-)register.
        pub fn register_all(&mut self, _registry: &mut ToolRegistry) {}

        /// Stub: there are no servers to shut down.
        pub fn shutdown_all(&mut self) {}

        /// Stub: no server has any in-flight call to cancel.
        pub fn cancel_session(&self, _session_id: u64) {}

        /// Stub: no server has any in-flight call to cancel.
        pub fn cancel_session_project(&self, _session_id: u64, _project_root: &std::path::Path) {}

        /// Stub: no session ever holds a private MCP server.
        #[must_use]
        pub fn sessions_for_slug(&self, _slug: &str) -> std::collections::HashSet<u64> {
            std::collections::HashSet::new()
        }

        /// Stub: there are no servers to reconnect.
        pub fn reconnect(&mut self, slug: &str) -> Result<(), String> {
            Err(format!("unknown MCP server {slug:?} (MCP is not built in)"))
        }

        /// Stub: there is no config to reload.
        pub fn reload(&mut self) -> Result<McpReloadOutcome, String> {
            Err("MCP is not built in".to_string())
        }

        /// Stub: no servers are ever ensured.
        pub fn ensure_session(
            &mut self,
            _session_id: u64,
            _project_root: Option<&std::path::Path>,
            _trusted: bool,
        ) -> SessionMcpOverlay {
            SessionMcpOverlay::empty()
        }

        /// Stub: nothing to reload.
        pub fn reload_session(
            &mut self,
            _session_id: u64,
            _project_root: Option<&std::path::Path>,
            _trusted: bool,
        ) -> SessionMcpOverlay {
            SessionMcpOverlay::empty()
        }

        /// Stub: nothing to release.
        pub fn release_session(&mut self, _session_id: u64) {}

        /// Stub: there is never any per-session project server.
        #[must_use]
        pub fn session_status(&self, _session_id: u64) -> Vec<McpServerStatus> {
            Vec::new()
        }

        /// Stub: there are never any servers, so no status rows.
        #[must_use]
        pub fn status(&self) -> Vec<McpServerStatus> {
            Vec::new()
        }

        /// Stub: creates an empty manager (same seam the real one exposes for
        /// tests).
        #[must_use]
        pub fn empty() -> Self {
            Self
        }

        /// Stub: there are never any servers.
        #[must_use]
        pub fn is_empty(&self) -> bool {
            true
        }

        /// Stub: there are never any servers.
        #[must_use]
        pub fn server_count(&self) -> usize {
            0
        }
    }
}

#[cfg(not(feature = "mcp"))]
pub use imp::McpManager;

#[cfg(test)]
#[cfg(feature = "mcp")]
mod tests {
    use super::*;

    #[test]
    fn empty_creates_manager_with_no_servers() {
        let manager = McpManager::empty();
        assert!(manager.is_empty());
        assert_eq!(manager.server_count(), 0);
        assert_eq!(manager.status().len(), 0);
    }

    #[test]
    fn shutdown_all_on_empty_is_noop() {
        let mut manager = McpManager::empty();
        manager.shutdown_all();
        assert!(manager.is_empty());
    }

    #[test]
    fn cancel_session_on_empty_is_noop() {
        let manager = McpManager::empty();
        manager.cancel_session(7);
        assert!(manager.is_empty());
    }

    #[test]
    fn drop_empty_manager_is_noop() {
        let manager = McpManager::empty();
        drop(manager);
    }

    #[test]
    fn from_config_with_no_file_creates_empty() {
        let dir = tempfile::tempdir().unwrap();
        config::set_test_config_root(Some(dir.path().to_path_buf()));
        let mut registry = crate::tools::ToolRegistry::new();
        let manager = McpManager::from_config(&mut registry);
        config::set_test_config_root(None);
        assert!(manager.is_empty());
        assert_eq!(manager.status().len(), 0);
    }

    #[test]
    fn resolve_name_disambiguates_a_collision() {
        let mut used = HashSet::new();
        let first = McpManager::resolve_name("s", "a.b", &used);
        used.insert(first.clone());
        let second = McpManager::resolve_name("s", "a_b", &used);
        assert_ne!(first, second);
        assert!(second.len() <= choreo_mcp::MAX_TOOL_NAME_LEN);
        assert_eq!(second, McpManager::resolve_name("s", "a_b", &used));
    }

    /// A minimal daemon-tier entry with the given slug and pooling attribute.
    fn stdio_entry(slug: &str, shared: bool) -> McpEntry {
        McpEntry {
            config: McpServerConfig {
                slug: slug.to_string(),
                transport: choreo_mcp::McpTransport::Stdio {
                    command: "true".to_string(),
                    args: Vec::new(),
                    env: HashMap::new(),
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
    fn status_reports_only_shared_daemon_servers() {
        let mut manager = McpManager::empty();
        manager.order = vec!["shared-srv".to_string(), "per-session-srv".to_string()];
        manager
            .configs
            .insert("shared-srv".to_string(), stdio_entry("shared-srv", true));
        manager.configs.insert(
            "per-session-srv".to_string(),
            stdio_entry("per-session-srv", false),
        );

        let statuses = manager.status();
        assert_eq!(
            statuses.iter().map(|s| s.slug.as_str()).collect::<Vec<_>>(),
            ["shared-srv"],
            "a daemon `shared = false` server is not a daemon-tier status row"
        );
        assert!(!statuses[0].connected);
        // The per-session row is owned by `session_status`, not `status`.
        assert_eq!(manager.session_status(1), [] as [McpServerStatus; 0]);
    }
}
