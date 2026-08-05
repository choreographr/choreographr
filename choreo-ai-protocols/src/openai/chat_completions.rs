use serde::{Deserialize, Serialize};
use tracing::debug;

use super::retry;
use super::{
    ChatRequestMessage, ChatToolDefinition, SseReader, endpoint_url, validate_tool_call_arguments,
};
use crate::shared::MAX_TOOL_CALLS;
use crate::types::{
    ChatAssistantToolUse, ChatToolCall, ChatTurnResult, FinalTextResult, StreamEvent,
};
use choreo_proto::TokenUsage;
use std::collections::HashMap;
use std::io;
use std::sync::mpsc;

// ── Chat Completions wire types ──────────────────────────────────────────

#[derive(Debug, Serialize)]
pub(crate) struct ChatCompletionsRequest<'a, M>
where
    M: Serialize,
{
    pub(crate) model: &'a str,
    #[serde(bound(serialize = "M: Serialize"))]
    pub(crate) messages: &'a [M],
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) tools: Option<&'a [ChatToolDefinition]>,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub(crate) stream: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) stream_options: Option<ChatCompletionsStreamOptions>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) max_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) max_completion_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) reasoning_effort: Option<&'a str>,
}

#[derive(Debug, Serialize)]
pub(crate) struct ChatCompletionsStreamOptions {
    pub(crate) include_usage: bool,
}

