//! iOS-native tools (`clipboard_write`, `clipboard_read`, `open_url`,
//! `notify`) — thin [`Tool`] wrappers over the bridge seam in
//! [`crate::tools::ios_bridge`].
//!
//! Compiled UNCONDITIONALLY on every target (the `powershell` precedent):
//! this module contains no Apple API at all, only bridge calls, so it
//! compiles and its unit tests run on every dev box. The module is inert on
//! non-iOS builds because nothing registers it there — registration happens
//! in [`crate::tools::ToolRegistry::register_platform_tools`], called from
//! `DaemonState::open` only when the embedder supplies a
//! `platform_tool_bridge`. The bridge's presence is the gate, not a `cfg`.
//!
//! # Cancellation semantics (shared by every tool here)
//!
//! 1. The cancelled flag is checked at ENTRY, before dispatch — a cancelled
//!    call must never touch the bridge (pinned by tests via
//!    `MockBridge::dispatched()` being empty).
//! 2. [`IosToolPending::wait`] polls the same flag while waiting for the
//!    reply and implements cancel-wins-on-reply.
//! 3. On the cancel path the tool calls `pending.cancel()` best-effort, so
//!    the Swift host can drop a still-queued request nobody will read.
//!
//! # Timeouts
//!
//! Each tool struct takes its deadline as a constructor parameter so tests
//! inject `Duration::ZERO` (the deterministic timeout path — no sleeps per
//! the repo's test discipline). Production constructors use the named
//! constants from `ios_bridge`.

pub(crate) mod clipboard;
pub(crate) mod notify;
pub(crate) mod open_url;

use crate::tools::ToolExecError;
use crate::tools::context::ToolContext;
use crate::tools::ios_bridge::{IosToolBridge, IosToolRequest, ToolBridgeError};
use std::sync::Arc;
use std::time::Duration;

/// The tool group all four iOS tools live in. Registered (and protected from
/// unload) by [`crate::tools::ToolRegistry::register_platform_tools`].
pub(crate) const IOS_GROUP: &str = "ios";

/// Shared dispatch-and-wait path for every iOS tool.
///
/// Entry-checks the cancelled flag (before touching the bridge), dispatches,
/// and blocks on the reply with the caller-supplied `timeout` and the
/// context's cancellation flag. Maps [`ToolBridgeError`] onto the
/// stringly-typed [`ToolExecError`] these simple tools use:
/// `Canceled` → cancellation message, `Timeout` → its own message,
/// `BridgeUnavailable` / `Platform` → error string.
pub(crate) fn run_bridge_tool<T: serde::Serialize>(
    bridge: &Arc<dyn IosToolBridge>,
    tool_name: &'static str,
    args: &T,
    timeout: Duration,
    ctx: Option<&ToolContext>,
) -> Result<serde_json::Value, ToolExecError> {
    // Cancel-at-entry: a cancelled call must not even reach the bridge.
    // The mock-based tests assert this via `MockBridge::dispatched()` being
    // empty after a cancelled execute.
    if let Some(ctx) = ctx
        && ctx.cancelled.load(std::sync::atomic::Ordering::Relaxed)
    {
        tracing::info!(
            tool = tool_name,
            session_id = ctx.session_id,
            "ios tool canceled before dispatch"
        );
        return Err(ToolExecError("request was canceled".into()));
    }

    // The SINGLE serialization point: the tools hand us their typed args
    // (or a `serde_json::Value` for the no-arg tools) and we serialize once
    // here — no per-tool pre-encode + re-encode dance.
    let args_json = serde_json::to_string(args).map_err(|e| {
        // Args are plain serde data; failure here is a bug, but it must not
        // panic (house rule) — surface it as a tool error.
        ToolExecError(format!("failed to encode ios tool arguments: {e}"))
    })?;

    tracing::debug!(
        tool = tool_name,
        timeout_ms = timeout.as_millis() as u64,
        "dispatching ios tool"
    );
    let pending = bridge
        .dispatch(IosToolRequest {
            // Reassigned by the bridge; zero here is only the pre-assign value.
            request_id: 0,
            name: tool_name.to_string(),
            args_json,
        })
        .map_err(|e| bridge_err_to_tool_err(tool_name, e))?;
    let request_id = pending.request_id;

    // The cancellation predicate reads the context's cooperative flag — the
    // sanctioned shared-state exception. With no context (tests) nothing
    // cancels.
    let cancelled_flag = ctx.map(|c| Arc::clone(&c.cancelled));
    let is_canceled = move || {
        cancelled_flag
            .as_ref()
            .is_some_and(|f| f.load(std::sync::atomic::Ordering::Relaxed))
    };

    let reply = pending.wait(timeout, &is_canceled);
    match reply {
        Ok(value) => {
            tracing::debug!(tool = tool_name, request_id, "ios tool reply received");
            Ok(value)
        }
        Err(e) => {
            if matches!(e, ToolBridgeError::Canceled) {
                // Best-effort Swift-side cancel on the late-cancel path: the
                // flag won, so nobody will read the reply — tell the host to
                // drop any still-queued work. `cancel` consumes the handle;
                // dropping it afterwards disconnects the reply channel, which
                // is the documented abandonment path.
                pending.cancel();
                tracing::info!(tool = tool_name, request_id, "ios tool canceled");
            } else {
                tracing::warn!(tool = tool_name, request_id, error = %e, "ios tool failed");
            }
            Err(bridge_err_to_tool_err(tool_name, e))
        }
    }
}

