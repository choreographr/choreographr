//! The chat-completions wire types this adapter parses and emits.
//!
//! Request bodies ([`ChatCompletionsRequest`]), non-streaming and streaming
//! response shapes, and the lenient [`FinishReason`] that a provider's unknown
//! value must degrade to rather than fail the parse. Reasoning arrives here as
//! both scalar fields and the structured `reasoning_details` array, which
//! [`AssistantMessage::take_reasoning`] folds through the adapter's one shared
//! state machine.

use serde::{Deserialize, Serialize};
use tracing::debug;

use super::super::{AssistantToolCall, ChatToolDefinition, Usage};
use super::reasoning_stream::{ReasoningStreamState, TurnReasoning};
use crate::cache_retention::CacheRetentionOptions;
use choreo_proto::{ChatReasoningField, ReasoningArtifact};

// ── Chat Completions wire types ──────────────────────────────────────────

#[derive(Debug, Serialize)]
pub(crate) struct ChatCompletionsRequest<'a, M>
where
    M: Serialize,
{
    pub(crate) model: &'a str,
    #[serde(bound(serialize = "M: Serialize"))]
    pub(crate) messages: &'a [M],
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) tools: Option<&'a [ChatToolDefinition]>,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub(crate) stream: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) stream_options: Option<ChatCompletionsStreamOptions>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) max_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) max_completion_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) reasoning_effort: Option<&'a str>,
    /// Per-session cache-affinity hint for OpenAI-compatible providers that
    /// accept it (see [`crate::CacheAffinity`]). Omitted when
    /// the provider's dialect is not `prompt-cache-key` or the turn has no
    /// session id.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) prompt_cache_key: Option<&'a str>,
    /// Per-session cache-affinity hint in `OpenRouter`'s dialect, which uses a
    /// top-level `session_id` instead of `prompt_cache_key`. At most one of the
    /// two affinity fields is set for a given request.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) session_id: Option<&'a str>,
    /// Extended prompt-cache retention for pre-GPT-5.6 `OpenAI` models (see
    /// [`crate::CacheRetentionDialect`]); omitted unless the model opted in and
    /// declares a long TTL tier.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) prompt_cache_retention: Option<&'static str>,
    /// Extended prompt-cache retention for GPT-5.6+ `OpenAI` models, nested
    /// under `prompt_cache_options`. Never emitted on Chat Completions: the
    /// only extended value (`"12h"`) is Responses-API-only, so the field is
    /// present for wire symmetry but stays `None` here.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) prompt_cache_options: Option<CacheRetentionOptions>,
}

#[derive(Debug, Clone, Copy, Serialize)]
pub(crate) struct ChatCompletionsStreamOptions {
    pub(crate) include_usage: bool,
}

