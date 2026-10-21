use serde::{Deserialize, Serialize};
use tracing::{debug, info, trace, warn};

use super::retry;
use super::{
    ChatRequestMessage, ChatToolDefinition, ResponsesStreamEvent, SseReader, endpoint_url,
    messages_to_responses_input, parse_responses_stream_event, validate_tool_call_arguments,
};
use crate::ToolResultItem;
use crate::shared::{MAX_TOOL_CALLS, RequestAuth};
use crate::types::{
    CallerInfo, ChatAssistantToolUse, ChatToolCall, ChatTurnResult, FinalTextResult, StreamEvent,
};
use choreo_proto::{ReasoningArtifact, TokenUsage};
use std::collections::HashMap;
use std::io;

// ── Responses API wire types ─────────────────────────────────────────────

#[derive(Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(crate) enum ResponsesInputItem {
    Message {
        role: String,
        /// A plain string, or an array of `input_text` / `input_image` parts
        /// when the message carries images (vision). Built via
        /// [`ChatRequestMessage::responses_content_value`].
        content: serde_json::Value,
    },
    #[serde(rename = "function_call_output")]
    FunctionCallOutput {
        call_id: String,
        output: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        caller: Option<CallerInfo>,
    },
}

/// Nested `reasoning` object for Responses requests.
///
/// The Responses API moved the Chat Completions top-level `reasoning_effort`
/// into this object (`reasoning.effort`), and the reasoning *summary* is
/// requested here too (`reasoning.summary`) — it is **not** an `include`
/// value (the `include` enum is a fixed set; `reasoning.summary` is a 400).
#[derive(Debug, Serialize)]
pub(crate) struct ResponsesReasoning<'a> {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) effort: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) summary: Option<&'a str>,
}

#[derive(Debug, Serialize)]
pub(crate) struct ResponsesRequest<'a> {
    pub(crate) model: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) input: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) instructions: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) tools: Option<Vec<ResponsesTool>>,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub(crate) stream: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) max_output_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) reasoning: Option<ResponsesReasoning<'a>>,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub(crate) store: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) previous_response_id: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) parallel_tool_calls: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) tool_choice: Option<serde_json::Value>,
}

/// Raw Responses API response envelope.
///
/// Fields like `id` come from the wire but aren't always read in the
/// current code path — they're kept for deserialization completeness
/// and future use (streaming contexts, resumption, etc.).
///
/// `output` holds each item as the raw `serde_json::Value` exactly as
/// received — mirroring the streaming path, which keeps reasoning items as
/// raw values. Display/tool-call extraction parses them lazily into
/// `ResponseOutputItem`, while the reasoning round-trip artifact preserves
/// every field (including unknown ones) byte-for-byte.
#[derive(Debug, Deserialize)]
// Read only in some build targets, so `dead_code` cannot be an `expect`;
// kept as an `allow` with its `allow_attributes` exemption.
#[allow(clippy::allow_attributes)]
#[allow(dead_code)]
pub(crate) struct ResponsesResponse {
    #[serde(default)]
    pub(crate) id: Option<String>,
    #[serde(default)]
    pub(crate) status: Option<String>,
    #[serde(default)]
    pub(crate) output: Vec<serde_json::Value>,
    #[serde(default)]
    pub(crate) usage: Option<super::Usage>,
}

