use tracing::{debug, info};

use super::ServiceConfig;
use super::retry;
use super::{
    ChatAssistantToolUse, ChatCompletionsRequest, ChatCompletionsResponse,
    ChatCompletionsStreamOptions, ChatCompletionsStreamResponse, ChatRequestMessage, ChatToolCall,
    ChatToolDefinition, ChatTurnResult, CompletionChunkKind, ModelListResponse, OpenAiClient,
    RequestFormat, ResponsesRequest, ResponsesResponse, SseReader, StreamToolCallDelta,
    endpoint_url, extract_responses_text_delta,
};
use std::collections::HashMap;
use std::io;
use std::sync::mpsc;

impl OpenAiClient {
    pub fn validate_and_list_models(&self) -> Result<Vec<String>, super::OpenAiError> {
        info!("listing models from {}", self.config.base_url);
        let url = endpoint_url(&self.config.base_url, &self.config.model_list_path)?;
        let retry = retry::RetryConfig::from_service_config(&self.config);
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
                &mut on_chunk,
            ),
        }
    }

    pub fn chat_completion_turn(
        &self,
        model: &str,
        messages: &[ChatRequestMessage],
        tools: &[ChatToolDefinition],
        on_retry: &mut Option<retry::RetryCallback>,
        cancel_rx: Option<&mpsc::Receiver<()>>,
    ) -> Result<ChatTurnResult, super::OpenAiError> {
        chat_completions_request_with_tools(
            &self.http,
            &self.config,
            &self.api_key,
            model,
            messages,
            tools,
            on_retry,
            cancel_rx,
        )
    }

    pub fn chat_completion_turn_streaming<F>(
        &self,
        model: &str,
        messages: &[ChatRequestMessage],
        tools: &[ChatToolDefinition],
        on_retry: &mut Option<retry::RetryCallback>,
        cancel_rx: Option<&mpsc::Receiver<()>>,
        on_chunk: F,
    ) -> Result<ChatTurnResult, super::OpenAiError>
    where
        F: FnMut(super::CompletionChunkKind, String) -> io::Result<()>,
    {
        // Instrument API call timing and error classification for monitoring.
        // We wrap both the streaming and non-streaming paths with a single
        // timing measurement that includes retry backoff.
        let api_start = std::time::Instant::now();
        let result: Result<ChatTurnResult, super::OpenAiError> = (|| {
            if !self.config.streaming {
                // Fall back to non-streaming, deliver the full response as a single
                // chunk through the callback so the caller's broadcasting path
                // stays uniform regardless of the streaming setting.
                let mut on_chunk = on_chunk;
                let result =
                    self.chat_completion_turn(model, messages, tools, on_retry, cancel_rx)?;
                match &result {
                    ChatTurnResult::FinalText(content) => {
                        if !content.is_empty() {
                            on_chunk(super::CompletionChunkKind::Answer, content.clone())?;
                        }
                    }
                    ChatTurnResult::ToolUse(tool_use) => {
                        if let Some(ref content) = tool_use.content
                            && !content.is_empty()
                        {
                            on_chunk(super::CompletionChunkKind::Answer, content.clone())?;
                        }
                        // Send reasoning through whichever field the model populated.
                        if let Some(reasoning) =
                            tool_use.reasoning.as_ref().filter(|r| !r.is_empty())
                        {
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
                model,
                messages,
                tools,
                on_retry,
                cancel_rx,
                on_chunk,
            )
        })();

        let elapsed = api_start.elapsed().as_secs_f64();
        match &result {
            Ok(_) => {
                crate::metrics::record_api_call(model, "chat/completions", elapsed);
            }
            Err(e) => {
                crate::metrics::record_api_call(model, "chat/completions", elapsed);
                crate::metrics::record_api_error(model, super::error_type_label(e));
            }
        }
        result
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
    let retry = retry::RetryConfig::from_service_config(config);
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
        match retry::chat_completions_max_tokens_field(config, model) {
            retry::MaxTokensField::MaxTokens => (max_tokens, None),
            retry::MaxTokensField::MaxCompletionTokens => (None, max_tokens),
        };
    let retry = retry::RetryConfig::from_service_config(config);
    let messages = [ChatRequestMessage::simple("user", prompt.to_string())];
    let body = serde_json::to_value(&ChatCompletionsRequest {
        model,
        messages: &messages,
        tools: None,
        stream: false,
        stream_options: None,
        max_tokens: max_tokens_field,
        max_completion_tokens: max_completion_tokens_field,
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
    on_retry: &mut Option<retry::RetryCallback>,
    cancel_rx: Option<&mpsc::Receiver<()>>,
) -> Result<ChatTurnResult, super::OpenAiError> {
    let start = std::time::Instant::now();
    let url = endpoint_url(&config.base_url, &config.chat_completions_path)?;
    let max_tokens = config.max_tokens_for_model(model);
    let (max_tokens_field, max_completion_tokens_field) =
        match retry::chat_completions_max_tokens_field(config, model) {
            retry::MaxTokensField::MaxTokens => (max_tokens, None),
            retry::MaxTokensField::MaxCompletionTokens => (None, max_tokens),
        };
    let retry = retry::RetryConfig::from_service_config(config);
    let body = serde_json::to_value(&ChatCompletionsRequest {
        model,
        messages,
        tools: Some(tools),
        stream: false,
        stream_options: None,
        max_tokens: max_tokens_field,
        max_completion_tokens: max_completion_tokens_field,
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

    let Some(choice) = payload.choices.into_iter().next() else {
        return Err(super::OpenAiError::EmptyResponse);
    };

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
            reasoning: choice
                .message
                .reasoning_content
                .or(choice.message.reasoning)
                .or(choice.message.reasoning_text),
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

    Ok(ChatTurnResult::FinalText(content))
}

fn chat_completions_request_streaming<F>(
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
    let url = endpoint_url(&config.base_url, &config.chat_completions_path)?;
    let max_tokens = config.max_tokens_for_model(model);
    let (max_tokens_field, max_completion_tokens_field) =
        match retry::chat_completions_max_tokens_field(config, model) {
            retry::MaxTokensField::MaxTokens => (max_tokens, None),
            retry::MaxTokensField::MaxCompletionTokens => (None, max_tokens),
        };
    let retry = retry::RetryConfig::from_service_config(config);
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
    let retry = retry::RetryConfig::from_service_config(config);
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
        match retry::chat_completions_max_tokens_field(config, model) {
            retry::MaxTokensField::MaxTokens => (max_tokens, None),
            retry::MaxTokensField::MaxCompletionTokens => (None, max_tokens),
        };
    let retry = retry::RetryConfig::from_service_config(config);
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
    while let Some(data) = reader.next_event()? {
        let payload: ChatCompletionsStreamResponse =
            serde_json::from_str(&data).map_err(io::Error::other)?;
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
        }));
    }

    Ok(ChatTurnResult::FinalText(full_content))
}

#[cfg(test)]
mod tests {
    use super::retry;
    use super::*;
    use crate::openai::StreamToolCallFunctionDelta;
    use std::time::Duration;

    // -- sleep_or_cancel tests -------------------------------------------

    #[test]
    fn sleep_or_cancel_signal_returns_cancelled() {
        let (tx, rx) = mpsc::channel::<()>();
        tx.send(()).unwrap();
        let result = retry::sleep_or_cancel(Duration::from_secs(10), Some(&rx));
        assert!(matches!(result, Err(crate::openai::OpenAiError::Cancelled)));
    }

    #[test]
    fn sleep_or_cancel_disconnected_returns_ok() {
        let (tx, rx) = mpsc::channel::<()>();
        drop(tx);
        let result = retry::sleep_or_cancel(Duration::from_millis(1), Some(&rx));
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
        });
        match result {
            ChatTurnResult::ToolUse(use_) => {
                assert_eq!(use_.content.as_deref(), Some("I'll search for that."));
                assert_eq!(use_.tool_calls.len(), 1);
            }
            _ => panic!("expected ToolUse"),
        }
    }
}
