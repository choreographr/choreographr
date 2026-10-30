//! Request-body construction for the Responses API.
//!
//! The Responses wire format takes a flat `input` array of typed items rather
//! than a `messages` list, so a canonical message slice has to be lowered item
//! by item; [`super::super::messages_to_responses_input`] does that, and this
//! module assembles the surrounding body (tool results, chained tail, reasoning
//! payload, tool definitions) for both the simple and the tool-carrying paths.

use std::collections::HashMap;
use std::io;

use super::super::{ChatRequestMessage, SystemPrompt, endpoint_url, messages_to_responses_input};
use super::input::CallerIndex;
use super::wire::{ResponsesReasoning, ResponsesRequest, ResponsesTool};

// ── Shared helpers ───────────────────────────────────────────────────────

/// Index every tool call in `messages` by its `call_id` → the attribution the
/// assistant message that requested it carried.
///
/// Built from the FULL history, and always before any chained-tail selection:
/// a chained tool round sends only the tool outputs, so the assistant message
/// that carried the call — and with it the `caller` the output has to repeat —
/// is exactly the message the tail drops. Deriving the index here, off a
/// borrow of the caller's slice, keeps the lookup allocation-light: one small
/// map of borrowed ids to borrowed attributions, never a copy of the
/// conversation.
fn caller_index(messages: &[ChatRequestMessage]) -> CallerIndex<'_> {
    messages
        .iter()
        .filter_map(|msg| msg.tool_calls.as_ref())
        .flatten()
        .filter_map(|call| {
            call.caller
                .as_ref()
                .map(|caller| (call.id.as_str(), caller))
        })
        .collect::<HashMap<_, _>>()
}

/// Build the `input` JSON value for a Responses API request: the full message
/// history when there is no chain, or only the post-assistant tail when
/// `previous_response_id` chains onto a stored response.
pub(super) fn build_responses_input(
    messages: &[ChatRequestMessage],
    previous_response_id: Option<&str>,
    system: SystemPrompt,
) -> Result<Option<serde_json::Value>, super::super::OpenAiError> {
    // A chained turn sends only the messages that postdate the last assistant
    // response — the server already holds the rest via `previous_response_id`,
    // and resending it would duplicate every prior turn (billing + context
    // inflation). A fresh/stateless turn sends the full history. The tool
    // outputs of a finished tool round are already in `messages` (the session
    // turn stores them in the model's call order), so the chained tail is
    // derived here rather than passed in as a separate buffer. A tail made up
    // only of tool outputs stays output-only — no system item rides it — so it
    // continues the stored response's call/output pairing unbroken (the rule
    // and its reason live on [`chain_input_messages`]). Both branches borrow, and
    // the caller index is derived BEFORE the tail is selected, so the whole
    // conversation is never copied to build a body.
    let callers = caller_index(messages);
    let items = if previous_response_id.is_some() {
        messages_to_responses_input(chain_input_messages(messages), system, &callers)?
    } else {
        messages_to_responses_input(messages, system, &callers)?
    };
    // System messages go into `input` as `{role: "system"}` items; the stateless
    // ChatGPT/Codex backend hoists them into `instructions` before this runs
    // (see `split_system_prompt`) so no system item reaches it.
    //
    // Nothing to send is an EMPTY ITEMS ARRAY, not an empty string: `input` is an
    // array of typed items here, so `[]` is the honest degenerate shape. A `""`
    // would instead be a request to send one empty-text user message.
    let input_value = serde_json::to_value(&items)
        .map_err(|e| super::super::OpenAiError::Io(io::Error::other(e)))?;
    Ok(Some(input_value))
}

