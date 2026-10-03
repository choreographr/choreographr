//! Per-server configuration and protocol-era selection.

use std::collections::HashMap;
use std::time::Duration;

/// Default timeout for a single MCP operation (handshake, listing, or call)
/// when the server config does not set one.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(60);

/// Which MCP protocol era a client should establish with a server.
///
/// The default is [`Self::Auto`], the spec-recommended behaviour: probe the
/// server with `server/discover` (a stateless-era handshake) and fall back to
/// the legacy `initialize` handshake only when the probe reports the peer is
/// legacy or does not answer in time. A recognized *modern* rejection (e.g.
/// `UnsupportedProtocolVersionError`) never falls back — it is reported.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum McpProtocolMode {
    /// Probe with `server/discover`, fall back to `initialize` on a legacy peer
    /// or a probe timeout (never on a recognized modern rejection).
    #[default]
    Auto,
    /// Force the legacy `initialize` handshake (for servers that mishandle
    /// pre-init traffic, and for tests).
    Legacy,
    /// Speak only the stateless era; refuse a peer that is legacy-only.
    Modern,
}

impl McpProtocolMode {
    /// Parse the `protocol` config value.
    ///
    /// Accepts `"auto"`, `"legacy"`, and `"modern"` (plus the explicit era
    /// string `"2026-07-28"` as an alias for `"modern"`). An unrecognized value
    /// returns `None` so the caller can warn and keep the default.
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "auto" => Some(Self::Auto),
            "legacy" | "initialize" => Some(Self::Legacy),
            "modern" | "2026-07-28" => Some(Self::Modern),
            _ => None,
        }
    }
}

/// Configuration for connecting to a single MCP server subprocess.
#[derive(Debug, Clone)]
pub struct McpServerConfig {
    /// Stable identifier for this server, used as a tool-name prefix.
    pub slug: String,
    /// The executable to launch (e.g. `npx`, `uvx`, or a server binary).
    pub command: String,
    /// Command-line arguments passed to `command`.
    pub args: Vec<String>,
    /// Extra environment variables for the subprocess.
    pub env: HashMap<String, String>,
    /// Whether this server is enabled for use.
    pub enabled: bool,
    /// Optional per-server request timeout, applied to the handshake, tool
    /// listing, and tool calls. When `None`, [`DEFAULT_TIMEOUT`] is used.
    pub timeout: Option<Duration>,
    /// Which protocol era to negotiate.
    pub protocol: McpProtocolMode,
}

impl McpServerConfig {
    /// The effective per-server request timeout (configured value or default).
    #[must_use]
    pub fn request_timeout(&self) -> Duration {
        self.timeout.unwrap_or(DEFAULT_TIMEOUT)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(timeout: Option<Duration>) -> McpServerConfig {
        McpServerConfig {
            slug: "test".into(),
            command: "echo".into(),
            args: vec![],
            env: HashMap::new(),
            enabled: true,
            timeout,
            protocol: McpProtocolMode::Auto,
        }
    }

    #[test]
    fn protocol_mode_default_is_auto() {
        assert_eq!(McpProtocolMode::default(), McpProtocolMode::Auto);
    }

    #[test]
    fn protocol_mode_parses_aliases() {
        assert_eq!(McpProtocolMode::parse("auto"), Some(McpProtocolMode::Auto));
        assert_eq!(
            McpProtocolMode::parse("LEGACY"),
            Some(McpProtocolMode::Legacy)
        );
        assert_eq!(
            McpProtocolMode::parse("2026-07-28"),
            Some(McpProtocolMode::Modern)
        );
        assert_eq!(McpProtocolMode::parse("nope"), None);
    }

    #[test]
    fn request_timeout_defaults_when_unset() {
        assert_eq!(config(None).request_timeout(), DEFAULT_TIMEOUT);
    }

    #[test]
    fn request_timeout_honors_config() {
        let cfg = config(Some(Duration::from_secs(5)));
        assert_eq!(cfg.request_timeout(), Duration::from_secs(5));
    }
}
