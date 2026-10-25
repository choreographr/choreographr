//! Request-body construction for the Responses API.
//!
//! The Responses wire format takes a flat `input` array of typed items rather
//! than a `messages` list, so a canonical message slice has to be lowered item
//! by item; [`super::super::messages_to_responses_input`] does that, and this
//! module assembles the surrounding body (tool results, chained tail, reasoning
//! payload, tool definitions) for both the simple and the tool-carrying paths.

use std::io;

use super::super::super::ToolResultItem;
use super::super::{ChatRequestMessage, endpoint_url, messages_to_responses_input};
use super::wire::{ResponsesInputItem, ResponsesReasoning, ResponsesRequest, ResponsesTool};

// ── Shared helpers ───────────────────────────────────────────────────────

/// Build the `input` JSON value for a Responses API request, based on
/// whether this is a follow-up turn carrying tool results or a first turn
/// built from message history.
pub(super) fn build_responses_input(
    tool_results: &[ToolResultItem],
    messages: &[ChatRequestMessage],
    previous_response_id: Option<&str>,
) -> Result<Option<serde_json::Value>, super::super::OpenAiError> {
    if tool_results.is_empty() {
        // First call in a turn. When chaining onto a previous response
        // (`previous_response_id` set), the server retains the full
        // conversation up to that response — resending the whole history
        // would duplicate every prior turn on top of the chained context
        // (billing + context-window inflation), so send only the messages
        // that postdate the last assistant response (in practice the new
        // user message) plus the freshly built system prompt.
        let items = if previous_response_id.is_some() {
            messages_to_responses_input(&chain_input_messages(messages))?
        } else {
            // No chain: the full history is the request. Pass `messages` by
            // reference so a long session is not cloned on every first call.
            messages_to_responses_input(messages)?
        };
        // System messages go into `input` as `{role: "system"}` items.
        let input_value = if items.is_empty() {
            serde_json::Value::String(String::new())
        } else {
            serde_json::to_value(&items)
                .map_err(|e| super::super::OpenAiError::Io(io::Error::other(e)))?
        };
        Ok(Some(input_value))
    } else {
        // Returning tool results from a previous turn: build
        // function_call_output items.  Both `instructions` and `messages`
        // are omitted because the Responses API remembers the full
        // conversation history from the original turn via
        // `previous_response_id` — only the new tool results are needed.
        let items: Vec<ResponsesInputItem> = tool_results
            .iter()
            .map(|tr| ResponsesInputItem::FunctionCallOutput {
                call_id: tr.call_id.clone(),
                output: tr.output.clone(),
                caller: tr.caller.clone(),
            })
            .collect();
        let input_value = serde_json::to_value(&items)
            .map_err(|e| super::super::OpenAiError::Io(io::Error::other(e)))?;
        Ok(Some(input_value))
    }
}

/// The minimal input for a fresh user turn chained onto a previous response:
/// the newest system message (the daemon rebuilds the system prompt every
/// request, so the latest one must reach the model even though the chain
/// already carries the old one) plus every message AFTER the last
/// assistant-role message — in the daemon's loop that is exactly the new user
/// message. Everything up to and including the last assistant message is
/// already in the chained response's context and must not be resent.
///
/// Only the *newest* system message is kept: any older system prompt is
/// already in the chained context, and resending it would duplicate it. In
/// the daemon the system message sits at the head of the list, so it leads
/// the request; in the defensive shapes where a system prompt appears
/// mid-tail, it stays in its original position rather than being hoisted,
/// so the conversation order is preserved.
pub(super) fn chain_input_messages(messages: &[ChatRequestMessage]) -> Vec<ChatRequestMessage> {
    match messages.iter().rposition(|m| m.role == "assistant") {
        // No assistant message yet — nothing has been chained, keep all. A
        // chain id implies a prior model call that produced an assistant
        // response, so this arm is defensive only.
        None => messages.to_vec(),
        Some(last_assistant) => {
            // Single pass over every message: drop everything at/before the
            // last assistant message (it is in the chained context) EXCEPT
            // the newest system message, which is rebuilt every turn and must
            // still reach the model. Older system messages — whether in the
            // chained region or inside the tail — are dropped, so the newest
            // one is never duplicated.
            let newest_system = messages.iter().rposition(|m| m.role == "system");
            messages
                .iter()
                .enumerate()
                .filter(|(i, m)| {
                    (*i > last_assistant && m.role != "system") || Some(*i) == newest_system
                })
                .map(|(_, m)| m.clone())
                .collect()
        }
    }
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
    let input_value = build_responses_input(
        params.tool_results,
        params.messages,
        params.previous_response_id,
    )?;

    let mut responses_tools: Vec<ResponsesTool> =
        params.tools.iter().map(ResponsesTool::from).collect();
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
    // affinity; a `None` route means no hint regardless of the dialect.
    let session_id = params.route.map(|(session_id, _request_id)| session_id);
    let affinity = session_id.map_or_default(|session_id| {
        crate::session_routing::cache_affinity_fields(
            config.cache_affinity,
            session_id,
            params.model,
        )
    });

    let body = serde_json::to_value(&ResponsesRequest {
        model: params.model,
        input: input_value,
        instructions: None,
        tools: tools_opt,
        stream,
        max_output_tokens,
        reasoning,
        // store: true is required for previous_response_id to work correctly
        // and matches the @ai-sdk/openai default behavior.
        store: true,
        previous_response_id: params.previous_response_id,
        parallel_tool_calls: None,
        tool_choice,
        prompt_cache_key: affinity.prompt_cache_key,
        session_id: affinity.session_id,
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
        // Simple requests are ephemeral — don't persist on the server side.
        store: false,
        previous_response_id: None,
        parallel_tool_calls: None,
        tool_choice: None,
        // The one-shot prompt API carries no session, so no cache-affinity hint.
        prompt_cache_key: None,
        session_id: None,
    })
    .map_err(|e| super::super::OpenAiError::Io(io::Error::other(e)))?;
    Ok((url, body))
}
