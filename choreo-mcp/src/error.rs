//! Error type shared across the MCP client, transport, and protocol layers.

use std::io;

/// Errors produced by the MCP client, transport, and protocol layers.
///
/// The daemon maps this into its own `ToolExecError` at the boundary; the
/// variants distinguish the failure stage (spawn, handshake, transport I/O,
/// protocol framing, or a server-reported JSON-RPC error).
#[derive(Debug, thiserror::Error)]
pub enum McpError {
    /// The MCP server subprocess could not be spawned or its stdio captured.
    #[error("failed to spawn subprocess: {0}")]
    SpawnFailed(String),

    /// The `initialize` handshake failed or returned a malformed response.
    #[error("MCP initialize handshake failed: {0}")]
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

    /// The server or its reader thread shut down unexpectedly.
    #[error("MCP server shut down unexpectedly")]
    ServerShutdown,

    /// The requested tool was not found on the server.
    #[error("tool not found: {0}")]
    ToolNotFound(String),

    /// The tool arguments were invalid.
    #[error("invalid params: {0}")]
    InvalidParams(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_spawn_failed_display() {
        let err = McpError::SpawnFailed("binary not found".into());
        let msg = err.to_string();
        assert!(msg.contains("binary not found"));
    }

    #[test]
    fn error_initialize_failed_display() {
        let err = McpError::InitializeFailed("version mismatch".into());
        let msg = err.to_string();
        assert!(msg.contains("version mismatch"));
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
        let msg = err.to_string();
        assert!(msg.contains("unexpected field"));
    }

    #[test]
    fn error_timeout_display() {
        let err = McpError::Timeout;
        assert_eq!(err.to_string(), "tool call timed out");
    }

    #[test]
    fn error_server_shutdown_display() {
        let err = McpError::ServerShutdown;
        assert_eq!(err.to_string(), "MCP server shut down unexpectedly");
    }

    #[test]
    fn error_tool_not_found_display() {
        let err = McpError::ToolNotFound("echo".into());
        let msg = err.to_string();
        assert!(msg.contains("echo"));
    }

    #[test]
    fn error_invalid_params_display() {
        let err = McpError::InvalidParams("missing name".into());
        let msg = err.to_string();
        assert!(msg.contains("missing name"));
    }
}