/// The minimal input for a turn chained onto a previous response: the newest
/// system message (the daemon rebuilds the system prompt every request, so the
/// latest one must reach the model even though the chain already carries the
/// old one) plus every message AFTER the last assistant-role message — in the
/// daemon's loop that is the new user message of a fresh turn, or the tool
/// outputs of a tool round. Everything up to and including the last assistant
/// message is already in the chained response's context and must not be resent.
///
/// A tail whose every message is a `tool` result is a **tool round**, and it
/// stays output-only: the chained response ends with the assistant's
/// `function_call`, so the outputs answering that call must follow it directly.
/// A `system` item between them would break a call/output adjacency the API
/// pairs strictly, and would add one more copy of the prompt to the stored
/// transcript on every tool-loop iteration.
///
/// Only the *newest* system message is kept: any older system prompt is
/// already in the chained context, and resending it would duplicate it. In
/// the daemon the system message sits at the head of the list, so it leads
/// the request; in the defensive shapes where a system prompt appears
/// mid-tail, it stays in its original position rather than being hoisted,
/// so the conversation order is preserved.
///
/// Yields borrowed messages: the tail is a *selection*, not a copy, so a chained
/// turn pays nothing for the history the server already holds.
pub(super) fn chain_input_messages(
    messages: &[ChatRequestMessage],
) -> impl Iterator<Item = &ChatRequestMessage> {
    // A chain id implies a prior model call that produced an assistant response,
    // so `last_assistant` is `None` only defensively — then nothing has been
    // chained and every message stays.
    let last_assistant = messages.iter().rposition(|m| m.role == "assistant");
    let newest_system = messages.iter().rposition(|m| m.role == "system");
    // Whether the post-assistant tail is a tool round: non-empty, and every
    // message in it a tool result. An empty tail (the chain's own last message
    // is the assistant turn) and a tail carrying a user/system message are both
    // "not a tool round", so the newest system prompt keeps its keep-arm there.
    let tool_round = last_assistant.is_some_and(|last| {
        let tail = messages.get(last + 1..).unwrap_or_default();
        !tail.is_empty() && tail.iter().all(|m| m.role == "tool")
    });
    messages.iter().enumerate().filter_map(move |(i, m)| {
        // Single pass over every message: drop everything at/before the last
        // assistant message (it is in the chained context) EXCEPT the newest
        // system message, which is rebuilt every turn and must still reach the
        // model — unless this is a tool round, where a system item would split
        // the call from the output that answers it. Older system messages —
        // whether in the chained region or inside the tail — are dropped, so the
        // newest one is never duplicated.
        let keep = match last_assistant {
            None => true,
            Some(last) => {
                (i > last && m.role != "system") || (Some(i) == newest_system && !tool_round)
            }
        };
        keep.then_some(m)
    })
}

/// Split the system prompt out of a canonical message list for a backend that
/// forbids `system`-role input items (the ChatGPT/Codex backend).
///
/// Returns the joined system-prompt string for the top-level `instructions`
/// field; the caller then lowers `messages` with [`SystemPrompt::Hoisted`], which
/// skips the `system` messages instead of emitting them, so the input array
/// carries no `role: "system"` item. A blank system prompt yields `None` rather
/// than an empty `instructions` string.
///
/// Deliberately returns only the prompt and never a filtered message list: the
/// conversation can carry megabytes of attached image bytes, and copying it just
/// to drop a leading message would repeat that copy on every tool-loop iteration.
fn split_system_prompt(messages: &[ChatRequestMessage]) -> Option<String> {
    let prompt: Vec<&str> = messages
        .iter()
        .filter(|msg| msg.role == "system")
        .filter_map(|msg| msg.content.as_deref().filter(|c| !c.trim().is_empty()))
        .collect();
    let joined = prompt.join("\n\n");
    (!joined.is_empty()).then_some(joined)
}

/// Shared helper: extract the reasoning text from a Responses API
/// reasoning-summary array.  Each entry may be a plain string or an object
/// with a `"text"` field.
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