#[derive(Debug, Deserialize)]
pub(crate) struct ChatCompletionsResponse {
    pub(crate) choices: Vec<Choice>,
    pub(crate) usage: Option<super::Usage>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct Choice {
    pub(crate) message: AssistantMessage,
}

#[derive(Debug, Deserialize)]
pub(crate) struct AssistantMessage {
    pub(crate) content: Option<String>,
    #[serde(default)]
    pub(crate) tool_calls: Vec<super::AssistantToolCall>,
    pub(crate) reasoning_content: Option<String>,
    pub(crate) reasoning: Option<String>,
    pub(crate) reasoning_text: Option<String>,
}

impl AssistantMessage {
    /// Extract reasoning content from whichever field the model populated
    /// (reasoning_content, reasoning, or reasoning_text).
    pub(crate) fn take_reasoning(&mut self) -> Option<String> {
        self.reasoning_content
            .take()
            .or_else(|| self.reasoning.take())
            .or_else(|| self.reasoning_text.take())
    }
}

#[derive(Debug, Deserialize)]
pub(crate) struct ChatCompletionsStreamResponse {
    pub(crate) choices: Vec<StreamChoice>,
    #[serde(default)]
    pub(crate) usage: Option<super::Usage>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct StreamChoice {
    pub(crate) delta: Option<StreamDelta>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct StreamDelta {
    pub(crate) content: Option<String>,
    pub(crate) tool_calls: Option<Vec<StreamToolCallDelta>>,
    pub(crate) reasoning_content: Option<String>,
    pub(crate) reasoning: Option<String>,
    pub(crate) reasoning_text: Option<String>,
}

#[derive(Debug, Default, Clone, Deserialize)]
pub(crate) struct StreamToolCallDelta {
    pub(crate) index: u32,
    pub(crate) id: Option<String>,
    // Deserialised from the API's "type" field but never read in Rust — kept
    // so serde doesn't choke on unknown fields and to document the wire format.
    #[allow(dead_code)]
    #[serde(rename = "type")]
    pub(crate) kind: Option<String>,
    pub(crate) function: Option<StreamToolCallFunctionDelta>,
}

#[derive(Debug, Default, Clone, Deserialize)]
pub(crate) struct StreamToolCallFunctionDelta {
    pub(crate) name: Option<String>,
    pub(crate) arguments: Option<String>,
}

// ── Simple (no-tool) chat completions request ────────────────────────────

pub(crate) fn chat_completions_request(
    agent: &ureq::Agent,
    config: &super::ServiceConfig,
    api_key: &str,
    model: &str,
    prompt: &str,
    cancel_rx: Option<&mpsc::Receiver<()>>,
) -> Result<String, super::OpenAiError> {
    let url = endpoint_url(&config.base_url, &config.chat_completions_path)?;
    let (max_tokens_field, max_completion_tokens_field) = config.max_tokens_field_pair(model);
    let retry = retry::retry_config_from_config(config);
    let messages = [ChatRequestMessage::simple("user", prompt.to_string())];
    let body = serde_json::to_value(&ChatCompletionsRequest {
        model,
        messages: &messages,
        tools: None,
        stream: false,
        stream_options: None,
        max_tokens: max_tokens_field,
        max_completion_tokens: max_completion_tokens_field,
        reasoning_effort: None,
    })
    .map_err(io::Error::other)?;
    // Hoist the no-op retry callback into a named local: a bare `&mut None`
    // temporary would be dropped before the retry call below (E0716).
    let mut no_retry = None;
    let mut ctx = retry::AttemptContext::new(&mut no_retry, cancel_rx, None);
    let response = retry::retry_send(agent, &url, api_key, &body, &retry, &mut ctx)?;
    let payload: ChatCompletionsResponse = response
        .into_body()
        .read_json()
        .map_err(|e| super::OpenAiError::Io(io::Error::other(e)))?;

    let content = payload
        .choices
        .into_iter()
        .next()
        .and_then(|choice| choice.message.content)
        .unwrap_or_default()
        .trim()
        .to_string();

    if content.is_empty() {
        return Err(super::OpenAiError::EmptyResponse);
    }

    Ok(content)
}

// ── Non-streaming chat completions with tools ────────────────────────────

#[expect(clippy::too_many_arguments)]
pub(crate) fn chat_completions_request_with_tools(
    agent: &ureq::Agent,
    config: &super::ServiceConfig,
    api_key: &str,
    model: &str,
    messages: &[ChatRequestMessage],
    tools: &[ChatToolDefinition],
    reasoning_effort: Option<&str>,
    on_retry: &mut Option<retry::RetryCallback>,
    cancel_rx: Option<&mpsc::Receiver<()>>,
) -> Result<ChatTurnResult, super::OpenAiError> {
    let start = std::time::Instant::now();
    let url = endpoint_url(&config.base_url, &config.chat_completions_path)?;
    let (max_tokens_field, max_completion_tokens_field) = config.max_tokens_field_pair(model);
    let retry = retry::retry_config_from_config(config);
    let body = serde_json::to_value(&ChatCompletionsRequest {
        model,
        messages,
        tools: Some(tools),
        stream: false,
        stream_options: None,
        max_tokens: max_tokens_field,
        max_completion_tokens: max_completion_tokens_field,
        reasoning_effort,
    })
    .map_err(io::Error::other)?;
    let mut ctx = retry::AttemptContext::new(on_retry, cancel_rx, None);
    let response = retry::retry_send(agent, &url, api_key, &body, &retry, &mut ctx)?;
    let payload: ChatCompletionsResponse = response
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
        "chat completion turn",
    );
    let Some(mut choice) = payload.choices.into_iter().next() else {
        return Err(super::OpenAiError::EmptyResponse);
    };

    // Extract reasoning early (before partial moves into tool_calls / content)
    let reasoning = choice.message.take_reasoning();

    // Extract token usage from the API response for cost tracking / display.
    let turn_usage: Option<TokenUsage> = payload.usage.map(|u| TokenUsage {
        input_tokens: u.prompt_tokens,
        output_tokens: u.completion_tokens,
        total_tokens: u.total_tokens,
    });

    let mut tool_calls: Vec<ChatToolCall> = choice
        .message
        .tool_calls
        .into_iter()
        .map(|tool_call| ChatToolCall {
            id: tool_call.id,
            name: tool_call.function.name,
            arguments_json: tool_call.function.arguments,
            caller: None,
        })
        .collect();
    let discarded = validate_tool_call_arguments(&mut tool_calls);
    if !tool_calls.is_empty() {
        return Ok(ChatTurnResult::ToolUse(ChatAssistantToolUse {
            content: choice.message.content,
            tool_calls,
            reasoning,
            usage: turn_usage,
            response_id: None,
        }));
    }

    if !discarded.is_empty() {
        // All calls had invalid arguments. Return the text if the model
        // produced any, so the session continues gracefully and the LLM
        // can retry with valid arguments on the next turn.
        let content = choice
            .message
            .content
            .unwrap_or_default()
            .trim()
            .to_string();
        return Ok(ChatTurnResult::FinalText(FinalTextResult {
            content,
            reasoning,
            usage: turn_usage,
            response_id: None,
        }));
    }

    let content = choice
        .message
        .content
        .unwrap_or_default()
        .trim()
        .to_string();
    if content.is_empty() {
        return Err(super::OpenAiError::EmptyResponse);
    }

    Ok(ChatTurnResult::FinalText(FinalTextResult {
        content,
        reasoning,
        usage: turn_usage,
        response_id: None,
    }))
}

// ── Simple streaming chat completions ────────────────────────────────────

#[expect(clippy::too_many_arguments)]
pub(crate) fn chat_completions_request_streaming<F>(
    agent: &ureq::Agent,
    config: &super::ServiceConfig,
    api_key: &str,
    model: &str,
    prompt: &str,
    reasoning_effort: Option<&str>,
    cancel_rx: Option<&mpsc::Receiver<()>>,
    on_event: &mut F,
) -> Result<(), super::OpenAiError>
where
    F: FnMut(StreamEvent) -> io::Result<()>,
{
    let url = endpoint_url(&config.base_url, &config.chat_completions_path)?;
    let (max_tokens_field, max_completion_tokens_field) = config.max_tokens_field_pair(model);
    let retry = retry::retry_config_from_config(config);
    let messages = [ChatRequestMessage::simple("user", prompt.to_string())];
    let body = serde_json::to_value(&ChatCompletionsRequest {
        model,
        messages: &messages,
        tools: None,
        stream: true,
        stream_options: Some(ChatCompletionsStreamOptions {
            include_usage: true,
        }),
        max_tokens: max_tokens_field,
        max_completion_tokens: max_completion_tokens_field,
        reasoning_effort,
    })
    .map_err(io::Error::other)?;
    // Per-attempt wall-clock deadline spanning the whole request (see `retry::AttemptDeadline`).
    let mut deadline = retry::AttemptDeadline::new(config.total_timeout_secs);
    // Hoist the no-op retry callback into a named local: a bare `&mut None`
    // temporary would be dropped before the retry call below (E0716).
    let mut no_retry = None;
    let mut ctx = retry::AttemptContext::new(&mut no_retry, cancel_rx, Some(&mut deadline));
    let response = retry::retry_send(agent, &url, api_key, &body, &retry, &mut ctx)?;
    let mut reader = SseReader::from_reader(response.into_body().into_reader());
    // The blocking socket read lives on a dedicated thread (see
    // `crate::stream`): `recv_sse_event` below polls the channel with a short
    // timeout, so an Escape during a stalled stream is noticed within ~200 ms
    // instead of never (the old code only checked cancellation between reads).
    // Cancelling also arms the reader thread's abort flag, so it stops at its
    // next loop boundary instead of parsing the remainder of the stream.
    let sse = crate::stream::spawn_sse_reader(move || reader.next_event(), deadline.current());
    let mut has_any_output = false;
    while let Some(data) = crate::stream::recv_sse_event(&sse, cancel_rx)? {
        let payload: ChatCompletionsStreamResponse =
            serde_json::from_str(&data).map_err(io::Error::other)?;
        for choice in payload.choices {
            let Some(delta) = choice.delta else {
                continue;
            };

            if let Some(content) = delta.content.filter(|c| !c.is_empty()) {
                has_any_output = true;
                on_event(StreamEvent::Answer(content))?;
            }
            for reasoning in [
                delta.reasoning_content,
                delta.reasoning,
                delta.reasoning_text,
            ]
            .into_iter()
            .flatten()
            .filter(|content| !content.is_empty())
            {
                has_any_output = true;
                on_event(StreamEvent::Reasoning(reasoning))?;
            }
        }
    }

    if !has_any_output {
        return Err(super::OpenAiError::EmptyResponse);
    }

    Ok(())
}

// ── Chat Completions tool call accumulation ─────────────────────────────

/// Accumulates tool call fields across streaming SSE chunks keyed by the
/// tool call index assigned by the API.
#[derive(Debug, Default)]
struct AccumulatingToolCall {
    id: Option<String>,
    name: Option<String>,
    arguments: String,
}

/// Accumulate tool call deltas from streaming SSE chunks into ordered tool
/// calls.  Deltas with the same index are combined — `id` and `name` are taken
/// from the last chunk that carries them, and `arguments` is concatenated.
pub(crate) fn accumulate_tool_calls_from_deltas(
    deltas: impl IntoIterator<Item = StreamToolCallDelta>,
) -> Vec<ChatToolCall> {
    let mut map: HashMap<u32, AccumulatingToolCall> = HashMap::new();
    for tc_delta in deltas {
        let entry = map.entry(tc_delta.index).or_default();
        if let Some(id) = tc_delta.id {
            entry.id = Some(id);
        }
        if let Some(func) = tc_delta.function {
            if let Some(name) = func.name {
                entry.name = Some(name);
            }
            if let Some(args) = func.arguments {
                entry.arguments.push_str(&args);
            }
        }
    }
    let mut calls: Vec<_> = map.into_iter().collect();
    calls.sort_by_key(|(idx, _)| *idx);
    calls
        .into_iter()
        .map(|(_, tc)| ChatToolCall {
            id: tc.id.unwrap_or_default(),
            name: tc.name.unwrap_or_default(),
            arguments_json: tc.arguments,
            caller: None,
        })
        .collect()
}

// ── Streaming chat completions with tools ───────────────────────────────

/// Streaming variant of `chat_completions_request_with_tools`.
///
/// Sends `stream: true` with tool definitions, reads SSE chunks, and calls
/// `on_chunk` for each content / reasoning delta so the caller can forward
/// it to subscribers immediately.  Tool call deltas are accumulated across
/// chunks and returned as `ChatTurnResult::ToolUse` when the stream ends.
#[expect(clippy::too_many_arguments)]
pub(crate) fn chat_completions_request_streaming_with_tools<F>(
    agent: &ureq::Agent,
    config: &super::ServiceConfig,
    api_key: &str,
    model: &str,
    messages: &[ChatRequestMessage],
    tools: &[ChatToolDefinition],
    reasoning_effort: Option<&str>,
    on_retry: &mut Option<retry::RetryCallback>,
    cancel_rx: Option<&mpsc::Receiver<()>>,
    on_event: &mut F,
) -> Result<ChatTurnResult, super::OpenAiError>
where
    F: FnMut(StreamEvent) -> io::Result<()>,
{
    let url = endpoint_url(&config.base_url, &config.chat_completions_path)?;
    let (max_tokens_field, max_completion_tokens_field) = config.max_tokens_field_pair(model);
    let retry = retry::retry_config_from_config(config);
    let body = serde_json::to_value(&ChatCompletionsRequest {
        model,
        messages,
        tools: Some(tools),
        stream: true,
        // Configurable stream_options — some OpenAI-compatible providers
        // reject the `stream_options` field entirely, so users can disable
        // it per-account to maintain compatibility.
        stream_options: if config.stream_options {
            Some(ChatCompletionsStreamOptions {
                include_usage: true,
            })
        } else {
            None
        },
        max_tokens: max_tokens_field,
        max_completion_tokens: max_completion_tokens_field,
        reasoning_effort,
    })
    .map_err(io::Error::other)?;
    // Per-attempt wall-clock deadline spanning the whole request (see `retry::AttemptDeadline`).
    let mut deadline = retry::AttemptDeadline::new(config.total_timeout_secs);
    let mut ctx = retry::AttemptContext::new(on_retry, cancel_rx, Some(&mut deadline));
    let response = retry::retry_send(agent, &url, api_key, &body, &retry, &mut ctx)?;
    let mut has_any_output = false;
    let mut full_content = String::new();
    let mut full_reasoning = String::new();
    // Collect raw tool call deltas across all chunks, then delegate to the
    // shared accumulator once the stream is fully consumed.
    let mut raw_tool_call_deltas: Vec<StreamToolCallDelta> = Vec::new();
    let mut seen_tool_call_indices = [false; MAX_TOOL_CALLS];
    let mut distinct_tool_call_count = 0usize;

    let mut reader = SseReader::from_reader(response.into_body().into_reader());
    // Track usage from the final SSE chunk (OpenAI sends a usage chunk with
    // choices: [] when stream_options.include_usage is true).
    let mut last_usage: Option<TokenUsage> = None;
    // Reader thread decouples the blocking socket read from cancellation
    // polling (see `crate::stream`); the abort flag on `sse` stops the thread
    // at its next loop boundary once the consumer cancels or drops it.
    let sse = crate::stream::spawn_sse_reader(move || reader.next_event(), deadline.current());
    while let Some(data) = crate::stream::recv_sse_event(&sse, cancel_rx)? {
        let payload: ChatCompletionsStreamResponse =
            serde_json::from_str(&data).map_err(io::Error::other)?;

        // Capture usage from the final chunk (OpenAI sends a usage chunk
        // with choices: []).
        if let Some(ref u) = payload.usage {
            debug!(
                prompt_tokens = u.prompt_tokens,
                completion_tokens = u.completion_tokens,
                total_tokens = u.total_tokens,
                "OpenAI streaming turn usage"
            );
            let usage = TokenUsage {
                input_tokens: u.prompt_tokens,
                output_tokens: u.completion_tokens,
                total_tokens: u.total_tokens,
            };
            last_usage = Some(usage);
        }

        for choice in payload.choices {
            let Some(delta) = choice.delta else {
                continue;
            };

            // Content chunks: answer text
            if let Some(content) = delta.content.filter(|c| !c.is_empty()) {
                has_any_output = true;
                full_content.push_str(&content);
                on_event(StreamEvent::Answer(content))?;
            }

            // Reasoning chunks — use references to avoid partial moves.
            for reasoning in [
                &delta.reasoning_content,
                &delta.reasoning,
                &delta.reasoning_text,
            ]
            .into_iter()
            .flatten()
            .filter(|r| !r.is_empty())
            {
                has_any_output = true;
                full_reasoning.push_str(reasoning);
                on_event(StreamEvent::Reasoning(reasoning.to_string()))?;
            }

            // Collect raw tool call deltas — the shared accumulator
            // (accumulate_tool_calls_from_deltas) will merge them by index
            // and produce sorted ChatToolCall output after the stream ends.
            if let Some(ref tcs) = delta.tool_calls {
                has_any_output = true;
                for tc in tcs.iter() {
                    if distinct_tool_call_count >= MAX_TOOL_CALLS {
                        return Err(super::OpenAiError::Io(io::Error::other(format!(
                            "too many tool calls (max {MAX_TOOL_CALLS})"
                        ))));
                    }
                    if (tc.index as usize) >= MAX_TOOL_CALLS {
                        return Err(super::OpenAiError::Io(io::Error::other(format!(
                            "tool call index {} out of bounds (max {})",
                            tc.index,
                            MAX_TOOL_CALLS - 1,
                        ))));
                    }
                    if !seen_tool_call_indices[tc.index as usize] {
                        seen_tool_call_indices[tc.index as usize] = true;
                        distinct_tool_call_count += 1;
                    }
                    raw_tool_call_deltas.push(tc.clone());
                }
            }
        }
    }

    if !has_any_output {
        return Err(super::OpenAiError::EmptyResponse);
    }

    if !raw_tool_call_deltas.is_empty() {
        let mut tool_calls = accumulate_tool_calls_from_deltas(raw_tool_call_deltas);
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
                response_id: None,
            }));
        }
        if !discarded.is_empty() {
            // All calls had invalid arguments. Return accumulated text so the
            // session can continue gracefully.
            return Ok(ChatTurnResult::FinalText(FinalTextResult {
                content: full_content,
                reasoning: if full_reasoning.is_empty() {
                    None
                } else {
                    Some(full_reasoning)
                },
                usage: last_usage,
                response_id: None,
            }));
        }
    }

    Ok(ChatTurnResult::FinalText(FinalTextResult {
        content: full_content,
        reasoning: if full_reasoning.is_empty() {
            None
        } else {
            Some(full_reasoning)
        },
        usage: last_usage,
        response_id: None,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    // -- validate_tool_call_arguments tests --------------------------------

    #[test]
    fn validate_valid_arguments_kept() {
        let mut calls = vec![
            ChatToolCall {
                id: "call_1".into(),
                name: "get_weather".into(),
                arguments_json: r#"{"city":"London"}"#.into(),
                caller: None,
            },
            ChatToolCall {
                id: "call_2".into(),
                name: "search".into(),
                arguments_json: r#"{"q":"rust"}"#.into(),
                caller: None,
            },
        ];
        let discarded = crate::openai::validate_tool_call_arguments(&mut calls);
        assert!(discarded.is_empty());
        assert_eq!(calls.len(), 2);
    }

    #[test]
    fn validate_invalid_arguments_discarded() {
        let mut calls = vec![
            ChatToolCall {
                id: "call_1".into(),
                name: "get_weather".into(),
                arguments_json: r#"{"city":"London"}"#.into(),
                caller: None,
            },
            ChatToolCall {
                id: "call_2".into(),
                name: "bad_tool".into(),
                arguments_json: "truncated garbage".into(),
                caller: None,
            },
        ];
        let discarded = crate::openai::validate_tool_call_arguments(&mut calls);
        assert_eq!(
            discarded,
            vec![choreo_proto::DiscardedToolCall {
                name: "bad_tool".into(),
                arguments_json: "truncated garbage".into(),
            }]
        );
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "get_weather");
    }

    #[test]
    fn validate_all_invalid_returns_all_names() {
        let mut calls = vec![
            ChatToolCall {
                id: "call_1".into(),
                name: "tool_a".into(),
                arguments_json: "bad".into(),
                caller: None,
            },
            ChatToolCall {
                id: "call_2".into(),
                name: "tool_b".into(),
                arguments_json: "also bad".into(),
                caller: None,
            },
        ];
        let discarded = crate::openai::validate_tool_call_arguments(&mut calls);
        assert_eq!(discarded.len(), 2);
        assert_eq!(discarded[0].name, "tool_a");
        assert_eq!(discarded[0].arguments_json, "bad");
        assert_eq!(discarded[1].name, "tool_b");
        assert_eq!(discarded[1].arguments_json, "also bad");
        assert!(calls.is_empty());
    }

    #[test]
    fn validate_empty_list_returns_empty() {
        let mut calls: Vec<ChatToolCall> = vec![];
        let discarded = crate::openai::validate_tool_call_arguments(&mut calls);
        assert!(discarded.is_empty());
        assert!(calls.is_empty());
    }

    // -- tool call accumulation tests ------------------------------------

    #[test]
    fn accumulate_no_deltas_returns_empty_vec() {
        let result = accumulate_tool_calls_from_deltas(vec![]);
        assert!(result.is_empty());
    }

    #[test]
    fn accumulate_single_tool_call_in_one_chunk() {
        let deltas = vec![StreamToolCallDelta {
            index: 0,
            id: Some("call_1".into()),
            kind: Some("function".into()),
            function: Some(StreamToolCallFunctionDelta {
                name: Some("get_weather".into()),
                arguments: Some(r#"{"city":"London"}"#.into()),
            }),
        }];
        let result = accumulate_tool_calls_from_deltas(deltas);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].id, "call_1");
        assert_eq!(result[0].name, "get_weather");
        assert_eq!(result[0].arguments_json, r#"{"city":"London"}"#);
    }

    #[test]
    fn accumulate_arguments_concatenated_across_chunks() {
        let deltas = vec![
            StreamToolCallDelta {
                index: 0,
                id: Some("call_1".into()),
                kind: None,
                function: Some(StreamToolCallFunctionDelta {
                    name: Some("get_weather".into()),
                    // First half: split inside the value string, not at a
                    // quote boundary, so concatenation yields valid JSON.
                    arguments: Some(r#"{"city":"Lon"#.into()),
                }),
            },
            StreamToolCallDelta {
                index: 0,
                id: None,
                kind: None,
                function: Some(StreamToolCallFunctionDelta {
                    name: None,
                    arguments: Some(r#"don"}"#.into()),
                }),
            },
        ];
        let result = accumulate_tool_calls_from_deltas(deltas);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].id, "call_1");
        assert_eq!(result[0].name, "get_weather");
        assert_eq!(result[0].arguments_json, r#"{"city":"London"}"#);
    }

    #[test]
    fn accumulate_multiple_tool_calls_sorted_by_index() {
        let deltas = vec![
            // Tool call 1, first chunk
            StreamToolCallDelta {
                index: 1,
                id: Some("call_2".into()),
                kind: None,
                function: Some(StreamToolCallFunctionDelta {
                    name: Some("search".into()),
                    arguments: Some(r#"{"q":"rust"}"#.into()),
                }),
            },
            // Tool call 0, arrives after index 1
            StreamToolCallDelta {
                index: 0,
                id: Some("call_1".into()),
                kind: None,
                function: Some(StreamToolCallFunctionDelta {
                    name: Some("get_weather".into()),
                    arguments: Some(r#"{"city":"Paris"}"#.into()),
                }),
            },
        ];
        let result = accumulate_tool_calls_from_deltas(deltas);
        assert_eq!(result.len(), 2);
        // Must be sorted by index: index 0 first, then index 1
        assert_eq!(result[0].id, "call_1");
        assert_eq!(result[0].name, "get_weather");
        assert_eq!(result[1].id, "call_2");
        assert_eq!(result[1].name, "search");
    }

    #[test]
    fn accumulate_missing_id_defaults_to_empty() {
        let deltas = vec![StreamToolCallDelta {
            index: 0,
            id: None,
            kind: None,
            function: Some(StreamToolCallFunctionDelta {
                name: Some("get_weather".into()),
                arguments: Some(r#"{}"#.into()),
            }),
        }];
        let result = accumulate_tool_calls_from_deltas(deltas);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].id, "");
        assert_eq!(result[0].name, "get_weather");
    }

    #[test]
    fn accumulate_missing_name_defaults_to_empty() {
        let deltas = vec![StreamToolCallDelta {
            index: 0,
            id: Some("call_1".into()),
            kind: None,
            function: Some(StreamToolCallFunctionDelta {
                name: None,
                arguments: Some(r#"{}"#.into()),
            }),
        }];
        let result = accumulate_tool_calls_from_deltas(deltas);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].id, "call_1");
        assert_eq!(result[0].name, "");
    }

    #[test]
    fn accumulate_no_function_delta_produces_empty_call() {
        let deltas = vec![StreamToolCallDelta {
            index: 0,
            id: Some("call_1".into()),
            kind: None,
            function: None,
        }];
        let result = accumulate_tool_calls_from_deltas(deltas);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].id, "call_1");
        assert_eq!(result[0].name, "");
        assert_eq!(result[0].arguments_json, "");
    }

    // -- streaming delta deserialisation tests --------------------------

    #[test]
    fn stream_delta_tool_calls_deserialises() {
        let payload: ChatCompletionsStreamResponse = serde_json::from_str(
            r#"{
                "choices":[{
                    "delta":{
                        "content":"Hello",
                        "tool_calls":[{
                            "index":0,
                            "id":"call_abc",
                            "type":"function",
                            "function":{"name":"get_weather","arguments":"{\"city\":\"London\"}"}
                        }]
                    }
                }]
            }"#,
        )
        .expect("parse");
        let delta = payload.choices.into_iter().next().unwrap().delta.unwrap();
        assert_eq!(delta.content.as_deref(), Some("Hello"));
        let tcs = delta.tool_calls.expect("tool_calls");
        assert_eq!(tcs.len(), 1);
        assert_eq!(tcs[0].index, 0);
        assert_eq!(tcs[0].id.as_deref(), Some("call_abc"));
        assert_eq!(tcs[0].kind.as_deref(), Some("function"));
        let func = tcs[0].function.as_ref().unwrap();
        assert_eq!(func.name.as_deref(), Some("get_weather"));
        assert_eq!(func.arguments.as_deref(), Some(r#"{"city":"London"}"#));
    }

    #[test]
    fn stream_delta_tool_calls_absent_when_not_in_json() {
        let payload: ChatCompletionsStreamResponse =
            serde_json::from_str(r#"{"choices":[{"delta":{"content":"Hi"}}]}"#).expect("parse");
        let delta = payload.choices.into_iter().next().unwrap().delta.unwrap();
        assert_eq!(delta.content.as_deref(), Some("Hi"));
        assert!(delta.tool_calls.is_none());
    }

    // -- accumulated deltas -> ChatTurnResult integration test ----------

    #[test]
    fn accumulate_deltas_to_tool_use_result() {
        // Simulate what the streaming function does: collect deltas from
        // multiple SSE chunks and pass them through the accumulator.
        let deltas = vec![
            StreamToolCallDelta {
                index: 0,
                id: Some("call_1".into()),
                kind: None,
                function: Some(StreamToolCallFunctionDelta {
                    name: Some("search".into()),
                    arguments: Some(r#"{"q":"we"#.into()),
                }),
            },
            StreamToolCallDelta {
                index: 0,
                id: None,
                kind: None,
                function: Some(StreamToolCallFunctionDelta {
                    name: None,
                    arguments: Some(r#"ather"}"#.into()),
                }),
            },
        ];
        let tool_calls = accumulate_tool_calls_from_deltas(deltas);
        assert_eq!(tool_calls.len(), 1);
        assert_eq!(tool_calls[0].id, "call_1");
        assert_eq!(tool_calls[0].name, "search");
        assert_eq!(tool_calls[0].arguments_json, r#"{"q":"weather"}"#);

        let result = ChatTurnResult::ToolUse(ChatAssistantToolUse {
            content: Some("I'll search for that.".into()),
            tool_calls,
            reasoning: None,
            usage: None,
            response_id: None,
        });
        match result {
            ChatTurnResult::ToolUse(use_) => {
                assert_eq!(use_.content.as_deref(), Some("I'll search for that."));
                assert_eq!(use_.tool_calls.len(), 1);
            }
            _ => panic!("expected ToolUse"),
        }
    }

    // -- reasoning_effort serialization tests ---------------------------

    #[test]
    fn reasoning_effort_serialization() {
        // Off → None (omitted from body)
        assert_eq!(crate::openai::reasoning_effort_api_value("off"), None);

        // Low → "low"
        assert_eq!(
            crate::openai::reasoning_effort_api_value("low"),
            Some("low")
        );

        // Medium → "medium"
        assert_eq!(
            crate::openai::reasoning_effort_api_value("medium"),
            Some("medium")
        );

        // High → "high"
        assert_eq!(
            crate::openai::reasoning_effort_api_value("high"),
            Some("high")
        );
    }

    #[test]
    fn chat_completions_request_omits_reasoning_effort_when_none() {
        let body = serde_json::to_value(&ChatCompletionsRequest {
            model: "gpt-4.1",
            messages: &[ChatRequestMessage::simple("user", "hello".into())],
            tools: None,
            stream: false,
            stream_options: None,
            max_tokens: None,
            max_completion_tokens: None,
            reasoning_effort: None,
        })
        .unwrap();
        assert!(body.get("reasoning_effort").is_none(), "should be omitted");
    }

    // -- token usage streaming response tests ----------------------------

    #[test]
    fn stream_response_deserializes_usage_chunk() {
        // OpenAI sends a usage-only chunk at the end of a stream with
        // stream_options.include_usage=true.
        let json = r#"{"choices":[],"usage":{"prompt_tokens":50,"completion_tokens":25,"total_tokens":75}}"#;
        let payload: ChatCompletionsStreamResponse = serde_json::from_str(json).unwrap();
        assert!(payload.choices.is_empty());
        let usage = payload.usage.expect("usage should be present");
        assert_eq!(usage.prompt_tokens, 50);
        assert_eq!(usage.completion_tokens, 25);
        assert_eq!(usage.total_tokens, 75);
    }

    #[test]
    fn stream_response_without_usage_defaults_to_none() {
        let json = r#"{"choices":[{"delta":{"content":"hello"}}]}"#;
        let payload: ChatCompletionsStreamResponse = serde_json::from_str(json).unwrap();
        assert_eq!(payload.choices.len(), 1);
        assert!(payload.usage.is_none());
    }

    #[test]
    fn test_chat_completions_response_non_streaming_with_usage() {
        // Non-streaming response with usage
        let json = r#"{"choices":[{"message":{"content":"Hello","tool_calls":[],"reasoning_content":null,"reasoning":null,"reasoning_text":null}}],"usage":{"prompt_tokens":10,"completion_tokens":5,"total_tokens":15}}"#;
        let resp: ChatCompletionsResponse = serde_json::from_str(json).unwrap();
        let usage = resp.usage.expect("usage should be present");
        assert_eq!(usage.prompt_tokens, 10);
        assert_eq!(usage.completion_tokens, 5);
        assert_eq!(usage.total_tokens, 15);
    }

    // -- chat completions stream delta keeps reasoning separate -----------

    #[test]
    fn chat_completions_stream_delta_keeps_reasoning_separate() {
        let payload: ChatCompletionsStreamResponse = serde_json::from_str(
            r#"{"choices":[{"delta":{"content":"answer","reasoning_text":"think"}}]}"#,
        )
        .expect("parse");

        let delta = payload
            .choices
            .into_iter()
            .next()
            .expect("choice")
            .delta
            .expect("delta");
        assert_eq!(delta.content.as_deref(), Some("answer"));
        assert_eq!(delta.reasoning_text.as_deref(), Some("think"));
    }
}
