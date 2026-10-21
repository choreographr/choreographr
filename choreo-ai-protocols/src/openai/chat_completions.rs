use serde::{Deserialize, Serialize};
use tracing::debug;

use super::retry;
use super::{
    ChatRequestMessage, ChatToolDefinition, SseReader, endpoint_url, validate_tool_call_arguments,
};
use crate::shared::{MAX_TOOL_CALLS, RequestAuth};
use crate::types::{
    ChatAssistantToolUse, ChatToolCall, ChatTurnResult, FinalTextResult, StreamEvent,
};
use choreo_proto::{ChatReasoningField, ReasoningArtifact, TokenUsage};
use std::collections::HashMap;
use std::io;

use super::reasoning_details::DetailsCapture;

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
}

#[derive(Debug, Serialize)]
pub(crate) struct ChatCompletionsStreamOptions {
    pub(crate) include_usage: bool,
}

#[derive(Debug, Deserialize)]
pub(crate) struct ChatCompletionsResponse {
    pub(crate) choices: Vec<Choice>,
    pub(crate) usage: Option<super::Usage>,
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
    pub(crate) tool_calls: Vec<super::AssistantToolCall>,
    pub(crate) reasoning_content: Option<String>,
    pub(crate) reasoning: Option<String>,
    pub(crate) reasoning_text: Option<String>,
    /// The structured `reasoning_details` array (`OpenRouter` / Kimi-shaped
    /// routes), kept as raw JSON so an unrecognised entry shape cannot fail the
    /// whole response parse.
    #[serde(default)]
    pub(crate) reasoning_details: Option<serde_json::Value>,
}

impl AssistantMessage {
    /// Extract the reasoning payload from whichever field the model populated.
    ///
    /// The scalar fields are consumed with the non-streaming precedence
    /// `reasoning_content` > `reasoning` > `reasoning_text`. When the route
    /// streamed a structured `reasoning_details` array, it becomes the captured
    /// artifact (the richer, replayable payload) regardless of which scalar
    /// field also carried display text.
    ///
    /// Returns the display text alongside an opaque [`ReasoningArtifact`].
    /// The artifact records WHICH field was consumed, so re-emission targets the
    /// same wire field the provider used — a provider that sends
    /// `reasoning_text` must not have its payload echoed back as
    /// `reasoning_content` on the next tool-loop turn. When no scalar reasoning
    /// streamed, the display text is derived from the details array's
    /// text/summary entries.
    pub(crate) fn take_reasoning(&mut self) -> (Option<String>, Option<ReasoningArtifact>) {
        // Structured details are normalized first so both the artifact and the
        // display fallback are ready before the scalar precedence is applied.
        let mut details = DetailsCapture::default();
        if let Some(raw) = self.reasoning_details.take() {
            // Nothing on this path displays the fragments live: the turn result
            // carries the derived text, so the callback is a no-op.
            details.push(&raw, &mut |_| {});
        }
        let details_display = details.display();
        let details_artifact = details.finish();

        let scalar = self
            .reasoning_content
            .take()
            .map(|text| (ChatReasoningField::ReasoningContent, text))
            .or_else(|| {
                self.reasoning
                    .take()
                    .map(|text| (ChatReasoningField::Reasoning, text))
            })
            .or_else(|| {
                self.reasoning_text
                    .take()
                    .map(|text| (ChatReasoningField::ReasoningText, text))
            });
        if let Some((field, text)) = scalar {
            // Prefer the structured artifact: it is the payload the route can
            // replay. Fall back to the scalar text when no details streamed.
            let artifact = details_artifact.or_else(|| chat_reasoning_artifact(field, &text));
            let display = if text.is_empty() {
                details_display
            } else {
                text
            };
            return ((!display.is_empty()).then_some(display), artifact);
        }
        // No scalar field: any display text comes from the details entries.
        (
            (!details_display.is_empty()).then_some(details_display),
            details_artifact,
        )
    }
}

