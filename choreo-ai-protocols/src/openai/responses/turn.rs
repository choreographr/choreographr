//! Turning a Responses API reply into a turn result.
//!
//! Owns the round-trip artifact for this adapter — the provider's `reasoning`
//! output items (with `encrypted_content` where present), serialized exactly as
//! received so they can be replayed into `input` — and the response →
//! [`ChatTurnResult`] conversion that hands its parts to the crate's shared
//! [`assemble_turn`].

use std::io;

use tracing::{debug, warn};

use super::super::reasoning_details::MAX_REASONING_BYTES;
use super::wire::{ResponseOutputItem, ResponsesResponse};
use crate::openai::turn::{TurnParts, assemble_turn};
use crate::types::{ChatToolCall, ChatTurnResult};
use choreo_proto::{ReasoningArtifact, TokenUsage};

/// Project the Responses API usage onto the shared token counter.
///
/// The Responses API's cache-hit analogue lives in
/// `input_tokens_details.cached_tokens`, which is one of the shapes
/// [`Usage::to_token_usage`](super::super::Usage::to_token_usage) resolves — so this path and every chat-completions
/// path share ONE projection and cannot disagree about the cache count. The
/// Responses wire carries no cache-write count, so `cache_write_tokens` stays
/// zero there.
pub(super) fn responses_usage_to_token_usage(usage: &super::super::Usage) -> TokenUsage {
    usage.to_token_usage()
}

/// Classify a response that stopped early (`status: "incomplete"`).
///
/// `incomplete` is a terminal SUCCESS with possibly-partial output, so its
/// reason decides what the caller does: a policy block is a terminal error
/// that retrying can never clear, and every other reason
/// (`max_output_tokens` included, and the absent-reason case) yields a partial
/// answer reported as truncated — the same reading chat gives
/// `finish_reason: length`. An unknown reason is logged so it stays
/// diagnosable without failing a turn the provider charged for.
pub(super) fn incomplete_stop(
    payload: &ResponsesResponse,
) -> Result<bool, super::super::OpenAiError> {
    let reason = payload
        .incomplete_details
        .as_ref()
        .and_then(|details| details.reason.as_deref());
    if payload.status.as_deref() != Some("incomplete") && reason.is_none() {
        return Ok(false);
    }
    match reason {
        Some("content_filter") => Err(super::super::OpenAiError::ContentFiltered {
            detail: "provider content filter stopped the response \
                     (incomplete: content_filter)"
                .into(),
        }),
        Some("max_output_tokens") | None => Ok(true),
        Some(other) => {
            warn!(reason = other, "responses turn stopped incomplete");
            Ok(true)
        }
    }
}

/// Build the opaque `ResponsesItems` artifact from the collected reasoning
/// output items, or `None` when nothing was captured. The payload is the
/// JSON serialization of the items exactly as received (type tag, id,
/// summary and — in stateless mode — `encrypted_content`).
///
/// Bounded by the crate's one reasoning byte budget, for the same reason the
/// chat adapter's capture is: a reasoning item is persisted with the turn and
/// replayed on every later request, so one heavy turn must not be able to
/// store an unbounded blob. An over-cap payload is dropped (the turn keeps its
/// display text and still counts as output) rather than failing the request.
pub(super) fn responses_items_artifact(
    items: &[serde_json::Value],
) -> Result<Option<ReasoningArtifact>, super::super::OpenAiError> {
    if items.is_empty() {
        return Ok(None);
    }
    let bytes = serde_json::to_vec(items)
        .map_err(|e| super::super::OpenAiError::Io(io::Error::other(e)))?;
    if bytes.len() > MAX_REASONING_BYTES {
        warn!(
            payload_bytes = bytes.len(),
            limit = MAX_REASONING_BYTES,
            "reasoning items exceed the per-turn cap; dropping the replay payload \
             (the turn keeps its display text and still counts as output)"
        );
        return Ok(None);
    }
    debug!(
        item_count = items.len(),
        payload_bytes = bytes.len(),
        "captured responses reasoning items artifact",
    );
    Ok(Some(ReasoningArtifact::ResponsesItems(bytes)))
}

/// Merge a streamed reasoning item into the collected opaque items.
///
/// Providers emit both `response.output_item.added` (item opens, summary
/// still empty) and `response.output_item.done` (item complete, with
/// `encrypted_content` in stateless mode) for the same item — dedupe by
/// `id`, keeping the later (complete) value. Items without an id are kept
/// as-is.
pub(super) fn merge_reasoning_item(items: &mut Vec<serde_json::Value>, item: serde_json::Value) {
    let id = item.get("id").and_then(|v| v.as_str()).map(String::from);
    let Some(id) = id else {
        // Items without an id can't be deduped — keep as-is.
        items.push(item);
        return;
    };
    // Same-id item already collected? Replace it with the later, complete
    // value; otherwise append. `pos` came from `position()` over `items`,
    // so the `get_mut` lookup always succeeds; the None arm is unreachable.
    match items
        .iter()
        .position(|it| it.get("id").and_then(|v| v.as_str()) == Some(id.as_str()))
    {
        Some(pos) => {
            if let Some(slot) = items.get_mut(pos) {
                *slot = item;
            }
        }
        None => items.push(item),
    }
}

/// Extract the display text from a Responses API reasoning-summary array.
///
/// The API nests a reasoning item's visible summary here, and the entries are
/// not uniform: an entry is either a bare string or an object carrying its text
/// under `text`. Both shapes are read and joined with a space, so a summary
/// split across several entries reads as one paragraph. Nothing readable yields
/// `None` rather than an empty string — an encrypted-only item has no summary
/// text at all, and `None` is what keeps a textless turn from inventing a blank
/// display string.
pub(super) fn extract_reasoning_text(summary: &[serde_json::Value]) -> Option<String> {
    let parts: Vec<String> = summary
        .iter()
        .filter_map(|v| {
            v.as_str()
                .map(String::from)
                .or_else(|| v.get("text").and_then(|t| t.as_str().map(String::from)))
        })
        .collect();
    if parts.is_empty() {
        None
    } else {
        Some(parts.join(" "))
    }
}