/// Items in a Responses API response output array.
///
/// Deserialize-only: the raw `serde_json::Value` of each output item is kept
/// for the round-trip artifact, so this typed form is used solely for
/// display/tool-call extraction and is never re-serialized (re-serializing
/// through the struct would drop unknown fields like a newer `content` shape).
/// Fields like `id`/`role` are kept for wire-format completeness (matching
/// `ResponsesResponse`) even though the current code reads only the fields it
/// needs.
#[derive(Debug, Deserialize)]
#[expect(dead_code)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(crate) enum ResponseOutputItem {
    Message {
        #[serde(default)]
        content: Vec<ResponseContentPart>,
        #[serde(default)]
        role: Option<String>,
    },
    Reasoning {
        #[serde(default)]
        id: Option<String>,
        #[serde(default)]
        summary: Vec<serde_json::Value>,
        /// Opaque encrypted reasoning content — present only when the request
        /// was sent with `store: false` (stateless mode). Kept verbatim for
        /// the round-trip artifact; never interpreted by the daemon.
        #[serde(default)]
        encrypted_content: Option<String>,
    },
    #[serde(rename = "function_call")]
    FunctionCall {
        call_id: String,
        name: String,
        arguments: String,
        #[serde(default)]
        caller: Option<CallerInfo>,
    },
    Program {
        #[serde(default)]
        id: Option<String>,
        call_id: String,
        #[serde(default)]
        code: Option<String>,
        #[serde(default)]
        fingerprint: Option<String>,
    },
    #[serde(rename = "program_output")]
    ProgramOutput {
        #[serde(default)]
        id: Option<String>,
        call_id: String,
        #[serde(default)]
        result: Option<String>,
        #[serde(default)]
        status: Option<String>,
    },
}

#[derive(Debug, Deserialize)]
pub(crate) struct ResponseContentPart {
    #[serde(rename = "type")]
    pub(crate) kind: String,
    pub(crate) text: Option<String>,
}

/// A tool definition in a Responses API request.
///
/// For regular function tools all fields are used; for the
/// `programmatic_tool_calling` hosted tool only `type` is needed — the
/// empty name/description/parameters are omitted via `skip_serializing_if`
/// so the wire format matches the `OpenAI` spec (just `{"type":"programmatic_tool_calling"}`).
/// See <https://developers.openai.com/api/docs/guides/tools-programmatic-tool-calling>
#[derive(Debug, Serialize)]
pub(crate) struct ResponsesTool {
    #[serde(rename = "type")]
    pub(crate) kind: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub(crate) name: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub(crate) description: String,
    #[serde(skip_serializing_if = "serde_json::Value::is_null")]
    pub(crate) parameters: serde_json::Value,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub(crate) strict: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) output_schema: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) allowed_callers: Option<Vec<super::AllowedCaller>>,
}

impl From<&ChatToolDefinition> for ResponsesTool {
    fn from(tool: &ChatToolDefinition) -> Self {
        Self {
            kind: "function".to_string(),
            name: tool.function.name.clone(),
            description: tool.function.description.clone(),
            parameters: tool.function.parameters.clone(),
            strict: false,
            output_schema: tool.function.output_schema.clone(),
            allowed_callers: tool.function.allowed_callers.clone(),
        }
    }
}

// ── Shared helpers ───────────────────────────────────────────────────────

/// Build the `input` JSON value for a Responses API request, based on
/// whether this is a follow-up turn carrying tool results or a first turn
/// built from message history.
fn build_responses_input(
    tool_results: &[ToolResultItem],
    messages: &[ChatRequestMessage],
    previous_response_id: Option<&str>,
) -> Result<Option<serde_json::Value>, super::OpenAiError> {
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
            serde_json::to_value(&items).map_err(|e| super::OpenAiError::Io(io::Error::other(e)))?
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
            .map_err(|e| super::OpenAiError::Io(io::Error::other(e)))?;
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
fn chain_input_messages(messages: &[ChatRequestMessage]) -> Vec<ChatRequestMessage> {
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
fn extract_reasoning_text(summary: &[serde_json::Value]) -> Option<String> {
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
fn build_responses_request_body(
    config: &super::ServiceConfig,
    params: &super::TurnParams<'_>,
    stream: bool,
) -> Result<(String, serde_json::Value), super::OpenAiError> {
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
    })
    .map_err(|e| super::OpenAiError::Io(io::Error::other(e)))?;

    Ok((url, body))
}

/// Build the URL and serialised request body for a simple (no-tool) Responses API request.
/// Unlike `build_responses_request_body`, this sends `input` as a plain string and
/// omits instructions, tools, and chaining fields — appropriate for one-shot completions.
fn build_simple_responses_body(
    config: &super::ServiceConfig,
    model: &str,
    prompt: &str,
    stream: bool,
) -> Result<(String, serde_json::Value), super::OpenAiError> {
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
    })
    .map_err(|e| super::OpenAiError::Io(io::Error::other(e)))?;
    Ok((url, body))
}

