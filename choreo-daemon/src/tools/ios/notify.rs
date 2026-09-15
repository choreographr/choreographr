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
    // The schema advertises maxLength mirroring the executor caps (schemars
    // counts JS string units — UTF-16 — while the executor counts chars; the
    // schema is advisory, so the executor remains the real boundary).
    #[schemars(length(max = 200))]
    pub(crate) title: String,
    /// Notification body (max 2000 characters, enforced in execute).
    #[schemars(length(max = 2000))]
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
        let value = run(&self.bridge, self.name(), &args, self.timeout, ctx)?;
        // Surface the host's scheduling verdict (IosToolHost replies
        // `{"scheduled": Bool}`) instead of discarding it — a host that
        // declined (e.g. notifications disabled system-wide) must not be
        // reported to the model as success. Absent field (null reply from
        // tests/older hosts) degrades to success.
        let scheduled = value
            .get("scheduled")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(true);
        Ok(if scheduled {
            "Notification posted.".to_string()
        } else {
            "The notification could not be scheduled (the system declined).".to_string()
        })
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

    fn tool(m: &Arc<MockBridge>, timeout: Duration) -> Notify {
        Notify::with_timeout(Arc::clone(m) as Arc<dyn IosToolBridge>, timeout)
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
        let context = test_ctx();
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
        let (bridge, _inner, context) =
            crate::tools::ios::test_util::cancel_race_fixture(Ok(serde_json::Value::Null));
        let err = Notify::with_timeout(
            Arc::clone(&bridge) as Arc<dyn IosToolBridge>,
            NOTIFY_TIMEOUT,
        )
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
        assert_eq!(
            bridge.cancels().len(),
            1,
            "late-cancel path must call pending.cancel()"
        );
    }

    #[test]
    fn schema_advertises_max_length() {
        // The advisory schema mirrors the executor caps (schemars counts
        // UTF-16 units; the executor counts chars — the executor remains the
        // boundary).
        let schema = Tool::schema(&Notify::new(Arc::new(MockBridge::default())));
        assert_eq!(schema["properties"]["title"]["maxLength"], 200);
        assert_eq!(schema["properties"]["body"]["maxLength"], 2000);
    }

    #[test]
    fn declined_schedule_is_surfaced() {
        // The host's `{"scheduled": false}` verdict must NOT be reported to
        // the model as success.
        let m = Arc::new(MockBridge::default());
        m.script(MockResponse::Reply(Ok(
            serde_json::json!({"scheduled": false}),
        )));
        let ret = tool(&m, NOTIFY_TIMEOUT)
            .execute(
                NotifyArgs {
                    title: "t".into(),
                    body: "b".into(),
                },
                None,
                None,
                None,
            )
            .unwrap();
        assert!(ret.contains("could not be scheduled"), "got: {ret}");
    }

    #[test]
    fn direct_only_callers() {
        let callers = Notify::new(Arc::new(MockBridge::default())).allowed_callers();
        assert_eq!(callers, vec![AllowedCaller::Direct]);
    }
}
