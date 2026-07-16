use std::io::{self, BufReader, Read};
use std::sync::mpsc;
use std::time::Duration;

use serde::Deserialize;
use tai_proto::{ThinkingEffort, TokenUsage};
use tracing::{debug, trace};

use crate::openai::{ChatRequestMessage, ChatToolDefinition};
use crate::providers::StreamEvent;
use crate::providers::shared::MAX_TOOL_CALLS;
use crate::providers::types::{
    ChatAssistantToolUse, ChatToolCall, ChatTurnResult, FinalTextResult,
};
use crate::retry;

use super::{
    AnthropicConfig, AnthropicError, MessagesRequest, MessagesResponse, ModelListResponse,
    build_message_payloads, build_tool_payloads, response_to_turn_result, thinking_payload,
};

/// Endpoint path for the Messages API.
const MESSAGES_PATH: &str = "/v1/messages";

/// Endpoint path for listing models.
const MODELS_PATH: &str = "/v1/models";

/// Fetch the list of available models from the Anthropic API.
pub(super) fn list_models_request(
    agent: &ureq::Agent,
    config: &AnthropicConfig,
    api_key: &str,
) -> Result<Vec<String>, AnthropicError> {
    let url = endpoint_url(&config.base_url, MODELS_PATH)?;
    let retry_cfg = retry::RetryConfig {
        max_attempts: config.retry_max_attempts,
        initial_backoff_ms: config.retry_initial_backoff_ms,
        max_backoff_ms: config.retry_max_backoff_ms,
    };

    let response = retry::retry_loop(
        || {
            agent
                .get(&url)
                .header("x-api-key", api_key.trim())
                .header("anthropic-version", &config.api_version)
                .call()
        },
        &retry_cfg,
        &mut None,
        None,
    )?;

    let payload: ModelListResponse = response
        .into_body()
        .read_json()
        .map_err(|e| AnthropicError::Io(io::Error::other(e)))?;

    let models: Vec<String> = payload.data.into_iter().map(|m| m.id).collect();
    Ok(models)
}

/// Build the full URL for a given path.
fn endpoint_url(base_url: &str, path: &str) -> io::Result<String> {
    if !path.starts_with('/') {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "path must start with '/'",
        ));
    }
    Ok(format!("{}{}", base_url.trim_end_matches('/'), path))
}

/// Send a POST /v1/messages request with retry.
#[allow(clippy::too_many_arguments)]
pub(super) fn messages_request(
    agent: &ureq::Agent,
    config: &AnthropicConfig,
    api_key: &str,
    model: &str,
    messages: &[ChatRequestMessage],
    tools: &[ChatToolDefinition],
    thinking_effort: ThinkingEffort,
    stream: bool,
    on_retry: &mut Option<retry::RetryCallback>,
    cancel_rx: Option<&mpsc::Receiver<()>>,
) -> Result<ChatTurnResult, AnthropicError> {
    let url = endpoint_url(&config.base_url, MESSAGES_PATH)?;
    let retry_cfg = retry::RetryConfig {
        max_attempts: config.retry_max_attempts,
        initial_backoff_ms: config.retry_initial_backoff_ms,
        max_backoff_ms: config.retry_max_backoff_ms,
    };

    let (payloads, system) = build_message_payloads(messages, tools);
    let tool_payloads = if tools.is_empty() {
        None
    } else {
        Some(build_tool_payloads(tools))
    };

    let thinking = thinking_payload(thinking_effort, config.max_tokens);
    if thinking.is_some() {
        debug!(
            budget_tokens = ?thinking.as_ref().map(|t| t.budget_tokens),
            "Anthropic thinking enabled"
        );
    }

    let body = serde_json::to_value(&MessagesRequest {
        model,
        max_tokens: config.max_tokens,
        system: system.as_deref(),
        messages: payloads,
        tools: tool_payloads,
        thinking,
        stream,
    })
    .map_err(io::Error::other)?;

    let response = retry::retry_loop(
        || {
            agent
                .post(&url)
                .header("x-api-key", api_key.trim())
                .header("anthropic-version", &config.api_version)
                .send_json(body.clone())
        },
        &retry_cfg,
        on_retry,
        cancel_rx,
    )
    .map_err(AnthropicError::from)?;

    let payload: MessagesResponse = response
        .into_body()
        .read_json()
        .map_err(|e| AnthropicError::Io(io::Error::other(e)))?;

    response_to_turn_result(payload)
}

