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

#[cfg(feature = "mcp")]
use crate::tools::ToolRegistry;
use crate::tools::{ToolDyn, ToolError, ToolOutput, ToolOutputFormat};
use choreo_ai_protocols::openai::ChatToolDefinition;
#[cfg(feature = "mcp")]
use choreo_mcp::{McpListChange, McpServer, McpServerConfig, McpServerHandle};
#[cfg(feature = "mcp")]
use config::McpEntry;
#[cfg(feature = "mcp")]
use std::collections::HashMap;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc;
#[cfg(feature = "mcp")]
use std::time::{Duration, Instant};
#[cfg(feature = "mcp")]
use tool::{McpListResourcesTool, McpReadResourceTool, McpToolWrapper};
#[cfg(feature = "mcp")]
use tracing::{debug, error, info, warn};

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
            groups: HashSet::new(),
        }
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
        self.tools.iter().any(|t| t.name() == name)
    }

    /// A tool definition for every tool in the set (Text/JSON-compatible).
    #[must_use]
    pub fn definitions(&self) -> Vec<ChatToolDefinition> {
        self.tools
            .iter()
            .map(|t| ChatToolDefinition::function(t.name(), t.description(), t.schema()))
            .collect()
    }

    /// Like [`Self::definitions`] but carrying `output_schema`/`allowed_callers`
    /// for the Responses API.
    #[must_use]
    pub fn definitions_for_responses(&self) -> Vec<ChatToolDefinition> {
        self.tools
            .iter()
            .map(|t| {
                let callers = t.allowed_callers();
                ChatToolDefinition::function_with_options(
                    t.name(),
                    t.description(),
                    t.schema(),
                    t.output_schema(),
                    if callers.is_empty() {
                        None
                    } else {
                        Some(callers)
                    },
                )
            })
            .collect()
    }

    /// Describe a call against a tool in this set, or `None` if unknown.
    #[must_use]
    pub fn describe_invocation_json(&self, name: &str, args_json: &str) -> Option<String> {
        self.tools
            .iter()
            .find(|t| t.name() == name)
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
        image_tx: Option<mpsc::Sender<crate::tools::PreparedImage>>,
    ) -> Option<Result<ToolOutput, ToolError>> {
        self.tools
            .iter()
            .find(|t| t.name() == tool_call.name)
            .map(|t| {
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
        image_tx: Option<mpsc::Sender<crate::tools::PreparedImage>>,
    ) -> Option<Result<ToolOutput, ToolError>> {
        self.tools
            .iter()
            .find(|t| t.name() == tool_call.name)
            .map(|t| {
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
        self.tools
            .iter()
            .find(|t| t.name() == name)
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
        if dir.join(PROJECT_CONFIG_FILE).is_file() {
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
        let mut pending: Vec<(String, std::thread::JoinHandle<anyhow::Result<McpServer>>)> =
            Vec::new();
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
                McpServer::connect_with_list_changes(&cfg.config, list_changes)
                    .map_err(anyhow::Error::from)
            });
            pending.push((slug, handle));
        }

        let deadline = Instant::now() + STARTUP_BUDGET;
        let mut used: HashSet<String> = HashSet::new();
        for (slug, handle) in pending {
            let remaining = deadline.saturating_duration_since(Instant::now());
            match join_with_budget(handle, remaining) {
                Some(Ok(server)) => {
                    if let Err(e) =
                        Self::register_server(&slug, server, &mut used, registry, &mut manager)
                    {
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
    pub fn register_all(&self, registry: &mut ToolRegistry) {
        let mut used: HashSet<String> = HashSet::new();
        for slug in &self.order {
            let Some(slot) = self.servers.get(slug) else {
                continue;
            };
            if let Err(e) = Self::register_server_tools(
                slug,
                &slot.handle,
                &slot.config.disabled_tools,
                &mut used,
                registry,
            ) {
                warn!(server = %slug, error = %e, "failed to list MCP tools during registry refresh");
            }
        }
    }

    fn register_server(
        slug: &str,
        server: McpServer,
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
        let tool_count =
            Self::register_server_tools(slug, &handle, &config.disabled_tools, used, registry)
                .map_err(|e| e.to_string())?;
        manager.servers.insert(
            slug.to_string(),
            ServerSlot {
                handle,
                server,
                config,
                tool_count,
            },
        );
        manager.failures.remove(slug);
        Ok(())
    }

    /// Register one server's advertised tools (and its catalogue group) into
    /// `registry`, returning the number of tools registered.
    fn register_server_tools(
        slug: &str,
        handle: &McpServerHandle,
        disabled: &[String],
        used: &mut HashSet<String>,
        registry: &mut ToolRegistry,
    ) -> Result<usize, choreo_mcp::McpError> {
        let server_name = handle.name().to_string();
        let tools = handle.list_tools()?;
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
        Ok(count)
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
        let tools = handle.list_tools()?;
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

    /// Compute the provider-safe name for `tool` on `slug`, appending a hash
    /// suffix when another server already claimed the sanitized name.
    fn resolve_name(slug: &str, tool: &str, used: &HashSet<String>) -> String {
        let base = choreo_mcp::build_tool_name(slug, tool);
        if !used.contains(&base) {
            return base;
        }
        choreo_mcp::build_tool_name_with_suffix(slug, tool, &format!("{slug}\u{0}{tool}"))
    }

    /// A snapshot of every daemon-tier server's state, in stable slug order.
    #[must_use]
    pub fn status(&self) -> Vec<McpServerStatus> {
        self.order
            .iter()
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

    /// Ensure a session's overlay: connect/create the project and per-session
    /// servers it needs, ref-counting shared project connections, and return
    /// the private tool set plus the groups to shadow.
    ///
    /// `project_root`/`trusted` come from the daemon (the session's working
    /// directory walked up to a `.mcp.json` and checked against the trust
    /// store). An UNTRUSTED project root yields no project connections: the
    /// declaration is read only to report the ignored slugs.
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

        // Resolve the project entries (first, so their slugs can suppress a
        // same-slug daemon per-session server — the project override is
        // whole-entry).
        let mut project_slugs: HashSet<String> = HashSet::new();
        let mut project_entries: Vec<McpEntry> = Vec::new();
        if let Some(root) = project_root {
            match config::load_project_config(root, trusted) {
                Ok(Some(entries)) => {
                    project_slugs.extend(entries.iter().map(|e| e.config.slug.clone()));
                    if trusted {
                        project_entries = entries;
                    } else {
                        ignored = entries.iter().map(|e| e.config.slug.clone()).collect();
                        ignored.sort();
                    }
                }
                Ok(None) => {}
                Err(e) => {
                    warn!(root = %root.display(), error = %e, "failed to read project .mcp.json")
                }
            }
        }

        // Daemon-tier per-session (shared=false) servers, unless the project
        // overrides that slug.
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
            &mut used,
            &mut tools,
            &mut groups,
            &mut statuses,
        );
        if let Some(root) = project_root
            && trusted
        {
            self.ensure_entries(
                session_id,
                Some(root),
                &project_entries,
                &mut used,
                &mut tools,
                &mut groups,
                &mut statuses,
            );
            // Every project slug shadows the daemon-tier group of the same name.
            for slug in &project_slugs {
                shadowed.insert(choreo_mcp::group_name(slug));
            }
        }

        SessionMcpOverlay {
            tools: Arc::new(ProjectToolSet { tools, groups }),
            shadowed_groups: shadowed,
            project_root: project_root.map(Path::to_path_buf),
            project_trusted: trusted,
            ignored_project_servers: ignored,
            statuses,
        }
    }

    /// Connect/ensure the slots for `entries`, appending their wrappers to
    /// `tools`. `root` is `None` for daemon-tier per-session servers.
    #[expect(clippy::too_many_arguments)]
    fn ensure_entries(
        &mut self,
        session_id: u64,
        root: Option<&Path>,
        entries: &[McpEntry],
        used: &mut HashSet<String>,
        tools: &mut Vec<Box<dyn ToolDyn>>,
        groups: &mut HashSet<String>,
        statuses: &mut Vec<McpServerStatus>,
    ) {
        for entry in entries {
            let slug = entry.config.slug.clone();
            let handle = if entry.shared {
                let Some(root) = root else {
                    continue;
                };
                self.ensure_shared(session_id, root, &entry.config)
            } else {
                self.ensure_session_slot(session_id, root, &entry.config)
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
                Ok(count) => statuses.push(McpServerStatus {
                    slug,
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
                }),
                Err(e) => warn!(server = %slug, error = %e, "failed to list project MCP tools"),
            }
        }
    }

    /// Ensure a project-shared connection for `(root, slug)`, adding the
    /// session to its reference set. Returns the handle + config.
    fn ensure_shared(
        &mut self,
        session_id: u64,
        root: &Path,
        config: &McpServerConfig,
    ) -> Option<(McpServerHandle, McpServerConfig)> {
        let key = (root.to_path_buf(), config.slug.clone());
        if let Some(shared) = self.project_shared.get_mut(&key) {
            shared.sessions.insert(session_id);
            return Some((shared.slot.handle.clone(), shared.slot.config.clone()));
        }
        let slot = Self::connect_slot(&self.list_change_tx, config, RELOAD_BUDGET)?;
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

    /// Ensure a per-session connection for `(session_id, root, slug)`.
    fn ensure_session_slot(
        &mut self,
        session_id: u64,
        root: Option<&Path>,
        config: &McpServerConfig,
    ) -> Option<(McpServerHandle, McpServerConfig)> {
        let key = (session_id, root.map(Path::to_path_buf), config.slug.clone());
        if let Some(slot) = self.session_slots.get(&key) {
            return Some((slot.handle.clone(), slot.config.clone()));
        }
        let slot = Self::connect_slot(&self.list_change_tx, config, RELOAD_BUDGET)?;
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

    /// Rebuild the connection to `slug`, re-registering its tools.
    ///
    /// # Errors
    ///
    /// Returns a message when `slug` is unknown or the (re)connect fails.
    pub fn reconnect(&mut self, slug: &str) -> Result<(), String> {
        // Daemon-tier shared server.
        if let Some(entry) = self.configs.get(slug)
            && entry.shared
        {
            let config = entry.config.clone();
            self.servers.remove(slug);
            if let Some(slot) = Self::connect_slot(&self.list_change_tx, &config, RECONNECT_BUDGET)
            {
                self.servers.insert(slug.to_string(), slot);
                self.failures.remove(slug);
                info!(server = %slug, "reconnected MCP server");
                return Ok(());
            }
            let msg = format!("reconnect to {slug:?} failed");
            self.failures.insert(slug.to_string(), msg.clone());
            return Err(msg);
        }
        // A project-shared connection (best-effort, by slug).
        let matching: Vec<(PathBuf, String)> = self
            .project_shared
            .keys()
            .filter(|(_, s)| s == slug)
            .cloned()
            .collect();
        if matching.is_empty() {
            return Err(format!("unknown MCP server {slug:?}"));
        }
        for key in matching {
            if let Some(shared) = self.project_shared.get(&key) {
                let config = shared.slot.config.clone();
                let sessions = shared.sessions.clone();
                if let Some(slot) =
                    Self::connect_slot(&self.list_change_tx, &config, RECONNECT_BUDGET)
                {
                    self.project_shared
                        .insert(key, SharedSlot { slot, sessions });
                } else {
                    return Err(format!("reconnect to {slug:?} failed"));
                }
            }
        }
        Ok(())
    }

    /// Re-read the daemon-tier `mcp.json` and reconcile the daemon-tier shared
    /// server set with it. Project-tier servers are reconciled per session
    /// (see [`McpManager::reload_session`]).
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

        let removed: Vec<String> = self
            .order
            .iter()
            .filter(|slug| !new_configs.contains_key(*slug))
            .cloned()
            .collect();
        for slug in &removed {
            self.servers.remove(slug);
            self.failures.remove(slug);
            info!(server = %slug, "MCP server removed by reload");
        }

        let mut added: Vec<String> = Vec::new();
        let mut restarted: Vec<String> = Vec::new();
        let mut unchanged: Vec<String> = Vec::new();
        let mut failed: Vec<String> = Vec::new();

        for entry in &entries {
            // `shared = false` daemon servers are per-session: they are not in
            // `self.servers`, so there is nothing to (re)connect here.
            if !entry.shared {
                unchanged.push(entry.config.slug.clone());
                continue;
            }
            let slug = entry.config.slug.clone();
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
        Ok(McpReloadOutcome {
            summary,
            servers: self.status(),
        })
    }

    /// Connect one server from its resolved config and return a ready slot,
    /// bounding the whole connect by `timeout`.
    fn connect_slot(
        list_change_tx: &crossbeam_channel::Sender<McpListChange>,
        config: &McpServerConfig,
        timeout: Duration,
    ) -> Option<ServerSlot> {
        let list_changes = Some(list_change_tx.clone());
        let cfg = config.clone();
        let handle = std::thread::spawn(move || {
            McpServer::connect_with_list_changes(&cfg, list_changes).map_err(anyhow::Error::from)
        });
        match join_with_budget(handle, timeout) {
            Some(Ok(server)) => Self::slot_from_server(server, config.clone()),
            Some(Err(e)) => {
                warn!(server = %config.slug, error = %e, "MCP server failed to connect");
                None
            }
            None => {
                warn!(server = %config.slug, "MCP server connect timed out");
                None
            }
        }
    }

    /// Build a `ServerSlot` from a freshly connected `server`.
    fn slot_from_server(server: McpServer, config: McpServerConfig) -> Option<ServerSlot> {
        let handle = server.handle();
        let tools = handle.list_tools().ok()?;
        let disabled: HashSet<&str> = config.disabled_tools.iter().map(String::as_str).collect();
        let tool_count = tools
            .iter()
            .filter(|t| !disabled.contains(t.name.as_str()))
            .count();
        Some(ServerSlot {
            handle,
            server,
            config,
            tool_count,
        })
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
        pub fn register_all(&self, _registry: &mut ToolRegistry) {}

        /// Stub: there are no servers to shut down.
        pub fn shutdown_all(&mut self) {}

        /// Stub: no server has any in-flight call to cancel.
        pub fn cancel_session(&self, _session_id: u64) {}

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
