use super::ToolExecError;
use notify_rust::Notification;
use notify_rust::Urgency;
use schemars::JsonSchema;
use serde::Deserialize;
use std::path::Path;

/// Empty (or blank) summaries produce title-less notifications, which
/// are confusing.  Reject them at argument-deserialisation time.
const SUMMARY_REQUIRED: &str = "summary must be a non-empty string";

#[derive(Debug, Deserialize, JsonSchema)]
pub struct NotifySendArgs {
    /// Notification title/summary (required)
    pub summary: String,
    /// Optional notification body text providing more details
    pub body: Option<String>,
    /// Urgency level of the notification
    pub urgency: Option<UrgencyLevel>,
    /// Notification expiration timeout: "default" for server default, "never" to require manual dismissal, or a positive integer for milliseconds
    pub timeout: Option<Timeout>,
    /// Optional icon path or name
    pub icon: Option<String>,
}

#[derive(Debug, Clone, Copy, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum UrgencyLevel {
    Low,
    Normal,
    Critical,
}

impl From<UrgencyLevel> for Urgency {
    fn from(level: UrgencyLevel) -> Self {
        match level {
            UrgencyLevel::Low => Urgency::Low,
            UrgencyLevel::Normal => Urgency::Normal,
            UrgencyLevel::Critical => Urgency::Critical,
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub enum Timeout {
    /// Server-chosen default timeout.
    Default,
    /// Notification never expires.
    Never,
    /// Custom timeout in milliseconds.
    Milliseconds(u32),
}

impl<'de> Deserialize<'de> for Timeout {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        use serde::de::Error;
        let value = serde_json::Value::deserialize(deserializer)?;
        match value {
            serde_json::Value::String(s) if s == "default" => Ok(Timeout::Default),
            serde_json::Value::String(s) if s == "never" => Ok(Timeout::Never),
            serde_json::Value::Number(n) => {
                let ms = n.as_u64().ok_or_else(|| {
                    Error::custom("expected a non-negative integer for timeout milliseconds")
                })?;
                let ms = u32::try_from(ms)
                    .map_err(|_| Error::custom("timeout milliseconds exceeds u32 range"))?;
                Ok(Timeout::Milliseconds(ms))
            }
            _ => Err(Error::custom(
                "expected \"default\", \"never\", or a positive integer (milliseconds)",
            )),
        }
    }
}

impl From<Timeout> for notify_rust::Timeout {
    fn from(t: Timeout) -> Self {
        match t {
            Timeout::Default => notify_rust::Timeout::Default,
            Timeout::Never => notify_rust::Timeout::Never,
            Timeout::Milliseconds(ms) => notify_rust::Timeout::Milliseconds(ms),
        }
    }
}

impl JsonSchema for Timeout {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        std::borrow::Cow::Borrowed("Timeout")
    }

    fn schema_id() -> std::borrow::Cow<'static, str> {
        std::borrow::Cow::Borrowed(concat!(module_path!(), "::Timeout"))
    }

    fn json_schema(_gen: &mut schemars::SchemaGenerator) -> schemars::Schema {
        schemars::json_schema!({
            "description":
                "Notification expiration timeout: \"default\" for server default, \
                 \"never\" to require manual dismissal, or a positive integer \
                 for milliseconds",
            "oneOf": [
                { "type": "string", "enum": ["default", "never"] },
                { "type": "integer", "minimum": 0 }
            ]
        })
    }
}

