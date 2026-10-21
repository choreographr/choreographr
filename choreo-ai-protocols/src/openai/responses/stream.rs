//! The streaming Responses API turn with tools.
//!
//! Owns the SSE loop, the per-`call_id` tool-call accumulator, and the
//! streaming reasoning/usage assembly — the streaming counterpart of the
//! non-streaming request entry points in [`super`].

use std::collections::HashMap;
use std::io;

use tracing::{debug, info, trace, warn};

use crate::shared::{MAX_TOOL_CALLS, RequestAuth};
use crate::types::{
    ChatAssistantToolUse, ChatToolCall, ChatTurnResult, FinalTextResult, StreamEvent,
};
use choreo_proto::TokenUsage;

use super::super::retry;
use super::super::{
    ResponsesStreamEvent, SseReader, parse_responses_stream_event, validate_tool_call_arguments,
};
use super::body::build_responses_request_body;
use super::turn::{merge_reasoning_item, responses_items_artifact, responses_usage_to_token_usage};

/// Accumulator for Responses API tool call arguments keyed by `call_id`.
/// Used in `responses_request_streaming_with_tools` to merge delta chunks.
///
/// # Delta-vs-Done semantics
///
/// The Responses API sends tool call arguments via two SSE event types:
///
/// - `FunctionCallArgumentsDelta` — a chunk of JSON to **append** to the
///   existing arguments accumulator (`arguments.push_str(&delta)`).
/// - `FunctionCallArgumentsDone` — the **complete** final arguments string.
///   When received, `arguments` is **replaced** (not appended), overwriting
///   any partial accumulation from prior delta events.
///
/// This matches the `OpenAI` spec: the done event carries the full final
/// arguments JSON, while delta events carry incremental fragments.
pub(super) struct AccCall {
    pub(super) name: Option<String>,
    pub(super) arguments: String,
    /// Monotonically increasing index assigned to this tool call, used to
    /// preserve insertion order since the Responses API does not provide
    /// server-side indices like Chat Completions does.
    pub(super) index: u32,
}

impl AccCall {
    pub(super) fn new(index: u32) -> Self {
        Self {
            name: None,
            arguments: String::new(),
            index,
        }
    }
}

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
        tool_results,
        no_retry,
        ..
    } = params;

    info!(
        model = %model,
        tool_count = tools.len(),
        tool_result_count = tool_results.len(),
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
            config,
            retry_cfg: &retry,
            route,
            on_uploaded,
        },
        &mut ctx,
    )?;

    let mut has_any_output = false;
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

    let mut acc_calls: HashMap<String, AccCall> = HashMap::new();
    let mut next_tool_index: u32 = 0;

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
                has_any_output = true;
                full_content.push_str(&text);
                on_event(StreamEvent::Answer(text))?;
            }
            Some(ResponsesStreamEvent::TextDone) => {
                // Text output is complete — continue listening for the
                // completion / failure events and any remaining tool calls.
            }
            Some(ResponsesStreamEvent::ReasoningSummaryText(text)) => {
                has_any_output = true;
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
            Some(ResponsesStreamEvent::FunctionCallArgumentsDelta { call_id, delta }) => {
                // `call_id` is provided by the trusted API server — no validation
                // needed; it's used directly as a HashMap key to accumulate deltas
                // for this specific tool call.
                has_any_output = true;
                if acc_calls.len() >= MAX_TOOL_CALLS {
                    return Err(super::super::OpenAiError::Io(io::Error::other(format!(
                        "too many tool calls (max {MAX_TOOL_CALLS})"
                    ))));
                }
                let entry = acc_calls.entry(call_id).or_insert_with(|| {
                    let i = next_tool_index;
                    next_tool_index += 1;
                    AccCall::new(i)
                });
                entry.arguments.push_str(&delta);
            }
            Some(ResponsesStreamEvent::FunctionCallArgumentsDone {
                call_id,
                name,
                arguments,
            }) => {
                // `call_id` comes from the trusted API server — safe to use as a
                // HashMap key.  `arguments` is the complete final JSON string
                // (not a delta), so it replaces any accumulated partial value.
                has_any_output = true;
                let entry = acc_calls.entry(call_id).or_insert_with(|| {
                    let i = next_tool_index;
                    next_tool_index += 1;
                    AccCall::new(i)
                });
                entry.name = Some(name);
                entry.arguments = arguments;
                trace!(
                    tool_name = ?entry.name,
                    args_len = entry.arguments.len(),
                    "openai responses: function call args done",
                );
            }
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
    // whose summary this client could not read) is output the provider billed
    // for, so it must never surface as an empty response. The chat adapter's
    // reasoning fold reports the same fact the same way.
    if !has_any_output && reasoning_items.is_empty() {
        return Err(super::super::OpenAiError::EmptyResponse);
    }

    if !acc_calls.is_empty() {
        let mut sorted: Vec<(String, AccCall)> = acc_calls.into_iter().collect();
        sorted.sort_by_key(|(_, acc)| acc.index);
        let total_calls = sorted.len();

        // First pass: filter out calls without a name (the provider sent
        // incomplete function call events, e.g. arguments delta without a
        // done event).
        let mut tool_calls: Vec<ChatToolCall> = sorted
            .into_iter()
            .filter_map(|(call_id, acc)| {
                let name = acc.name.as_deref().filter(|n| !n.is_empty())?;
                Some(ChatToolCall {
                    id: call_id,
                    name: name.to_string(),
                    arguments_json: acc.arguments,
                    caller: None,
                })
            })
            .collect();

        // Second pass: discard calls with truncated arguments JSON.
        let discarded = validate_tool_call_arguments(&mut tool_calls);

        if !tool_calls.is_empty() {
            return Ok(ChatTurnResult::ToolUse(ChatAssistantToolUse {
                content: if full_content.is_empty() {
                    None
                } else {
                    Some(full_content)
                },
                tool_calls,
                reasoning: if full_reasoning.is_empty() {
                    None
                } else {
                    Some(full_reasoning)
                },
                usage: last_usage,
                response_id,
                reasoning_artifact,
            }));
        }

        // All calls were discarded — show a user-visible error in the TUI,
        // not just a log line, unless the model also produced text.
        if !discarded.is_empty() && !full_content.is_empty() {
            return Ok(ChatTurnResult::FinalText(FinalTextResult {
                content: full_content,
                truncated: false,
                reasoning: if full_reasoning.is_empty() {
                    None
                } else {
                    Some(full_reasoning)
                },
                usage: last_usage,
                response_id,
                reasoning_artifact,
            }));
        }
        if !discarded.is_empty() {
            return Err(super::super::OpenAiError::TruncatedToolCall { discarded });
        }
        warn!("discarded {total_calls} incomplete tool call(s) from provider (no name set)",);
    }

    // If only reasoning arrived and no answer text, the turn is still model
    // output — it becomes a `FinalText` with empty content that carries the
    // reasoning and its round-trip artifact. Only a turn that produced neither
    // content nor reasoning is empty.
    if full_content.is_empty() && reasoning_artifact.is_none() {
        return Err(super::super::OpenAiError::EmptyResponse);
    }

    Ok(ChatTurnResult::FinalText(FinalTextResult {
        content: full_content,
        truncated,
        reasoning: if full_reasoning.is_empty() {
            None
        } else {
            Some(full_reasoning)
        },
        usage: last_usage,
        response_id,
        reasoning_artifact,
    }))
}
