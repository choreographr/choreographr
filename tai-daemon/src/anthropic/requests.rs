use std::io::{self, BufReader, Read};
use std::sync::mpsc;
use std::time::Duration;

use serde::Deserialize;

use crate::openai::{
    ChatAssistantToolUse, ChatRequestMessage, ChatToolCall, ChatToolDefinition, ChatTurnResult,
    CompletionChunkKind, FinalTextResult,
};
use crate::retry;

use super::{
    AnthropicConfig, AnthropicError, MessagesRequest, MessagesResponse, build_message_payloads,
    build_tool_payloads, response_to_turn_result,
};

/// Endpoint path for the Messages API.
const MESSAGES_PATH: &str = "/v1/messages";

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
    client: &reqwest::blocking::Client,
    config: &AnthropicConfig,
    api_key: &str,
    model: &str,
    messages: &[ChatRequestMessage],
    tools: &[ChatToolDefinition],
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

    let body = serde_json::to_value(&MessagesRequest {
        model,
        max_tokens: config.max_tokens,
        system: system.as_deref(),
        messages: payloads,
        tools: tool_payloads,
        stream,
    })
    .map_err(io::Error::other)?;

    let response = retry::retry_loop(
        || {
            client
                .post(&url)
                .header("x-api-key", api_key.trim())
                .header("anthropic-version", &config.api_version)
                .json(&body)
                .send()
        },
        &retry_cfg,
        on_retry,
        cancel_rx,
    )
    .map_err(AnthropicError::from)?;

    let payload: MessagesResponse = response
        .json()
        .map_err(|e| AnthropicError::Io(io::Error::other(e)))?;

    response_to_turn_result(payload)
}

/// Streaming POST /v1/messages request via SSE with retry.
#[allow(clippy::too_many_arguments)]
pub(super) fn messages_request_streaming<F>(
    client: &reqwest::blocking::Client,
    config: &AnthropicConfig,
    api_key: &str,
    model: &str,
    messages: &[ChatRequestMessage],
    tools: &[ChatToolDefinition],
    on_retry: &mut Option<retry::RetryCallback>,
    cancel_rx: Option<&mpsc::Receiver<()>>,
    mut on_chunk: F,
) -> Result<ChatTurnResult, AnthropicError>
where
    F: FnMut(CompletionChunkKind, String) -> io::Result<()>,
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

    let body = serde_json::to_value(&MessagesRequest {
        model,
        max_tokens: config.max_tokens,
        system: system.as_deref(),
        messages: payloads,
        tools: tool_payloads,
        stream: true,
    })
    .map_err(io::Error::other)?;

    let response = retry::retry_loop(
        || {
            client
                .post(&url)
                .header("x-api-key", api_key.trim())
                .header("anthropic-version", &config.api_version)
                .json(&body)
                .send()
        },
        &retry_cfg,
        on_retry,
        cancel_rx,
    )
    .map_err(AnthropicError::from)?;

    // Parse the SSE stream using an Anthropic-specific reader.
    let mut reader = AnthropicSseReader::from_reader(response);
    let mut saw_text = false;
    let mut full_text = String::new();
    let mut full_reasoning = String::new();
    // Accumulates tool call fields across content_block_delta chunks keyed
    // by the content block index.
    let mut pending_tool_calls: Vec<StreamToolCall> = Vec::new();

    while let Some((event_type, data)) = reader.next_event()? {
        match event_type.as_str() {
            "content_block_start" => {
                let start: ContentBlockStart = serde_json::from_str(&data)
                    .map_err(|e| AnthropicError::Io(io::Error::other(e)))?;
                match start.content_block {
                    StreamContentBlock::Text { text } => {
                        if !text.is_empty() {
                            saw_text = true;
                            full_text.push_str(&text);
                            on_chunk(CompletionChunkKind::Answer, text)?;
                        }
                    }
                    StreamContentBlock::ToolUse { id, name, input } => {
                        saw_text = true;
                        // Ensure the vector is large enough.
                        let idx = start.index as usize;
                        while pending_tool_calls.len() <= idx {
                            pending_tool_calls.push(StreamToolCall {
                                id: String::new(),
                                name: String::new(),
                                arguments: String::new(),
                            });
                        }
                        pending_tool_calls[idx].id = id;
                        pending_tool_calls[idx].name = name;
                        pending_tool_calls[idx].arguments = input.to_string();
                    }
                    StreamContentBlock::Thinking { thinking } => {
                        if !thinking.is_empty() {
                            saw_text = true;
                            full_reasoning.push_str(&thinking);
                            on_chunk(CompletionChunkKind::Reasoning, thinking)?;
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
                            saw_text = true;
                            full_text.push_str(&text);
                            on_chunk(CompletionChunkKind::Answer, text)?;
                        }
                    }
                    StreamDelta::InputJsonDelta { partial_json } => {
                        saw_text = true;
                        let idx = delta.index as usize;
                        while pending_tool_calls.len() <= idx {
                            pending_tool_calls.push(StreamToolCall::default());
                        }
                        pending_tool_calls[idx].arguments.push_str(&partial_json);
                    }
                    StreamDelta::ThinkingDelta { thinking } => {
                        if !thinking.is_empty() {
                            saw_text = true;
                            full_reasoning.push_str(&thinking);
                            on_chunk(CompletionChunkKind::Reasoning, thinking)?;
                        }
                    }
                }
            }
            "message_stop" => {
                break;
            }
            // message_start, content_block_stop, message_delta, ping — skip
            _ => {}
        }
    }

    if !saw_text {
        return Err(AnthropicError::EmptyResponse);
    }

    // If we collected tool calls, return ToolUse.
    let tool_calls: Vec<ChatToolCall> = pending_tool_calls
        .into_iter()
        .map(|tc| ChatToolCall {
            id: tc.id,
            name: tc.name,
            arguments_json: tc.arguments,
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
