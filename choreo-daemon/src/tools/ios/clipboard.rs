//! `clipboard_write` / `clipboard_read` — iOS pasteboard tools over the
//! bridge. Compiled unconditionally (no Apple API here; see `mod.rs`).

use super::{IOS_GROUP, run_bridge_tool as run};
use crate::tools::context::ToolContext;
use crate::tools::ios_bridge::{CLIPBOARD_TIMEOUT, IosToolBridge};
use crate::tools::{AllowedCaller, EmptyArgs, Tool, ToolExecError};
use choreo_keystore::ServiceCredential;
use serde::{Deserialize, Serialize};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

// ── clipboard_write ─────────────────────────────────────────────────────────

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
pub(crate) struct ClipboardWriteArgs {
    /// The text to place on the pasteboard (replaces its current contents).
    pub(crate) text: String,
}

/// `clipboard_write`: sets the pasteboard text. The Swift host replies
/// `null` on success, so the tool returns a fixed human-readable line.
pub(crate) struct ClipboardWrite {
    /// Shared bridge (one per process; dispatch is enqueue-only).
    bridge: Arc<dyn IosToolBridge>,
    /// Deadline for the bridge round-trip; `Duration::ZERO` in tests makes
    /// the wait time out deterministically (no sleeps).
    timeout: Duration,
}

impl ClipboardWrite {
    pub(crate) fn new(bridge: Arc<dyn IosToolBridge>) -> Self {
        Self {
            bridge,
            timeout: CLIPBOARD_TIMEOUT,
        }
    }

    /// Test constructor with an injected deadline (the deterministic timeout
    /// path — production code always uses [`CLIPBOARD_TIMEOUT`]).
    #[cfg(test)]
    pub(crate) fn with_timeout(bridge: Arc<dyn IosToolBridge>, timeout: Duration) -> Self {
        Self { bridge, timeout }
    }
}

impl Tool for ClipboardWrite {
    type Args = ClipboardWriteArgs;
    type Return = String;
    type Error = ToolExecError;

    fn name(&self) -> &'static str {
        "clipboard_write"
    }
    fn group(&self) -> &'static str {
        IOS_GROUP
    }
    fn description(&self) -> &'static str {
        "Write text to the device clipboard (iOS pasteboard), replacing its \
         current contents."
    }
    fn describe_invocation(&self, args: &Self::Args) -> String {
        format!(
            "Writing {} characters to the clipboard.",
            args.text.chars().count()
        )
    }
    // Direct-only: a programmatic (JS) caller could turn this tool into the
    // last hop of an exfiltration chain (any computed value → shared OS
    // clipboard → user pastes it anywhere). Same policy as the other
    // session-config tools (set_working_dir/load_tools).
    fn allowed_callers(&self) -> Vec<AllowedCaller> {
        vec![AllowedCaller::Direct]
    }

    fn execute(
        &self,
        args: Self::Args,
        _x_credentials: Option<&ServiceCredential>,
        _working_dir: Option<&Path>,
        ctx: Option<&ToolContext>,
    ) -> Result<Self::Return, Self::Error> {
        run(&self.bridge, self.name(), &args, self.timeout, ctx)?;
        Ok("Clipboard updated.".to_string())
    }

    fn return_string(ret: &Self::Return) -> String {
        ret.clone()
    }
}

// ── clipboard_read ──────────────────────────────────────────────────────────

/// `clipboard_read`: returns the pasteboard text (empty string when the
/// pasteboard holds none). Takes no arguments ([`EmptyArgs`]).
pub(crate) struct ClipboardRead {
    bridge: Arc<dyn IosToolBridge>,
    timeout: Duration,
}

impl ClipboardRead {
    pub(crate) fn new(bridge: Arc<dyn IosToolBridge>) -> Self {
        Self {
            bridge,
            timeout: CLIPBOARD_TIMEOUT,
        }
    }

    #[cfg(test)]
    pub(crate) fn with_timeout(bridge: Arc<dyn IosToolBridge>, timeout: Duration) -> Self {
        Self { bridge, timeout }
    }
}

impl Tool for ClipboardRead {
    type Args = EmptyArgs;
    type Return = String;
    type Error = ToolExecError;

