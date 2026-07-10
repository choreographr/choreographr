mod requests;
#[cfg(test)]
mod tests;

use std::io;
use std::sync::mpsc;
use std::time::Duration;
use tracing::debug;

use serde::{Deserialize, Serialize};

use crate::openai::{
    ChatAssistantToolUse, ChatRequestMessage, ChatToolCall, ChatToolDefinition, ChatTurnResult,
    CompletionChunkKind, FinalTextResult,
};

/// Default base URL for the Gemini API.
const DEFAULT_BASE_URL: &str = "https://generativelanguage.googleapis.com/v1beta";

/// Configuration for the Google Gemini API client.
#[derive(Debug, Clone)]
pub struct GoogleConfig {
    pub base_url: String,
    pub streaming: bool,
    pub retry_max_attempts: u32,
    pub retry_initial_backoff_ms: u64,
    pub retry_max_backoff_ms: u64,
    pub connect_timeout_secs: u64,
    pub request_timeout_secs: u64,
}

impl Default for GoogleConfig {
    fn default() -> Self {
        Self {
            base_url: DEFAULT_BASE_URL.to_string(),
            streaming: true,
            retry_max_attempts: 5,
            retry_initial_backoff_ms: 1000,
            retry_max_backoff_ms: 30000,
            connect_timeout_secs: 30,
            request_timeout_secs: 120,
        }
    }
}

impl GoogleConfig {
    /// Apply account-level overrides onto this config.
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
    }
}

/// Errors from the Google Gemini API.
#[derive(Debug, thiserror::Error)]
pub enum GoogleError {
    #[error("unauthorized ({status}): {detail}")]
    Unauthorized { status: u16, detail: String },
    #[error("rate limited: {detail}")]
    RateLimited {
        retry_after_secs: Option<u64>,
        detail: String,
    },
    #[error("server error ({status}): {detail}")]
    ServerError { status: u16, detail: String },
    #[error("client error ({status}): {detail}")]
    ClientError { status: u16, detail: String },
    #[error("provider returned an empty response")]
    EmptyResponse,
    #[error("request cancelled during retry backoff")]
    Cancelled,
    #[error("{0}")]
    Io(#[from] std::io::Error),
}

impl From<crate::retry::ProviderHttpError> for GoogleError {
    fn from(err: crate::retry::ProviderHttpError) -> Self {
        match err {
            crate::retry::ProviderHttpError::Unauthorized { status, detail } => {
                GoogleError::Unauthorized { status, detail }
            }
            crate::retry::ProviderHttpError::RateLimited {
                retry_after_secs,
                detail,
            } => GoogleError::RateLimited {
                retry_after_secs,
                detail,
            },
            crate::retry::ProviderHttpError::ServerError { status, detail } => {
                GoogleError::ServerError { status, detail }
            }
            crate::retry::ProviderHttpError::ClientError { status, detail } => {
                GoogleError::ClientError { status, detail }
            }
            crate::retry::ProviderHttpError::EmptyResponse => GoogleError::EmptyResponse,
            crate::retry::ProviderHttpError::Cancelled => GoogleError::Cancelled,
            crate::retry::ProviderHttpError::Io(e) => GoogleError::Io(e),
        }
    }
}

/// Map a Google error to a stable label for metrics.
pub(crate) fn error_type_label(e: &GoogleError) -> &'static str {
    match e {
        GoogleError::Unauthorized { .. } => "unauthorized",
        GoogleError::RateLimited { .. } => "rate_limited",
        GoogleError::ServerError { .. } => "server_error",
        GoogleError::ClientError { .. } => "client_error",
        GoogleError::EmptyResponse => "empty_response",
        GoogleError::Cancelled => "cancelled",
        GoogleError::Io(_) => "other",
    }
}

/// The Google Gemini API client.
#[derive(Clone, Debug)]
pub struct GoogleClient {
    config: GoogleConfig,
    api_key: String,
    http: reqwest::blocking::Client,
}

impl GoogleClient {
    pub fn new(config: GoogleConfig, api_key: String) -> io::Result<Self> {
        let http = reqwest::blocking::Client::builder()
            .connect_timeout(Duration::from_secs(config.connect_timeout_secs))
            .timeout(Duration::from_secs(config.request_timeout_secs))
            .build()
            .map_err(io::Error::other)?;
        Ok(Self {
            config,
            api_key,
            http,
        })
    }

    pub fn config(&self) -> &GoogleConfig {
        &self.config
    }

    pub fn api_key(&self) -> &str {
        &self.api_key
    }

    /// List available models from the known Gemini models list.
    pub fn validate_and_list_models(&self) -> Result<Vec<String>, GoogleError> {
        Ok(KNOWN_GEMINI_MODELS.iter().map(|s| s.to_string()).collect())
    }

