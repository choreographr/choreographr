//! Error type shared across the MCP client, dispatcher, and engine layers.

use std::io;

/// Errors produced by the MCP client, its per-server dispatcher, and the
/// `rmcp`-backed engine.
///
/// The daemon maps this into its own `ToolExecError` at the boundary. The
/// variants distinguish the failure stage (subprocess spawn, handshake, a
/// server-reported JSON-RPC error, transport I/O, a deadline, or a client-side
/// cancellation), so a caller can react — never crash — on any of them.
#[derive(Debug, thiserror::Error)]
pub enum McpError {
    /// The MCP server subprocess could not be spawned or its stdio captured.
    #[error("failed to spawn subprocess: {0}")]
    SpawnFailed(String),

    /// The lifecycle handshake failed or returned a malformed response.
    ///
    /// Covers both the legacy `initialize` handshake and the stateless
    /// `server/discover` probe: whichever era was negotiated, a failure to
    /// establish the connection surfaces here.
    #[error("MCP handshake failed: {0}")]
    InitializeFailed(String),

    /// The server returned a JSON-RPC error response.
    #[error("JSON-RPC error: code={code} message={message}")]
    JsonRpcError {
        /// The JSON-RPC error code (e.g. `-32601`).
        code: i64,
        /// The server-supplied error message.
        message: String,
    },

    /// A protocol-level error (serialization failure or malformed response).
    #[error("protocol error: {0}")]
    ProtocolError(String),

    /// No response arrived before the call's deadline.
    #[error("tool call timed out")]
    Timeout,

    /// An underlying I/O error on the transport.
    #[error("I/O error: {0}")]
    Io(#[from] io::Error),

    /// The server closed the connection or the transport died unexpectedly.
    #[error("MCP server connection closed")]
    ServerShutdown,

    /// A send failed at the transport layer (a dropped connection, a refused
    /// request) as distinct from a protocol-level error in the message payload.
    ///
    /// This is a reconnect trigger: a request or listing that fails this way
    /// tells the dispatcher the connection is (or may be) dead, so the bounded
    /// restart policy rebuilds it. A protocol error (a malformed payload, an
    /// unexpected result type) is a settled answer and does not trigger a
    /// rebuild. An HTTP authorization challenge is deliberately *not* mapped
    /// here — it surfaces as [`Self::AuthRequired`], which is actionable rather
    /// than transient.
    #[error("MCP transport error: {0}")]
    Transport(String),

    /// The client cancelled the request before it completed.
    ///
    /// Raised when a session cancel stops an in-flight call; the dispatcher
    /// also sends the server a best-effort `notifications/cancelled` so a
    /// cooperative peer can stop work.
    #[error("MCP request cancelled")]
    Cancelled,

    /// The per-server dispatcher is gone, so no command can be delivered.
    ///
    /// Distinct from [`Self::ServerShutdown`]: the connection may still be
    /// alive, but the client-side worker thread that would carry the command
    /// has exited (e.g. after an explicit shutdown).
    #[error("MCP server dispatcher is not running")]
    NotConnected,

    /// The configured endpoint speaks a transport this client does not
    /// implement.
    ///
    /// The only such case today is the deprecated 2024-11-05 HTTP+SSE
    /// transport (a single GET that returns an `endpoint` event), which the
    /// spec removed; the message names it so a user knows to move the server to
    /// Streamable HTTP rather than debugging an opaque connect failure.
    #[error("unsupported MCP transport: {0}")]
    UnsupportedTransport(String),

    /// The remote server rejected the request as unauthorized
    /// (HTTP 401 or 403).
    ///
    /// A remote server that requires a credential this client did not send
    /// fails here. The message is actionable: it names the two supported
    /// options — a static token in the server's `headers`, or waiting for
    /// OAuth support — rather than leaving the raw status to be decoded.
    #[error("authorization required by {server}: {hint}")]
    AuthRequired {
        /// The server slug whose endpoint rejected the request.
        server: String,
        /// The actionable guidance (which header to set, and the OAuth note).
        hint: String,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_spawn_failed_display() {
        let err = McpError::SpawnFailed("binary not found".into());
        assert!(err.to_string().contains("binary not found"));
    }

    #[test]
    fn error_initialize_failed_display() {
        let err = McpError::InitializeFailed("version mismatch".into());
        assert!(err.to_string().contains("version mismatch"));
    }

    #[test]
    fn error_json_rpc_error_display() {
        let err = McpError::JsonRpcError {
            code: -32601,
            message: "method not found".into(),
        };
        let msg = err.to_string();
        assert!(msg.contains("-32601"));
        assert!(msg.contains("method not found"));
    }

    #[test]
    fn error_protocol_error_display() {
        let err = McpError::ProtocolError("unexpected field".into());
        assert!(err.to_string().contains("unexpected field"));
    }

    #[test]
    fn error_timeout_display() {
        assert_eq!(McpError::Timeout.to_string(), "tool call timed out");
    }

    #[test]
    fn error_server_shutdown_display() {
        assert_eq!(
            McpError::ServerShutdown.to_string(),
            "MCP server connection closed"
        );
    }

    #[test]
    fn error_transport_display() {
        let err = McpError::Transport("connection reset by peer".into());
        assert!(err.to_string().contains("connection reset by peer"));
    }

    #[test]
    fn error_cancelled_display() {
        assert_eq!(McpError::Cancelled.to_string(), "MCP request cancelled");
    }

    #[test]
    fn error_not_connected_display() {
        assert_eq!(
            McpError::NotConnected.to_string(),
            "MCP server dispatcher is not running"
        );
    }

    #[test]
    fn error_unsupported_transport_display() {
        let err = McpError::UnsupportedTransport("HTTP+SSE".into());
        assert!(err.to_string().contains("HTTP+SSE"));
    }

    #[test]
    fn error_auth_required_display_names_server_and_guidance() {
        let err = McpError::AuthRequired {
            server: "docs".into(),
            hint: "set a token".into(),
        };
        let msg = err.to_string();
        assert!(msg.contains("docs"), "{msg}");
        assert!(msg.contains("set a token"), "{msg}");
    }
}
