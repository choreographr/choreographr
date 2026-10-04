// Real implementation (connect/handshake/discover/shutdown over stdio) is
// compiled only with the `mcp` feature. Without it, the module below degrades
// to a no-op stub (see the `#[cfg(not(feature = "mcp"))]` block) so the
// manager's call sites in cli.rs / daemon.rs / server/lifecycle.rs compile
// unchanged in both configurations.
#[cfg(feature = "mcp")]
pub mod config;
#[cfg(feature = "mcp")]
pub mod tool;

#[cfg(feature = "mcp")]
use crate::tools::{ToolDyn, ToolRegistry};
#[cfg(feature = "mcp")]
use choreo_mcp::{McpListChange, McpServer, McpServerConfig, McpServerHandle};
#[cfg(feature = "mcp")]
use std::collections::{HashMap, HashSet};
#[cfg(feature = "mcp")]
use std::time::{Duration, Instant};
#[cfg(feature = "mcp")]
use tool::{McpListResourcesTool, McpReadResourceTool, McpToolWrapper};
#[cfg(feature = "mcp")]
use tracing::{debug, error, info, warn};

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

/// Manages all MCP server connections and their registered tools.
#[cfg(feature = "mcp")]
pub struct McpManager {
    /// One slot per *connected* server, keyed by server slug.
    servers: HashMap<String, ServerSlot>,
    /// The resolved config of *every* configured server (connected or not),
    /// keyed by slug, so a failed/skipped server can be retried and reported.
    configs: HashMap<String, McpServerConfig>,
    /// The configured slugs in a stable (sorted) order, so status and
    /// registration are deterministic.
    order: Vec<String>,
    /// Startup connect failures, keyed by slug, for the status surface (a
    /// server that is not connected because its connect failed or timed out).
    failures: HashMap<String, String>,
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
    /// Connect every enabled server, discover its tools, and register them in
    /// the `ToolRegistry`.
    ///
    /// Servers connect in parallel on background threads; the whole batch is
    /// bounded by [`STARTUP_BUDGET`] so a hung server cannot stall startup. A
    /// server that is not ready in time is logged and skipped (its thread
    /// detaches and its connection is dropped), but it stays in the manager's
    /// status list and config map so `/mcp` can report it and a reconnect can
    /// retry it.
    pub fn from_config(registry: &mut ToolRegistry) -> Self {
        let mut configs: Vec<McpServerConfig> = match config::load_mcp_config() {
            Ok(configs) => configs,
            Err(e) => {
                warn!("failed to load MCP config: {e}");
                Vec::new()
            }
        };
        // Deterministic registration order (a HashMap-derived Vec is not), so a
        // name collision between two servers always resolves the same way and a
        // reconnect reproduces the same names.
        configs.sort_by(|a, b| a.slug.cmp(&b.slug));

        // One shared list-changed channel: every server's subscription task
        // sends into this sender, and the daemon consumes the single receiver
        // (see `take_list_change_rx`). Created even when no server supports
        // subscriptions — the receiver is simply never taken, or the channel
        // stays empty.
        let (list_change_tx, list_change_rx) = crossbeam_channel::unbounded::<McpListChange>();

        let mut manager = Self {
            servers: HashMap::new(),
            configs: configs
                .iter()
                .map(|c| (c.slug.clone(), c.clone()))
                .collect(),
            order: configs.iter().map(|c| c.slug.clone()).collect(),
            failures: HashMap::new(),
            list_change_tx: list_change_tx.clone(),
            list_change_rx: Some(list_change_rx),
        };

        // Spawn all connect threads up front so they handshake in parallel.
        let mut pending: Vec<(String, std::thread::JoinHandle<anyhow::Result<McpServer>>)> =
            Vec::new();
        for cfg in configs {
            let slug = cfg.slug.clone();
            info!(
                server = %slug,
                transport = cfg.transport.label(),
                target = cfg.transport.target(),
                "spawning MCP server"
            );
            let list_changes = Some(list_change_tx.clone());
            let handle = std::thread::spawn(move || {
                McpServer::connect_with_list_changes(&cfg, list_changes)
                    .map_err(anyhow::Error::from)
            });
            pending.push((slug, handle));
        }

        let deadline = Instant::now() + STARTUP_BUDGET;
        // One collision set for the whole startup batch, so two servers whose
        // tool names sanitize identically are disambiguated deterministically.
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

    /// Re-register every connected server's tools into `registry`.
    ///
    /// Used when the daemon rebuilds its [`ToolRegistry`] after a list-changed
    /// event: unlike startup (which drops a server whose listing fails), a
    /// refresh names the failure and keeps the server connected, because a
    /// transient listing error must not take a working server offline.
    pub fn register_all(&self, registry: &mut ToolRegistry) {
        // A fresh collision set per refresh; the sorted order keeps a name
        // stable across refreshes (and identical to startup).
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

    /// Discover a server's tools and register them (and the server) in the
    /// manager.
    ///
    /// # Errors
    ///
    /// Returns the listing error (as text) when `tools/list` fails, in which
    /// case the server is not inserted.
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
            .cloned()
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
    ///
    /// Shared by startup ([`register_server`](Self::register_server)) and a
    /// post-list-change refresh ([`register_all`](Self::register_all)) so the
    /// naming and resource-tool rules never drift. A server that declares the
    /// `resources` capability additionally gets the `list_resources` /
    /// `read_resource` catalogue tools; a server without it would only fail the
    /// call, so they are not offered. Tools named in `disabled` (the config's
    /// `disabledTools`) are not offered. `used` carries the provider-safe names
    /// already claimed by earlier servers so a collision (two names that
    /// sanitize the same) is disambiguated with a hash suffix rather than
    /// silently overwriting.
    ///
    /// # Errors
    ///
    /// Returns the listing error when `tools/list` fails.
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

        // Reserve the resource tools' names first so a server tool that happens
        // to share a name cannot displace them (they are always registered after
        // the server's own tools).
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

        // A server that declares the `resources` capability gets the
        // catalogue tools; a server without it would only fail the call, so
        // they are not offered.
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

    /// Compute the provider-safe name for `tool` on `slug`, appending a hash
    /// suffix when another server already claimed the sanitized name.
    fn resolve_name(slug: &str, tool: &str, used: &HashSet<String>) -> String {
        let base = choreo_mcp::build_tool_name(slug, tool);
        if !used.contains(&base) {
            return base;
        }
        // The seed ties the suffix to the originating identity, so two colliding
        // tools get different suffixes deterministically.
        choreo_mcp::build_tool_name_with_suffix(slug, tool, &format!("{slug}\u{0}{tool}"))
    }

    /// A snapshot of every configured server's state, in stable slug order.
    ///
    /// Reports connected servers (with their tool counts) and servers that
    /// failed or timed out at startup (with the recorded error), so the `/mcp`
    /// status surface and `session_inspect` can show the whole configured set.
    #[must_use]
    pub fn status(&self) -> Vec<McpServerStatus> {
        self.order
            .iter()
            .map(|slug| {
                let config = self.configs.get(slug);
                let transport =
                    config.map_or_else(String::new, |c| c.transport.label().to_string());
                let target = config.map_or_else(String::new, |c| c.transport.target().to_string());
                match self.servers.get(slug) {
                    Some(slot) => McpServerStatus {
                        slug: slug.clone(),
                        transport,
                        target,
                        connected: true,
                        tool_count: slot.tool_count,
                        server_name: Some(slot.handle.name().to_string()),
                        server_version: Some(slot.handle.version().to_string()),
                        last_error: None,
                    },
                    None => McpServerStatus {
                        slug: slug.clone(),
                        transport,
                        target,
                        connected: false,
                        tool_count: 0,
                        server_name: None,
                        server_version: None,
                        last_error: self.failures.get(slug).cloned(),
                    },
                }
            })
            .collect()
    }

    /// Rebuild the connection to `slug`, re-registering its tools.
    ///
    /// Used by the `/mcp` surface's reconnect action: a server that failed at
    /// startup (or was stopped) is re-attempted with the current config, and a
    /// live connection is torn down and replaced. Bounded by [`RECONNECT_BUDGET`].
    /// The caller rebuilds the tool catalogue afterwards (this does not touch a
    /// `ToolRegistry`), so a reconnect cannot leave a half-updated catalogue.
    ///
    /// # Errors
    ///
    /// Returns a message when `slug` is unknown, the connect fails or times out,
    /// or the tool listing fails.
    pub fn reconnect(&mut self, slug: &str) -> Result<(), String> {
        let config = self
            .configs
            .get(slug)
            .cloned()
            .ok_or_else(|| format!("unknown MCP server {slug:?}"))?;
        // Drop any existing slot first: its `Drop` shuts the old dispatcher
        // down, so the fresh connect does not leave a stale connection behind.
        self.servers.remove(slug);

        match Self::connect_slot(&self.list_change_tx, &config, RECONNECT_BUDGET) {
            Ok(slot) => {
                self.servers.insert(slug.to_string(), slot);
                self.failures.remove(slug);
                info!(server = %slug, "reconnected MCP server");
                Ok(())
            }
            Err(msg) => {
                self.failures.insert(slug.to_string(), msg.clone());
                Err(msg)
            }
        }
    }

    /// Re-read the MCP configuration from disk and reconcile the running
    /// server set with it, without restarting the daemon.
    ///
    /// The user and project `mcp_servers.json` files are re-read and resolved
    /// exactly as at startup. Each configured slug is then handled by whether
    /// it already exists and whether its resolved config changed:
    ///
    /// - a slug whose resolved config is identical to a currently connected
    ///   server is left untouched (its connection and dispatcher stay live);
    /// - a newly-added slug, a slug whose resolved config changed, or a slug
    ///   that is configured but not connected (a prior connect failure) is
    ///   (re)connected, replacing any stale connection;
    /// - a slug that vanished from the config is disconnected (its slot's
    ///   `Drop` shuts the dispatcher down with a bounded join).
    ///
    /// `self.configs`/`self.order` are replaced with the resolved set, so a
    /// later [`status`](Self::status)/[`reconnect`](Self::reconnect) sees the
    /// new configuration. The caller rebuilds the tool catalogue afterwards
    /// (this does not touch a `ToolRegistry`).
    ///
    /// # Errors
    ///
    /// Returns an error only when the config cannot be read or parsed. A
    /// server that fails to (re)connect is recorded in `failures` and appears
    /// in the outcome's status list, not as a hard error — the reload itself
    /// completed.
    pub fn reload(&mut self) -> Result<McpReloadOutcome, String> {
        let mut configs: Vec<McpServerConfig> =
            config::load_mcp_config().map_err(|e| format!("failed to load MCP config: {e}"))?;
        // Deterministic order matches startup: a name collision resolves the
        // same way across a reload as it did at boot.
        configs.sort_by(|a, b| a.slug.cmp(&b.slug));
        let new_order: Vec<String> = configs.iter().map(|c| c.slug.clone()).collect();
        let new_configs: HashMap<String, McpServerConfig> = configs
            .iter()
            .map(|c| (c.slug.clone(), c.clone()))
            .collect();

        // Slugs that left the config: close their connection and forget any
        // recorded failure. Done first so a slug reused across the reload (a
        // removal plus an add in one edit) never aliases the old connection.
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

        for cfg in configs {
            let slug = cfg.slug.clone();
            let was_configured = self.configs.contains_key(&slug);
            // An existing slot whose resolved config is unchanged is kept as
            // is; every other case needs a fresh connection.
            if self
                .servers
                .get(&slug)
                .is_some_and(|slot| slot.config == cfg)
            {
                unchanged.push(slug);
                continue;
            }
            if was_configured {
                restarted.push(slug.clone());
            } else {
                added.push(slug.clone());
            }
            // Drop any stale connection before the fresh connect (bounded).
            self.servers.remove(&slug);
            match Self::connect_slot(&self.list_change_tx, &cfg, RELOAD_BUDGET) {
                Ok(slot) => {
                    self.failures.remove(&slug);
                    self.servers.insert(slug, slot);
                }
                Err(e) => {
                    warn!(server = %slug, error = %e, "MCP server failed to connect during reload");
                    self.failures.insert(slug.clone(), e);
                    failed.push(slug);
                }
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
    /// bounding the whole connect (handshake, discovery, initial listing) by
    /// `timeout`.
    ///
    /// Shared by a manual reconnect and a config reload so both take the same
    /// path and neither can leave a half-connected server behind.
    ///
    /// # Errors
    ///
    /// Returns the connect error (as text) when the connect fails, times out,
    /// or the initial tool listing fails.
    fn connect_slot(
        list_change_tx: &crossbeam_channel::Sender<McpListChange>,
        config: &McpServerConfig,
        timeout: Duration,
    ) -> Result<ServerSlot, String> {
        let list_changes = Some(list_change_tx.clone());
        let cfg = config.clone();
        let handle = std::thread::spawn(move || {
            McpServer::connect_with_list_changes(&cfg, list_changes).map_err(anyhow::Error::from)
        });
        match join_with_budget(handle, timeout) {
            Some(Ok(server)) => Self::slot_from_server(server, config.clone()),
            Some(Err(e)) => Err(e.to_string()),
            None => Err(format!("connect timed out after {}s", timeout.as_secs())),
        }
    }

    /// Build a `ServerSlot` from a freshly connected `server`, listing its
    /// tools once to count those that will be registered (honouring
    /// `disabledTools`).
    ///
    /// # Errors
    ///
    /// Returns a message when the tool listing fails.
    fn slot_from_server(server: McpServer, config: McpServerConfig) -> Result<ServerSlot, String> {
        let handle = server.handle();
        let tools = handle
            .list_tools()
            .map_err(|e| format!("failed to list tools: {e}"))?;
        let disabled: HashSet<&str> = config.disabled_tools.iter().map(String::as_str).collect();
        let tool_count = tools
            .iter()
            .filter(|t| !disabled.contains(t.name.as_str()))
            .count();
        Ok(ServerSlot {
            handle,
            server,
            config,
            tool_count,
        })
    }

    /// Shut down all MCP servers, joining each dispatcher with a bounded wait.
    pub fn shutdown_all(&mut self) {
        info!(count = self.servers.len(), "shutting down MCP servers");
        for (slug, slot) in self.servers.drain() {
            // Log per-server BEGIN/END: if a Ctrl+C wedge stops between the two,
            // THIS server is the culprit.
            debug!(server = %slug, "shutting down MCP server");
            // Dropping the slot drops the `McpServer`, whose `Drop` sends the
            // shutdown command and joins the dispatcher with a bounded wait.
            drop(slot);
            debug!(server = %slug, "MCP server shut down");
        }
        info!("all MCP servers shut down");
    }

    /// Cancel every in-flight tool call started by `session_id`.
    ///
    /// Best-effort and non-blocking: the dispatcher for each server cancels the
    /// matching calls (and tells the server to stop cooperatively). Called from
    /// the daemon's session-cancel path.
    pub fn cancel_session(&self, session_id: u64) {
        for slot in self.servers.values() {
            slot.handle.cancel_session(session_id);
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
            list_change_tx,
            list_change_rx: Some(list_change_rx),
        }
    }

    /// Whether no servers are connected (for testing/inspection).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.servers.is_empty()
    }

    /// The number of connected servers.
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
///
/// The handle is moved into a waiter thread that joins and forwards the result
/// over a crossbeam channel; this thread waits on that channel with
/// `recv_timeout`, so a hung connect cannot stall the caller.
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
        // The waiter died (e.g. the connect thread panicked) — nothing to
        // register.
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

    use super::{McpReloadOutcome, McpServerStatus};

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
        // Point the loaders at a nonexistent dir so no real user/project config
        // is picked up regardless of the test machine.
        let dir = tempfile::tempdir().unwrap();
        config::set_test_config_root(Some(dir.path().to_path_buf()));
        config::set_test_project_root(Some(None));
        let mut registry = crate::tools::ToolRegistry::new();
        let manager = McpManager::from_config(&mut registry);
        config::set_test_config_root(None);
        config::set_test_project_root(None);
        assert!(manager.is_empty());
        assert_eq!(manager.status().len(), 0);
    }

    #[test]
    fn reload_with_no_config_reports_all_zero() {
        // No servers are configured, so a reload reconciles nothing and still
        // succeeds with an all-zero summary.
        let dir = tempfile::tempdir().unwrap();
        config::set_test_config_root(Some(dir.path().to_path_buf()));
        config::set_test_project_root(Some(None));
        let mut manager = McpManager::empty();
        let outcome = manager.reload().expect("empty reload succeeds");
        config::set_test_config_root(None);
        config::set_test_project_root(None);
        assert_eq!(
            outcome.summary,
            "MCP reload: 0 added, 0 removed, 0 restarted, 0 unchanged, 0 failed"
        );
        assert!(
            outcome.servers.is_empty(),
            "no servers -> empty status list"
        );
    }

    #[test]
    fn reload_with_malformed_config_is_a_hard_error() {
        // A config file that exists but does not parse fails the reload
        // outright (unlike startup, where a bad file degrades to no servers).
        let dir = tempfile::tempdir().unwrap();
        let cfg_dir = dir.path().join("choreographr");
        std::fs::create_dir_all(&cfg_dir).unwrap();
        std::fs::write(cfg_dir.join("mcp_servers.json"), "{ not json").unwrap();
        config::set_test_config_root(Some(dir.path().to_path_buf()));
        config::set_test_project_root(Some(None));
        let mut manager = McpManager::empty();
        let err = manager.reload().expect_err("malformed config fails");
        config::set_test_config_root(None);
        config::set_test_project_root(None);
        assert!(err.contains("failed to load MCP config"), "got: {err}");
    }

    #[test]
    fn resolve_name_disambiguates_a_collision() {
        let mut used = HashSet::new();
        let first = McpManager::resolve_name("s", "a.b", &used);
        used.insert(first.clone());
        // A second tool that sanitizes to the same base gets a hashed suffix.
        let second = McpManager::resolve_name("s", "a_b", &used);
        assert_ne!(first, second);
        assert!(second.len() <= choreo_mcp::MAX_TOOL_NAME_LEN);
        // Deterministic: the same inputs reproduce the same name.
        assert_eq!(second, McpManager::resolve_name("s", "a_b", &used));
    }
}
