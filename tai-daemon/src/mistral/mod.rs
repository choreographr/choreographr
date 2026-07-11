mod requests;
#[cfg(test)]
mod tests;

use std::io;
use tracing::debug;

use serde::{Deserialize, Serialize};

use crate::openai::{
    ChatAssistantToolUse, ChatRequestMessage, ChatToolCall, ChatToolDefinition, ChatTurnResult,
    CompletionChunkKind, FinalTextResult,
};
use crate::providers::ChatTurnRequest;
use tai_proto::TokenUsage;

const DEFAULT_BASE_URL: &str = "https://api.mistral.ai/v1";
const DEFAULT_MAX_TOKENS: u32 = 4096;

#[derive(Debug, Clone)]
pub struct MistralConfig {
    pub base_url: String,
    pub max_tokens: u32,
    pub streaming: bool,
    pub retry_max_attempts: u32,
    pub retry_initial_backoff_ms: u64,
    pub retry_max_backoff_ms: u64,
    pub connect_timeout_secs: u64,
    pub request_timeout_secs: u64,
}

impl Default for MistralConfig {
    fn default() -> Self {
        Self {
            base_url: DEFAULT_BASE_URL.to_string(),
            max_tokens: DEFAULT_MAX_TOKENS,
            streaming: true,
            retry_max_attempts: 5,
            retry_initial_backoff_ms: 1000,
            retry_max_backoff_ms: 30000,
            connect_timeout_secs: 30,
            request_timeout_secs: 120,
        }
    }
}

impl MistralConfig {
    pub fn apply_overrides(&mut self, cfg: &crate::accounts::AccountConfig) {
        if let Some(base_url) = &cfg.base_url {
            self.base_url = base_url.clone();
        }
        if let Some(streaming) = cfg.streaming {
            self.streaming = streaming;
        }
        if let Some(retry) = cfg.retry_max_attempts {
            self.retry_max_attempts = retry;
        }
        if let Some(connect) = cfg.connect_timeout_secs {
            self.connect_timeout_secs = connect;
        }
        if let Some(request) = cfg.request_timeout_secs {
            self.request_timeout_secs = request;
        }
        if let Some(ms) = cfg.retry_initial_backoff_ms {
            self.retry_initial_backoff_ms = ms;
        }
        if let Some(ms) = cfg.retry_max_backoff_ms {
            self.retry_max_backoff_ms = ms;
        }
    }
}

// Error type is re-exported from the shared provider infrastructure.
pub use crate::providers::shared::ProviderError as MistralError;

#[derive(Clone, Debug)]
pub struct MistralClient {
    config: MistralConfig,
    api_key: String,
    http: reqwest::blocking::Client,
}

// ── ProviderClient trait impl ───────────────────────────────────────────

use crate::providers::ProviderClient;
use tai_proto::{InferenceError, ThinkingEffort};

impl ProviderClient for MistralClient {
    fn provider_slug(&self) -> &'static str {
        "mistral"
    }

    fn chat_completion_turn(
        &self,
        params: ChatTurnRequest<'_>,
    ) -> Result<ChatTurnResult, InferenceError> {
        let api_start = std::time::Instant::now();
        let model = params.model;
        let result = self.chat_completion_turn(params);
        crate::providers::shared::timed_result(api_start, model, "mistral", result)
    }

    fn chat_completion_turn_streaming(
        &self,
        params: ChatTurnRequest<'_>,
        on_chunk: &mut dyn FnMut(CompletionChunkKind, String) -> io::Result<()>,
    ) -> Result<ChatTurnResult, InferenceError> {
        let api_start = std::time::Instant::now();
        let model = params.model;
        let result = self.chat_completion_turn_streaming(params, on_chunk);
        crate::providers::shared::timed_result(api_start, model, "mistral", result)
    }

    fn list_models(&self) -> Result<Vec<String>, InferenceError> {
        let result = self.validate_and_list_models();
        result.map_err(crate::providers::shared::provider_error_to_inference)
    }
}

impl MistralClient {
    pub fn new(config: MistralConfig, api_key: String) -> io::Result<Self> {
        let http = crate::providers::shared::build_http_client(
            config.connect_timeout_secs,
            config.request_timeout_secs,
        )?;
        Ok(Self {
            config,
            api_key,
            http,
        })
    }

    pub fn config(&self) -> &MistralConfig {
        &self.config
    }

    pub fn api_key(&self) -> &str {
        &self.api_key
    }

    pub fn validate_and_list_models(&self) -> Result<Vec<String>, MistralError> {
        crate::providers::list_models_with_fallback(
            || requests::list_models_request(&self.http, &self.config, &self.api_key),
            KNOWN_MISTRAL_MODELS,
            "Mistral",
        )
    }

    pub fn chat_completion_turn(
        &self,
        params: ChatTurnRequest<'_>,
    ) -> Result<ChatTurnResult, MistralError> {
        debug!(
            effort = %params.thinking_effort.as_label(),
            "Mistral chat completion turn"
        );
        requests::chat_completion_request(
            &self.http,
            &self.config,
            &self.api_key,
            params.model,
            params.messages,
            params.tools,
            params.thinking_effort,
            params.on_retry,
            params.cancel_rx,
        )
    }

