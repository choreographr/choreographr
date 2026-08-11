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
//! network reach into chain-specific clients. Every node-supplied string is
//! sanitized before it enters the tool transcript: scalar strings (chain
//! names, ENS records) via [`sanitize_value`], serde-rendered JSON (decoded
//! storage/block values) via [`sanitize_json`]. Output is capped at
//! [`MAX_TOOL_OUTPUT_BYTES`], and every call is bounded by [`RPC_TIMEOUT`].

pub mod evm;
pub mod runtime;
pub mod subxt;

mod error;

pub use error::BlockchainError;

/// Shared byte budget for blockchain tool output — owned by `choreo-sanitize`
/// (see `MAX_TOOL_OUTPUT_BYTES` there) and re-exported here so the evm/subxt
/// modules keep importing it from `crate`. A single query (e.g. a full
/// Substrate block dump) can never flood the conversation.
pub(crate) use choreo_sanitize::MAX_TOOL_OUTPUT_BYTES;
/// Byte-cap + truncation marker for blockchain tool output — shared from
/// `choreo-sanitize` (the leaf crate that owns the output byte budget), so
/// the daemon's tools, the blockchain tools, and the client all cap and mark
/// truncation identically.
pub(crate) use choreo_sanitize::truncate_tool_output;

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

/// Strip an optional `0x` / `0X` prefix from a hex string, returning the input
/// unchanged when there is no prefix.
///
/// Every hex parser in this crate routes through this helper so they all
/// tolerate both prefix spellings consistently (the model will sometimes send
/// uppercase `0X`).
pub(crate) fn strip_hex_prefix(s: &str) -> &str {
    s.strip_prefix("0x")
        .or_else(|| s.strip_prefix("0X"))
        .unwrap_or(s)
}

/// Escape control characters and Unicode line/paragraph separators in a
/// node-supplied value so it cannot corrupt the line-oriented tool output or
/// inject terminal escape sequences.
///
/// The RPC endpoints these tools talk to are arbitrary and untrusted (see the
/// crate-level trust model), so every string that originates off-process —
/// chain names, `client_version`, ENS records, decoded storage/block JSON —
/// is passed through this before being interpolated into a `key: value` line.
/// This mirrors the daemon's `sanitize_name` policy (see `sanitize_text` in
/// choreo-daemon's `tools/mod.rs`):
///
/// - C0/C1 control characters (`char::is_control`) are escaped via
///   `escape_default` (`\n`, `\t`, `\u{1b}`, …).
/// - U+2028 LINE SEPARATOR / U+2029 PARAGRAPH SEPARATOR are **not**
///   `is_control` (categories Zl/Zp), yet terminals render them as line
///   breaks — they must be escaped to preserve the one-line-per-value
///   invariant.
/// - Unicode format characters (category Cf) are invisible but can reorder,
///   hide, or spoof rendered text: bidi marks/overrides/isolates, zero-width
///   space and word joiner, invisible operators, the BOM, and more. The
///   joiners U+200C/U+200D (ZWNJ/ZWJ) do not reorder or hide text and are
///   legitimate in some scripts, so they pass through.
pub(crate) fn sanitize_value(text: &str) -> String {
    // Fast path: ASCII printables need no escaping. Multi-byte UTF-8 bytes
    // are all >= 0x80, so any non-ASCII text falls through to the slow path
    // (it may hide a separator or bidi char).
    if text.bytes().all(|b| (0x20..=0x7e).contains(&b)) {
        return text.to_string();
    }
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        if sanitize_keeps(c) {
            out.push(c);
        } else {
            // escape_default renders the special escapes (`\t`, `\r`, `\n`,
            // …) and everything else control-related as `\u{...}` — all inert
            // ASCII text, so nothing terminal-affecting or line-splitting leaks.
            out.extend(c.escape_default());
        }
    }
    out
}

/// Sanitize serde-produced JSON text for the tool transcript, preserving its
/// structural line breaks.
///
/// [`sanitize_value`] escapes newlines (they are C0 controls), which would
/// flatten a pretty-printed JSON blob into one long line of literal `\n`
/// sequences. That is the right policy for *scalar* node strings — a hostile
/// chain name must not be able to inject a line — but JSON is different:
/// serde_json never emits an unescaped control character (a hostile value
/// inside a JSON string renders as the two-character `\n` / `\u00xx` escape),
/// so the *only* literal newlines in JSON output are the structural separators
/// serde itself emitted. This function splits on those, sanitizes each line
/// via [`sanitize_value`] — catching the Cf format chars (bidi, ZWSP, …) that
/// serde does NOT escape — and re-joins with real newlines.
///
/// Only the first `budget` bytes are processed (cut on a char boundary), so a
/// pathological node response (e.g. a multi-megabyte full-block JSON) cannot
/// force a full-size sanitize copy before the caller's final
/// [`truncate_tool_output`] applies the authoritative cap and marker.
pub(crate) fn sanitize_json(text: &str, budget: usize) -> String {
    // Clamp before floor_char_boundary: it panics on indices past the string.
    let end = text.floor_char_boundary(budget.min(text.len()));
    text[..end]
        .lines()
        .map(sanitize_value)
        .collect::<Vec<_>>()
        .join("\n")
}

