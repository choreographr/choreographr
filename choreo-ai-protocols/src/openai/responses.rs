//! The `OpenAI` Responses API client: the non-streaming and streaming request
//! entry points, and the module layout that keeps each concern readable.
//!
//! The Responses wire format differs from chat completions in three ways this
//! client has to own: a flat `input` item array instead of `messages`, its own
//! opaque `reasoning` output items for round-tripping, and `previous_response_id`
//! chaining. Those live in [`input`], [`turn`] and [`stream`] respectively; this
//! file keeps the request entry points and the dispatch shape.
//!
//! - [`wire`] — the request/response/output-item types.
//! - [`input`] — messages → `input` item lowering (shared by both body paths).
//! - [`body`] — request-body assembly around that lowered input.
//! - [`turn`] — the reasoning-item artifact and the response → turn conversion.
//! - [`stream`] — the streaming turn with tools (SSE loop + accumulator).

use tracing::{debug, info};

use super::retry;
use super::{ResponsesStreamEvent, SseReader, parse_responses_stream_event};
use crate::shared::RequestAuth;
use crate::types::{ChatTurnResult, StreamEvent};
use std::io;

mod body;
mod input;
mod stream;
mod turn;
mod wire;

use body::{build_responses_request_body, build_simple_responses_body};
// The caller index is part of the lowering's signature, so the test suite names
// it directly to pass an empty one.
#[cfg(test)]
pub(crate) use input::CallerIndex;
pub(crate) use input::{SystemPrompt, messages_to_responses_input};
pub(crate) use stream::responses_request_streaming_with_tools;
use turn::incomplete_stop;
pub(crate) use turn::responses_response_to_turn;
// The item type is named only by the test suite now that the lowering lives in
// `input` and reaches `wire` directly.
#[cfg(test)]
pub(crate) use wire::ResponsesInputItem;
use wire::{ResponseOutputItem, ResponsesResponse};

// ── Simple (no-tool) responses request ───────────────────────────────────

pub(crate) fn responses_request(
    agent: &ureq::Agent,
    config: &super::ServiceConfig,
    auth: &RequestAuth,
    model: &str,
    prompt: &str,
    cancel_rx: Option<&crossbeam_channel::Receiver<()>>,
) -> Result<String, super::OpenAiError> {
    let (url, body) = build_simple_responses_body(config, model, prompt, false)?;
    let retry = retry::retry_config_from_config(config);
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
            model,
            config,
            retry_cfg: &retry,
            route: None,
            on_uploaded: None,
        },
        &mut ctx,
    )?;
    let payload: ResponsesResponse = response
        .into_body()
        .read_json()
        .map_err(|e| super::OpenAiError::Io(io::Error::other(e)))?;

    // A policy block is terminal even on the one-shot path. There is no turn
    // result here to carry a truncation flag, so the reason only decides
    // whether the reply is an error; the partial text below still has to reach
    // a non-empty answer on its own.
    incomplete_stop(&payload)?;

    let mut content = String::new();
    for raw in payload.output {
        // Parse each raw item for message-text extraction; a malformed item
        // surfaces as an error (matching the previous typed-deserialization
        // behavior) rather than being silently skipped.
        let item: ResponseOutputItem =
            serde_json::from_value(raw).map_err(|e| super::OpenAiError::Io(io::Error::other(e)))?;
        if let ResponseOutputItem::Message { content: parts, .. } = item
            && let Some(text) = parts
                .into_iter()
                .filter_map(|part| part.text)
                .map(|text| text.trim().to_string())
                .find(|text| !text.is_empty())
        {
            content = text;
            break;
        }
    }

    if content.is_empty() {
        return Err(super::OpenAiError::EmptyResponse);
    }

    Ok(content)
}

// ── Simple streaming responses request ───────────────────────────────────

