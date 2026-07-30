use serde::{Deserialize, Serialize};
use tracing::{debug, info, trace, warn};

use super::retry;
use super::{
    ChatRequestMessage, ChatToolDefinition, ResponsesStreamEvent, SseReader, endpoint_url,
    messages_to_responses_input, parse_responses_stream_event,
    validate_tool_call_arguments,
};
use crate::providers::StreamEvent;
use crate::providers::shared::MAX_TOOL_CALLS;
use crate::providers::types::{
    CallerInfo, ChatAssistantToolUse, ChatToolCall, ChatTurnResult, FinalTextResult,
};
use crate::providers::ToolResultItem;
use choreo_proto::TokenUsage;
use std::collections::HashMap;
use std::io;
use std::sync::mpsc;

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
#[derive(Debug, Deserialize)]
#[allow(dead_code)]
pub(crate) struct ResponsesResponse {
    #[serde(default)]
    pub(crate) id: Option<String>,
    #[serde(default)]
    pub(crate) status: Option<String>,
    #[serde(default)]
    pub(crate) output: Vec<ResponseOutputItem>,
    #[serde(default)]
    pub(crate) usage: Option<super::Usage>,
}

/// Items in a Responses API response output array.
#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
#[allow(dead_code)]
pub(crate) enum ResponseOutputItem {
    Message {
        #[serde(default)]
        content: Vec<ResponseContentPart>,
        #[serde(default)]
        role: Option<String>,
    },
    Reasoning {
        #[serde(default)]
        summary: Vec<serde_json::Value>,
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
) -> Result<Option<serde_json::Value>, super::OpenAiError> {
    if tool_results.is_empty() {
        // First call in a turn: convert messages to Responses input items.
        // System messages go into `input` as `{role: "system"}` items.
        let items = messages_to_responses_input(messages);
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
#[allow(clippy::too_many_arguments)]
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
    let input_value = build_responses_input(tool_results, messages)?;

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
    cancel_rx: Option<&mpsc::Receiver<()>>,
) -> Result<String, super::OpenAiError> {
    let (url, body) = build_simple_responses_body(config, model, prompt, false)?;
    let retry = retry::retry_config_from_config(config);
    let response = retry::retry_send(agent, &url, api_key, &body, &retry, &mut None, cancel_rx)?;
    let payload: ResponsesResponse = response
        .into_body()
        .read_json()
        .map_err(|e| super::OpenAiError::Io(io::Error::other(e)))?;

    let content = payload
        .output
        .into_iter()
        .filter_map(|item| {
            if let ResponseOutputItem::Message { content, .. } = item {
                Some(content)
            } else {
                None
            }
        })
        .flatten()
        .filter_map(|part| part.text)
        .map(|text| text.trim().to_string())
        .find(|text| !text.is_empty())
        .unwrap_or_default();

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
    cancel_rx: Option<&mpsc::Receiver<()>>,
    on_event: &mut F,
) -> Result<(), super::OpenAiError>
where
    F: FnMut(StreamEvent) -> io::Result<()>,
{
    let (url, body) = build_simple_responses_body(config, model, prompt, true)?;
    let retry = retry::retry_config_from_config(config);
    let response = retry::retry_send(agent, &url, api_key, &body, &retry, &mut None, cancel_rx)?;
    let mut reader = SseReader::from_reader(response.into_body().into_reader());
    let mut has_any_output = false;
    loop {
        retry::check_cancelled(cancel_rx)?;

        let Some(data) = reader.next_event()? else {
            break;
        };
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
#[allow(clippy::too_many_arguments)]
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
    cancel_rx: Option<&mpsc::Receiver<()>>,
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
    let response = retry::retry_send(agent, &url, api_key, &body, &retry, on_retry, cancel_rx)?;
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

    let response_id = payload.id.clone();
    let turn_usage = payload.usage.map(|u| TokenUsage {
        input_tokens: u.prompt_tokens,
        output_tokens: u.completion_tokens,
        total_tokens: u.total_tokens,
    });

    // Parse output items: extract text, reasoning, and tool calls.
    let mut full_text = String::new();
    let mut full_reasoning = String::new();
    let mut tool_calls: Vec<ChatToolCall> = Vec::new();

    for item in payload.output {
        match item {
            ResponseOutputItem::Message { content, .. } => {
                for part in content {
                    if part.kind == "output_text" {
                        if let Some(t) = &part.text {
                            full_text.push_str(t);
                        }
                    }
                }
            }
            ResponseOutputItem::Reasoning { summary } => {
                if let Some(text) = extract_reasoning_text(&summary) {
                    full_reasoning.push_str(&text);
                }
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
#[allow(clippy::too_many_arguments)]
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
    cancel_rx: Option<&mpsc::Receiver<()>>,
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
    let response = retry::retry_send(agent, &url, api_key, &body, &retry, on_retry, cancel_rx)?;

    let mut has_any_output = false;
    let mut full_content = String::new();
    let mut full_reasoning = String::new();
    let mut last_usage: Option<TokenUsage> = None;
    let mut response_id: Option<String> = None;

    let mut acc_calls: HashMap<String, AccCall> = HashMap::new();
    let mut next_tool_index: u32 = 0;

    let mut reader = SseReader::from_reader(response.into_body().into_reader());
    loop {
        retry::check_cancelled(cancel_rx)?;

        let Some(data) = reader.next_event()? else {
            break;
        };
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
    }))
}
