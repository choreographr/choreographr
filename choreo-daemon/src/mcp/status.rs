//! The MCP status / report / outcome value types.
//!
//! These are the daemon-facing snapshots and results the `/mcp` surface and
//! `session_inspect` return. They are defined UNCONDITIONALLY (outside the `mcp`
//! feature gate) so a status-returning daemon command and its callers compile no
//! matter how the daemon is built; without the feature the server lists are
//! simply always empty.

use std::path::PathBuf;

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