/// Streaming POST /v1/messages request via SSE with retry.
#[allow(clippy::too_many_arguments)]
pub(super) fn messages_request_streaming<F>(
    agent: &ureq::Agent,
    config: &AnthropicConfig,
    api_key: &str,
    model: &str,
    messages: &[ChatRequestMessage],
    tools: &[ChatToolDefinition],
    thinking_effort: ThinkingEffort,
    on_retry: &mut Option<retry::RetryCallback>,
    cancel_rx: Option<&mpsc::Receiver<()>>,
    mut on_event: F,
) -> Result<ChatTurnResult, AnthropicError>
where
    F: FnMut(StreamEvent) -> io::Result<()>,
{
    let url = endpoint_url(&config.base_url, MESSAGES_PATH)?;
    let retry_cfg = retry::RetryConfig {
        max_attempts: config.retry_max_attempts,
        initial_backoff_ms: config.retry_initial_backoff_ms,
        max_backoff_ms: config.retry_max_backoff_ms,
    };

    let (payloads, system) = build_message_payloads(messages, tools);
    let tool_payloads = if tools.is_empty() {
        None
    } else {
        Some(build_tool_payloads(tools))
    };

    let thinking = thinking_payload(thinking_effort, config.max_tokens);
    if thinking.is_some() {
        debug!(
            budget_tokens = ?thinking.as_ref().map(|t| t.budget_tokens),
            "Anthropic thinking enabled"
        );
    }

    let body = serde_json::to_value(&MessagesRequest {
        model,
        max_tokens: config.max_tokens,
        system: system.as_deref(),
        messages: payloads,
        tools: tool_payloads,
        thinking,
        stream: true,
    })
    .map_err(io::Error::other)?;

    let response = retry::retry_loop(
        || {
            agent
                .post(&url)
                .header("x-api-key", api_key.trim())
                .header("anthropic-version", &config.api_version)
                .send_json(body.clone())
        },
        &retry_cfg,
        on_retry,
        cancel_rx,
    )
    .map_err(AnthropicError::from)?;

    // Parse the SSE stream using an Anthropic-specific reader.
    let mut reader = AnthropicSseReader::from_reader(response.into_body().into_reader());
    let mut has_any_output = false;
    let mut full_text = String::new();
    let mut full_reasoning = String::new();
    // Accumulates tool call fields across content_block_delta chunks keyed
    // by the content block index.
    let mut pending_tool_calls: Vec<StreamToolCall> = Vec::new();
    // Track input/output tokens delivered via message_start and message_delta.
    let mut input_tokens: Option<u32> = None;
    let mut output_tokens: Option<u32> = None;

    while let Some((event_type, data)) = reader.next_event()? {
        match event_type.as_str() {
            "content_block_start" => {
                let start: ContentBlockStart = serde_json::from_str(&data)
                    .map_err(|e| AnthropicError::Io(io::Error::other(e)))?;
                match start.content_block {
                    StreamContentBlock::Text { text } => {
                        if !text.is_empty() {
                            has_any_output = true;
                            full_text.push_str(&text);
                            on_event(StreamEvent::Answer(text))?;
                        }
                    }
                    StreamContentBlock::ToolUse { id, name, input } => {
                        has_any_output = true;
                        let idx = start.index as usize;
                        if idx >= MAX_TOOL_CALLS {
                            return Err(AnthropicError::Io(io::Error::other(format!(
                                "tool call index {idx} exceeds maximum ({MAX_TOOL_CALLS})"
                            ))));
                        }
                        while pending_tool_calls.len() <= idx {
                            pending_tool_calls.push(StreamToolCall {
                                id: String::new(),
                                name: String::new(),
                                arguments: String::new(),
                            });
                        }
                        let input_str = input.to_string();
                        pending_tool_calls[idx].id = id.clone();
                        pending_tool_calls[idx].name = name.clone();
                        pending_tool_calls[idx].arguments = input_str.clone();
                        // Emit initial tool call args
                        trace!(
                            index = start.index,
                            tool_name = %name,
                            input_len = input_str.len(),
                            "anthropic: tool call content block start",
                        );
                        on_event(StreamEvent::ToolCallArg {
                            index: start.index,
                            call_id: id,
                            tool_name: name,
                            delta: input_str,
                        })?;
                    }
                    StreamContentBlock::Thinking { thinking } => {
                        if !thinking.is_empty() {
                            has_any_output = true;
                            full_reasoning.push_str(&thinking);
                            on_event(StreamEvent::Reasoning(thinking))?;
                        }
                    }
                    StreamContentBlock::RedactedThinking { .. } => {}
                }
            }
            "content_block_delta" => {
                let delta: ContentBlockDelta = serde_json::from_str(&data)
                    .map_err(|e| AnthropicError::Io(io::Error::other(e)))?;
                match delta.delta {
                    StreamDelta::TextDelta { text } => {
                        if !text.is_empty() {
                            has_any_output = true;
                            full_text.push_str(&text);
                            on_event(StreamEvent::Answer(text))?;
                        }
                    }
                    StreamDelta::InputJsonDelta { partial_json } => {
                        has_any_output = true;
                        let idx = delta.index as usize;
                        if idx >= MAX_TOOL_CALLS {
                            return Err(AnthropicError::Io(io::Error::other(format!(
                                "tool call index {idx} exceeds maximum ({MAX_TOOL_CALLS})"
                            ))));
                        }
                        while pending_tool_calls.len() <= idx {
                            pending_tool_calls.push(StreamToolCall::default());
                        }
                        pending_tool_calls[idx].arguments.push_str(&partial_json);
                        // Emit tool call arg delta
                        let known_id = pending_tool_calls[idx].id.clone();
                        let known_name = pending_tool_calls[idx].name.clone();
                        trace!(
                            index = delta.index,
                            call_id = %known_id,
                            partial_len = partial_json.len(),
                            "anthropic: tool call arg delta",
                        );
                        on_event(StreamEvent::ToolCallArg {
                            index: delta.index,
                            call_id: known_id,
                            tool_name: known_name,
                            delta: partial_json,
                        })?;
                    }
                    StreamDelta::ThinkingDelta { thinking } => {
                        if !thinking.is_empty() {
                            has_any_output = true;
                            full_reasoning.push_str(&thinking);
                            on_event(StreamEvent::Reasoning(thinking))?;
                        }
                    }
                }
            }
            "message_start" => {
                // Parse input_tokens from the message_start event.
                let start: MessageStart = serde_json::from_str(&data)
                    .map_err(|e| AnthropicError::Io(io::Error::other(e)))?;
                input_tokens = start.message.usage.map(|u| u.input_tokens);
            }
            "message_delta" => {
                // Parse output_tokens from the message_delta event.
                let delta_msg: MessageDelta = serde_json::from_str(&data)
                    .map_err(|e| AnthropicError::Io(io::Error::other(e)))?;
                output_tokens = delta_msg.usage.map(|u| u.output_tokens);
            }
            "message_stop" => {
                break;
            }
            // content_block_stop, ping — skip
            _ => {}
        }
    }

    // Build usage from the tokens collected during message_start and
    // message_delta events.
    let usage: Option<TokenUsage> = match (input_tokens, output_tokens) {
        (Some(in_tok), Some(out_tok)) => {
            let total = in_tok + out_tok;
            debug!(
                input_tokens = in_tok,
                output_tokens = out_tok,
                total_tokens = total,
                "Anthropic streaming turn usage"
            );
            Some(TokenUsage {
                input_tokens: in_tok,
                output_tokens: out_tok,
                total_tokens: total,
            })
        }
        _ => None,
    };

    if !has_any_output {
        return Err(AnthropicError::EmptyResponse);
    }

    // If we collected tool calls, return ToolUse.
    let tool_calls: Vec<ChatToolCall> = pending_tool_calls
        .into_iter()
        .map(|tc| ChatToolCall {
            id: tc.id,
            name: tc.name,
            arguments_json: tc.arguments,
            caller: None,
        })
        .collect();

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
            usage,
            response_id: None,
        }));
    }

    if full_text.is_empty() {
        return Err(AnthropicError::EmptyResponse);
    }

    Ok(ChatTurnResult::FinalText(FinalTextResult {
        content: full_text,
        reasoning: if full_reasoning.is_empty() {
            None
        } else {
            Some(full_reasoning)
        },
        usage,
        response_id: None,
    }))
}

