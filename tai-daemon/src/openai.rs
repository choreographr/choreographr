#[path = "openai_config.rs"]
mod openai_config;
#[path = "openai_requests.rs"]
mod openai_requests;
#[path = "openai_sse.rs"]
mod openai_sse;
#[cfg(test)]
#[path = "openai_tests.rs"]
mod openai_tests;

pub(crate) use openai_config::endpoint_url;
pub use openai_config::{
    AuthConfig, auth_config_path, completion, load_auth_config, validate_and_list_models,
};
#[cfg(test)]
pub(crate) use openai_sse::build_sse_event;
pub(crate) use openai_sse::{SseReader, extract_responses_text_delta};

use serde::{Deserialize, Serialize};
use std::io;

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
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
}