/// Whether `c` passes through [`sanitize_value`] unchanged. ASCII can never be
/// a Unicode line/paragraph separator or format char, so the general-category
/// lookup only runs for non-ASCII input.
fn sanitize_keeps(c: char) -> bool {
    if c.is_ascii() {
        return (' '..='~').contains(&c);
    }
    !c.is_control() && !is_unsafe_unicode(c)
}

/// The non-control Unicode that must still be escaped — line / paragraph
/// separators plus the non-joiner format-char spoofing class — is the shared
/// `choreo_sanitize::is_unsafe_unicode` predicate (the leaf crate that owns
/// the Unicode-safety policy, used by the daemon and the TUI too). The
/// code-space sweep guarding it lives next to it.
use choreo_sanitize::is_unsafe_unicode;

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
        // Sub-second budgets (tests) report millis; the production 30s budget
        // reports seconds — the number stays honest at both scales.
        let rendered = if timeout.as_secs() >= 1 {
            format!("{} seconds", timeout.as_secs())
        } else {
            format!("{} ms", timeout.as_millis())
        };
        BlockchainError::Other(format!("RPC request timed out after {rendered}"))
    })?
}

#[cfg(test)]
mod tests {
    use super::*;

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
        // The sub-second budget is reported in ms, not "0 seconds".
        assert!(err.to_string().contains("10 ms"), "{err}");
    }

    #[test]
    fn strip_hex_prefix_handles_both_spellings() {
        // The shared helper must accept raw hex, lowercase `0x`, and uppercase
        // `0X` — every parser in the crate routes through it, so the behavior
        // they all rely on is pinned here rather than in a re-implemented copy.
        assert_eq!(strip_hex_prefix("deadbeef"), "deadbeef");
        assert_eq!(strip_hex_prefix("0xdeadbeef"), "deadbeef");
        assert_eq!(strip_hex_prefix("0XDEADBEEF"), "DEADBEEF");
        assert_eq!(strip_hex_prefix(""), "");
        assert_eq!(strip_hex_prefix("0x"), "");
    }

    #[test]
    fn sanitize_value_escapes_control_and_separator_chars() {
        // C0/C1 controls, the Zl/Zp separators, and Cf format chars must be
        // escaped so a hostile node value cannot split or spoof output lines.
        assert_eq!(sanitize_value("plain value"), "plain value");
        assert_eq!(sanitize_value("line\nbreak"), "line\\nbreak");
        assert_eq!(sanitize_value("tab\there"), "tab\\there");
        assert_eq!(sanitize_value("esc\u{1b}[31m"), "esc\\u{1b}[31m");
        assert_eq!(sanitize_value("sep\u{2028}arator"), "sep\\u{2028}arator");
        assert_eq!(sanitize_value("bidi\u{202e}evil"), "bidi\\u{202e}evil");
        // Joiners are legitimate in some scripts and pass through.
        assert_eq!(sanitize_value("a\u{200c}b"), "a\u{200c}b");
        // Non-ASCII but safe text passes through untouched.
        assert_eq!(sanitize_value("café"), "café");
    }

    #[test]
    fn sanitize_json_preserves_structural_newlines_and_escapes_chars() {
        // serde's pretty JSON emits literal \n only as structural separators
        // (control chars inside string values are escaped by serde), so the
        // sanitizer can keep the multi-line structure while still escaping the
        // Cf format chars (bidi, ZWSP, …) that serde does NOT escape.
        let pretty = "{\n  \"k\": \"v\u{202e}\"\n}";
        assert_eq!(
            sanitize_json(pretty, MAX_TOOL_OUTPUT_BYTES),
            "{\n  \"k\": \"v\\u{202e}\"\n}"
        );
        // Compact JSON stays single-line.
        let compact = "{\"k\":\"v\u{202e}\"}";
        assert_eq!(
            sanitize_json(compact, MAX_TOOL_OUTPUT_BYTES),
            "{\"k\":\"v\\u{202e}\"}"
        );
    }

    #[test]
    fn sanitize_json_caps_before_sanitizing() {
        // The sanitize pass must be bounded: content beyond `budget` bytes is
        // dropped before any escaping runs, so a huge node response cannot
        // force a full-size copy — and a hostile char past the cap cannot
        // slip into the output.
        let big = "x".repeat(MAX_TOOL_OUTPUT_BYTES * 4) + "\u{1b}";
        let out = sanitize_json(&big, MAX_TOOL_OUTPUT_BYTES);
        assert_eq!(out.len(), MAX_TOOL_OUTPUT_BYTES);
        assert!(
            !out.contains('\u{1b}'),
            "ESC beyond the cap must be dropped"
        );
    }
}
