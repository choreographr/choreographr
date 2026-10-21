//! Streamed chat-completions accumulation.
//!
//! A streamed chat turn arrives as a sequence of SSE chunks whose content,
//! reasoning, and tool-call fields are all split across deltas: tool calls are
//! keyed by the API-assigned index and their `name` / `arguments` arrive in
//! fragments, usage lands on a final chunk with an empty `choices` array, and
//! the finish reason arrives once at the end. [`ChatCompletionsStreamAccumulator`]
//! folds those chunks into one turn result, and
//! [`accumulate_tool_calls_from_deltas`] merges the per-index tool-call deltas
//! into ordered calls.
//!
//! The concern is split out of the request path so the parse→accumulate→turn
//! pipeline is unit-testable without an HTTP connection.

use std::collections::HashMap;
use std::io;

use choreo_proto::TokenUsage;
use tracing::debug;

use super::reasoning_stream::ReasoningStreamState;
use super::{
    ChatCompletionsStreamResponse, FinishReason, StreamToolCallDelta, finish_reason_terminal_error,
};
use crate::openai::OpenAiError;
use crate::openai::turn::{TurnParts, assemble_turn};
use crate::shared::MAX_TOOL_CALLS;
use crate::types::{ChatToolCall, ChatTurnResult, StreamEvent};

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
#[derive(Debug, Default)]
pub(super) struct ChatCompletionsStreamAccumulator {
    full_content: String,
    /// The turn's streaming reasoning: both capture channels, the locked scalar
    /// field, and the channel that owns the live display (see
    /// [`ReasoningStreamState`]).
    reasoning: ReasoningStreamState,
    /// Raw tool call deltas across all chunks, merged by index by
    /// `accumulate_tool_calls_from_deltas` once the stream is fully consumed.
    raw_tool_call_deltas: Vec<StreamToolCallDelta>,
    /// Usage from the final SSE chunk (`OpenAI` sends a usage chunk with
    /// choices: [] when `stream_options.include_usage` is true).
    last_usage: Option<TokenUsage>,
    /// Finish reason from the final SSE chunk (last non-None wins — the
    /// provider sends it exactly once, but tolerate repeats defensively).
    finish_reason: Option<FinishReason>,
}

impl ChatCompletionsStreamAccumulator {
    /// Fold one streamed chunk into the accumulated state, forwarding
    /// content/reasoning deltas to `on_event` immediately so subscribers
    /// see them in real time.
    pub(super) fn apply(
        &mut self,
        payload: &ChatCompletionsStreamResponse,
        on_event: &mut impl FnMut(StreamEvent) -> io::Result<()>,
    ) -> Result<(), OpenAiError> {
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

            // Content chunks: answer text. Emitted before the reasoning
            // channels so a delta carrying both keeps the reply ahead of the
            // thinking — the same order the simple streaming path and the
            // non-streaming bridge use; the client stores the two in separate
            // turn fields and renders the answer above a collapsible reasoning
            // section.
            if let Some(content) = delta.content.as_ref().filter(|c| !c.is_empty()) {
                self.full_content.push_str(content);
                on_event(StreamEvent::Answer(content.clone()))?;
            }

            // Both reasoning channels fold through one shared state machine,
            // which owns the display-claim rule and the capture of the other
            // channel. Whether the turn produced any output is derived from the
            // accumulated content, tool calls, and captured artifact when the
            // result is assembled, so the fold's produced flag is not tracked
            // here.
            self.reasoning.apply(delta, on_event)?;
            // Collect raw tool call deltas — the shared accumulator
            // (accumulate_tool_calls_from_deltas) will merge them by index
            // and produce sorted ChatToolCall output after the stream ends.
            if let Some(ref tcs) = delta.tool_calls {
                for tc in tcs {
                    // One call per index, so bounding the index bounds the call
                    // count: an out-of-range index is the only way a route can
                    // exceed `MAX_TOOL_CALLS`, and it is rejected here rather
                    // than silently truncated into a colliding slot.
                    if (tc.index as usize) >= MAX_TOOL_CALLS {
                        return Err(OpenAiError::Io(io::Error::other(format!(
                            "tool call index {} out of bounds (max {})",
                            tc.index,
                            MAX_TOOL_CALLS - 1,
                        ))));
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
    pub(super) fn into_turn_result(self) -> Result<ChatTurnResult, OpenAiError> {
        // A declared terminal finish reason wins over the accumulated body:
        // z.ai's `sensitive` streams no content, which would otherwise fall
        // through to the (retryable-looking) EmptyResponse error.
        if let Some(err) = finish_reason_terminal_error(self.finish_reason.as_ref()) {
            return Err(err);
        }
        // `length` on a FINAL-TEXT turn is a truncation notice, not an error.
        let truncated = matches!(self.finish_reason, Some(FinishReason::Length));

        // The state machine's finish applies the persisted precedence: a captured
        // structured details payload outranks the scalar text as the artifact (it
        // is the richer payload the route replays), and the scalar text supplies
        // the display unless it is empty, in which case the accumulated structured
        // text stands in. `produced` is the fold's own record of whether the
        // provider sent reasoning at all, so a payload dropped by the replay cap
        // still keeps the turn from reading as an empty response.
        let reasoning = self.reasoning.finish();

        // The shared assembler owns the tool-call / truncation / empty-response
        // decision tree (identical to the non-streaming path); a tool-call
        // message forwards only non-empty content.
        let tool_use_content = (!self.full_content.is_empty()).then(|| self.full_content.clone());
        let content_is_empty = self.full_content.is_empty();
        let tool_calls = accumulate_tool_calls_from_deltas(self.raw_tool_call_deltas);
        assemble_turn(TurnParts {
            tool_use_content,
            final_text: self.full_content,
            content_is_empty,
            tool_calls,
            reasoning: reasoning.display,
            reasoning_artifact: reasoning.artifact,
            produced_reasoning: reasoning.produced,
            usage: self.last_usage,
            truncated,
            response_id: None,
        })
    }
}