// ── Simple (no-tool) responses request ───────────────────────────────────

pub(crate) fn responses_request(
    agent: &ureq::Agent,
    config: &super::ServiceConfig,
    auth: &RequestAuth,
    model: &str,
    prompt: &str,
    cancel_rx: Option<&crossbeam_channel::Receiver<()>>,
) -> Result<String, super::OpenAiError> {
    let (url, body) = build_simple_responses_body(config, model, prompt, false)?;
    let retry = retry::retry_config_from_config(config);
    // Hoist the no-op retry callback into a named local: a bare `&mut None`
    // temporary would be dropped before the retry call below (E0716).
    let mut no_retry = None;
    let mut ctx = retry::AttemptContext::new(&mut no_retry, cancel_rx, None);
    // No session identity on the plain prompt API — no gateway routing headers.
    let response = retry::retry_send(
        retry::RetrySend {
            agent,
            url: &url,
            auth,
            body: &body,
            config,
            retry_cfg: &retry,
            route: None,
            on_uploaded: None,
        },
        &mut ctx,
    )?;
    let payload: ResponsesResponse = response
        .into_body()
        .read_json()
        .map_err(|e| super::OpenAiError::Io(io::Error::other(e)))?;

    let mut content = String::new();
    for raw in payload.output {
        // Parse each raw item for message-text extraction; a malformed item
        // surfaces as an error (matching the previous typed-deserialization
        // behavior) rather than being silently skipped.
        let item: ResponseOutputItem =
            serde_json::from_value(raw).map_err(|e| super::OpenAiError::Io(io::Error::other(e)))?;
        if let ResponseOutputItem::Message { content: parts, .. } = item
            && let Some(text) = parts
                .into_iter()
                .filter_map(|part| part.text)
                .map(|text| text.trim().to_string())
                .find(|text| !text.is_empty())
        {
            content = text;
            break;
        }
    }

    if content.is_empty() {
        return Err(super::OpenAiError::EmptyResponse);
    }

    Ok(content)
}

// ── Simple streaming responses request ───────────────────────────────────

pub(crate) fn responses_request_streaming<F>(
    agent: &ureq::Agent,
    config: &super::ServiceConfig,
    auth: &RequestAuth,
    params: super::SimpleParams<'_>,
    on_event: &mut F,
) -> Result<(), super::OpenAiError>
where
    F: FnMut(StreamEvent) -> io::Result<()>,
{
    let super::SimpleParams {
        model,
        prompt,
        cancel_rx,
        ..
    } = params;
    let (url, body) = build_simple_responses_body(config, model, prompt, true)?;
    let retry = retry::retry_config_from_config(config);
    // Per-attempt wall-clock deadline spanning the whole request (see `retry::AttemptDeadline`).
    let mut deadline = retry::AttemptDeadline::new(config.total_timeout_secs);
    // Hoist the no-op retry callback into a named local: a bare `&mut None`
    // temporary would be dropped before the retry call below (E0716).
    let mut no_retry = None;
    let mut ctx = retry::AttemptContext::new(&mut no_retry, cancel_rx, Some(&mut deadline));
    // No session identity on the plain prompt API — no gateway routing headers.
    let response = retry::retry_send(
        retry::RetrySend {
            agent,
            url: &url,
            auth,
            body: &body,
            config,
            retry_cfg: &retry,
            route: None,
            on_uploaded: None,
        },
        &mut ctx,
    )?;
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
    let mut has_any_output = false;
    while let Some(data) = crate::stream::recv_sse_event(&sse, cancel_rx)? {
        let event = parse_responses_stream_event(&data)?;
        match event {
            Some(ResponsesStreamEvent::TextDelta(text)) => {
                has_any_output = true;
                on_event(StreamEvent::Answer(text))?;
            }
            Some(ResponsesStreamEvent::TextDone) => {
                // Text output is complete; continue waiting for completion event.
            }
            Some(ResponsesStreamEvent::ResponseCompleted { .. }) => {
                break;
            }
            Some(ResponsesStreamEvent::ResponseFailed(error)) => {
                return Err(super::OpenAiError::Io(io::Error::other(error)));
            }
            Some(ResponsesStreamEvent::ResponseIncomplete) => {
                return Err(super::OpenAiError::Io(io::Error::other(
                    "response incomplete",
                )));
            }
            _ => {}
        }
    }

    if !has_any_output {
        return Err(super::OpenAiError::EmptyResponse);
    }

    Ok(())
}

