mod requests;
#[cfg(test)]
mod tests;

use std::io;
use tracing::debug;

use serde::{Deserialize, Serialize};

use crate::openai::{ChatRequestMessage, ChatToolDefinition};
use crate::providers::ChatTurnRequest;
use crate::providers::StreamEvent;
use crate::providers::types::{
    ChatAssistantToolUse, ChatToolCall, ChatTurnResult, FinalTextResult,
};
use tai_proto::TokenUsage;

/// Default base URL for the Gemini API.
const DEFAULT_BASE_URL: &str = "https://generativelanguage.googleapis.com/v1beta";

/// Configuration for the Google Gemini API client.
#[derive(Debug, Clone)]
pub struct GoogleConfig {
    pub base_url: String,
    pub context_window_config: crate::providers::ContextWindowConfig,
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
            context_window_config: crate::providers::ContextWindowConfig::default(),
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
        if let Some(ms) = cfg.retry_initial_backoff_ms {
            self.retry_initial_backoff_ms = ms;
        }
        if let Some(ms) = cfg.retry_max_backoff_ms {
            self.retry_max_backoff_ms = ms;
        }
        self.context_window_config
            .apply_overrides(cfg.context_window, cfg.model_context_windows.as_ref());
    }
}

/// Errors from the Google Gemini API.
pub use crate::providers::shared::ProviderError as GoogleError;

/// The Google Gemini API client.
#[derive(Clone, Debug)]
pub struct GoogleClient {
    config: GoogleConfig,
    api_key: String,
    http: ureq::Agent,
}

impl GoogleClient {
    pub fn new(config: GoogleConfig, api_key: String) -> io::Result<Self> {
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

    pub fn config(&self) -> &GoogleConfig {
        &self.config
    }

    pub fn api_key(&self) -> &str {
        &self.api_key
    }

    /// List available models from the API, falling back to the curated static list
    /// if the endpoint is unreachable or the API key lacks permission.
    pub fn validate_and_list_models(&self) -> Result<Vec<String>, GoogleError> {
        crate::providers::list_models_with_fallback(
            || requests::list_models_request(&self.http, &self.config, &self.api_key),
            KNOWN_GEMINI_MODELS,
            "Google",
        )
    }

    /// Non-streaming chat completion turn via the Gemini generateContent API.
    pub fn chat_completion_turn(
        &self,
        params: ChatTurnRequest<'_>,
    ) -> Result<ChatTurnResult, GoogleError> {
        debug!(effort = %params.thinking_effort.as_label(), "Google chat completion turn");
        requests::generate_content_request(
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

    /// Streaming chat completion turn via the Gemini streamGenerateContent API.
    pub fn chat_completion_turn_streaming<F>(
        &self,
        params: ChatTurnRequest<'_>,
        mut on_event: F,
    ) -> Result<ChatTurnResult, GoogleError>
    where
        F: FnMut(StreamEvent) -> io::Result<()>,
    {
        debug!(?params.thinking_effort, "google chat_completion_turn_streaming");
        if !self.config.streaming {
            let result = self.chat_completion_turn(params)?;
            let result =
                crate::providers::shared::emit_non_streaming_events(result, &mut on_event)?;
            return Ok(result);
        }

        requests::generate_content_request_streaming(
            &self.http,
            &self.config,
            &self.api_key,
            params.model,
            params.messages,
            params.tools,
            params.thinking_effort,
            params.on_retry,
            params.cancel_rx,
            on_event,
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
        params: ChatTurnRequest<'_>,
    ) -> Result<ChatTurnResult, InferenceError> {
        let api_start = std::time::Instant::now();
        let model = params.model;
        let result = self.chat_completion_turn(params);
        crate::providers::shared::timed_result(api_start, model, "google", result)
    }

    fn chat_completion_turn_streaming(
        &self,
        params: ChatTurnRequest<'_>,
        on_event: &mut dyn FnMut(StreamEvent) -> io::Result<()>,
    ) -> Result<ChatTurnResult, InferenceError> {
        let api_start = std::time::Instant::now();
        let model = params.model;
        let result = self.chat_completion_turn_streaming(params, on_event);
        crate::providers::shared::timed_result(api_start, model, "google", result)
    }

    fn list_models(&self) -> Result<Vec<String>, InferenceError> {
        let result = self.validate_and_list_models();
        result.map_err(crate::providers::shared::provider_error_to_inference)
    }

    fn context_window_for_model(&self, model: &str) -> Option<u32> {
        self.config
            .context_window_config
            .context_window_for_model(model)
    }
}

/// Response from GET /v1beta/models.
#[derive(Debug, Deserialize)]
pub(super) struct ModelListResponse {
    models: Vec<ModelInfo>,
}

#[derive(Debug, Deserialize)]
pub(super) struct ModelInfo {
    name: String,
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
    #[serde(default)]
    usage_metadata: Option<UsageMetadata>,
}

#[derive(Debug, Deserialize)]
pub(super) struct UsageMetadata {
    #[serde(default, rename = "promptTokenCount")]
    prompt_token_count: u32,
    #[serde(default, rename = "candidatesTokenCount")]
    candidates_token_count: u32,
    #[serde(default, rename = "totalTokenCount")]
    total_token_count: u32,
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
                "name": &t.function.name,
                "description": &t.function.description,
                "parameters": &t.function.parameters,
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
    let usage: Option<TokenUsage> = response.usage_metadata.map(|u| TokenUsage {
        input_tokens: u.prompt_token_count,
        output_tokens: u.candidates_token_count,
        total_tokens: u.total_token_count,
    });
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
                    caller: None,
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
            usage,
            response_id: None,
        }));
    }

    let content = text_parts.join("");
    if content.is_empty() {
        return Err(GoogleError::EmptyResponse);
    }

    Ok(ChatTurnResult::FinalText(FinalTextResult {
        content,
        reasoning,
        usage,
        response_id: None,
    }))
}
