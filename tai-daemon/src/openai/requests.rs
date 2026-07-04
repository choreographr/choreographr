use super::ServiceConfig;
use super::{
    ChatAssistantToolUse, ChatCompletionsRequest, ChatCompletionsResponse,
    ChatCompletionsStreamOptions, ChatCompletionsStreamResponse, ChatRequestMessage, ChatToolCall,
    ChatToolDefinition, ChatTurnResult, CompletionChunkKind, ModelListResponse, OpenAiClient,
    RequestFormat, ResponsesRequest, ResponsesResponse, SseReader, endpoint_url,
    extract_responses_text_delta,
};
use std::{io, time::Duration};

#[derive(Debug, Clone)]
pub(crate) struct RetryConfig {
    pub(crate) max_attempts: u32,
    pub(crate) initial_backoff_ms: u64,
    pub(crate) max_backoff_ms: u64,
}

impl RetryConfig {
    fn from_service_config(config: &ServiceConfig) -> Self {
        Self {
            max_attempts: config.retry_max_attempts,
            initial_backoff_ms: config.retry_initial_backoff_ms,
            max_backoff_ms: config.retry_max_backoff_ms,
        }
    }
}

pub(crate) fn backoff_duration(retry_number: u32, config: &RetryConfig) -> Duration {
    let multiplier = 2u64.saturating_pow(retry_number.saturating_sub(1));
    let base = config.initial_backoff_ms.saturating_mul(multiplier);
    let capped = base.min(config.max_backoff_ms);
    let jitter: f64 = rand::random_range(0.75..=1.25);
    Duration::from_millis((capped as f64 * jitter) as u64)
}

pub(crate) fn is_retryable_status(status: reqwest::StatusCode) -> bool {
    matches!(
        status,
        reqwest::StatusCode::TOO_MANY_REQUESTS
            | reqwest::StatusCode::INTERNAL_SERVER_ERROR
            | reqwest::StatusCode::BAD_GATEWAY
            | reqwest::StatusCode::SERVICE_UNAVAILABLE
            | reqwest::StatusCode::GATEWAY_TIMEOUT
    )
}

pub(crate) fn parse_retry_after_secs(headers: &reqwest::header::HeaderMap) -> Option<u64> {
    headers
        .get(reqwest::header::RETRY_AFTER)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u64>().ok())
}

fn status_to_error(
    status: reqwest::StatusCode,
    detail: &str,
    headers: &reqwest::header::HeaderMap,
) -> super::OpenAiError {
    if status == reqwest::StatusCode::UNAUTHORIZED || status == reqwest::StatusCode::FORBIDDEN {
        return super::OpenAiError::Unauthorized {
            status: status.as_u16(),
            detail: detail.to_string(),
        };
    }
    if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
        return super::OpenAiError::RateLimited {
            retry_after_secs: parse_retry_after_secs(headers),
            detail: detail.to_string(),
        };
    }
    if status.is_server_error() {
        return super::OpenAiError::ServerError {
            status: status.as_u16(),
            detail: detail.to_string(),
        };
    }
    if status.is_client_error() {
        return super::OpenAiError::ClientError {
            status: status.as_u16(),
            detail: detail.to_string(),
        };
    }
    super::OpenAiError::Io(io::Error::new(io::ErrorKind::Other, detail.to_string()))
}

