use std::io::{self, BufReader, Read};
use std::sync::mpsc;
use std::time::Duration;

use crate::openai::{ChatRequestMessage, ChatToolDefinition, ChatTurnResult, CompletionChunkKind};
use crate::retry;

use super::{
    GenerateContentRequest, GenerateContentResponse, GoogleConfig, GoogleError,
    build_message_payloads, build_tool_payloads, model_url, response_to_turn_result,
};

/// Endpoint action for non-streaming content generation.
const GENERATE_CONTENT: &str = "generateContent";

/// Endpoint action for streaming content generation with SSE.
const STREAM_GENERATE_CONTENT: &str = "streamGenerateContent?alt=sse";

/// Send a POST /v1beta/models/{model}:generateContent request with retry.
#[allow(clippy::too_many_arguments)]
pub(super) fn generate_content_request(
    client: &reqwest::blocking::Client,
    config: &GoogleConfig,
    api_key: &str,
    model: &str,
    messages: &[ChatRequestMessage],
    tools: &[ChatToolDefinition],
    on_retry: &mut Option<retry::RetryCallback>,
    cancel_rx: Option<&mpsc::Receiver<()>>,
) -> Result<ChatTurnResult, GoogleError> {
    let url = model_url(&config.base_url, model, GENERATE_CONTENT)?;
    let retry_cfg = retry::RetryConfig {
        max_attempts: config.retry_max_attempts,
        initial_backoff_ms: config.retry_initial_backoff_ms,
        max_backoff_ms: config.retry_max_backoff_ms,
    };

    let (payloads, system_instruction) = build_message_payloads(messages);
    let tool_payloads = if tools.is_empty() {
        None
    } else {
        Some(build_tool_payloads(tools))
    };

    let system_value = system_instruction.map(|s| serde_json::json!({"parts": [{"text": s}]}));

    let body = serde_json::to_value(&GenerateContentRequest {
        contents: payloads,
        system_instruction: system_value,
        tools: tool_payloads,
    })
    .map_err(io::Error::other)?;

    let response = retry::retry_loop(
        || {
            client
                .post(&url)
                .header("x-goog-api-key", api_key.trim())
                .json(&body)
                .send()
        },
        &retry_cfg,
        on_retry,
        cancel_rx,
    )
    .map_err(GoogleError::from)?;

    let status = response.status();
    if !status.is_success() {
        let body_text = response.text().unwrap_or_default();
        let detail = extract_error_detail(&body_text);
        return Err(status_to_google_error(status.as_u16(), &detail));
    }

    let payload: GenerateContentResponse = response
        .json()
        .map_err(|e| GoogleError::Io(io::Error::other(e)))?;

    response_to_turn_result(payload)
}

/// Streaming POST /v1beta/models/{model}:streamGenerateContent?alt=sse via SSE with retry.
#[allow(clippy::too_many_arguments)]
pub(super) fn generate_content_request_streaming<F>(
    client: &reqwest::blocking::Client,
    config: &GoogleConfig,
    api_key: &str,
    model: &str,
    messages: &[ChatRequestMessage],
    tools: &[ChatToolDefinition],
    on_retry: &mut Option<retry::RetryCallback>,
    cancel_rx: Option<&mpsc::Receiver<()>>,
    mut on_chunk: F,
) -> Result<ChatTurnResult, GoogleError>
where
    F: FnMut(CompletionChunkKind, String) -> io::Result<()>,
{
    let url = model_url(&config.base_url, model, STREAM_GENERATE_CONTENT)?;
    let retry_cfg = retry::RetryConfig {
        max_attempts: config.retry_max_attempts,
        initial_backoff_ms: config.retry_initial_backoff_ms,
        max_backoff_ms: config.retry_max_backoff_ms,
    };

    let (payloads, system_instruction) = build_message_payloads(messages);
    let tool_payloads = if tools.is_empty() {
        None
    } else {
        Some(build_tool_payloads(tools))
    };

    let system_value = system_instruction.map(|s| serde_json::json!({"parts": [{"text": s}]}));

    let body = serde_json::to_value(&GenerateContentRequest {
        contents: payloads,
        system_instruction: system_value,
        tools: tool_payloads,
    })
    .map_err(io::Error::other)?;

    let response = retry::retry_loop(
        || {
            client
                .post(&url)
                .header("x-goog-api-key", api_key.trim())
                .json(&body)
                .send()
        },
        &retry_cfg,
        on_retry,
        cancel_rx,
    )
    .map_err(GoogleError::from)?;

    let status = response.status();
    if !status.is_success() {
        let body_text = response.text().unwrap_or_default();
        let detail = extract_error_detail(&body_text);
        return Err(status_to_google_error(status.as_u16(), &detail));
    }

    let mut reader = GeminiSseReader::from_reader(response);
    let mut saw_text = false;
    let mut full_text = String::new();
    let mut pending_tool_calls: Vec<super::ChatToolCall> = Vec::new();

    while let Some(data) = reader.next_event()? {
        let payload: GenerateContentResponse =
            serde_json::from_str(&data).map_err(|e| GoogleError::Io(io::Error::other(e)))?;

        // Process the first candidate's parts
        let Some(candidate) = payload.candidates.into_iter().next() else {
            continue;
        };
        let Some(content) = candidate.content else {
            continue;
        };

        for part in content.parts {
            match part {
                super::ResponsePart::Text { text } => {
                    if !text.is_empty() {
                        saw_text = true;
                        full_text.push_str(&text);
                        on_chunk(CompletionChunkKind::Answer, text)?;
                    }
                }
                super::ResponsePart::FunctionCall { function_call } => {
                    saw_text = true;
                    let id = format!("fc_{}", function_call.name);
                    let args_json = function_call.args.to_string();
                    pending_tool_calls.push(super::ChatToolCall {
                        id,
                        name: function_call.name,
                        arguments_json: args_json,
                    });
                }
            }
        }
    }

    if !saw_text {
        return Err(GoogleError::EmptyResponse);
    }

    if !pending_tool_calls.is_empty() {
        return Ok(super::ChatTurnResult::ToolUse(
            super::ChatAssistantToolUse {
                content: if full_text.is_empty() {
                    None
                } else {
                    Some(full_text)
                },
                tool_calls: pending_tool_calls,
                reasoning: None,
            },
        ));
    }

    if full_text.is_empty() {
        return Err(GoogleError::EmptyResponse);
    }

    Ok(super::ChatTurnResult::FinalText(super::FinalTextResult {
        content: full_text,
        reasoning: None,
    }))
}