    /// Non-streaming chat completion turn via the Gemini generateContent API.
    #[allow(clippy::too_many_arguments)]
    pub fn chat_completion_turn(
        &self,
        model: &str,
        messages: &[ChatRequestMessage],
        tools: &[ChatToolDefinition],
        thinking_effort: ThinkingEffort,
        on_retry: &mut Option<crate::retry::RetryCallback>,
        cancel_rx: Option<&mpsc::Receiver<()>>,
    ) -> Result<ChatTurnResult, GoogleError> {
        debug!(effort = %thinking_effort.as_label(), "Google chat completion turn");
        requests::generate_content_request(
            &self.http,
            &self.config,
            &self.api_key,
            model,
            messages,
            tools,
            thinking_effort,
            on_retry,
            cancel_rx,
        )
    }

    /// Streaming chat completion turn via the Gemini streamGenerateContent API.
    #[allow(clippy::too_many_arguments)]
    pub fn chat_completion_turn_streaming<F>(
        &self,
        model: &str,
        messages: &[ChatRequestMessage],
        tools: &[ChatToolDefinition],
        thinking_effort: ThinkingEffort,
        on_retry: &mut Option<crate::retry::RetryCallback>,
        cancel_rx: Option<&mpsc::Receiver<()>>,
        on_chunk: F,
    ) -> Result<ChatTurnResult, GoogleError>
    where
        F: FnMut(CompletionChunkKind, String) -> io::Result<()>,
    {
        debug!(?thinking_effort, "google chat_completion_turn_streaming");
        if !self.config.streaming {
            let mut on_chunk = on_chunk;
            let result = self.chat_completion_turn(
                model,
                messages,
                tools,
                thinking_effort,
                on_retry,
                cancel_rx,
            )?;
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

        requests::generate_content_request_streaming(
            &self.http,
            &self.config,
            &self.api_key,
            model,
            messages,
            tools,
            thinking_effort,
            on_retry,
            cancel_rx,
            on_chunk,
        )
    }
}

// ── ProviderClient trait impl ───────────────────────────────────────────

use crate::providers::ProviderClient;
use tai_proto::{InferenceError, ThinkingEffort};

impl ProviderClient for GoogleClient {
    fn provider_slug(&self) -> &'static str {
        "google"
    }

    fn chat_completion_turn(
        &self,
        model: &str,
        messages: &[ChatRequestMessage],
        tools: &[ChatToolDefinition],
        thinking_effort: ThinkingEffort,
        on_retry: &mut Option<crate::retry::RetryCallback>,
        cancel_rx: Option<&mpsc::Receiver<()>>,
    ) -> Result<ChatTurnResult, InferenceError> {
        let api_start = std::time::Instant::now();
        let result =
            self.chat_completion_turn(model, messages, tools, thinking_effort, on_retry, cancel_rx);
        crate::providers::timed_result(
            api_start,
            model,
            "google",
            result,
            error_type_label,
            google_error_to_inference,
        )
    }

    fn chat_completion_turn_streaming(
        &self,
        model: &str,
        messages: &[ChatRequestMessage],
        tools: &[ChatToolDefinition],
        thinking_effort: ThinkingEffort,
        on_retry: &mut Option<crate::retry::RetryCallback>,
        cancel_rx: Option<&mpsc::Receiver<()>>,
        on_chunk: &mut dyn FnMut(CompletionChunkKind, String) -> io::Result<()>,
    ) -> Result<ChatTurnResult, InferenceError> {
        let api_start = std::time::Instant::now();
        let result = self.chat_completion_turn_streaming(
            model,
            messages,
            tools,
            thinking_effort,
            on_retry,
            cancel_rx,
            on_chunk,
        );
        crate::providers::timed_result(
            api_start,
            model,
            "google",
            result,
            error_type_label,
            google_error_to_inference,
        )
    }

    fn list_models(&self) -> Result<Vec<String>, InferenceError> {
        let result = self.validate_and_list_models();
        result.map_err(google_error_to_inference)
    }
}

fn google_error_to_inference(e: GoogleError) -> InferenceError {
    match e {
        GoogleError::Unauthorized { status, detail } => {
            InferenceError::Unauthorized { status, detail }
        }
        GoogleError::RateLimited {
            retry_after_secs,
            detail,
        } => InferenceError::RateLimited {
            retry_after_secs,
            detail,
        },
        GoogleError::ServerError { status, detail } => {
            InferenceError::ServerError { status, detail }
        }
        GoogleError::ClientError { status, detail } => {
            InferenceError::ClientError { status, detail }
        }
        GoogleError::EmptyResponse => InferenceError::EmptyResponse,
        GoogleError::Cancelled => InferenceError::Cancelled,
        GoogleError::Io(e) => InferenceError::Io(e.to_string()),
    }
}

