mod config;
mod requests;
mod retry;
mod sse;
#[cfg(test)]
mod tests;
pub use crate::providers::shared::MaxTokensField;

pub(crate) use config::endpoint_url;
// Re-export deprecated load_service_config for backward compatibility
// with any existing callers (e.g., external code) that may still use it.
#[allow(deprecated)]
pub use config::{
    DaemonConfig, ServiceConfig, completion, config_path, load_daemon_config, load_service_config,
    validate_and_list_models,
};
pub(crate) use sse::SseReader;
#[cfg(test)]
pub(crate) use sse::build_sse_event;
#[cfg(test)]
pub(crate) use sse::extract_responses_text_delta;
pub(crate) use sse::{ResponsesStreamEvent, parse_responses_stream_event};

#[cfg(test)]
pub(crate) use crate::retry::{
    RetryConfig, backoff_duration, is_retryable_status, parse_retry_after_secs,
};
pub use retry::RetryCallback;

use serde::{Deserialize, Serialize};
use std::io;
use tai_proto::TokenUsage;

/// Re-export the shared provider error type so all OpenAI code continues to
/// use `super::OpenAiError` without structural changes.
pub use crate::providers::shared::ProviderError as OpenAiError;

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RequestFormat {
    Responses,
    ChatCompletions,
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
#[serde(tag = "type", rename_all = "snake_case")]
pub(crate) enum ResponsesInputItem {
    Message {
        role: String,
        content: String,
    },
    #[serde(rename = "function_call_output")]
    FunctionCallOutput {
        call_id: String,
        output: String,
    },
}

#[derive(Debug, Serialize)]
struct ResponsesRequest<'a> {
    model: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    input: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    instructions: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tools: Option<Vec<ResponsesTool>>,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    stream: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_output_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    reasoning_effort: Option<&'a str>,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    store: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    previous_response_id: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    include: Option<Vec<&'a str>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    parallel_tool_calls: Option<bool>,
}

