use super::ServiceConfig;
use super::{
    ChatAssistantToolUse, ChatCompletionsRequest, ChatCompletionsResponse,
    ChatCompletionsStreamOptions, ChatCompletionsStreamResponse, ChatRequestMessage, ChatToolCall,
    ChatToolDefinition, ChatTurnResult, CompletionChunkKind, ModelListResponse, OpenAiClient,
    RequestFormat, ResponsesRequest, ResponsesResponse, SseReader, endpoint_url,
    extract_responses_text_delta,
};
use serde::Deserialize;
use std::{future::Future, io, time::Duration};

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

fn status_to_error(status: reqwest::StatusCode, detail: &str) -> io::Error {
    let kind = match status {
        s if s.is_client_error() && s != reqwest::StatusCode::TOO_MANY_REQUESTS => {
            io::ErrorKind::InvalidInput
        }
        _ => io::ErrorKind::Other,
    };
    io::Error::new(kind, detail.to_string())
}

async fn send_request_raw(
    request: &reqwest::RequestBuilder,
    retry: &RetryConfig,
) -> io::Result<reqwest::Response> {
    let mut attempt: u32 = 0;
    loop {
        attempt += 1;

        let req = request.try_clone().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::Other,
                "request body cannot be cloned for retry",
            )
        })?;

        match req.send().await {
            Ok(response) => {
                let status = response.status();
                if status.is_success() {
                    return Ok(response);
                }

                if is_retryable_status(status) && attempt < retry.max_attempts {
                    let retry_after = parse_retry_after_secs(response.headers());
                    let body = response.text().await.unwrap_or_default();
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
                        %body,
                        delay_ms = delay.as_millis(),
                        "retrying request"
                    );
                    tokio::time::sleep(delay).await;
                    continue;
                }

                let body = response.text().await.unwrap_or_default();
                let trimmed_body = body.trim();
                let detail = if trimmed_body.is_empty() {
                    format!("request failed with status {status}")
                } else {
                    format!("request failed with status {status}: {trimmed_body}")
                };
                return Err(status_to_error(status, &detail));
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
                    tokio::time::sleep(delay).await;
                    continue;
                }
                return Err(io::Error::other(error));
            }
        }
    }
}

async fn send_request<R>(
    request: &reqwest::RequestBuilder,
    retry: &RetryConfig,
) -> io::Result<R>
where
    R: for<'de> Deserialize<'de>,
{
    let response = send_request_raw(request, retry).await?;
    response.json().await.map_err(io::Error::other)
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
    pub async fn validate_and_list_models(&self) -> io::Result<Vec<String>> {
        let url = endpoint_url(&self.config.base_url, &self.config.model_list_path)?;
        let retry = RetryConfig::from_service_config(&self.config);
        let request = self.http.get(&url).bearer_auth(self.api_key.trim());
        let payload: ModelListResponse = send_request(&request, &retry).await?;
        Ok(payload.data.into_iter().map(|model| model.id).collect())
    }

    pub async fn completion(&self, model: &str, prompt: &str) -> io::Result<String> {
        match self.config.request_format_for_model(model) {
            RequestFormat::Responses => {
                responses_request(&self.http, &self.config, &self.api_key, model, prompt).await
            }
            RequestFormat::ChatCompletions => {
                chat_completions_request(&self.http, &self.config, &self.api_key, model, prompt).await
            }
        }
    }

    pub async fn completion_stream<F, Fut>(
        &self,
        model: &str,
        prompt: &str,
        mut on_chunk: F,
    ) -> io::Result<()>
    where
        F: FnMut(CompletionChunkKind, String) -> Fut,
        Fut: Future<Output = io::Result<()>>,
    {
        if !self.config.streaming {
            let content = self.completion(model, prompt).await?;
            if !content.is_empty() {
                on_chunk(CompletionChunkKind::Answer, content).await?;
            }
            return Ok(());
        }

        match self.config.request_format_for_model(model) {
            RequestFormat::Responses => {
                responses_request_streaming(&self.http, &self.config, &self.api_key, model, prompt, &mut on_chunk)
                    .await
            }
            RequestFormat::ChatCompletions => {
                chat_completions_request_streaming(
                    &self.http,
                    &self.config,
                    &self.api_key,
                    model,
                    prompt,
                    &mut on_chunk,
                )
                .await
            }
        }
    }

    pub async fn chat_completion_turn(
        &self,
        model: &str,
        messages: &[ChatRequestMessage],
        tools: &[ChatToolDefinition],
    ) -> io::Result<ChatTurnResult> {
        chat_completions_request_with_tools(&self.http, &self.config, &self.api_key, model, messages, tools).await
    }
}

async fn responses_request(
    client: &reqwest::Client,
    config: &ServiceConfig,
    api_key: &str,
    model: &str,
    prompt: &str,
) -> io::Result<String> {
    let url = endpoint_url(&config.base_url, &config.responses_path)?;
    let retry = RetryConfig::from_service_config(config);
    let request = client.post(&url).bearer_auth(api_key.trim()).json(&ResponsesRequest {
        model,
        input: prompt,
        stream: false,
    });
    let payload: ResponsesResponse = send_request(&request, &retry).await?;

    let content = payload
        .output
        .into_iter()
        .flat_map(|item| item.content.into_iter())
        .filter_map(|item| item.text)
        .map(|text| text.trim().to_string())
        .find(|text| !text.is_empty())
        .unwrap_or_default();

    if content.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "provider returned an empty response",
        ));
    }

    Ok(content)
}