/// Known Gemini models (curated static list).
const KNOWN_GEMINI_MODELS: &[&str] = &[
    "gemini-2.5-pro-exp-03-25",
    "gemini-2.5-pro",
    "gemini-2.5-flash-preview-05-06",
    "gemini-2.5-flash",
    "gemini-2.0-flash",
    "gemini-2.0-flash-lite",
    "gemini-2.0-flash-exp",
    "gemini-2.0-flash-thinking-exp-01-21",
    "gemini-2.0-flash-thinking-exp",
    "gemini-1.5-pro",
    "gemini-1.5-flash",
    "gemini-1.5-flash-8b",
    "gemini-1.5-flash-8b-exp-0827",
    "gemma-3-27b-it",
    "gemma-3-12b-it",
    "gemma-3-4b-it",
    "gemma-3-1b-it",
    "gemma-2-27b-it",
    "gemma-2-9b-it",
    "gemma-2-2b-it",
    "embedding-001",
    "text-embedding-004",
];

// ── API types ──────────────────────────────────────────────────────────

/// Request body for POST /v1beta/models/{model}:generateContent.
#[derive(Debug, Serialize)]
struct GenerateContentRequest<'a> {
    contents: Vec<ContentPayload<'a>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    system_instruction: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tools: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none", rename = "thinkingConfig")]
    thinking_config: Option<ThinkingConfigPayload>,
}

#[derive(Debug, Serialize)]
struct ThinkingConfigPayload {
    #[serde(rename = "includeThoughts")]
    include_thoughts: bool,
}

#[derive(Debug, Serialize)]
struct ContentPayload<'a> {
    role: &'a str,
    parts: Vec<PartPayload<'a>>,
}

#[derive(Debug, Serialize)]
#[serde(untagged)]
enum PartPayload<'a> {
    Text {
        text: &'a str,
    },
    FunctionCall {
        function_call: FunctionCallPayload<'a>,
    },
    FunctionResponse {
        function_response: FunctionResponsePayload<'a>,
    },
}

#[derive(Debug, Serialize)]
struct FunctionCallPayload<'a> {
    name: &'a str,
    args: serde_json::Value,
}

#[derive(Debug, Serialize)]
struct FunctionResponsePayload<'a> {
    name: &'a str,
    response: serde_json::Value,
}

/// Response body from POST /v1beta/models/{model}:generateContent.
#[derive(Debug, Deserialize)]
struct GenerateContentResponse {
    #[serde(default)]
    candidates: Vec<Candidate>,
}

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct Candidate {
    content: Option<ContentBlock>,
    #[serde(default)]
    finish_reason: Option<String>,
    #[serde(default)]
    index: i64,
}

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct ContentBlock {
    parts: Vec<ResponsePart>,
    #[serde(default)]
    role: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum ResponsePart {
    Text {
        text: String,
    },
    FunctionCall {
        #[serde(rename = "functionCall")]
        function_call: FunctionCallResponse,
    },
    Thinking {
        thinking: String,
        #[serde(default)]
        #[allow(dead_code)]
        signature: Option<String>,
    },
}

#[derive(Debug, Deserialize)]
struct FunctionCallResponse {
    name: String,
    #[serde(default)]
    args: serde_json::Value,
}

/// Gemini API error response body.
#[derive(Debug, Deserialize)]
pub(crate) struct GeminiErrorBody {
    #[serde(default)]
    pub(crate) error: Option<GeminiErrorDetail>,
}

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
pub(crate) struct GeminiErrorDetail {
    #[serde(default)]
    pub(crate) code: i64,
    #[serde(default)]
    pub(crate) message: String,
    #[serde(default)]
    pub(crate) status: String,
}

/// Gemini uses the REST pattern `models/{model}:{action}` rather than a path-segment
/// approach like OpenAI.  We append the action as a colon-delimited suffix to keep
/// the endpoint self-describing for different capabilities (generateContent vs
/// streamGenerateContent).
fn model_url(base_url: &str, model: &str, action: &str) -> io::Result<String> {
    let base = base_url.trim_end_matches('/');
    Ok(format!("{}/models/{}:{}", base, model, action))
}

/// Convert a list of messages into Gemini contents format.
///
/// System messages are collected into the `system_instruction` field and are
/// NOT added to `contents` because Gemini (unlike OpenAI) uses a separate
/// top-level field for system instructions, and putting them in contents would
/// cause a rejection.
/// Assistant messages use role "model" (Gemini's term for the model, matching
/// the API's role enum). Tool results use role "user" with functionResponse parts
/// because Gemini requires tool responses to be sent under the user role.
fn build_message_payloads<'a>(
    messages: &'a [ChatRequestMessage],
) -> (Vec<ContentPayload<'a>>, Option<String>) {
    let mut system_texts: Vec<String> = Vec::new();
    let mut payloads: Vec<ContentPayload<'a>> = Vec::new();

    for msg in messages {
        match msg.role {
            "system" => {
                if let Some(ref content) = msg.content {
                    system_texts.push(content.clone());
                }
            }
            "tool" => {
                let text = msg.content.as_deref().unwrap_or("");
                let name = msg.tool_call_id.as_deref().unwrap_or("");
                payloads.push(ContentPayload {
                    role: "user",
                    parts: vec![PartPayload::FunctionResponse {
                        function_response: FunctionResponsePayload {
                            name,
                            response: serde_json::json!({"content": text}),
                        },
                    }],
                });
            }
            "assistant" => {
                let mut parts: Vec<PartPayload<'a>> = Vec::new();
                if let Some(text) = msg.content.as_deref().filter(|t| !t.is_empty()) {
                    parts.push(PartPayload::Text { text });
                }
                if let Some(ref calls) = msg.tool_calls {
                    for tc in calls {
                        let args: serde_json::Value =
                            serde_json::from_str(&tc.function.arguments).unwrap_or_default();
                        parts.push(PartPayload::FunctionCall {
                            function_call: FunctionCallPayload {
                                name: &tc.function.name,
                                args,
                            },
                        });
                    }
                }
                payloads.push(ContentPayload {
                    role: "model",
                    parts,
                });
            }
            role => {
                let content = msg.content.as_deref().unwrap_or("");
                payloads.push(ContentPayload {
                    role,
                    parts: vec![PartPayload::Text { text: content }],
                });
            }
        }
    }

    let system_instruction = if system_texts.is_empty() {
        None
    } else {
        Some(system_texts.join("\n"))
    };

    (payloads, system_instruction)
}