// ── SSE types for streaming ──────────────────────────────────────────

/// An open tool call being accumulated during streaming.
#[derive(Default)]
struct StreamToolCall {
    id: String,
    name: String,
    arguments: String,
}

/// SSE event for content_block_start.
#[derive(Debug, Deserialize)]
struct ContentBlockStart {
    index: u32,
    #[serde(rename = "content_block")]
    content_block: StreamContentBlock,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type")]
enum StreamContentBlock {
    #[serde(rename = "text")]
    Text { text: String },
    #[serde(rename = "tool_use")]
    ToolUse {
        id: String,
        name: String,
        input: serde_json::Value,
    },
    #[serde(rename = "thinking")]
    Thinking { thinking: String },
    #[serde(rename = "redacted_thinking")]
    #[allow(dead_code)]
    RedactedThinking { data: String },
}

/// SSE event for content_block_delta.
#[derive(Debug, Deserialize)]
struct ContentBlockDelta {
    index: u32,
    delta: StreamDelta,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type")]
#[allow(clippy::enum_variant_names)]
enum StreamDelta {
    #[serde(rename = "text_delta")]
    TextDelta { text: String },
    #[serde(rename = "input_json_delta")]
    InputJsonDelta { partial_json: String },
    #[serde(rename = "thinking_delta")]
    ThinkingDelta { thinking: String },
}

// ── Anthropic-specific SSE reader ────────────────────────────────────

/// SSE reader that yields `(event_type, data)` pairs from the Anthropic
/// streaming API, which uses both `event:` and `data:` lines.
struct AnthropicSseReader {
    reader: BufReader<Box<dyn Read + Send>>,
    pending: Vec<u8>,
    current_event: Option<String>,
    current_data: Vec<String>,
    finished: bool,
    /// Scratch buffer reused across iterations to avoid allocating per line.
    line_buf: Vec<u8>,
}

impl AnthropicSseReader {
    fn from_reader(read: impl Read + Send + 'static) -> Self {
        Self {
            reader: BufReader::new(Box::new(read)),
            pending: Vec::new(),
            current_event: None,
            current_data: Vec::new(),
            finished: false,
            line_buf: Vec::new(),
        }
    }