    pub fn chat_completion_turn_streaming<F>(
        &self,
        params: ChatTurnRequest<'_>,
        on_chunk: F,
    ) -> Result<ChatTurnResult, MistralError>
    where
        F: FnMut(CompletionChunkKind, String) -> io::Result<()>,
    {
        debug!(?params.thinking_effort, "mistral chat_completion_turn_streaming");
        if !self.config.streaming {
            let mut on_chunk = on_chunk;
            let result = self.chat_completion_turn(params)?;
            match &result {
                ChatTurnResult::FinalText(final_text) => {
                    if !final_text.content.is_empty() {
                        on_chunk(CompletionChunkKind::Answer, final_text.content.clone())?;
                    }
                    if let Some(reasoning) = final_text.reasoning.as_ref().filter(|r| !r.is_empty())
                    {
                        on_chunk(CompletionChunkKind::Reasoning, reasoning.clone())?;
                    }
                }
                ChatTurnResult::ToolUse(tool_use) => {
                    if let Some(ref content) = tool_use.content
                        && !content.is_empty()
                    {
                        on_chunk(CompletionChunkKind::Answer, content.clone())?;
                    }
                    if let Some(reasoning) = tool_use.reasoning.as_ref().filter(|r| !r.is_empty()) {
                        on_chunk(CompletionChunkKind::Reasoning, reasoning.clone())?;
                    }
                }
            }
            return Ok(result);
        }

        requests::chat_completion_request_streaming(
            &self.http,
            &self.config,
            &self.api_key,
            params.model,
            params.messages,
            params.tools,
            params.thinking_effort,
            params.on_retry,
            params.cancel_rx,
            on_chunk,
        )
    }
}

// ── API types ──────────────────────────────────────────────────────────

#[derive(Debug, Serialize)]
struct ChatCompletionRequest<'a> {
    model: &'a str,
    messages: Vec<MessagePayload<'a>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tools: Option<Vec<ToolPayload<'a>>>,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    stream: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    reasoning_effort: Option<&'a str>,
}

#[derive(Debug, Serialize)]
struct MessagePayload<'a> {
    role: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    content: Option<MessageContent<'a>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_calls: Option<Vec<ToolCallPayload<'a>>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_call_id: Option<&'a str>,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    prefix: bool,
}

#[derive(Debug, Serialize)]
#[serde(untagged)]
enum MessageContent<'a> {
    Text(&'a str),
    #[serde(skip_serializing)]
    _Parts(Vec<ContentChunk<'a>>),
}

#[derive(Debug, Serialize)]
struct ContentChunk<'a> {
    r#type: &'a str,
    text: &'a str,
}

#[derive(Debug, Serialize)]
struct ToolCallPayload<'a> {
    id: &'a str,
    #[serde(rename = "type")]
    kind: &'a str,
    function: ToolCallFunctionPayload<'a>,
}

#[derive(Debug, Serialize)]
struct ToolCallFunctionPayload<'a> {
    name: &'a str,
    arguments: &'a str,
}

#[derive(Debug, Serialize)]
struct ToolPayload<'a> {
    #[serde(rename = "type")]
    kind: &'a str,
    function: ToolFunctionPayload<'a>,
}

#[derive(Debug, Serialize)]
struct ToolFunctionPayload<'a> {
    name: &'a str,
    description: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    parameters: Option<serde_json::Value>,
}

#[derive(Debug, Deserialize)]
struct ChatCompletionResponse {
    #[serde(rename = "id")]
    _id: String,
    choices: Vec<Choice>,
    #[serde(default, rename = "usage")]
    usage: Option<UsageInfo>,
}

#[derive(Debug, Deserialize)]
struct Choice {
    #[serde(rename = "index")]
    _index: u64,
    message: AssistantMessageResponse,
    #[serde(default, rename = "finish_reason")]
    _finish_reason: Option<String>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct AssistantMessageResponse {
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    tool_calls: Vec<ToolCallResponse>,
}

#[derive(Debug, Deserialize)]
struct ToolCallResponse {
    #[serde(rename = "id")]
    _id: String,
    #[serde(rename = "type")]
    _kind: String,
    function: ToolCallFunctionResponse,
}

#[derive(Debug, Deserialize)]
struct ToolCallFunctionResponse {
    name: String,
    arguments: String,
}

#[derive(Debug, Deserialize)]
pub(crate) struct UsageInfo {
    #[serde(default, rename = "prompt_tokens")]
    prompt_tokens: u32,
    #[serde(default, rename = "completion_tokens")]
    completion_tokens: u32,
    #[serde(default, rename = "total_tokens")]
    total_tokens: u32,
}

// Streaming types

#[derive(Debug, Deserialize)]
struct CompletionChunk {
    #[serde(default)]
    choices: Vec<StreamChoice>,
}

#[derive(Debug, Deserialize)]
struct StreamChoice {
    #[serde(rename = "index")]
    _index: u64,
    #[serde(default)]
    delta: StreamDelta,
    #[serde(default, rename = "finish_reason")]
    _finish_reason: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct StreamDelta {
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    tool_calls: Option<Vec<StreamToolCallDelta>>,
}

#[derive(Debug, Default, Deserialize)]
struct StreamToolCallDelta {
    index: u64,
    id: Option<String>,
    #[serde(rename = "type")]
    _kind: Option<String>,
    function: Option<StreamToolCallFunctionDelta>,
}

#[derive(Debug, Default, Deserialize)]
struct StreamToolCallFunctionDelta {
    name: Option<String>,
    arguments: Option<String>,
}

// Known Mistral models

const KNOWN_MISTRAL_MODELS: &[&str] = &[
    "mistral-large-latest",
    "mistral-large-2411",
    "mistral-medium-latest",
    "mistral-small-latest",
    "mistral-small-2509",
    "codestral-latest",
    "ministral-8b-latest",
    "ministral-3b-latest",
    "open-mistral-nemo",
    "open-codestral-mamba",
];

// Message conversion functions

fn build_message_payloads<'a>(messages: &'a [ChatRequestMessage]) -> Vec<MessagePayload<'a>> {
    let mut payloads: Vec<MessagePayload> = Vec::new();

