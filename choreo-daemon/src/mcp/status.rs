//! The MCP status / report / outcome value types.
//!
//! These are the daemon-facing snapshots and results the `/mcp` surface and
//! `session_inspect` return. They are defined UNCONDITIONALLY (outside the `mcp`
//! feature gate) so a status-returning daemon command and its callers compile no
//! matter how the daemon is built; without the feature the server lists are
//! simply always empty.

use std::path::PathBuf;

use choreo_proto::{McpAuthState, McpOAuthConfig};

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
    /// The target-scoped keystore credential key for a remote server, computed
    /// by the daemon (the sole authority) so provisioning and connect-time load
    /// agree. `None` for a stdio server.
    pub credential_key: Option<String>,
    /// The authentication this server uses (see [`McpAuthState`]). Filled by
    /// the daemon from its credential store; the manager leaves the default and
    /// the status handler overrides it.
    pub auth: McpAuthState,
    /// The server's resolved `oauth` block (no secret), when one is configured
    /// and the server is trusted — so a frontend can run `mcp login` without a
    /// copy of the config.
    pub oauth: Option<McpOAuthConfig>,
}

impl McpServerStatus {
    /// A one-line human-readable summary of this server's state.
    #[must_use]
    pub fn summary(&self) -> String {
        let base = if self.connected {
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
        };
        // Append the auth label only when the state is actionable, so a server
        // needing a re-login is visible in `/mcp` without adding noise to the
        // common authenticated / no-auth cases. The label comes from the wire
        // type so the two summaries cannot drift.
        match self.auth.label() {
            "" => base,
            label => format!("{base} [{label}]"),
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

#[cfg(test)]
mod tests {
    use super::*;

    /// The daemon-side summary appends the same auth label as the wire type, so
    /// a server needing a re-login is visible in `/mcp`. `choreo-proto`'s
    /// `types/tests.rs` pins the wire rendering; this pins the daemon one so the
    /// two cannot silently drift.
    #[test]
    fn summary_renders_the_auth_label() {
        fn status(auth: McpAuthState) -> McpServerStatus {
            McpServerStatus {
                slug: "docs".into(),
                tier: "daemon".into(),
                transport: "http".into(),
                target: "https://example.com/mcp".into(),
                connected: true,
                tool_count: 3,
                server_name: Some("docs-server".into()),
                server_version: Some("1.0.0".into()),
                last_error: None,
                credential_key: Some("mcp:docs|abc".into()),
                auth,
                oauth: None,
            }
        }

        // A plain, unauthenticated server carries no trailing auth suffix.
        assert_eq!(
            status(McpAuthState::NotApplicable).summary(),
            "docs [http → https://example.com/mcp] connected: docs-server 1.0.0, 3 tool(s)"
        );
        // A terminally rejected grant must read as a re-login prompt.
        assert_eq!(
            status(McpAuthState::Reauthentication).summary(),
            "docs [http → https://example.com/mcp] connected: docs-server 1.0.0, 3 tool(s) [re-authentication required]"
        );
    }
}
