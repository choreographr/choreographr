//! `open_url` — opens an https/mailto URL on the device via the bridge.
//! Compiled unconditionally (no Apple API here; see `mod.rs`).

use super::{IOS_GROUP, run_bridge_tool as run};
use crate::tools::context::ToolContext;
use crate::tools::ios_bridge::{IosToolBridge, OPEN_URL_TIMEOUT};
use crate::tools::{AllowedCaller, Tool, ToolExecError};
use choreo_keystore::ServiceCredential;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct OpenUrlArgs {
    /// The URL to open. Only `https:` and `mailto:` schemes are accepted —
    /// see [`OpenUrlArgs`]'s hand-written `JsonSchema` and the re-validation
    /// in `validate`.
    pub(crate) url: String,
}

impl JsonSchema for OpenUrlArgs {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        std::borrow::Cow::Borrowed("OpenUrlArgs")
    }

    fn json_schema(_gen: &mut schemars::SchemaGenerator) -> schemars::Schema {
        // Hand-written so the advertised schema narrows the model to the two
        // allowed schemes (the `pattern` is advisory; execute() is the real
        // boundary). A generic `String` schema would invite `file:`/`data:`/
        // custom-app schemes, which is exactly the injection surface this
        // tool must not have.
        schemars::json_schema!({
            "type": "object",
            "properties": {
                "url": {
                    "type": "string",
                    "pattern": "^(https|mailto):",
                    "description": "The URL to open. Only https and mailto schemes are allowed."
                }
            },
            "required": ["url"],
            "additionalProperties": false
        })
    }
}

impl OpenUrlArgs {
    /// The executor-side boundary. The JSON Schema is advisory (the model may
    /// send anything), so execute() re-validates: a REAL URL parse (the `url`
    /// crate — a bare `starts_with("https:")` would accept degenerate strings
    /// like `https:not-a-url`), the scheme allow-list, and a
    /// control-character ban anywhere in the string (control chars could
    /// otherwise smuggle newlines/C0 bytes through URL handling on the host
    /// side).
    fn validate(&self) -> Result<(), ToolExecError> {
        // Control-char ban FIRST: url::Url::parse would reject (or strip)
        // them with its own error, masking this more precise message.
        if self.url.chars().any(char::is_control) {
            return Err(ToolExecError(
                "open_url: the URL must not contain control characters".into(),
            ));
        }
        let parsed = url::Url::parse(&self.url).map_err(|e| {
            ToolExecError(format!(
                "open_url only supports https and mailto URLs (not a valid URL: {e})"
            ))
        })?;
        match parsed.scheme() {
            "https" | "mailto" => {}
            other => {
                return Err(ToolExecError(format!(
                    "open_url only supports https and mailto URLs (got scheme: {other:?})"
                )));
            }
        }
        Ok(())
    }
}

/// `open_url`: hands the URL to the host (SpringBoard on iOS) and waits for
/// the completion handler. The Rust side parses/validates with the `url`
/// crate (see [`OpenUrlArgs::validate`]); the Swift host runs the SAME
/// scheme check independently over its own `URL` parse — both sides gate.
pub(crate) struct OpenUrl {
    bridge: Arc<dyn IosToolBridge>,
    timeout: Duration,
}

impl OpenUrl {
    pub(crate) fn new(bridge: Arc<dyn IosToolBridge>) -> Self {
        Self {
            bridge,
            timeout: OPEN_URL_TIMEOUT,
        }
    }

    /// Test constructor with an injected deadline (deterministic timeout —
    /// no sleeps; production always uses [`OPEN_URL_TIMEOUT`]).
    #[cfg(test)]
    pub(crate) fn with_timeout(bridge: Arc<dyn IosToolBridge>, timeout: Duration) -> Self {
        Self { bridge, timeout }
    }
}

impl Tool for OpenUrl {
    type Args = OpenUrlArgs;
    type Return = String;
    type Error = ToolExecError;

