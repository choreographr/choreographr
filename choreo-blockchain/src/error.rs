//! Error type shared by the blockchain tool entry points.

/// Errors produced by the blockchain tool implementations.
///
/// The daemon's thin `Tool` wrappers map this to their own `ToolExecError` via
/// `From<BlockchainError> for ToolExecError`; the daemon never sees the
/// internals (which client failed, at what layer).
#[derive(Debug, thiserror::Error)]
pub enum BlockchainError {
    /// The RPC URL failed to parse as a URL.
    #[error("invalid RPC URL: {0}")]
    InvalidUrl(String),
    /// The sidecar tokio runtime was not initialized (feature disabled, or
    /// `runtime::init()` not called at startup).
    #[error("blockchain runtime not initialized")]
    RuntimeNotInitialized,
    /// alloy (EVM) client error.
    #[error("alloy error: {0}")]
    Alloy(String),
    /// subxt (Substrate) client error.
    #[error("subxt error: {0}")]
    Subxt(String),
    /// Any other failure (bad address, missing block, invalid hex, …).
    #[error("{0}")]
    Other(String),
}

impl From<serde_json::Error> for BlockchainError {
    fn from(e: serde_json::Error) -> Self {
        BlockchainError::Other(format!("invalid arguments: {e}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn errors_are_displayable() {
        assert!(
            BlockchainError::RuntimeNotInitialized
                .to_string()
                .contains("runtime not initialized")
        );
        assert!(BlockchainError::Alloy("boom".into()).to_string() == "alloy error: boom");
        assert!(BlockchainError::Subxt("boom".into()).to_string() == "subxt error: boom");
        assert!(BlockchainError::InvalidUrl("nope".into()).to_string() == "invalid RPC URL: nope");
    }

    #[test]
    fn serde_json_error_converts() {
        let err = serde_json::from_str::<u8>("not a number").unwrap_err();
        let converted: BlockchainError = err.into();
        assert!(converted.to_string().contains("invalid arguments"));
    }
}
