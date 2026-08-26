//! Sidecar `tokio` runtime for the Coordination Platform's subxt client.
//!
//! The daemon and this crate's blocking `execute_*` entry points are
//! synchronous, thread-based code. Only `subxt` is async, so the crate owns a
//! single process-wide runtime created once at startup; the blocking
//! `execute_*` functions run their subxt futures on it via
//! [`Runtime::block_on`]. IPFS (`ureq`) and the indexer (`tungstenite`,
//! synchronous mode) do NOT use this runtime.
//!
//! [`init`] must be called exactly once before any tx/state tool executes
//! (the daemon does so from `main()`). [`get`] returns `None` before init or
//! when the runtime failed to build, and callers map that to a
//! [`crate::CoordError::RuntimeNotInitialized`] rather than panicking.

use std::sync::OnceLock;

static RUNTIME: OnceLock<tokio::runtime::Runtime> = OnceLock::new();

/// Create the multi-threaded tokio runtime (with IO + time drivers) and store
/// it in the process-wide sidecar. Idempotent: subsequent calls are no-ops,
/// including calls that race a concurrent initializer.
///
/// Fails only if the OS refuses to spawn the worker threads, surfaced as an
/// error instead of panicking so the daemon can abort startup cleanly.
pub fn init() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    if RUNTIME.get().is_some() {
        return Ok(());
    }
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    // A racing thread may have initialized the runtime between the check above
    // and this `set`; that is a success, not an error — the sidecar exists and
    // our built runtime is simply discarded.
    if RUNTIME.set(rt).is_ok() {
        tracing::info!("coordinator tokio runtime initialized");
    }
    Ok(())
}

/// Access the sidecar runtime, or `None` if [`init`] has not been called (or
/// failed). Callers must not panic on `None` — they surface a
/// [`crate::CoordError::RuntimeNotInitialized`] error instead.
pub fn get() -> Option<&'static tokio::runtime::Runtime> {
    RUNTIME.get()
}

/// Run `fut` to completion on the sidecar tokio runtime.
///
/// Returns [`crate::CoordError::RuntimeNotInitialized`] if [`init`] was never
/// called (or failed), so callers surface a clear error instead of panicking on
/// a missing runtime. Logs the wall-clock duration so every tool call leaves an
/// observability trail.
pub(crate) fn block_on<F>(fut: F) -> Result<F::Output, crate::CoordError>
where
    F: std::future::Future,
{
    let rt = get().ok_or(crate::CoordError::RuntimeNotInitialized)?;
    let start = std::time::Instant::now();
    let out = rt.block_on(fut);
    tracing::debug!(
        elapsed_ms = start.elapsed().as_millis(),
        "coordinator tool call completed"
    );
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn init_then_get_returns_some() {
        // Idempotent — safe to call even if another test initialized it.
        init().expect("runtime init should succeed");
        assert!(
            get().is_some(),
            "get() must return the runtime after init()"
        );
    }

    #[test]
    fn block_on_runs_on_sidecar() {
        init().expect("runtime init should succeed");
        let out = block_on(async { 42u8 }).expect("block_on must run on an initialized runtime");
        assert_eq!(out, 42);
    }
}
