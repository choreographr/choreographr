//! Blockchain tools for Choreographr — EVM (alloy) and Substrate/Polkadot
//! (subxt) read-only queries, plus the sidecar `tokio` runtime they run on.
//!
//! The daemon (`choreo-daemon`) depends on this crate behind its `blockchain`
//! cargo feature (off by default) and registers thin `Tool` wrappers over the
//! synchronous `execute_*` entry points in [`evm`] and [`subxt`]. This crate is
//! deliberately the *only* workspace member that depends on `tokio`: the
//! daemon itself stays thread-only and blocks on the sidecar runtime here.

pub mod evm;
pub mod runtime;
pub mod subxt;

mod error;

pub use error::BlockchainError;

/// Shared byte budget for blockchain tool output, mirroring the daemon's
/// `MAX_TOOL_OUTPUT_BYTES` (128 KiB ≈ ~32K tokens for ASCII). A single query
/// (e.g. a full Substrate block dump) can never flood the conversation.
pub(crate) const MAX_TOOL_OUTPUT_BYTES: usize = 128 * 1024;

/// Cap `content` at [`MAX_TOOL_OUTPUT_BYTES`], cutting on a char boundary so a
/// multi-byte UTF-8 char is never split, and append a truncation marker.
pub(crate) fn truncate_tool_output(content: &str) -> String {
    if content.len() <= MAX_TOOL_OUTPUT_BYTES {
        return content.to_string();
    }
    let split = content.floor_char_boundary(MAX_TOOL_OUTPUT_BYTES);
    let mut truncated = content[..split].to_string();
    truncated.push_str("\n...[truncated]");
    truncated
}

/// Run `fut` to completion on the sidecar tokio runtime.
///
/// Returns [`BlockchainError::RuntimeNotInitialized`] if [`runtime::init`] was
/// never called (or failed), so callers surface a clear error instead of
/// panicking on a missing runtime.
pub(crate) fn block_on<F>(fut: F) -> Result<F::Output, BlockchainError>
where
    F: std::future::Future,
{
    let rt = runtime::get().ok_or(BlockchainError::RuntimeNotInitialized)?;
    Ok(rt.block_on(fut))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncate_keeps_short_content_untouched() {
        assert_eq!(truncate_tool_output("hello"), "hello");
        assert_eq!(truncate_tool_output(""), "");
    }

    #[test]
    fn truncate_caps_long_content() {
        let big = "x".repeat(MAX_TOOL_OUTPUT_BYTES + 100);
        let out = truncate_tool_output(&big);
        assert!(out.ends_with("...[truncated]"));
        // body (capped at the budget) + "\n...[truncated]" marker
        assert_eq!(out.len(), MAX_TOOL_OUTPUT_BYTES + "\n...[truncated]".len());
    }

    #[test]
    fn truncate_never_splits_utf8() {
        // 3-byte chars where the cap lands mid-char must stay valid UTF-8.
        let big = "€".repeat((MAX_TOOL_OUTPUT_BYTES / 3) + 10);
        let out = truncate_tool_output(&big);
        assert!(out.ends_with("...[truncated]"));
        std::str::from_utf8(out.as_bytes()).expect("truncated output must be valid UTF-8");
    }

    #[test]
    fn block_on_without_runtime_is_an_error() {
        // This test must not depend on whether another test initialized the
        // runtime; get() may be Some, in which case block_on succeeds. Only
        // assert the *shape*: a missing runtime maps to RuntimeNotInitialized.
        match runtime::get() {
            None => {
                let fut = async { 42u8 };
                let err = block_on(fut).unwrap_err();
                assert!(matches!(err, BlockchainError::RuntimeNotInitialized));
            }
            Some(_) => {
                let fut = async { 42u8 };
                assert_eq!(block_on(fut).unwrap(), 42);
            }
        }
    }
}
