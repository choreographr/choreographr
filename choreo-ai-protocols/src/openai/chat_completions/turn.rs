//! Assembling a chat-completions reply into a turn result.
//!
//! Both the non-streaming response conversion and the streaming accumulator
//! funnel into [`assemble_turn`], so the `ToolUse` / truncated-tool-call /
//! final-text decision tree — and the terminal finish-reason errors that must
//! surface before it — exist exactly once for both paths.

use tracing::debug;

use super::super::validate_tool_call_arguments;
use super::wire::{ChatCompletionsResponse, FinishReason};
use crate::types::{ChatAssistantToolUse, ChatToolCall, ChatTurnResult, FinalTextResult};
use choreo_proto::{ReasoningArtifact, TokenUsage};

/// Convert a parsed non-streaming chat completions response into a turn
/// result. The raw reasoning value is captured into the round-trip artifact
/// inside `AssistantMessage::take_reasoning`, before the field is consumed.
/// Terminal finish reasons must surface as distinct errors, not turn
/// results: a content-filter refusal (z.ai `sensitive` / `OpenAI`
/// `content_filter`) is never retryable, and a context-window overflow is a
/// compaction-bug signal. `Length` deliberately maps to NO error here — it is
/// a truncation notice carried on the `FinalText` result instead (and is normal
/// flow when tool calls are present).
pub(crate) fn finish_reason_terminal_error(
    reason: Option<&FinishReason>,
) -> Option<super::super::OpenAiError> {
    let reason = reason?;
    if reason.is_content_filtered() {
        // Follows the images adapter's ContentFiltered precedent: the HTTP
        // response itself succeeded, so no status rides along, and the
        // retry layer must never treat this as retryable.
        return Some(super::super::OpenAiError::ContentFiltered {
            detail: "provider content filter refused the response \
                     (finish_reason: sensitive/content_filter)"
                .into(),
        });
    }
    if reason.is_context_window_exceeded() {
        return Some(super::super::OpenAiError::ContextWindowExceeded {
            detail: "the prompt exceeded the model's context window \
                     (finish_reason: model_context_window_exceeded)"
                .into(),
        });
    }
    None
}

pub(crate) fn chat_completions_response_to_turn(
    payload: ChatCompletionsResponse,
) -> Result<ChatTurnResult, super::super::OpenAiError> {
    let Some(mut choice) = payload.choices.into_iter().next() else {
        return Err(super::super::OpenAiError::EmptyResponse);
    };

    // A declared terminal finish reason wins over whatever (possibly empty)
    // body the provider sent — z.ai's `sensitive` often arrives with an empty
    // content, which would otherwise look like a (retryable) empty response.
    if let Some(err) = finish_reason_terminal_error(choice.finish_reason.as_ref()) {
        return Err(err);
    }
    // `length` on a FINAL-TEXT turn means the answer was cut off; the tool-call
    // arm ignores it (a length-truncated tool call is normal tool-loop flow).
    let truncated = matches!(choice.finish_reason, Some(FinishReason::Length));

    // Extract reasoning early (before the partial moves into tool_calls / content)
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
            cache_write_tokens: 0,
        }
    });

    let tool_calls: Vec<ChatToolCall> = choice
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

    // A tool-call message keeps the provider's raw `content` Option; the
    // final-text content is trimmed (a blank answer reads as none), and the
    // empty-response guard keys on that trimmed text.
    let tool_use_content = choice.message.content;
    let final_text = tool_use_content
        .as_deref()
        .unwrap_or_default()
        .trim()
        .to_string();
    let content_is_empty = final_text.is_empty();

    assemble_turn(ChatTurnParts {
        tool_use_content,
        final_text,
        content_is_empty,
        tool_calls,
        reasoning,
        reasoning_artifact,
        usage: turn_usage,
        truncated,
    })
}

/// The finalized parts of one chat turn, handed to [`assemble_turn`].
///
/// Bundled so the assembler takes one value instead of a long positional list
/// (no `too_many_arguments` suppression) and each caller names every part
/// explicitly.
pub(super) struct ChatTurnParts {
    /// Content to attach to a `ToolUse` result (the provider's raw option for
    /// the non-streaming path, `None`-when-empty for the streaming path).
    pub(super) tool_use_content: Option<String>,
    /// Content to attach to a `FinalText` result (already normalized by the
    /// caller: trimmed for non-streaming, the accumulated text for streaming).
    pub(super) final_text: String,
    /// Whether the turn's visible content is empty (drives the empty-response
    /// and truncated-tool-call guards).
    pub(super) content_is_empty: bool,
    /// The turn's tool calls, collected but not yet validated.
    pub(super) tool_calls: Vec<ChatToolCall>,
    /// The turn's display reasoning text.
    pub(super) reasoning: Option<String>,
    /// The opaque round-trip reasoning artifact.
    pub(super) reasoning_artifact: Option<ReasoningArtifact>,
    /// Token usage reported for the turn, when the provider sent any.
    pub(super) usage: Option<TokenUsage>,
    /// Whether `finish_reason` was `length` on a final-text turn.
    pub(super) truncated: bool,
}

/// Assemble a chat turn result from its finalized parts.
///
/// Shared by the non-streaming response conversion and the streaming
/// accumulator so the `ToolUse` / truncated-tool-call / final-text decision tree
/// exists exactly once. A message with empty content, no tool calls, and no
/// reasoning artifact is a genuine empty response.
pub(super) fn assemble_turn(
    parts: ChatTurnParts,
) -> Result<ChatTurnResult, super::super::OpenAiError> {
    let ChatTurnParts {
        tool_use_content,
        final_text,
        content_is_empty,
        mut tool_calls,
        reasoning,
        reasoning_artifact,
        usage,
        truncated,
    } = parts;
    // Truncated / malformed tool calls are dropped here, once, for both paths.
    let discarded = validate_tool_call_arguments(&mut tool_calls);
    if !tool_calls.is_empty() {
        return Ok(ChatTurnResult::ToolUse(ChatAssistantToolUse {
            content: tool_use_content,
            tool_calls,
            reasoning,
            usage,
            response_id: None,
            reasoning_artifact,
        }));
    }
    if !discarded.is_empty() {
        // Every tool call had invalid (truncated) arguments: the provider cut
        // the response off mid-call, so the call is unsafe to execute. If the
        // model also produced visible text, surface it — the casualty is the
        // tool call, not the answer. Otherwise there is nothing usable to
        // return, so signal a dedicated `TruncatedToolCall` instead of
        // fabricating an empty "final answer" that looks complete — the daemon
        // special-cases it to tell the model what happened and let it retry in
        // smaller steps.
        if content_is_empty {
            return Err(super::super::OpenAiError::TruncatedToolCall { discarded });
        }
        return Ok(ChatTurnResult::FinalText(FinalTextResult {
            content: final_text,
            truncated,
            reasoning,
            usage,
            response_id: None,
            reasoning_artifact,
        }));
    }
    // An encrypted-only (or unrecognised-only) reasoning turn has no visible
    // text but IS model output; a missing artifact is what makes this a genuine
    // empty response.
    if content_is_empty && reasoning_artifact.is_none() {
        return Err(super::super::OpenAiError::EmptyResponse);
    }
    Ok(ChatTurnResult::FinalText(FinalTextResult {
        content: final_text,
        truncated,
        reasoning,
        usage,
        response_id: None,
        reasoning_artifact,
    }))
}
