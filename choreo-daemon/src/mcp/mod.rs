// Real implementation (connect/handshake/discover/shutdown over stdio) is
// compiled only with the `mcp` feature. Without it, the module degrades to a
// no-op stub (see `stub.rs`) so the manager's call sites in cli.rs / daemon.rs /
// server/lifecycle.rs compile unchanged in both configurations.
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
// slot connect helper, `slot` the shared slot value types and the per-operation
// budgets, `query` the `/mcp` status snapshots and the per-session call
// cancellation, `paths` the config-dir resolution and file names, and `status`
// the daemon-facing status/report/outcome types. `stub` is the feature-off
// stand-in. Their `impl McpManager` blocks reach the manager's private fields
// as child modules; the items other modules use are re-exported here so every
// `crate::mcp::…` path is unchanged.
#[cfg(feature = "mcp")]
mod overlay;
mod paths;
mod pool;
mod project;
mod query;
#[cfg(feature = "mcp")]
mod slot;
mod status;
#[cfg(not(feature = "mcp"))]
mod stub;

pub use paths::{PROJECT_CONFIG_FILE, TRUST_FILE, config_dir, set_test_config_root, trust_path};
pub use project::{ProjectToolSet, SessionMcpOverlay, project_root_for};
pub use status::{McpReloadOutcome, McpServerStatus, McpStatusReport, McpTrustOutcome};
#[cfg(not(feature = "mcp"))]
pub use stub::McpManager;

