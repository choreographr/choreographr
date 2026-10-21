use serde::{Deserialize, Serialize};
use tracing::debug;

use super::retry;
use super::{
    ChatRequestMessage, ChatToolDefinition, SseReader, endpoint_url, validate_tool_call_arguments,
};
use crate::shared::RequestAuth;
use crate::types::{
    ChatAssistantToolUse, ChatToolCall, ChatTurnResult, FinalTextResult, StreamEvent,
};
use choreo_proto::{ChatReasoningField, ReasoningArtifact, TokenUsage};
use std::io;

mod accumulator;
mod reasoning_stream;

use accumulator::ChatCompletionsStreamAccumulator;
#[cfg(test)]
// The sibling test suite exercises the delta merger on its own, so it is in
// scope here (the request path only reaches it through the accumulator).
use accumulator::accumulate_tool_calls_from_deltas;
use reasoning_stream::ReasoningStreamState;

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
    /// Returns the display text alongside an opaque [`ReasoningArtifact`], which
    /// records WHICH field the payload was observed under so re-emission targets
    /// the same wire field — a provider that sends `reasoning_text` must not have
    /// its payload echoed back as `reasoning_content` on the next tool-loop turn.
    pub(crate) fn take_reasoning(&mut self) -> (Option<String>, Option<ReasoningArtifact>) {
        let mut state = ReasoningStreamState::default();
        let delta = StreamDelta {
            content: None,
            tool_calls: None,
            reasoning_content: self.reasoning_content.take(),
            reasoning: self.reasoning.take(),
            reasoning_text: self.reasoning_text.take(),
            reasoning_details: self.reasoning_details.take(),
        };
        // The fold cannot fail with a no-op callback; its produced flag is the
        // streaming caller's concern, not the non-streaming result's.
        let _ = state.apply(&delta, &mut |_| Ok(()));
        state.finish()
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

#[derive(Debug, Deserialize)]
pub(crate) struct ChatCompletionsStreamResponse {
    pub(crate) choices: Vec<StreamChoice>,
    /// Token usage, present only on the final chunk of a stream that reports it.
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
    // `length` on a FINAL-TEXT turn means the answer was cut off; the tool-call
    // arm ignores it (a length-truncated tool call is normal tool-loop flow).
    let truncated = matches!(choice.finish_reason, Some(FinishReason::Length));

    // Extract reasoning early (before the partial moves into tool_calls / content)
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

    let tool_calls: Vec<ChatToolCall> = choice
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

    // A tool-call message keeps the provider's raw `content` Option; the
    // final-text content is trimmed (a blank answer reads as none), and the
    // empty-response guard keys on that trimmed text.
    let tool_use_content = choice.message.content;
    let final_text = tool_use_content
        .as_deref()
        .unwrap_or_default()
        .trim()
        .to_string();
    let content_is_empty = final_text.is_empty();

    assemble_turn(ChatTurnParts {
        tool_use_content,
        final_text,
        content_is_empty,
        tool_calls,
        reasoning,
        reasoning_artifact,
        usage: turn_usage,
        truncated,
    })
}

/// The finalized parts of one chat turn, handed to [`assemble_turn`].
///
/// Bundled so the assembler takes one value instead of a long positional list
/// (no `too_many_arguments` suppression) and each caller names every part
/// explicitly.
struct ChatTurnParts {
    /// Content to attach to a `ToolUse` result (the provider's raw option for
    /// the non-streaming path, `None`-when-empty for the streaming path).
    tool_use_content: Option<String>,
    /// Content to attach to a `FinalText` result (already normalized by the
    /// caller: trimmed for non-streaming, the accumulated text for streaming).
    final_text: String,
    /// Whether the turn's visible content is empty (drives the empty-response
    /// and truncated-tool-call guards).
    content_is_empty: bool,
    /// The turn's tool calls, collected but not yet validated.
    tool_calls: Vec<ChatToolCall>,
    /// The turn's display reasoning text.
    reasoning: Option<String>,
    /// The opaque round-trip reasoning artifact.
    reasoning_artifact: Option<ReasoningArtifact>,
    /// Token usage reported for the turn, when the provider sent any.
    usage: Option<TokenUsage>,
    /// Whether `finish_reason` was `length` on a final-text turn.
    truncated: bool,
}

/// Assemble a chat turn result from its finalized parts.
///
/// Shared by the non-streaming response conversion and the streaming
/// accumulator so the `ToolUse` / truncated-tool-call / final-text decision tree
/// exists exactly once. A message with empty content, no tool calls, and no
/// reasoning artifact is a genuine empty response.
fn assemble_turn(parts: ChatTurnParts) -> Result<ChatTurnResult, super::OpenAiError> {
    let ChatTurnParts {
        tool_use_content,
        final_text,
        content_is_empty,
        mut tool_calls,
        reasoning,
        reasoning_artifact,
        usage,
        truncated,
    } = parts;
    // Truncated / malformed tool calls are dropped here, once, for both paths.
    let discarded = validate_tool_call_arguments(&mut tool_calls);
    if !tool_calls.is_empty() {
        return Ok(ChatTurnResult::ToolUse(ChatAssistantToolUse {
            content: tool_use_content,
            tool_calls,
            reasoning,
            usage,
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
        // fabricating an empty "final answer" that looks complete — the daemon
        // special-cases it to tell the model what happened and let it retry in
        // smaller steps.
        if content_is_empty {
            return Err(super::OpenAiError::TruncatedToolCall { discarded });
        }
        return Ok(ChatTurnResult::FinalText(FinalTextResult {
            content: final_text,
            truncated,
            reasoning,
            usage,
            response_id: None,
            reasoning_artifact,
        }));
    }
    // An encrypted-only (or unrecognised-only) reasoning turn has no visible
    // text but IS model output; a missing artifact is what makes this a genuine
    // empty response.
    if content_is_empty && reasoning_artifact.is_none() {
        return Err(super::OpenAiError::EmptyResponse);
    }
    Ok(ChatTurnResult::FinalText(FinalTextResult {
        content: final_text,
        truncated,
        reasoning,
        usage,
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
    // One shared state machine owns the turn's reasoning display (and, on the
    // tool path, its artifact); this path persists no artifact, so `finish` is
    // never called.
    let mut reasoning = ReasoningStreamState::default();
    while let Some(data) = crate::stream::recv_sse_event(&sse, cancel_rx)? {
        let payload: ChatCompletionsStreamResponse =
            serde_json::from_str(&data).map_err(io::Error::other)?;
        for choice in payload.choices {
            let Some(delta) = choice.delta else {
                continue;
            };

            // Answer before reasoning, matching the tool accumulator and the
            // non-streaming bridge (`emit_non_streaming_events`): the reply is
            // the primary content, and the client renders it above a
            // collapsible reasoning section regardless of arrival order.
            if let Some(content) = delta.content.as_ref().filter(|c| !c.is_empty()) {
                has_any_output = true;
                on_event(StreamEvent::Answer(content.clone()))?;
            }
            has_any_output |= reasoning.apply(&delta, on_event)?;
        }
    }

    if !has_any_output {
        return Err(super::OpenAiError::EmptyResponse);
    }

    Ok(())
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
