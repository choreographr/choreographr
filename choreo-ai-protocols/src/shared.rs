use std::io;

use choreo_proto::InferenceError;
use serde::{Deserialize, Serialize};

use crate::types::{ChatTurnResult, StreamEvent};

/// Maximum number of tool calls we accept in a single streaming response.
/// Prevents OOM from a provider sending a maliciously large index.
pub const MAX_TOOL_CALLS: usize = 128;

/// Determines which JSON field carries the token limit in a chat
/// completions request body.
#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MaxTokensField {
    /// Use the `max_tokens` field.
    MaxTokens,
    /// Use the `max_completion_tokens` field.
    MaxCompletionTokens,
}

/// Unified error type for all API providers.
/// Each provider module re-exports this as its own error type.
#[derive(Debug, thiserror::Error)]
pub enum ProviderError {
    #[error("unauthorized ({status}): {detail}")]
    Unauthorized { status: u16, detail: String },
    #[error("rate limited: {detail}")]
    RateLimited {
        retry_after_secs: Option<u64>,
        detail: String,
    },
    #[error("server error ({status}): {detail}")]
    ServerError { status: u16, detail: String },
    #[error("client error ({status}): {detail}")]
    ClientError { status: u16, detail: String },
    #[error("provider returned an empty response")]
    EmptyResponse,
    #[error("request cancelled during retry backoff")]
    Cancelled,
    #[error("total request deadline exceeded while reading streaming response")]
    DeadlineExceeded,
    #[error("tool call arguments truncated by provider: {}", .discarded.iter().map(|d| d.to_string()).collect::<Vec<_>>().join(", "))]
    TruncatedToolCall {
        discarded: Vec<choreo_proto::DiscardedToolCall>,
    },
    #[error("{0}")]
    Io(#[from] std::io::Error),
}

impl From<crate::retry::ProviderHttpError> for ProviderError {
    fn from(err: crate::retry::ProviderHttpError) -> Self {
        match err {
            crate::retry::ProviderHttpError::Unauthorized { status, detail } => {
                ProviderError::Unauthorized { status, detail }
            }
            crate::retry::ProviderHttpError::RateLimited {
                retry_after_secs,
                detail,
            } => ProviderError::RateLimited {
                retry_after_secs,
                detail,
            },
            crate::retry::ProviderHttpError::ServerError { status, detail } => {
                ProviderError::ServerError { status, detail }
            }
            crate::retry::ProviderHttpError::ClientError { status, detail } => {
                ProviderError::ClientError { status, detail }
            }
            crate::retry::ProviderHttpError::EmptyResponse => ProviderError::EmptyResponse,
            crate::retry::ProviderHttpError::Cancelled => ProviderError::Cancelled,
            crate::retry::ProviderHttpError::Io(e) => ProviderError::Io(e),
        }
    }
}

impl From<ProviderError> for io::Error {
    fn from(err: ProviderError) -> Self {
        io::Error::other(err.to_string())
    }
}

/// Map a ProviderError variant to a stable label string.
///
/// Test-only helper. Delegates to [`InferenceError::metric_label`] so the
/// mapping cannot drift from the canonical variant list that owns it in
/// `choreo-proto` — `ProviderError` and `InferenceError` are a 1:1
/// conversion, so converting first then labeling is always correct.
#[cfg(test)]
pub(crate) fn error_type_label(e: ProviderError) -> &'static str {
    provider_error_to_inference(e).metric_label()
}

/// Convert a ProviderError into the shared InferenceError type used
/// across the ProviderClient trait boundary.
pub(crate) fn provider_error_to_inference(e: ProviderError) -> InferenceError {
    match e {
        ProviderError::Unauthorized { status, detail } => {
            InferenceError::Unauthorized { status, detail }
        }
        ProviderError::RateLimited {
            retry_after_secs,
            detail,
        } => InferenceError::RateLimited {
            retry_after_secs,
            detail,
        },
        ProviderError::ServerError { status, detail } => {
            InferenceError::ServerError { status, detail }
        }
        ProviderError::ClientError { status, detail } => {
            InferenceError::ClientError { status, detail }
        }
        ProviderError::EmptyResponse => InferenceError::EmptyResponse,
        ProviderError::Cancelled => InferenceError::Cancelled,
        ProviderError::DeadlineExceeded => InferenceError::DeadlineExceeded,
        ProviderError::TruncatedToolCall { discarded } => {
            InferenceError::TruncatedToolCall { discarded }
        }
        ProviderError::Io(e) => InferenceError::Io(e),
    }
}

