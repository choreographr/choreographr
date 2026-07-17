use std::io;
use thiserror::Error;

/// Error types for the ACP (Agent Communication Protocol) crate.
///
/// These are the fallible paths in the protocol: JSON-RPC exchange,
/// daemon connection, session management, transport I/O, and
/// serialization/deserialization of wire frames.
#[derive(Debug, Error)]
pub enum AcpError {
    /// A JSON-RPC error response from the peer.  Carries the standard
    /// code/message/data fields so callers can inspect the reason
    /// programmatically without re-parsing.
    #[error("JSON-RPC error: code={code} message={message}")]
    JsonRpc {
        code: i64,
        message: String,
        data: Option<serde_json::Value>,
    },

    /// Connecting to the daemon's Unix socket failed.
    #[error("daemon connection failed: {0}")]
    DaemonConnection(String),

    /// The requested ACP session does not exist on the daemon side.
    #[error("session not found: {0}")]
    SessionNotFound(String),

    /// The session is already processing a prompt — concurrent prompts
    /// are not allowed per the ACP spec.
    #[error("session is busy: {0}")]
    SessionBusy(String),

    /// The daemon socket was disconnected unexpectedly during I/O.
    #[error("transport disconnected")]
    TransportDisconnected,

    /// Low-level I/O errors (socket read/write, file operations).
    #[error(transparent)]
    Io(#[from] io::Error),

    /// Protocol-level frame errors from `tai-proto`.
    #[error(transparent)]
    Proto(#[from] tai_proto::ProtoError),

    /// JSON serialization / deserialization errors.
    #[error(transparent)]
    Serde(#[from] serde_json::Error),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_rpc_display() {
        let err = AcpError::JsonRpc {
            code: -32601,
            message: "Method not found".into(),
            data: None,
        };
        let msg = err.to_string();
        assert!(msg.contains("code=-32601"));
        assert!(msg.contains("message=Method not found"));
    }

    #[test]
    fn json_rpc_display_with_data() {
        let err = AcpError::JsonRpc {
            code: -32000,
            message: "bad request".into(),
            data: Some(serde_json::json!({"reason": "timeout"})),
        };
        let msg = err.to_string();
        assert!(msg.contains("code=-32000"));
        assert!(msg.contains("message=bad request"));
    }

    #[test]
    fn daemon_connection_display() {
        let err = AcpError::DaemonConnection("connection refused".into());
        assert_eq!(
            err.to_string(),
            "daemon connection failed: connection refused"
        );
    }

    #[test]
    fn session_not_found_display() {
        let err = AcpError::SessionNotFound("sess_abc".into());
        assert_eq!(err.to_string(), "session not found: sess_abc");
    }

    #[test]
    fn session_busy_display() {
        let err = AcpError::SessionBusy("sess_xyz".into());
        assert_eq!(err.to_string(), "session is busy: sess_xyz");
    }

    #[test]
    fn transport_disconnected_display() {
        let err = AcpError::TransportDisconnected;
        assert_eq!(err.to_string(), "transport disconnected");
    }

    #[test]
    fn from_io_error() {
        let io_err = io::Error::new(io::ErrorKind::NotFound, "file not found");
        let acp_err: AcpError = io_err.into();
        assert!(matches!(acp_err, AcpError::Io(_)));
        assert!(acp_err.to_string().contains("file not found"));
    }

    #[test]
    fn from_serde_error() {
        let serde_err = serde_json::from_str::<serde_json::Value>("invalid json").unwrap_err();
        let acp_err: AcpError = serde_err.into();
        assert!(matches!(acp_err, AcpError::Serde(_)));
    }

    #[test]
    fn debug_format() {
        let err = AcpError::SessionNotFound("test".into());
        let debug = format!("{err:?}");
        assert!(debug.contains("SessionNotFound"));
    }
}