async fn chat_completions_request(
    client: &reqwest::Client,
    config: &ServiceConfig,
    api_key: &str,
    model: &str,
    prompt: &str,
) -> io::Result<String> {
    let url = endpoint_url(&config.base_url, &config.chat_completions_path)?;
    let max_tokens = config.max_tokens_for_model(model);
    let (max_tokens_field, max_completion_tokens_field) =
        match chat_completions_max_tokens_field(config, model) {
            MaxTokensField::MaxTokens => (max_tokens, None),
            MaxTokensField::MaxCompletionTokens => (None, max_tokens),
        };
    let retry = RetryConfig::from_service_config(config);
    let request = client.post(&url).bearer_auth(api_key.trim()).json(&ChatCompletionsRequest {
        model,
        messages: vec![ChatRequestMessage::simple("user", prompt.to_string())],
        tools: None,
        stream: false,
        stream_options: None,
        max_tokens: max_tokens_field,
        max_completion_tokens: max_completion_tokens_field,
    });
    let payload: ChatCompletionsResponse = send_request(&request, &retry).await?;

    let content = payload
        .choices
        .into_iter()
        .next()
        .and_then(|choice| choice.message.content)
        .unwrap_or_default()
        .trim()
        .to_string();

    if content.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "provider returned an empty response",
        ));
    }

    Ok(content)
}

async fn chat_completions_request_with_tools(
    client: &reqwest::Client,
    config: &ServiceConfig,
    api_key: &str,
    model: &str,
    messages: &[ChatRequestMessage],
    tools: &[ChatToolDefinition],
) -> io::Result<ChatTurnResult> {
    let url = endpoint_url(&config.base_url, &config.chat_completions_path)?;
    let max_tokens = config.max_tokens_for_model(model);
    let (max_tokens_field, max_completion_tokens_field) =
        match chat_completions_max_tokens_field(config, model) {
            MaxTokensField::MaxTokens => (max_tokens, None),
            MaxTokensField::MaxCompletionTokens => (None, max_tokens),
        };
    let retry = RetryConfig::from_service_config(config);
    let request = client.post(&url).bearer_auth(api_key.trim()).json(&ChatCompletionsRequest {
        model,
        messages: messages.to_vec(),
        tools: Some(tools.to_vec()),
        stream: false,
        stream_options: None,
        max_tokens: max_tokens_field,
        max_completion_tokens: max_completion_tokens_field,
    });
    let payload: ChatCompletionsResponse = send_request(&request, &retry).await?;

    let Some(choice) = payload.choices.into_iter().next() else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "provider returned an empty response",
        ));
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
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "provider returned an empty response",
        ));
    }

    Ok(ChatTurnResult::FinalText(content))
}

async fn chat_completions_request_streaming<F, Fut>(
    client: &reqwest::Client,
    config: &ServiceConfig,
    api_key: &str,
    model: &str,
    prompt: &str,
    on_chunk: &mut F,
) -> io::Result<()>
where
    F: FnMut(CompletionChunkKind, String) -> Fut,
    Fut: Future<Output = io::Result<()>>,
{
    let url = endpoint_url(&config.base_url, &config.chat_completions_path)?;
    let max_tokens = config.max_tokens_for_model(model);
    let (max_tokens_field, max_completion_tokens_field) =
        match chat_completions_max_tokens_field(config, model) {
            MaxTokensField::MaxTokens => (max_tokens, None),
            MaxTokensField::MaxCompletionTokens => (None, max_tokens),
        };
    let retry = RetryConfig::from_service_config(config);
    let request = client.post(&url).bearer_auth(api_key.trim()).json(
        &ChatCompletionsRequest {
            model,
            messages: vec![ChatRequestMessage::simple("user", prompt.to_string())],
            tools: None,
            stream: true,
            stream_options: Some(ChatCompletionsStreamOptions {
                include_usage: true,
            }),
            max_tokens: max_tokens_field,
            max_completion_tokens: max_completion_tokens_field,
        },
    );
    let response = send_request_raw(&request, &retry).await?;

    let mut reader = SseReader::new(response);
    let mut saw_text = false;
    while let Some(data) = reader.next_event().await? {
        let payload: ChatCompletionsStreamResponse =
            serde_json::from_str(&data).map_err(io::Error::other)?;
        for choice in payload.choices {
            let Some(delta) = choice.delta else {
                continue;
            };

            if let Some(content) = delta.content.filter(|content| !content.is_empty()) {
                saw_text = true;
                on_chunk(CompletionChunkKind::Answer, content).await?;
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
                on_chunk(CompletionChunkKind::Reasoning, reasoning).await?;
            }
        }
    }

    if !saw_text {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "provider returned an empty streamed response",
        ));
    }

    Ok(())
}

async fn responses_request_streaming<F, Fut>(
    client: &reqwest::Client,
    config: &ServiceConfig,
    api_key: &str,
    model: &str,
    prompt: &str,
    on_chunk: &mut F,
) -> io::Result<()>
where
    F: FnMut(CompletionChunkKind, String) -> Fut,
    Fut: Future<Output = io::Result<()>>,
{
    let url = endpoint_url(&config.base_url, &config.responses_path)?;
    let retry = RetryConfig::from_service_config(config);
    let request = client.post(&url).bearer_auth(api_key.trim()).json(&ResponsesRequest {
        model,
        input: prompt,
        stream: true,
    });
    let response = send_request_raw(&request, &retry).await?;

    let mut reader = SseReader::new(response);
    let mut saw_text = false;
    while let Some(data) = reader.next_event().await? {
        if let Some(delta) = extract_responses_text_delta(&data)?
            && !delta.is_empty()
        {
            saw_text = true;
            on_chunk(CompletionChunkKind::Answer, delta).await?;
        }
    }

    if !saw_text {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "provider returned an empty streamed response",
        ));
    }

    Ok(())
}
