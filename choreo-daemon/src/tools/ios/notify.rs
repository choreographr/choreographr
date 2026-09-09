//! `notify` — posts a local user notification via the bridge.
//! Compiled unconditionally (no Apple API here; see `mod.rs`).

use super::{IOS_GROUP, run_bridge_tool as run};
use crate::tools::context::ToolContext;
use crate::tools::ios_bridge::{IosToolBridge, NOTIFY_TIMEOUT};
use crate::tools::{AllowedCaller, Tool, ToolExecError};
use choreo_keystore::ServiceCredential;
use serde::{Deserialize, Serialize};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

/// Cap on the notification title (chars, not bytes — user-visible length).
pub(crate) const NOTIFY_TITLE_MAX: usize = 200;
/// Cap on the notification body (chars, not bytes — user-visible length).
pub(crate) const NOTIFY_BODY_MAX: usize = 2000;

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
pub(crate) struct NotifyArgs {
    /// Notification title (max 200 characters, enforced in execute).
    pub(crate) title: String,
    /// Notification body (max 2000 characters, enforced in execute).
    pub(crate) body: String,
}

/// `notify`: posts a local notification. The Swift host may lazily trigger a
/// provisional-authorization round trip, hence the longest of the three iOS
/// timeouts.
pub(crate) struct Notify {
    bridge: Arc<dyn IosToolBridge>,
    timeout: Duration,
}

impl Notify {
    pub(crate) fn new(bridge: Arc<dyn IosToolBridge>) -> Self {
        Self {
            bridge,
            timeout: NOTIFY_TIMEOUT,
        }
    }

    /// Test constructor with an injected deadline (deterministic timeout —
    /// no sleeps; production always uses [`NOTIFY_TIMEOUT`]).
    #[cfg(test)]
    pub(crate) fn with_timeout(bridge: Arc<dyn IosToolBridge>, timeout: Duration) -> Self {
        Self { bridge, timeout }
    }
}

impl Tool for Notify {
    type Args = NotifyArgs;
    type Return = String;
    type Error = ToolExecError;

