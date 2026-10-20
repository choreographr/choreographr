//! The MCP (Model Context Protocol) client: the `McpManager` type and the
//! modules it is split across.
//!
//! MCP tools are dynamic: the manager connects stdio and Streamable-HTTP
//! servers, pools and ref-counts their connections, and registers their
//! advertised tools into the daemon's tool registry under `mcp/<slug>` groups.
//! The manager is split across cohesive child modules — `paths`, `status`,
//! `project`, `pool`, `slot`, `overlay`, `query` — whose `impl McpManager`
//! blocks reach the manager's private fields, plus `stub` for the feature-off
//! build. The items other modules use are re-exported here so every
//! `crate::mcp::…` path is unchanged.

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
pub mod oauth;
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

/// A background MCP connect's outcome: the connected server plus its listed
/// tools, or the connect error message.
///
/// Carried across the command channel from a connect worker to the command
/// loop, which owns the pool and is the sole writer of the tool catalogue (see
/// `DaemonCommand::McpConnected`). `McpServer` is `Send` — it already lives in
/// `DaemonState` — so the whole outcome rides the channel by value.
#[cfg(feature = "mcp")]
pub type McpConnectOutcome = Result<(McpServer, Vec<McpTool>), String>;

// The connect plumbing is used by the daemon's background connect workers
// (`daemon::mcp_commands`, `server::core`), so the slot module's private helpers
// are re-exported here for those crate-level call sites.
#[cfg(feature = "mcp")]
pub(crate) use slot::{BACKGROUND_CONNECT_BUDGET, connect_entry};

// The asynchronous overlay plan/install types cross the command channel, so the
// daemon's off-loop connect handlers reach them through this re-export.
#[cfg(feature = "mcp")]
pub use overlay::PendingSessionConnect;
#[cfg(not(feature = "mcp"))]
pub use stub::McpManager;

#[cfg(feature = "mcp")]
use crate::tools::ToolRegistry;
#[cfg(feature = "mcp")]
use choreo_mcp::{McpListChange, McpServer, McpServerConfig, McpServerHandle, McpTool};
#[cfg(feature = "mcp")]
use config::McpEntry;
#[cfg(feature = "mcp")]
use slot::{
    CATALOGUE_REFRESH_BUDGET, RECONNECT_BUDGET, RECONNECT_TOTAL_BUDGET, ServerSlot, SharedSlot,
};
#[cfg(feature = "mcp")]
use std::collections::HashMap;
#[cfg(feature = "mcp")]
use std::collections::HashSet;
#[cfg(feature = "mcp")]
use std::path::PathBuf;
#[cfg(feature = "mcp")]
use std::time::Duration;
#[cfg(feature = "mcp")]
use tool::build_server_wrappers;
#[cfg(feature = "mcp")]
use tracing::{debug, info, warn};

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
    /// Daemon-tier shared entries awaiting their FIRST connect, handed to the
    /// daemon's background startup worker (via [`McpManager::take_startup_queue`]).
    /// The queue is populated by [`McpManager::new_from_config`] but drained only
    /// once the daemon core exists, because the connect workers need the
    /// command channel and the OAuth runtime snapshot that are installed there.
    startup_queue: Vec<McpEntry>,
    /// Slugs with a background (re)connect in flight. A second request for the
    /// same slug — a duplicate unlock, a sign-in racing the startup sweep — is
    /// dropped rather than spawning a second worker, so at most one connect per
    /// slug is ever outstanding.
    in_flight: HashSet<String>,
    /// Slugs an unlock asked to resync while a connect was ALREADY in flight.
    ///
    /// The TUI auto-unlocks immediately on connect, so the common case is an
    /// unlock landing while the background startup connect (which ran with an
    /// encrypted grant and will 401) is still handshaking. A failed in-flight
    /// connect whose slug is recorded here is retried ONCE, now that the
    /// credentials are live; success clears the entry so a healthy server is
    /// never churned.
    resync_requested: HashSet<String>,
}

