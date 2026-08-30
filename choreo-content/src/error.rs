//! Structured error type for the Coordination Platform tools.
//!
//! Every `execute_*` entry point returns `Result<T, ContentError>`. The daemon
//! converts this into a tool execution error via
//! `impl From<ContentError> for ToolExecError`. Production code never panics —
//! all failures (subxt/interop, IPFS, indexer, protobuf decode, item-id
//! derivation, account/keystore, config) are surfaced here.

use thiserror::Error;

/// Errors produced by the Choreographr Coordination Platform tools.
#[derive(Debug, Error)]
pub enum ContentError {
    /// The sidecar tokio runtime was never initialized (or failed to start).
    #[error("coordinator runtime is not initialized")]
    RuntimeNotInitialized,

    /// Substrate/subxt transport or interop failure.
    #[error("substrate error: {0}")]
    Substrate(String),

    /// The transaction was rejected or could not be submitted.
    #[error("transaction failed: {0}")]
    Transaction(String),

    /// The account used to sign is missing from the keystore credential store.
    #[error("unknown polkadot account: {0}")]
    UnknownAccount(String),

    /// An IPFS operation failed.
    #[error("ipfs error: {0}")]
    Ipfs(String),

    /// An image could not be read, decoded, or JPEG-encoded.
    #[error("image error: {0}")]
    Image(String),

    /// The indexer WebSocket API request failed.
    #[error("indexer error: {0}")]
    Indexer(String),

    /// A content payload could not be encoded/decoded (protobuf) or a field
    /// was missing/ill-formed.
    #[error("content error: {0}")]
    Content(String),

    /// An item ID could not be derived/decoded, or an argument was invalid.
    #[error("invalid argument: {0}")]
    InvalidArgument(String),

    /// A CID or sha2-256 digest could not be converted/parsed.
    #[error("cid error: {0}")]
    Cid(String),

    /// The keystore/account operation failed.
    #[error("account error: {0}")]
    Account(String),

    /// Any other failure (bounded, informational).
    #[error("content error: {0}")]
    Other(String),
}

impl ContentError {
    /// Convenience constructor for a plain string error.
    pub fn other(msg: impl Into<String>) -> Self {
        ContentError::Other(msg.into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn messages_render() {
        assert_eq!(
            ContentError::RuntimeNotInitialized.to_string(),
            "coordinator runtime is not initialized"
        );
        assert!(
            ContentError::Substrate("boom".into())
                .to_string()
                .contains("boom")
        );
        assert!(
            ContentError::Account("missing".into())
                .to_string()
                .contains("missing")
        );
    }

    #[test]
    fn other_constructor() {
        assert_eq!(ContentError::other("x").to_string(), "content error: x");
    }
}
