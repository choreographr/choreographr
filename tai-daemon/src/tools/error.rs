use super::ToolOutput;
use serde::{Deserialize, Serialize};

/// A simple string-message error for tools that don't need structured errors.
/// Set `type Error = ToolExecError` and return `Err(ToolExecError("msg".into()))`.
#[derive(Debug, Serialize, Deserialize, thiserror::Error)]
#[error("{0}")]
pub struct ToolExecError(pub String);

/// Infrastructure errors — things that happen around tool execution.
/// Never produced directly by a tool's `execute()`.
#[derive(Debug, Serialize, Deserialize, thiserror::Error)]
pub enum ToolError {
    #[error("invalid arguments: {0}")]
    InvalidArguments(String),
    #[error("I/O error: {0}")]
    Io(String),
    #[error("postcard error: {0}")]
    Postcard(String),
    #[error("{0}")]
    Other(String),
}

impl From<serde_json::Error> for ToolError {
    fn from(e: serde_json::Error) -> Self {
        Self::InvalidArguments(e.to_string())
    }
}

impl From<postcard::Error> for ToolError {
    fn from(e: postcard::Error) -> Self {
        Self::Postcard(e.to_string())
    }
}

impl From<std::io::Error> for ToolError {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e.to_string())
    }
}

impl From<std::io::Error> for ToolExecError {
    fn from(e: std::io::Error) -> Self {
        ToolExecError(e.to_string())
    }
}

impl From<ToolError> for ToolExecError {
    fn from(e: ToolError) -> Self {
        ToolExecError(e.to_string())
    }
}

impl From<ToolExecError> for ToolError {
    fn from(e: ToolExecError) -> Self {
        ToolError::Other(e.0)
    }
}

pub(crate) fn tool_ok(content: String) -> ToolOutput {
    ToolOutput {
        content,
        is_error: false,
    }
}

pub(crate) fn tool_err(error: impl ToString) -> ToolOutput {
    ToolOutput {
        content: error.to_string(),
        is_error: true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_exec_error_from_string() {
        let err = ToolExecError("hello".into());
        assert_eq!(err.to_string(), "hello");
    }

    #[test]
    fn tool_exec_error_from_io_error() {
        let io = std::io::Error::new(std::io::ErrorKind::NotFound, "file not found");
        let err: ToolExecError = io.into();
        assert!(err.to_string().contains("file not found"));
    }

    #[test]
    fn tool_error_from_serde_json_error() {
        let serde_err = serde_json::from_str::<String>("invalid").unwrap_err();
        let err: ToolError = serde_err.into();
        assert!(matches!(err, ToolError::InvalidArguments(_)));
    }

    #[test]
    fn tool_error_from_postcard_error() {
        let postcard_err = postcard::from_bytes::<u8>(&[]).unwrap_err();
        let err: ToolError = postcard_err.into();
        assert!(matches!(err, ToolError::Postcard(_)));
    }

    #[test]
    fn tool_error_to_tool_exec_error_round_trip() {
        let tool_err = ToolError::Other("something went wrong".into());
        let exec_err: ToolExecError = tool_err.into();
        assert!(exec_err.to_string().contains("something went wrong"));

        let back: ToolError = exec_err.into();
        assert!(matches!(back, ToolError::Other(_)));
    }

    #[test]
    fn tool_ok_returns_success_output() {
        let output = tool_ok("success".into());
        assert!(!output.is_error);
        assert_eq!(output.content, "success");
    }

    #[test]
    fn tool_err_returns_error_output() {
        let output = tool_err("failure");
        assert!(output.is_error);
        assert_eq!(output.content, "failure");
    }
}
