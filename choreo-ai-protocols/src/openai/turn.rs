//! Turn-result assembly shared by both `OpenAI` adapters.
//!
//! A provider reply becomes a [`ChatTurnResult`] through one decision tree
//! regardless of wire format: tool calls win, then a reply whose every tool call
//! was truncated, then a genuine final answer — and a final answer that carries
//! nothing at all is only an empty response when the model also produced no
//! reasoning. Each adapter's own `turn` module owns how it reads *its* wire
//! format (finish reasons, `incomplete` details, output-item variants); this one
//! owns the shared verdict — and, with it, the single place a turn's content is
//! normalized — so the chat-completions and Responses paths cannot drift apart.

use super::validate_tool_call_arguments;
use crate::types::{ChatAssistantToolUse, ChatToolCall, ChatTurnResult, FinalTextResult};
use choreo_proto::{ReasoningArtifact, TokenUsage};

/// The finalized parts of one turn, handed to [`assemble_turn`].
///
/// Bundled so the assembler takes one value instead of a long positional list
/// (no `too_many_arguments` suppression) and each caller names every part
/// explicitly.
pub(super) struct TurnParts {
    /// The provider's raw content for this turn, exactly as it arrived: the
    /// non-streaming chat reply's own `content` option, or the text a stream
    /// accumulated. Deliberately unnormalized — the assembler owns trimming and
    /// the empty-content verdict, so an adapter that stores content as it
    /// arrived cannot disagree with one that buffers a stream.
    pub(super) content: Option<String>,
    /// The turn's tool calls, collected but not yet validated.
    pub(super) tool_calls: Vec<ChatToolCall>,
    /// How many calls the stream mentioned without ever naming the function, so
    /// they could not be turned into a [`ChatToolCall`] at all. Such a call is
    /// unrunnable data the provider still acted on, which is why a turn left
    /// with nothing else fails instead of reading as an empty answer (see the
    /// verdict below).
    pub(super) unnamed_calls: usize,
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
        content,
        mut tool_calls,
        unnamed_calls,
        reasoning,
        reasoning_artifact,
        produced_reasoning,
        usage,
        truncated,
        response_id,
    } = parts;
    // The one normalization of a turn's content: the final answer is the
    // provider's raw content trimmed, and "this turn carries no visible text" is
    // read off that same trimmed text. Doing it here — rather than per adapter —
    // is what makes a streamed answer read exactly like a non-streaming one, and
    // what makes a whitespace-only reply count as empty on every path.
    let final_text = content.as_deref().unwrap_or_default().trim().to_string();
    let content_is_empty = final_text.is_empty();
    // Truncated / malformed tool calls are dropped here, once, for every path.
    let discarded = validate_tool_call_arguments(&mut tool_calls);
    if !tool_calls.is_empty() {
        return Ok(ChatTurnResult::ToolUse(ChatAssistantToolUse {
            // The tool-use arm keeps the RAW content: a tool-call turn's text is
            // echoed back as the provider sent it. `Some("")` therefore stays
            // `Some("")` — content that was present and empty is not the same
            // fact as content that was absent, and only the raw option knows
            // which one the provider reported.
            content,
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
    // A provider can also mention a call it never names (the Responses adapter
    // reads the function name off the call's output item, so a stream that omits
    // it has nothing to run). Such a call cannot be executed and cannot be shown,
    // so a turn left with no text and no calls fails rather than being reported
    // as an empty answer that hides what the model did — the same reasoning that
    // refuses to report a truncated call as no output above.
    if content_is_empty && unnamed_calls > 0 {
        return Err(super::OpenAiError::UnidentifiedToolCalls {
            count: unnamed_calls,
        });
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

    /// The inert parts every case below leaves alone: no tool calls, no
    /// reasoning, no usage, no chain id.
    fn textless_parts(content: Option<String>) -> TurnParts {
        TurnParts {
            content,
            tool_calls: Vec::new(),
            unnamed_calls: 0,
            reasoning: None,
            reasoning_artifact: None,
            produced_reasoning: false,
            usage: None,
            truncated: false,
            response_id: None,
        }
    }

    #[test]
    fn a_turn_with_neither_text_tools_nor_reasoning_is_empty() {
        let err = assemble_turn(textless_parts(None))
            .expect_err("an all-empty turn is an empty response");
        assert!(matches!(err, super::super::OpenAiError::EmptyResponse));
    }

    #[test]
    fn produced_reasoning_alone_keeps_a_textless_turn_non_empty() {
        // Encrypted-only reasoning, or a payload the replay cap refused: the
        // provider billed output, so the turn is a `FinalText` with no content
        // rather than an empty response.
        let turn = assemble_turn(TurnParts {
            produced_reasoning: true,
            ..textless_parts(None)
        })
        .expect("produced reasoning is model output");
        assert!(matches!(turn, ChatTurnResult::FinalText(_)));
    }

    #[test]
    fn a_turn_whose_calls_could_not_be_named_fails_instead_of_reading_empty() {
        // A provider that mentions calls without ever naming the function has
        // acted, but nothing it produced can be run or shown. Reporting that as
        // an empty answer hides the call, so the turn fails — and it fails even
        // when reasoning came with it, because the reasoning is not the answer
        // the user is waiting for.
        let err = assemble_turn(TurnParts {
            unnamed_calls: 2,
            ..textless_parts(None)
        })
        .expect_err("an unnameable call is not an answer");
        assert!(matches!(
            err,
            super::super::OpenAiError::UnidentifiedToolCalls { count: 2 }
        ));

        let err = assemble_turn(TurnParts {
            unnamed_calls: 1,
            produced_reasoning: true,
            ..textless_parts(None)
        })
        .expect_err("reasoning does not stand in for a call that cannot run");
        assert!(matches!(
            err,
            super::super::OpenAiError::UnidentifiedToolCalls { count: 1 }
        ));
    }

    #[test]
    fn a_named_call_outranks_the_calls_that_could_not_be_named() {
        // One runnable call is a tool-use turn even if the stream also mentioned
        // calls it could not identify: the loop gets the work it can do, and the
        // unusable ones were logged by the adapter's accumulator.
        let turn = assemble_turn(TurnParts {
            tool_calls: vec![ChatToolCall {
                id: "call_1".to_string(),
                name: "get_weather".to_string(),
                arguments_json: "{}".to_string(),
                caller: None,
            }],
            unnamed_calls: 1,
            ..textless_parts(None)
        })
        .expect("a usable tool call wins");
        assert!(matches!(turn, ChatTurnResult::ToolUse(_)));
    }

    #[test]
    fn a_padded_answer_is_trimmed_for_the_final_text() {
        // Normalization happens here and only here, so an adapter never has to
        // remember it: the answer the caller renders carries no surrounding
        // whitespace, whichever path produced it.
        let turn = assemble_turn(textless_parts(Some("  the answer \n".to_string())))
            .expect("a padded answer is still an answer");
        match turn {
            ChatTurnResult::FinalText(result) => assert_eq!(result.content, "the answer"),
            other => panic!("expected FinalText, got {other:?}"),
        }
    }

    #[test]
    fn a_whitespace_only_answer_is_empty() {
        // Content that is present but blank is not visible text: the turn is an
        // empty response (or, with reasoning, a textless final answer) rather
        // than a `FinalText` whose content reads as blank.
        let err = assemble_turn(textless_parts(Some("   \n\t".to_string())))
            .expect_err("a blank answer is not visible text");
        assert!(matches!(err, super::super::OpenAiError::EmptyResponse));
        let turn = assemble_turn(TurnParts {
            produced_reasoning: true,
            ..textless_parts(Some("   \n\t".to_string()))
        })
        .expect("reasoning is still model output");
        match turn {
            ChatTurnResult::FinalText(result) => assert_eq!(result.content, ""),
            other => panic!("expected FinalText, got {other:?}"),
        }
    }

    #[test]
    fn a_tool_use_turn_keeps_the_raw_content_option() {
        // The tool-use arm echoes the provider's raw content, so content that
        // arrived present-and-empty stays `Some("")` — only the final-text
        // normalization above trims and empties.
        let turn = assemble_turn(TurnParts {
            tool_calls: vec![ChatToolCall {
                id: "call_1".to_string(),
                name: "get_weather".to_string(),
                arguments_json: "{}".to_string(),
                caller: None,
            }],
            ..textless_parts(Some(String::new()))
        })
        .expect("a usable tool call wins");
        match turn {
            ChatTurnResult::ToolUse(result) => assert_eq!(result.content, Some(String::new())),
            other => panic!("expected ToolUse, got {other:?}"),
        }
    }
}
