use std::io;

#[derive(Debug, thiserror::Error)]
pub enum McpError {
    #[error("failed to spawn subprocess: {0}")]
    SpawnFailed(String),

    #[error("MCP initialize handshake failed: {0}")]
    InitializeFailed(String),

    #[error("JSON-RPC error: code={code} message={message}")]
    JsonRpcError { code: i64, message: String },

    #[error("protocol error: {0}")]
    ProtocolError(String),

    #[error("tool call timed out")]
    Timeout,

    #[error("I/O error: {0}")]
    Io(#[from] io::Error),

    #[error("MCP server shut down unexpectedly")]
    ServerShutdown,

    #[error("tool not found: {0}")]
    ToolNotFound(String),

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