#[cfg(feature = "mcp")]
use crate::tools::ToolRegistry;
#[cfg(feature = "mcp")]
use choreo_mcp::{McpListChange, McpServer, McpServerHandle, McpTool};
#[cfg(feature = "mcp")]
use config::McpEntry;
#[cfg(feature = "mcp")]
use slot::{
    CATALOGUE_REFRESH_BUDGET, CATALOGUE_REFRESH_TOTAL_BUDGET, PendingConnect, RECONNECT_BUDGET,
    RECONNECT_TOTAL_BUDGET, RELOAD_BUDGET, RELOAD_TOTAL_BUDGET, STARTUP_BUDGET, ServerSlot,
    SharedSlot, connect_and_list, join_with_budget,
};
#[cfg(feature = "mcp")]
use std::collections::HashMap;
#[cfg(feature = "mcp")]
use std::collections::HashSet;
#[cfg(feature = "mcp")]
use std::path::PathBuf;
#[cfg(feature = "mcp")]
use std::time::{Duration, Instant};
#[cfg(feature = "mcp")]
use tool::build_server_wrappers;
#[cfg(feature = "mcp")]
use tracing::{debug, error, info, warn};

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
    /// command loop, and one slow server must not freeze every session. The
    /// WHOLE sweep is additionally bounded by [`CATALOGUE_REFRESH_TOTAL_BUDGET`],
    /// so a reload/reconnect over many servers cannot compound into an unbounded
    /// command-loop stall. A server that misses either deadline keeps its
    /// previously-listed tool set (so its group does not blink out of the
    /// catalogue) and is re-listed on the next refresh.
    pub fn register_all(&mut self, registry: &mut ToolRegistry) {
        let mut used: HashSet<String> = HashSet::new();
        // One deadline for the WHOLE sweep: each server is re-listed under the
        // smaller remaining share, so the total cannot exceed the aggregate
        // budget no matter how many servers are connected.
        let deadline = Instant::now() + CATALOGUE_REFRESH_TOTAL_BUDGET;
        for slug in self.order.clone() {
            let Some(slot) = self.servers.get_mut(&slug) else {
                continue;
            };
            // Bound this server's re-list by the remaining share of the
            // aggregate budget; once it is spent, skip the re-list and keep the
            // server's CACHED tools so its group stays in the catalogue.
            // Register straight from the cache (no clone).
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                warn!(
                    server = %slug,
                    "catalogue-refresh budget exhausted; keeping the cached tool set"
                );
            } else {
                Self::relist_slot(slot, remaining.min(CATALOGUE_REFRESH_BUDGET));
            }
            Self::register_server_tools(
                &slug,
                &slot.handle,
                &slot.tools,
                &slot.config.disabled_tools,
                &mut used,
                registry,
            );
        }
    }

    /// Re-list ONE daemon-tier shared server and update its cached tool set.
    ///
    /// A list-changed event names exactly one server, so re-listing only that
    /// server keeps the catalogue rebuild off every other server's request
    /// path: a full [`McpManager::register_all`] sweep would re-list all N
    /// connected servers (each bounded by [`CATALOGUE_REFRESH_BUDGET`]), so a
    /// single event could stall the command loop for up to N × the budget.
    ///
    /// The re-listing is bounded by the same short catalogue-refresh deadline
    /// as `register_all` (not the per-server request timeout), and a server
    /// that misses it — or fails to list — keeps its previous tool set with a
    /// warning, so its group never blinks out of the catalogue; it is re-listed
    /// on the next event or a full `register_all`.
    ///
    /// A no-op when `slug` is not a connected daemon shared server: a
    /// per-session or project server's change is handled through its session
    /// overlay, not the daemon-wide catalogue.
    pub fn refresh_server(&mut self, slug: &str) {
        let Some(slot) = self.servers.get_mut(slug) else {
            debug!(
                server = %slug,
                "list change for a server outside the daemon catalogue; ignoring"
            );
            return;
        };
        Self::relist_slot(slot, CATALOGUE_REFRESH_BUDGET);
    }

    /// Re-list one connected `slot`, bounded by `deadline`, refreshing its
    /// cached tool set and enabled tool count.
    ///
    /// A listing that fails or misses `deadline` keeps the slot's previous tool
    /// set with a warning, so the server's `mcp/<slug>` group never blinks out
    /// of the catalogue; it is re-listed on the next refresh. Shared by the full
    /// [`McpManager::register_all`] sweep and the single-server
    /// [`McpManager::refresh_server`] path so the two cannot drift.
    fn relist_slot(slot: &mut ServerSlot, deadline: Duration) {
        match slot.handle.list_tools_with_deadline(deadline) {
            Ok(tools) => {
                // Keep `tool_count` in step with the fresh listing so the `/mcp`
                // status surface reports the real tool count (the count excludes
                // disabled tools, matching `slot_from_server`).
                slot.tool_count = Self::enabled_tool_count(&tools, &slot.config.disabled_tools);
                slot.tools = tools;
            }
            Err(e) => {
                warn!(
                    server = %slot.config.slug,
                    error = %e,
                    "MCP tool listing missed the catalogue-refresh budget; keeping the previous tool set"
                );
            }
        }
    }

    /// Re-register every daemon-tier shared server's CACHED tool set into
    /// `registry`, WITHOUT re-listing any of them.
    ///
    /// The catalogued counterpart of the re-list in
    /// [`McpManager::refresh_server`]: after the one changed server has been
    /// re-listed, this rebuilds the daemon-wide catalogue from every server's
    /// cached `tools`. It walks `self.order` with a fresh `used` set exactly
    /// like [`McpManager::register_all`], so the registered tool names and
    /// groups are identical to a full sweep — tool naming depends only on
    /// `self.order` and the tool lists, never on a live listing — while the
    /// network round-trips are skipped.
    pub fn register_cached(&self, registry: &mut ToolRegistry) {
        let mut used: HashSet<String> = HashSet::new();
        for slug in &self.order {
            let Some(slot) = self.servers.get(slug) else {
                continue;
            };
            Self::register_server_tools(
                slug,
                &slot.handle,
                &slot.tools,
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
            &tools,
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
    /// `registry`, returning the number of server tools registered (excluding
    /// the resource-catalogue wrappers).
    ///
    /// The listing itself is done by the caller (inside its connect/refresh
    /// budget), so this is pure registration: it cannot fail. The wrapper
    /// construction (names, disabled filter, resource-catalogue tools) is shared
    /// with the per-session overlay via [`build_server_wrappers`].
    fn register_server_tools(
        slug: &str,
        handle: &McpServerHandle,
        tools: &[McpTool],
        disabled: &[String],
        used: &mut HashSet<String>,
        registry: &mut ToolRegistry,
    ) -> usize {
        let built = build_server_wrappers(slug, handle, tools, disabled, used);
        let server_name = handle.name().to_string();
        registry.register_dynamic_group(built.group.clone(), format!("MCP server: {server_name}"));
        let count = built.tool_count;
        for (name, wrapper) in built.tools {
            registry.register_dynamic(name, &built.group, wrapper);
        }
        info!(
            server = %slug,
            name = %server_name,
            tool_count = count,
            "registered MCP server tools"
        );
        count
    }

    /// The number of `tools` not hidden by `disabled`.
    ///
    /// The per-server `/mcp` tool count excludes disabled tools, matching a
    /// fresh connect ([`McpManager::slot_from_server`]). Used to keep a slot's
    /// cached count in step after a re-list.
    fn enabled_tool_count(tools: &[McpTool], disabled: &[String]) -> usize {
        let disabled: HashSet<&str> = disabled.iter().map(String::as_str).collect();
        tools
            .iter()
            .filter(|t| !disabled.contains(t.name.as_str()))
            .count()
    }

    /// Shut down all MCP servers, joining each dispatcher with a bounded wait.
    ///
    /// Idempotent: a call with nothing left to shut down is a silent no-op.
    /// On the normal teardown path the command loop calls this explicitly (see
    /// `start_daemon_core`), and the manager's own `Drop` — the panic-path
    /// safety net — calls it a second time when `DaemonState` is dropped; the
    /// second call must not repeat the begin/end log pair for work the first
    /// already did. Every drained slot closes its connection on drop, so the
    /// three connection collections being empty *is* the "nothing to do"
    /// condition — nothing else on the manager is a live connection.
    pub fn shutdown_all(&mut self) {
        // `is_empty` is the single definition of "nothing left to shut down",
        // shared with the public predicate, so the no-op guard cannot drift from
        // the emptiness check the rest of the manager relies on.
        if self.is_empty() {
            return;
        }
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
    /// Safety net for the paths that drop the manager WITHOUT the command
    /// loop's explicit [`McpManager::shutdown_all`] call — a panicking command
    /// loop, or a manager built outside the daemon (tests). On the normal path
    /// it finds nothing to do and returns silently, because `shutdown_all` is
    /// idempotent; keeping it unconditional means a manager is always joined
    /// with the same bounded wait no matter how it goes out of scope.
    fn drop(&mut self) {
        self.shutdown_all();
    }
}

#[cfg(test)]
#[cfg(feature = "mcp")]
mod tests {
    use super::*;
    use choreo_mcp::McpServerConfig;

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
    fn shutdown_all_ignores_bookkeeping_and_is_idempotent() {
        // `configs`/`order`/`failures` are bookkeeping, not live connections: a
        // manager configured with a server that never connected has NOTHING to
        // shut down, so repeated calls are no-ops that leave the config view
        // intact. This is exactly the normal teardown sequence — the command
        // loop calls `shutdown_all` explicitly and the manager's `Drop` calls
        // it again — so the second call must not re-emit the begin/end log pair
        // over already-drained collections.
        let mut manager = McpManager::empty();
        manager.order = vec!["never-connected".to_string()];
        manager.configs.insert(
            "never-connected".to_string(),
            stdio_entry("never-connected", true),
        );

        manager.shutdown_all();
        manager.shutdown_all();

        assert!(manager.is_empty());
        assert_eq!(manager.order, ["never-connected"]);
        assert!(manager.configs.contains_key("never-connected"));
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