/// Build a ureq agent with connect, idle-read, and total-deadline timeouts.
///
/// Two distinct read-side bounds are applied, because a streaming SSE body
/// needs both:
///
/// - `read_timeout_secs` is an *idle*/no-progress timeout — it resets each
///   time a new chunk arrives on a streaming response.  A value of `0` means
///   no limit.  It cannot interrupt a stream that keeps trickling keep-alive
///   bytes without ever forming a complete event.
/// - `total_timeout_secs` is a hard wall-clock deadline for a single HTTP
///   attempt, from DNS lookup through the last byte of the body read.  ureq's
///   `timeout_global` is the only timeout that fires even when keep-alive data
///   trickles in, so it is the backstop that prevents a stalled SSE stream
///   from hanging the worker forever.  A value of `0` means no limit.  It
///   covers one attempt: each retry restarts the deadline, so retries plus
///   their backoff can exceed this value in aggregate.
pub(crate) fn build_agent(
    connect_timeout_secs: u64,
    read_timeout_secs: u64,
    total_timeout_secs: u64,
) -> ureq::Agent {
    ureq::Agent::new_with_config(
        ureq::Agent::config_builder()
            .timeout_connect(Some(std::time::Duration::from_secs(connect_timeout_secs)))
            .timeout_recv_body(if read_timeout_secs > 0 {
                Some(std::time::Duration::from_secs(read_timeout_secs))
            } else {
                None
            })
            .timeout_global(if total_timeout_secs > 0 {
                Some(std::time::Duration::from_secs(total_timeout_secs))
            } else {
                None
            })
            .http_status_as_error(false)
            .build(),
    )
}

/// Try to list models via the API; fall back to the static known list on any
/// error.  Used by provider implementations to gracefully degrade when the
/// models endpoint is unreachable or the API key lacks permission.
pub(crate) fn list_models_with_fallback<F, E>(
    fetch: F,
    static_list: &[&str],
    provider_name: &str,
) -> Result<Vec<String>, E>
where
    F: FnOnce() -> Result<Vec<String>, E>,
    E: std::fmt::Display,
{
    match fetch() {
        Ok(models) => {
            tracing::info!("{provider_name} models returned: {}", models.len());
            Ok(models)
        }
        Err(e) => {
            tracing::warn!(
                "failed to list models from {provider_name} API, using static list: {e}"
            );
            Ok(static_list.iter().map(|s| s.to_string()).collect())
        }
    }
}

