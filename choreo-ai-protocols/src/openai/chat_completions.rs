//! The `OpenAI` Chat Completions adapter: the request entry points (simple,
//! tool-carrying, and their streaming variants) plus the module layout.
//!
//! - [`wire`] — the request/response wire types and the lenient `FinishReason`.
//! - [`turn`] — the reply → turn conversion, shared by both paths.
//! - [`accumulator`] — the streamed-chunk fold and tool-call merger.
//! - [`reasoning_stream`] — the turn's reasoning display/artifact state machine.

use tracing::debug;

use super::retry;
use super::{ChatRequestMessage, ChatToolDefinition, SseReader, endpoint_url};
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

// ── The one chat-completions body builder ───────────────────────────────

/// The shape of one chat-completions request body, independent of the catalog
/// and config policy the builder folds in.
///
/// Every chat-completions entry point (simple, tool-carrying, streaming and
/// non-streaming) describes its body through this one struct and hands it to
/// [`build_chat_completions_body`], so a request field is added in exactly one
/// place and can never reach one path while being missed on another.
pub(crate) struct ChatCompletionsBodyParams<'a> {
    pub(crate) model: &'a str,
    /// The canonical conversation; the builder lowers it to the wire messages
    /// under the route's reasoning-echo policy.
    pub(crate) messages: &'a [ChatRequestMessage],
    pub(crate) tools: Option<&'a [ChatToolDefinition]>,
    pub(crate) stream: bool,
    pub(crate) stream_options: Option<ChatCompletionsStreamOptions>,
    pub(crate) reasoning_effort: Option<&'a str>,
    /// Per-call output-token cap (the cache-warming ping's `Some(1)`); `None`
    /// leaves the provider/config default in place.
    pub(crate) max_output_tokens_override: Option<u32>,
    /// The real session id, or `None` on the session-less one-shot prompt API
    /// (which sends no cache-affinity hint regardless of the configured dialect).
    pub(crate) session_id: Option<&'a str>,
}

/// Build a serialized chat-completions request body from `config` and the
/// per-request shape in `params`.
///
/// This is the single assembly point for every chat-completions path: it
/// resolves the output-token field pair (with the optional per-call cap),
/// lowers the messages through [`super::chat_wire_messages`], and resolves the
/// per-session cache-affinity body fields from the configured
/// [`crate::CacheAffinity`] dialect. A field added to the wire type is therefore
/// populated identically on every path or not at all.
///
/// # Errors
///
/// Returns an [`super::OpenAiError::Io`] when the body cannot be serialized to
/// JSON.
pub(crate) fn build_chat_completions_body(
    config: &super::ServiceConfig,
    params: &ChatCompletionsBodyParams<'_>,
) -> Result<serde_json::Value, super::OpenAiError> {
    let (max_tokens, max_completion_tokens) =
        config.max_tokens_field_pair_with_override(params.model, params.max_output_tokens_override);
    // Lower the messages to the wire shape under this route's echo policy, so
    // reasoning re-emission is decided once for every chat-completions path.
    let wire = super::chat_wire_messages(
        params.messages,
        crate::catalog::chat_reasoning_echo(&config.provider_slug, params.model),
    );
    // The one-shot prompt API has no session id, so it sends no hint even when
    // the provider's dialect is set; a real turn resolves the dialect here. The
    // dialect is filtered through the rejection registry so a provider that
    // answered a 400 to this field stops receiving it for the process.
    let affinity = params.session_id.map_or_default(|session_id| {
        crate::session_routing::cache_affinity_fields(
            config.effective_cache_affinity(),
            session_id,
            params.model,
        )
    });
    serde_json::to_value(&ChatCompletionsRequest {
        model: params.model,
        messages: &wire.messages,
        tools: params.tools,
        stream: params.stream,
        stream_options: params.stream_options,
        max_tokens,
        max_completion_tokens,
        reasoning_effort: params.reasoning_effort,
        prompt_cache_key: affinity.prompt_cache_key,
        session_id: affinity.session_id,
    })
    .map_err(|e| super::OpenAiError::Io(io::Error::other(e)))
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
    let retry = retry::retry_config_from_config(config);
    let messages = [ChatRequestMessage::simple("user", prompt.to_string())];
    // The one-shot prompt API carries no session, so no cache-affinity hint.
    let body = build_chat_completions_body(
        config,
        &ChatCompletionsBodyParams {
            model,
            messages: &messages,
            tools: None,
            stream: false,
            stream_options: None,
            reasoning_effort: None,
            max_output_tokens_override: None,
            session_id: None,
        },
    )?;
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
    let retry = retry::retry_config_from_config(config).with_no_retry(no_retry);
    let body = build_chat_completions_body(
        config,
        &ChatCompletionsBodyParams {
            model,
            messages,
            tools: Some(tools),
            stream: false,
            stream_options: None,
            reasoning_effort,
            max_output_tokens_override,
            // The turn's real session id drives per-session cache affinity.
            session_id: route.map(|(session_id, _request_id)| session_id),
        },
    )?;
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
    let retry = retry::retry_config_from_config(config);
    let messages = [ChatRequestMessage::simple("user", prompt.to_string())];
    // The one-shot prompt API carries no session, so no cache-affinity hint.
    let body = build_chat_completions_body(
        config,
        &ChatCompletionsBodyParams {
            model,
            messages: &messages,
            tools: None,
            stream: true,
            stream_options: Some(ChatCompletionsStreamOptions {
                include_usage: true,
            }),
            reasoning_effort,
            max_output_tokens_override: None,
            session_id: None,
        },
    )?;
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
    // The streamed finish reason, captured as it arrives (the last non-None wins)
    // so a declared terminal reason is classified after the stream — the same
    // helper the tool path and the non-streaming simple path use.
    let mut finish_reason: Option<FinishReason> = None;
    // One shared state machine owns the turn's reasoning display (and, on the
    // tool path, its artifact); this path persists no artifact, so `finish` is
    // never called.
    let mut reasoning = ReasoningStreamState::default();
    while let Some(data) = crate::stream::recv_sse_event(&sse, cancel_rx)? {
        let payload: ChatCompletionsStreamResponse =
            serde_json::from_str(&data).map_err(io::Error::other)?;
        for choice in payload.choices {
            // Capture the finish reason before the `delta` field is moved out;
            // it lands on the final chunk (often with `delta: null`), which the
            // `let else` below would otherwise skip entirely.
            if choice.finish_reason.is_some() {
                finish_reason.clone_from(&choice.finish_reason);
            }
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

    // A declared terminal finish reason is terminal on the streaming one-shot
    // path too: z.ai's `sensitive` (and OpenAI's `content_filter`) streams no
    // content, which would otherwise be reported as an (apparently retryable)
    // empty completion rather than the policy refusal it is. The tool path and
    // the non-streaming simple path classify the same reasons through this same
    // helper, so the two simple paths cannot drift apart.
    if let Some(err) = finish_reason_terminal_error(finish_reason.as_ref()) {
        return Err(err);
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
    let retry = retry::retry_config_from_config(config).with_no_retry(no_retry);
    let body = build_chat_completions_body(
        config,
        &ChatCompletionsBodyParams {
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
            reasoning_effort,
            max_output_tokens_override,
            // The turn's real session id drives per-session cache affinity.
            session_id: route.map(|(session_id, _request_id)| session_id),
        },
    )?;
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
