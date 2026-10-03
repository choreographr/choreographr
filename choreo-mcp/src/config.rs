//! Per-server configuration, transport selection, and protocol-era negotiation.
//!
//! [`McpServerConfig`] is the fully-resolved shape the engine consumes: the
//! daemon's loader reads `mcp_servers.json`, resolves the transport (stdio or
//! Streamable HTTP) and the protocol era, and hands the crate one config per
//! server. Keeping the resolution out of the crate means the JSON-layer
//! concerns (key inference, `${ENV}` expansion) live next to the file format,
//! while the engine only ever sees a decided transport.

use std::collections::HashMap;
use std::time::Duration;

/// Default timeout for a single MCP operation (handshake, listing, or call)
/// when the server config does not set one.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(60);

/// Default cap on concurrent in-flight tool calls to one server.
///
/// Without a bound, a burst of calls from many sessions could spawn an
/// unbounded number of concurrent requests against one server; a small default
/// keeps one server from being swamped while still allowing real parallelism.
/// A server config may raise or lower it with `maxConcurrentCalls`.
pub const DEFAULT_MAX_CONCURRENT_CALLS: usize = 4;

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

/// The `transport` config key: which physical transport to use, or infer it.
///
/// Only the loader sees this type; [`McpServerConfig`] carries the already
/// resolved [`McpTransport`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum McpTransportKind {
    /// Infer from the keys present: `url` → HTTP, `command` → stdio.
    Auto,
    /// Launch a subprocess and speak JSON-RPC over its stdio.
    Stdio,
    /// Connect to a remote Streamable HTTP endpoint.
    Http,
}

impl McpTransportKind {
    /// Parse the `transport` config value (`"auto"`, `"stdio"`, `"http"`).
    ///
    /// An unrecognized value returns `None` so the caller can warn and keep the
    /// inferred default.
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "auto" => Some(Self::Auto),
            "stdio" | "process" => Some(Self::Stdio),
            "http" | "streamable-http" => Some(Self::Http),
            _ => None,
        }
    }
}

/// A resolved transport for one MCP server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum McpTransport {
    /// Launch a subprocess and speak newline-delimited JSON-RPC over its stdio.
    Stdio {
        /// The executable to launch (e.g. `npx`, `uvx`, or a server binary).
        command: String,
        /// Command-line arguments passed to `command`.
        args: Vec<String>,
        /// Extra environment variables for the subprocess.
        env: HashMap<String, String>,
        /// Working directory for the subprocess, when the config set one. A
        /// relative server bundle often expects to run from a specific
        /// directory; `None` inherits the daemon's own cwd.
        cwd: Option<String>,
        /// Optional path of a per-server log file. When set, the child's
        /// `stderr` is captured into this file (size-capped) instead of
        /// inheriting the daemon's own stderr, so a chatty server's
        /// diagnostics are isolated per server. `None` inherits stderr.
        log_path: Option<std::path::PathBuf>,
    },
    /// Connect to a remote server over the Streamable HTTP transport.
    Http {
        /// The single POST endpoint URL (e.g. `https://example.com/mcp`).
        url: String,
        /// Extra HTTP headers sent with every request (e.g. `Authorization`).
        headers: HashMap<String, String>,
    },
}

impl McpTransport {
    /// A short, log-friendly name for the transport (`"stdio"` / `"http"`).
    #[must_use]
    pub fn label(&self) -> &'static str {
        match self {
            Self::Stdio { .. } => "stdio",
            Self::Http { .. } => "http",
        }
    }

    /// The command (stdio) or URL (HTTP) the transport targets, for logs.
    #[must_use]
    pub fn target(&self) -> &str {
        match self {
            Self::Stdio { command, .. } => command,
            Self::Http { url, .. } => url,
        }
    }
}

