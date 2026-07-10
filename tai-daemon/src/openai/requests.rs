use tracing::{debug, info};

use super::ServiceConfig;
use super::retry;
use super::{
    ChatAssistantToolUse, ChatCompletionsRequest, ChatCompletionsResponse,
    ChatCompletionsStreamOptions, ChatCompletionsStreamResponse, ChatRequestMessage, ChatToolCall,
    ChatToolDefinition, ChatTurnResult, CompletionChunkKind, FinalTextResult, ModelListResponse,
    OpenAiClient, RequestFormat, ResponsesRequest, ResponsesResponse, SseReader,
    StreamToolCallDelta, endpoint_url, extract_responses_text_delta, reasoning_effort_api_value,
};
use crate::providers::ChatTurnRequest;
use std::collections::HashMap;
use std::io;
use std::sync::mpsc;
use tai_proto::TokenUsage;

impl OpenAiClient {
    pub fn validate_and_list_models(&self) -> Result<Vec<String>, super::OpenAiError> {
        info!("listing models from {}", self.config.base_url);
        let url = endpoint_url(&self.config.base_url, &self.config.model_list_path)?;
        let retry = retry::retry_config_from_config(&self.config);
        let response = retry::retry_send_get_simple(&self.http, &url, &self.api_key, &retry)?;
        let payload: ModelListResponse = response
            .json()
            .map_err(|e| super::OpenAiError::Io(io::Error::other(e)))?;
        let models: Vec<String> = payload.data.into_iter().map(|model| model.id).collect();
        info!("models returned: {}", models.len());
        Ok(models)
    }

    pub fn completion(&self, model: &str, prompt: &str) -> Result<String, super::OpenAiError> {
        match self.config.request_format_for_model(model) {
            RequestFormat::Responses => {
                responses_request(&self.http, &self.config, &self.api_key, model, prompt)
            }
            RequestFormat::ChatCompletions => {
                chat_completions_request(&self.http, &self.config, &self.api_key, model, prompt)
            }
        }
    }

    pub fn completion_stream<F>(
        &self,
        model: &str,
        prompt: &str,
        mut on_chunk: F,
    ) -> Result<(), super::OpenAiError>
    where
        F: FnMut(CompletionChunkKind, String) -> io::Result<()>,
    {
        if !self.config.streaming {
            let content = self.completion(model, prompt)?;
            if !content.is_empty() {
                on_chunk(CompletionChunkKind::Answer, content)?;
            }
            return Ok(());
        }

        match self.config.request_format_for_model(model) {
            RequestFormat::Responses => responses_request_streaming(
                &self.http,
                &self.config,
                &self.api_key,
                model,
                prompt,
                &mut on_chunk,
            ),
            RequestFormat::ChatCompletions => chat_completions_request_streaming(
                &self.http,
                &self.config,
                &self.api_key,
                model,
                prompt,
                None, // No reasoning_effort for simple completion
                &mut on_chunk,
            ),
        }
    }

    pub fn chat_completion_turn(
        &self,
        params: ChatTurnRequest<'_>,
    ) -> Result<ChatTurnResult, super::OpenAiError> {
        let reasoning_effort = reasoning_effort_api_value(params.thinking_effort);
        debug!(?params.thinking_effort, ?reasoning_effort, "chat_completion_turn");
        chat_completions_request_with_tools(
            &self.http,
            &self.config,
            &self.api_key,
            params.model,
            params.messages,
            params.tools,
            reasoning_effort,
            params.on_retry,
            params.cancel_rx,
        )
    }