/// When a provider is configured for non-streaming mode, emit the result
/// through the streaming callback so the caller's event-driven path stays
/// uniform regardless of the streaming setting.
///
/// Consumes `result` and returns it back so the caller still owns the value
/// after emitting events.  Strings that must go to both the callback and the
/// returned result are still cloned (the event path gets the clone, the
/// returned result keeps the original).
pub(crate) fn emit_non_streaming_events(
    result: ChatTurnResult,
    on_event: &mut dyn FnMut(StreamEvent) -> io::Result<()>,
) -> io::Result<ChatTurnResult> {
    match result {
        ChatTurnResult::FinalText(final_text) => {
            if !final_text.content.is_empty() {
                on_event(StreamEvent::Answer(final_text.content.clone()))?;
            }
            if let Some(ref reasoning) = final_text.reasoning
                && !reasoning.is_empty()
            {
                on_event(StreamEvent::Reasoning(reasoning.clone()))?;
            }
            Ok(ChatTurnResult::FinalText(final_text))
        }
        ChatTurnResult::ToolUse(tool_use) => {
            if let Some(ref content) = tool_use.content
                && !content.is_empty()
            {
                on_event(StreamEvent::Answer(content.clone()))?;
            }
            if let Some(ref reasoning) = tool_use.reasoning
                && !reasoning.is_empty()
            {
                on_event(StreamEvent::Reasoning(reasoning.clone()))?;
            }
            Ok(ChatTurnResult::ToolUse(tool_use))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{ChatAssistantToolUse, ChatToolCall, FinalTextResult};

    /// Collect emitted events from a call to [`emit_non_streaming_events`].
    fn collect_events(result: ChatTurnResult) -> Vec<StreamEvent> {
        let e = std::cell::RefCell::new(Vec::new());
        let mut cb = |ev: StreamEvent| {
            e.borrow_mut().push(ev);
            Ok(())
        };
        emit_non_streaming_events(result, &mut cb).unwrap();
        e.into_inner()
    }

    // ── FinalText ──────────────────────────────────────────────────────────

    #[test]
    fn final_text_with_content_emits_answer() {
        let result = ChatTurnResult::FinalText(FinalTextResult {
            content: "hello".into(),
            reasoning: None,
            usage: None,
            response_id: None,
        });
        assert_eq!(
            collect_events(result),
            vec![StreamEvent::Answer("hello".into())]
        );
    }

    #[test]
    fn final_text_with_content_and_reasoning_emits_both() {
        let result = ChatTurnResult::FinalText(FinalTextResult {
            content: "hello".into(),
            reasoning: Some("thinking...".into()),
            usage: None,
            response_id: None,
        });
        assert_eq!(
            collect_events(result),
            vec![
                StreamEvent::Answer("hello".into()),
                StreamEvent::Reasoning("thinking...".into()),
            ]
        );
    }

    #[test]
    fn final_text_empty_content_emits_nothing() {
        let result = ChatTurnResult::FinalText(FinalTextResult {
            content: String::new(),
            reasoning: None,
            usage: None,
            response_id: None,
        });
        assert!(collect_events(result).is_empty());
    }

    #[test]
    fn final_text_empty_reasoning_is_skipped() {
        let result = ChatTurnResult::FinalText(FinalTextResult {
            content: "hi".into(),
            reasoning: Some(String::new()),
            usage: None,
            response_id: None,
        });
        assert_eq!(
            collect_events(result),
            vec![StreamEvent::Answer("hi".into())]
        );
    }

    // ── ToolUse ───────────────────────────────────────────────────────────

    fn make_tool_use(content: Option<&str>, reasoning: Option<&str>) -> ChatTurnResult {
        ChatTurnResult::ToolUse(ChatAssistantToolUse {
            content: content.map(String::from),
            tool_calls: vec![],
            reasoning: reasoning.map(String::from),
            usage: None,
            response_id: None,
        })
    }

    #[test]
    fn tool_use_with_content_emits_answer() {
        let result = make_tool_use(Some("tool text"), None);
        assert_eq!(
            collect_events(result),
            vec![StreamEvent::Answer("tool text".into())]
        );
    }

    #[test]
    fn tool_use_with_content_and_reasoning_emits_both() {
        let result = make_tool_use(Some("tool text"), Some("tool reasoning"));
        assert_eq!(
            collect_events(result),
            vec![
                StreamEvent::Answer("tool text".into()),
                StreamEvent::Reasoning("tool reasoning".into()),
            ]
        );
    }

    #[test]
    fn tool_use_empty_content_emits_reasoning_only() {
        let result = make_tool_use(None, Some("reasoning"));
        assert_eq!(
            collect_events(result),
            vec![StreamEvent::Reasoning("reasoning".into())]
        );
    }

    #[test]
    fn tool_use_empty_content_empty_reasoning_emits_nothing() {
        let result = make_tool_use(None, None);
        assert!(collect_events(result).is_empty());
    }

    #[test]
    fn tool_use_empty_reasoning_is_skipped() {
        let result = make_tool_use(Some("text"), Some(String::new()).as_deref());
        assert_eq!(
            collect_events(result),
            vec![StreamEvent::Answer("text".into())]
        );
    }

    // ── Callback error propagation ────────────────────────────────────────

    #[test]
    fn callback_error_propagates_final_text() {
        let result = ChatTurnResult::FinalText(FinalTextResult {
            content: "boom".into(),
            reasoning: None,
            usage: None,
            response_id: None,
        });
        let mut cb = |_: StreamEvent| Err(io::Error::other("oops"));
        let err = emit_non_streaming_events(result, &mut cb).unwrap_err();
        assert_eq!(err.to_string(), "oops");
    }

    #[test]
    fn callback_error_propagates_tool_use() {
        let result = ChatTurnResult::ToolUse(ChatAssistantToolUse {
            content: Some("text".into()),
            tool_calls: vec![ChatToolCall {
                id: "c1".into(),
                name: "tool".into(),
                arguments_json: "{}".into(),
                caller: None,
            }],
            reasoning: None,
            usage: None,
            response_id: None,
        });
        let mut cb = |_: StreamEvent| Err(io::Error::other("oops"));
        let err = emit_non_streaming_events(result, &mut cb).unwrap_err();
        assert_eq!(err.to_string(), "oops");
    }
}