#[cfg(feature = "mcp")]
impl McpManager {
    /// Load the daemon-tier MCP configuration and prepare the shared server
    /// queue, WITHOUT connecting anything.
    ///
    /// `shared = false` daemon-tier servers are NOT connected here: they are
    /// per-session, connected lazily when a session resolves its overlay (the
    /// daemon plans it against the pool and connects what is missing off-loop).
    /// The `shared` entries are returned by
    /// [`McpManager::take_startup_queue`] so the daemon core can connect them on
    /// a background worker — this constructor returns immediately, so
    /// `DaemonState::open` (and therefore the socket bind) never waits on an MCP
    /// handshake.
    pub fn new_from_config() -> Self {
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

        // Which daemon-tier entries the background startup worker should
        // connect: only the SHARED ones. A `shared = false` server is
        // per-session, connected lazily when a session resolves its overlay.
        let startup_queue: Vec<McpEntry> = configs.iter().filter(|c| c.shared).cloned().collect();
        for deferred in configs.iter().filter(|c| !c.shared) {
            debug!(
                server = %deferred.config.slug,
                "daemon-tier server is not shared; deferred to per-session connect"
            );
        }

        let (list_change_tx, list_change_rx) = crossbeam_channel::unbounded::<McpListChange>();

        Self {
            servers: HashMap::new(),
            configs: configs
                .iter()
                .map(|c| (c.config.slug.clone(), c.clone()))
                .collect(),
            order: configs.iter().map(|c| c.config.slug.clone()).collect(),
            failures: HashMap::new(),
            project_shared: HashMap::new(),
            session_slots: HashMap::new(),
            list_change_tx,
            list_change_rx: Some(list_change_rx),
            startup_queue,
            in_flight: HashSet::new(),
            resync_requested: HashSet::new(),
        }
    }

    /// Take the daemon-tier shared entries awaiting their first connect.
    /// Called once by `start_daemon_core`, which spawns the background workers
    /// once the command channel and OAuth runtime snapshot exist. Returning the
    /// config (rather than spawning in `new_from_config`) keeps every MCP
    /// connect off the `DaemonState::open` path, so the socket is bound and
    /// clients are served before any handshake starts.
    pub fn take_startup_queue(&mut self) -> Vec<McpEntry> {
        std::mem::take(&mut self.startup_queue)
    }

    /// Blocking startup seam: connect every queued daemon-tier shared server
    /// synchronously (each bounded by `budget`) and register their CACHED tools
    /// into `registry`.
    ///
    /// The daemon never calls this — it connects on background workers so
    /// startup is never blocked. This exists for an embedder or a test harness
    /// that wants a fully populated catalogue before proceeding, and is the
    /// direct executable counterpart of the work `new_from_config` queues.
    pub fn connect_all_blocking(&mut self, registry: &mut ToolRegistry, budget: Duration) {
        for entry in self.take_startup_queue() {
            let slug = entry.config.slug.clone();
            if let Some(slot) = Self::connect_slot(&self.list_change_tx, &entry.config, budget) {
                self.failures.remove(&slug);
                self.servers.insert(slug, slot);
            } else {
                warn!(server = %slug, "MCP server did not connect within the budget");
                self.failures.insert(
                    slug,
                    format!("did not connect within {}s", budget.as_secs()),
                );
            }
        }
        self.register_cached(registry);
    }

    /// Mark `slug` as having a background connect in flight, returning `false`
    /// when one already is (the caller must then skip spawning a second).
    pub fn mark_in_flight(&mut self, slug: &str) -> bool {
        self.in_flight.insert(slug.to_string())
    }

    /// Record that `slug` should be retried if its in-flight connect fails (an
    /// unlock asked to resync while the connect was running).
    pub fn request_resync(&mut self, slug: &str) {
        self.resync_requested.insert(slug.to_string());
    }

    /// Take (and clear) a pending resync request for `slug`. Returns `true` at
    /// most once per request, so a failed connect triggers exactly one retry.
    pub fn take_resync_request(&mut self, slug: &str) -> bool {
        self.resync_requested.remove(slug)
    }

    /// Clear `slug`'s in-flight marker (a connect either landed or failed).
    pub fn clear_in_flight(&mut self, slug: &str) {
        self.in_flight.remove(slug);
    }

    /// Whether `slug` is a configured daemon-tier SHARED server (connected or
    /// not).
    ///
    /// Only a shared entry is connected at the daemon level: a `shared = false`
    /// entry belongs to each referencing session's overlay, so its connection
    /// lives in `session_slots` and is reconciled by the overlay resolution, not
    /// by a daemon connect or by [`McpManager::install_connected`].
    #[must_use]
    pub fn is_shared_configured(&self, slug: &str) -> bool {
        self.configs.get(slug).is_some_and(|e| e.shared)
    }

