//! Feature-off `McpManager` stub.
//!
//! Mirrors the metrics-module stub convention: without the `mcp` feature there
//! is no `choreo-mcp` dependency, so the manager holds no servers. Construction,
//! teardown, and `Drop` are all no-ops, and the API surface matches the real
//! manager exactly (same method signatures) so call sites need no feature cfgs.

use crate::tools::ToolRegistry;

use super::{McpReloadOutcome, McpServerStatus, SessionMcpOverlay};

/// No-op stand-in for the real `McpManager` (see the module docs).
pub struct McpManager;

impl McpManager {
    /// Stub: no MCP config is loaded and no servers are spawned.
    pub fn from_config(_registry: &mut ToolRegistry) -> Self {
        Self
    }

    /// Stub: no server has any tools to (re-)register.
    pub fn register_all(&mut self, _registry: &mut ToolRegistry) {}

    /// Stub: there are never any cached tools to register.
    pub fn register_cached(&self, _registry: &mut ToolRegistry) {}

    /// Stub: there is never a server to re-list.
    pub fn refresh_server(&mut self, _slug: &str) {}

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
