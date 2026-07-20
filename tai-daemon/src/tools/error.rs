use super::ToolOutput;
use thiserror::Error;

#[derive(Error, Debug)]
pub enum ToolError {
    #[error("invalid arguments: {0}")]
    InvalidArguments(#[from] serde_json::Error),
    #[error("unsupported method: {0}")]
    UnsupportedMethod(String),
    #[error("invalid url: {0}")]
    InvalidUrl(String),
    #[error("unsupported URL scheme: {0}")]
    UnsupportedUrlScheme(String),
    #[error("invalid header {name}: {error}")]
    InvalidHeader { name: String, error: String },
    #[error("request failed: {0}")]
    RequestFailed(String),
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("postcard error: {0}")]
    Postcard(#[from] postcard::Error),
    #[error("{0}")]
    Other(String),
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

impl From<ToolError> for ToolOutput {
    fn from(error: ToolError) -> Self {
        ToolOutput {
            content: error.to_string(),
            is_error: true,
        }
    }
}
