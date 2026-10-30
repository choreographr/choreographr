//! The streaming Responses API turn with tools.
//!
//! Owns the SSE loop, the per-call tool-call accumulator (a call is assembled
//! from its `function_call` output item and the argument events that name it),
//! and the streaming reasoning/usage assembly — the streaming counterpart of the
//! non-streaming request entry points in [`super`].

use std::collections::HashMap;
use std::io;

use tracing::{debug, info, trace, warn};

use crate::shared::{MAX_TOOL_CALLS, RequestAuth};
use crate::types::{CallerInfo, ChatToolCall, ChatTurnResult, StreamEvent};
use choreo_proto::TokenUsage;

use super::super::retry;
use super::super::{CallRef, ResponsesStreamEvent, SseReader, parse_responses_stream_event};
use super::body::build_responses_request_body;
use super::turn::{merge_reasoning_item, responses_items_artifact, responses_usage_to_token_usage};
use crate::openai::turn::{TurnParts, assemble_turn};

/// One function call the stream is assembling, held under the identity the
/// stream named it with (its output item's id, or its call id).
///
/// # What each event contributes
///
/// - `response.output_item.*` ([`ResponsesStreamEvent::FunctionCallItem`]) —
///   the call's *identity*: the function's name, which no other event on the
///   wire carries, plus the `call_id` the request echoes back. The item also
///   carries its own `arguments` copy, complete on `output_item.done`; on
///   `output_item.added` it is empty.
/// - `response.function_call_arguments.delta` — a chunk of JSON to **append**
///   to the accumulator (`arguments.push_str(&delta)`).
/// - `response.function_call_arguments.done` — the **complete** final arguments
///   string. When received, `arguments` is **replaced** (not appended),
///   overwriting any partial accumulation from prior delta events.
///
/// That matches the `OpenAI` spec: the done event carries the full final
/// arguments JSON, while delta events carry incremental fragments. The item's
/// own copy is held to the same rule, minus the empty one — see [`AccCalls::absorb`].
pub(super) struct AccCall {
    /// The provider's `call_id`; the wire may omit it, in which case the
    /// request answers the call by whichever identity it did give.
    pub(super) call_id: Option<String>,
    /// The function the model asked for, named by the call's output item.
    pub(super) name: Option<String>,
    pub(super) arguments: String,
    /// The provider's `caller` attribution for this call, captured from its
    /// `function_call` output item. Kept so the finished call carries it back
    /// onto the rebuilt request: the API needs the same value on the
    /// `function_call_output` that answers the call to resume the generated
    /// program.
    pub(super) caller: Option<CallerInfo>,
    /// Monotonically increasing index assigned to this tool call, used to
    /// preserve insertion order since the Responses API does not provide
    /// server-side indices like Chat Completions does.
    pub(super) index: u32,
}

impl AccCall {
    pub(super) fn new(index: u32) -> Self {
        Self {
            call_id: None,
            name: None,
            arguments: String::new(),
            caller: None,
            index,
        }
    }
}

/// The streamed tool calls of one turn, keyed by the identity the stream named
/// each call with — its output item's id, falling back to its call id.
///
/// The Responses API assigns no server-side index, so the accumulator mints one
/// per first-seen call to keep the model's call order. The map is the call-count
/// bound: a *new* identity past [`MAX_TOOL_CALLS`] is rejected, while a later
/// event for an already-known call is always accepted — a full map must not
/// turn a legal continuation into an error.
#[derive(Default)]
pub(super) struct AccCalls {
    by_id: HashMap<String, AccCall>,
    next_index: u32,
}

impl AccCalls {
    /// The accumulator slot for one call identity, minting one when this is the
    /// first time the stream names it.
    pub(super) fn entry(
        &mut self,
        call_id: String,
    ) -> Result<&mut AccCall, super::super::OpenAiError> {
        if !self.by_id.contains_key(&call_id) && self.by_id.len() >= MAX_TOOL_CALLS {
            return Err(super::super::OpenAiError::Io(io::Error::other(format!(
                "too many tool calls (max {MAX_TOOL_CALLS})"
            ))));
        }
        let next_index = &mut self.next_index;
        Ok(self.by_id.entry(call_id).or_insert_with(|| {
            let index = *next_index;
            *next_index = next_index.saturating_add(1);
            AccCall::new(index)
        }))
    }

