//! The streaming Responses API turn with tools.
//!
//! Owns the SSE loop and the streaming reasoning/usage assembly — the streaming
//! counterpart of the non-streaming request entry points in [`super`]. The tool
//! calls the stream names are assembled by [`super::accumulator`], which this
//! loop feeds one call event at a time.

use std::io;

use tracing::{debug, info, warn};

use crate::shared::RequestAuth;
use crate::types::{ChatTurnResult, StreamEvent};
use choreo_proto::TokenUsage;

use super::super::retry;
use super::super::{ResponsesStreamEvent, SseReader, parse_responses_stream_event};
use super::accumulator::AccCalls;
use super::body::build_responses_request_body;
use super::turn::{merge_reasoning_item, responses_items_artifact, responses_usage_to_token_usage};
use crate::openai::turn::{TurnParts, assemble_turn};

// ── Streaming responses with tools ───────────────────────────────────────

/// Streaming Responses API turn with tool definitions, reasoning effort,
/// tool results, retry support, and cancellation.
///
/// Sends `stream: true` with tool definitions, reads SSE chunks, and calls
/// `on_chunk` for each content / reasoning delta so the caller can forward
/// it to subscribers immediately.  Tool call deltas are accumulated across
/// chunks and returned as `ChatTurnResult::ToolUse` when the stream ends.
pub(crate) fn responses_request_streaming_with_tools<F>(
    agent: &ureq::Agent,
    config: &super::super::ServiceConfig,
    auth: &RequestAuth,
    params: super::super::TurnParams<'_>,
    on_event: &mut F,
) -> Result<ChatTurnResult, super::super::OpenAiError>
where
    F: FnMut(StreamEvent) -> io::Result<()>,
{
    let (url, body) = build_responses_request_body(config, &params, true)?;
    let super::super::TurnParams {
        model,
        messages: _,
        tools,
        on_retry,
        on_uploaded,
        cancel_rx,
        route,
        no_retry,
        ..
    } = params;

    info!(
        model = %model,
        tool_count = tools.len(),
        streaming = true,
        "responses streaming request with tools",
    );

    let retry = retry::retry_config_from_config(config).with_no_retry(no_retry);
    // Per-attempt wall-clock deadline spanning the whole request (see `retry::AttemptDeadline`).
    let mut deadline = retry::AttemptDeadline::new(config.total_timeout_secs);
    let mut ctx = retry::AttemptContext::new(on_retry, cancel_rx, Some(&mut deadline));
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

    let mut full_content = String::new();
    let mut full_reasoning = String::new();
    let mut last_usage: Option<TokenUsage> = None;
    let mut response_id: Option<String> = None;
    // The response stopped early with output it did keep (`max_output_tokens`
    // and any reason this client does not classify): the partial answer rides
    // a `FinalText` with this flag rather than failing the turn, exactly as
    // chat maps `finish_reason: length`.
    let mut truncated = false;
    // Opaque reasoning items collected across the stream (deduped by id in
    // `merge_reasoning_item`) — serialized into the round-trip artifact at
    // the end.
    let mut reasoning_items: Vec<serde_json::Value> = Vec::new();

    let mut acc_calls = AccCalls::default();

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
    while let Some(data) = crate::stream::recv_sse_event(&sse, cancel_rx)? {
        let event = parse_responses_stream_event(&data)?;
        match event {
            Some(ResponsesStreamEvent::TextDelta(text)) => {
                full_content.push_str(&text);
                on_event(StreamEvent::Answer(text))?;
            }
            Some(ResponsesStreamEvent::TextDone) => {
                // Text output is complete — continue listening for the
                // completion / failure events and any remaining tool calls.
            }
            Some(ResponsesStreamEvent::ReasoningSummaryText(text)) => {
                full_reasoning.push_str(&text);
                on_event(StreamEvent::Reasoning(text))?;
            }
            Some(ResponsesStreamEvent::ReasoningItem(item)) => {
                // Capture the opaque reasoning item verbatim (id, summary,
                // and encrypted_content in stateless mode) for the
                // round-trip artifact. `output_item.added` and `.done` both
                // map here — merge_reasoning_item dedupes by id.
                //
                // The item also makes this a produced response even when it
                // carries nothing displayable (an encrypted-only item): the
                // provider generated and billed it, so the turn must never be
                // reported as an empty response. That fact is read from
                // `reasoning_items` at the end, so it survives an item the
                // dedupe replaced.
                merge_reasoning_item(&mut reasoning_items, item);
            }
            Some(
                event @ (ResponsesStreamEvent::FunctionCallItem { .. }
                | ResponsesStreamEvent::FunctionCallArgumentsDelta { .. }
                | ResponsesStreamEvent::FunctionCallArgumentsDone { .. }),
            ) => acc_calls.absorb(&event)?,
            Some(ResponsesStreamEvent::ResponseCompleted { id, usage }) => {
                response_id = id;
                if let Some(u) = usage {
                    debug!(
                        prompt_tokens = u.prompt_tokens,
                        completion_tokens = u.completion_tokens,
                        total_tokens = u.total_tokens,
                        "responses streaming turn usage",
                    );
                    last_usage = Some(responses_usage_to_token_usage(&u));
                }
                break;
            }
            Some(ResponsesStreamEvent::ResponseFailed(error)) => {
                return Err(super::super::OpenAiError::Io(io::Error::other(error)));
            }
            Some(ResponsesStreamEvent::ResponseIncomplete { id, usage, reason }) => {
                // A terminal SUCCESS with possibly-partial output. The provider
                // names why it stopped under `incomplete_details.reason`;
                // `max_output_tokens` (and any reason this client does not
                // classify) leaves a partial answer we surface as a truncation,
                // while a policy block is terminal and never retryable.
                response_id = id;
                if let Some(u) = usage {
                    last_usage = Some(responses_usage_to_token_usage(&u));
                }
                match reason.as_deref() {
                    Some("content_filter") => {
                        return Err(super::super::OpenAiError::ContentFiltered {
                            detail: "provider content filter stopped the response \
                                     (response.incomplete: content_filter)"
                                .into(),
                        });
                    }
                    other => {
                        if let Some(reason) = other
                            && reason != "max_output_tokens"
                        {
                            warn!(reason, "responses stream ended incomplete");
                        }
                        truncated = true;
                        break;
                    }
                }
            }
            Some(ResponsesStreamEvent::ProgramCodeDelta(delta)) => {
                // Streaming JS code from the model-generated program that runs
                // in OpenAI's hosted V8. We just observe it; the function_call
                // items it triggers come through as separate SSE events.
                tracing::trace!(delta_len = delta.len(), "program code delta");
            }
            Some(ResponsesStreamEvent::ProgramCodeDone {
                call_id,
                fingerprint,
            }) => {
                // The program definition is complete. The `fingerprint` is an
                // opaque token for resuming/replaying this program.
                tracing::debug!(
                    %call_id,
                    has_fingerprint = fingerprint.is_some(),
                    "program code complete",
                );
            }
            Some(ResponsesStreamEvent::ProgramOutputDone {
                call_id,
                result,
                status,
            }) => {
                // The program finished with a result. `status` is "completed"
                // or "incomplete". A final `message` item may follow in a
                // later SSE event; the caller must keep reading.
                tracing::debug!(
                    %call_id,
                    result_len = result.len(),
                    %status,
                    "program output done",
                );
            }
            _ => {
                // Unknown event types are silently ignored.
            }
        }
    }

    let reasoning_artifact = responses_items_artifact(&reasoning_items)?;

    // "The model produced output" is a fact about the response, not about what
    // survived into a display string: an encrypted-only reasoning item (or one
    // whose summary this client could not read, or one the byte cap refused) is
    // output the provider billed for, so it must never surface as an empty
    // response. The chat adapter's reasoning fold reports the same fact the same
    // way.
    let produced_reasoning = !reasoning_items.is_empty() || !full_reasoning.is_empty();

    // The shared assembler owns the tool-call / truncation / empty-response
    // decision tree, so this path and the non-streaming one cannot disagree.
    // The accumulator reports a call nothing on the stream ever named — neither
    // an output item nor the name a gateway may put on the arguments event — so
    // the assembler can fail the turn loudly rather than answer with nothing.
    let calls = acc_calls.finish();

    // The assembler owns the normalization, so the accumulated answer is handed
    // over raw (moved, never copied): `None` says there was no content at all,
    // and trimming plus the empty-content verdict both happen there.
    assemble_turn(TurnParts {
        content: (!full_content.is_empty()).then_some(full_content),
        tool_calls: calls.calls,
        unnamed_calls: calls.unnamed,
        reasoning: (!full_reasoning.is_empty()).then_some(full_reasoning),
        reasoning_artifact,
        produced_reasoning,
        usage: last_usage,
        truncated,
        response_id,
    })
}
