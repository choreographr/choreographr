//! Turning a Responses API reply into a turn result.
//!
//! Owns the round-trip artifact for this adapter — the provider's `reasoning`
//! output items (with `encrypted_content` where present), serialized exactly as
//! received so they can be replayed into `input` — and the response →
//! [`ChatTurnResult`] conversion that applies the tool-call and empty-response
//! guards.

use std::io;

use tracing::debug;

use super::super::validate_tool_call_arguments;
use super::body::extract_reasoning_text;
use super::wire::{ResponseOutputItem, ResponsesResponse};
use crate::types::{ChatAssistantToolUse, ChatToolCall, ChatTurnResult, FinalTextResult};
use choreo_proto::{ReasoningArtifact, TokenUsage};

/// Build the opaque `ResponsesItems` artifact from the collected reasoning
/// output items, or `None` when nothing was captured. The payload is the
/// JSON serialization of the items exactly as received (type tag, id,
/// summary and — in stateless mode — `encrypted_content`).
pub(super) fn responses_items_artifact(
    items: &[serde_json::Value],
) -> Result<Option<ReasoningArtifact>, super::super::OpenAiError> {
    if items.is_empty() {
        return Ok(None);
    }
    let bytes = serde_json::to_vec(items)
        .map_err(|e| super::super::OpenAiError::Io(io::Error::other(e)))?;
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

/// Convert a parsed non-streaming Responses API response into a turn
/// result, collecting the `reasoning` output items (and `encrypted_content`
/// where present) into the round-trip artifact. `response_id` and the
/// display reasoning text keep their existing behavior — the artifact is
/// purely additive.
pub(crate) fn responses_response_to_turn(
    payload: ResponsesResponse,
) -> Result<ChatTurnResult, super::super::OpenAiError> {
    let response_id = payload.id.clone();
    let turn_usage = payload.usage.map(|u| TokenUsage {
        input_tokens: u.prompt_tokens,
        output_tokens: u.completion_tokens,
        total_tokens: u.total_tokens,
        // The Responses API has no `prompt_tokens_details` object; its
        // cached-token analogue (`input_tokens_details.cached_tokens`) is a
        // possible follow-up, so chat-completions z.ai parsing only for now.
        cached_tokens: 0,
        cache_write_tokens: 0,
    });

    // Parse output items: extract text, reasoning, tool calls, and the
    // opaque reasoning items for the round-trip artifact.
    let mut full_text = String::new();
    let mut full_reasoning = String::new();
    let mut tool_calls: Vec<ChatToolCall> = Vec::new();
    let mut reasoning_items: Vec<serde_json::Value> = Vec::new();

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
        }
    }

    let reasoning_artifact = responses_items_artifact(&reasoning_items)?;

    let discarded = validate_tool_call_arguments(&mut tool_calls);
    if !tool_calls.is_empty() {
        return Ok(ChatTurnResult::ToolUse(ChatAssistantToolUse {
            content: if full_text.is_empty() {
                None
            } else {
                Some(full_text)
            },
            tool_calls,
            reasoning: if full_reasoning.is_empty() {
                None
            } else {
                Some(full_reasoning)
            },
            usage: turn_usage,
            response_id,
            reasoning_artifact,
        }));
    }

    // All calls had invalid arguments — surface the error unless the
    // model also produced text, in which case return the text.
    if !discarded.is_empty() {
        if !full_text.is_empty() {
            return Ok(ChatTurnResult::FinalText(FinalTextResult {
                content: full_text,
                // Responses API has no finish_reason-equivalent length flag
                // parsed today; only the chat-completions adapter sets it.
                truncated: false,
                reasoning: if full_reasoning.is_empty() {
                    None
                } else {
                    Some(full_reasoning)
                },
                usage: turn_usage,
                response_id,
                reasoning_artifact,
            }));
        }
        return Err(super::super::OpenAiError::TruncatedToolCall { discarded });
    }

    if full_text.is_empty() {
        return Err(super::super::OpenAiError::EmptyResponse);
    }

    Ok(ChatTurnResult::FinalText(FinalTextResult {
        content: full_text,
        truncated: false,
        reasoning: if full_reasoning.is_empty() {
            None
        } else {
            Some(full_reasoning)
        },
        usage: turn_usage,
        response_id,
        reasoning_artifact,
    }))
}