    /// Whether `slug` is a configured daemon-tier server (connected or not).
    #[must_use]
    pub fn is_configured(&self, slug: &str) -> bool {
        self.configs.contains_key(slug)
    }

    /// The last recorded connect/refresh failure for `slug`, if any.
    #[must_use]
    pub fn failure(&self, slug: &str) -> Option<String> {
        self.failures.get(slug).cloned()
    }

    /// Whether `slug` has a live daemon-tier shared connection.
    #[must_use]
    pub fn is_connected(&self, slug: &str) -> bool {
        self.servers.contains_key(slug)
    }

    /// The connect inputs for a background (re)connect of the daemon-tier SHARED
    /// server `slug`: its resolved config and a clone of the list-change sender
    /// (so the rebuilt transport re-subscribes).
    ///
    /// `None` when no such daemon-tier entry is configured, or when the entry is
    /// `shared = false`: a per-session entry is connected per session by the
    /// overlay resolution, and a daemon-tier connect of it would register a
    /// second connection whose tools duplicate the session's private ones.
    #[must_use]
    pub fn connect_inputs(
        &self,
        slug: &str,
    ) -> Option<(McpServerConfig, crossbeam_channel::Sender<McpListChange>)> {
        let entry = self.configs.get(slug).filter(|e| e.shared)?;
        Some((entry.config.clone(), self.list_change_tx.clone()))
    }

    /// Install a daemon-tier SHARED server connected by a background worker,
    /// replacing any existing connection for `slug`.
    ///
    /// The caller (the command loop) rebuilds the tool catalogue from the cached
    /// tool sets afterwards, so registration order and tool naming are derived
    /// from the manager's `order`, never from the order connects complete. A
    /// slug that is no longer a configured SHARED server (removed or flipped to
    /// `shared = false` by a reload while the connect was in flight) is
    /// rejected: the freshly connected server is dropped.
    ///
    /// # Errors
    ///
    /// Returns the slug's absence from the configured shared set as an error
    /// message.
    pub fn install_connected(
        &mut self,
        slug: &str,
        server: McpServer,
        tools: &[McpTool],
    ) -> Result<(), String> {
        if !self.is_shared_configured(slug) {
            return Err(format!(
                "server {slug:?} is no longer a configured daemon-tier shared server"
            ));
        }
        let config = self
            .configs
            .get(slug)
            .map(|e| e.config.clone())
            .ok_or_else(|| format!("server {slug:?} is no longer configured"))?;
        // The slot derives its tool count from the tools + disabled filter, so
        // the `/mcp` status surface reports the real count without a re-list.
        let slot = Self::slot_from_server(server, tools, config);
        let tool_count = slot.tool_count;
        self.servers.insert(slug.to_string(), slot);
        self.failures.remove(slug);
        self.in_flight.remove(slug);
        info!(server = %slug, tool_count, "connected MCP server (background)");
        Ok(())
    }

    /// Record a background connect's failure for `slug`, clearing its in-flight
    /// marker so a later attempt (a reload, a reconnect, an unlock) can retry.
    pub fn note_connect_failure(&mut self, slug: &str, error: String) {
        self.in_flight.remove(slug);
        warn!(server = %slug, error = %error, "background MCP connect failed");
        self.failures.insert(slug.to_string(), error);
    }

    /// Take the shared list-changed receiver, if a server could have opened a
    /// subscription. Called once by the daemon command-loop assembly, which
    /// spawns a forwarder thread pumping it into [`DaemonCommand::McpListChanged`].
    ///
    /// [`DaemonCommand::McpListChanged`]: crate::daemon::DaemonCommand::McpListChanged
    pub fn take_list_change_rx(&mut self) -> Option<crossbeam_channel::Receiver<McpListChange>> {
        self.list_change_rx.take()
    }