    fn name(&self) -> &'static str {
        "clipboard_read"
    }
    fn group(&self) -> &'static str {
        IOS_GROUP
    }
    fn description(&self) -> &'static str {
        "Read the current text from the device clipboard (iOS pasteboard). \
         Returns an empty string when the pasteboard holds no text."
    }
    fn describe_invocation(&self, _args: &Self::Args) -> String {
        "Reading the clipboard.".to_string()
    }
    // Direct-only: reading the pasteboard hands arbitrary user data to the
    // model — programmatic callers must never get that surface.
    fn allowed_callers(&self) -> Vec<AllowedCaller> {
        vec![AllowedCaller::Direct]
    }

    fn execute(
        &self,
        _args: Self::Args,
        _x_credentials: Option<&ServiceCredential>,
        _working_dir: Option<&Path>,
        ctx: Option<&ToolContext>,
    ) -> Result<Self::Return, Self::Error> {
        // The Swift host replies `{"text": String}` (empty string when the
        // pasteboard has no text — see IosToolHost.swift).
        let value = run(
            &self.bridge,
            self.name(),
            &serde_json::json!({}),
            self.timeout,
            ctx,
        )?;
        Ok(value
            .get("text")
            .and_then(|t| t.as_str())
            .unwrap_or_default()
            .to_string())
    }

    fn return_string(ret: &Self::Return) -> String {
        ret.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::ios::test_util::test_ctx;
    use crate::tools::ios_bridge::{MockBridge, MockResponse, ToolBridgeError};
    use std::sync::atomic::Ordering;

    #[test]
    fn clipboard_write_success() {
        let m = Arc::new(MockBridge::default());
        m.script(MockResponse::Reply(Ok(serde_json::Value::Null)));
        let tool = ClipboardWrite::with_timeout(
            Arc::clone(&m) as Arc<dyn IosToolBridge>,
            CLIPBOARD_TIMEOUT,
        );
        let ret = tool
            .execute(ClipboardWriteArgs { text: "hi".into() }, None, None, None)
            .unwrap();
        assert_eq!(ret, "Clipboard updated.");
        let dispatched = m.dispatched();
        assert_eq!(dispatched.len(), 1);
        assert_eq!(dispatched[0].name, "clipboard_write");
        assert_eq!(dispatched[0].args_json, r#"{"text":"hi"}"#);
    }

    #[test]
    fn clipboard_read_success_and_empty() {
        let m = Arc::new(MockBridge::default());
        m.script(MockResponse::Reply(Ok(
            serde_json::json!({"text": "copied!"}),
        )));
        let tool = ClipboardRead::with_timeout(
            Arc::clone(&m) as Arc<dyn IosToolBridge>,
            CLIPBOARD_TIMEOUT,
        );
        assert_eq!(
            tool.execute(EmptyArgs {}, None, None, None).unwrap(),
            "copied!"
        );

        // No "text" field (or null payload) degrades to the empty string —
        // the documented pasteboard-has-no-text case must not error.
        m.script(MockResponse::Reply(Ok(serde_json::Value::Null)));
        assert_eq!(tool.execute(EmptyArgs {}, None, None, None).unwrap(), "");
    }

    #[test]
    fn platform_error_surfaces() {
        let m = Arc::new(MockBridge::default());
        m.script(MockResponse::Reply(Err(ToolBridgeError::Platform(
            "pasteboard refused".into(),
        ))));
        let tool = ClipboardWrite::with_timeout(
            Arc::clone(&m) as Arc<dyn IosToolBridge>,
            CLIPBOARD_TIMEOUT,
        );
        let err = tool
            .execute(ClipboardWriteArgs { text: "x".into() }, None, None, None)
            .unwrap_err();
        assert_eq!(err.to_string(), "pasteboard refused");
    }

    #[test]
    fn bridge_unavailable_surfaces() {
        // Empty script → the mock drops the sender → BridgeUnavailable.
        let m = Arc::new(MockBridge::default());
        let tool = ClipboardRead::with_timeout(
            Arc::clone(&m) as Arc<dyn IosToolBridge>,
            CLIPBOARD_TIMEOUT,
        );
        let err = tool.execute(EmptyArgs {}, None, None, None).unwrap_err();
        assert!(err.to_string().contains("bridge unavailable"));
    }

    #[test]
    fn zero_timeout_times_out() {
        let m = Arc::new(MockBridge::default());
        m.script(MockResponse::Reply(Ok(serde_json::Value::Null)));
        // Injected Duration::ZERO: deterministic timeout, no waiting at all.
        let tool =
            ClipboardWrite::with_timeout(Arc::clone(&m) as Arc<dyn IosToolBridge>, Duration::ZERO);
        let err = tool
            .execute(ClipboardWriteArgs { text: "x".into() }, None, None, None)
            .unwrap_err();
        assert!(err.to_string().contains("timed out"));
    }

    #[test]
    fn cancel_at_entry_never_dispatches() {
        let m = Arc::new(MockBridge::default());
        let context = test_ctx();
        context.cancelled.store(true, Ordering::Relaxed);
        let tool = ClipboardWrite::with_timeout(
            Arc::clone(&m) as Arc<dyn IosToolBridge>,
            CLIPBOARD_TIMEOUT,
        );
        let err = tool
            .execute(
                ClipboardWriteArgs { text: "x".into() },
                None,
                None,
                Some(&context),
            )
            .unwrap_err();
        assert_eq!(err.to_string(), "request was canceled");
        // The binding assertion: the bridge saw NOTHING.
        assert!(m.dispatched().is_empty());
    }

    #[test]
    fn cancel_wins_over_arrived_reply() {
        // The bridge flips the cancel flag DURING dispatch: the entry check
        // passes, the reply is already queued when wait runs, and wait's
        // cancel-precedence must surface Canceled (the recorded best-effort
        // cancel proves the late-cancel path ran). Deterministic — no threads.
        let (bridge, _inner, context) = crate::tools::ios::test_util::cancel_race_fixture(Ok(
            serde_json::json!({"text": "late"}),
        ));
        let tool = ClipboardRead::with_timeout(
            Arc::clone(&bridge) as Arc<dyn IosToolBridge>,
            CLIPBOARD_TIMEOUT,
        );
        let err = tool
            .execute(EmptyArgs {}, None, None, Some(&context))
            .unwrap_err();
        assert_eq!(err.to_string(), "request was canceled");
        assert_eq!(bridge.dispatched().len(), 1, "dispatch must have happened");
        assert_eq!(
            bridge.cancels().len(),
            1,
            "late-cancel path must call pending.cancel()"
        );
    }

    #[test]
    fn ios_tools_are_direct_only() {
        let m: Arc<dyn IosToolBridge> = Arc::new(MockBridge::default());
        for callers in [
            ClipboardWrite::with_timeout(Arc::clone(&m), CLIPBOARD_TIMEOUT).allowed_callers(),
            ClipboardRead::with_timeout(m, CLIPBOARD_TIMEOUT).allowed_callers(),
        ] {
            assert_eq!(callers, vec![AllowedCaller::Direct]);
        }
    }
}