    fn name(&self) -> &'static str {
        "notify"
    }
    fn group(&self) -> &'static str {
        IOS_GROUP
    }
    fn description(&self) -> &'static str {
        "Post a local user notification on the device with a title and body."
    }
    fn describe_invocation(&self, args: &Self::Args) -> String {
        format!("Posting notification: {}.", args.title)
    }
    // Direct-only: a notification is a user-visible channel — programmatic
    // callers must not be able to spam it (exfiltration-chain mitigation,
    // same as the other iOS tools).
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
        // Length caps enforced HERE (executor is the boundary; the schema's
        // maxLength is advisory). Chars, not bytes: the cap bounds what the
        // user sees, and a byte cap would silently differ for CJK text.
        if args.title.chars().count() > NOTIFY_TITLE_MAX {
            return Err(ToolExecError(format!(
                "notify: title exceeds {NOTIFY_TITLE_MAX} characters"
            )));
        }
        if args.body.chars().count() > NOTIFY_BODY_MAX {
            return Err(ToolExecError(format!(
                "notify: body exceeds {NOTIFY_BODY_MAX} characters"
            )));
        }
        let value = serde_json::to_value(&args)
            .map_err(|e| ToolExecError(format!("failed to encode notify arguments: {e}")))?;
        run(&self.bridge, self.name(), value, self.timeout, ctx)?;
        Ok("Notification posted.".to_string())
    }

    fn return_string(ret: &Self::Return) -> String {
        ret.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::ios::test_util::CancelDuringDispatchBridge;
    use crate::tools::ios_bridge::{MockBridge, MockResponse, ToolBridgeError};
    use std::sync::atomic::Ordering;

    fn tool(m: &Arc<MockBridge>, timeout: Duration) -> Notify {
        Notify::with_timeout(Arc::clone(m) as Arc<dyn IosToolBridge>, timeout)
    }

    fn ctx() -> ToolContext {
        let (tx, _rx) = std::sync::mpsc::channel();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.redb");
        std::mem::forget(dir); // context outlives this scope; see clipboard.rs
        ToolContext::new(7, Arc::new(redb::Database::create(path).unwrap()), tx)
    }

    #[test]
    fn success_dispatches() {
        let m = Arc::new(MockBridge::default());
        m.script(MockResponse::Reply(Ok(serde_json::Value::Null)));
        let ret = tool(&m, NOTIFY_TIMEOUT)
            .execute(
                NotifyArgs {
                    title: "Hi".into(),
                    body: "Body".into(),
                },
                None,
                None,
                None,
            )
            .unwrap();
        assert_eq!(ret, "Notification posted.");
        let dispatched = m.dispatched();
        assert_eq!(dispatched.len(), 1);
        // Parse rather than compare the raw string: serde_json object key
        // order is not the struct-declaration order.
        let sent: serde_json::Value = serde_json::from_str(&dispatched[0].args_json).unwrap();
        assert_eq!(sent, serde_json::json!({"title": "Hi", "body": "Body"}));
    }

    #[test]
    fn length_caps_enforced_before_dispatch() {
        let m = Arc::new(MockBridge::default());
        let t = tool(&m, NOTIFY_TIMEOUT);
        let err = t
            .execute(
                NotifyArgs {
                    title: "x".repeat(NOTIFY_TITLE_MAX + 1),
                    body: "ok".into(),
                },
                None,
                None,
                None,
            )
            .unwrap_err();
        assert!(err.to_string().contains("title exceeds 200"));

        // Exactly at the cap is fine (script a reply for that dispatch).
        m.script(MockResponse::Reply(Ok(serde_json::Value::Null)));
        t.execute(
            NotifyArgs {
                title: "x".repeat(NOTIFY_TITLE_MAX),
                body: "y".repeat(NOTIFY_BODY_MAX),
            },
            None,
            None,
            None,
        )
        .unwrap_or_else(|e| panic!("at-cap values must pass: {e}"));

        let err = t
            .execute(
                NotifyArgs {
                    title: "ok".into(),
                    body: "y".repeat(NOTIFY_BODY_MAX + 1),
                },
                None,
                None,
                None,
            )
            .unwrap_err();
        assert!(err.to_string().contains("body exceeds 2000"));
        // Only the two successful dispatches (at-cap call) happened.
        assert_eq!(m.dispatched().len(), 1);
    }

    #[test]
    fn error_matrix() {
        // Platform error verbatim.
        let m = Arc::new(MockBridge::default());
        m.script(MockResponse::Reply(Err(ToolBridgeError::Platform(
            "not authorized".into(),
        ))));
        let err = tool(&m, NOTIFY_TIMEOUT)
            .execute(
                NotifyArgs {
                    title: "t".into(),
                    body: "b".into(),
                },
                None,
                None,
                None,
            )
            .unwrap_err();
        assert_eq!(err.to_string(), "not authorized");

        // Dropped sender → BridgeUnavailable.
        let m = Arc::new(MockBridge::default());
        let err = tool(&m, NOTIFY_TIMEOUT)
            .execute(
                NotifyArgs {
                    title: "t".into(),
                    body: "b".into(),
                },
                None,
                None,
                None,
            )
            .unwrap_err();
        assert!(err.to_string().contains("bridge unavailable"));

        // Injected ZERO deadline → Timeout, no waiting.
        let m = Arc::new(MockBridge::default());
        m.script(MockResponse::Reply(Ok(serde_json::Value::Null)));
        let err = tool(&m, Duration::ZERO)
            .execute(
                NotifyArgs {
                    title: "t".into(),
                    body: "b".into(),
                },
                None,
                None,
                None,
            )
            .unwrap_err();
        assert!(err.to_string().contains("timed out"));
    }

    #[test]
    fn cancel_at_entry_never_dispatches_and_cancel_wins_on_reply() {
        let m = Arc::new(MockBridge::default());
        let context = ctx();
        context.cancelled.store(true, Ordering::Relaxed);
        let err = tool(&m, NOTIFY_TIMEOUT)
            .execute(
                NotifyArgs {
                    title: "t".into(),
                    body: "b".into(),
                },
                None,
                None,
                Some(&context),
            )
            .unwrap_err();
        assert_eq!(err.to_string(), "request was canceled");
        assert!(m.dispatched().is_empty());

        // Cancel-wins-on-reply: the wrapper flips the flag DURING dispatch
        // (entry check passed, reply already queued when wait runs).
        // Deterministic — no threads, no sleeps.
        let inner = Arc::new(MockBridge::default());
        inner.script(MockResponse::Reply(Ok(serde_json::Value::Null)));
        let flag = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let bridge = CancelDuringDispatchBridge {
            inner: Arc::clone(&inner),
            flag: Arc::clone(&flag),
        };
        let mut context = ctx();
        context.cancelled = flag.clone();
        let wrapped: Arc<dyn IosToolBridge> = Arc::new(bridge);
        let err = Notify::with_timeout(wrapped, NOTIFY_TIMEOUT)
            .execute(
                NotifyArgs {
                    title: "t".into(),
                    body: "b".into(),
                },
                None,
                None,
                Some(&context),
            )
            .unwrap_err();
        assert_eq!(err.to_string(), "request was canceled");
        let b = CancelDuringDispatchBridge {
            inner,
            flag: Arc::clone(&flag),
        };
        assert_eq!(
            b.cancels().len(),
            1,
            "late-cancel path must call pending.cancel()"
        );
    }

    #[test]
    fn direct_only_callers() {
        let callers = Notify::new(Arc::new(MockBridge::default())).allowed_callers();
        assert_eq!(callers, vec![AllowedCaller::Direct]);
    }
}
