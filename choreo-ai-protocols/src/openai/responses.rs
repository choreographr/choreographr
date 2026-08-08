use serde::{Deserialize, Serialize};
use tracing::{debug, info, trace, warn};

use super::retry;
use super::{
    ChatRequestMessage, ChatToolDefinition, ResponsesStreamEvent, SseReader, endpoint_url,
    messages_to_responses_input, parse_responses_stream_event, validate_tool_call_arguments,
};
use crate::ToolResultItem;
use crate::shared::MAX_TOOL_CALLS;
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
        content: String,
    },
    #[serde(rename = "function_call_output")]
    FunctionCallOutput {
        call_id: String,
        output: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        caller: Option<CallerInfo>,
    },
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
    pub(crate) reasoning_effort: Option<&'a str>,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub(crate) store: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) previous_response_id: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) include: Option<Vec<&'a str>>,
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
#[allow(dead_code)]
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
/// so the wire format matches the OpenAI spec (just `{"type":"programmatic_tool_calling"}`).
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
            name: tool.function.name.to_string(),
            description: tool.function.description.to_string(),
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
#[expect(clippy::too_many_arguments)]
fn build_responses_request_body(
    config: &super::ServiceConfig,
    model: &str,
    messages: &[ChatRequestMessage],
    tools: &[ChatToolDefinition],
    reasoning_effort: Option<&str>,
    previous_response_id: Option<&str>,
    tool_results: &[ToolResultItem],
    stream: bool,
    programmatic_tool_calling: bool,
) -> Result<(String, serde_json::Value), super::OpenAiError> {
    let url = endpoint_url(&config.base_url, &config.responses_path)?;
    let max_output_tokens = config.max_output_tokens_for_model(model);
    let input_value = build_responses_input(tool_results, messages, previous_response_id)?;

    let mut responses_tools: Vec<ResponsesTool> = tools.iter().map(ResponsesTool::from).collect();
    if programmatic_tool_calling {
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

    // Always request reasoning summary — the server omits it for models
    // that don't support reasoning.
    let include = Some(vec!["reasoning.summary"]);

    // tool_choice: "auto" tells the model to use function calling.
    // Without this, some models may generate tool calls as plain text instead.
    let tool_choice = if tools_opt.is_some() {
        Some("auto".into())
    } else {
        None
    };

    let body = serde_json::to_value(&ResponsesRequest {
        model,
        input: input_value,
        instructions: None,
        tools: tools_opt,
        stream,
        max_output_tokens,
        reasoning_effort,
        // store: true is required for previous_response_id to work correctly
        // and matches the @ai-sdk/openai default behavior.
        store: true,
        previous_response_id,
        include,
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
        reasoning_effort: None,
        // Simple requests are ephemeral — don't persist on the server side.
        store: false,
        previous_response_id: None,
        include: None,
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
    api_key: &str,
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
    let response = retry::retry_send(agent, &url, api_key, &body, config, &retry, &mut ctx)?;
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
    api_key: &str,
    model: &str,
    prompt: &str,
    cancel_rx: Option<&crossbeam_channel::Receiver<()>>,
    on_event: &mut F,
) -> Result<(), super::OpenAiError>
where
    F: FnMut(StreamEvent) -> io::Result<()>,
{
    let (url, body) = build_simple_responses_body(config, model, prompt, true)?;
    let retry = retry::retry_config_from_config(config);
    // Per-attempt wall-clock deadline spanning the whole request (see `retry::AttemptDeadline`).
    let mut deadline = retry::AttemptDeadline::new(config.total_timeout_secs);
    // Hoist the no-op retry callback into a named local: a bare `&mut None`
    // temporary would be dropped before the retry call below (E0716).
    let mut no_retry = None;
    let mut ctx = retry::AttemptContext::new(&mut no_retry, cancel_rx, Some(&mut deadline));
    let response = retry::retry_send(agent, &url, api_key, &body, config, &retry, &mut ctx)?;
    let mut reader = SseReader::from_reader(response.into_body().into_reader());
    // Reader thread decouples the blocking socket read from cancellation
    // polling (see `crate::stream`); the abort flag on `sse` stops the thread
    // at its next loop boundary once the consumer cancels or drops it.
    let sse = crate::stream::spawn_sse_reader(move || reader.next_event(), deadline.current());
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
#[expect(clippy::too_many_arguments)]
pub(crate) fn responses_request_with_tools(
    agent: &ureq::Agent,
    config: &super::ServiceConfig,
    api_key: &str,
    model: &str,
    messages: &[ChatRequestMessage],
    tools: &[ChatToolDefinition],
    reasoning_effort: Option<&str>,
    previous_response_id: Option<&str>,
    tool_results: &[ToolResultItem],
    on_retry: &mut Option<retry::RetryCallback>,
    cancel_rx: Option<&crossbeam_channel::Receiver<()>>,
    programmatic_tool_calling: bool,
) -> Result<ChatTurnResult, super::OpenAiError> {
    let start = std::time::Instant::now();

    let (url, body) = build_responses_request_body(
        config,
        model,
        messages,
        tools,
        reasoning_effort,
        previous_response_id,
        tool_results,
        false,
        programmatic_tool_calling,
    )?;

    let has_instructions = messages.iter().any(|m| m.role == "system");
    info!(
        model = %model,
        tool_count = tools.len(),
        tool_result_count = tool_results.len(),
        has_instructions = has_instructions,
        "responses request with tools",
    );

    let retry = retry::retry_config_from_config(config);
    let mut ctx = retry::AttemptContext::new(on_retry, cancel_rx, None);
    let response = retry::retry_send(agent, &url, api_key, &body, config, &retry, &mut ctx)?;
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
/// summary and — in stateless mode — encrypted_content).
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
    // value; otherwise append.
    match items
        .iter()
        .position(|it| it.get("id").and_then(|v| v.as_str()) == Some(id.as_str()))
    {
        Some(pos) => items[pos] = item,
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

/// Accumulator for Responses API tool call arguments keyed by call_id.
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
/// This matches the OpenAI spec: the done event carries the full final
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
#[expect(clippy::too_many_arguments)]
pub(crate) fn responses_request_streaming_with_tools<F>(
    agent: &ureq::Agent,
    config: &super::ServiceConfig,
    api_key: &str,
    model: &str,
    messages: &[ChatRequestMessage],
    tools: &[ChatToolDefinition],
    reasoning_effort: Option<&str>,
    previous_response_id: Option<&str>,
    tool_results: &[ToolResultItem],
    on_retry: &mut Option<retry::RetryCallback>,
    cancel_rx: Option<&crossbeam_channel::Receiver<()>>,
    programmatic_tool_calling: bool,
    on_event: &mut F,
) -> Result<ChatTurnResult, super::OpenAiError>
where
    F: FnMut(StreamEvent) -> io::Result<()>,
{
    let (url, body) = build_responses_request_body(
        config,
        model,
        messages,
        tools,
        reasoning_effort,
        previous_response_id,
        tool_results,
        true,
        programmatic_tool_calling,
    )?;

    info!(
        model = %model,
        tool_count = tools.len(),
        tool_result_count = tool_results.len(),
        streaming = true,
        "responses streaming request with tools",
    );

    let retry = retry::retry_config_from_config(config);
    // Per-attempt wall-clock deadline spanning the whole request (see `retry::AttemptDeadline`).
    let mut deadline = retry::AttemptDeadline::new(config.total_timeout_secs);
    let mut ctx = retry::AttemptContext::new(on_retry, cancel_rx, Some(&mut deadline));
    let response = retry::retry_send(agent, &url, api_key, &body, config, &retry, &mut ctx)?;

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
    let sse = crate::stream::spawn_sse_reader(move || reader.next_event(), deadline.current());
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // ── Serialisation tests ───────────────────────────────────────────

    #[test]
    fn responses_input_item_message_serializes() {
        let item = ResponsesInputItem::Message {
            role: "user".to_string(),
            content: "hello".to_string(),
        };
        let value = serde_json::to_value(&item).unwrap();
        assert_eq!(value["type"], "message");
        assert_eq!(value["role"], "user");
        assert_eq!(value["content"], "hello");
    }

    #[test]
    fn responses_input_item_function_call_output_serializes() {
        let item = ResponsesInputItem::FunctionCallOutput {
            call_id: "call_1".to_string(),
            output: "result".to_string(),
            caller: None,
        };
        let value = serde_json::to_value(&item).unwrap();
        assert_eq!(value["type"], "function_call_output");
        assert_eq!(value["call_id"], "call_1");
        assert_eq!(value["output"], "result");
        // caller should be absent when None
        assert!(value.get("caller").is_none());
    }

    #[test]
    fn responses_request_serializes_with_store_true() {
        let req = ResponsesRequest {
            model: "gpt-4",
            input: Some(json!("hello")),
            instructions: None,
            tools: None,
            stream: false,
            max_output_tokens: None,
            reasoning_effort: None,
            store: true,
            previous_response_id: None,
            include: None,
            parallel_tool_calls: None,
            tool_choice: None,
        };
        let value = serde_json::to_value(&req).unwrap();
        assert_eq!(value["store"], true);
        assert_eq!(value["model"], "gpt-4");
        assert_eq!(value["input"], "hello");
        // instructions and tools should be absent
        assert!(value.get("instructions").is_none());
        assert!(value.get("tools").is_none());
    }

    #[test]
    fn responses_request_serializes_with_tools() {
        let req = ResponsesRequest {
            model: "gpt-4",
            input: Some(json!("hello")),
            instructions: None,
            tools: Some(vec![ResponsesTool {
                kind: "function".to_string(),
                name: "get_weather".to_string(),
                description: "Get the weather".to_string(),
                parameters: json!({"type": "object"}),
                strict: false,
                output_schema: None,
                allowed_callers: None,
            }]),
            stream: false,
            max_output_tokens: None,
            reasoning_effort: None,
            store: false,
            previous_response_id: None,
            include: None,
            parallel_tool_calls: None,
            tool_choice: None,
        };
        let value = serde_json::to_value(&req).unwrap();
        let tools = value["tools"].as_array().unwrap();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0]["type"], "function");
        assert_eq!(tools[0]["name"], "get_weather");
    }

    #[test]
    fn responses_request_serializes_with_programmatic_tool_calling() {
        let req = ResponsesRequest {
            model: "gpt-4",
            input: Some(json!("hello")),
            instructions: None,
            tools: Some(vec![ResponsesTool {
                kind: "programmatic_tool_calling".to_string(),
                name: String::new(),
                description: String::new(),
                parameters: serde_json::Value::Null,
                strict: false,
                output_schema: None,
                allowed_callers: None,
            }]),
            stream: false,
            max_output_tokens: None,
            reasoning_effort: None,
            store: false,
            previous_response_id: None,
            include: None,
            parallel_tool_calls: None,
            tool_choice: None,
        };
        let value = serde_json::to_value(&req).unwrap();
        let tools = value["tools"].as_array().unwrap();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0]["type"], "programmatic_tool_calling");
        // Only the type field should be present
        assert_eq!(tools[0].as_object().unwrap().len(), 1);
    }

    #[test]
    fn responses_request_omits_optional_fields_when_none() {
        let req = ResponsesRequest {
            model: "gpt-4",
            input: None,
            instructions: None,
            tools: None,
            stream: false,
            max_output_tokens: None,
            reasoning_effort: None,
            store: false,
            previous_response_id: None,
            include: None,
            parallel_tool_calls: None,
            tool_choice: None,
        };
        let value = serde_json::to_value(&req).unwrap();
        let obj = value.as_object().unwrap();
        // Only `model` should be present (stream and store are skipped when false)
        assert_eq!(obj.len(), 1);
        assert_eq!(value["model"], "gpt-4");
        assert!(obj.get("instructions").is_none());
        assert!(obj.get("previous_response_id").is_none());
        assert!(obj.get("parallel_tool_calls").is_none());
        assert!(obj.get("tool_choice").is_none());
    }

    #[test]
    fn responses_response_deserializes_with_message_and_usage() {
        let json_str = r#"
            {
                "id": "resp_123",
                "status": "completed",
                "output": [
                    {
                        "type": "message",
                        "role": "assistant",
                        "content": [
                            {"type": "output_text", "text": "Hello there"}
                        ]
                    }
                ],
                "usage": {
                    "prompt_tokens": 10,
                    "completion_tokens": 5,
                    "total_tokens": 15
                }
            }
        "#;
        let response: ResponsesResponse = serde_json::from_str(json_str).unwrap();
        assert_eq!(response.id.as_deref(), Some("resp_123"));
        assert_eq!(response.status.as_deref(), Some("completed"));
        assert_eq!(response.output.len(), 1);
        assert!(response.usage.is_some());
        let usage = response.usage.unwrap();
        assert_eq!(usage.prompt_tokens, 10);
        assert_eq!(usage.completion_tokens, 5);
        assert_eq!(usage.total_tokens, 15);
    }

    #[test]
    fn response_output_item_deserializes_function_call() {
        let json_str =
            r#"{"type":"function_call","call_id":"call_1","name":"get_weather","arguments":"{}"}"#;
        let item: ResponseOutputItem = serde_json::from_str(json_str).unwrap();
        match item {
            ResponseOutputItem::FunctionCall {
                call_id,
                name,
                arguments,
                caller,
            } => {
                assert_eq!(call_id, "call_1");
                assert_eq!(name, "get_weather");
                assert_eq!(arguments, "{}");
                assert!(caller.is_none());
            }
            other => panic!("expected FunctionCall, got {other:?}"),
        }
    }

    #[test]
    fn response_output_item_deserializes_reasoning() {
        let json_str = r#"{"type":"reasoning","summary":[{"text":"thinking..."}]}"#;
        let item: ResponseOutputItem = serde_json::from_str(json_str).unwrap();
        match item {
            ResponseOutputItem::Reasoning {
                id,
                summary,
                encrypted_content,
            } => {
                assert!(id.is_none());
                assert_eq!(summary.len(), 1);
                assert_eq!(summary[0]["text"], "thinking...");
                assert!(encrypted_content.is_none());
            }
            other => panic!("expected Reasoning, got {other:?}"),
        }
    }

    // ── Reasoning artifact capture (phase 2c) ───────────────────────────

    #[test]
    fn non_streaming_captures_reasoning_items_artifact() {
        // A stateless-mode response carries `encrypted_content` on the
        // reasoning item — it must ride along in the artifact verbatim.
        let json_str = r#"{
            "id": "resp_abc",
            "output": [
                {
                    "type": "reasoning",
                    "id": "re_1",
                    "summary": [{"type": "summary_text", "text": "think carefully"}],
                    "encrypted_content": "eJxT_opaque"
                },
                {
                    "type": "message",
                    "role": "assistant",
                    "content": [{"type": "output_text", "text": "Here is the answer"}]
                }
            ]
        }"#;
        let payload: ResponsesResponse = serde_json::from_str(json_str).unwrap();
        let result = responses_response_to_turn(payload).unwrap();
        match result {
            ChatTurnResult::FinalText(f) => {
                assert_eq!(f.content, "Here is the answer");
                // Display text unchanged — the artifact is additive.
                assert_eq!(f.reasoning.as_deref(), Some("think carefully"));
                let expected = serde_json::to_vec(&json!([
                    {
                        "type": "reasoning",
                        "id": "re_1",
                        "summary": [{"type": "summary_text", "text": "think carefully"}],
                        "encrypted_content": "eJxT_opaque"
                    }
                ]))
                .unwrap();
                assert_eq!(
                    f.reasoning_artifact,
                    Some(ReasoningArtifact::ResponsesItems(expected))
                );
            }
            other => panic!("expected FinalText, got {other:?}"),
        }
    }

    #[test]
    fn non_streaming_captures_artifact_on_tool_use() {
        let json_str = r#"{
            "id": "resp_123",
            "output": [
                {"type": "reasoning", "id": "re_1", "summary": [{"text": "picking tool"}]},
                {"type": "function_call", "call_id": "call_1", "name": "get_weather", "arguments": "{\"city\":\"London\"}"}
            ]
        }"#;
        let payload: ResponsesResponse = serde_json::from_str(json_str).unwrap();
        let result = responses_response_to_turn(payload).unwrap();
        match result {
            ChatTurnResult::ToolUse(t) => {
                assert_eq!(t.reasoning.as_deref(), Some("picking tool"));
                // Response id still captured (unchanged behavior).
                assert_eq!(t.response_id.as_deref(), Some("resp_123"));
                let expected = serde_json::to_vec(&json!([
                    {"type": "reasoning", "id": "re_1", "summary": [{"text": "picking tool"}]}
                ]))
                .unwrap();
                assert_eq!(
                    t.reasoning_artifact,
                    Some(ReasoningArtifact::ResponsesItems(expected))
                );
            }
            other => panic!("expected ToolUse, got {other:?}"),
        }
    }

    #[test]
    fn non_streaming_captures_unknown_reasoning_fields_verbatim() {
        // A newer reasoning item shape may carry reasoning text in a `content`
        // array alongside `summary`. The raw-value capture must preserve the
        // unknown field verbatim in the artifact — the previous typed
        // re-serialization would have dropped it (and the display text in
        // `summary` is still picked up for `reasoning`).
        let json_str = r#"{
            "id": "resp_new",
            "output": [
                {
                    "type": "reasoning",
                    "id": "re_new",
                    "summary": [{"type": "summary_text", "text": "visible summary"}],
                    "content": [{"type": "output_text", "text": "thinking"}]
                },
                {
                    "type": "message",
                    "role": "assistant",
                    "content": [{"type": "output_text", "text": "Answer here"}]
                }
            ]
        }"#;
        let payload: ResponsesResponse = serde_json::from_str(json_str).unwrap();
        let result = responses_response_to_turn(payload).unwrap();
        match result {
            ChatTurnResult::FinalText(f) => {
                assert_eq!(f.content, "Answer here");
                // Display text still comes from `summary`.
                assert_eq!(f.reasoning.as_deref(), Some("visible summary"));
                let ReasoningArtifact::ResponsesItems(bytes) =
                    f.reasoning_artifact.expect("artifact captured")
                else {
                    panic!("expected ResponsesItems artifact");
                };
                let items: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
                // The unknown `content` field survives verbatim alongside the
                // known ones — nothing is dropped or re-ordered.
                assert_eq!(items[0]["type"], "reasoning");
                assert_eq!(items[0]["id"], "re_new");
                assert_eq!(items[0]["summary"][0]["text"], "visible summary");
                assert_eq!(items[0]["content"][0]["type"], "output_text");
                assert_eq!(items[0]["content"][0]["text"], "thinking");
            }
            other => panic!("expected FinalText, got {other:?}"),
        }
    }

    #[test]
    fn non_streaming_no_reasoning_yields_none_artifact() {
        // Control case: a response with no reasoning output items.
        let json_str = r#"{
            "output": [
                {"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": "plain"}]}
            ]
        }"#;
        let payload: ResponsesResponse = serde_json::from_str(json_str).unwrap();
        let result = responses_response_to_turn(payload).unwrap();
        match result {
            ChatTurnResult::FinalText(f) => {
                assert_eq!(f.content, "plain");
                assert!(f.reasoning.is_none());
                assert!(f.reasoning_artifact.is_none());
            }
            other => panic!("expected FinalText, got {other:?}"),
        }
    }

    #[test]
    fn merge_reasoning_item_dedupes_by_id_keeping_later() {
        // Providers emit `output_item.added` (item opens) then
        // `output_item.done` (item complete) for the same reasoning item.
        // The later, complete value must win.
        let mut items: Vec<serde_json::Value> = Vec::new();
        merge_reasoning_item(
            &mut items,
            json!({ "id": "re_1", "type": "reasoning", "summary": [] }),
        );
        merge_reasoning_item(
            &mut items,
            json!({
                "id": "re_1",
                "type": "reasoning",
                "summary": [{"text": "final summary"}],
                "encrypted_content": "eJxT_done"
            }),
        );
        merge_reasoning_item(
            &mut items,
            json!({ "id": "re_2", "type": "reasoning", "summary": [] }),
        );
        assert_eq!(items.len(), 2);
        assert_eq!(items[0]["id"], "re_1");
        assert_eq!(items[0]["encrypted_content"], "eJxT_done");
        assert_eq!(items[1]["id"], "re_2");
    }

    #[test]
    fn merge_reasoning_item_without_id_is_appended() {
        let mut items: Vec<serde_json::Value> = Vec::new();
        merge_reasoning_item(&mut items, json!({ "type": "reasoning", "summary": [1] }));
        merge_reasoning_item(&mut items, json!({ "type": "reasoning", "summary": [2] }));
        assert_eq!(items.len(), 2);
    }

    #[test]
    fn responses_items_artifact_serializes_exact_bytes() {
        // The artifact payload is the serde_json serialization of the
        // collected items — pinned byte-exact so a serde refactor cannot
        // silently change the round-trip payload.
        let items = vec![json!({ "type": "reasoning", "id": "re_1" })];
        let artifact = responses_items_artifact(&items).unwrap().unwrap();
        assert_eq!(
            artifact,
            ReasoningArtifact::ResponsesItems(br#"[{"id":"re_1","type":"reasoning"}]"#.to_vec())
        );
        // Empty collection -> None.
        assert!(responses_items_artifact(&[]).unwrap().is_none());
    }

    #[test]
    fn responses_items_artifact_keeps_encrypted_content() {
        // The opaque encrypted payload must survive the serialize step
        // uninterpreted — this is the stateless-mode continuity contract.
        let items = vec![json!({
            "type": "reasoning",
            "id": "re_9",
            "summary": [],
            "encrypted_content": "eJxT_opaque_bytes"
        })];
        let artifact = responses_items_artifact(&items).unwrap().unwrap();
        let decoded: serde_json::Value = match artifact {
            ReasoningArtifact::ResponsesItems(bytes) => serde_json::from_slice(&bytes).unwrap(),
            other => panic!("expected ResponsesItems, got {other:?}"),
        };
        assert_eq!(decoded[0]["encrypted_content"], "eJxT_opaque_bytes");
    }

    #[test]
    fn streaming_reasoning_item_events_flow_into_artifact_bytes() {
        // End-to-end for the streaming capture path without HTTP: feed
        // canned SSE event data through the parser + merge helper, exactly
        // as the streaming loop does, and assert the artifact bytes match.
        let mut items: Vec<serde_json::Value> = Vec::new();
        let added = parse_responses_stream_event(
            r#"{"type":"response.output_item.added","output_index":0,"item":{"id":"re_1","type":"reasoning","summary":[]}}"#,
        )
        .unwrap()
        .expect("event");
        let done = parse_responses_stream_event(
            r#"{"type":"response.output_item.done","output_index":0,"item":{"id":"re_1","type":"reasoning","summary":[{"text":"streamed reasoning"}],"encrypted_content":"eJxT_stream"}}"#,
        )
        .unwrap()
        .expect("event");
        match (added, done) {
            (ResponsesStreamEvent::ReasoningItem(a), ResponsesStreamEvent::ReasoningItem(d)) => {
                merge_reasoning_item(&mut items, a);
                merge_reasoning_item(&mut items, d);
            }
            other => panic!("expected two ReasoningItem events, got {other:?}"),
        }
        let artifact = responses_items_artifact(&items).unwrap().unwrap();
        let expected = serde_json::to_vec(&json!([{
            "id": "re_1",
            "type": "reasoning",
            "summary": [{"text": "streamed reasoning"}],
            "encrypted_content": "eJxT_stream"
        }]))
        .unwrap();
        assert_eq!(artifact, ReasoningArtifact::ResponsesItems(expected));
    }

    #[test]
    fn response_output_item_deserializes_program() {
        let json_str = r#"{"type":"program","call_id":"prog_1","code":"console.log('hello')","fingerprint":"fp_1"}"#;
        let item: ResponseOutputItem = serde_json::from_str(json_str).unwrap();
        match item {
            ResponseOutputItem::Program {
                call_id,
                code,
                id,
                fingerprint,
            } => {
                assert_eq!(call_id, "prog_1");
                assert_eq!(code.as_deref(), Some("console.log('hello')"));
                assert_eq!(fingerprint.as_deref(), Some("fp_1"));
                assert!(id.is_none());
            }
            other => panic!("expected Program, got {other:?}"),
        }
    }

    #[test]
    fn response_output_item_deserializes_program_output() {
        let json_str =
            r#"{"type":"program_output","call_id":"prog_1","result":"ok","status":"completed"}"#;
        let item: ResponseOutputItem = serde_json::from_str(json_str).unwrap();
        match item {
            ResponseOutputItem::ProgramOutput {
                call_id,
                result,
                status,
                id,
            } => {
                assert_eq!(call_id, "prog_1");
                assert_eq!(result.as_deref(), Some("ok"));
                assert_eq!(status.as_deref(), Some("completed"));
                assert!(id.is_none());
            }
            other => panic!("expected ProgramOutput, got {other:?}"),
        }
    }

    // ── Input building tests ──────────────────────────────────────────

    #[test]
    fn build_responses_input_empty_returns_empty_array() {
        let result = build_responses_input(&[], &[], None).unwrap();
        let value = result.expect("expected Some value");
        // Empty messages produce an empty-string input
        assert_eq!(value, json!(""));
    }

    #[test]
    fn build_responses_input_with_tool_results() {
        let tool_results = vec![ToolResultItem {
            call_id: "call_1".to_string(),
            output: "weather_data".to_string(),
            caller: None,
        }];
        // The tool-results branch ignores messages and the chain id entirely:
        // the server reconstructs history from `previous_response_id`.
        let result = build_responses_input(&tool_results, &[], Some("resp_1")).unwrap();
        let value = result.expect("expected Some value");
        let arr = value.as_array().unwrap();
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0]["type"], "function_call_output");
        assert_eq!(arr[0]["call_id"], "call_1");
        assert_eq!(arr[0]["output"], "weather_data");
    }

    #[test]
    fn build_responses_input_with_messages() {
        let messages = vec![
            ChatRequestMessage::simple("user", "hello".into()),
            ChatRequestMessage::simple("assistant", "hi there".into()),
        ];
        let result = build_responses_input(&[], &messages, None).unwrap();
        let value = result.expect("expected Some value");
        let arr = value.as_array().unwrap();
        assert_eq!(arr.len(), 2);
        assert_eq!(arr[0]["role"], "user");
        assert_eq!(arr[0]["content"], "hello");
        assert_eq!(arr[1]["role"], "assistant");
        assert_eq!(arr[1]["content"], "hi there");
    }

    #[test]
    fn build_responses_input_chained_turn_sends_only_trailing_messages() {
        // Chaining a fresh user turn via `previous_response_id`: the server
        // already holds everything up to the last assistant response, so
        // resending the full history would duplicate it. Only the trailing
        // user message (+ any system prompt) may be sent.
        let messages = vec![
            ChatRequestMessage::simple("system", "You are a helpful assistant.".into()),
            ChatRequestMessage::simple("user", "turn one".into()),
            ChatRequestMessage::simple("assistant", "old answer".into()),
            ChatRequestMessage::simple("user", "turn two".into()),
        ];
        let result = build_responses_input(&[], &messages, Some("resp_1")).unwrap();
        let value = result.expect("expected Some value");
        let arr = value.as_array().unwrap();
        // system + the new user message only — the old turns are chained
        // server-side and must not be replayed.
        assert_eq!(arr.len(), 2);
        assert_eq!(arr[0]["role"], "system");
        assert_eq!(arr[1]["role"], "user");
        assert_eq!(arr[1]["content"], "turn two");
    }

    #[test]
    fn build_responses_input_chained_turn_keeps_system_prompt() {
        // The system prompt is rebuilt by the daemon on every request (skills,
        // working dir), so the newest one must still reach the model even
        // though the chain carries the old one — system messages survive the
        // trailing filter.
        let messages = vec![
            ChatRequestMessage::simple("system", "old system".into()),
            ChatRequestMessage::simple("user", "turn one".into()),
            ChatRequestMessage::simple("assistant", "answer".into()),
        ];
        let result = build_responses_input(&[], &messages, Some("resp_1")).unwrap();
        let value = result.expect("expected Some value");
        let arr = value.as_array().unwrap();
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0]["role"], "system");
        assert_eq!(arr[0]["content"], "old system");
    }

    #[test]
    fn build_responses_input_chained_without_assistant_keeps_all() {
        // Defensive: a chain id with no assistant message yet — nothing has
        // been chained server-side, so every message must be kept.
        let messages = vec![
            ChatRequestMessage::simple("user", "hello".into()),
            ChatRequestMessage::simple("user", "again".into()),
        ];
        let result = build_responses_input(&[], &messages, Some("resp_1")).unwrap();
        let value = result.expect("expected Some value");
        let arr = value.as_array().unwrap();
        assert_eq!(arr.len(), 2);
    }

    #[test]
    fn build_responses_input_chained_keeps_only_newest_system_message() {
        // If the caller ever accumulates more than one system message, only the
        // newest may be sent: the older one is already in the chained response's
        // context and resending it would duplicate it (billing + context
        // inflation). The newest system message leads the input.
        let messages = vec![
            ChatRequestMessage::simple("system", "old system".into()),
            ChatRequestMessage::simple("user", "turn one".into()),
            ChatRequestMessage::simple("assistant", "answer".into()),
            ChatRequestMessage::simple("system", "new system".into()),
            ChatRequestMessage::simple("user", "turn two".into()),
        ];
        let result = build_responses_input(&[], &messages, Some("resp_1")).unwrap();
        let value = result.expect("expected Some value");
        let arr = value.as_array().unwrap();
        assert_eq!(arr.len(), 2);
        assert_eq!(arr[0]["role"], "system");
        assert_eq!(arr[0]["content"], "new system");
        assert_eq!(arr[1]["role"], "user");
        assert_eq!(arr[1]["content"], "turn two");
    }

    #[test]
    fn build_responses_input_chained_preserves_mid_tail_order() {
        // Defensive shape: a system prompt appearing INSIDE the tail (after a
        // user message) must stay in its original position rather than being
        // hoisted to the front — the conversation order is preserved. The
        // pre-existing system message at the head is dropped (older prompt,
        // already in the chained context).
        let messages = vec![
            ChatRequestMessage::simple("system", "old system".into()),
            ChatRequestMessage::simple("user", "turn one".into()),
            ChatRequestMessage::simple("assistant", "answer".into()),
            ChatRequestMessage::simple("user", "mid".into()),
            ChatRequestMessage::simple("system", "late system".into()),
            ChatRequestMessage::simple("user", "turn two".into()),
        ];
        let result = build_responses_input(&[], &messages, Some("resp_1")).unwrap();
        let value = result.expect("expected Some value");
        let arr = value.as_array().unwrap();
        assert_eq!(arr.len(), 3);
        assert_eq!(arr[0]["role"], "user");
        assert_eq!(arr[0]["content"], "mid");
        assert_eq!(arr[1]["role"], "system");
        assert_eq!(arr[1]["content"], "late system");
        assert_eq!(arr[2]["role"], "user");
        assert_eq!(arr[2]["content"], "turn two");
    }

    // ── Reasoning extraction tests ────────────────────────────────────

    #[test]
    fn extract_reasoning_text_plain_strings() {
        let input = vec![
            serde_json::Value::String("think".into()),
            serde_json::Value::String("more".into()),
        ];
        assert_eq!(extract_reasoning_text(&input), Some("think more".into()));
    }

    #[test]
    fn extract_reasoning_text_objects_with_text_field() {
        let input = vec![json!({"text": "thinking..."})];
        assert_eq!(extract_reasoning_text(&input), Some("thinking...".into()));
    }

    #[test]
    fn extract_reasoning_text_mixed() {
        let input = vec![json!("first"), json!({"text": "second"}), json!("third")];
        assert_eq!(
            extract_reasoning_text(&input),
            Some("first second third".into())
        );
    }

    #[test]
    fn extract_reasoning_text_empty_returns_none() {
        let input: Vec<serde_json::Value> = vec![];
        assert_eq!(extract_reasoning_text(&input), None);
    }

    // ── ResponsesTool tests ───────────────────────────────────────────

    #[test]
    fn responses_tool_from_chat_tool_definition() {
        let tool_def =
            ChatToolDefinition::function("test_func", "A test function", json!({"type": "object"}));
        let tool = ResponsesTool::from(&tool_def);
        assert_eq!(tool.kind, "function");
        assert_eq!(tool.name, "test_func");
        assert_eq!(tool.description, "A test function");
        assert_eq!(tool.parameters, json!({"type": "object"}));
        assert!(!tool.strict);
        assert!(tool.output_schema.is_none());
        assert!(tool.allowed_callers.is_none());
    }

    #[test]
    fn responses_tool_serializes_programmatic_only_as_type_only() {
        let tool = ResponsesTool {
            kind: "programmatic_tool_calling".to_string(),
            name: String::new(),
            description: String::new(),
            parameters: serde_json::Value::Null,
            strict: false,
            output_schema: None,
            allowed_callers: None,
        };
        let value = serde_json::to_value(&tool).unwrap();
        let obj = value.as_object().unwrap();
        assert_eq!(obj.len(), 1);
        assert_eq!(value["type"], "programmatic_tool_calling");
    }

    // ── AccCall tests ─────────────────────────────────────────────────

    #[test]
    fn acc_call_new_sets_index() {
        let call = AccCall::new(5);
        assert_eq!(call.index, 5);
        assert!(call.name.is_none());
        assert_eq!(call.arguments, "");
    }

    // ── Request body building tests ───────────────────────────────────

    #[test]
    fn build_simple_responses_body_sets_store_false() {
        let config = super::super::ServiceConfig {
            base_url: "https://api.openai.com/v1".into(),
            ..Default::default()
        };
        let (_url, body) = build_simple_responses_body(&config, "gpt-4", "hello", false).unwrap();
        // store: false is skipped via skip_serializing_if — the field
        // is absent rather than explicit false.
        assert!(
            body.get("store").is_none(),
            "store should be absent when false"
        );
        assert_eq!(body["model"], "gpt-4");
        assert_eq!(body["input"], "hello");
        assert!(body.get("tools").is_none());
        assert!(body.get("instructions").is_none());
    }

    #[test]
    fn build_simple_responses_body_sets_stream() {
        let config = super::super::ServiceConfig {
            base_url: "https://api.openai.com/v1".into(),
            ..Default::default()
        };
        let (_url, body) = build_simple_responses_body(&config, "gpt-4", "hello", true).unwrap();
        assert_eq!(body["stream"], true);
    }

    // ── Full response deserialisation integration ─────────────────────

    #[test]
    fn responses_response_deserializes_empty_output() {
        let result: ResponsesResponse = serde_json::from_str(r#"{"output": []}"#).unwrap();
        assert!(result.output.is_empty());
        assert!(result.id.is_none());
        assert!(result.status.is_none());
        assert!(result.usage.is_none());
    }

    #[test]
    fn responses_response_deserializes_multiple_items() {
        let json_str = r#"
            {
                "output": [
                    {
                        "type": "message",
                        "role": "assistant",
                        "content": [
                            {"type": "output_text", "text": "Let me check"}
                        ]
                    },
                    {
                        "type": "function_call",
                        "call_id": "call_1",
                        "name": "get_weather",
                        "arguments": "{\"city\":\"London\"}"
                    }
                ]
            }
        "#;
        let response: ResponsesResponse = serde_json::from_str(json_str).unwrap();
        assert_eq!(response.output.len(), 2);
    }

    // ── Reasoning artifact re-emission (phase 4a) ────────────────────────

    #[test]
    fn messages_to_responses_input_reemits_reasoning_items() {
        // An assistant message carrying the opaque ResponsesItems artifact must
        // replay those items verbatim into `input`, ahead of the message
        // content item (provider output ordering).
        let msg = ChatRequestMessage {
            role: "assistant",
            content: Some("Here is the answer".to_string()),
            tool_call_id: None,
            tool_calls: None,
            reasoning_content: None,
            reasoning: None,
            reasoning_text: None,
            reasoning_artifact: Some(ReasoningArtifact::ResponsesItems(
                br#"[{"type":"reasoning","id":"re_1","summary":[{"text":"think carefully"}]}]"#
                    .to_vec(),
            )),
        };
        let items = messages_to_responses_input(&[msg]).unwrap();
        assert_eq!(items.len(), 2);
        assert_eq!(items[0]["type"], "reasoning");
        assert_eq!(items[0]["id"], "re_1");
        assert_eq!(items[0]["summary"][0]["text"], "think carefully");
        assert_eq!(items[1]["type"], "message");
        assert_eq!(items[1]["content"], "Here is the answer");
        // `reasoning_content` is chat-completions-only — never on Responses
        // messages (neither the replayed reasoning items nor the message).
        assert!(items[0].get("reasoning_content").is_none());
        assert!(items[1].get("reasoning_content").is_none());
    }

    #[test]
    fn messages_to_responses_input_reasoning_items_preserve_order() {
        // Multiple reasoning items + tool calls: item order must be preserved.
        let msg = ChatRequestMessage {
            role: "assistant",
            content: Some("Let me check".to_string()),
            tool_call_id: None,
            tool_calls: None,
            reasoning_content: None,
            reasoning: None,
            reasoning_text: None,
            reasoning_artifact: Some(ReasoningArtifact::ResponsesItems(
                br#"[{"type":"reasoning","id":"re_1"},{"type":"reasoning","id":"re_2","encrypted_content":"eJxT_opaque"}]"#
                    .to_vec(),
            )),
        };
        let items = messages_to_responses_input(&[msg]).unwrap();
        assert_eq!(items.len(), 3);
        assert_eq!(items[0]["id"], "re_1");
        assert_eq!(items[1]["id"], "re_2");
        assert_eq!(items[1]["encrypted_content"], "eJxT_opaque");
        assert_eq!(items[2]["type"], "message");
    }

    #[test]
    fn messages_to_responses_input_no_artifact_plain_message() {
        // Control: no artifact → plain message item, no reasoning on the wire.
        let msg = ChatRequestMessage::simple("assistant", "plain".into());
        let items = messages_to_responses_input(&[msg]).unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0]["type"], "message");
        assert!(items[0].get("reasoning_content").is_none());
        assert!(items[0].get("reasoning_artifact").is_none());
    }
}