    /// The slot one event names, minting it the first time the stream mentions
    /// the call.
    ///
    /// An output-item id keys its own slot, because that is the join the
    /// documented argument events use. A bare call id is resolved against the
    /// slots already minted — a stream that sends output items *and* names only
    /// call ids on its argument events must still land on the call's one slot,
    /// or the same tool would be run twice — and only keys a new slot when
    /// nothing claims it.
    fn slot(&mut self, call: &CallRef) -> Result<&mut AccCall, super::super::OpenAiError> {
        let key = match call {
            CallRef::Item(item_id) => item_id.clone(),
            CallRef::Call(call_id) => self
                .by_id
                .iter()
                .find(|(_, acc)| acc.call_id.as_deref() == Some(call_id.as_str()))
                .map_or_else(|| call_id.clone(), |(key, _)| key.clone()),
        };
        self.entry(key)
    }

    /// The accumulated calls in the model's own order.
    pub(super) fn into_ordered(self) -> Vec<(String, AccCall)> {
        let mut sorted: Vec<(String, AccCall)> = self.by_id.into_iter().collect();
        sorted.sort_by_key(|(_, acc)| acc.index);
        sorted
    }

    /// Apply one call-related stream event to the accumulator.
    ///
    /// The three call events are handled here rather than in the stream loop so
    /// the accumulation rules (append a delta, replace with `done`, name and
    /// attribute the call from its item) are exercisable without a socket.
    pub(super) fn absorb(
        &mut self,
        event: &ResponsesStreamEvent,
    ) -> Result<(), super::super::OpenAiError> {
        match event {
            ResponsesStreamEvent::FunctionCallItem {
                item_id,
                call_id,
                name,
                arguments,
                caller,
            } => {
                // The item is where a streamed call is identified: the argument
                // events name nothing but the item they belong to. An item that
                // names neither id can't be answered by any
                // `function_call_output`, so there is nothing to accumulate.
                let Some(call) = CallRef::of(item_id.as_deref(), call_id.as_deref()) else {
                    return Ok(());
                };
                let entry = self.slot(&call)?;
                if let Some(call_id) = call_id.as_deref() {
                    entry.call_id = Some(call_id.to_string());
                }
                if let Some(name) = name.as_deref() {
                    entry.name = Some(name.to_string());
                }
                // The item carries its own copy of the arguments — complete on
                // `output_item.done` — so it replaces whatever the deltas built.
                // The empty copy on `output_item.added` is not a value, and must
                // not wipe arguments that already arrived.
                if !arguments.is_empty() {
                    entry.arguments.clone_from(arguments);
                }
                if caller.is_some() {
                    entry.caller.clone_from(caller);
                }
                trace!(
                    tool_name = ?entry.name,
                    args_len = entry.arguments.len(),
                    "openai responses: function call item",
                );
            }
            ResponsesStreamEvent::FunctionCallArgumentsDelta { call, delta } => {
                // `call` is provided by the trusted API server — no validation
                // needed; it is used directly as the accumulator key for this
                // specific tool call.
                self.slot(call)?.arguments.push_str(delta);
            }
            ResponsesStreamEvent::FunctionCallArgumentsDone { call, arguments } => {
                // `arguments` is the complete final JSON string (not a delta), so
                // it replaces any accumulated partial value.
                self.slot(call)?.arguments.clone_from(arguments);
            }
            // Not a call event: the stream loop routes only the three variants
            // above here.
            _ => {}
        }
        Ok(())
    }

    /// The finished tool calls in the model's own order, plus how many calls the
    /// stream mentioned without ever naming — a call with no function name can't
    /// be run, so it is counted for the turn's verdict rather than invented.
    pub(super) fn finish(self) -> FinishedCalls {
        let ordered = self.into_ordered();
        let mentioned = ordered.len();
        let calls: Vec<ChatToolCall> = ordered
            .into_iter()
            .filter_map(|(identity, acc)| {
                let name = acc.name.as_deref().filter(|n| !n.is_empty())?;
                Some(ChatToolCall {
                    // The request answers the call by `call_id`; a stream that
                    // never named one is answered by the identity it did give.
                    id: acc.call_id.unwrap_or(identity),
                    name: name.to_string(),
                    arguments_json: acc.arguments,
                    caller: acc.caller,
                })
            })
            .collect();
        let unnamed = mentioned - calls.len();
        if unnamed > 0 {
            warn!(
                dropped = unnamed,
                "the provider mentioned function call(s) without naming them \
                 (the stream carried no function_call output item)"
            );
        }
        FinishedCalls { calls, unnamed }
    }
}

/// What one streamed turn's call accumulation produced.
#[derive(Debug)]
pub(super) struct FinishedCalls {
    /// The calls the model made, in its own order.
    pub(super) calls: Vec<ChatToolCall>,
    /// How many calls the stream mentioned without naming the function. They
    /// cannot be run, so the turn has to account for them rather than reporting
    /// an answer that silently omits them.
    pub(super) unnamed: usize,
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
    // The accumulator reports a call whose output item never arrived (nothing on
    // the wire names the function) so the assembler can fail the turn loudly
    // rather than answer with nothing.
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
