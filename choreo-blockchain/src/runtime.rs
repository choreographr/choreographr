//! Sidecar `tokio` runtime for the async blockchain clients (alloy/subxt).
//!
//! The daemon (and this crate's public `execute_*` entry points) are
//! synchronous, thread-based code. alloy and subxt, however, are async
//! libraries that require a tokio reactor, so this crate owns a single
//! process-wide runtime created once at startup. The blocking `execute_*`
//! functions in `evm`/`subxt` run their futures on it via [`Runtime::block_on`].
//!
//! [`init`] must be called exactly once before any tool executes (the daemon
//! does so from `main()` behind its `blockchain` feature). [`get`] returns
//! `None` before init or when the runtime failed to build, and callers map
//! that to a [`crate::BlockchainError::RuntimeNotInitialized`] rather than
//! panicking.

use std::sync::OnceLock;

static RUNTIME: OnceLock<tokio::runtime::Runtime> = OnceLock::new();

/// Create the multi-threaded tokio runtime (with IO + time drivers) and store
/// it in the process-wide sidecar. Idempotent: subsequent calls are no-ops.
///
/// Fails only if the OS refuses to spawn the worker threads — surfaced as an
/// error instead of panicking so the daemon can abort startup cleanly.
pub fn init() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    if RUNTIME.get().is_some() {
        return Ok(());
    }
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    // `set` only fails if a racing thread already initialized the runtime;
    // the early return above makes that unreachable, but handle it gracefully.
    RUNTIME
        .set(rt)
        .map_err(|_| std::io::Error::other("blockchain tokio runtime already initialized"))?;
    tracing::info!("blockchain tokio runtime initialized");
    Ok(())
}

/// Access the sidecar runtime, or `None` if [`init`] has not been called (or
/// failed). Callers must not panic on `None` — they surface a
/// `RuntimeNotInitialized` error instead.
pub fn get() -> Option<&'static tokio::runtime::Runtime> {
    RUNTIME.get()
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
}