    fn name(&self) -> &'static str {
        "open_url"
    }
    fn group(&self) -> &'static str {
        IOS_GROUP
    }
    fn description(&self) -> &'static str {
        "Open an https or mailto URL on the device (hands off to the system)."
    }
    fn describe_invocation(&self, args: &Self::Args) -> String {
        format!("Opening URL: {}.", args.url)
    }
    // Direct-only: open_url is the strongest side-effect-per-byte of the
    // four (system handoff, possibly deep links into apps) — the same
    // exfiltration-chain mitigation as the other iOS tools.
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
        // Re-validate here: the schema is advisory, the executor is the
        // boundary. Validation failures happen BEFORE the bridge dispatch.
        args.validate()?;
        let value = run(&self.bridge, self.name(), &args, self.timeout, ctx)?;
        // Surface the host's verdict (IosToolHost replies `{"opened": Bool}`
        // from the completion handler) instead of discarding it — the model
        // wants to know whether the URL actually opened. Absent field (null
        // reply from tests/older hosts) degrades to success.
        let opened = value
            .get("opened")
            .and_then(|v| v.as_bool())
            .unwrap_or(true);
        Ok(if opened {
            "URL opened.".to_string()
        } else {
            "The system declined to open the URL.".to_string()
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

    fn tool(m: &Arc<MockBridge>, timeout: Duration) -> OpenUrl {
        OpenUrl::with_timeout(Arc::clone(m) as Arc<dyn IosToolBridge>, timeout)
    }

    fn ctx() -> ToolContext {
        test_ctx()
    }

    #[test]
    fn schema_narrows_to_https_mailto() {
        let schema = Tool::schema(&OpenUrl::new(Arc::new(MockBridge::default())));
        assert_eq!(schema["properties"]["url"]["pattern"], "^(https|mailto):");
    }

    #[test]
    fn https_and_mailto_pass_validation() {
        let m = Arc::new(MockBridge::default());
        for url in ["https://example.com/x?y=1", "mailto:a@b.c"] {
            m.script(MockResponse::Reply(Ok(serde_json::Value::Null)));
            tool(&m, OPEN_URL_TIMEOUT)
                .execute(OpenUrlArgs { url: url.into() }, None, None, None)
                .unwrap_or_else(|e| panic!("valid url {url} rejected: {e}"));
        }
        let dispatched = m.dispatched();
        assert_eq!(dispatched.len(), 2);
        assert_eq!(dispatched[0].name, "open_url");
        assert_eq!(dispatched[1].args_json, r#"{"url":"mailto:a@b.c"}"#);
    }

    #[test]
    fn disallowed_schemes_rejected_before_dispatch() {
        let m = Arc::new(MockBridge::default());
        for url in [
            "file:///etc/passwd",
            "http://insecure.example",
            "ftp://x",
            "data:text/html,hi",
            "myapp://deep/link",
            "nonsense",
        ] {
            let err = tool(&m, OPEN_URL_TIMEOUT)
                .execute(OpenUrlArgs { url: url.into() }, None, None, None)
                .unwrap_err();
            assert!(
                err.to_string().contains("https and mailto"),
                "url {url} failed with the wrong error: {err}"
            );
        }
        assert!(
            m.dispatched().is_empty(),
            "rejected URLs must never dispatch"
        );
    }

    #[test]
    fn control_characters_rejected_before_dispatch() {
        let m = Arc::new(MockBridge::default());
        for url in ["https://ok\u{0}bad", "https://x\ny", "mailto:a@b\u{7}"] {
            let err = tool(&m, OPEN_URL_TIMEOUT)
                .execute(OpenUrlArgs { url: url.into() }, None, None, None)
                .unwrap_err();
            assert!(err.to_string().contains("control characters"));
        }
        assert!(m.dispatched().is_empty());
    }

    #[test]
    fn bridge_errors_map_sensibly() {
        // Platform error passes through verbatim.
        let m = Arc::new(MockBridge::default());
        m.script(MockResponse::Reply(Err(ToolBridgeError::Platform(
            "springboard refused".into(),
        ))));
        let err = tool(&m, OPEN_URL_TIMEOUT)
            .execute(
                OpenUrlArgs {
                    url: "https://x".into(),
                },
                None,
                None,
                None,
            )
            .unwrap_err();
        assert_eq!(err.to_string(), "springboard refused");

        // Dropped sender → BridgeUnavailable message.
        let m = Arc::new(MockBridge::default());
        let err = tool(&m, OPEN_URL_TIMEOUT)
            .execute(
                OpenUrlArgs {
                    url: "https://x".into(),
                },
                None,
                None,
                None,
            )
            .unwrap_err();
        assert!(err.to_string().contains("bridge unavailable"));
    }

    #[test]
    fn zero_timeout_times_out() {
        let m = Arc::new(MockBridge::default());
        m.script(MockResponse::Reply(Ok(serde_json::Value::Null)));
        let err = tool(&m, Duration::ZERO)
            .execute(
                OpenUrlArgs {
                    url: "https://x".into(),
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
        // Cancel at entry.
        context.cancelled.store(true, Ordering::Relaxed);
        let err = tool(&m, OPEN_URL_TIMEOUT)
            .execute(
                OpenUrlArgs {
                    url: "https://x".into(),
                },
                None,
                None,
                Some(&context),
            )
            .unwrap_err();
        assert_eq!(err.to_string(), "request was canceled");
        assert!(m.dispatched().is_empty());

        // Cancel wins on reply: the wrapper flips the flag DURING dispatch
        // (entry check already passed; reply already queued when wait runs).
        // Deterministic — no threads, no sleeps.
        let (bridge, _inner, context) =
            crate::tools::ios::test_util::cancel_race_fixture(Ok(serde_json::Value::Null));
        let err = OpenUrl::with_timeout(
            Arc::clone(&bridge) as Arc<dyn IosToolBridge>,
            OPEN_URL_TIMEOUT,
        )
        .execute(
            OpenUrlArgs {
                url: "https://y".into(),
            },
            None,
            None,
            Some(&context),
        )
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
    fn declined_open_is_surfaced() {
        // The host's `{"opened": false}` verdict must NOT be reported to the
        // model as a successful open.
        let m = Arc::new(MockBridge::default());
        m.script(MockResponse::Reply(Ok(
            serde_json::json!({"opened": false}),
        )));
        let ret = tool(&m, OPEN_URL_TIMEOUT)
            .execute(
                OpenUrlArgs {
                    url: "https://x".into(),
                },
                None,
                None,
                None,
            )
            .unwrap();
        assert!(ret.contains("declined"), "got: {ret}");
    }

    #[test]
    fn direct_only_callers() {
        let callers = OpenUrl::new(Arc::new(MockBridge::default())).allowed_callers();
        assert_eq!(callers, vec![AllowedCaller::Direct]);
    }
}