/// Build the opaque `ChatReasoning` artifact from captured reasoning text,
/// tagging it with the wire field it came from (`field`), or `None` when
/// nothing reusable was produced. Empty strings are skipped: an empty payload
/// captures nothing, and a later passback policy shouldn't be tempted to echo
/// an empty reasoning field (some providers reject `""` outright).
fn chat_reasoning_artifact(
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

/// Fold one `reasoning_details` delta into `capture`, forwarding each newly
/// recognised fragment to `on_event` as a live reasoning display event.
///
/// The capture reports fragments through an infallible callback because it has
/// nothing to do with display, so an `on_event` failure cannot be propagated
/// from inside it: the first error is parked and re-raised here after the delta
/// is folded. Deferring the error rather than collecting the fragments into a
/// list keeps a delta with nothing displayable allocation-free.
fn push_details_fragments<F>(
    capture: &mut DetailsCapture,
    raw: &serde_json::Value,
    on_event: &mut F,
) -> io::Result<()>
where
    F: FnMut(StreamEvent) -> io::Result<()>,
{
    let mut emit_error: Option<io::Error> = None;
    capture.push(raw, &mut |fragment| {
        // After a failure the events are dropped, but the delta is still folded
        // so the capture describes the stream that was actually received.
        if emit_error.is_none()
            && let Err(error) = on_event(StreamEvent::Reasoning(fragment.to_string()))
        {
            emit_error = Some(error);
        }
    });
    match emit_error {
        Some(error) => Err(error),
        None => Ok(()),
    }
}

#[derive(Debug, Deserialize)]
pub(crate) struct ChatCompletionsStreamResponse {
    pub(crate) choices: Vec<StreamChoice>,
    #[serde(default)]
    pub(crate) usage: Option<super::Usage>,
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
    fn is_content_filtered(&self) -> bool {
        matches!(self, FinishReason::Sensitive)
    }

    /// Whether this reason signals a prompt over the model's context window:
    /// terminal (a compaction-bug signal), never retried.
    fn is_context_window_exceeded(&self) -> bool {
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

// ── Simple (no-tool) chat completions request ────────────────────────────

pub(crate) fn chat_completions_request(
    agent: &ureq::Agent,
    config: &super::ServiceConfig,
    auth: &RequestAuth,
    model: &str,
    prompt: &str,
    cancel_rx: Option<&crossbeam_channel::Receiver<()>>,
) -> Result<String, super::OpenAiError> {
    let url = endpoint_url(&config.base_url, &config.chat_completions_path)?;
    let (max_tokens_field, max_completion_tokens_field) = config.max_tokens_field_pair(model);
    let retry = retry::retry_config_from_config(config);
    let messages = [ChatRequestMessage::simple("user", prompt.to_string())];
    // The route's reasoning-echo policy shapes every message on the wire; a
    // prompt-only request carries no assistant message, so it is a no-op here,
    // but it must still go through the same lowering as every other body.
    let wire = super::chat_wire_messages(
        &messages,
        crate::catalog::chat_reasoning_echo(&config.provider_slug, model),
    );
    let body = serde_json::to_value(&ChatCompletionsRequest {
        model,
        messages: &wire.messages,
        tools: None,
        stream: false,
        stream_options: None,
        max_tokens: max_tokens_field,
        max_completion_tokens: max_completion_tokens_field,
        reasoning_effort: None,
    })
    .map_err(io::Error::other)?;
    // Hoist the no-op retry callback into a named local: a bare `&mut None`
    // temporary would be dropped before the retry call below (E0716).
    let mut no_retry = None;
    let mut ctx = retry::AttemptContext::new(&mut no_retry, cancel_rx, None);
    // No session identity on the plain prompt API — no gateway routing headers.
    let response = retry::retry_send(
        retry::RetrySend {
            agent,
            url: &url,
            auth,
            body: &body,
            config,
            retry_cfg: &retry,
            route: None,
            on_uploaded: None,
        },
        &mut ctx,
    )?;
    let payload: ChatCompletionsResponse = response
        .into_body()
        .read_json()
        .map_err(|e| super::OpenAiError::Io(io::Error::other(e)))?;

    let content = payload
        .choices
        .into_iter()
        .next()
        .and_then(|choice| choice.message.content)
        .unwrap_or_default()
        .trim()
        .to_string();

    if content.is_empty() {
        return Err(super::OpenAiError::EmptyResponse);
    }

    Ok(content)
}

// ── Non-streaming chat completions with tools ────────────────────────────

pub(crate) fn chat_completions_request_with_tools(
    agent: &ureq::Agent,
    config: &super::ServiceConfig,
    auth: &RequestAuth,
    params: super::TurnParams<'_>,
) -> Result<ChatTurnResult, super::OpenAiError> {
    let super::TurnParams {
        model,
        messages,
        tools,
        reasoning_effort,
        on_retry,
        on_uploaded,
        cancel_rx,
        route,
        max_output_tokens_override,
        no_retry,
        ..
    } = params;
    let start = std::time::Instant::now();
    let url = endpoint_url(&config.base_url, &config.chat_completions_path)?;
    let (max_tokens_field, max_completion_tokens_field) =
        config.max_tokens_field_pair_with_override(model, max_output_tokens_override);
    let retry = retry::retry_config_from_config(config).with_no_retry(no_retry);
    let wire = super::chat_wire_messages(
        messages,
        crate::catalog::chat_reasoning_echo(&config.provider_slug, model),
    );
    let body = serde_json::to_value(&ChatCompletionsRequest {
        model,
        messages: &wire.messages,
        tools: Some(tools),
        stream: false,
        stream_options: None,
        max_tokens: max_tokens_field,
        max_completion_tokens: max_completion_tokens_field,
        reasoning_effort,
    })
    .map_err(io::Error::other)?;
    // The upload hook fires once the payload is fully sent, so a non-streaming
    // provider reports the same `Waiting` boundary as the streaming path.
    let mut ctx = retry::AttemptContext::new(on_retry, cancel_rx, None);
    let response = retry::retry_send(
        retry::RetrySend {
            agent,
            url: &url,
            auth,
            body: &body,
            config,
            retry_cfg: &retry,
            route,
            on_uploaded,
        },
        &mut ctx,
    )?;
    let payload: ChatCompletionsResponse = response
        .into_body()
        .read_json()
        .map_err(|e| super::OpenAiError::Io(io::Error::other(e)))?;

    let elapsed = start.elapsed();
    debug!(
        model = %model,
        elapsed_ms = elapsed.as_millis(),
        prompt_tokens = payload.usage.as_ref().map(|u| u.prompt_tokens),
        completion_tokens = payload.usage.as_ref().map(|u| u.completion_tokens),
        total_tokens = payload.usage.as_ref().map(|u| u.total_tokens),
        "chat completion turn",
    );
    chat_completions_response_to_turn(payload)
}

/// Convert a parsed non-streaming chat completions response into a turn
/// result. The raw reasoning value is captured into the round-trip artifact
/// inside `AssistantMessage::take_reasoning`, before the field is consumed.
/// Terminal finish reasons must surface as distinct errors, not turn
/// results: a content-filter refusal (z.ai `sensitive` / `OpenAI`
/// `content_filter`) is never retryable, and a context-window overflow is a
/// compaction-bug signal. `Length` deliberately maps to NO error here — it is
/// a truncation notice carried on the `FinalText` result instead (and is normal
/// flow when tool calls are present).
fn finish_reason_terminal_error(reason: Option<&FinishReason>) -> Option<super::OpenAiError> {
    let reason = reason?;
    if reason.is_content_filtered() {
        // Follows the images adapter's ContentFiltered precedent: the HTTP
        // response itself succeeded, so no status rides along, and the
        // retry layer must never treat this as retryable.
        return Some(super::OpenAiError::ContentFiltered {
            detail: "provider content filter refused the response \
                     (finish_reason: sensitive/content_filter)"
                .into(),
        });
    }
    if reason.is_context_window_exceeded() {
        return Some(super::OpenAiError::ContextWindowExceeded {
            detail: "the prompt exceeded the model's context window \
                     (finish_reason: model_context_window_exceeded)"
                .into(),
        });
    }
    None
}

fn chat_completions_response_to_turn(
    payload: ChatCompletionsResponse,
) -> Result<ChatTurnResult, super::OpenAiError> {
    let Some(mut choice) = payload.choices.into_iter().next() else {
        return Err(super::OpenAiError::EmptyResponse);
    };

    // A declared terminal finish reason wins over whatever (possibly empty)
    // body the provider sent — z.ai's `sensitive` often arrives with an empty
    // content, which would otherwise look like a (retryable) empty response.
    if let Some(err) = finish_reason_terminal_error(choice.finish_reason.as_ref()) {
        return Err(err);
    }
    // `length` on a FINAL-TEXT turn means the answer was cut off; only the
    // FinalText paths below set the flag (ToolUse is normal tool-loop flow).
    let truncated = matches!(choice.finish_reason, Some(FinishReason::Length));

    // Extract reasoning early (before partial moves into tool_calls / content)
    let (reasoning, reasoning_artifact) = choice.message.take_reasoning();

    // Extract token usage from the API response for cost tracking / display.
    // z.ai reports cached prompt tokens via `prompt_tokens_details`; providers
    // without the details object yield 0.
    let turn_usage: Option<TokenUsage> = payload.usage.map(|u| {
        let cached_tokens = u.prompt_tokens_details.map_or(0, |d| d.cached_tokens);
        if cached_tokens > 0 {
            debug!(
                cached_tokens,
                prompt_tokens = u.prompt_tokens,
                "chat-completions cached prompt tokens"
            );
        }
        TokenUsage {
            input_tokens: u.prompt_tokens,
            output_tokens: u.completion_tokens,
            total_tokens: u.total_tokens,
            cached_tokens,
            cache_write_tokens: 0,
        }
    });

    let mut tool_calls: Vec<ChatToolCall> = choice
        .message
        .tool_calls
        .into_iter()
        .map(|tool_call| ChatToolCall {
            id: tool_call.id,
            name: tool_call.function.name,
            arguments_json: tool_call.function.arguments,
            caller: None,
        })
        .collect();
    let discarded = validate_tool_call_arguments(&mut tool_calls);
    if !tool_calls.is_empty() {
        return Ok(ChatTurnResult::ToolUse(ChatAssistantToolUse {
            content: choice.message.content,
            tool_calls,
            reasoning,
            usage: turn_usage,
            response_id: None,
            reasoning_artifact,
        }));
    }

    if !discarded.is_empty() {
        // Every tool call had invalid (truncated) arguments: the provider cut
        // the response off mid-call, so the call is unsafe to execute. If the
        // model also produced visible text, surface it — the casualty is the
        // tool call, not the answer. Otherwise there is nothing usable to
        // return, so signal a dedicated `TruncatedToolCall` instead of
        // fabricating an empty "final answer" that looks complete. This mirrors
        // the Responses adapter and the shared invariant that a length-
        // truncated tool call must never be laundered into a final turn — the
        // daemon special-cases `TruncatedToolCall` to tell the model what
        // happened and let it retry in smaller steps.
        let content = choice
            .message
            .content
            .unwrap_or_default()
            .trim()
            .to_string();
        if content.is_empty() {
            return Err(super::OpenAiError::TruncatedToolCall { discarded });
        }
        return Ok(ChatTurnResult::FinalText(FinalTextResult {
            content,
            truncated,
            reasoning,
            usage: turn_usage,
            response_id: None,
            reasoning_artifact,
        }));
    }

    let content = choice
        .message
        .content
        .unwrap_or_default()
        .trim()
        .to_string();
    // An encrypted-only reasoning_details turn has no visible text but IS model
    // output; a missing artifact is what makes this a genuine empty response.
    if content.is_empty() && reasoning_artifact.is_none() {
        return Err(super::OpenAiError::EmptyResponse);
    }

    Ok(ChatTurnResult::FinalText(FinalTextResult {
        content,
        truncated,
        reasoning,
        usage: turn_usage,
        response_id: None,
        reasoning_artifact,
    }))
}

// ── Simple streaming chat completions ────────────────────────────────────

pub(crate) fn chat_completions_request_streaming<F>(
    agent: &ureq::Agent,
    config: &super::ServiceConfig,
    auth: &RequestAuth,
    params: super::SimpleParams<'_>,
    on_event: &mut F,
) -> Result<(), super::OpenAiError>
where
    F: FnMut(StreamEvent) -> io::Result<()>,
{
    let super::SimpleParams {
        model,
        prompt,
        reasoning_effort,
        cancel_rx,
    } = params;
    let url = endpoint_url(&config.base_url, &config.chat_completions_path)?;
    let (max_tokens_field, max_completion_tokens_field) = config.max_tokens_field_pair(model);
    let retry = retry::retry_config_from_config(config);
    let messages = [ChatRequestMessage::simple("user", prompt.to_string())];
    let wire = super::chat_wire_messages(
        &messages,
        crate::catalog::chat_reasoning_echo(&config.provider_slug, model),
    );
    let body = serde_json::to_value(&ChatCompletionsRequest {
        model,
        messages: &wire.messages,
        tools: None,
        stream: true,
        stream_options: Some(ChatCompletionsStreamOptions {
            include_usage: true,
        }),
        max_tokens: max_tokens_field,
        max_completion_tokens: max_completion_tokens_field,
        reasoning_effort,
    })
    .map_err(io::Error::other)?;
    // Per-attempt wall-clock deadline spanning the whole request (see `retry::AttemptDeadline`).
    let mut deadline = retry::AttemptDeadline::new(config.total_timeout_secs);
    // Hoist the no-op retry callback into a named local: a bare `&mut None`
    // temporary would be dropped before the retry call below (E0716).
    let mut no_retry = None;
    let mut ctx = retry::AttemptContext::new(&mut no_retry, cancel_rx, Some(&mut deadline));
    // No session identity on the plain prompt API — no gateway routing headers.
    let response = retry::retry_send(
        retry::RetrySend {
            agent,
            url: &url,
            auth,
            body: &body,
            config,
            retry_cfg: &retry,
            route: None,
            on_uploaded: None,
        },
        &mut ctx,
    )?;
    let mut reader = SseReader::from_reader(response.into_body().into_reader());
    // The blocking socket read lives on a dedicated thread (see
    // `crate::stream`): `recv_sse_event` below is fully event-driven — it
    // `select_biased!`s on the event channel, the cancellation channel, and
    // (when a deadline is set) an exact timer, so an Escape during a stalled
    // stream is noticed the moment it is sent instead of on a poll tick.
    // Cancelling also arms the reader thread's abort flag, so it stops at its
    // next loop boundary instead of parsing the remainder of the stream.
    let context = crate::stream::SseContext {
        provider: config.provider_slug.clone(),
        model: model.to_owned(),
    };
    let sse =
        crate::stream::spawn_sse_reader(context, move || reader.next_event(), deadline.current());
    let mut has_any_output = false;
    // The capture only needs the derived fragments here (this path persists no
    // artifact); `details_streaming` latches on the first delta that leaves an
    // entry behind, and from then on the structured channel owns the live display.
    let mut details = DetailsCapture::default();
    let mut details_streaming = false;
    while let Some(data) = crate::stream::recv_sse_event(&sse, cancel_rx)? {
        let payload: ChatCompletionsStreamResponse =
            serde_json::from_str(&data).map_err(io::Error::other)?;
        for choice in payload.choices {
            let Some(delta) = choice.delta else {
                continue;
            };

            if let Some(raw) = delta.reasoning_details.as_ref() {
                push_details_fragments(&mut details, raw, on_event)?;
                if details.has_entries() {
                    details_streaming = true;
                    has_any_output = true;
                }
            }
            if let Some(content) = delta.content.filter(|c| !c.is_empty()) {
                has_any_output = true;
                on_event(StreamEvent::Answer(content))?;
            }
            for reasoning in [
                delta.reasoning_content,
                delta.reasoning,
                delta.reasoning_text,
            ]
            .into_iter()
            .flatten()
            .filter(|content| !content.is_empty())
            {
                has_any_output = true;
                // The scalar field and the structured array carry the same
                // content when a route populates both, so only one channel may
                // feed the live display: the structured entries, when the turn
                // has any, and the scalar field otherwise.
                if !details_streaming {
                    on_event(StreamEvent::Reasoning(reasoning))?;
                }
            }
        }
    }

    if !has_any_output {
        return Err(super::OpenAiError::EmptyResponse);
    }

    Ok(())
}

// ── Chat Completions tool call accumulation ─────────────────────────────

/// Accumulates tool call fields across streaming SSE chunks keyed by the
/// tool call index assigned by the API.
#[derive(Debug, Default)]
struct AccumulatingToolCall {
    id: Option<String>,
    name: Option<String>,
    arguments: String,
}

/// Accumulate tool call deltas from streaming SSE chunks into ordered tool
/// calls.  Deltas with the same index are combined — `id` and `name` are taken
/// from the last chunk that carries them, and `arguments` is concatenated.
pub(crate) fn accumulate_tool_calls_from_deltas(
    deltas: impl IntoIterator<Item = StreamToolCallDelta>,
) -> Vec<ChatToolCall> {
    let mut map: HashMap<u32, AccumulatingToolCall> = HashMap::new();
    for tc_delta in deltas {
        let entry = map.entry(tc_delta.index).or_default();
        if let Some(id) = tc_delta.id {
            entry.id = Some(id);
        }
        if let Some(func) = tc_delta.function {
            if let Some(name) = func.name {
                entry.name = Some(name);
            }
            if let Some(args) = func.arguments {
                entry.arguments.push_str(&args);
            }
        }
    }
    let mut calls: Vec<_> = map.into_iter().collect();
    calls.sort_by_key(|(idx, _)| *idx);
    calls
        .into_iter()
        .map(|(_, tc)| ChatToolCall {
            id: tc.id.unwrap_or_default(),
            name: tc.name.unwrap_or_default(),
            arguments_json: tc.arguments,
            caller: None,
        })
        .collect()
}

// ── Streaming chat completions with tools ───────────────────────────────

/// Mutable state accumulated across streamed chat-completions chunks.
///
/// Extracted from the streaming request loop so the parse→accumulate→turn
/// pipeline is unit-testable without an HTTP connection (mirrors the
/// `accumulate_tool_calls_from_deltas` helper above).
#[derive(Debug)]
struct ChatCompletionsStreamAccumulator {
    has_any_output: bool,
    full_content: String,
    full_reasoning: String,
    /// The reasoning field the stream is using, locked in on the first
    /// non-empty delta (`None` until then) — see [`ChatReasoningField`].
    /// Mirrors the non-streaming precedence (`reasoning_content` >
    /// `reasoning` > `reasoning_text` in `take_reasoning`) so the round-trip
    /// artifact is re-emitted to the SAME field the provider used — a stream
    /// that sends `reasoning_text` must not have it replayed as
    /// `reasoning_content` on the next tool-loop turn.
    reasoning_field: Option<ChatReasoningField>,
    /// The structured `reasoning_details` capture for this turn. Richest payload
    /// the chat wire can replay; when it holds entries it becomes the artifact.
    reasoning_details: DetailsCapture,
    /// Whether the structured `reasoning_details` channel has produced an entry
    /// this turn. Latched on the first delta that leaves one behind, and from
    /// then on it owns the live display: the scalar fields and the array carry
    /// the same content on a route that populates both, so emitting both would
    /// render the reasoning twice.
    details_streaming: bool,
    /// Raw tool call deltas across all chunks, merged by index by
    /// `accumulate_tool_calls_from_deltas` once the stream is fully consumed.
    raw_tool_call_deltas: Vec<StreamToolCallDelta>,
    seen_tool_call_indices: [bool; MAX_TOOL_CALLS],
    distinct_tool_call_count: usize,
    /// Usage from the final SSE chunk (`OpenAI` sends a usage chunk with
    /// choices: [] when `stream_options.include_usage` is true).
    last_usage: Option<TokenUsage>,
    /// Finish reason from the final SSE chunk (last non-None wins — the
    /// provider sends it exactly once, but tolerate repeats defensively).
    finish_reason: Option<FinishReason>,
}

impl Default for ChatCompletionsStreamAccumulator {
    fn default() -> Self {
        // Manual impl: std implements `Default` only for arrays up to `[T; 32]`
        // (const-generic array Default was never stabilized), so `[bool;
        // MAX_TOOL_CALLS]` (128) cannot be derived.
        Self {
            has_any_output: false,
            full_content: String::new(),
            full_reasoning: String::new(),
            reasoning_field: None,
            reasoning_details: DetailsCapture::default(),
            details_streaming: false,
            raw_tool_call_deltas: Vec::new(),
            seen_tool_call_indices: [false; MAX_TOOL_CALLS],
            distinct_tool_call_count: 0,
            last_usage: None,
            finish_reason: None,
        }
    }
}

impl ChatCompletionsStreamAccumulator {
    /// Fold one streamed chunk into the accumulated state, forwarding
    /// content/reasoning deltas to `on_event` immediately so subscribers
    /// see them in real time.
    fn apply(
        &mut self,
        payload: &ChatCompletionsStreamResponse,
        on_event: &mut impl FnMut(StreamEvent) -> io::Result<()>,
    ) -> Result<(), super::OpenAiError> {
        // Capture usage from the final chunk (choices: []).
        if let Some(ref u) = payload.usage {
            let cached_tokens = u
                .prompt_tokens_details
                .as_ref()
                .map_or(0, |d| d.cached_tokens);
            debug!(
                prompt_tokens = u.prompt_tokens,
                completion_tokens = u.completion_tokens,
                total_tokens = u.total_tokens,
                cached_tokens,
                "OpenAI streaming turn usage"
            );
            let usage = TokenUsage {
                input_tokens: u.prompt_tokens,
                output_tokens: u.completion_tokens,
                total_tokens: u.total_tokens,
                cached_tokens,
                cache_write_tokens: 0,
            };
            self.last_usage = Some(usage);
        }

        for choice in &payload.choices {
            // Capture the finish reason (arrives on the final chunk).
            if choice.finish_reason.is_some() {
                self.finish_reason.clone_from(&choice.finish_reason);
            }
            let Some(delta) = &choice.delta else {
                continue;
            };

            // Content chunks: answer text
            if let Some(content) = delta.content.as_ref().filter(|c| !c.is_empty()) {
                self.has_any_output = true;
                self.full_content.push_str(content);
                on_event(StreamEvent::Answer(content.clone()))?;
            }

            // Structured reasoning_details deltas come first so the structured
            // channel claims the turn's display before any scalar field is
            // considered: normalize the delta into the capture and forward each
            // new text/summary fragment as a display event. Any recognised entry
            // (even an encrypted-only one) means the model produced output, so
            // the turn is never reported as empty.
            if let Some(raw) = delta.reasoning_details.as_ref() {
                push_details_fragments(&mut self.reasoning_details, raw, on_event)?;
                if self.reasoning_details.has_entries() {
                    self.details_streaming = true;
                    self.has_any_output = true;
                }
            }

            // Scalar reasoning chunks — accumulate into `full_reasoning` (used
            // for both display and the round-trip artifact) and forward each
            // delta as a display event. The field is locked in on the first
            // non-empty delta (precedence reasoning_content > reasoning >
            // reasoning_text, matching the non-streaming `take_reasoning`
            // choice), so the artifact is re-emitted to the SAME field the
            // provider used; a stray delta from a different field is ignored
            // rather than mixed into the blob.
            for (field, text) in [
                (
                    ChatReasoningField::ReasoningContent,
                    &delta.reasoning_content,
                ),
                (ChatReasoningField::Reasoning, &delta.reasoning),
                (ChatReasoningField::ReasoningText, &delta.reasoning_text),
            ] {
                let Some(text) = text.as_deref().filter(|t| !t.is_empty()) else {
                    continue;
                };
                self.has_any_output = true;
                let chosen = self.reasoning_field.get_or_insert(field);
                if *chosen != field {
                    // Stray delta from a non-primary field — don't display or
                    // capture it (the artifact must stay field-pure so it
                    // replays to the correct wire field).
                    continue;
                }
                // The scalar text is always accumulated: it is the display
                // fallback for a turn whose details yield none and the artifact
                // fallback when no details were captured, and a route that
                // streams only the scalar field is the other, still-supported
                // shape. Only the live event is withheld while the structured
                // channel is active, because the two carry the same content and
                // emitting both renders the reasoning twice.
                self.full_reasoning.push_str(text);
                if !self.details_streaming {
                    on_event(StreamEvent::Reasoning(text.to_string()))?;
                }
            }

            // Collect raw tool call deltas — the shared accumulator
            // (accumulate_tool_calls_from_deltas) will merge them by index
            // and produce sorted ChatToolCall output after the stream ends.
            if let Some(ref tcs) = delta.tool_calls {
                self.has_any_output = true;
                for tc in tcs {
                    if self.distinct_tool_call_count >= MAX_TOOL_CALLS {
                        return Err(super::OpenAiError::Io(io::Error::other(format!(
                            "too many tool calls (max {MAX_TOOL_CALLS})"
                        ))));
                    }
                    if (tc.index as usize) >= MAX_TOOL_CALLS {
                        return Err(super::OpenAiError::Io(io::Error::other(format!(
                            "tool call index {} out of bounds (max {})",
                            tc.index,
                            MAX_TOOL_CALLS - 1,
                        ))));
                    }
                    // The bounds check above guarantees `tc.index` is within
                    // the fixed-size array, so this lookup always succeeds;
                    // the None arm is unreachable.
                    if let Some(seen) = self.seen_tool_call_indices.get_mut(tc.index as usize)
                        && !*seen
                    {
                        *seen = true;
                        self.distinct_tool_call_count += 1;
                    }
                    self.raw_tool_call_deltas.push(tc.clone());
                }
            }
        }
        Ok(())
    }

    /// Finalize the accumulated stream into a turn result, attaching the
    /// `ChatReasoning` artifact captured from the accumulated reasoning
    /// deltas (the concatenated deltas of the locked-in field are the
    /// provider's raw reasoning value, echoed verbatim — to the same field —
    /// on tool-loop turns).
    fn into_turn_result(self) -> Result<ChatTurnResult, super::OpenAiError> {
        // A declared terminal finish reason wins over the accumulated body:
        // z.ai's `sensitive` streams no content, which would otherwise fall
        // through to the (retryable-looking) EmptyResponse error.
        if let Some(err) = finish_reason_terminal_error(self.finish_reason.as_ref()) {
            return Err(err);
        }
        if !self.has_any_output {
            return Err(super::OpenAiError::EmptyResponse);
        }
        // `length` on a FINAL-TEXT turn is a truncation notice, not an error;
        // only the FinalText paths below carry the flag.
        let truncated = matches!(self.finish_reason, Some(FinishReason::Length));

        // The field was locked in on the first non-empty delta; the artifact
        // carries that field so re-emission targets the same wire field. A
        // captured structured details payload outranks the scalar text (it is
        // the richer payload the route replays), and its text/summary entries
        // supply the display fallback when no scalar reasoning streamed.
        let details_display = self.reasoning_details.display();
        let reasoning_artifact = self.reasoning_details.finish().or_else(|| {
            self.reasoning_field
                .and_then(|field| chat_reasoning_artifact(field, &self.full_reasoning))
        });
        let display_reasoning = if self.full_reasoning.is_empty() {
            details_display
        } else {
            self.full_reasoning.clone()
        };
        let display_reasoning = (!display_reasoning.is_empty()).then_some(display_reasoning);

        if !self.raw_tool_call_deltas.is_empty() {
            let mut tool_calls = accumulate_tool_calls_from_deltas(self.raw_tool_call_deltas);
            let discarded = validate_tool_call_arguments(&mut tool_calls);
            if !tool_calls.is_empty() {
                return Ok(ChatTurnResult::ToolUse(ChatAssistantToolUse {
                    content: if self.full_content.is_empty() {
                        None
                    } else {
                        Some(self.full_content)
                    },
                    tool_calls,
                    reasoning: display_reasoning.clone(),
                    usage: self.last_usage,
                    response_id: None,
                    reasoning_artifact,
                }));
            }
            if !discarded.is_empty() {
                // Every accumulated tool call had invalid (truncated)
                // arguments. If the model also streamed visible text, return it
                // (the tool call is the casualty, not the answer); otherwise
                // surface `TruncatedToolCall` so the daemon can recover instead
                // of rendering an empty, truncated-looking final answer. Same
                // invariant and shape as the non-streaming path above.
                if self.full_content.is_empty() {
                    return Err(super::OpenAiError::TruncatedToolCall { discarded });
                }
                return Ok(ChatTurnResult::FinalText(FinalTextResult {
                    content: self.full_content,
                    truncated,
                    reasoning: display_reasoning.clone(),
                    usage: self.last_usage,
                    response_id: None,
                    reasoning_artifact,
                }));
            }
        }

        Ok(ChatTurnResult::FinalText(FinalTextResult {
            content: self.full_content,
            truncated,
            reasoning: display_reasoning,
            usage: self.last_usage,
            response_id: None,
            reasoning_artifact,
        }))
    }
}

/// Streaming variant of `chat_completions_request_with_tools`.
///
/// Sends `stream: true` with tool definitions, reads SSE chunks, and calls
/// `on_chunk` for each content / reasoning delta so the caller can forward
/// it to subscribers immediately.  Tool call deltas are accumulated across
/// chunks and returned as `ChatTurnResult::ToolUse` when the stream ends.
pub(crate) fn chat_completions_request_streaming_with_tools<F>(
    agent: &ureq::Agent,
    config: &super::ServiceConfig,
    auth: &RequestAuth,
    params: super::TurnParams<'_>,
    on_event: &mut F,
) -> Result<ChatTurnResult, super::OpenAiError>
where
    F: FnMut(StreamEvent) -> io::Result<()>,
{
    let super::TurnParams {
        model,
        messages,
        tools,
        reasoning_effort,
        on_retry,
        on_uploaded,
        cancel_rx,
        route,
        max_output_tokens_override,
        no_retry,
        ..
    } = params;
    let url = endpoint_url(&config.base_url, &config.chat_completions_path)?;
    let (max_tokens_field, max_completion_tokens_field) =
        config.max_tokens_field_pair_with_override(model, max_output_tokens_override);
    let retry = retry::retry_config_from_config(config).with_no_retry(no_retry);
    let wire = super::chat_wire_messages(
        messages,
        crate::catalog::chat_reasoning_echo(&config.provider_slug, model),
    );
    let body = serde_json::to_value(&ChatCompletionsRequest {
        model,
        messages: &wire.messages,
        tools: Some(tools),
        stream: true,
        // Configurable stream_options — some OpenAI-compatible providers
        // reject the `stream_options` field entirely, so users can disable
        // it per-account to maintain compatibility.
        stream_options: if config.stream_options {
            Some(ChatCompletionsStreamOptions {
                include_usage: true,
            })
        } else {
            None
        },
        max_tokens: max_tokens_field,
        max_completion_tokens: max_completion_tokens_field,
        reasoning_effort,
    })
    .map_err(io::Error::other)?;
    // Per-attempt wall-clock deadline spanning the whole request (see `retry::AttemptDeadline`).
    let mut deadline = retry::AttemptDeadline::new(config.total_timeout_secs);
    let mut ctx = retry::AttemptContext::new(on_retry, cancel_rx, Some(&mut deadline));
    let response = retry::retry_send(
        retry::RetrySend {
            agent,
            url: &url,
            auth,
            body: &body,
            config,
            retry_cfg: &retry,
            route,
            on_uploaded,
        },
        &mut ctx,
    )?;
    let mut reader = SseReader::from_reader(response.into_body().into_reader());
    let mut acc = ChatCompletionsStreamAccumulator::default();
    // Reader thread decouples the blocking socket read from cancellation
    // polling (see `crate::stream`); the abort flag on `sse` stops the thread
    // at its next loop boundary once the consumer cancels or drops it.
    let context = crate::stream::SseContext {
        provider: config.provider_slug.clone(),
        model: model.to_owned(),
    };
    let sse =
        crate::stream::spawn_sse_reader(context, move || reader.next_event(), deadline.current());
    while let Some(data) = crate::stream::recv_sse_event(&sse, cancel_rx)? {
        let payload: ChatCompletionsStreamResponse =
            serde_json::from_str(&data).map_err(io::Error::other)?;
        acc.apply(&payload, on_event)?;
    }

    acc.into_turn_result()
}

// The test suite lives in a sibling file so the implementation stays
// readable at a glance; the module path is unchanged, so the moved tests
// reach their subjects through the same `super` paths.
#[cfg(test)]
mod tests;
