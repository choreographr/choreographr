use bytes::Bytes;
use futures_util::{StreamExt, stream::BoxStream};
use serde::{Deserialize, Serialize};
use std::{collections::HashMap, fs, future::Future, io, path::PathBuf};

const DEFAULT_BASE_URL: &str = "https://api.openai.com/v1";
const DEFAULT_MODEL_LIST_PATH: &str = "/models";
const DEFAULT_RESPONSES_PATH: &str = "/responses";
const DEFAULT_CHAT_COMPLETIONS_PATH: &str = "/chat/completions";

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RequestFormat {
    Responses,
    ChatCompletions,
}

#[derive(Debug, Clone, Deserialize)]
pub struct AuthConfig {
    pub api_key: String,
    #[serde(default = "default_base_url")]
    pub base_url: String,
    #[serde(default = "default_model_list_path")]
    pub model_list_path: String,
    #[serde(default = "default_responses_path")]
    pub responses_path: String,
    #[serde(default = "default_chat_completions_path")]
    pub chat_completions_path: String,
    #[serde(default = "default_request_format")]
    pub default_request_format: RequestFormat,
    #[serde(default)]
    pub model_request_formats: HashMap<String, RequestFormat>,
    #[serde(default)]
    pub chat_completions_max_tokens: Option<u32>,
    #[serde(default)]
    pub model_max_tokens: HashMap<String, u32>,
    #[serde(default = "default_streaming")]
    pub streaming: bool,
}

fn default_base_url() -> String {
    DEFAULT_BASE_URL.to_string()
}

fn default_model_list_path() -> String {
    DEFAULT_MODEL_LIST_PATH.to_string()
}

fn default_responses_path() -> String {
    DEFAULT_RESPONSES_PATH.to_string()
}

fn default_chat_completions_path() -> String {
    DEFAULT_CHAT_COMPLETIONS_PATH.to_string()
}

fn default_request_format() -> RequestFormat {
    RequestFormat::ChatCompletions
}

fn default_streaming() -> bool {
    true
}

#[derive(Debug, Deserialize)]
struct ModelListResponse {
    data: Vec<ModelInfo>,
}

#[derive(Debug, Deserialize)]
struct ModelInfo {
    id: String,
}

#[derive(Debug, Serialize)]
struct ResponsesRequest<'a> {
    model: &'a str,
    input: &'a str,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    stream: bool,
}

#[derive(Debug, Deserialize)]
struct ResponsesResponse {
    output: Vec<OutputItem>,
}

#[derive(Debug, Deserialize)]
struct OutputItem {
    #[serde(default)]
    content: Vec<ContentItem>,
}

#[derive(Debug, Deserialize)]
struct ContentItem {
    text: Option<String>,
}

#[derive(Debug, Serialize)]
struct ChatCompletionsRequest<'a, M>
where
    M: Serialize,
{
    model: &'a str,
    messages: Vec<M>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tools: Option<Vec<ChatToolDefinition>>,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    stream: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    stream_options: Option<ChatCompletionsStreamOptions>,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_completion_tokens: Option<u32>,
}