pub(crate) fn responses_request_streaming<F>(
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
        cancel_rx,
        ..
    } = params;
    let (url, body) = build_simple_responses_body(config, model, prompt, true)?;
    let retry = retry::retry_config_from_config(config);
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
            model,
            config,
            retry_cfg: &retry,
            route: None,
            on_uploaded: None,
        },
        &mut ctx,
    )?;
    let mut reader = SseReader::from_reader(response.into_body().into_reader());
    // Reader thread decouples the blocking socket read from cancellation
    // polling (see `crate::stream`); the abort flag on `sse` stops the thread
    // at its next loop boundary once the consumer cancels or drops it.
    let context = crate::stream::SseContext {
        provider: config.provider_slug.clone(),
        model: model.to_owned(),
    };
    let sse =
        crate::stream::spawn_sse_reader(context, move || reader.next_event(), deadline.current());
    let mut has_any_output = false;
    while let Some(data) = crate::stream::recv_sse_event(&sse, cancel_rx)? {
        let event = parse_responses_stream_event(&data)?;
        match event {
            Some(ResponsesStreamEvent::TextDelta(text)) => {
                has_any_output = true;
                on_event(StreamEvent::Answer(text))?;
            }
            Some(ResponsesStreamEvent::TextDone) => {
                // Text output is complete; continue waiting for completion event.
            }
            Some(ResponsesStreamEvent::ResponseCompleted { .. }) => {
                break;
            }
            Some(ResponsesStreamEvent::ResponseFailed(error)) => {
                return Err(super::OpenAiError::Io(io::Error::other(error)));
            }
            Some(ResponsesStreamEvent::ResponseIncomplete { reason, .. }) => {
                // A one-shot prompt has no artifact to carry and no turn result
                // to flag, so a policy block is the only outcome it can report:
                // `has_any_output` below decides whether the partial text is a
                // usable answer.
                if reason.as_deref() == Some("content_filter") {
                    return Err(super::OpenAiError::ContentFiltered {
                        detail: "provider content filter stopped the response \
                                 (response.incomplete: content_filter)"
                            .into(),
                    });
                }
                break;
            }
            _ => {}
        }
    }

    if !has_any_output {
        return Err(super::OpenAiError::EmptyResponse);
    }

    Ok(())
}

// ── Non-streaming responses with tools ───────────────────────────────────

/// Non-streaming Responses API turn with tool definitions, reasoning effort,
/// and optional tool results from a previous turn.
pub(crate) fn responses_request_with_tools(
    agent: &ureq::Agent,
    config: &super::ServiceConfig,
    auth: &RequestAuth,
    params: super::TurnParams<'_>,
) -> Result<ChatTurnResult, super::OpenAiError> {
    let start = std::time::Instant::now();

    let (url, body) = build_responses_request_body(config, &params, false)?;
    let super::TurnParams {
        model,
        messages,
        tools,
        on_retry,
        on_uploaded,
        cancel_rx,
        route,
        no_retry,
        ..
    } = params;

    let has_instructions = messages.iter().any(|m| m.role == "system");
    info!(
        model = %model,
        tool_count = tools.len(),
        has_instructions = has_instructions,
        "responses request with tools",
    );

    let retry = retry::retry_config_from_config(config).with_no_retry(no_retry);
    // The upload hook fires once the payload is fully sent, so a non-streaming
    // provider reports the same `Waiting` boundary as the streaming path.
    let mut ctx = retry::AttemptContext::new(on_retry, cancel_rx, None);
    let response = retry::retry_send(
        retry::RetrySend {
            agent,
            url: &url,
            auth,
            body: &body,
            model,
            config,
            retry_cfg: &retry,
            route,
            on_uploaded,
        },
        &mut ctx,
    )?;
    let payload: ResponsesResponse = response
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
        "responses turn",
    );

    responses_response_to_turn(payload)
}

// The suite reaches its subjects through this module, so the moved items are
// re-exported here for it (`use super::*;` in the tests).
#[cfg(test)]
use body::*;
#[cfg(test)]
use stream::*;
#[cfg(test)]
use turn::*;
#[cfg(test)]
use wire::*;

// The test suite lives in a sibling file so the implementation stays
// readable at a glance; the module path is unchanged, so the moved tests
// reach their subjects through the same `super` paths.
#[cfg(test)]
mod tests;
