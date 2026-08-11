//! Blockchain tools for Choreographr — EVM (alloy) and Substrate/Polkadot
//! (subxt) read-only queries, plus the sidecar `tokio` runtime they run on.
//!
//! The daemon (`choreo-daemon`) depends on this crate behind its `blockchain`
//! cargo feature (off by default) and registers thin `Tool` wrappers over the
//! synchronous `execute_*` entry points in [`evm`] and [`subxt`]. This crate is
//! deliberately the *only* workspace member that depends on `tokio`: the
//! daemon itself stays thread-only and blocks on the sidecar runtime here.
//!
//! # Trust model
//!
//! The `execute_*` entry points accept an arbitrary `rpc_url` / `ws_url` from
//! the model and open HTTP(S)/WebSocket connections to it. That is the same
//! trust surface as the daemon's `http_request` tool (any URL is reachable),
//! not a new capability — but these tools are gated behind the `blockchain`
//! feature (off by default) precisely because they extend the daemon's
//! network reach into chain-specific clients. Output is capped at
//! [`MAX_TOOL_OUTPUT_BYTES`] and every call is bounded by [`RPC_TIMEOUT`].

pub mod evm;
pub mod runtime;
pub mod subxt;

mod error;

pub use error::BlockchainError;

/// Shared byte budget for blockchain tool output, mirroring the daemon's
/// `MAX_TOOL_OUTPUT_BYTES` (128 KiB ≈ ~32K tokens for ASCII). A single query
/// (e.g. a full Substrate block dump) can never flood the conversation.
pub(crate) const MAX_TOOL_OUTPUT_BYTES: usize = 128 * 1024;

/// Wall-clock cap for one blockchain tool call's async work (client setup
/// plus every RPC request it issues). The daemon already bounds the whole
/// tool with a ~60s timeout, but that can only abandon the blocked execution
/// thread — a synchronous [`block_on`] cannot be interrupted — so a
/// black-holed RPC endpoint would otherwise leak a thread until the network
/// gave up. Capping the work here lets the tool return a clean error instead.
pub(crate) const RPC_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// Log the start of a blockchain tool call at debug level.
///
/// Every `execute_*` entry point calls this once with its tool name and RPC
/// target so this crate's modules emit `tracing` events themselves (the
/// daemon logs the tool call centrally, but the RPC-level detail lives here).
pub(crate) fn log_execution(tool: &'static str, rpc_target: &str) {
    tracing::debug!(tool, rpc_target = %rpc_target, "blockchain tool executing");
}

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
/// panicking on a missing runtime. Logs the wall-clock duration so every tool
/// call leaves an observability trail.
pub(crate) fn block_on<F>(fut: F) -> Result<F::Output, BlockchainError>
where
    F: std::future::Future,
{
    let rt = runtime::get().ok_or(BlockchainError::RuntimeNotInitialized)?;
    let start = std::time::Instant::now();
    let out = rt.block_on(fut);
    tracing::debug!(
        elapsed_ms = start.elapsed().as_millis(),
        "blockchain tool call completed"
    );
    Ok(out)
}

/// Run `fut` with a [`RPC_TIMEOUT`] wall-clock cap.
///
/// The wrapped future must resolve to a `Result` so a timeout can be reported
/// as a [`BlockchainError::Other`] in the same shape the tool impls return;
/// the caller then propagates it with `??` alongside a
/// [`BlockchainError::RuntimeNotInitialized`] from [`block_on`].
pub(crate) async fn rpc_call<F, T>(fut: F) -> Result<T, BlockchainError>
where
    F: std::future::Future<Output = Result<T, BlockchainError>>,
{
    rpc_call_with_timeout(fut, RPC_TIMEOUT).await
}

/// Core of [`rpc_call`] with an explicit timeout, so tests can exercise the
/// timeout path without waiting the production budget.
async fn rpc_call_with_timeout<F, T>(
    fut: F,
    timeout: std::time::Duration,
) -> Result<T, BlockchainError>
where
    F: std::future::Future<Output = Result<T, BlockchainError>>,
{
    tokio::time::timeout(timeout, fut).await.map_err(|_| {
        BlockchainError::Other(format!(
            "RPC request timed out after {} seconds",
            timeout.as_secs()
        ))
    })?
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

    #[test]
    fn rpc_call_passes_results_through() {
        runtime::init().expect("runtime init should succeed");
        // block_on returns Result<inner, RuntimeNotInitialized>; rpc_call's inner
        // future resolves to the tool's Result — so the two expects land on the
        // outer (runtime) and inner (tool) layers respectively.
        let ok = block_on(rpc_call(async {
            Ok::<_, BlockchainError>("hello".to_string())
        }))
        .expect("block_on must run")
        .expect("inner result must be ok");
        assert_eq!(ok, "hello");

        let err = block_on(rpc_call(async {
            Err::<u8, _>(BlockchainError::Other("boom".into()))
        }))
        .expect("block_on must run")
        .expect_err("inner result must be an error");
        assert!(matches!(err, BlockchainError::Other(msg) if msg == "boom"));
    }

    #[test]
    fn rpc_call_times_out_never_resolving_future() {
        runtime::init().expect("runtime init should succeed");
        // A future that never resolves must be cut off by the timeout and
        // surface the timeout error instead of hanging the test (the 10ms
        // budget is a deterministic deadline, not a sleep-based wait).
        let never = std::future::pending::<Result<u8, BlockchainError>>();
        let err = block_on(rpc_call_with_timeout(
            never,
            std::time::Duration::from_millis(10),
        ))
        .expect("block_on must run")
        .expect_err("the pending future must time out");
        assert!(err.to_string().contains("timed out after"));
    }
}