#[derive(Debug, Deserialize)]
pub(crate) struct ChatCompletionsResponse {
    pub(crate) choices: Vec<Choice>,
    pub(crate) usage: Option<Usage>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct Choice {
    pub(crate) message: AssistantMessage,
    /// z.ai/OpenAI finish reason on the choice. `None` when the provider
    /// omits it (never treated as an error — see `FinishReason`).
    pub(crate) finish_reason: Option<FinishReason>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct AssistantMessage {
    pub(crate) content: Option<String>,
    #[serde(default)]
    pub(crate) tool_calls: Vec<AssistantToolCall>,
    pub(crate) reasoning_content: Option<String>,
    pub(crate) reasoning: Option<String>,
    pub(crate) reasoning_text: Option<String>,
    /// The structured `reasoning_details` array (`OpenRouter` and the bare
    /// shape some routes stream), kept as raw JSON so an unrecognised entry
    /// shape cannot fail the whole response parse.
    pub(crate) reasoning_details: Option<serde_json::Value>,
}

impl AssistantMessage {
    /// Extract the reasoning payload and display text for the turn.
    ///
    /// The response's reasoning fields are folded through the same
    /// [`ReasoningStreamState`] the streaming paths use, so field precedence
    /// (`reasoning_content` > `reasoning` > `reasoning_text`, first NON-empty
    /// wins), empty-skipping, the structured-artifact-over-scalar choice, and
    /// the display fallback are decided in exactly one place. The live-display
    /// callback is a no-op: a non-streaming turn has no stream to render, so the
    /// derived text rides the turn result instead.
    ///
    /// Returns the turn's [`TurnReasoning`]: its display text, its opaque
    /// [`ReasoningArtifact`] (which records WHICH field the payload was observed
    /// under, so re-emission targets the same wire field — a provider that sends
    /// `reasoning_text` must not have its payload echoed back as
    /// `reasoning_content` on the next tool-loop turn), and whether the response
    /// carried reasoning at all.
    pub(crate) fn take_reasoning(&mut self) -> TurnReasoning {
        let mut state = ReasoningStreamState::default();
        let delta = StreamDelta {
            content: None,
            tool_calls: None,
            reasoning_content: self.reasoning_content.take(),
            reasoning: self.reasoning.take(),
            reasoning_text: self.reasoning_text.take(),
            reasoning_details: self.reasoning_details.take(),
        };
        // The fold cannot fail with a no-op callback (there is no live stream to
        // render); the fold's own output fact still rides on the result, so an
        // encrypted-only or over-cap response is not read as empty.
        let _ = state.apply(&delta, &mut |_| Ok(()));
        state.finish()
    }
}

/// Build the opaque `ChatReasoning` artifact from captured reasoning text,
/// tagging it with the wire field it came from (`field`), or `None` when
/// nothing reusable was produced. Empty strings are skipped: an empty payload
/// captures nothing, and a later passback policy shouldn't be tempted to echo
/// an empty reasoning field (some providers reject `""` outright).
pub(crate) fn chat_reasoning_artifact(
    field: ChatReasoningField,
    reasoning: &str,
) -> Option<ReasoningArtifact> {
    if reasoning.is_empty() {
        return None;
    }
    debug!(
        ?field,
        payload_bytes = reasoning.len(),
        "captured chat reasoning artifact"
    );
    Some(ReasoningArtifact::ChatReasoning {
        field,
        bytes: reasoning.as_bytes().to_vec(),
    })
}

#[derive(Debug, Deserialize)]
pub(crate) struct ChatCompletionsStreamResponse {
    pub(crate) choices: Vec<StreamChoice>,
    /// Token usage, present only on the final chunk of a stream that reports it.
    pub(crate) usage: Option<Usage>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct StreamChoice {
    pub(crate) delta: Option<StreamDelta>,
    /// The finish reason arrives on the final SSE chunk (before `[DONE]`);
    /// every earlier chunk has it absent.
    pub(crate) finish_reason: Option<FinishReason>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct StreamDelta {
    pub(crate) content: Option<String>,
    pub(crate) tool_calls: Option<Vec<StreamToolCallDelta>>,
    pub(crate) reasoning_content: Option<String>,
    pub(crate) reasoning: Option<String>,
    pub(crate) reasoning_text: Option<String>,
    /// A structured `reasoning_details` delta (an array of entries, or a single
    /// entry object for tolerance).
    pub(crate) reasoning_details: Option<serde_json::Value>,
}

#[derive(Debug, Default, Clone, Deserialize)]
pub(crate) struct StreamToolCallDelta {
    pub(crate) index: u32,
    pub(crate) id: Option<String>,
    // Deserialised from the API's "type" field but never read in Rust — kept
    // so serde doesn't choke on unknown fields and to document the wire format.
    // Read only in some build targets, so `dead_code` cannot be an `expect`;
    // kept as an `allow` with its `allow_attributes` exemption.
    #[allow(clippy::allow_attributes)]
    #[allow(dead_code)]
    #[serde(rename = "type")]
    pub(crate) kind: Option<String>,
    pub(crate) function: Option<StreamToolCallFunctionDelta>,
}

#[derive(Debug, Default, Clone, Deserialize)]
pub(crate) struct StreamToolCallFunctionDelta {
    pub(crate) name: Option<String>,
    pub(crate) arguments: Option<String>,
}

/// Lenient, provider-portable chat-completions `finish_reason`.
///
/// Covers z.ai's documented set (`stop`, `tool_calls`, `length`, `sensitive`,
/// `model_context_window_exceeded`, `network_error`) plus `OpenAI`'s aliases
/// (`content_filter` → [`FinishReason::Sensitive`], `function_call` →
/// [`FinishReason::ToolCalls`]). Unknown values map to
/// [`FinishReason::Other`] and NEVER fail the response parse — a provider
/// adding a new finish reason must degrade to a logged diagnostic, not a
/// hard error on every response.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum FinishReason {
    Stop,
    ToolCalls,
    Length,
    Sensitive,
    ContextWindowExceeded,
    NetworkError,
    /// Any unrecognized raw value, preserved verbatim for diagnostics.
    Other(String),
}

impl FinishReason {
    /// Map a raw wire string to the lenient enum. Never fails.
    fn parse(raw: &str) -> Self {
        match raw {
            "stop" => FinishReason::Stop,
            // `function_call` is OpenAI's legacy single-function alias for
            // the modern `tool_calls` — treat identically.
            "tool_calls" | "function_call" => FinishReason::ToolCalls,
            "length" => FinishReason::Length,
            // z.ai's `sensitive` (content-filter refusal) and OpenAI's
            // `content_filter` are the same policy denial — both terminal.
            "sensitive" | "content_filter" => FinishReason::Sensitive,
            "model_context_window_exceeded" => FinishReason::ContextWindowExceeded,
            "network_error" => FinishReason::NetworkError,
            other => {
                // Preserve the raw string so new provider values are
                // diagnosable in logs instead of silently vanishing.
                debug!(raw = %other, "unknown chat-completions finish_reason");
                FinishReason::Other(other.to_string())
            }
        }
    }

    /// Whether this reason is a *content-filter* denial (z.ai `sensitive`,
    /// `OpenAI` `content_filter`): terminal and never retryable — resending
    /// the same prompt can never clear the policy filter.
    pub(super) fn is_content_filtered(&self) -> bool {
        matches!(self, FinishReason::Sensitive)
    }

    /// Whether this reason signals a prompt over the model's context window:
    /// terminal (a compaction-bug signal), never retried.
    pub(super) fn is_context_window_exceeded(&self) -> bool {
        matches!(self, FinishReason::ContextWindowExceeded)
    }
}

// Custom impl: serde's derive would hard-error on an unrecognized string,
// which would fail the WHOLE response parse. Going through `String` +
// `FinishReason::parse` keeps unknown values lenient (`Other(raw)`).
impl<'de> Deserialize<'de> for FinishReason {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(deserializer)?;
        Ok(FinishReason::parse(&raw))
    }
}