    /// Yield the next complete SSE event as `(event_type, data)`.
    /// Returns `None` when the stream ends.
    fn next_event(&mut self) -> io::Result<Option<(String, String)>> {
        loop {
            if self.finished {
                return Ok(None);
            }

            // Try to extract a complete event from buffered lines.
            if let Some(event) = self.drain_event()? {
                return Ok(Some(event));
            }

            // Read more bytes from the underlying reader.
            let mut buf = [0u8; 4096];
            let n = match self.reader.read(&mut buf) {
                Ok(0) => {
                    self.finished = true;
                    return Ok(self.finish_event());
                }
                Ok(n) => n,
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(10));
                    continue;
                }
                Err(e) => return Err(e),
            };
            self.pending.extend_from_slice(&buf[..n]);
        }
    }

    /// Consume complete lines from `pending` and return an event if one is
    /// fully delimited by a blank line.
    fn drain_event(&mut self) -> io::Result<Option<(String, String)>> {
        while let Some(line_end) = self.pending.iter().position(|&b| b == b'\n') {
            // Move the line bytes out of pending into the scratch buffer.
            self.line_buf.clear();
            self.line_buf.extend(self.pending.drain(..=line_end));
            // Strip trailing newline/carriage-return.
            while matches!(self.line_buf.last(), Some(b'\n') | Some(b'\r')) {
                self.line_buf.pop();
            }

            if self.line_buf.is_empty() {
                // Empty line delimits events — build one if we have data.
                if !self.current_data.is_empty() {
                    let data = self.current_data.join("\n");
                    let event = self.current_event.take().unwrap_or_default();
                    self.current_data.clear();
                    return Ok(Some((event, data)));
                }
                self.current_event = None;
                continue;
            }

            let line = std::str::from_utf8(&self.line_buf)
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;

            if let Some(value) = line.strip_prefix("event:") {
                self.current_event = Some(value.trim().to_string());
            } else if let Some(value) = line.strip_prefix("data:") {
                let trimmed = value.trim_start();
                // Handle [DONE] sentinel used by some streaming APIs.
                if trimmed == "[DONE]" {
                    self.finished = true;
                    return Ok(None);
                }
                self.current_data.push(trimmed.to_string());
            }
            // Ignore other line types (e.g. "id:", "retry:").
        }
        Ok(None)
    }

    /// Flush any remaining data as a final event.
    fn finish_event(&mut self) -> Option<(String, String)> {
        if self.current_data.is_empty() {
            return None;
        }
        let data = self.current_data.join("\n");
        let event = self.current_event.take().unwrap_or_default();
        self.current_data.clear();
        Some((event, data))
    }
}

// ── Streaming usage types ─────────────────────────────────────────────

/// SSE event for message_start — carries input_tokens.
#[derive(Debug, Deserialize)]
struct MessageStart {
    #[serde(rename = "message")]
    message: MessageStartMessage,
}

#[derive(Debug, Deserialize)]
struct MessageStartMessage {
    #[serde(default)]
    usage: Option<AnthropicStreamUsage>,
}

/// SSE event for message_delta — carries output_tokens.
#[derive(Debug, Deserialize)]
struct MessageDelta {
    #[serde(default)]
    usage: Option<AnthropicStreamUsageDelta>,
}

#[derive(Debug, Deserialize)]
struct AnthropicStreamUsage {
    input_tokens: u32,
}

#[derive(Debug, Deserialize)]
struct AnthropicStreamUsageDelta {
    output_tokens: u32,
}
