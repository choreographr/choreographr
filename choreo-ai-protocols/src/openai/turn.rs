//! Turn-result assembly shared by both `OpenAI` adapters.
//!
//! A provider reply becomes a [`ChatTurnResult`] through one decision tree
//! regardless of wire format: tool calls win, then a reply whose every tool call
//! was truncated, then a genuine final answer — and a final answer that carries
//! nothing at all is only an empty response when the model also produced no
//! reasoning. Each adapter's own `turn` module owns how it reads *its* wire
//! format (finish reasons, `incomplete` details, output-item variants); this
//! one owns the shared verdict, so the chat-completions and Responses paths
//! cannot drift apart on it.

use super::validate_tool_call_arguments;
use crate::types::{ChatAssistantToolUse, ChatToolCall, ChatTurnResult, FinalTextResult};
use choreo_proto::{ReasoningArtifact, TokenUsage};

/// The finalized parts of one turn, handed to [`assemble_turn`].
///
/// Bundled so the assembler takes one value instead of a long positional list
/// (no `too_many_arguments` suppression) and each caller names every part
/// explicitly.
pub(super) struct TurnParts {
    /// Content to attach to a `ToolUse` result (the provider's raw option for
    /// a non-streaming reply, `None`-when-empty for a streamed one).
    pub(super) tool_use_content: Option<String>,
    /// Content to attach to a `FinalText` result (already normalized by the
    /// caller: trimmed for a non-streaming reply, the accumulated text for a
    /// streamed one).
    pub(super) final_text: String,
    /// Whether the turn's visible content is empty (drives the empty-response
    /// and truncated-tool-call guards). Separate from `final_text` because the
    /// two adapters normalize it differently.
    pub(super) content_is_empty: bool,
    /// The turn's tool calls, collected but not yet validated.
    pub(super) tool_calls: Vec<ChatToolCall>,
    /// The turn's display reasoning text.
    pub(super) reasoning: Option<String>,
    /// The opaque round-trip reasoning artifact.
    pub(super) reasoning_artifact: Option<ReasoningArtifact>,
    /// Whether the provider produced reasoning output this turn — independent of
    /// whether any of it could be displayed or replayed, so a turn whose payload
    /// was over the replay cap is still model output.
    pub(super) produced_reasoning: bool,
    /// Token usage reported for the turn, when the provider sent any.
    pub(super) usage: Option<TokenUsage>,
    /// Whether the reply was cut off (chat's `finish_reason: length`, Responses'
    /// `incomplete`) on a final-text turn.
    pub(super) truncated: bool,
    /// The provider's response id, when the wire format reports one (Responses
    /// chains the next turn onto it via `previous_response_id`).
    pub(super) response_id: Option<String>,
}

/// Assemble a turn result from its finalized parts.
///
/// The order is the contract: a usable tool call always wins, a reply whose
/// every tool call was truncated surfaces the answer when there is one and a
/// dedicated `TruncatedToolCall` when there is not, and a final answer with no
/// visible text is only empty when the provider produced no reasoning either.
pub(super) fn assemble_turn(parts: TurnParts) -> Result<ChatTurnResult, super::OpenAiError> {
    let TurnParts {
        tool_use_content,
        final_text,
        content_is_empty,
        mut tool_calls,
        reasoning,
        reasoning_artifact,
        produced_reasoning,
        usage,
        truncated,
        response_id,
    } = parts;
    // Truncated / malformed tool calls are dropped here, once, for every path.
    let discarded = validate_tool_call_arguments(&mut tool_calls);
    if !tool_calls.is_empty() {
        return Ok(ChatTurnResult::ToolUse(ChatAssistantToolUse {
            content: tool_use_content,
            tool_calls,
            reasoning,
            usage,
            response_id,
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
            return Err(super::OpenAiError::TruncatedToolCall { discarded });
        }
        return Ok(ChatTurnResult::FinalText(FinalTextResult {
            content: final_text,
            truncated,
            reasoning,
            usage,
            response_id,
            reasoning_artifact,
        }));
    }
    // An encrypted-only, unrecognised-only, or over-the-replay-cap reasoning
    // turn has no visible text but IS model output. The question here is what the
    // provider produced, not what this client chose to keep, so it is answered by
    // the adapter's own output fact rather than by whether an artifact survived.
    if content_is_empty && !produced_reasoning {
        return Err(super::OpenAiError::EmptyResponse);
    }
    Ok(ChatTurnResult::FinalText(FinalTextResult {
        content: final_text,
        truncated,
        reasoning,
        usage,
        response_id,
        reasoning_artifact,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_turn_with_neither_text_tools_nor_reasoning_is_empty() {
        let err = assemble_turn(TurnParts {
            tool_use_content: None,
            final_text: String::new(),
            content_is_empty: true,
            tool_calls: Vec::new(),
            reasoning: None,
            reasoning_artifact: None,
            produced_reasoning: false,
            usage: None,
            truncated: false,
            response_id: None,
        })
        .expect_err("an all-empty turn is an empty response");
        assert!(matches!(err, super::super::OpenAiError::EmptyResponse));
    }

    #[test]
    fn produced_reasoning_alone_keeps_a_textless_turn_non_empty() {
        // Encrypted-only reasoning, or a payload the replay cap refused: the
        // provider billed output, so the turn is a `FinalText` with no content
        // rather than an empty response.
        let turn = assemble_turn(TurnParts {
            tool_use_content: None,
            final_text: String::new(),
            content_is_empty: true,
            tool_calls: Vec::new(),
            reasoning: None,
            reasoning_artifact: None,
            produced_reasoning: true,
            usage: None,
            truncated: false,
            response_id: None,
        })
        .expect("produced reasoning is model output");
        assert!(matches!(turn, ChatTurnResult::FinalText(_)));
    }
}
