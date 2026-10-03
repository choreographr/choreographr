//! Sidecar `tokio` runtime for the MCP client.
//!
//! The daemon (and this crate's blocking `McpServer::*` methods) is
//! synchronous, thread-based code. Only `rmcp` is async, so the crate owns a
//! single process-wide multi-thread runtime, created once at startup; the
//! blocking entry points run their client futures on it via [`block_on`], and
//! each per-server dispatcher thread spawns its in-flight call tasks onto it
//! through [`handle`].
//!
//! A multi-thread runtime (not current-thread) is required: a dispatcher
//! thread blocks on its command channel while the call tasks and the `rmcp`
//! service loop must keep making progress on the runtime's worker threads.
//!
//! [`init`] must be called before any server connects. [`get`] returns `None`
//! before init or when the runtime failed to build, and callers map that to an
//! error rather than panicking.

use crate::error::McpError;
use std::sync::OnceLock;

static RUNTIME: OnceLock<tokio::runtime::Runtime> = OnceLock::new();

/// Create the multi-threaded tokio runtime (with IO + time drivers) and store
/// it in the process-wide sidecar. Idempotent: subsequent calls are no-ops,
/// including calls that race a concurrent initializer.
///
/// Fails only if the OS refuses to spawn the worker threads, surfaced as a
/// [`McpError`] instead of panicking so the daemon can skip MCP cleanly.
///
/// # Errors
///
/// Returns [`McpError::ProtocolError`] if `tokio::runtime::Builder` cannot
/// build the multi-threaded runtime (e.g. worker-thread spawn failure). Never
/// fails once the runtime is already initialized.
pub fn init() -> Result<(), McpError> {
    if RUNTIME.get().is_some() {
        return Ok(());
    }
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|e| McpError::ProtocolError(format!("failed to build MCP runtime: {e}")))?;
    // A racing thread may have initialized the runtime between the check above
    // and this `set`; that is a success, not an error — the sidecar exists and
    // our built runtime is simply discarded.
    if RUNTIME.set(rt).is_ok() {
        tracing::info!("MCP tokio runtime initialized");
    }
    Ok(())
}

/// Access the sidecar runtime, or `None` if [`init`] has not been called (or
/// failed). Callers must not panic on `None`; they surface
/// [`McpError::ProtocolError`] instead.
#[must_use]
pub fn get() -> Option<&'static tokio::runtime::Runtime> {
    RUNTIME.get()
}

/// A [`tokio::runtime::Handle`] for the sidecar runtime, or [`McpError`] if it
/// is not initialized.
///
/// The dispatcher thread uses this to `spawn` in-flight call tasks: `Handle` is
/// `Clone + Send + Sync` and `spawn` may be called from any thread, so a
/// blocking dispatcher can hand work to the runtime's worker threads without
/// itself entering an async context.
///
/// # Errors
///
/// Returns [`McpError::ProtocolError`] when [`init`] has not run.
pub fn handle() -> Result<tokio::runtime::Handle, McpError> {
    get()
        .map(|rt| rt.handle().clone())
        .ok_or_else(|| McpError::ProtocolError("MCP runtime not initialized".into()))
}

/// Run `fut` to completion on the sidecar tokio runtime.
///
/// Returns [`McpError::ProtocolError`] if [`init`] was never called (or
/// failed), so callers surface a clear error instead of panicking on a missing
/// runtime.
///
/// This must only be called from a thread that is NOT itself a runtime worker
/// (the daemon and each dispatcher thread qualify); calling it from inside an
/// async task would panic in tokio.
///
/// # Errors
///
/// Returns [`McpError::ProtocolError`] when [`init`] has not run.
pub fn block_on<F>(fut: F) -> Result<F::Output, McpError>
where
    F: std::future::Future,
{
    let rt = get().ok_or_else(|| McpError::ProtocolError("MCP runtime not initialized".into()))?;
    Ok(rt.block_on(fut))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn init_then_get_returns_some() {
        // Idempotent — safe even if another test initialized it first.
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

    #[test]
    fn handle_returns_a_spawner() {
        init().expect("runtime init should succeed");
        let handle = handle().expect("handle must be available after init");
        // Spawn a trivial task and confirm the handle is usable; the task's
        // absence of shared state keeps this free of cross-thread coupling.
        handle.spawn(async {});
    }
}