/// Map a [`ToolBridgeError`] onto the tools' error type. Kept separate from
/// [`run_bridge_tool`] so dispatch failures and wait failures share one
/// mapping (they can produce overlapping variants).
fn bridge_err_to_tool_err(tool_name: &'static str, e: ToolBridgeError) -> ToolExecError {
    match e {
        ToolBridgeError::Canceled => ToolExecError("request was canceled".into()),
        ToolBridgeError::Timeout => {
            ToolExecError(format!("{tool_name} timed out waiting for the iOS host"))
        }
        ToolBridgeError::BridgeUnavailable => ToolExecError(format!(
            "{tool_name} could not reach the iOS host (bridge unavailable)"
        )),
        ToolBridgeError::Platform(msg) => ToolExecError(msg),
    }
}

#[cfg(test)]
pub(crate) mod test_util {
    //! Deterministic test scaffolding shared by the per-tool test modules.

    use super::*;
    use crate::tools::ios_bridge::{MockBridge, MockResponse};

    /// A bridge wrapper that flips the context's cancellation flag DURING
    /// dispatch — i.e. after the tool's entry check but before `wait` runs.
    /// This pins the cancel-wins-on-reply race with no threads and no
    /// sleeps: the mock's reply is already buffered when dispatch returns,
    /// yet `wait` must still surface `Canceled` and the tool must call
    /// `pending.cancel()` (observable on the inner mock's `cancels()`).
    pub(crate) struct CancelDuringDispatchBridge {
        pub inner: Arc<MockBridge>,
        pub flag: Arc<std::sync::atomic::AtomicBool>,
    }

    impl CancelDuringDispatchBridge {
        pub(crate) fn dispatched(&self) -> Vec<crate::tools::ios_bridge::IosToolRequest> {
            self.inner.dispatched()
        }

        pub(crate) fn cancels(&self) -> Vec<u64> {
            self.inner.cancels()
        }
    }

    /// A test `ToolContext` whose DB backing file is DELETED immediately
    /// after the redb handle is created: the open handle stays valid (Unix
    /// unlink semantics; on Windows the remove may fail and is ignored —
    /// the TempDir still cleans up on drop), and the TempDir is dropped
    /// normally, so tests leave no accumulating files behind.
    pub(crate) fn test_ctx() -> ToolContext {
        let (tx, _rx) = std::sync::mpsc::channel();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.redb");
        let db = Arc::new(redb::Database::create(&path).unwrap());
        // Unlink while the handle is open — no mem::forget leak of the
        // TempDir (the previous approach leaked one tempdir per test run).
        let _ = std::fs::remove_file(&path);
        ToolContext::new(7, db, tx)
    }

    /// A `ToolContext` whose cancelled flag is the GIVEN atomic (shared
    /// with a [`CancelDuringDispatchBridge`] that flips it mid-dispatch).
    pub(crate) fn ctx_with_flag(flag: Arc<std::sync::atomic::AtomicBool>) -> ToolContext {
        let mut c = test_ctx();
        c.cancelled = flag;
        c
    }

    /// Build the cancel-wins-on-reply fixture shared by the per-tool tests:
    /// a bridge that flips the context's flag DURING dispatch (the entry
    /// check already passed) over an inner mock with one scripted reply, so
    /// the reply is already buffered when `wait` runs. Coerce the returned
    /// bridge with `Arc::clone(&bridge) as Arc<dyn IosToolBridge>` when
    /// handing it to a tool constructor.
    pub(crate) fn cancel_race_fixture(
        reply: crate::tools::ios_bridge::ToolBridgeReply,
    ) -> (
        Arc<CancelDuringDispatchBridge>,
        Arc<MockBridge>,
        ToolContext,
    ) {
        let inner = Arc::new(MockBridge::default());
        inner.script(MockResponse::Reply(reply));
        let flag = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let bridge = Arc::new(CancelDuringDispatchBridge {
            inner: Arc::clone(&inner),
            flag: Arc::clone(&flag),
        });
        (bridge, inner, ctx_with_flag(flag))
    }

    impl IosToolBridge for CancelDuringDispatchBridge {
        fn dispatch(
            &self,
            request: IosToolRequest,
        ) -> Result<crate::tools::ios_bridge::IosToolPending, ToolBridgeError> {
            // Flip AFTER the tool's entry check ran (it happens before this
            // call) and BEFORE the reply is consumed by wait.
            self.flag.store(true, std::sync::atomic::Ordering::Relaxed);
            self.inner.dispatch(request)
        }
    }
}
