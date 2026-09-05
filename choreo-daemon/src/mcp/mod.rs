// Real implementation (spawn/handshake/discover/shutdown over stdio) is
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
use anyhow::{Context, Result};
#[cfg(feature = "mcp")]
use choreo_mcp::{McpClient, McpServerConfig};
#[cfg(feature = "mcp")]
use std::collections::HashMap;
#[cfg(feature = "mcp")]
use std::sync::{Arc, Mutex};
#[cfg(feature = "mcp")]
use tool::McpToolWrapper;
#[cfg(feature = "mcp")]
use tracing::{debug, error, info, warn};

/// Manages all MCP server subprocesses and their registered tools.
#[cfg(feature = "mcp")]
pub struct McpManager {
    /// MCP client per server, keyed by server slug. Arc<Mutex<>> so
    /// McpToolWrapper instances can share the same client reference.
    clients: HashMap<String, Arc<Mutex<McpClient>>>,
}

#[cfg(feature = "mcp")]
impl McpManager {
    /// Spawn a single MCP server subprocess and perform the initialize handshake.
    fn spawn_server(cfg: &McpServerConfig) -> Result<McpClient> {
        info!(
            server = %cfg.slug,
            command = %cfg.command,
            "spawning MCP server"
        );
        McpClient::spawn(cfg).with_context(|| format!("failed to spawn MCP server '{}'", cfg.slug))
    }

    /// Discover tools from an MCP client and register them in the ToolRegistry.
    fn register_server_tools(
        slug: &str,
        client: &mut McpClient,
        registry: &mut ToolRegistry,
        shared: Arc<Mutex<McpClient>>,
    ) {
        match client.list_tools() {
            Ok(tools) => {
                registry.register_dynamic_group(
                    format!("mcp/{slug}"),
                    format!("MCP server: {}", client.server_name()),
                );

                info!(
                    server = %slug,
                    name = %client.server_name(),
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
                        Arc::clone(&shared),
                    );
                    registry.register_dynamic(
                        wrapper.name().to_string(),
                        wrapper.group().to_string(),
                        Box::new(wrapper),
                    );
                }
            }
            Err(e) => {
                error!(
                    server = %slug,
                    error = %e,
                    "failed to list MCP tools, shutting down server"
                );
                // shared (Arc<Mutex<McpClient>>) is dropped here, killing the subprocess
            }
        }
    }

    /// Create a new McpManager, spawn all enabled servers, discover their
    /// tools, and register them in the ToolRegistry.
    pub fn from_config(registry: &mut ToolRegistry) -> Self {
        let configs = match config::load_mcp_config() {
            Ok(c) => c,
            Err(e) => {
                warn!("failed to load MCP config: {e}");
                Vec::new()
            }
        };

        let mut manager = Self {
            clients: HashMap::new(),
        };

        // Spawn all servers in parallel on background threads.
        let mut handles: Vec<(String, std::thread::JoinHandle<anyhow::Result<McpClient>>)> =
            Vec::new();

        for cfg in &configs {
            let slug = cfg.slug.clone();
            let cfg_clone = cfg.clone();
            let handle = std::thread::spawn(move || Self::spawn_server(&cfg_clone));
            handles.push((slug, handle));
        }

        // Collect results and register each server's tools.
        for (slug, handle) in handles {
            match handle.join() {
                Ok(Ok(client)) => {
                    let shared = Arc::new(Mutex::new(client));
                    let mut guard = match shared.lock() {
                        Ok(g) => g,
                        Err(e) => {
                            error!(
                                server = %slug,
                                "MCP client lock poisoned: {e}"
                            );
                            continue;
                        }
                    };

                    Self::register_server_tools(&slug, &mut guard, registry, Arc::clone(&shared));

                    // Drop the lock so the manager doesn't hold it while storing the Arc.
                    drop(guard);

                    manager.clients.insert(slug, shared);
                }
                Ok(Err(e)) => {
                    error!(server = %slug, error = %e, "failed to spawn MCP server");
                }
                Err(_) => {
                    error!(server = %slug, "MCP server spawn thread panicked");
                }
            }
        }

        manager
    }

    /// Shut down all MCP servers.
    pub fn shutdown_all(&mut self) {
        info!(count = self.clients.len(), "shutting down MCP servers");
        for (slug, shared) in self.clients.drain() {
            // Log per-server BEGIN and END: if a Ctrl+C wedge stops between
            // the two, THIS server is the culprit — either its client mutex
            // is held by a stuck tool call (blocked before the "done" line)
            // or its transport shutdown hung (after it).
            debug!(server = %slug, "waiting for MCP client lock + shutdown");
            match shared.lock() {
                Ok(mut client) => {
                    debug!(server = %slug, "shutting down MCP server");
                    client.shutdown();
                }
                Err(e) => {
                    warn!(
                        server = %slug,
                        "MCP client lock poisoned during shutdown: {e}"
                    );
                }
            }
            debug!(server = %slug, "MCP server shut down");
        }
        info!("all MCP servers shut down");
    }

    /// Create an empty McpManager with no servers (for testing).
    pub fn empty() -> Self {
        Self {
            clients: HashMap::new(),
        }
    }

    /// Return a reference to the clients map (for testing/inspection).
    pub fn clients(&self) -> &HashMap<String, Arc<Mutex<McpClient>>> {
        &self.clients
    }
}

#[cfg(feature = "mcp")]
impl Drop for McpManager {
    fn drop(&mut self) {
        self.shutdown_all();
    }
}

// Feature-off stub: mirrors the metrics-module stub convention. The manager
// holds no clients (there is nothing to manage without the choreo-mcp
// dependency), so construction, teardown, and Drop are all no-ops. The API
// surface matches the real manager exactly — same method signatures — so
// callers cannot tell the difference, and no call site needs feature cfgs.
#[cfg(not(feature = "mcp"))]
mod imp {
    use crate::tools::ToolRegistry;

    /// No-op stand-in for the real McpManager (see the module-level cfg note).
    pub struct McpManager;

    impl McpManager {
        /// Stub: no MCP config is loaded and no servers are spawned.
        pub fn from_config(_registry: &mut ToolRegistry) -> Self {
            Self
        }

        /// Stub: there are no servers to shut down.
        pub fn shutdown_all(&mut self) {}

        /// Stub: creates an empty manager (same seam the real one exposes for
        /// tests).
        pub fn empty() -> Self {
            Self
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
    fn empty_creates_manager_with_no_clients() {
        let manager = McpManager::empty();
        assert!(manager.clients().is_empty());
    }

    #[test]
    fn shutdown_all_on_empty_is_noop() {
        let mut manager = McpManager::empty();
        // Should not panic or error
        manager.shutdown_all();
        assert!(manager.clients().is_empty());
    }

    #[test]
    fn drop_empty_manager_is_noop() {
        // Just verify dropping an empty manager doesn't panic
        let manager = McpManager::empty();
        drop(manager);
    }

    #[test]
    fn from_config_with_no_file_creates_empty() {
        let mut registry = crate::tools::ToolRegistry::new();
        let manager = McpManager::from_config(&mut registry);
        assert!(manager.clients().is_empty());
    }
}