pub fn execute_notify_send(
    args: &NotifySendArgs,
    _working_dir: Option<&Path>,
) -> Result<String, ToolExecError> {
    // Reject empty/whitespace-only summaries early — a notification with no
    // title is never useful and typically indicates a caller bug.
    if args.summary.trim().is_empty() {
        return Err(ToolExecError(SUMMARY_REQUIRED.into()));
    }

    // Build the notification with a fixed app name so the desktop environment
    // groups Tai's notifications under a single application entry.
    let mut notification = Notification::new();
    notification.summary(&args.summary).appname("Tai");

    // All fields except `summary` are optional; each is mapped to the
    // notify-rust builder method only when present.
    if let Some(body) = &args.body {
        notification.body(body);
    }
    if let Some(urgency) = &args.urgency {
        notification.urgency(Urgency::from(*urgency));
    }
    if let Some(timeout) = &args.timeout {
        notification.timeout(*timeout);
    }
    if let Some(icon) = &args.icon {
        notification.icon(icon);
    }

    // `show()` sends the notification over DBus and returns a handle whose
    // `id()` is the server-assigned notification ID (useful for callers that
    // want to reference or replace the notification later).
    let handle = notification.show().map_err(|e| {
        tracing::error!(error = %e, summary = %args.summary, "desktop notification failed");
        ToolExecError(format!("notification failed: {e}"))
    })?;

    tracing::info!(id = handle.id(), summary = %args.summary, "desktop notification sent");
    Ok(format!("Notification sent (id: {})", handle.id()))
}

pub(crate) struct NotifySend;

define_tool!(
    NotifySend,
    "notify_send",
    "Send a desktop notification to the user. Use this to alert the user, notify them of completed tasks, or provide information that requires their attention.",
    NotifySendArgs,
    execute_notify_send,
    "desktop"
);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deserialize_minimal_args() {
        let args: NotifySendArgs = serde_json::from_str(r#"{"summary":"test"}"#).unwrap();
        assert_eq!(args.summary, "test");
        assert!(args.body.is_none());
        assert!(args.urgency.is_none());
        assert!(args.timeout.is_none());
        assert!(args.icon.is_none());
    }

    #[test]
    fn deserialize_all_fields() {
        let args: NotifySendArgs = serde_json::from_str(
            r#"{
                "summary": "hello",
                "body": "world",
                "urgency": "critical",
                "timeout": 5000,
                "icon": "dialog-warning"
            }"#,
        )
        .unwrap();
        assert_eq!(args.summary, "hello");
        assert_eq!(args.body.unwrap(), "world");
        assert_eq!(args.urgency.unwrap() as i32, UrgencyLevel::Critical as i32);
        assert!(matches!(args.timeout.unwrap(), Timeout::Milliseconds(5000)));
        assert_eq!(args.icon.unwrap(), "dialog-warning");
    }

    #[test]
    fn deserialize_invalid_urgency_fails() {
        let result: Result<NotifySendArgs, _> =
            serde_json::from_str(r#"{"summary":"x","urgency":"urgent"}"#);
        assert!(result.is_err());
    }

    #[test]
    fn deserialize_missing_summary_fails() {
        let result: Result<NotifySendArgs, _> = serde_json::from_str(r#"{"body":"no title"}"#);
        assert!(result.is_err());
    }

    #[test]
    fn timeout_deserialize_default_string() {
        let to: Timeout = serde_json::from_str("\"default\"").unwrap();
        assert!(matches!(to, Timeout::Default));
    }

    #[test]
    fn timeout_deserialize_never_string() {
        let to: Timeout = serde_json::from_str("\"never\"").unwrap();
        assert!(matches!(to, Timeout::Never));
    }

    #[test]
    fn timeout_deserialize_integer() {
        let to: Timeout = serde_json::from_str("3000").unwrap();
        assert!(matches!(to, Timeout::Milliseconds(3000)));
    }

    #[test]
    fn timeout_conversion_to_notify_rust() {
        assert!(matches!(
            notify_rust::Timeout::from(Timeout::Default),
            notify_rust::Timeout::Default
        ));
        assert!(matches!(
            notify_rust::Timeout::from(Timeout::Never),
            notify_rust::Timeout::Never
        ));
        assert!(matches!(
            notify_rust::Timeout::from(Timeout::Milliseconds(5000)),
            notify_rust::Timeout::Milliseconds(5000)
        ));
    }

    #[test]
    fn timeout_deserialize_invalid_string_fails() {
        let result: Result<Timeout, _> = serde_json::from_str("\"invalid\"");
        assert!(result.is_err());
    }

    #[test]
    fn timeout_deserialize_negative_integer_fails() {
        let result: Result<Timeout, _> = serde_json::from_str("-1");
        assert!(result.is_err());
    }
}