    /// Re-list ONE daemon-tier shared server and update its cached tool set.
    ///
    /// A list-changed event names exactly one server, and the catalogue is
    /// rebuilt from every other server's CACHE, so the command loop pays one
    /// round-trip per event rather than one per connected server.
    ///
    /// The re-listing is bounded by the short `CATALOGUE_REFRESH_BUDGET`
    /// deadline (not the per-server request timeout), and a server that misses
    /// it — or fails to list — keeps its previous tool set with a warning, so its
    /// group never blinks out of the catalogue; it is re-listed on the next
    /// event.
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
    /// of the catalogue; it is re-listed on the next event.
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
    /// The catalogue-build counterpart of the re-list in
    /// [`McpManager::refresh_server`]: after the one changed server has been
    /// re-listed, this rebuilds the daemon-wide catalogue from every server's
    /// cached `tools`. It walks `self.order` with a fresh `used` set so the
    /// registered tool names and groups are deterministic, while the network
    /// round-trips are skipped. It is the single catalogue-build path: a
    /// background connect landing, a list change, and a reload dropping a server
    /// all end here.
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
            startup_queue: Vec::new(),
            in_flight: HashSet::new(),
            resync_requested: HashSet::new(),
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
    fn new_from_config_with_no_file_creates_empty() {
        let dir = tempfile::tempdir().unwrap();
        config::set_test_config_root(Some(dir.path().to_path_buf()));
        let mut manager = McpManager::new_from_config();
        config::set_test_config_root(None);
        assert!(manager.is_empty());
        assert!(manager.take_startup_queue().is_empty());
        assert_eq!(manager.status().len(), 0);
    }

    #[test]
    fn new_from_config_queues_only_shared_servers() {
        // The startup queue must contain only the `shared` daemon-tier servers
        // (they are connected on the background worker); a `shared = false`
        // server is per-session and connected lazily, so it must NOT be queued.
        let dir = tempfile::tempdir().unwrap();
        let cfg = dir.path().join("choreographr");
        std::fs::create_dir_all(&cfg).unwrap();
        std::fs::write(
            cfg.join("mcp.json"),
            r#"{"mcpServers":{"shared":{"command":"true"},"private":{"command":"true","shared":false}}}"#,
        )
        .unwrap();
        config::set_test_config_root(Some(dir.path().to_path_buf()));
        let mut manager = McpManager::new_from_config();
        config::set_test_config_root(None);

        assert!(manager.is_empty(), "construction connects nothing");
        assert!(manager.is_configured("shared"));
        assert!(manager.is_configured("private"));
        let queue = manager.take_startup_queue();
        let slugs: Vec<&str> = queue.iter().map(|e| e.config.slug.as_str()).collect();
        assert_eq!(slugs, ["shared"], "only the shared entry is queued");
        assert!(
            manager.take_startup_queue().is_empty(),
            "the queue drains once"
        );
    }

    #[test]
    fn in_flight_and_resync_are_one_shot() {
        let mut manager = McpManager::empty();
        assert!(manager.mark_in_flight("a"), "first mark takes");
        assert!(!manager.mark_in_flight("a"), "a second mark is rejected");
        // A resync request is consumed exactly once, so a failed connect retries
        // at most once.
        manager.request_resync("a");
        assert!(manager.take_resync_request("a"));
        assert!(!manager.take_resync_request("a"));
        // Clearing the in-flight marker (a connect settled) re-arms it.
        manager.clear_in_flight("a");
        assert!(manager.mark_in_flight("a"));
    }

    #[test]
    fn connect_inputs_are_shared_only() {
        // Only a `shared = true` entry is connected at the daemon level. A daemon
        // `shared = false` entry belongs to each referencing session's overlay, so
        // a daemon-tier connect of it would pool a SECOND connection whose tools
        // duplicate the session's private ones.
        let mut manager = McpManager::empty();
        manager
            .configs
            .insert("shared".to_string(), stdio_entry("shared", true));
        manager
            .configs
            .insert("per".to_string(), stdio_entry("per", false));
        manager.order = vec!["per".to_string(), "shared".to_string()];

        assert!(
            manager.connect_inputs("shared").is_some(),
            "a shared entry is connectable in the background"
        );
        assert!(
            manager.connect_inputs("per").is_none(),
            "a per-session entry has no daemon-tier connect"
        );

        // Both are CONFIGURED (so status/reload see them); only one is shared.
        assert!(manager.is_configured("per"));
        assert!(manager.is_shared_configured("shared"));
        assert!(
            !manager.is_shared_configured("per"),
            "a per-session entry is not a daemon-tier shared server"
        );
    }

    #[test]
    fn note_connect_failure_records_error_and_clears_in_flight() {
        let mut manager = McpManager::empty();
        manager.mark_in_flight("a");
        manager.note_connect_failure("a", "boom".to_string());
        assert_eq!(manager.failure("a").as_deref(), Some("boom"));
        assert!(
            manager.mark_in_flight("a"),
            "a failure clears the in-flight marker so a retry can start"
        );
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