/// Convert a parsed non-streaming Responses API response into a turn
/// result, collecting the `reasoning` output items (and `encrypted_content`
/// where present) into the round-trip artifact. `response_id` and the
/// display reasoning text keep their existing behavior — the artifact is
/// purely additive.
pub(crate) fn responses_response_to_turn(
    payload: ResponsesResponse,
) -> Result<ChatTurnResult, super::super::OpenAiError> {
    let response_id = payload.id.clone();
    // A declared stop wins over whatever (possibly empty) body arrived: a
    // length-capped answer keeps its partial text as `truncated`, while a
    // content-filter refusal is a distinct terminal error. Classified before
    // the usage field is moved out below.
    let truncated = incomplete_stop(&payload)?;
    let turn_usage = payload.usage.as_ref().map(responses_usage_to_token_usage);

    // Parse output items: extract text, reasoning, tool calls, and the
    // opaque reasoning items for the round-trip artifact.
    let mut full_text = String::new();
    let mut full_reasoning = String::new();
    let mut tool_calls: Vec<ChatToolCall> = Vec::new();
    let mut reasoning_items: Vec<serde_json::Value> = Vec::new();
    // Kinds this client does not model are skipped, but counted so the omission
    // stays diagnosable — a count, never the item payload.
    let mut unknown_item_count = 0usize;

    for raw in payload.output {
        // Parse the raw item into the typed form for display/tool-call
        // extraction. The raw value itself is kept (never re-serialized) for
        // the reasoning artifact below, so unknown fields — e.g. a newer
        // `content` shape alongside `summary` — and explicit `null`s survive
        // byte-for-byte, matching the streaming path which stores reasoning
        // items as raw `serde_json::Value` directly.
        let item: ResponseOutputItem = serde_json::from_value(raw.clone())
            .map_err(|e| super::super::OpenAiError::Io(io::Error::other(e)))?;
        match item {
            ResponseOutputItem::Message { content, .. } => {
                for part in content {
                    if part.kind == "output_text"
                        && let Some(t) = &part.text
                    {
                        full_text.push_str(t);
                    }
                }
            }
            ResponseOutputItem::Reasoning { summary, .. } => {
                if let Some(text) = extract_reasoning_text(&summary) {
                    full_reasoning.push_str(&text);
                }
                // Capture the RAW item exactly as the API returned it — every
                // field, value, and ordering survives (unknown fields such as
                // `content` are never dropped). This keeps the non-streaming
                // artifact byte-identical to the streaming path's raw-value
                // capture for the same logical item.
                reasoning_items.push(raw);
            }
            ResponseOutputItem::FunctionCall {
                call_id,
                name,
                arguments,
                caller,
            } => {
                tool_calls.push(ChatToolCall {
                    id: call_id,
                    name,
                    arguments_json: arguments,
                    caller,
                });
            }
            // Program items contain JS code generated by the model for
            // orchestrating tool calls in OpenAI's hosted V8 runtime.
            // We log it but don't execute it — the program makes
            // function_call items (with `caller` pointing back to this program)
            // that we handle normally via the tool loop.
            ResponseOutputItem::Program { call_id, code, .. } => {
                if let Some(ref code_text) = code {
                    tracing::debug!(
                        program_call_id = %call_id,
                        code_len = code_text.len(),
                        "programmatic tool calling program",
                    );
                }
            }
            // Program output signals a program finished executing in the
            // hosted runtime.
            ResponseOutputItem::ProgramOutput {
                call_id,
                result,
                status,
                ..
            } => {
                tracing::debug!(
                    program_call_id = %call_id,
                    has_result = result.is_some(),
                    status = status.as_deref().unwrap_or("unknown"),
                    "programmatic tool calling program output",
                );
            }
            // An output-item kind this client does not model. It is data the
            // provider chose to send, not a malformed answer, so the surrounding
            // items still produce the turn — the streaming path ignores unknown
            // events for the same reason.
            ResponseOutputItem::Unknown => unknown_item_count += 1,
        }
    }

    if unknown_item_count > 0 {
        debug!(
            unknown_item_count,
            "skipped responses output items of an unmodelled kind",
        );
    }

    let reasoning_artifact = responses_items_artifact(&reasoning_items)?;

    // No answer text, but a reasoning item was captured: the provider produced
    // (and billed) output this turn, so the shared assembler turns it into a
    // `FinalText` with empty content that carries the reasoning and its
    // round-trip artifact — never an empty response. That is what
    // `produced_reasoning` reports; an item the byte cap refused still counts,
    // so it is read from the collected items, not from the surviving artifact.
    let produced_reasoning = !reasoning_items.is_empty() || !full_reasoning.is_empty();
    assemble_turn(TurnParts {
        // The assembler normalizes: an empty answer is handed over as `None`
        // (there is no present-but-empty content on this path), and the text
        // itself moves rather than being copied.
        content: (!full_text.is_empty()).then_some(full_text),
        tool_calls,
        // A non-streaming item names its own function or fails to deserialize,
        // so nothing can be left unnamed here.
        unnamed_calls: 0,
        reasoning: (!full_reasoning.is_empty()).then_some(full_reasoning),
        reasoning_artifact,
        produced_reasoning,
        usage: turn_usage,
        truncated,
        response_id,
    })
}