// ── Non-streaming responses with tools ───────────────────────────────────

/// Non-streaming Responses API turn with tool definitions, reasoning effort,
/// and optional tool results from a previous turn.
pub(crate) fn responses_request_with_tools(
    agent: &ureq::Agent,
    config: &super::ServiceConfig,
    auth: &RequestAuth,
    params: super::TurnParams<'_>,
) -> Result<ChatTurnResult, super::OpenAiError> {
    let start = std::time::Instant::now();

    let (url, body) = build_responses_request_body(config, &params, false)?;
    let super::TurnParams {
        model,
        messages,
        tools,
        on_retry,
        on_uploaded,
        cancel_rx,
        route,
        tool_results,
        no_retry,
        ..
    } = params;

    let has_instructions = messages.iter().any(|m| m.role == "system");
    info!(
        model = %model,
        tool_count = tools.len(),
        tool_result_count = tool_results.len(),
        has_instructions = has_instructions,
        "responses request with tools",
    );

    let retry = retry::retry_config_from_config(config).with_no_retry(no_retry);
    // The upload hook fires once the payload is fully sent, so a non-streaming
    // provider reports the same `Waiting` boundary as the streaming path.
    let mut ctx = retry::AttemptContext::new(on_retry, cancel_rx, None);
    let response = retry::retry_send(
        retry::RetrySend {
            agent,
            url: &url,
            auth,
            body: &body,
            config,
            retry_cfg: &retry,
            route,
            on_uploaded,
        },
        &mut ctx,
    )?;
    let payload: ResponsesResponse = response
        .into_body()
        .read_json()
        .map_err(|e| super::OpenAiError::Io(io::Error::other(e)))?;

    let elapsed = start.elapsed();
    debug!(
        model = %model,
        elapsed_ms = elapsed.as_millis(),
        prompt_tokens = payload.usage.as_ref().map(|u| u.prompt_tokens),
        completion_tokens = payload.usage.as_ref().map(|u| u.completion_tokens),
        total_tokens = payload.usage.as_ref().map(|u| u.total_tokens),
        "responses turn",
    );

    responses_response_to_turn(payload)
}