#[derive(Debug, Deserialize)]
struct ResponsesResponse {
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    status: Option<String>,
    #[serde(default)]
    output: Vec<ResponseOutputItem>,
    #[serde(default)]
    usage: Option<Usage>,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ResponseOutputItem {
    Message {
        #[serde(default)]
        content: Vec<ResponseContentPart>,
        #[serde(default)]
        role: Option<String>,
    },
    Reasoning {
        #[serde(default)]
        summary: Vec<serde_json::Value>,
    },
    #[serde(rename = "function_call")]
    FunctionCall {
        call_id: String,
        name: String,
        arguments: String,
    },
}

#[derive(Debug, Deserialize)]
struct ResponseContentPart {
    #[serde(rename = "type")]
    kind: String,
    text: Option<String>,
}

#[derive(Debug, Serialize)]
pub(crate) struct ResponsesTool {
    #[serde(rename = "type")]
    kind: String,
    name: String,
    description: String,
    parameters: serde_json::Value,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    strict: bool,
}

impl From<&ChatToolDefinition> for ResponsesTool {
    fn from(tool: &ChatToolDefinition) -> Self {
        Self {
            kind: "function".to_string(),
            name: tool.function.name.to_string(),
            description: tool.function.description.to_string(),
            parameters: tool.function.parameters.clone(),
            strict: false,
        }
    }
}

#[derive(Debug, Serialize)]
struct ChatCompletionsRequest<'a, M>
where
    M: Serialize,
{
    model: &'a str,
    #[serde(bound(serialize = "M: Serialize"))]
    messages: &'a [M],
    #[serde(skip_serializing_if = "Option::is_none")]
    tools: Option<&'a [ChatToolDefinition]>,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    stream: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    stream_options: Option<ChatCompletionsStreamOptions>,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_completion_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    reasoning_effort: Option<&'a str>,
}

#[derive(Debug, Serialize)]
struct ChatCompletionsStreamOptions {
    include_usage: bool,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct Usage {
    prompt_tokens: u32,
    completion_tokens: u32,
    total_tokens: u32,
}

#[derive(Debug, Clone, Serialize)]
pub struct ChatToolDefinition {
    #[serde(rename = "type")]
    kind: &'static str,
    pub(crate) function: ChatToolFunction,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct ChatToolFunction {
    pub(crate) name: &'static str,
    pub(crate) description: &'static str,
    pub(crate) parameters: serde_json::Value,
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

impl ChatRequestMessage {
    pub fn simple(role: &'static str, content: String) -> Self {
        ChatRequestMessage {
            role,
            content: Some(content),
            tool_call_id: None,
            tool_calls: None,
            reasoning_content: None,
            reasoning: None,
            reasoning_text: None,
        }
    }
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
    usage: Option<Usage>,
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

impl AssistantMessage {
    /// Extract reasoning content from whichever field the model populated
    /// (reasoning_content, reasoning, or reasoning_text).
    fn take_reasoning(&mut self) -> Option<String> {
        self.reasoning_content
            .take()
            .or_else(|| self.reasoning.take())
            .or_else(|| self.reasoning_text.take())
    }
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
    pub reasoning: Option<String>,
    pub usage: Option<TokenUsage>,
    pub response_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FinalTextResult {
    pub content: String,
    pub reasoning: Option<String>,
    pub usage: Option<TokenUsage>,
    pub response_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChatTurnResult {
    FinalText(FinalTextResult),
    ToolUse(ChatAssistantToolUse),
}

#[derive(Debug, Deserialize)]
struct ChatCompletionsStreamResponse {
    choices: Vec<StreamChoice>,
    #[serde(default)]
    usage: Option<Usage>,
}

#[derive(Debug, Deserialize)]
struct StreamChoice {
    delta: Option<StreamDelta>,
}

#[derive(Debug, Deserialize)]
struct StreamDelta {
    content: Option<String>,
    tool_calls: Option<Vec<StreamToolCallDelta>>,
    reasoning_content: Option<String>,
    reasoning: Option<String>,
    reasoning_text: Option<String>,
}

#[derive(Debug, Default, Clone, Deserialize)]
struct StreamToolCallDelta {
    index: u32,
    id: Option<String>,
    // Deserialised from the API's "type" field but never read in Rust — kept
    // so serde doesn't choke on unknown fields and to document the wire format.
    #[allow(dead_code)]
    #[serde(rename = "type")]
    kind: Option<String>,
    function: Option<StreamToolCallFunctionDelta>,
}

#[derive(Debug, Default, Clone, Deserialize)]
struct StreamToolCallFunctionDelta {
    name: Option<String>,
    arguments: Option<String>,
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

#[derive(Clone, Debug)]
pub struct OpenAiClient {
    config: ServiceConfig,
    api_key: String,
    http: ureq::Agent,
}

impl OpenAiClient {
    pub fn new(config: ServiceConfig, api_key: String) -> io::Result<Self> {
        let http = crate::providers::shared::build_agent(
            config.connect_timeout_secs,
            config.request_timeout_secs,
        );
        Ok(Self {
            config,
            api_key,
            http,
        })
    }

    pub fn config(&self) -> &ServiceConfig {
        &self.config
    }

    pub fn api_key(&self) -> &str {
        &self.api_key
    }
}

/// Map ThinkingEffort to the OpenAI `reasoning_effort` API string value.
/// Returns None for Off (parameter should be omitted).
pub(crate) fn reasoning_effort_api_value(effort: ThinkingEffort) -> Option<&'static str> {
    match effort {
        ThinkingEffort::Off => None,
        ThinkingEffort::Low => Some("low"),
        ThinkingEffort::Medium => Some("medium"),
        ThinkingEffort::High => Some("high"),
    }
}

/// Convert ChatRequestMessage slice to Responses API input format.
/// Returns (instructions, input_items) where instructions is the system prompt
/// extracted from system-role messages.
pub(crate) fn messages_to_responses_input(
    messages: &[ChatRequestMessage],
) -> (Option<String>, Vec<ResponsesInputItem>) {
    let mut instructions = None;
    let mut items = Vec::new();

    for msg in messages {
        match msg.role {
            "system" => {
                if let Some(ref content) = msg.content {
                    tracing::debug!(
                        "extracted instructions from system message (len={})",
                        content.len()
                    );
                    instructions = Some(content.clone());
                }
            }
            "user" | "assistant" => {
                if let Some(ref content) = msg.content {
                    items.push(ResponsesInputItem::Message {
                        role: msg.role.to_string(),
                        content: content.clone(),
                    });
                }
            }
            "tool" => {
                // Tool results become function_call_output items
                if let Some(ref call_id) = msg.tool_call_id
                    && let Some(ref content) = msg.content
                {
                    items.push(ResponsesInputItem::FunctionCallOutput {
                        call_id: call_id.clone(),
                        output: content.clone(),
                    });
                }
            }
            _ => {
                tracing::warn!(
                    "unexpected message role in messages_to_responses_input: {}",
                    msg.role
                );
            }
        }
    }

    tracing::debug!(
        "messages_to_responses_input: {} items, instructions={}",
        items.len(),
        instructions.is_some()
    );

    (instructions, items)
}

// ── ProviderClient trait impl ───────────────────────────────────────────

use crate::providers::{ChatTurnRequest, ProviderClient};
use tai_proto::{InferenceError, ThinkingEffort};

impl ProviderClient for OpenAiClient {
    fn provider_slug(&self) -> &'static str {
        "openai"
    }

    fn chat_completion_turn(
        &self,
        params: ChatTurnRequest<'_>,
    ) -> Result<ChatTurnResult, InferenceError> {
        let api_start = std::time::Instant::now();
        let model = params.model;
        let result = self.chat_completion_turn(params);
        crate::providers::shared::timed_result(api_start, model, "openai", result)
    }

    fn chat_completion_turn_streaming(
        &self,
        params: ChatTurnRequest<'_>,
        on_chunk: &mut dyn FnMut(CompletionChunkKind, String) -> io::Result<()>,
    ) -> Result<ChatTurnResult, InferenceError> {
        let api_start = std::time::Instant::now();
        let model = params.model;
        let result = self.chat_completion_turn_streaming(params, on_chunk);
        crate::providers::shared::timed_result(api_start, model, "openai", result)
    }

    fn list_models(&self) -> Result<Vec<String>, InferenceError> {
        let result = self.validate_and_list_models();
        result.map_err(crate::providers::shared::provider_error_to_inference)
    }
}