fn retry_send(
    client: &reqwest::blocking::Client,
    url: &str,
    api_key: &str,
    body: &serde_json::Value,
    retry: &RetryConfig,
) -> Result<reqwest::blocking::Response, super::OpenAiError> {
    let mut attempt: u32 = 0;
    loop {
        attempt += 1;

        let result = client
            .post(url)
            .bearer_auth(api_key.trim())
            .json(body)
            .send();

        match result {
            Ok(response) => {
                let status = response.status();
                let headers = response.headers().clone();
                if status.is_success() {
                    return Ok(response);
                }

                if is_retryable_status(status) && attempt < retry.max_attempts {
                    let retry_after = parse_retry_after_secs(response.headers());
                    let body_text = response.text().unwrap_or_default();
                    let delay = if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
                        retry_after
                            .map(Duration::from_secs)
                            .unwrap_or_else(|| backoff_duration(attempt, retry))
                    } else {
                        backoff_duration(attempt, retry)
                    };
                    tracing::warn!(
                        attempt,
                        max_attempts = retry.max_attempts,
                        ?status,
                        %body_text,
                        delay_ms = delay.as_millis(),
                        "retrying request"
                    );
                    std::thread::sleep(delay);
                    continue;
                }

                let body_text = response.text().unwrap_or_default();
                let trimmed_body = body_text.trim();
                let detail = if trimmed_body.is_empty() {
                    format!("request failed with status {status}")
                } else {
                    format!("request failed with status {status}: {trimmed_body}")
                };
                return Err(status_to_error(status, &detail, &headers));
            }
            Err(error) => {
                if (error.is_connect() || error.is_timeout()) && attempt < retry.max_attempts {
                    let delay = backoff_duration(attempt, retry);
                    tracing::warn!(
                        attempt,
                        max_attempts = retry.max_attempts,
                        ?error,
                        delay_ms = delay.as_millis(),
                        "retrying request after connection/timeout error"
                    );
                    std::thread::sleep(delay);
                    continue;
                }
                return Err(super::OpenAiError::Io(io::Error::other(error)));
            }
        }
    }
}

fn retry_send_get(
    client: &reqwest::blocking::Client,
    url: &str,
    api_key: &str,
    retry: &RetryConfig,
) -> Result<reqwest::blocking::Response, super::OpenAiError> {
    let mut attempt: u32 = 0;
    loop {
        attempt += 1;

        let result = client.get(url).bearer_auth(api_key.trim()).send();

        match result {
            Ok(response) => {
                let status = response.status();
                let headers = response.headers().clone();
                if status.is_success() {
                    return Ok(response);
                }

                if is_retryable_status(status) && attempt < retry.max_attempts {
                    let retry_after = parse_retry_after_secs(response.headers());
                    let body_text = response.text().unwrap_or_default();
                    let delay = if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
                        retry_after
                            .map(Duration::from_secs)
                            .unwrap_or_else(|| backoff_duration(attempt, retry))
                    } else {
                        backoff_duration(attempt, retry)
                    };
                    tracing::warn!(
                        attempt,
                        max_attempts = retry.max_attempts,
                        ?status,
                        %body_text,
                        delay_ms = delay.as_millis(),
                        "retrying request"
                    );
                    std::thread::sleep(delay);
                    continue;
                }

                let body_text = response.text().unwrap_or_default();
                let trimmed_body = body_text.trim();
                let detail = if trimmed_body.is_empty() {
                    format!("request failed with status {status}")
                } else {
                    format!("request failed with status {status}: {trimmed_body}")
                };
                return Err(status_to_error(status, &detail, &headers));
            }
            Err(error) => {
                if (error.is_connect() || error.is_timeout()) && attempt < retry.max_attempts {
                    let delay = backoff_duration(attempt, retry);
                    tracing::warn!(
                        attempt,
                        max_attempts = retry.max_attempts,
                        ?error,
                        delay_ms = delay.as_millis(),
                        "retrying request after connection/timeout error"
                    );
                    std::thread::sleep(delay);
                    continue;
                }
                return Err(super::OpenAiError::Io(io::Error::other(error)));
            }
        }
    }
}

#[derive(Debug, Clone, Copy)]
enum MaxTokensField {
    MaxTokens,
    MaxCompletionTokens,
}

fn chat_completions_max_tokens_field(config: &ServiceConfig, model: &str) -> MaxTokensField {
    if config.base_url.contains("opencode.ai") || model == "big-pickle" {
        MaxTokensField::MaxTokens
    } else {
        MaxTokensField::MaxCompletionTokens
    }
}