/// Build the opaque `ResponsesItems` artifact from the collected reasoning
/// output items, or `None` when nothing was captured. The payload is the
/// JSON serialization of the items exactly as received (type tag, id,
/// summary and — in stateless mode — `encrypted_content`).
fn responses_items_artifact(
    items: &[serde_json::Value],
) -> Result<Option<ReasoningArtifact>, super::OpenAiError> {
    if items.is_empty() {
        return Ok(None);
    }
    let bytes =
        serde_json::to_vec(items).map_err(|e| super::OpenAiError::Io(io::Error::other(e)))?;
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
fn merge_reasoning_item(items: &mut Vec<serde_json::Value>, item: serde_json::Value) {
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
fn responses_response_to_turn(
    payload: ResponsesResponse,
) -> Result<ChatTurnResult, super::OpenAiError> {
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
            .map_err(|e| super::OpenAiError::Io(io::Error::other(e)))?;
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
        return Err(super::OpenAiError::TruncatedToolCall { discarded });
    }

    if full_text.is_empty() {
        return Err(super::OpenAiError::EmptyResponse);
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

// ── Responses tool call accumulator ──────────────────────────────────────

/// Accumulator for Responses API tool call arguments keyed by `call_id`.
/// Used in `responses_request_streaming_with_tools` to merge delta chunks.
///
/// # Delta-vs-Done semantics
///
/// The Responses API sends tool call arguments via two SSE event types:
///
/// - `FunctionCallArgumentsDelta` — a chunk of JSON to **append** to the
///   existing arguments accumulator (`arguments.push_str(&delta)`).
/// - `FunctionCallArgumentsDone` — the **complete** final arguments string.
///   When received, `arguments` is **replaced** (not appended), overwriting
///   any partial accumulation from prior delta events.
///
/// This matches the `OpenAI` spec: the done event carries the full final
/// arguments JSON, while delta events carry incremental fragments.
struct AccCall {
    name: Option<String>,
    arguments: String,
    /// Monotonically increasing index assigned to this tool call, used to
    /// preserve insertion order since the Responses API does not provide
    /// server-side indices like Chat Completions does.
    index: u32,
}

impl AccCall {
    fn new(index: u32) -> Self {
        Self {
            name: None,
            arguments: String::new(),
            index,
        }
    }
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
    config: &super::ServiceConfig,
    auth: &RequestAuth,
    params: super::TurnParams<'_>,
    on_event: &mut F,
) -> Result<ChatTurnResult, super::OpenAiError>
where
    F: FnMut(StreamEvent) -> io::Result<()>,
{
    let (url, body) = build_responses_request_body(config, &params, true)?;
    let super::TurnParams {
        model,
        messages: _,
        tools,
        on_retry,
        on_uploaded,
        cancel_rx,
        route,
        tool_results,
        no_retry,
        ..
    } = params;

    info!(
        model = %model,
        tool_count = tools.len(),
        tool_result_count = tool_results.len(),
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
            config,
            retry_cfg: &retry,
            route,
            on_uploaded,
        },
        &mut ctx,
    )?;

    let mut has_any_output = false;
    let mut full_content = String::new();
    let mut full_reasoning = String::new();
    let mut last_usage: Option<TokenUsage> = None;
    let mut response_id: Option<String> = None;
    // Opaque reasoning items collected across the stream (deduped by id in
    // `merge_reasoning_item`) — serialized into the round-trip artifact at
    // the end.
    let mut reasoning_items: Vec<serde_json::Value> = Vec::new();

    let mut acc_calls: HashMap<String, AccCall> = HashMap::new();
    let mut next_tool_index: u32 = 0;

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
                has_any_output = true;
                full_content.push_str(&text);
                on_event(StreamEvent::Answer(text))?;
            }
            Some(ResponsesStreamEvent::TextDone) => {
                // Text output is complete — continue listening for the
                // completion / failure events and any remaining tool calls.
            }
            Some(ResponsesStreamEvent::ReasoningSummary(summary)) => {
                if let Some(text) = extract_reasoning_text(&summary) {
                    has_any_output = true;
                    full_reasoning.push_str(&text);
                    on_event(StreamEvent::Reasoning(text))?;
                }
            }
            Some(ResponsesStreamEvent::ReasoningItem(item)) => {
                // Capture the opaque reasoning item verbatim (id, summary,
                // and encrypted_content in stateless mode) for the
                // round-trip artifact. `output_item.added` and `.done` both
                // map here — merge_reasoning_item dedupes by id.
                merge_reasoning_item(&mut reasoning_items, item);
            }
            Some(ResponsesStreamEvent::FunctionCallArgumentsDelta { call_id, delta }) => {
                // `call_id` is provided by the trusted API server — no validation
                // needed; it's used directly as a HashMap key to accumulate deltas
                // for this specific tool call.
                has_any_output = true;
                if acc_calls.len() >= MAX_TOOL_CALLS {
                    return Err(super::OpenAiError::Io(io::Error::other(format!(
                        "too many tool calls (max {MAX_TOOL_CALLS})"
                    ))));
                }
                let entry = acc_calls.entry(call_id).or_insert_with(|| {
                    let i = next_tool_index;
                    next_tool_index += 1;
                    AccCall::new(i)
                });
                entry.arguments.push_str(&delta);
            }
            Some(ResponsesStreamEvent::FunctionCallArgumentsDone {
                call_id,
                name,
                arguments,
            }) => {
                // `call_id` comes from the trusted API server — safe to use as a
                // HashMap key.  `arguments` is the complete final JSON string
                // (not a delta), so it replaces any accumulated partial value.
                has_any_output = true;
                let entry = acc_calls.entry(call_id).or_insert_with(|| {
                    let i = next_tool_index;
                    next_tool_index += 1;
                    AccCall::new(i)
                });
                entry.name = Some(name);
                entry.arguments = arguments;
                trace!(
                    tool_name = ?entry.name,
                    args_len = entry.arguments.len(),
                    "openai responses: function call args done",
                );
            }
            Some(ResponsesStreamEvent::ResponseCompleted { id, usage }) => {
                response_id = id;
                if let Some(u) = usage {
                    debug!(
                        prompt_tokens = u.prompt_tokens,
                        completion_tokens = u.completion_tokens,
                        total_tokens = u.total_tokens,
                        "responses streaming turn usage",
                    );
                    let usage = TokenUsage {
                        input_tokens: u.prompt_tokens,
                        output_tokens: u.completion_tokens,
                        total_tokens: u.total_tokens,
                        // See responses_response_to_turn: Responses API
                        // cached-token plumbing is a possible follow-up.
                        cached_tokens: 0,
                        cache_write_tokens: 0,
                    };
                    last_usage = Some(usage);
                }
                break;
            }
            Some(ResponsesStreamEvent::ResponseFailed(error)) => {
                return Err(super::OpenAiError::Io(io::Error::other(error)));
            }
            Some(ResponsesStreamEvent::ResponseIncomplete) => {
                return Err(super::OpenAiError::Io(io::Error::other(
                    "response incomplete",
                )));
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

    if !has_any_output {
        return Err(super::OpenAiError::EmptyResponse);
    }

    if !acc_calls.is_empty() {
        let mut sorted: Vec<(String, AccCall)> = acc_calls.into_iter().collect();
        sorted.sort_by_key(|(_, acc)| acc.index);
        let total_calls = sorted.len();

        // First pass: filter out calls without a name (the provider sent
        // incomplete function call events, e.g. arguments delta without a
        // done event).
        let mut tool_calls: Vec<ChatToolCall> = sorted
            .into_iter()
            .filter_map(|(call_id, acc)| {
                let name = acc.name.as_deref().filter(|n| !n.is_empty())?;
                Some(ChatToolCall {
                    id: call_id,
                    name: name.to_string(),
                    arguments_json: acc.arguments,
                    caller: None,
                })
            })
            .collect();

        // Second pass: discard calls with truncated arguments JSON.
        let discarded = validate_tool_call_arguments(&mut tool_calls);

        if !tool_calls.is_empty() {
            return Ok(ChatTurnResult::ToolUse(ChatAssistantToolUse {
                content: if full_content.is_empty() {
                    None
                } else {
                    Some(full_content)
                },
                tool_calls,
                reasoning: if full_reasoning.is_empty() {
                    None
                } else {
                    Some(full_reasoning)
                },
                usage: last_usage,
                response_id,
                reasoning_artifact,
            }));
        }

        // All calls were discarded — show a user-visible error in the TUI,
        // not just a log line, unless the model also produced text.
        if !discarded.is_empty() && !full_content.is_empty() {
            return Ok(ChatTurnResult::FinalText(FinalTextResult {
                content: full_content,
                truncated: false,
                reasoning: if full_reasoning.is_empty() {
                    None
                } else {
                    Some(full_reasoning)
                },
                usage: last_usage,
                response_id,
                reasoning_artifact,
            }));
        }
        if !discarded.is_empty() {
            return Err(super::OpenAiError::TruncatedToolCall { discarded });
        }
        warn!("discarded {total_calls} incomplete tool call(s) from provider (no name set)",);
    }

    // If only reasoning events arrived without any answer text, treat it as
    // empty — matching the non-streaming variant's behaviour.
    if full_content.is_empty() {
        return Err(super::OpenAiError::EmptyResponse);
    }

    Ok(ChatTurnResult::FinalText(FinalTextResult {
        content: full_content,
        truncated: false,
        reasoning: if full_reasoning.is_empty() {
            None
        } else {
            Some(full_reasoning)
        },
        usage: last_usage,
        response_id,
        reasoning_artifact,
    }))
}

// The test suite lives in a sibling file so the implementation stays
// readable at a glance; the module path is unchanged, so the moved tests
// reach their subjects through the same `super` paths.
#[cfg(test)]
mod tests;
