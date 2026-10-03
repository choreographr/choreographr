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
use choreo_mcp::{McpListChange, McpServer, McpServerHandle};
#[cfg(feature = "mcp")]
use std::collections::HashMap;
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
}

/// Manages all MCP server connections and their registered tools.
#[cfg(feature = "mcp")]
pub struct McpManager {
    /// One slot per server, keyed by server slug.
    servers: HashMap<String, ServerSlot>,
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
    /// detaches and its connection is dropped).
    pub fn from_config(registry: &mut ToolRegistry) -> Self {
        let configs = match config::load_mcp_config() {
            Ok(configs) => configs,
            Err(e) => {
                warn!("failed to load MCP config: {e}");
                Vec::new()
            }
        };

        // One shared list-changed channel: every server's subscription task
        // sends into this sender, and the daemon consumes the single receiver
        // (see `take_list_change_rx`). Created even when no server supports
        // subscriptions — the receiver is simply never taken, or the channel
        // stays empty.
        let (list_change_tx, list_change_rx) = crossbeam_channel::unbounded::<McpListChange>();

        let mut manager = Self {
            servers: HashMap::new(),
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
        for (slug, handle) in pending {
            let remaining = deadline.saturating_duration_since(Instant::now());
            match join_with_budget(handle, remaining) {
                Some(Ok(server)) => {
                    Self::register_server(&slug, server, registry, &mut manager);
                }
                Some(Err(e)) => {
                    error!(server = %slug, error = %e, "failed to connect MCP server");
                }
                None => {
                    warn!(
                        server = %slug,
                        budget_ms = STARTUP_BUDGET.as_millis(),
                        "MCP server did not connect within the startup budget; skipping"
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
        for (slug, slot) in &self.servers {
            if let Err(e) = Self::register_server_tools(slug, &slot.handle, registry) {
                warn!(server = %slug, error = %e, "failed to list MCP tools during registry refresh");
            }
        }
    }

    /// Discover a server's tools and register them (and the server) in the
    /// manager. A listing failure drops the server, which shuts it down.
    fn register_server(
        slug: &str,
        server: McpServer,
        registry: &mut ToolRegistry,
        manager: &mut Self,
    ) {
        let handle = server.handle();
        match Self::register_server_tools(slug, &handle, registry) {
            Ok(()) => {
                manager
                    .servers
                    .insert(slug.to_string(), ServerSlot { handle, server });
            }
            Err(e) => {
                error!(server = %slug, error = %e, "failed to list MCP tools; dropping server");
                // `server` is dropped here, shutting the connection down.
            }
        }
    }

    /// Register one server's advertised tools (and its catalogue group) into
    /// `registry`.
    ///
    /// Shared by startup ([`register_server`](Self::register_server)) and a
    /// post-list-change refresh ([`register_all`](Self::register_all)) so the
    /// naming and resource-tool rules never drift. A server that declares the
    /// `resources` capability additionally gets the `list_resources` /
    /// `read_resource` catalogue tools; a server without it would only fail the
    /// call, so they are not offered.
    ///
    /// # Errors
    ///
    /// Returns the listing error when `tools/list` fails.
    fn register_server_tools(
        slug: &str,
        handle: &McpServerHandle,
        registry: &mut ToolRegistry,
    ) -> Result<(), choreo_mcp::McpError> {
        let server_name = handle.name().to_string();
        let tools = handle.list_tools()?;
        registry
            .register_dynamic_group(format!("mcp/{slug}"), format!("MCP server: {server_name}"));
        info!(
            server = %slug,
            name = %server_name,
            tool_count = tools.len(),
            "registered MCP server tools"
        );
        for mcp_tool in tools {
            let description = mcp_tool.description.unwrap_or_default();
            let wrapper = McpToolWrapper::new(
                slug,
                &mcp_tool.name,
                &description,
                mcp_tool.input_schema,
                mcp_tool.output_schema,
                handle.clone(),
            );
            // Own the strings BEFORE moving `wrapper` into the box: a
            // borrow extending into the call would conflict with the
            // move.
            let name = wrapper.name().to_string();
            let group = wrapper.group().to_string();
            registry.register_dynamic(name, &group, Box::new(wrapper));
        }
        // A server that declares the `resources` capability gets the
        // catalogue tools; a server without it would only fail the call, so
        // they are not offered.
        if handle.supports_resources() {
            let group = format!("mcp/{slug}");
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
        Ok(())
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
        Self {
            servers: HashMap::new(),
            list_change_rx: None,
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
        let mut registry = crate::tools::ToolRegistry::new();
        let manager = McpManager::from_config(&mut registry);
        assert!(manager.is_empty());
    }
}