    for msg in messages {
        match msg.role {
            "system" => {
                let text = msg.content.as_deref().unwrap_or("");
                payloads.push(MessagePayload {
                    role: "system",
                    content: Some(MessageContent::Text(text)),
                    tool_calls: None,
                    tool_call_id: None,
                    prefix: false,
                });
            }
            "tool" => {
                let text = msg.content.as_deref().unwrap_or("");
                payloads.push(MessagePayload {
                    role: "tool",
                    content: Some(MessageContent::Text(text)),
                    tool_calls: None,
                    tool_call_id: msg.tool_call_id.as_deref(),
                    prefix: false,
                });
            }
            "assistant" => {
                let content = msg
                    .content
                    .as_deref()
                    .filter(|t| !t.is_empty())
                    .map(MessageContent::Text);
                let tool_calls = msg.tool_calls.as_ref().map(|calls| {
                    calls
                        .iter()
                        .map(|tc| ToolCallPayload {
                            id: &tc.id,
                            kind: "function",
                            function: ToolCallFunctionPayload {
                                name: &tc.function.name,
                                arguments: &tc.function.arguments,
                            },
                        })
                        .collect()
                });
                payloads.push(MessagePayload {
                    role: "assistant",
                    content,
                    tool_calls,
                    tool_call_id: None,
                    prefix: false,
                });
            }
            role => {
                let text = msg.content.as_deref().unwrap_or("");
                payloads.push(MessagePayload {
                    role,
                    content: Some(MessageContent::Text(text)),
                    tool_calls: None,
                    tool_call_id: None,
                    prefix: false,
                });
            }
        }
    }

    payloads
}

fn build_tool_payloads(tools: &[ChatToolDefinition]) -> Vec<ToolPayload<'_>> {
    tools
        .iter()
        .map(|t| ToolPayload {
            kind: "function",
            function: ToolFunctionPayload {
                name: t.function.name,
                description: t.function.description,
                parameters: Some(t.function.parameters.clone()),
            },
        })
        .collect()
}

fn response_to_turn_result(
    response: ChatCompletionResponse,
) -> Result<ChatTurnResult, MistralError> {
    let choice = response
        .choices
        .into_iter()
        .next()
        .ok_or(MistralError::EmptyResponse)?;

    let msg = choice.message;

    let content = msg.content.unwrap_or_default();
    let tool_calls: Vec<ChatToolCall> = msg
        .tool_calls
        .into_iter()
        .map(|tc| ChatToolCall {
            id: tc._id,
            name: tc.function.name,
            arguments_json: tc.function.arguments,
        })
        .collect();

    // Extract token usage from the response for cost tracking / display.
    let usage: Option<TokenUsage> = response.usage.map(|u| {
        let usage = TokenUsage {
            input_tokens: u.prompt_tokens,
            output_tokens: u.completion_tokens,
            total_tokens: u.total_tokens,
        };
        debug!(
            input_tokens = usage.input_tokens,
            output_tokens = usage.output_tokens,
            total_tokens = usage.total_tokens,
            "Mistral turn usage"
        );
        usage
    });

    if !tool_calls.is_empty() {
        let text = if content.is_empty() {
            None
        } else {
            Some(content)
        };
        return Ok(ChatTurnResult::ToolUse(ChatAssistantToolUse {
            content: text,
            tool_calls,
            reasoning: None,
            usage,
        }));
    }

    if content.is_empty() {
        return Err(MistralError::EmptyResponse);
    }

    Ok(ChatTurnResult::FinalText(FinalTextResult {
        content,
        reasoning: None,
        usage,
    }))
}

/// Map ThinkingEffort to Mistral's reasoning_effort string value.
/// Off → None (omit the field from the request).
fn thinking_payload(effort: ThinkingEffort) -> Option<&'static str> {
    match effort {
        ThinkingEffort::Off => None,
        ThinkingEffort::Low => Some("low"),
        ThinkingEffort::Medium => Some("medium"),
        ThinkingEffort::High => Some("high"),
    }
}