/// Shared helper: build the URL and serialised request body for a
/// Responses API request with tools (and optionally tool results from a
/// previous turn).
pub(super) fn build_responses_request_body(
    config: &super::super::ServiceConfig,
    params: &super::super::TurnParams<'_>,
    stream: bool,
) -> Result<(String, serde_json::Value), super::super::OpenAiError> {
    let url = endpoint_url(&config.base_url, &config.responses_path)?;
    // A per-call cap (the cache-warming ping's 1-token cap) takes precedence
    // over the configured output cap; `None` falls back to the config value.
    let max_output_tokens = params
        .max_output_tokens_override
        .or(config.max_output_tokens_for_model(params.model));
    // The stateless ChatGPT/Codex backend rejects `system`-role input items,
    // ignores `previous_response_id`, and expects the full history on every
    // request, so its system prompt is hoisted into `instructions`, the chained
    // tool tail is not used, and `store` is off. Every other Responses provider
    // keeps the system message inline, chains its tail, and stores — untouched.
    //
    // The hoist only JOINS the prompt; `SystemPrompt::Hoisted` then makes the
    // lowering skip those messages, so `messages` is borrowed throughout.
    let stateless = config.responses_stateless();
    let instructions = stateless
        .then(|| split_system_prompt(params.messages))
        .flatten();
    let system = if stateless {
        SystemPrompt::Hoisted
    } else {
        SystemPrompt::Inline
    };
    // Stateless providers send the FULL `messages` (already containing every
    // earlier tool-loop turn) with no chain; chained providers continue their
    // chain, deriving the tool-result tail from `messages` inside the builder.
    let previous_response_id = if stateless {
        None
    } else {
        params.previous_response_id
    };
    let input_value = build_responses_input(params.messages, previous_response_id, system)?;

    let mut responses_tools: Vec<ResponsesTool> =
        params.tools.iter().map(ResponsesTool::from).collect();
    // No `!stateless` guard here: `params.programmatic_tool_calling` already
    // comes from `ServiceConfig::programmatic_tool_calling_for_model`, which is
    // where the stateless dialect's rejection of hosted tools is decided. One
    // policy site, so the withholding cannot drift from the tool list.
    if params.programmatic_tool_calling {
        responses_tools.push(ResponsesTool {
            kind: "programmatic_tool_calling".to_string(),
            name: String::new(),
            description: String::new(),
            parameters: serde_json::Value::Null,
            strict: false,
            output_schema: None,
            allowed_callers: None,
        });
    }
    let tools_opt = if responses_tools.is_empty() {
        None
    } else {
        Some(responses_tools)
    };

    // Reasoning config lives under a nested `reasoning` object in the Responses
    // API. Two former mistakes are fixed here:
    //   * the Chat Completions top-level `reasoning_effort` is rejected
    //     ("Unsupported parameter: 'reasoning_effort' … moved to
    //     'reasoning.effort'"), so the effort goes in `reasoning.effort`;
    //   * the reasoning summary is requested via `reasoning.summary` ("auto"),
    //     NOT via `include` — `include` accepts only a fixed enum
    //     (`reasoning.encrypted_content`, …) and 400s on `reasoning.summary`.
    //
    // The whole object is emitted ONLY for models the catalogue records as
    // reasoning-capable: non-reasoning models (gpt-4o, gpt-4.1, …) reject any
    // reasoning config outright (400), so a stray non-`off` effort must never
    // leak one through — gate on the capability, not on effort being present.
    let reasoning_capable = config.model_supports_reasoning(params.model);
    let reasoning = reasoning_capable.then_some(ResponsesReasoning {
        effort: params.reasoning_effort,
        summary: Some("auto"),
    });

    // tool_choice: "auto" tells the model to use function calling.
    // Without this, some models may generate tool calls as plain text instead.
    let tool_choice = if tools_opt.is_some() {
        Some("auto".into())
    } else {
        None
    };

    // The turn's real session id (the gateway route) drives per-session cache
    // affinity; a `None` route means no hint regardless of the dialect. The
    // dialect is filtered through the rejection registry so a provider that
    // answered a 400 to this field stops receiving it for the process.
    let session_id = params.route.map(|(session_id, _request_id)| session_id);
    let affinity = session_id.map_or_default(|session_id| {
        crate::session_routing::cache_affinity_fields(
            config.effective_cache_affinity(params.model),
            session_id,
            params.model,
        )
    });
    // Extended prompt-cache retention, per model and opt-in. The Responses API
    // is where the newer dialect's extended `"12h"` value is accepted.
    let retention =
        crate::cache_retention::cache_retention_fields(&config.provider_slug, params.model, true);

    let body = serde_json::to_value(&ResponsesRequest {
        model: params.model,
        input: input_value,
        instructions: instructions.as_deref(),
        tools: tools_opt,
        stream,
        max_output_tokens,
        reasoning,
        // Stateless mode keeps no server-side transcript, so the opaque
        // encrypted reasoning payload is the only continuity payload available:
        // ask for it on every reasoning item and replay the captured items into
        // `input` (the provider's passback policy is `all_turns`). A chained
        // provider needs none — `previous_response_id` already carries its
        // reasoning, and an unused `include` would only add a rejected field.
        //
        // Gated on the reasoning capability too, for the same reason the
        // `reasoning` object below is: `include` is validated strictly, and a
        // model that does not reason has no reasoning item to return.
        include: (stateless && reasoning_capable).then_some(vec!["reasoning.encrypted_content"]),
        // store must be `Some(true)` for `previous_response_id` chaining on the
        // standard Responses API, and an explicit `Some(false)` for the stateless
        // ChatGPT/Codex backend: that request must never persist, and an omitted
        // field would leave the backend's own storing default in force.
        store: Some(!stateless),
        previous_response_id,
        parallel_tool_calls: None,
        tool_choice,
        prompt_cache_key: affinity.prompt_cache_key,
        session_id: affinity.session_id,
        prompt_cache_retention: retention.prompt_cache_retention,
        prompt_cache_options: retention.prompt_cache_options,
    })
    .map_err(|e| super::super::OpenAiError::Io(io::Error::other(e)))?;

    Ok((url, body))
}

