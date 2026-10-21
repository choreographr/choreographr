//! The `OpenAI` Chat Completions adapter: the request entry points (simple,
//! tool-carrying, and their streaming variants) plus the module layout.
//!
//! - [`wire`] — the request/response wire types and the lenient `FinishReason`.
//! - [`turn`] — the reply → turn conversion, shared by both paths.
//! - [`accumulator`] — the streamed-chunk fold and tool-call merger.
//! - [`reasoning_stream`] — the turn's reasoning display/artifact state machine.

use tracing::debug;

use super::retry;
use super::{ChatRequestMessage, SseReader, endpoint_url};
use crate::shared::RequestAuth;
use crate::types::{ChatTurnResult, StreamEvent};
use std::io;

mod accumulator;
mod reasoning_stream;

use accumulator::ChatCompletionsStreamAccumulator;
#[cfg(test)]
// The sibling test suite exercises the delta merger on its own, so it is in
// scope here (the request path only reaches it through the accumulator).
use accumulator::accumulate_tool_calls_from_deltas;
use reasoning_stream::ReasoningStreamState;
pub(crate) use turn::{chat_completions_response_to_turn, finish_reason_terminal_error};
pub(crate) use wire::{
    ChatCompletionsRequest, ChatCompletionsResponse, ChatCompletionsStreamOptions,
    ChatCompletionsStreamResponse, FinishReason, StreamDelta, StreamToolCallDelta,
    chat_reasoning_artifact,
};

// The test suite names the wire types through this module (`super::*`); the
// request paths reach them through `wire` directly.
#[cfg(test)]
use wire::*;

mod turn;
mod wire;

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

    // A one-shot prompt has no turn result to carry a truncation flag, but a
    // declared terminal finish reason is still terminal: z.ai's `sensitive`
    // streams no content at all, which would otherwise be reported as an
    // (apparently retryable) empty completion rather than the policy refusal it
    // is. The turn paths classify the same reasons through this same helper.
    let Some(choice) = payload.choices.into_iter().next() else {
        return Err(super::OpenAiError::EmptyResponse);
    };
    if let Some(err) = finish_reason_terminal_error(choice.finish_reason.as_ref()) {
        return Err(err);
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