impl OpenAiClient {
    pub fn validate_and_list_models(&self) -> Result<Vec<String>, super::OpenAiError> {
        let url = endpoint_url(&self.config.base_url, &self.config.model_list_path)?;
        let retry = RetryConfig::from_service_config(&self.config);
        let response = retry_send_get(&self.http, &url, &self.api_key, &retry)?;
        let payload: ModelListResponse = response
            .json()
            .map_err(|e| super::OpenAiError::Io(io::Error::other(e)))?;
        Ok(payload.data.into_iter().map(|model| model.id).collect())
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
    ) -> Result<ChatTurnResult, super::OpenAiError> {
        chat_completions_request_with_tools(
            &self.http,
            &self.config,
            &self.api_key,
            model,
            messages,
            tools,
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
    let retry = RetryConfig::from_service_config(config);
    let body = serde_json::to_value(&ResponsesRequest {
        model,
        input: prompt,
        stream: false,
    })
    .map_err(io::Error::other)?;
    let response = retry_send(client, &url, api_key, &body, &retry)?;
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
        match chat_completions_max_tokens_field(config, model) {
            MaxTokensField::MaxTokens => (max_tokens, None),
            MaxTokensField::MaxCompletionTokens => (None, max_tokens),
        };
    let retry = RetryConfig::from_service_config(config);
    let body = serde_json::to_value(&ChatCompletionsRequest {
        model,
        messages: vec![ChatRequestMessage::simple("user", prompt.to_string())],
        tools: None,
        stream: false,
        stream_options: None,
        max_tokens: max_tokens_field,
        max_completion_tokens: max_completion_tokens_field,
    })
    .map_err(io::Error::other)?;
    let response = retry_send(client, &url, api_key, &body, &retry)?;
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

fn chat_completions_request_with_tools(
    client: &reqwest::blocking::Client,
    config: &ServiceConfig,
    api_key: &str,
    model: &str,
    messages: &[ChatRequestMessage],
    tools: &[ChatToolDefinition],
) -> Result<ChatTurnResult, super::OpenAiError> {
    let url = endpoint_url(&config.base_url, &config.chat_completions_path)?;
    let max_tokens = config.max_tokens_for_model(model);
    let (max_tokens_field, max_completion_tokens_field) =
        match chat_completions_max_tokens_field(config, model) {
            MaxTokensField::MaxTokens => (max_tokens, None),
            MaxTokensField::MaxCompletionTokens => (None, max_tokens),
        };
    let retry = RetryConfig::from_service_config(config);
    let body = serde_json::to_value(&ChatCompletionsRequest {
        model,
        messages: messages.to_vec(),
        tools: Some(tools.to_vec()),
        stream: false,
        stream_options: None,
        max_tokens: max_tokens_field,
        max_completion_tokens: max_completion_tokens_field,
    })
    .map_err(io::Error::other)?;
    let response = retry_send(client, &url, api_key, &body, &retry)?;
    let payload: ChatCompletionsResponse = response
        .json()
        .map_err(|e| super::OpenAiError::Io(io::Error::other(e)))?;

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
            reasoning_content: choice.message.reasoning_content,
            reasoning: choice.message.reasoning,
            reasoning_text: choice.message.reasoning_text,
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
        match chat_completions_max_tokens_field(config, model) {
            MaxTokensField::MaxTokens => (max_tokens, None),
            MaxTokensField::MaxCompletionTokens => (None, max_tokens),
        };
    let retry = RetryConfig::from_service_config(config);
    let body = serde_json::to_value(&ChatCompletionsRequest {
        model,
        messages: vec![ChatRequestMessage::simple("user", prompt.to_string())],
        tools: None,
        stream: true,
        stream_options: Some(ChatCompletionsStreamOptions {
            include_usage: true,
        }),
        max_tokens: max_tokens_field,
        max_completion_tokens: max_completion_tokens_field,
    })
    .map_err(io::Error::other)?;
    let response = retry_send(client, &url, api_key, &body, &retry)?;
    let body_bytes = response.bytes().map_err(io::Error::other)?.to_vec();

    let mut reader = SseReader::new(body_bytes);
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
    let retry = RetryConfig::from_service_config(config);
    let body = serde_json::to_value(&ResponsesRequest {
        model,
        input: prompt,
        stream: true,
    })
    .map_err(io::Error::other)?;
    let response = retry_send(client, &url, api_key, &body, &retry)?;
    let body_bytes = response.bytes().map_err(io::Error::other)?.to_vec();

    let mut reader = SseReader::new(body_bytes);
    let mut saw_text = false;
    while let Some(data) = reader.next_event()? {
        if let Some(delta) = extract_responses_text_delta(&data)? {
            if !delta.is_empty() {
                saw_text = true;
                on_chunk(CompletionChunkKind::Answer, delta)?;
            }
        }
    }

    if !saw_text {
        return Err(super::OpenAiError::EmptyResponse);
    }

    Ok(())
}