    pub fn chat_completion_turn_streaming<F>(
        &self,
        params: ChatTurnRequest<'_>,
        on_chunk: F,
    ) -> Result<ChatTurnResult, super::OpenAiError>
    where
        F: FnMut(super::CompletionChunkKind, String) -> io::Result<()>,
    {
        let reasoning_effort = reasoning_effort_api_value(params.thinking_effort);
        debug!(
            ?params.thinking_effort,
            ?reasoning_effort,
            "chat_completion_turn_streaming"
        );
        if !self.config.streaming {
            // Fall back to non-streaming, deliver the full response as a single
            // chunk through the callback so the caller's broadcasting path
            // stays uniform regardless of the streaming setting.
            let mut on_chunk = on_chunk;
            let result = self.chat_completion_turn(params)?;
            match &result {
                ChatTurnResult::FinalText(final_text) => {
                    if !final_text.content.is_empty() {
                        on_chunk(
                            super::CompletionChunkKind::Answer,
                            final_text.content.clone(),
                        )?;
                    }
                    if let Some(reasoning) = final_text.reasoning.as_ref().filter(|r| !r.is_empty())
                    {
                        on_chunk(super::CompletionChunkKind::Reasoning, reasoning.clone())?;
                    }
                }
                ChatTurnResult::ToolUse(tool_use) => {
                    if let Some(ref content) = tool_use.content
                        && !content.is_empty()
                    {
                        on_chunk(super::CompletionChunkKind::Answer, content.clone())?;
                    }
                    // Send reasoning through whichever field the model populated.
                    if let Some(reasoning) = tool_use.reasoning.as_ref().filter(|r| !r.is_empty()) {
                        on_chunk(super::CompletionChunkKind::Reasoning, reasoning.clone())?;
                    }
                }
            }
            return Ok(result);
        }

        chat_completions_request_streaming_with_tools(
            &self.http,
            &self.config,
            &self.api_key,
            params.model,
            params.messages,
            params.tools,
            reasoning_effort,
            params.on_retry,
            params.cancel_rx,
            on_chunk,
        )
    }
}

fn responses_request(
    client: &reqwest::blocking::Client,
    config: &ServiceConfig,
    api_key: &str,
    model: &str,
    prompt: &str,
) -> Result<String, super::OpenAiError> {
    let url = endpoint_url(&config.base_url, &config.responses_path)?;
    let retry = retry::retry_config_from_config(config);
    let body = serde_json::to_value(&ResponsesRequest {
        model,
        input: prompt,
        stream: false,
    })
    .map_err(io::Error::other)?;
    let response = retry::retry_send_simple(client, &url, api_key, &body, &retry)?;
    let payload: ResponsesResponse = response
        .json()
        .map_err(|e| super::OpenAiError::Io(io::Error::other(e)))?;

    let content = payload
        .output
        .into_iter()
        .flat_map(|item| item.content.into_iter())
        .filter_map(|item| item.text)
        .map(|text| text.trim().to_string())
        .find(|text| !text.is_empty())
        .unwrap_or_default();

    if content.is_empty() {
        return Err(super::OpenAiError::EmptyResponse);
    }

    Ok(content)
}