/// Configuration for connecting to a single MCP server.
#[derive(Debug, Clone)]
pub struct McpServerConfig {
    /// Stable identifier for this server, used as a tool-name prefix.
    pub slug: String,
    /// The resolved transport (stdio subprocess or Streamable HTTP).
    pub transport: McpTransport,
    /// Whether this server is enabled for use.
    pub enabled: bool,
    /// Optional per-server request timeout, applied to the handshake, tool
    /// listing, and tool calls. When `None`, [`DEFAULT_TIMEOUT`] is used.
    pub timeout: Option<Duration>,
    /// Which protocol era to negotiate.
    pub protocol: McpProtocolMode,
    /// Optional cap on concurrent in-flight tool calls to this server. When
    /// `None`, [`DEFAULT_MAX_CONCURRENT_CALLS`] is used. Values below 1 are
    /// treated as 1 by [`max_concurrent_calls`](Self::max_concurrent_calls).
    pub max_concurrent_calls: Option<usize>,
    /// Tool names (as the server advertises them) to hide from the model.
    /// A server with many tools can have a few the operator never wants
    /// offered; listing them here keeps the rest of the server's catalogue
    /// intact. Matching is exact and against the server's original name.
    pub disabled_tools: Vec<String>,
}

impl McpServerConfig {
    /// The effective per-server request timeout (configured value or default).
    #[must_use]
    pub fn request_timeout(&self) -> Duration {
        self.timeout.unwrap_or(DEFAULT_TIMEOUT)
    }

    /// The effective cap on concurrent in-flight tool calls (configured value
    /// or [`DEFAULT_MAX_CONCURRENT_CALLS`]), clamped to at least 1 so a
    /// misconfigured `0` can never deadlock the dispatcher.
    #[must_use]
    pub fn max_concurrent_calls(&self) -> usize {
        self.max_concurrent_calls
            .unwrap_or(DEFAULT_MAX_CONCURRENT_CALLS)
            .max(1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(timeout: Option<Duration>) -> McpServerConfig {
        McpServerConfig {
            slug: "test".into(),
            transport: McpTransport::Stdio {
                command: "echo".into(),
                args: vec![],
                env: HashMap::new(),
                cwd: None,
                log_path: None,
            },
            enabled: true,
            timeout,
            protocol: McpProtocolMode::Auto,
            max_concurrent_calls: None,
            disabled_tools: Vec::new(),
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
    fn transport_kind_parses_and_rejects() {
        assert_eq!(
            McpTransportKind::parse("auto"),
            Some(McpTransportKind::Auto)
        );
        assert_eq!(
            McpTransportKind::parse("HTTP"),
            Some(McpTransportKind::Http)
        );
        assert_eq!(
            McpTransportKind::parse("stdio"),
            Some(McpTransportKind::Stdio)
        );
        assert_eq!(McpTransportKind::parse("smoke-signals"), None);
    }

    #[test]
    fn transport_labels_and_targets() {
        let stdio = McpTransport::Stdio {
            command: "npx".into(),
            args: vec![],
            env: HashMap::new(),
            cwd: None,
            log_path: None,
        };
        assert_eq!(stdio.label(), "stdio");
        assert_eq!(stdio.target(), "npx");

        let http = McpTransport::Http {
            url: "https://example.com/mcp".into(),
            headers: HashMap::new(),
        };
        assert_eq!(http.label(), "http");
        assert_eq!(http.target(), "https://example.com/mcp");
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

    #[test]
    fn max_concurrent_calls_defaults_when_unset() {
        assert_eq!(
            config(None).max_concurrent_calls(),
            DEFAULT_MAX_CONCURRENT_CALLS
        );
    }

    #[test]
    fn max_concurrent_calls_honors_config() {
        let mut cfg = config(None);
        cfg.max_concurrent_calls = Some(8);
        assert_eq!(cfg.max_concurrent_calls(), 8);
    }

    #[test]
    fn max_concurrent_calls_clamps_zero_to_one() {
        let mut cfg = config(None);
        cfg.max_concurrent_calls = Some(0);
        assert_eq!(cfg.max_concurrent_calls(), 1);
    }
}