/// Map an HTTP status code and detail string to a GoogleError.
fn status_to_google_error(status: u16, detail: &str) -> GoogleError {
    match status {
        400 | 401 | 403 => GoogleError::Unauthorized {
            status,
            detail: detail.to_string(),
        },
        429 => GoogleError::RateLimited {
            retry_after_secs: None,
            detail: detail.to_string(),
        },
        500..=599 => GoogleError::ServerError {
            status,
            detail: detail.to_string(),
        },
        _ => GoogleError::ClientError {
            status,
            detail: detail.to_string(),
        },
    }
}

/// Extract a human-readable detail string from a Gemini error response body.
pub(crate) fn extract_error_detail(body: &str) -> String {
    if let Ok(err_body) = serde_json::from_str::<super::GeminiErrorBody>(body)
        && let Some(err) = err_body.error
        && !err.message.is_empty()
    {
        return format!("{}: {}", err.status, err.message);
    }
    body.to_string()
}

// ── Gemini SSE reader ────────────────────────────────────────────────

/// SSE reader for the Gemini streaming API.
///
/// Gemini SSE format is simple:
/// - Lines start with `data:` and contain JSON payloads
/// - Empty lines separate events
/// - `data: [DONE]` terminates the stream
///
/// A custom reader is needed here (rather than reusing the Anthropic SSE reader)
/// because Gemini sends each JSON chunk on its own `data:` line without the event
/// type and `id` fields that the Anthropic protocol includes.
pub(crate) struct GeminiSseReader {
    reader: BufReader<Box<dyn Read + Send>>,
    pending: Vec<u8>,
    lines: Vec<String>,
    finished: bool,
}

impl GeminiSseReader {
    pub(super) fn from_reader(read: impl Read + Send + 'static) -> Self {
        Self {
            reader: BufReader::new(Box::new(read)),
            pending: Vec::new(),
            lines: Vec::new(),
            finished: false,
        }
    }

    /// Yield the next complete SSE event data.
    /// Returns `None` when the stream ends (including on `[DONE]`).
    pub(super) fn next_event(&mut self) -> io::Result<Option<String>> {
        loop {
            if self.finished {
                return Ok(None);
            }

            if let Some(event) = self.drain_event()? {
                return Ok(Some(event));
            }

            let mut buf = [0u8; 4096];
            let n = match self.reader.read(&mut buf) {
                Ok(0) => {
                    self.finished = true;
                    return Ok(self.flush_lines());
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

    /// Consume complete lines from `pending` and return an event when
    /// terminated by a blank line.
    fn drain_event(&mut self) -> io::Result<Option<String>> {
        while let Some(line_end) = self.pending.iter().position(|&b| b == b'\n') {
            let mut line: Vec<u8> = self.pending.drain(..=line_end).collect();
            // Strip trailing newline/carriage-return.
            while matches!(line.last(), Some(b'\n') | Some(b'\r')) {
                line.pop();
            }

            if line.is_empty() {
                // Blank line delimits events — join collected lines.
                if !self.lines.is_empty() {
                    let event = self.lines.join("\n");
                    self.lines.clear();
                    return Ok(Some(event));
                }
                continue;
            }

            let line_str = std::str::from_utf8(&line)
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;

            if let Some(value) = line_str.strip_prefix("data:") {
                let trimmed = value.trim_start();
                if trimmed == "[DONE]" {
                    self.finished = true;
                    // Emit any collected lines before DONE
                    if !self.lines.is_empty() {
                        let event = self.lines.join("\n");
                        self.lines.clear();
                        return Ok(Some(event));
                    }
                    return Ok(None);
                }
                self.lines.push(trimmed.to_string());
            }
            // Non-data lines are ignored.
        }
        Ok(None)
    }

    /// Flush any remaining lines as a final event.
    fn flush_lines(&mut self) -> Option<String> {
        if self.lines.is_empty() {
            return None;
        }
        let event = self.lines.join("\n");
        self.lines.clear();
        Some(event)
    }
}