/// Map tool definitions to Gemini's tool format.
///
/// Gemini wraps function declarations inside a `functionDeclarations` array per
/// tool entry (rather than OpenAI's flat `tools` array), because the Gemini API
/// supports non-function tool types (like code execution and retrieval) that
/// share the same tool wrapper.
fn build_tool_payloads(tools: &[ChatToolDefinition]) -> serde_json::Value {
    let declarations: Vec<serde_json::Value> = tools
        .iter()
        .map(|t| {
            serde_json::json!({
                "name": t.function.name,
                "description": t.function.description,
                "parameters": t.function.parameters,
            })
        })
        .collect();
    serde_json::json!([{"functionDeclarations": declarations}])
}

/// Map ThinkingEffort to Google's thinkingConfig.
/// Off → None (omitted from body). Anything else → includeThoughts: true.
fn thinking_config_payload(effort: ThinkingEffort) -> Option<ThinkingConfigPayload> {
    match effort {
        ThinkingEffort::Off => {
            debug!("Google thinking: Off, omitting thinkingConfig");
            None
        }
        _ => {
            debug!("Google thinking enabled with effort={}", effort.as_label());
            Some(ThinkingConfigPayload {
                include_thoughts: true,
            })
        }
    }
}

/// Convert a non-streaming Gemini response into a ChatTurnResult.
fn response_to_turn_result(
    response: GenerateContentResponse,
) -> Result<ChatTurnResult, GoogleError> {
    let candidate = response
        .candidates
        .into_iter()
        .next()
        .ok_or(GoogleError::EmptyResponse)?;

    let content = candidate.content.ok_or(GoogleError::EmptyResponse)?;

    let mut text_parts: Vec<String> = Vec::new();
    let mut tool_calls: Vec<ChatToolCall> = Vec::new();
    let mut reasoning_parts: Vec<String> = Vec::new();

    for part in content.parts {
        match part {
            ResponsePart::Text { text } => {
                text_parts.push(text);
            }
            ResponsePart::FunctionCall { function_call } => {
                let id = format!("fc_{}", function_call.name);
                let args_json = function_call.args.to_string();
                tool_calls.push(ChatToolCall {
                    id,
                    name: function_call.name,
                    arguments_json: args_json,
                });
            }
            ResponsePart::Thinking { thinking, .. } => {
                reasoning_parts.push(thinking);
            }
        }
    }

    let reasoning = if reasoning_parts.is_empty() {
        None
    } else {
        Some(reasoning_parts.join("\n"))
    };

    if !tool_calls.is_empty() {
        let content = if text_parts.is_empty() {
            None
        } else {
            Some(text_parts.join(""))
        };
        return Ok(ChatTurnResult::ToolUse(ChatAssistantToolUse {
            content,
            tool_calls,
            reasoning,
        }));
    }

    let content = text_parts.join("");
    if content.is_empty() {
        return Err(GoogleError::EmptyResponse);
    }

    Ok(ChatTurnResult::FinalText(FinalTextResult {
        content,
        reasoning,
    }))
}