#[derive(Debug, Serialize)]
struct ChatCompletionsStreamOptions {
    include_usage: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct ChatToolDefinition {
    #[serde(rename = "type")]
    kind: &'static str,
    function: ChatToolFunction,
}

#[derive(Debug, Clone, Serialize)]
struct ChatToolFunction {
    name: &'static str,
    description: &'static str,
    parameters: serde_json::Value,
}

#[derive(Debug, Clone, Serialize)]
pub struct ChatRequestMessage {
    pub role: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<AssistantToolCall>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning_content: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning_text: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AssistantToolCall {
    pub id: String,
    #[serde(rename = "type")]
    pub kind: String,
    pub function: AssistantToolFunction,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AssistantToolFunction {
    pub name: String,
    pub arguments: String,
}

#[derive(Debug, Deserialize)]
struct ChatCompletionsResponse {
    choices: Vec<Choice>,
}

#[derive(Debug, Deserialize)]
struct Choice {
    message: AssistantMessage,
}

#[derive(Debug, Deserialize)]
struct AssistantMessage {
    content: Option<String>,
    #[serde(default)]
    tool_calls: Vec<AssistantToolCall>,
    reasoning_content: Option<String>,
    reasoning: Option<String>,
    reasoning_text: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChatToolCall {
    pub id: String,
    pub name: String,
    pub arguments_json: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChatAssistantToolUse {
    pub content: Option<String>,
    pub tool_calls: Vec<ChatToolCall>,
    pub reasoning_content: Option<String>,
    pub reasoning: Option<String>,
    pub reasoning_text: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChatTurnResult {
    FinalText(String),
    ToolUse(ChatAssistantToolUse),
}

#[derive(Debug, Deserialize)]
struct ChatCompletionsStreamResponse {
    choices: Vec<StreamChoice>,
}

#[derive(Debug, Deserialize)]
struct StreamChoice {
    delta: Option<StreamDelta>,
}

#[derive(Debug, Deserialize)]
struct StreamDelta {
    content: Option<String>,
    reasoning_content: Option<String>,
    reasoning: Option<String>,
    reasoning_text: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompletionChunkKind {
    Answer,
    Reasoning,
}

impl ChatToolDefinition {
    pub fn function(
        name: &'static str,
        description: &'static str,
        parameters: serde_json::Value,
    ) -> Self {
        Self {
            kind: "function",
            function: ChatToolFunction {
                name,
                description,
                parameters,
            },
        }
    }
}

#[derive(Clone)]
pub struct OpenAiClient {
    config: AuthConfig,
    http: reqwest::Client,
}

pub fn auth_config_path() -> io::Result<PathBuf> {
    let config_dir = dirs::config_dir().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            "could not determine standard config directory",
        )
    })?;
    Ok(config_dir.join("tai-daemon").join("auth.toml"))
}

pub fn load_auth_config() -> io::Result<AuthConfig> {
    let path = auth_config_path()?;
    let raw = fs::read_to_string(&path).map_err(|error| {
        io::Error::new(
            error.kind(),
            format!("failed to read auth config at {}: {error}", path.display()),
        )
    })?;

    let config: AuthConfig = toml::from_str(&raw).map_err(|error| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("failed to parse auth config at {}: {error}", path.display()),
        )
    })?;

    if config.api_key.trim().is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "auth config at {} contains an empty api_key",
                path.display()
            ),
        ));
    }

    Ok(config)
}

fn endpoint_url(base_url: &str, path: &str) -> io::Result<String> {
    if !path.starts_with('/') {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "path must start with '/'",
        ));
    }
    Ok(format!("{}{}", base_url.trim_end_matches('/'), path))
}

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

impl AuthConfig {
    pub fn request_format_for_model(&self, model: &str) -> RequestFormat {
        self.model_request_formats
            .get(model)
            .copied()
            .unwrap_or(self.default_request_format)
    }

    pub fn max_tokens_for_model(&self, model: &str) -> Option<u32> {
        self.model_max_tokens
            .get(model)
            .copied()
            .or(self.chat_completions_max_tokens)
    }
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
    pub fn new(config: AuthConfig) -> io::Result<Self> {
        let http = reqwest::Client::builder()
            .build()
            .map_err(io::Error::other)?;
        Ok(Self { config, http })
    }

    pub fn config(&self) -> &AuthConfig {
        &self.config
    }

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

struct SseReader {
    stream: BoxStream<'static, Result<Bytes, reqwest::Error>>,
    pending: Vec<u8>,
    event_lines: Vec<String>,
    finished: bool,
}

impl SseReader {
    fn new(response: reqwest::Response) -> Self {
        Self {
            stream: response.bytes_stream().boxed(),
            pending: Vec::new(),
            event_lines: Vec::new(),
            finished: false,
        }
    }