fn chat_completions_request(
    client: &reqwest::blocking::Client,
    config: &ServiceConfig,
    api_key: &str,
    model: &str,
    prompt: &str,
) -> Result<String, super::OpenAiError> {
    let url = endpoint_url(&config.base_url, &config.chat_completions_path)?;
    let max_tokens = config.max_tokens_for_model(model);
    let (max_tokens_field, max_completion_tokens_field) =
        match config.max_tokens_field_for_model(model) {
            super::MaxTokensField::MaxTokens => (max_tokens, None),
            super::MaxTokensField::MaxCompletionTokens => (None, max_tokens),
        };
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
    let response = retry::retry_send_simple(client, &url, api_key, &body, &retry)?;
    let payload: ChatCompletionsResponse = response
        .json()
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

#[allow(clippy::too_many_arguments)]
fn chat_completions_request_with_tools(
    client: &reqwest::blocking::Client,
    config: &ServiceConfig,
    api_key: &str,
    model: &str,
    messages: &[ChatRequestMessage],
    tools: &[ChatToolDefinition],
    reasoning_effort: Option<&'static str>,
    on_retry: &mut Option<retry::RetryCallback>,
    cancel_rx: Option<&mpsc::Receiver<()>>,
) -> Result<ChatTurnResult, super::OpenAiError> {
    let start = std::time::Instant::now();
    let url = endpoint_url(&config.base_url, &config.chat_completions_path)?;
    let max_tokens = config.max_tokens_for_model(model);
    let (max_tokens_field, max_completion_tokens_field) =
        match config.max_tokens_field_for_model(model) {
            super::MaxTokensField::MaxTokens => (max_tokens, None),
            super::MaxTokensField::MaxCompletionTokens => (None, max_tokens),
        };
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
    let response = retry::retry_send(client, &url, api_key, &body, &retry, on_retry, cancel_rx)?;
    let payload: ChatCompletionsResponse = response
        .json()
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

    if !choice.message.tool_calls.is_empty() {
        return Ok(ChatTurnResult::ToolUse(ChatAssistantToolUse {
            content: choice.message.content,
            tool_calls: choice
                .message
                .tool_calls
                .into_iter()
                .map(|tool_call| ChatToolCall {
                    id: tool_call.id,
                    name: tool_call.function.name,
                    arguments_json: tool_call.function.arguments,
                })
                .collect(),
            reasoning,
            usage: turn_usage,
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
    }))
}

fn chat_completions_request_streaming<F>(
    client: &reqwest::blocking::Client,
    config: &ServiceConfig,
    api_key: &str,
    model: &str,
    prompt: &str,
    reasoning_effort: Option<&'static str>,
    on_chunk: &mut F,
) -> Result<(), super::OpenAiError>
where
    F: FnMut(CompletionChunkKind, String) -> io::Result<()>,
{
    let url = endpoint_url(&config.base_url, &config.chat_completions_path)?;
    let max_tokens = config.max_tokens_for_model(model);
    let (max_tokens_field, max_completion_tokens_field) =
        match config.max_tokens_field_for_model(model) {
            super::MaxTokensField::MaxTokens => (max_tokens, None),
            super::MaxTokensField::MaxCompletionTokens => (None, max_tokens),
        };
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
    let response = retry::retry_send_simple(client, &url, api_key, &body, &retry)?;
    let mut reader = SseReader::from_reader(response);
    let mut saw_text = false;
    while let Some(data) = reader.next_event()? {
        let payload: ChatCompletionsStreamResponse =
            serde_json::from_str(&data).map_err(io::Error::other)?;
        for choice in payload.choices {
            let Some(delta) = choice.delta else {
                continue;
            };

            if let Some(content) = delta.content.filter(|content| !content.is_empty()) {
                saw_text = true;
                on_chunk(CompletionChunkKind::Answer, content)?;
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
                saw_text = true;
                on_chunk(CompletionChunkKind::Reasoning, reasoning)?;
            }
        }
    }

    if !saw_text {
        return Err(super::OpenAiError::EmptyResponse);
    }

    Ok(())
}

fn responses_request_streaming<F>(
    client: &reqwest::blocking::Client,
    config: &ServiceConfig,
    api_key: &str,
    model: &str,
    prompt: &str,
    on_chunk: &mut F,
) -> Result<(), super::OpenAiError>
where
    F: FnMut(CompletionChunkKind, String) -> io::Result<()>,
{
    let url = endpoint_url(&config.base_url, &config.responses_path)?;
    let retry = retry::retry_config_from_config(config);
    let body = serde_json::to_value(&ResponsesRequest {
        model,
        input: prompt,
        stream: true,
    })
    .map_err(io::Error::other)?;
    let response = retry::retry_send_simple(client, &url, api_key, &body, &retry)?;
    let mut reader = SseReader::from_reader(response);
    let mut saw_text = false;
    while let Some(data) = reader.next_event()? {
        if let Some(delta) = extract_responses_text_delta(&data)?
            && !delta.is_empty()
        {
            saw_text = true;
            on_chunk(CompletionChunkKind::Answer, delta)?;
        }
    }

    if !saw_text {
        return Err(super::OpenAiError::EmptyResponse);
    }

    Ok(())
}

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
fn accumulate_tool_calls_from_deltas(
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
        })
        .collect()
}

