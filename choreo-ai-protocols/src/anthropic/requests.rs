use std::io::{self, BufReader, Read};
use std::time::Duration;

use choreo_proto::TokenUsage;
use serde::Deserialize;
use tracing::{debug, trace};

use crate::openai::{ChatRequestMessage, ChatToolDefinition};
use crate::retry;
use crate::shared::MAX_TOOL_CALLS;
use crate::types::{
    ChatAssistantToolUse, ChatToolCall, ChatTurnResult, FinalTextResult, StreamEvent,
};

use super::{
    AnthropicConfig, AnthropicError, MessagesRequest, MessagesResponse, ModelListResponse,
    ThinkingArtifactBlock, anthropic_thinking_artifact, build_message_payloads,
    build_tool_payloads, response_to_turn_result, thinking_payload,
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

    // `api_key` borrows from the client's `Zeroizing<String>` (see
    // `AnthropicClient::api_key`), so the key already lives in wipe-on-drop
    // storage and `.trim()` makes no extra heap copy here — unlike OpenAI's
    // `Bearer …` prefix string, which is a derived value and IS wrapped in
    // `Zeroizing` (see `openai/retry.rs`). The only transient copy is inside
    // ureq's request object, which dies with the request. The same applies at
    // the other `x-api-key` header sites in this file.
    // Hoist the no-op retry callback into a named local: a bare `&mut None`
    // temporary would be dropped before the retry call below (E0716).
    let mut no_retry = None;
    let mut ctx = retry::AttemptContext::new(&mut no_retry, None, None);
    let response = retry::retry_loop(
        || {
            agent
                .get(&url)
                .header("x-api-key", api_key.trim())
                .header("anthropic-version", &config.api_version)
                .call()
        },
        &retry_cfg,
        &mut ctx,
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
#[expect(clippy::too_many_arguments)]
pub(super) fn messages_request(
    agent: &ureq::Agent,
    config: &AnthropicConfig,
    api_key: &str,
    model: &str,
    messages: &[ChatRequestMessage],
    tools: &[ChatToolDefinition],
    thinking_effort: &str,
    stream: bool,
    on_retry: &mut Option<retry::RetryCallback>,
    cancel_rx: Option<&crossbeam_channel::Receiver<()>>,
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

    let mut ctx = retry::AttemptContext::new(on_retry, cancel_rx, None);
    let response = retry::retry_loop(
        || {
            agent
                .post(&url)
                .header("x-api-key", api_key.trim())
                .header("anthropic-version", &config.api_version)
                .send_json(body.clone())
        },
        &retry_cfg,
        &mut ctx,
    )
    .map_err(AnthropicError::from)?;

    let payload: MessagesResponse = response
        .into_body()
        .read_json()
        .map_err(|e| AnthropicError::Io(io::Error::other(e)))?;

    response_to_turn_result(payload)
}

/// Streaming POST /v1/messages request via SSE with retry.
#[expect(clippy::too_many_arguments)]
pub(super) fn messages_request_streaming<F>(
    agent: &ureq::Agent,
    config: &AnthropicConfig,
    api_key: &str,
    model: &str,
    messages: &[ChatRequestMessage],
    tools: &[ChatToolDefinition],
    thinking_effort: &str,
    on_retry: &mut Option<retry::RetryCallback>,
    cancel_rx: Option<&crossbeam_channel::Receiver<()>>,
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

    // Per-attempt wall-clock deadline spanning the whole request (see `retry::AttemptDeadline`).
    let mut deadline = retry::AttemptDeadline::new(config.total_timeout_secs);
    let mut ctx = retry::AttemptContext::new(on_retry, cancel_rx, Some(&mut deadline));
    let response = retry::retry_loop(
        || {
            agent
                .post(&url)
                .header("x-api-key", api_key.trim())
                .header("anthropic-version", &config.api_version)
                .send_json(body.clone())
        },
        &retry_cfg,
        &mut ctx,
    )
    .map_err(AnthropicError::from)?;

    // Parse the SSE stream using an Anthropic-specific reader, moved onto a
    // dedicated thread so a stalled/trickling stream cannot block cancellation
    // (see `crate::stream`); the abort flag on `sse` stops the thread at its
    // next loop boundary once the consumer cancels or drops it.
    let mut reader = AnthropicSseReader::from_reader(response.into_body().into_reader());
    let sse = crate::stream::spawn_sse_reader(move || reader.next_event(), deadline.current());
    let mut has_any_output = false;
    let mut full_text = String::new();
    let mut full_reasoning = String::new();
    // Accumulates tool call fields across content_block_delta chunks keyed
    // by the content block index.
    let mut pending_tool_calls: Vec<StreamToolCall> = Vec::new();
    // Track input/output tokens delivered via message_start and message_delta.
    let mut input_tokens: Option<u32> = None;
    let mut output_tokens: Option<u32> = None;
    // Reconstructs the thinking / redacted_thinking blocks in wire order for
    // the opaque round-trip artifact (same shape as the non-streaming path).
    let mut thinking_blocks = ThinkingBlockAccumulator::new();

    while let Some((event_type, data)) = crate::stream::recv_sse_event(&sse, cancel_rx)? {
        match event_type.as_str() {
            "content_block_start" => {
                let start: ContentBlockStart = serde_json::from_str(&data)
                    .map_err(|e| AnthropicError::Io(io::Error::other(e)))?;
                // Retain thinking / redacted_thinking blocks for the artifact
                // before the match below consumes the content block.
                thinking_blocks.on_content_block_start(&start.content_block);
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
                        trace!(
                            index = start.index,
                            tool_name = %name,
                            input_len = input_str.len(),
                            "anthropic: tool call content block start",
                        );
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
                // Accumulate thinking text / signature fragments for the
                // artifact before the match below consumes the delta.
                thinking_blocks.on_content_block_delta(&delta.delta);
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
                        trace!(
                            index = delta.index,
                            partial_len = partial_json.len(),
                            "anthropic: tool call arg delta",
                        );
                    }
                    StreamDelta::ThinkingDelta { thinking } => {
                        if !thinking.is_empty() {
                            has_any_output = true;
                            full_reasoning.push_str(&thinking);
                            on_event(StreamEvent::Reasoning(thinking))?;
                        }
                    }
                    StreamDelta::SignatureDelta { .. } => {
                        // No display output; the signature is captured into the
                        // artifact by the accumulator above.
                    }
                }
            }
            "content_block_stop" => {
                // Finalize the open thinking block (if any) into the artifact.
                thinking_blocks.on_content_block_stop();
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

    // Assemble the round-trip artifact from the blocks collected during the
    // stream (a thinking block left open at stream end is flushed by `finish`).
    let reasoning_artifact = anthropic_thinking_artifact(&thinking_blocks.finish())?;

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
            reasoning_artifact,
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
        reasoning_artifact,
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
#[expect(clippy::enum_variant_names)]
enum StreamDelta {
    #[serde(rename = "text_delta")]
    TextDelta { text: String },
    #[serde(rename = "input_json_delta")]
    InputJsonDelta { partial_json: String },
    #[serde(rename = "thinking_delta")]
    ThinkingDelta { thinking: String },
    #[serde(rename = "signature_delta")]
    SignatureDelta { signature: String },
}

/// Accumulates thinking / redacted_thinking blocks in wire order during
/// streaming, mirroring the non-streaming artifact assembly in `mod.rs`.
///
/// Thinking text arrives piecemeal across a `content_block_start` (initial
/// text) plus `thinking_delta` fragments, while the encrypted signature
/// arrives as one or more `signature_delta` fragments before the block's
/// `content_block_stop`. Redacted blocks carry all their data in the start
/// event. Text / tool_use blocks never appear in the artifact and are
/// ignored.
struct ThinkingBlockAccumulator {
    /// Completed blocks in original order.
    blocks: Vec<ThinkingArtifactBlock>,
    /// The thinking block currently open (between start and stop), being
    /// filled by thinking_delta / signature_delta fragments.
    open: Option<StreamThinkingBlock>,
}

/// A thinking block being filled by streaming deltas.
#[derive(Default)]
struct StreamThinkingBlock {
    thinking: String,
    signature: String,
}

impl ThinkingBlockAccumulator {
    fn new() -> Self {
        Self {
            blocks: Vec::new(),
            open: None,
        }
    }

    /// Handle a `content_block_start` event for the artifact.
    fn on_content_block_start(&mut self, block: &StreamContentBlock) {
        match block {
            StreamContentBlock::Thinking { thinking } => {
                self.open = Some(StreamThinkingBlock {
                    thinking: thinking.clone(),
                    signature: String::new(),
                });
            }
            StreamContentBlock::RedactedThinking { data } => {
                self.blocks
                    .push(ThinkingArtifactBlock::RedactedThinking { data: data.clone() });
            }
            _ => {}
        }
    }

    /// Handle a `content_block_delta` event for the artifact.
    fn on_content_block_delta(&mut self, delta: &StreamDelta) {
        match delta {
            StreamDelta::ThinkingDelta { thinking } => {
                if let Some(open) = self.open.as_mut() {
                    open.thinking.push_str(thinking);
                }
            }
            StreamDelta::SignatureDelta { signature } => {
                if let Some(open) = self.open.as_mut() {
                    open.signature.push_str(signature);
                }
            }
            _ => {}
        }
    }

    /// Handle a `content_block_stop` event: finalize the open thinking block.
    fn on_content_block_stop(&mut self) {
        if let Some(open) = self.open.take() {
            self.blocks.push(ThinkingArtifactBlock::Thinking {
                thinking: open.thinking,
                signature: open.signature,
            });
        }
    }

    /// Consume the accumulator, flushing any thinking block left open at
    /// stream end (its `content_block_stop` never arrived).
    fn finish(mut self) -> Vec<ThinkingArtifactBlock> {
        if let Some(open) = self.open.take() {
            self.blocks.push(ThinkingArtifactBlock::Thinking {
                thinking: open.thinking,
                signature: open.signature,
            });
        }
        self.blocks
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use choreo_proto::ReasoningArtifact;
    use serde_json::json;

    /// Feed the accumulator the SSE-shaped events for a signed thinking block
    /// (text split across start + delta, signature split across two deltas)
    /// followed by a redacted_thinking block, and assert the assembled artifact
    /// is byte-exact: block order preserved, signature and redacted data
    /// intact. Object keys serialize alphabetically (serde_json default
    /// BTreeMap ordering without `preserve_order`).
    #[test]
    fn signature_delta_accumulates_into_thinking_artifact() {
        let mut acc = ThinkingBlockAccumulator::new();

        // content_block_start: thinking block, initial text.
        let start: ContentBlockStart = serde_json::from_value(json!({
            "index": 0,
            "content_block": {"type": "thinking", "thinking": "Let me"}
        }))
        .unwrap();
        acc.on_content_block_start(&start.content_block);

        // thinking_delta: more thinking text.
        let delta: ContentBlockDelta = serde_json::from_value(json!({
            "index": 0,
            "delta": {"type": "thinking_delta", "thinking": " analyze."}
        }))
        .unwrap();
        acc.on_content_block_delta(&delta.delta);

        // signature_delta x2: the encrypted signature streams in fragments.
        for part in ["sig_abc", "123"] {
            let delta: ContentBlockDelta = serde_json::from_value(json!({
                "index": 0,
                "delta": {"type": "signature_delta", "signature": part}
            }))
            .unwrap();
            acc.on_content_block_delta(&delta.delta);
        }
        acc.on_content_block_stop();

        // redacted_thinking: all data arrives in the start block.
        let start: ContentBlockStart = serde_json::from_value(json!({
            "index": 1,
            "content_block": {"type": "redacted_thinking", "data": "eJxT_opaque"}
        }))
        .unwrap();
        acc.on_content_block_start(&start.content_block);
        acc.on_content_block_stop();

        let artifact = anthropic_thinking_artifact(&acc.finish()).unwrap().unwrap();
        let expected = br#"[{"signature":"sig_abc123","thinking":"Let me analyze.","type":"thinking"},{"data":"eJxT_opaque","type":"redacted_thinking"}]"#;
        assert_eq!(
            artifact,
            ReasoningArtifact::AnthropicThinking(expected.to_vec())
        );
    }

    /// A thinking block left open at stream end (no `content_block_stop`) must
    /// still be flushed into the artifact, including any signature fragments
    /// already seen.
    #[test]
    fn streaming_thinking_block_left_open_is_flushed() {
        let mut acc = ThinkingBlockAccumulator::new();
        let start: ContentBlockStart = serde_json::from_value(json!({
            "index": 0,
            "content_block": {"type": "thinking", "thinking": ""}
        }))
        .unwrap();
        acc.on_content_block_start(&start.content_block);
        let delta: ContentBlockDelta = serde_json::from_value(json!({
            "index": 0,
            "delta": {"type": "signature_delta", "signature": "sig_open"}
        }))
        .unwrap();
        acc.on_content_block_delta(&delta.delta);

        let artifact = anthropic_thinking_artifact(&acc.finish()).unwrap().unwrap();
        let expected = br#"[{"signature":"sig_open","thinking":"","type":"thinking"}]"#;
        assert_eq!(
            artifact,
            ReasoningArtifact::AnthropicThinking(expected.to_vec())
        );
    }

    /// A stream with no thinking blocks captures no artifact.
    #[test]
    fn streaming_without_thinking_has_no_artifact() {
        let acc = ThinkingBlockAccumulator::new();
        assert!(
            anthropic_thinking_artifact(&acc.finish())
                .unwrap()
                .is_none()
        );
    }

    /// The SSE handler previously *errored* on `signature_delta` events: the
    /// variant was missing, so deserialization failed and killed the whole
    /// stream. Pin that it now parses and carries the signature.
    #[test]
    fn signature_delta_deserializes() {
        let delta: ContentBlockDelta = serde_json::from_value(json!({
            "index": 0,
            "delta": {"type": "signature_delta", "signature": "sig_1"}
        }))
        .unwrap();
        match delta.delta {
            StreamDelta::SignatureDelta { signature } => assert_eq!(signature, "sig_1"),
            other => panic!("expected SignatureDelta, got {other:?}"),
        }
    }
}
