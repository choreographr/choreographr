//! Turning a chat-completions reply into a turn result.
//!
//! Both the non-streaming response conversion and the streaming accumulator
//! funnel into the crate's shared [`assemble_turn`], so the `ToolUse` /
//! truncated-tool-call / final-text decision tree — and the terminal
//! finish-reason errors that must surface before it — exist exactly once for
//! every `OpenAI` path.

use tracing::debug;

use super::super::turn::{TurnParts, assemble_turn};
use super::wire::{ChatCompletionsResponse, FinishReason};
use crate::types::{ChatToolCall, ChatTurnResult};
use choreo_proto::TokenUsage;

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
    let reasoning = choice.message.take_reasoning();

    // Extract token usage from the API response for cost tracking / display.
    // The cache-hit count comes from the shared `Usage` projection, so this
    // path and the streaming accumulator cannot disagree about which cache
    // shape wins (see `openai::wire::Usage::to_token_usage`).
    let turn_usage: Option<TokenUsage> = payload.usage.map(|u| {
        let usage = u.to_token_usage();
        if usage.cached_tokens > 0 {
            debug!(
                cached_tokens = usage.cached_tokens,
                prompt_tokens = usage.input_tokens,
                "chat-completions cached prompt tokens"
            );
        }
        usage
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

    // The reply's own `content` option rides into the assembler untouched: a
    // tool-call turn echoes it raw (`Some("")` stays present-and-empty), and a
    // final answer is trimmed there — this path never normalizes text itself.
    assemble_turn(TurnParts {
        content: choice.message.content,
        tool_calls,
        reasoning: reasoning.display,
        reasoning_artifact: reasoning.artifact,
        produced_reasoning: reasoning.produced,
        usage: turn_usage,
        truncated,
        response_id: None,
    })
}