/// Streaming variant of `chat_completions_request_with_tools`.
///
/// Sends `stream: true` with tool definitions, reads SSE chunks, and calls
/// `on_chunk` for each content / reasoning delta so the caller can forward
/// it to subscribers immediately.  Tool call deltas are accumulated across
/// chunks and returned as `ChatTurnResult::ToolUse` when the stream ends.
#[allow(clippy::too_many_arguments)]
fn chat_completions_request_streaming_with_tools<F>(
    client: &reqwest::blocking::Client,
    config: &ServiceConfig,
    api_key: &str,
    model: &str,
    messages: &[ChatRequestMessage],
    tools: &[ChatToolDefinition],
    reasoning_effort: Option<&'static str>,
    on_retry: &mut Option<retry::RetryCallback>,
    cancel_rx: Option<&mpsc::Receiver<()>>,
    mut on_chunk: F,
) -> Result<ChatTurnResult, super::OpenAiError>
where
    F: FnMut(CompletionChunkKind, String) -> io::Result<()>,
{
    let url = endpoint_url(&config.base_url, &config.chat_completions_path)?;
    let max_tokens = config.max_tokens_for_model(model);
    let (max_tokens_field, max_completion_tokens_field) =
        match config.max_tokens_field_for_model(model) {
            super::MaxTokensField::MaxTokens => (max_tokens, None),
            super::MaxTokensField::MaxCompletionTokens => (None, max_tokens),
        };
    let retry = retry::retry_config_from_config(config);
    let body = serde_json::to_value(&ChatCompletionsRequest {
        model,
        messages,
        tools: Some(tools),
        stream: true,
        stream_options: Some(ChatCompletionsStreamOptions {
            include_usage: true,
        }),
        max_tokens: max_tokens_field,
        max_completion_tokens: max_completion_tokens_field,
        reasoning_effort,
    })
    .map_err(io::Error::other)?;
    let response = retry::retry_send(client, &url, api_key, &body, &retry, on_retry, cancel_rx)?;
    let mut saw_text = false;
    let mut full_content = String::new();
    let mut full_reasoning = String::new();
    // Collect raw tool call deltas across all chunks, then delegate to the
    // shared accumulator once the stream is fully consumed.
    let mut raw_tool_call_deltas: Vec<StreamToolCallDelta> = Vec::new();

    let mut reader = SseReader::from_reader(response);
    // Track usage from the final SSE chunk (OpenAI sends a usage chunk with
    // choices: [] when stream_options.include_usage is true).
    let mut last_usage: Option<TokenUsage> = None;
    while let Some(data) = reader.next_event()? {
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
            last_usage = Some(TokenUsage {
                input_tokens: u.prompt_tokens,
                output_tokens: u.completion_tokens,
                total_tokens: u.total_tokens,
            });
        }

        for choice in payload.choices {
            let Some(delta) = choice.delta else {
                continue;
            };

            // Content chunks: answer text
            if let Some(content) = delta.content.filter(|c| !c.is_empty()) {
                saw_text = true;
                full_content.push_str(&content);
                on_chunk(CompletionChunkKind::Answer, content)?;
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
                saw_text = true;
                full_reasoning.push_str(reasoning);
                on_chunk(CompletionChunkKind::Reasoning, reasoning.clone())?;
            }

            // Collect raw tool call deltas — the shared accumulator
            // (accumulate_tool_calls_from_deltas) will merge them by index
            // and produce sorted ChatToolCall output after the stream ends.
            if let Some(ref tcs) = delta.tool_calls {
                saw_text = true;
                raw_tool_call_deltas.extend(tcs.iter().cloned());
            }
        }
    }

    if !saw_text {
        return Err(super::OpenAiError::EmptyResponse);
    }

    if !raw_tool_call_deltas.is_empty() {
        let tool_calls = accumulate_tool_calls_from_deltas(raw_tool_call_deltas);
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
        }));
    }

    Ok(ChatTurnResult::FinalText(FinalTextResult {
        content: full_content,
        reasoning: if full_reasoning.is_empty() {
            None
        } else {
            Some(full_reasoning)
        },
        usage: last_usage,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::openai::StreamToolCallFunctionDelta;
    use std::time::Duration;
    use tai_proto::ThinkingEffort;

    // -- sleep_or_cancel tests -------------------------------------------

    #[test]
    fn sleep_or_cancel_signal_returns_cancelled() {
        let (tx, rx) = mpsc::channel::<()>();
        tx.send(()).unwrap();
        let result = crate::retry::sleep_or_cancel(Duration::from_secs(10), Some(&rx));
        assert!(result.is_err());
    }

    #[test]
    fn sleep_or_cancel_disconnected_returns_ok() {
        let (tx, rx) = mpsc::channel::<()>();
        drop(tx);
        let result = crate::retry::sleep_or_cancel(Duration::from_millis(1), Some(&rx));
        assert!(result.is_ok());
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
        assert_eq!(
            crate::openai::reasoning_effort_api_value(ThinkingEffort::Off),
            None
        );

        // Low → "low"
        assert_eq!(
            crate::openai::reasoning_effort_api_value(ThinkingEffort::Low),
            Some("low")
        );

        // Medium → "medium"
        assert_eq!(
            crate::openai::reasoning_effort_api_value(ThinkingEffort::Medium),
            Some("medium")
        );

        // High → "high"
        assert_eq!(
            crate::openai::reasoning_effort_api_value(ThinkingEffort::High),
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

    #[test]
    fn chat_completions_request_includes_reasoning_effort_when_set() {
        let body = serde_json::to_value(&ChatCompletionsRequest {
            model: "o3-mini",
            messages: &[ChatRequestMessage::simple("user", "hello".into())],
            tools: None,
            stream: false,
            stream_options: None,
            max_tokens: None,
            max_completion_tokens: None,
            reasoning_effort: Some("low"),
        })
        .unwrap();
        assert_eq!(
            body.get("reasoning_effort"),
            Some(&serde_json::Value::String("low".into()))
        );
    }
}
