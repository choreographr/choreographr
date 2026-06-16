use super::{
    AuthConfig, ChatAssistantToolUse, ChatCompletionsRequest, ChatCompletionsResponse,
    ChatCompletionsStreamOptions, ChatCompletionsStreamResponse, ChatRequestMessage, ChatToolCall,
    ChatToolDefinition, ChatTurnResult, CompletionChunkKind, ModelListResponse, OpenAiClient,
    RequestFormat, ResponsesRequest, ResponsesResponse, SseReader, endpoint_url,
    extract_responses_text_delta,
};
use serde::Deserialize;
use std::{future::Future, io};

async fn send_request<R>(request: reqwest::RequestBuilder) -> io::Result<R>
where
    R: for<'de> Deserialize<'de>,
{
    let response = send_request_raw(request).await?;
    response.json().await.map_err(io::Error::other)
}

async fn send_request_raw(request: reqwest::RequestBuilder) -> io::Result<reqwest::Response> {
    let response = request.send().await.map_err(io::Error::other)?;
    let status = response.status();
    if !status.is_success() {
        let body = response.text().await.unwrap_or_default();
        let trimmed_body = body.trim();
        let detail = if trimmed_body.is_empty() {
            format!("request failed with status {status}")
        } else {
            format!("request failed with status {status}: {trimmed_body}")
        };
        return Err(io::Error::new(io::ErrorKind::PermissionDenied, detail));
    }

    Ok(response)
}

#[derive(Debug, Clone, Copy)]
enum MaxTokensField {
    MaxTokens,
    MaxCompletionTokens,
}

fn chat_completions_max_tokens_field(config: &AuthConfig, model: &str) -> MaxTokensField {
    if config.base_url.contains("opencode.ai") || model == "big-pickle" {
        MaxTokensField::MaxTokens
    } else {
        MaxTokensField::MaxCompletionTokens
    }
}

impl OpenAiClient {
    pub async fn validate_and_list_models(&self) -> io::Result<Vec<String>> {
        let url = endpoint_url(&self.config.base_url, &self.config.model_list_path)?;
        let payload: ModelListResponse =
            send_request(self.http.get(&url).bearer_auth(self.config.api_key.trim())).await?;
        Ok(payload.data.into_iter().map(|model| model.id).collect())
    }

    pub async fn completion(&self, model: &str, prompt: &str) -> io::Result<String> {
        match self.config.request_format_for_model(model) {
            RequestFormat::Responses => {
                responses_request(&self.http, &self.config, model, prompt).await
            }
            RequestFormat::ChatCompletions => {
                chat_completions_request(&self.http, &self.config, model, prompt).await
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
                responses_request_streaming(&self.http, &self.config, model, prompt, &mut on_chunk)
                    .await
            }
            RequestFormat::ChatCompletions => {
                chat_completions_request_streaming(
                    &self.http,
                    &self.config,
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
        chat_completions_request_with_tools(&self.http, &self.config, model, messages, tools).await
    }
}

async fn responses_request(
    client: &reqwest::Client,
    config: &AuthConfig,
    model: &str,
    prompt: &str,
) -> io::Result<String> {
    let url = endpoint_url(&config.base_url, &config.responses_path)?;
    let payload: ResponsesResponse =
        send_request(client.post(&url).bearer_auth(config.api_key.trim()).json(
            &ResponsesRequest {
                model,
                input: prompt,
                stream: false,
            },
        ))
        .await?;

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
    config: &AuthConfig,
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
    let payload: ChatCompletionsResponse =
        send_request(client.post(&url).bearer_auth(config.api_key.trim()).json(
            &ChatCompletionsRequest {
                model,
                messages: vec![ChatRequestMessage {
                    role: "user",
                    content: Some(prompt.to_string()),
                    tool_call_id: None,
                    tool_calls: None,
                    reasoning_content: None,
                    reasoning: None,
                    reasoning_text: None,
                }],
                tools: None,
                stream: false,
                stream_options: None,
                max_tokens: max_tokens_field,
                max_completion_tokens: max_completion_tokens_field,
            },
        ))
        .await?;

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
    config: &AuthConfig,
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
    let payload: ChatCompletionsResponse =
        send_request(client.post(&url).bearer_auth(config.api_key.trim()).json(
            &ChatCompletionsRequest {
                model,
                messages: messages.to_vec(),
                tools: Some(tools.to_vec()),
                stream: false,
                stream_options: None,
                max_tokens: max_tokens_field,
                max_completion_tokens: max_completion_tokens_field,
            },
        ))
        .await?;

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
    config: &AuthConfig,
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
    let response = send_request_raw(client.post(&url).bearer_auth(config.api_key.trim()).json(
        &ChatCompletionsRequest {
            model,
            messages: vec![ChatRequestMessage {
                role: "user",
                content: Some(prompt.to_string()),
                tool_call_id: None,
                tool_calls: None,
                reasoning_content: None,
                reasoning: None,
                reasoning_text: None,
            }],
            tools: None,
            stream: true,
            stream_options: Some(ChatCompletionsStreamOptions {
                include_usage: true,
            }),
            max_tokens: max_tokens_field,
            max_completion_tokens: max_completion_tokens_field,
        },
    ))
    .await?;

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
    config: &AuthConfig,
    model: &str,
    prompt: &str,
    on_chunk: &mut F,
) -> io::Result<()>
where
    F: FnMut(CompletionChunkKind, String) -> Fut,
    Fut: Future<Output = io::Result<()>>,
{
    let url = endpoint_url(&config.base_url, &config.responses_path)?;
    let response = send_request_raw(client.post(&url).bearer_auth(config.api_key.trim()).json(
        &ResponsesRequest {
            model,
            input: prompt,
            stream: true,
        },
    ))
    .await?;

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