/// Build the URL and serialised request body for a simple (no-tool) Responses API request.
/// Unlike `build_responses_request_body`, this sends `input` as a plain string and
/// omits instructions, tools, and chaining fields — appropriate for one-shot completions.
pub(super) fn build_simple_responses_body(
    config: &super::super::ServiceConfig,
    model: &str,
    prompt: &str,
    stream: bool,
) -> Result<(String, serde_json::Value), super::super::OpenAiError> {
    let url = endpoint_url(&config.base_url, &config.responses_path)?;
    let body = serde_json::to_value(&ResponsesRequest {
        model,
        // Simple (non-turn) requests send input as a plain string rather than
        // an items array.  The server expands it to a single user message.
        input: Some(serde_json::Value::String(prompt.to_string())),
        instructions: None,
        tools: None,
        stream,
        max_output_tokens: None,
        reasoning: None,
        // A one-shot prompt carries no captured reasoning to replay, so it asks
        // for no extra output items.
        include: None,
        // Simple requests are ephemeral — don't persist on the server side, and
        // say so explicitly (`Some(false)` rather than `None`), so the one-shot
        // prompt never depends on a backend's storing default.
        store: Some(false),
        previous_response_id: None,
        parallel_tool_calls: None,
        tool_choice: None,
        // The one-shot prompt API carries no session, so no cache-affinity hint.
        prompt_cache_key: None,
        session_id: None,
        // Retention is a per-model turn policy; the session-less one-shot
        // prompt helper sends none.
        prompt_cache_retention: None,
        prompt_cache_options: None,
    })
    .map_err(|e| super::super::OpenAiError::Io(io::Error::other(e)))?;
    Ok((url, body))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_joins_the_system_prompt_and_leaves_the_messages_untouched() {
        let messages = vec![
            ChatRequestMessage::simple("system", "be helpful".into()),
            ChatRequestMessage::simple("user", "hi".into()),
        ];
        assert_eq!(
            split_system_prompt(&messages).as_deref(),
            Some("be helpful")
        );
        // The split is a read, never a filter: the caller's slice is unchanged
        // and the system item is dropped later, by `SystemPrompt::Hoisted`.
        assert_eq!(messages.len(), 2);
    }

    #[test]
    fn split_joins_multiple_system_messages_and_skips_blank() {
        let messages = vec![
            ChatRequestMessage::simple("system", "first".into()),
            ChatRequestMessage::simple("system", "   ".into()),
            ChatRequestMessage::simple("system", "second".into()),
            ChatRequestMessage::simple("user", "hi".into()),
        ];
        // Non-blank system prompts joined by a blank line; the blank one dropped.
        assert_eq!(
            split_system_prompt(&messages).as_deref(),
            Some("first\n\nsecond")
        );
    }

    #[test]
    fn split_with_no_system_message_yields_none() {
        let messages = vec![ChatRequestMessage::simple("user", "hi".into())];
        assert!(split_system_prompt(&messages).is_none());
    }

    #[test]
    fn chain_selection_borrows_from_the_original_history() {
        // The chained tail is a SELECTION over the caller's slice, not a copy:
        // its items must point into the original history. A `.clone()` here
        // would deep-copy every prior turn (image bytes included) on every
        // tool-loop iteration, which is what this test exists to prevent.
        let messages = vec![
            ChatRequestMessage::simple("system", "be helpful".into()),
            ChatRequestMessage::simple("user", "first".into()),
            ChatRequestMessage::simple("assistant", "answer".into()),
            ChatRequestMessage::simple("user", "second".into()),
        ];
        let tail: Vec<&ChatRequestMessage> = chain_input_messages(&messages).collect();
        assert_eq!(tail.len(), 2, "the newest system message plus the tail");
        assert!(
            std::ptr::eq(tail[0], &raw const messages[0]),
            "the hoisted system prompt is borrowed, not copied"
        );
        assert!(
            std::ptr::eq(tail[1], &raw const messages[3]),
            "the new user turn is borrowed, not copied"
        );
    }
}