    async fn next_event(&mut self) -> io::Result<Option<String>> {
        if self.finished {
            return Ok(None);
        }

        loop {
            if let Some(event) = self.drain_complete_event()? {
                return Ok(Some(event));
            }

            match self.stream.next().await {
                Some(chunk) => {
                    let chunk = chunk.map_err(io::Error::other)?;
                    self.pending.extend_from_slice(&chunk);
                }
                None => {
                    if !self.pending.is_empty() {
                        let line = String::from_utf8(std::mem::take(&mut self.pending))
                            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
                        self.event_lines
                            .push(line.trim_end_matches('\r').to_string());
                    }
                    self.finished = true;
                    return self.finish_event();
                }
            }
        }
    }

    fn drain_complete_event(&mut self) -> io::Result<Option<String>> {
        while let Some(line_end) = self.pending.iter().position(|byte| *byte == b'\n') {
            let mut line = self.pending.drain(..=line_end).collect::<Vec<_>>();
            if matches!(line.last(), Some(b'\n')) {
                line.pop();
            }
            if matches!(line.last(), Some(b'\r')) {
                line.pop();
            }

            if line.is_empty() {
                if let Some(event) = build_sse_event(&mut self.event_lines) {
                    if event == "[DONE]" {
                        self.finished = true;
                        return Ok(None);
                    }
                    return Ok(Some(event));
                }
                continue;
            }

            let line = String::from_utf8(line)
                .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
            self.event_lines.push(line);
        }

        Ok(None)
    }

    fn finish_event(&mut self) -> io::Result<Option<String>> {
        let Some(event) = build_sse_event(&mut self.event_lines) else {
            return Ok(None);
        };
        if event == "[DONE]" {
            return Ok(None);
        }
        Ok(Some(event))
    }
}

fn build_sse_event(event_lines: &mut Vec<String>) -> Option<String> {
    if event_lines.is_empty() {
        return None;
    }

    let data = event_lines
        .iter()
        .filter_map(|line| line.strip_prefix("data:"))
        .map(|value| value.trim_start())
        .collect::<Vec<_>>()
        .join("\n");
    event_lines.clear();

    if data.is_empty() { None } else { Some(data) }
}

fn extract_responses_text_delta(data: &str) -> io::Result<Option<String>> {
    let payload: serde_json::Value = serde_json::from_str(data).map_err(io::Error::other)?;
    let Some(event_type) = payload.get("type").and_then(|value| value.as_str()) else {
        return Ok(None);
    };

    let delta = match event_type {
        "response.output_text.delta" => payload.get("delta").and_then(|value| value.as_str()),
        "response.output_text.done" => None,
        _ => None,
    };

    Ok(delta.map(str::to_string))
}

pub async fn validate_and_list_models(config: &AuthConfig) -> io::Result<Vec<String>> {
    OpenAiClient::new(config.clone())?
        .validate_and_list_models()
        .await
}

pub async fn completion(config: &AuthConfig, model: &str, prompt: &str) -> io::Result<String> {
    OpenAiClient::new(config.clone())?
        .completion(model, prompt)
        .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_sse_event_joins_multiple_data_lines() {
        let mut lines = vec![
            "event: message".to_string(),
            "data: hello".to_string(),
            "data: world".to_string(),
        ];
        let event = build_sse_event(&mut lines).expect("event");
        assert_eq!(event, "hello\nworld");
        assert!(lines.is_empty());
    }

    #[test]
    fn build_sse_event_returns_done_marker() {
        let mut lines = vec!["data: [DONE]".to_string()];
        let event = build_sse_event(&mut lines).expect("event");
        assert_eq!(event, "[DONE]");
        assert!(lines.is_empty());
    }

    #[test]
    fn extracts_responses_text_delta() {
        let delta = extract_responses_text_delta(
            r#"{"type":"response.output_text.delta","delta":"hello"}"#,
        )
        .expect("extract")
        .expect("delta");
        assert_eq!(delta, "hello");
    }

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
