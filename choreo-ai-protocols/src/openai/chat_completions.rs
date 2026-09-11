use serde::{Deserialize, Serialize};
use tracing::debug;

use super::retry;
use super::{
    ChatRequestMessage, ChatToolDefinition, SseReader, endpoint_url, validate_tool_call_arguments,
};
use crate::shared::MAX_TOOL_CALLS;
use crate::types::{
    ChatAssistantToolUse, ChatToolCall, ChatTurnResult, FinalTextResult, StreamEvent,
};
use choreo_proto::{ChatReasoningField, ReasoningArtifact, TokenUsage};
use std::collections::HashMap;
use std::io;

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
}

impl AssistantMessage {
    /// Extract reasoning content from whichever field the model populated
    /// (reasoning_content, reasoning, or reasoning_text), consuming the field
    /// with the non-streaming precedence reasoning_content > reasoning >
    /// reasoning_text.
    ///
    /// Returns the display text alongside an opaque [`ReasoningArtifact`]
    /// capturing the same raw value as UTF-8 bytes. The artifact records
    /// WHICH field was consumed, so re-emission targets the same wire field
    /// the provider used — a provider that sends `reasoning_text` must not
    /// have its payload echoed back as `reasoning_content` on the next
    /// tool-loop turn.
    pub(crate) fn take_reasoning(&mut self) -> (Option<String>, Option<ReasoningArtifact>) {
        // Consume the fields in precedence order and record the winner; the
        // artifact is built from the value BEFORE it leaves `self`, so the
        // round-trip payload survives the parse boundary untouched.
        if let Some(text) = self.reasoning_content.take() {
            let artifact = chat_reasoning_artifact(ChatReasoningField::ReasoningContent, &text);
            return (Some(text), artifact);
        }
        if let Some(text) = self.reasoning.take() {
            let artifact = chat_reasoning_artifact(ChatReasoningField::Reasoning, &text);
            return (Some(text), artifact);
        }
        if let Some(text) = self.reasoning_text.take() {
            let artifact = chat_reasoning_artifact(ChatReasoningField::ReasoningText, &text);
            return (Some(text), artifact);
        }
        (None, None)
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
}

#[derive(Debug, Default, Clone, Deserialize)]
pub(crate) struct StreamToolCallDelta {
    pub(crate) index: u32,
    pub(crate) id: Option<String>,
    // Deserialised from the API's "type" field but never read in Rust — kept
    // so serde doesn't choke on unknown fields and to document the wire format.
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
/// `model_context_window_exceeded`, `network_error`) plus OpenAI's aliases
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
    /// OpenAI `content_filter`): terminal and never retryable — resending
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
    api_key: &str,
    model: &str,
    prompt: &str,
    cancel_rx: Option<&crossbeam_channel::Receiver<()>>,
) -> Result<String, super::OpenAiError> {
    let url = endpoint_url(&config.base_url, &config.chat_completions_path)?;
    let (max_tokens_field, max_completion_tokens_field) = config.max_tokens_field_pair(model);
    let retry = retry::retry_config_from_config(config);
    let messages = [ChatRequestMessage::simple("user", prompt.to_string())];
    let body = serde_json::to_value(&ChatCompletionsRequest {
        model,
        messages: &messages,
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
    let response = retry::retry_send(agent, &url, api_key, &body, config, &retry, &mut ctx, None)?;
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

#[expect(clippy::too_many_arguments)]
pub(crate) fn chat_completions_request_with_tools(
    agent: &ureq::Agent,
    config: &super::ServiceConfig,
    api_key: &str,
    model: &str,
    messages: &[ChatRequestMessage],
    tools: &[ChatToolDefinition],
    reasoning_effort: Option<&str>,
    on_retry: &mut Option<retry::RetryCallback>,
    cancel_rx: Option<&crossbeam_channel::Receiver<()>>,
    // Gateway routing identity (session_id, request_id) for the opencode
    // zen/go providers; `None` when the caller has no session (prompt API).
    route: Option<(&str, &str)>,
) -> Result<ChatTurnResult, super::OpenAiError> {
    let start = std::time::Instant::now();
    let url = endpoint_url(&config.base_url, &config.chat_completions_path)?;
    let (max_tokens_field, max_completion_tokens_field) = config.max_tokens_field_pair(model);
    let retry = retry::retry_config_from_config(config);
    let body = serde_json::to_value(&ChatCompletionsRequest {
        model,
        messages,
        tools: Some(tools),
        stream: false,
        stream_options: None,
        max_tokens: max_tokens_field,
        max_completion_tokens: max_completion_tokens_field,
        reasoning_effort,
    })
    .map_err(io::Error::other)?;
    let mut ctx = retry::AttemptContext::new(on_retry, cancel_rx, None);
    let response = retry::retry_send(agent, &url, api_key, &body, config, &retry, &mut ctx, route)?;
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
/// results: a content-filter refusal (z.ai `sensitive` / OpenAI
/// `content_filter`) is never retryable, and a context-window overflow is a
/// compaction-bug signal. `Length` deliberately maps to NO error here — it is
/// a truncation notice carried on the FinalText result instead (and is normal
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
        // All calls had invalid arguments. Return the text if the model
        // produced any, so the session continues gracefully and the LLM
        // can retry with valid arguments on the next turn.
        let content = choice
            .message
            .content
            .unwrap_or_default()
            .trim()
            .to_string();
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
    if content.is_empty() {
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

#[expect(clippy::too_many_arguments)]
pub(crate) fn chat_completions_request_streaming<F>(
    agent: &ureq::Agent,
    config: &super::ServiceConfig,
    api_key: &str,
    model: &str,
    prompt: &str,
    reasoning_effort: Option<&str>,
    cancel_rx: Option<&crossbeam_channel::Receiver<()>>,
    on_event: &mut F,
) -> Result<(), super::OpenAiError>
where
    F: FnMut(StreamEvent) -> io::Result<()>,
{
    let url = endpoint_url(&config.base_url, &config.chat_completions_path)?;
    let (max_tokens_field, max_completion_tokens_field) = config.max_tokens_field_pair(model);
    let retry = retry::retry_config_from_config(config);
    let messages = [ChatRequestMessage::simple("user", prompt.to_string())];
    let body = serde_json::to_value(&ChatCompletionsRequest {
        model,
        messages: &messages,
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
    let response = retry::retry_send(agent, &url, api_key, &body, config, &retry, &mut ctx, None)?;
    let mut reader = SseReader::from_reader(response.into_body().into_reader());
    // The blocking socket read lives on a dedicated thread (see
    // `crate::stream`): `recv_sse_event` below is fully event-driven — it
    // `select_biased!`s on the event channel, the cancellation channel, and
    // (when a deadline is set) an exact timer, so an Escape during a stalled
    // stream is noticed the moment it is sent instead of on a poll tick.
    // Cancelling also arms the reader thread's abort flag, so it stops at its
    // next loop boundary instead of parsing the remainder of the stream.
    let sse = crate::stream::spawn_sse_reader(move || reader.next_event(), deadline.current());
    let mut has_any_output = false;
    while let Some(data) = crate::stream::recv_sse_event(&sse, cancel_rx)? {
        let payload: ChatCompletionsStreamResponse =
            serde_json::from_str(&data).map_err(io::Error::other)?;
        for choice in payload.choices {
            let Some(delta) = choice.delta else {
                continue;
            };

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
                on_event(StreamEvent::Reasoning(reasoning))?;
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
    /// Raw tool call deltas across all chunks, merged by index by
    /// `accumulate_tool_calls_from_deltas` once the stream is fully consumed.
    raw_tool_call_deltas: Vec<StreamToolCallDelta>,
    seen_tool_call_indices: [bool; MAX_TOOL_CALLS],
    distinct_tool_call_count: usize,
    /// Usage from the final SSE chunk (OpenAI sends a usage chunk with
    /// choices: [] when stream_options.include_usage is true).
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
            };
            self.last_usage = Some(usage);
        }

        for choice in &payload.choices {
            // Capture the finish reason (arrives on the final chunk).
            if choice.finish_reason.is_some() {
                self.finish_reason = choice.finish_reason.clone();
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

            // Reasoning chunks — accumulate into `full_reasoning` (used for
            // both display and the round-trip artifact) and forward each
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
                self.full_reasoning.push_str(text);
                on_event(StreamEvent::Reasoning(text.to_string()))?;
            }

            // Collect raw tool call deltas — the shared accumulator
            // (accumulate_tool_calls_from_deltas) will merge them by index
            // and produce sorted ChatToolCall output after the stream ends.
            if let Some(ref tcs) = delta.tool_calls {
                self.has_any_output = true;
                for tc in tcs.iter() {
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
                    if let Some(seen) = self.seen_tool_call_indices.get_mut(tc.index as usize) {
                        if !*seen {
                            *seen = true;
                            self.distinct_tool_call_count += 1;
                        }
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
        // carries that field so re-emission targets the same wire field.
        let reasoning_artifact = self
            .reasoning_field
            .and_then(|field| chat_reasoning_artifact(field, &self.full_reasoning));

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
                    reasoning: if self.full_reasoning.is_empty() {
                        None
                    } else {
                        Some(self.full_reasoning)
                    },
                    usage: self.last_usage,
                    response_id: None,
                    reasoning_artifact,
                }));
            }
            if !discarded.is_empty() {
                // All calls had invalid arguments. Return accumulated text so
                // the session can continue gracefully.
                return Ok(ChatTurnResult::FinalText(FinalTextResult {
                    content: self.full_content,
                    truncated,
                    reasoning: if self.full_reasoning.is_empty() {
                        None
                    } else {
                        Some(self.full_reasoning)
                    },
                    usage: self.last_usage,
                    response_id: None,
                    reasoning_artifact,
                }));
            }
        }

        Ok(ChatTurnResult::FinalText(FinalTextResult {
            content: self.full_content,
            truncated,
            reasoning: if self.full_reasoning.is_empty() {
                None
            } else {
                Some(self.full_reasoning)
            },
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
#[expect(clippy::too_many_arguments)]
pub(crate) fn chat_completions_request_streaming_with_tools<F>(
    agent: &ureq::Agent,
    config: &super::ServiceConfig,
    api_key: &str,
    model: &str,
    messages: &[ChatRequestMessage],
    tools: &[ChatToolDefinition],
    reasoning_effort: Option<&str>,
    on_retry: &mut Option<retry::RetryCallback>,
    cancel_rx: Option<&crossbeam_channel::Receiver<()>>,
    // Gateway routing identity (session_id, request_id) for the opencode
    // zen/go providers; `None` when the caller has no session (prompt API).
    route: Option<(&str, &str)>,
    on_event: &mut F,
) -> Result<ChatTurnResult, super::OpenAiError>
where
    F: FnMut(StreamEvent) -> io::Result<()>,
{
    let url = endpoint_url(&config.base_url, &config.chat_completions_path)?;
    let (max_tokens_field, max_completion_tokens_field) = config.max_tokens_field_pair(model);
    let retry = retry::retry_config_from_config(config);
    let body = serde_json::to_value(&ChatCompletionsRequest {
        model,
        messages,
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
    let response = retry::retry_send(agent, &url, api_key, &body, config, &retry, &mut ctx, route)?;
    let mut reader = SseReader::from_reader(response.into_body().into_reader());
    let mut acc = ChatCompletionsStreamAccumulator::default();
    // Reader thread decouples the blocking socket read from cancellation
    // polling (see `crate::stream`); the abort flag on `sse` stops the thread
    // at its next loop boundary once the consumer cancels or drops it.
    let sse = crate::stream::spawn_sse_reader(move || reader.next_event(), deadline.current());
    while let Some(data) = crate::stream::recv_sse_event(&sse, cancel_rx)? {
        let payload: ChatCompletionsStreamResponse =
            serde_json::from_str(&data).map_err(io::Error::other)?;
        acc.apply(&payload, on_event)?;
    }

    acc.into_turn_result()
}

#[cfg(test)]
mod tests {
    use super::*;

    // -- finish_reason lenient deserialization -----------------------------

    #[test]
    fn finish_reason_parses_known_values() {
        for (raw, expected) in [
            ("stop", FinishReason::Stop),
            ("tool_calls", FinishReason::ToolCalls),
            ("length", FinishReason::Length),
            ("sensitive", FinishReason::Sensitive),
            (
                "model_context_window_exceeded",
                FinishReason::ContextWindowExceeded,
            ),
            ("network_error", FinishReason::NetworkError),
        ] {
            let parsed: FinishReason = serde_json::from_value(serde_json::json!(raw))
                .unwrap_or_else(|e| panic!("parse {raw}: {e}"));
            assert_eq!(parsed, expected, "raw {raw}");
        }
    }

    #[test]
    fn finish_reason_maps_openai_aliases() {
        // OpenAI's legacy `function_call` and its `content_filter` must map
        // to the shared variants so behavior is provider-portable.
        let function_call: FinishReason =
            serde_json::from_value(serde_json::json!("function_call")).unwrap();
        assert_eq!(function_call, FinishReason::ToolCalls);
        let content_filter: FinishReason =
            serde_json::from_value(serde_json::json!("content_filter")).unwrap();
        assert_eq!(content_filter, FinishReason::Sensitive);
        assert!(content_filter.is_content_filtered());
    }

    #[test]
    fn finish_reason_unknown_value_maps_to_other_not_parse_failure() {
        // A provider adding a new finish reason must degrade to Other(raw),
        // never fail the whole response parse.
        let parsed: FinishReason =
            serde_json::from_value(serde_json::json!("some_future_reason")).unwrap();
        assert_eq!(
            parsed,
            FinishReason::Other("some_future_reason".to_string())
        );
        // Not terminal in any way.
        assert!(!parsed.is_content_filtered());
        assert!(!parsed.is_context_window_exceeded());
    }

    #[test]
    fn choice_without_finish_reason_parses_to_none() {
        let payload: ChatCompletionsResponse =
            serde_json::from_str(r#"{"choices":[{"message":{"content":"hi"}}],"usage":null}"#)
                .unwrap();
        let choice = payload.choices.into_iter().next().unwrap();
        assert!(choice.finish_reason.is_none());
    }

    #[test]
    fn stream_choice_parses_finish_reason_on_final_chunk() {
        let payload: ChatCompletionsStreamResponse =
            serde_json::from_str(r#"{"choices":[{"delta":{},"finish_reason":"length"}]}"#).unwrap();
        let choice = payload.choices.into_iter().next().unwrap();
        assert_eq!(choice.finish_reason, Some(FinishReason::Length));
    }

    // -- finish_reason behavior mapping (non-streaming) ---------------------

    #[test]
    fn length_on_final_text_sets_truncation_flag() {
        let json = r#"{"choices":[{"message":{"content":"partial answer"},"finish_reason":"length"}],"usage":null}"#;
        let payload: ChatCompletionsResponse = serde_json::from_str(json).unwrap();
        let turn = chat_completions_response_to_turn(payload).unwrap();
        let ChatTurnResult::FinalText(result) = turn else {
            panic!("expected FinalText");
        };
        assert!(result.truncated, "length on final text must set truncated");
        assert_eq!(result.content, "partial answer");
    }

    #[test]
    fn length_on_tool_use_does_not_set_truncation_flag() {
        // Tool calls + length is normal tool-loop flow, not a user-visible
        // truncation: the ToolUse result carries no truncation semantics.
        let json = r#"{"choices":[{"message":{
            "content":null,
            "tool_calls":[{"id":"call_1","type":"function","function":{"name":"get_weather","arguments":"{\"city\":\"London\"}"}}],
            "reasoning_content":null,"reasoning":null,"reasoning_text":null
        },"finish_reason":"length"}],"usage":null}"#;
        let payload: ChatCompletionsResponse = serde_json::from_str(json).unwrap();
        let turn = chat_completions_response_to_turn(payload).unwrap();
        assert!(matches!(turn, ChatTurnResult::ToolUse(_)));
    }

    #[test]
    fn stop_keeps_behavior_unchanged() {
        let json =
            r#"{"choices":[{"message":{"content":"done"},"finish_reason":"stop"}],"usage":null}"#;
        let payload: ChatCompletionsResponse = serde_json::from_str(json).unwrap();
        let turn = chat_completions_response_to_turn(payload).unwrap();
        let ChatTurnResult::FinalText(result) = turn else {
            panic!("expected FinalText");
        };
        assert!(!result.truncated);
        assert_eq!(result.content, "done");
    }

    #[test]
    fn sensitive_is_terminal_content_filtered_error() {
        // z.ai's `sensitive` refusal often arrives with EMPTY content — it
        // must surface as the terminal ContentFiltered error, not as an
        // (empty-response) error that could be retried.
        let json = r#"{"choices":[{"message":{"content":null,"tool_calls":[],"reasoning_content":null,"reasoning":null,"reasoning_text":null},"finish_reason":"sensitive"}],"usage":null}"#;
        let payload: ChatCompletionsResponse = serde_json::from_str(json).unwrap();
        let err = chat_completions_response_to_turn(payload).unwrap_err();
        assert!(
            matches!(err, crate::openai::OpenAiError::ContentFiltered { .. }),
            "expected ContentFiltered, got {err:?}"
        );
        // And the OpenAI alias behaves identically.
        let json = r#"{"choices":[{"message":{"content":null},"finish_reason":"content_filter"}],"usage":null}"#;
        let payload: ChatCompletionsResponse = serde_json::from_str(json).unwrap();
        let err = chat_completions_response_to_turn(payload).unwrap_err();
        assert!(matches!(
            err,
            crate::openai::OpenAiError::ContentFiltered { .. }
        ));
    }

    #[test]
    fn sensitive_maps_to_non_retryable_inference_error() {
        // The retryability contract: the mapped InferenceError must be the
        // ContentFiltered variant, which the daemon never retries (it only
        // special-cases Cancelled and TruncatedToolCall; the HTTP retry layer
        // only retries 429/5xx status codes, and this error is produced after
        // a successful 200 parse).
        let err = crate::openai::OpenAiError::ContentFiltered {
            detail: "blocked".into(),
        };
        let inference = crate::shared::provider_error_to_inference(err);
        assert!(matches!(
            inference,
            choreo_proto::InferenceError::ContentFiltered { .. }
        ));
        assert_eq!(inference.metric_label(), "content_filtered");
    }

    #[test]
    fn context_window_exceeded_is_terminal_error() {
        let json = r#"{"choices":[{"message":{"content":null},"finish_reason":"model_context_window_exceeded"}],"usage":null}"#;
        let payload: ChatCompletionsResponse = serde_json::from_str(json).unwrap();
        let err = chat_completions_response_to_turn(payload).unwrap_err();
        match err {
            crate::openai::OpenAiError::ContextWindowExceeded { detail } => {
                assert!(detail.contains("context window"), "{detail}");
            }
            other => panic!("expected ContextWindowExceeded, got {other:?}"),
        }
        // The mapped InferenceError carries the stable metrics label.
        let err = crate::openai::OpenAiError::ContextWindowExceeded {
            detail: "too long".into(),
        };
        assert_eq!(
            crate::shared::provider_error_to_inference(err).metric_label(),
            "context_window_exceeded"
        );
    }

    // -- finish_reason behavior mapping (streaming) -------------------------

    #[test]
    fn streaming_length_sets_truncation_flag_on_final_text() {
        let mut acc = ChatCompletionsStreamAccumulator::default();
        let chunk1: ChatCompletionsStreamResponse =
            serde_json::from_str(r#"{"choices":[{"delta":{"content":"partial"}}]}"#).unwrap();
        let chunk2: ChatCompletionsStreamResponse =
            serde_json::from_str(r#"{"choices":[{"delta":{},"finish_reason":"length"}]}"#).unwrap();
        acc.apply(&chunk1, &mut |_| Ok(())).unwrap();
        acc.apply(&chunk2, &mut |_| Ok(())).unwrap();
        let result = acc.into_turn_result().unwrap();
        match result {
            ChatTurnResult::FinalText(f) => assert!(f.truncated),
            other => panic!("expected FinalText, got {other:?}"),
        }
    }

    #[test]
    fn streaming_sensitive_is_terminal_even_with_empty_body() {
        // The silent-failure fix: a content-filtered stream sends no deltas,
        // so without the finish-reason check it would fall through to the
        // generic EmptyResponse error.
        let mut acc = ChatCompletionsStreamAccumulator::default();
        let chunk: ChatCompletionsStreamResponse =
            serde_json::from_str(r#"{"choices":[{"delta":{},"finish_reason":"sensitive"}]}"#)
                .unwrap();
        acc.apply(&chunk, &mut |_| Ok(())).unwrap();
        let err = acc.into_turn_result().unwrap_err();
        assert!(matches!(
            err,
            crate::openai::OpenAiError::ContentFiltered { .. }
        ));
    }

    #[test]
    fn streaming_context_window_exceeded_is_terminal() {
        let mut acc = ChatCompletionsStreamAccumulator::default();
        let chunk: ChatCompletionsStreamResponse = serde_json::from_str(
            r#"{"choices":[{"delta":{},"finish_reason":"model_context_window_exceeded"}]}"#,
        )
        .unwrap();
        acc.apply(&chunk, &mut |_| Ok(())).unwrap();
        let err = acc.into_turn_result().unwrap_err();
        assert!(matches!(
            err,
            crate::openai::OpenAiError::ContextWindowExceeded { .. }
        ));
    }

    #[test]
    fn streaming_stop_leaves_truncation_flag_false() {
        let mut acc = ChatCompletionsStreamAccumulator::default();
        let chunk1: ChatCompletionsStreamResponse =
            serde_json::from_str(r#"{"choices":[{"delta":{"content":"all done"}}]}"#).unwrap();
        let chunk2: ChatCompletionsStreamResponse =
            serde_json::from_str(r#"{"choices":[{"delta":{},"finish_reason":"stop"}]}"#).unwrap();
        acc.apply(&chunk1, &mut |_| Ok(())).unwrap();
        acc.apply(&chunk2, &mut |_| Ok(())).unwrap();
        let result = acc.into_turn_result().unwrap();
        match result {
            ChatTurnResult::FinalText(f) => {
                assert!(!f.truncated);
                assert_eq!(f.content, "all done");
            }
            other => panic!("expected FinalText, got {other:?}"),
        }
    }

    // -- validate_tool_call_arguments tests --------------------------------

    #[test]
    fn validate_valid_arguments_kept() {
        let mut calls = vec![
            ChatToolCall {
                id: "call_1".into(),
                name: "get_weather".into(),
                arguments_json: r#"{"city":"London"}"#.into(),
                caller: None,
            },
            ChatToolCall {
                id: "call_2".into(),
                name: "search".into(),
                arguments_json: r#"{"q":"rust"}"#.into(),
                caller: None,
            },
        ];
        let discarded = crate::openai::validate_tool_call_arguments(&mut calls);
        assert!(discarded.is_empty());
        assert_eq!(calls.len(), 2);
    }

    #[test]
    fn validate_invalid_arguments_discarded() {
        let mut calls = vec![
            ChatToolCall {
                id: "call_1".into(),
                name: "get_weather".into(),
                arguments_json: r#"{"city":"London"}"#.into(),
                caller: None,
            },
            ChatToolCall {
                id: "call_2".into(),
                name: "bad_tool".into(),
                arguments_json: "truncated garbage".into(),
                caller: None,
            },
        ];
        let discarded = crate::openai::validate_tool_call_arguments(&mut calls);
        assert_eq!(
            discarded,
            vec![choreo_proto::DiscardedToolCall {
                name: "bad_tool".into(),
                arguments_json: "truncated garbage".into(),
            }]
        );
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "get_weather");
    }

    #[test]
    fn validate_all_invalid_returns_all_names() {
        let mut calls = vec![
            ChatToolCall {
                id: "call_1".into(),
                name: "tool_a".into(),
                arguments_json: "bad".into(),
                caller: None,
            },
            ChatToolCall {
                id: "call_2".into(),
                name: "tool_b".into(),
                arguments_json: "also bad".into(),
                caller: None,
            },
        ];
        let discarded = crate::openai::validate_tool_call_arguments(&mut calls);
        assert_eq!(discarded.len(), 2);
        assert_eq!(discarded[0].name, "tool_a");
        assert_eq!(discarded[0].arguments_json, "bad");
        assert_eq!(discarded[1].name, "tool_b");
        assert_eq!(discarded[1].arguments_json, "also bad");
        assert!(calls.is_empty());
    }

    #[test]
    fn validate_empty_list_returns_empty() {
        let mut calls: Vec<ChatToolCall> = vec![];
        let discarded = crate::openai::validate_tool_call_arguments(&mut calls);
        assert!(discarded.is_empty());
        assert!(calls.is_empty());
    }

    // -- tool call accumulation tests ------------------------------------

    #[test]
    fn accumulate_no_deltas_returns_empty_vec() {
        let result = accumulate_tool_calls_from_deltas(vec![]);
        assert!(result.is_empty());
    }

    #[test]
    fn accumulate_single_tool_call_in_one_chunk() {
        let deltas = vec![StreamToolCallDelta {
            index: 0,
            id: Some("call_1".into()),
            kind: Some("function".into()),
            function: Some(StreamToolCallFunctionDelta {
                name: Some("get_weather".into()),
                arguments: Some(r#"{"city":"London"}"#.into()),
            }),
        }];
        let result = accumulate_tool_calls_from_deltas(deltas);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].id, "call_1");
        assert_eq!(result[0].name, "get_weather");
        assert_eq!(result[0].arguments_json, r#"{"city":"London"}"#);
    }

    #[test]
    fn accumulate_arguments_concatenated_across_chunks() {
        let deltas = vec![
            StreamToolCallDelta {
                index: 0,
                id: Some("call_1".into()),
                kind: None,
                function: Some(StreamToolCallFunctionDelta {
                    name: Some("get_weather".into()),
                    // First half: split inside the value string, not at a
                    // quote boundary, so concatenation yields valid JSON.
                    arguments: Some(r#"{"city":"Lon"#.into()),
                }),
            },
            StreamToolCallDelta {
                index: 0,
                id: None,
                kind: None,
                function: Some(StreamToolCallFunctionDelta {
                    name: None,
                    arguments: Some(r#"don"}"#.into()),
                }),
            },
        ];
        let result = accumulate_tool_calls_from_deltas(deltas);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].id, "call_1");
        assert_eq!(result[0].name, "get_weather");
        assert_eq!(result[0].arguments_json, r#"{"city":"London"}"#);
    }

    #[test]
    fn accumulate_multiple_tool_calls_sorted_by_index() {
        let deltas = vec![
            // Tool call 1, first chunk
            StreamToolCallDelta {
                index: 1,
                id: Some("call_2".into()),
                kind: None,
                function: Some(StreamToolCallFunctionDelta {
                    name: Some("search".into()),
                    arguments: Some(r#"{"q":"rust"}"#.into()),
                }),
            },
            // Tool call 0, arrives after index 1
            StreamToolCallDelta {
                index: 0,
                id: Some("call_1".into()),
                kind: None,
                function: Some(StreamToolCallFunctionDelta {
                    name: Some("get_weather".into()),
                    arguments: Some(r#"{"city":"Paris"}"#.into()),
                }),
            },
        ];
        let result = accumulate_tool_calls_from_deltas(deltas);
        assert_eq!(result.len(), 2);
        // Must be sorted by index: index 0 first, then index 1
        assert_eq!(result[0].id, "call_1");
        assert_eq!(result[0].name, "get_weather");
        assert_eq!(result[1].id, "call_2");
        assert_eq!(result[1].name, "search");
    }

    #[test]
    fn accumulate_missing_id_defaults_to_empty() {
        let deltas = vec![StreamToolCallDelta {
            index: 0,
            id: None,
            kind: None,
            function: Some(StreamToolCallFunctionDelta {
                name: Some("get_weather".into()),
                arguments: Some(r#"{}"#.into()),
            }),
        }];
        let result = accumulate_tool_calls_from_deltas(deltas);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].id, "");
        assert_eq!(result[0].name, "get_weather");
    }

    #[test]
    fn accumulate_missing_name_defaults_to_empty() {
        let deltas = vec![StreamToolCallDelta {
            index: 0,
            id: Some("call_1".into()),
            kind: None,
            function: Some(StreamToolCallFunctionDelta {
                name: None,
                arguments: Some(r#"{}"#.into()),
            }),
        }];
        let result = accumulate_tool_calls_from_deltas(deltas);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].id, "call_1");
        assert_eq!(result[0].name, "");
    }

    #[test]
    fn accumulate_no_function_delta_produces_empty_call() {
        let deltas = vec![StreamToolCallDelta {
            index: 0,
            id: Some("call_1".into()),
            kind: None,
            function: None,
        }];
        let result = accumulate_tool_calls_from_deltas(deltas);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].id, "call_1");
        assert_eq!(result[0].name, "");
        assert_eq!(result[0].arguments_json, "");
    }

    // -- streaming delta deserialisation tests --------------------------

    #[test]
    fn stream_delta_tool_calls_deserialises() {
        let payload: ChatCompletionsStreamResponse = serde_json::from_str(
            r#"{
                "choices":[{
                    "delta":{
                        "content":"Hello",
                        "tool_calls":[{
                            "index":0,
                            "id":"call_abc",
                            "type":"function",
                            "function":{"name":"get_weather","arguments":"{\"city\":\"London\"}"}
                        }]
                    }
                }]
            }"#,
        )
        .expect("parse");
        let delta = payload.choices.into_iter().next().unwrap().delta.unwrap();
        assert_eq!(delta.content.as_deref(), Some("Hello"));
        let tcs = delta.tool_calls.expect("tool_calls");
        assert_eq!(tcs.len(), 1);
        assert_eq!(tcs[0].index, 0);
        assert_eq!(tcs[0].id.as_deref(), Some("call_abc"));
        assert_eq!(tcs[0].kind.as_deref(), Some("function"));
        let func = tcs[0].function.as_ref().unwrap();
        assert_eq!(func.name.as_deref(), Some("get_weather"));
        assert_eq!(func.arguments.as_deref(), Some(r#"{"city":"London"}"#));
    }

    #[test]
    fn stream_delta_tool_calls_absent_when_not_in_json() {
        let payload: ChatCompletionsStreamResponse =
            serde_json::from_str(r#"{"choices":[{"delta":{"content":"Hi"}}]}"#).expect("parse");
        let delta = payload.choices.into_iter().next().unwrap().delta.unwrap();
        assert_eq!(delta.content.as_deref(), Some("Hi"));
        assert!(delta.tool_calls.is_none());
    }

    // -- accumulated deltas -> ChatTurnResult integration test ----------

    #[test]
    fn accumulate_deltas_to_tool_use_result() {
        // Simulate what the streaming function does: collect deltas from
        // multiple SSE chunks and pass them through the accumulator.
        let deltas = vec![
            StreamToolCallDelta {
                index: 0,
                id: Some("call_1".into()),
                kind: None,
                function: Some(StreamToolCallFunctionDelta {
                    name: Some("search".into()),
                    arguments: Some(r#"{"q":"we"#.into()),
                }),
            },
            StreamToolCallDelta {
                index: 0,
                id: None,
                kind: None,
                function: Some(StreamToolCallFunctionDelta {
                    name: None,
                    arguments: Some(r#"ather"}"#.into()),
                }),
            },
        ];
        let tool_calls = accumulate_tool_calls_from_deltas(deltas);
        assert_eq!(tool_calls.len(), 1);
        assert_eq!(tool_calls[0].id, "call_1");
        assert_eq!(tool_calls[0].name, "search");
        assert_eq!(tool_calls[0].arguments_json, r#"{"q":"weather"}"#);

        let result = ChatTurnResult::ToolUse(ChatAssistantToolUse {
            content: Some("I'll search for that.".into()),
            tool_calls,
            reasoning: None,
            usage: None,
            response_id: None,
            reasoning_artifact: None,
        });
        match result {
            ChatTurnResult::ToolUse(use_) => {
                assert_eq!(use_.content.as_deref(), Some("I'll search for that."));
                assert_eq!(use_.tool_calls.len(), 1);
            }
            _ => panic!("expected ToolUse"),
        }
    }

    // -- reasoning_effort serialization tests ---------------------------

    #[test]
    fn reasoning_effort_serialization() {
        // Off → None (omitted from body)
        assert_eq!(crate::openai::reasoning_effort_api_value("off"), None);

        // Low → "low"
        assert_eq!(
            crate::openai::reasoning_effort_api_value("low"),
            Some("low")
        );

        // Medium → "medium"
        assert_eq!(
            crate::openai::reasoning_effort_api_value("medium"),
            Some("medium")
        );

        // High → "high"
        assert_eq!(
            crate::openai::reasoning_effort_api_value("high"),
            Some("high")
        );
    }

    #[test]
    fn chat_completions_request_omits_reasoning_effort_when_none() {
        let body = serde_json::to_value(&ChatCompletionsRequest {
            model: "gpt-4.1",
            messages: &[ChatRequestMessage::simple("user", "hello".into())],
            tools: None,
            stream: false,
            stream_options: None,
            max_tokens: None,
            max_completion_tokens: None,
            reasoning_effort: None,
        })
        .unwrap();
        assert!(body.get("reasoning_effort").is_none(), "should be omitted");
    }

    // -- token usage streaming response tests ----------------------------

    #[test]
    fn stream_response_deserializes_usage_chunk() {
        // OpenAI sends a usage-only chunk at the end of a stream with
        // stream_options.include_usage=true.
        let json = r#"{"choices":[],"usage":{"prompt_tokens":50,"completion_tokens":25,"total_tokens":75}}"#;
        let payload: ChatCompletionsStreamResponse = serde_json::from_str(json).unwrap();
        assert!(payload.choices.is_empty());
        let usage = payload.usage.expect("usage should be present");
        assert_eq!(usage.prompt_tokens, 50);
        assert_eq!(usage.completion_tokens, 25);
        assert_eq!(usage.total_tokens, 75);
    }

    #[test]
    fn stream_response_without_usage_defaults_to_none() {
        let json = r#"{"choices":[{"delta":{"content":"hello"}}]}"#;
        let payload: ChatCompletionsStreamResponse = serde_json::from_str(json).unwrap();
        assert_eq!(payload.choices.len(), 1);
        assert!(payload.usage.is_none());
    }

    #[test]
    fn test_chat_completions_response_non_streaming_with_usage() {
        // Non-streaming response with usage
        let json = r#"{"choices":[{"message":{"content":"Hello","tool_calls":[],"reasoning_content":null,"reasoning":null,"reasoning_text":null}}],"usage":{"prompt_tokens":10,"completion_tokens":5,"total_tokens":15}}"#;
        let resp: ChatCompletionsResponse = serde_json::from_str(json).unwrap();
        let usage = resp.usage.expect("usage should be present");
        assert_eq!(usage.prompt_tokens, 10);
        assert_eq!(usage.completion_tokens, 5);
        assert_eq!(usage.total_tokens, 15);
    }

    #[test]
    fn usage_parses_zai_cached_prompt_tokens() {
        // z.ai-shaped usage: prompt_tokens_details.cached_tokens present.
        let json = r#"{"choices":[],"usage":{"prompt_tokens":100,"completion_tokens":20,"total_tokens":120,"prompt_tokens_details":{"cached_tokens":64}}}"#;
        let resp: ChatCompletionsResponse = serde_json::from_str(json).unwrap();
        let usage = resp.usage.expect("usage should be present");
        assert_eq!(
            usage
                .prompt_tokens_details
                .expect("details present")
                .cached_tokens,
            64
        );
    }

    #[test]
    fn usage_without_prompt_tokens_details_defaults_to_none() {
        // OpenAI/DeepSeek-style usage with no details object must still parse.
        let json = r#"{"choices":[],"usage":{"prompt_tokens":10,"completion_tokens":5,"total_tokens":15}}"#;
        let resp: ChatCompletionsResponse = serde_json::from_str(json).unwrap();
        let usage = resp.usage.expect("usage should be present");
        assert!(usage.prompt_tokens_details.is_none());
    }

    #[test]
    fn response_to_turn_maps_zai_cached_tokens_into_token_usage() {
        // z.ai reports cached prompt tokens in prompt_tokens_details; the
        // parsed TokenUsage must carry that count for cost/usage reporting.
        let json = r#"{"choices":[{"message":{"content":"hi"}}],"usage":{"prompt_tokens":100,"completion_tokens":20,"total_tokens":120,"prompt_tokens_details":{"cached_tokens":64}}}"#;
        let payload: ChatCompletionsResponse = serde_json::from_str(json).unwrap();
        let turn = chat_completions_response_to_turn(payload).expect("turn");
        let ChatTurnResult::FinalText(result) = turn else {
            panic!("expected FinalText");
        };
        assert_eq!(
            result.usage,
            Some(TokenUsage {
                input_tokens: 100,
                output_tokens: 20,
                total_tokens: 120,
                cached_tokens: 64,
            })
        );
    }

    #[test]
    fn response_to_turn_defaults_cached_tokens_to_zero_when_absent() {
        let json = r#"{"choices":[{"message":{"content":"hi"}}],"usage":{"prompt_tokens":10,"completion_tokens":5,"total_tokens":15}}"#;
        let payload: ChatCompletionsResponse = serde_json::from_str(json).unwrap();
        let turn = chat_completions_response_to_turn(payload).expect("turn");
        let ChatTurnResult::FinalText(result) = turn else {
            panic!("expected FinalText");
        };
        assert_eq!(
            result.usage,
            Some(TokenUsage {
                input_tokens: 10,
                output_tokens: 5,
                total_tokens: 15,
                cached_tokens: 0,
            })
        );
    }

    // -- chat completions stream delta keeps reasoning separate -----------

    #[test]
    fn chat_completions_stream_delta_keeps_reasoning_separate() {
        let payload: ChatCompletionsStreamResponse = serde_json::from_str(
            r#"{"choices":[{"delta":{"content":"answer","reasoning_text":"think"}}]}"#,
        )
        .expect("parse");

        let delta = payload
            .choices
            .into_iter()
            .next()
            .expect("choice")
            .delta
            .expect("delta");
        assert_eq!(delta.content.as_deref(), Some("answer"));
        assert_eq!(delta.reasoning_text.as_deref(), Some("think"));
    }

    // -- reasoning artifact capture (phase 2b) ---------------------------

    #[test]
    fn non_streaming_captures_reasoning_content_artifact() {
        // DeepSeek/Kimi-style response: `reasoning_content` populated.
        let json = r#"{"choices":[{"message":{"content":"answer","tool_calls":[],"reasoning_content":"Let me think step-by-step","reasoning":null,"reasoning_text":null}}],"usage":null}"#;
        let payload: ChatCompletionsResponse = serde_json::from_str(json).unwrap();
        let result = chat_completions_response_to_turn(payload).unwrap();
        match result {
            ChatTurnResult::FinalText(f) => {
                assert_eq!(f.content, "answer");
                assert_eq!(f.reasoning.as_deref(), Some("Let me think step-by-step"));
                assert_eq!(
                    f.reasoning_artifact,
                    Some(ReasoningArtifact::ChatReasoning {
                        field: ChatReasoningField::ReasoningContent,
                        bytes: b"Let me think step-by-step".to_vec(),
                    })
                );
            }
            other => panic!("expected FinalText, got {other:?}"),
        }
    }

    #[test]
    fn non_streaming_captures_reasoning_field_artifact() {
        // Some providers populate the bare `reasoning` field instead.
        let json = r#"{"choices":[{"message":{"content":"answer","tool_calls":[],"reasoning_content":null,"reasoning":"bare reasoning field","reasoning_text":null}}],"usage":null}"#;
        let payload: ChatCompletionsResponse = serde_json::from_str(json).unwrap();
        let result = chat_completions_response_to_turn(payload).unwrap();
        match result {
            ChatTurnResult::FinalText(f) => {
                assert_eq!(f.reasoning.as_deref(), Some("bare reasoning field"));
                assert_eq!(
                    f.reasoning_artifact,
                    Some(ReasoningArtifact::ChatReasoning {
                        field: ChatReasoningField::Reasoning,
                        bytes: b"bare reasoning field".to_vec(),
                    })
                );
            }
            other => panic!("expected FinalText, got {other:?}"),
        }
    }

    #[test]
    fn non_streaming_captures_reasoning_text_field_artifact() {
        // And others use `reasoning_text` — whichever field is populated
        // must be the one captured.
        let json = r#"{"choices":[{"message":{"content":"answer","tool_calls":[],"reasoning_content":null,"reasoning":null,"reasoning_text":"text reasoning"}}],"usage":null}"#;
        let payload: ChatCompletionsResponse = serde_json::from_str(json).unwrap();
        let result = chat_completions_response_to_turn(payload).unwrap();
        match result {
            ChatTurnResult::FinalText(f) => {
                assert_eq!(f.reasoning.as_deref(), Some("text reasoning"));
                assert_eq!(
                    f.reasoning_artifact,
                    Some(ReasoningArtifact::ChatReasoning {
                        field: ChatReasoningField::ReasoningText,
                        bytes: b"text reasoning".to_vec(),
                    })
                );
            }
            other => panic!("expected FinalText, got {other:?}"),
        }
    }

    #[test]
    fn non_streaming_captures_artifact_on_tool_use() {
        // The artifact must ride along on ChatAssistantToolUse too — that's
        // the DeepSeek tool-loop case where `reasoning_content` has to be
        // echoed back on the next assistant message.
        let json = r#"{"choices":[{"message":{
            "content":null,
            "tool_calls":[{"id":"call_1","type":"function","function":{"name":"get_weather","arguments":"{\"city\":\"London\"}"}}],
            "reasoning_content":"reasoning for tool call",
            "reasoning":null,"reasoning_text":null
        }}],"usage":null}"#;
        let payload: ChatCompletionsResponse = serde_json::from_str(json).unwrap();
        let result = chat_completions_response_to_turn(payload).unwrap();
        match result {
            ChatTurnResult::ToolUse(t) => {
                assert_eq!(t.reasoning.as_deref(), Some("reasoning for tool call"));
                assert_eq!(
                    t.reasoning_artifact,
                    Some(ReasoningArtifact::ChatReasoning {
                        field: ChatReasoningField::ReasoningContent,
                        bytes: b"reasoning for tool call".to_vec(),
                    })
                );
            }
            other => panic!("expected ToolUse, got {other:?}"),
        }
    }

    #[test]
    fn non_streaming_no_reasoning_yields_none_artifact() {
        // Control case: a response with no reasoning at all.
        let json =
            r#"{"choices":[{"message":{"content":"plain answer","tool_calls":[]}}],"usage":null}"#;
        let payload: ChatCompletionsResponse = serde_json::from_str(json).unwrap();
        let result = chat_completions_response_to_turn(payload).unwrap();
        match result {
            ChatTurnResult::FinalText(f) => {
                assert_eq!(f.content, "plain answer");
                assert!(f.reasoning.is_none());
                assert!(f.reasoning_artifact.is_none());
            }
            other => panic!("expected FinalText, got {other:?}"),
        }
    }

    #[test]
    fn non_streaming_empty_reasoning_yields_none_artifact() {
        // `reasoning_content: ""` captures nothing reusable — an empty
        // payload must not become an artifact.
        let json = r#"{"choices":[{"message":{"content":"answer","tool_calls":[],"reasoning_content":"","reasoning":null,"reasoning_text":null}}],"usage":null}"#;
        let payload: ChatCompletionsResponse = serde_json::from_str(json).unwrap();
        let result = chat_completions_response_to_turn(payload).unwrap();
        match result {
            ChatTurnResult::FinalText(f) => {
                assert!(f.reasoning_artifact.is_none());
            }
            other => panic!("expected FinalText, got {other:?}"),
        }
    }

    #[test]
    fn streaming_accumulates_reasoning_artifact() {
        // Two chunks carrying reasoning deltas plus an answer chunk, then
        // finalize — the concatenated deltas become the artifact bytes.
        let mut acc = ChatCompletionsStreamAccumulator::default();
        let chunk1: ChatCompletionsStreamResponse =
            serde_json::from_str(r#"{"choices":[{"delta":{"reasoning_content":"Let me think"}}]}"#)
                .unwrap();
        let chunk2: ChatCompletionsStreamResponse = serde_json::from_str(
            r#"{"choices":[{"delta":{"reasoning_content":" step-by-step","content":"answer"}}]}"#,
        )
        .unwrap();
        let mut events = Vec::new();
        acc.apply(&chunk1, &mut |e| {
            events.push(e);
            Ok(())
        })
        .unwrap();
        acc.apply(&chunk2, &mut |e| {
            events.push(e);
            Ok(())
        })
        .unwrap();
        let result = acc.into_turn_result().unwrap();
        match result {
            ChatTurnResult::FinalText(f) => {
                assert_eq!(f.content, "answer");
                assert_eq!(f.reasoning.as_deref(), Some("Let me think step-by-step"));
                assert_eq!(
                    f.reasoning_artifact,
                    Some(ReasoningArtifact::ChatReasoning {
                        field: ChatReasoningField::ReasoningContent,
                        bytes: b"Let me think step-by-step".to_vec(),
                    })
                );
            }
            other => panic!("expected FinalText, got {other:?}"),
        }
        // Both reasoning deltas were forwarded as display events. Within a
        // chunk, content is emitted before reasoning (pre-existing order).
        assert_eq!(
            events,
            vec![
                StreamEvent::Reasoning("Let me think".into()),
                StreamEvent::Answer("answer".into()),
                StreamEvent::Reasoning(" step-by-step".into()),
            ]
        );
    }

    #[test]
    fn streaming_tool_use_carries_reasoning_artifact() {
        let mut acc = ChatCompletionsStreamAccumulator::default();
        let chunk: ChatCompletionsStreamResponse = serde_json::from_str(
            r#"{"choices":[{"delta":{
                "reasoning_content":"deciding on tool",
                "tool_calls":[{"index":0,"id":"call_1","type":"function","function":{"name":"get_weather","arguments":"{\"city\":\"London\"}"}}]
            }}]}"#,
        )
        .unwrap();
        let mut events = Vec::new();
        acc.apply(&chunk, &mut |e| {
            events.push(e);
            Ok(())
        })
        .unwrap();
        let result = acc.into_turn_result().unwrap();
        match result {
            ChatTurnResult::ToolUse(t) => {
                assert_eq!(t.reasoning.as_deref(), Some("deciding on tool"));
                assert_eq!(
                    t.reasoning_artifact,
                    Some(ReasoningArtifact::ChatReasoning {
                        field: ChatReasoningField::ReasoningContent,
                        bytes: b"deciding on tool".to_vec(),
                    })
                );
            }
            other => panic!("expected ToolUse, got {other:?}"),
        }
    }

    #[test]
    fn streaming_no_reasoning_yields_none_artifact() {
        // Control case: stream with only answer text.
        let mut acc = ChatCompletionsStreamAccumulator::default();
        let chunk: ChatCompletionsStreamResponse =
            serde_json::from_str(r#"{"choices":[{"delta":{"content":"hi"}}]}"#).unwrap();
        let mut events = Vec::new();
        acc.apply(&chunk, &mut |e| {
            events.push(e);
            Ok(())
        })
        .unwrap();
        let result = acc.into_turn_result().unwrap();
        match result {
            ChatTurnResult::FinalText(f) => {
                assert!(f.reasoning.is_none());
                assert!(f.reasoning_artifact.is_none());
            }
            other => panic!("expected FinalText, got {other:?}"),
        }
    }

    #[test]
    fn streaming_locks_reasoning_field_on_first_delta() {
        // A stream that sends `reasoning_text` (not `reasoning_content`) must
        // capture ONLY that field: the round-trip artifact is re-emitted to
        // the same field the provider used, so a later stray
        // `reasoning_content` delta must not be mixed into the payload (it
        // would corrupt the replayed reasoning on the next tool-loop turn).
        let mut acc = ChatCompletionsStreamAccumulator::default();
        let chunk1: ChatCompletionsStreamResponse = serde_json::from_str(
            r#"{"choices":[{"delta":{"reasoning_text":"think step by step"}}]}"#,
        )
        .unwrap();
        let chunk2: ChatCompletionsStreamResponse = serde_json::from_str(
            r#"{"choices":[{"delta":{"reasoning_content":"stray other field","content":"answer"}}]}"#,
        )
        .unwrap();
        let mut events = Vec::new();
        acc.apply(&chunk1, &mut |e| {
            events.push(e);
            Ok(())
        })
        .unwrap();
        acc.apply(&chunk2, &mut |e| {
            events.push(e);
            Ok(())
        })
        .unwrap();
        let result = acc.into_turn_result().unwrap();
        match result {
            ChatTurnResult::FinalText(f) => {
                assert_eq!(f.content, "answer");
                assert_eq!(f.reasoning.as_deref(), Some("think step by step"));
                assert_eq!(
                    f.reasoning_artifact,
                    Some(ReasoningArtifact::ChatReasoning {
                        field: ChatReasoningField::ReasoningText,
                        bytes: b"think step by step".to_vec(),
                    }),
                    "artifact must capture only the locked-in reasoning_text field",
                );
                // The field tag must record WHICH wire field was captured, so
                // re-emission targets reasoning_text (not reasoning_content).
                if let Some(ReasoningArtifact::ChatReasoning { field, .. }) = f.reasoning_artifact {
                    assert_eq!(field, ChatReasoningField::ReasoningText);
                }
            }
            other => panic!("expected FinalText, got {other:?}"),
        }
        // Only the locked-in field's delta is forwarded as a display event;
        // the stray reasoning_content delta is neither displayed nor captured.
        assert_eq!(
            events,
            vec![
                StreamEvent::Reasoning("think step by step".into()),
                StreamEvent::Answer("answer".into()),
            ]
        );
    }

    #[test]
    fn streaming_single_chunk_prefers_reasoning_content() {
        // A single chunk carrying BOTH reasoning_content and reasoning must
        // pick reasoning_content — the non-streaming precedence
        // (take_reasoning consumes reasoning_content first).
        let mut acc = ChatCompletionsStreamAccumulator::default();
        let chunk: ChatCompletionsStreamResponse = serde_json::from_str(
            r#"{"choices":[{"delta":{"reasoning_content":"primary","reasoning":"secondary"}}]}"#,
        )
        .unwrap();
        acc.apply(&chunk, &mut |_| Ok(())).unwrap();
        let result = acc.into_turn_result().unwrap();
        match result {
            ChatTurnResult::FinalText(f) => {
                assert_eq!(f.reasoning.as_deref(), Some("primary"));
                assert_eq!(
                    f.reasoning_artifact,
                    Some(ReasoningArtifact::ChatReasoning {
                        field: ChatReasoningField::ReasoningContent,
                        bytes: b"primary".to_vec(),
                    }),
                );
            }
            other => panic!("expected FinalText, got {other:?}"),
        }
    }

    // -- reasoning artifact re-emission (phase 4a) -------------------------

    #[test]
    fn chat_request_message_reemits_chat_reasoning_artifact() {
        // An assistant message carrying the opaque ChatReasoning artifact must
        // re-emit it as `reasoning_content` on the wire (DeepSeek/Kimi reject
        // a tool-loop turn that drops it).
        let msg = ChatRequestMessage {
            role: "assistant",
            content: Some("answer".into()),
            images: Vec::new(),
            tool_call_id: None,
            tool_calls: None,
            reasoning_content: None,
            reasoning: None,
            reasoning_text: None,
            reasoning_artifact: Some(ReasoningArtifact::ChatReasoning {
                field: ChatReasoningField::ReasoningContent,
                bytes: b"Let me think step-by-step".to_vec(),
            }),
        };
        let body = serde_json::to_value(&msg).unwrap();
        assert_eq!(body["reasoning_content"], "Let me think step-by-step");
        // The artifact field itself never appears on the wire.
        assert!(body.get("reasoning_artifact").is_none());
    }

    #[test]
    fn chat_request_message_reemits_to_captured_field() {
        // The artifact records WHICH chat field the provider used; re-emission
        // must target that same field — a `reasoning_text` artifact goes back
        // as `reasoning_text`, a `reasoning` artifact as `reasoning`, and
        // neither leaks into `reasoning_content` (the historical default).
        for (field, wire_key) in [
            (ChatReasoningField::ReasoningText, "reasoning_text"),
            (ChatReasoningField::Reasoning, "reasoning"),
        ] {
            let msg = ChatRequestMessage {
                role: "assistant",
                content: Some("answer".into()),
                images: Vec::new(),
                tool_call_id: None,
                tool_calls: None,
                reasoning_content: None,
                reasoning: None,
                reasoning_text: None,
                reasoning_artifact: Some(ReasoningArtifact::ChatReasoning {
                    field,
                    bytes: b"provider reasoning".to_vec(),
                }),
            };
            let body = serde_json::to_value(&msg).unwrap();
            assert_eq!(body[wire_key], "provider reasoning");
            for other in ["reasoning_content", "reasoning", "reasoning_text"] {
                if other != wire_key {
                    assert!(
                        body.get(other).is_none(),
                        "{other} must be absent when re-emitting {wire_key}"
                    );
                }
            }
            assert!(body.get("reasoning_artifact").is_none());
        }
    }

    #[test]
    fn chat_request_message_without_artifact_omits_reasoning_content() {
        // Control: no artifact → no `reasoning_content` on the wire.
        let msg = ChatRequestMessage::simple("assistant", "plain".into());
        let body = serde_json::to_value(&msg).unwrap();
        assert!(body.get("reasoning_content").is_none());
    }

    #[test]
    fn chat_request_message_corrupt_artifact_is_dropped_not_fatal() {
        // A persisted artifact whose bytes are not valid UTF-8 must NOT fail
        // the whole request serialization (the pre-fix behavior surfaced a
        // corrupted DB blob as a hard error on every subsequent turn). It is
        // logged and dropped: the message serializes as a plain assistant
        // message with no reasoning echo.
        let msg = ChatRequestMessage {
            role: "assistant",
            content: Some("answer".into()),
            images: Vec::new(),
            tool_call_id: None,
            tool_calls: None,
            reasoning_content: None,
            reasoning: None,
            reasoning_text: None,
            reasoning_artifact: Some(ReasoningArtifact::ChatReasoning {
                field: ChatReasoningField::ReasoningContent,
                // 0xFF is never valid UTF-8.
                bytes: vec![0xFF, 0xFE, 0x00, 0x41],
            }),
        };
        let body = serde_json::to_value(&msg).unwrap();
        assert_eq!(body["content"], "answer");
        assert!(body.get("reasoning_content").is_none());
        assert!(body.get("reasoning_artifact").is_none());
    }

    #[test]
    fn chat_request_message_non_assistant_role_drops_artifact() {
        // The artifact is assistant-only: a user message must never carry
        // `reasoning_content`, even if an artifact were attached.
        let msg = ChatRequestMessage {
            role: "user",
            content: Some("hi".into()),
            images: Vec::new(),
            tool_call_id: None,
            tool_calls: None,
            reasoning_content: None,
            reasoning: None,
            reasoning_text: None,
            reasoning_artifact: Some(ReasoningArtifact::ChatReasoning {
                field: ChatReasoningField::ReasoningContent,
                bytes: b"should not leak".to_vec(),
            }),
        };
        let body = serde_json::to_value(&msg).unwrap();
        assert!(body.get("reasoning_content").is_none());
        assert!(body.get("reasoning_artifact").is_none());
    }

    #[test]
    fn chat_request_message_wrong_artifact_variant_does_not_leak() {
        // A non-ChatReasoning artifact is foreign to this adapter — it must
        // not be misinterpreted as chat reasoning.
        let msg = ChatRequestMessage {
            role: "assistant",
            content: Some("answer".into()),
            images: Vec::new(),
            tool_call_id: None,
            tool_calls: None,
            reasoning_content: None,
            reasoning: None,
            reasoning_text: None,
            reasoning_artifact: Some(ReasoningArtifact::GoogleSignatures(
                b"encrypted-sig".to_vec(),
            )),
        };
        let body = serde_json::to_value(&msg).unwrap();
        assert!(body.get("reasoning_content").is_none());
        assert!(body.get("reasoning_artifact").is_none());
    }
}
