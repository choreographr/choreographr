mod chat_completions;
mod config;
mod responses;
mod retry;
mod sse;
#[cfg(test)]
mod tests;
pub use crate::providers::shared::MaxTokensField;
use crate::providers::{ChatTurnResult, StreamEvent};
use tracing::warn;

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

// Re-export common retry types for use by sub-modules and tests.
#[cfg(test)]
pub(crate) use crate::retry::{
    RetryConfig, backoff_duration, is_retryable_status, parse_retry_after_secs,
};
pub use retry::RetryCallback;



use serde::{Deserialize, Serialize};
use std::io;

/// Re-export the shared provider error type so all OpenAI code continues to
/// use `super::OpenAiError` without structural changes.
pub use crate::providers::shared::ProviderError as OpenAiError;

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RequestFormat {
    Responses,
    ChatCompletions,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AllowedCaller {
    Direct,
    Programmatic,
}

#[derive(Debug, Deserialize)]
struct ModelListResponse {
    data: Vec<ModelInfo>,
}

#[derive(Debug, Deserialize)]
struct ModelInfo {
    id: String,
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
    pub function: ChatToolFunction,
}

#[derive(Debug, Clone, Serialize)]
pub struct ChatToolFunction {
    pub name: String,
    pub description: String,
    pub parameters: serde_json::Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output_schema: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub allowed_callers: Option<Vec<AllowedCaller>>,
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

impl ChatToolDefinition {
    pub fn function(
        name: impl Into<String>,
        description: impl Into<String>,
        parameters: serde_json::Value,
    ) -> Self {
        Self {
            kind: "function",
            function: ChatToolFunction {
                name: name.into(),
                description: description.into(),
                parameters,
                output_schema: None,
                allowed_callers: None,
            },
        }
    }

    /// Create a tool definition with output_schema and allowed_callers.
    pub fn function_with_options(
        name: impl Into<String>,
        description: impl Into<String>,
        parameters: serde_json::Value,
        output_schema: Option<serde_json::Value>,
        allowed_callers: Option<Vec<AllowedCaller>>,
    ) -> Self {
        Self {
            kind: "function",
            function: ChatToolFunction {
                name: name.into(),
                description: description.into(),
                parameters,
                output_schema,
                allowed_callers,
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

    // ── High-level dispatch methods ────────────────────────────────────
    //
    // Each method inspects `self.config.request_format_for_model(model)` to
    // delegate to either Chat Completions or Responses API logic.

    pub fn validate_and_list_models(&self) -> Result<Vec<String>, OpenAiError> {
        use tracing::info;
        info!("listing models from {}", self.config.base_url);
        let url = endpoint_url(&self.config.base_url, &self.config.model_list_path)?;
        let retry = retry::retry_config_from_config(&self.config);
        let response = retry::retry_send_get(
            &self.http,
            &url,
            &self.api_key,
            &retry,
            &mut None,
            None,
        )?;
        let payload: ModelListResponse = response
            .into_body()
            .read_json()
            .map_err(|e| OpenAiError::Io(io::Error::other(e)))?;
        let models: Vec<String> = payload.data.into_iter().map(|model| model.id).collect();
        info!("models returned: {}", models.len());
        Ok(models)
    }

    pub fn completion(&self, model: &str, prompt: &str) -> Result<String, OpenAiError> {
        match self.config.request_format_for_model(model) {
            RequestFormat::Responses => {
                responses::responses_request(&self.http, &self.config, &self.api_key, model, prompt, None)
            }
            RequestFormat::ChatCompletions => {
                chat_completions::chat_completions_request(
                    &self.http, &self.config, &self.api_key, model, prompt, None,
                )
            }
        }
    }

    pub fn completion_stream<F>(&self, model: &str, prompt: &str, mut on_event: F) -> Result<(), OpenAiError>
    where
        F: FnMut(StreamEvent) -> io::Result<()>,
    {
        if !self.config.streaming {
            let content = self.completion(model, prompt)?;
            if !content.is_empty() {
                on_event(StreamEvent::Answer(content))?;
            }
            return Ok(());
        }

        match self.config.request_format_for_model(model) {
            RequestFormat::Responses => responses::responses_request_streaming(
                &self.http, &self.config, &self.api_key, model, prompt, None, &mut on_event,
            ),
            RequestFormat::ChatCompletions => chat_completions::chat_completions_request_streaming(
                &self.http, &self.config, &self.api_key, model, prompt,
                None, None, &mut on_event,
            ),
        }
    }

    pub fn chat_completion_turn(
        &self,
        params: crate::providers::ChatTurnRequest<'_>,
    ) -> Result<ChatTurnResult, OpenAiError> {
        let reasoning_effort = reasoning_effort_api_value(&params.thinking_effort);
        tracing::debug!(effort = %params.thinking_effort, ?reasoning_effort, "chat_completion_turn");
        match self.config.request_format_for_model(params.model) {
            RequestFormat::Responses => responses::responses_request_with_tools(
                &self.http, &self.config, &self.api_key,
                params.model, params.messages, params.tools,
                reasoning_effort,
                params.previous_response_id, params.tool_results,
                params.on_retry, params.cancel_rx,
                params.programmatic_tool_calling,
            ),
            RequestFormat::ChatCompletions => chat_completions::chat_completions_request_with_tools(
                &self.http, &self.config, &self.api_key,
                params.model, params.messages, params.tools,
                reasoning_effort,
                params.on_retry, params.cancel_rx,
            ),
        }
    }

    pub fn chat_completion_turn_streaming<F>(
        &self,
        params: crate::providers::ChatTurnRequest<'_>,
        mut on_event: F,
    ) -> Result<ChatTurnResult, OpenAiError>
    where
        F: FnMut(StreamEvent) -> io::Result<()>,
    {
        let reasoning_effort = reasoning_effort_api_value(&params.thinking_effort);
        tracing::debug!(
            effort = %params.thinking_effort,
            ?reasoning_effort,
            "chat_completion_turn_streaming"
        );
        if !self.config.streaming {
            let result = self.chat_completion_turn(params)?;
            let result =
                crate::providers::shared::emit_non_streaming_events(result, &mut on_event)?;
            return Ok(result);
        }

        match self.config.request_format_for_model(params.model) {
            RequestFormat::Responses => responses::responses_request_streaming_with_tools(
                &self.http, &self.config, &self.api_key,
                params.model, params.messages, params.tools,
                reasoning_effort,
                params.previous_response_id, params.tool_results,
                params.on_retry, params.cancel_rx,
                params.programmatic_tool_calling,
                &mut on_event,
            ),
            RequestFormat::ChatCompletions => {
                chat_completions::chat_completions_request_streaming_with_tools(
                    &self.http, &self.config, &self.api_key,
                    params.model, params.messages, params.tools,
                    reasoning_effort,
                    params.on_retry, params.cancel_rx,
                    &mut on_event,
                )
            }
        }
    }
}

/// Map reasoning slug string to OpenAI's `reasoning_effort` API value.
/// "off" → None (omit the field). Others → Some(slug).
pub(crate) fn reasoning_effort_api_value(slug: &str) -> Option<&str> {
    if slug == "off" { None } else { Some(slug) }
}

/// Convert ChatRequestMessage slice to Responses API input format.
/// System messages go into `input` as `{role: "system"}` items (not the
/// `instructions` field); the `instructions` field is a separate top-level
/// parameter set via explicit provider configuration.
pub(crate) fn messages_to_responses_input(
    messages: &[ChatRequestMessage],
) -> Vec<responses::ResponsesInputItem> {
    let mut items = Vec::new();

    for msg in messages {
        match msg.role {
            "system" => {
                if let Some(ref content) = msg.content {
                    items.push(responses::ResponsesInputItem::Message {
                        role: "system".to_string(),
                        content: content.clone(),
                    });
                }
            }
            "user" | "assistant" => {
                if let Some(ref content) = msg.content {
                    items.push(responses::ResponsesInputItem::Message {
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
                    items.push(responses::ResponsesInputItem::FunctionCallOutput {
                        call_id: call_id.clone(),
                        output: content.clone(),
                        caller: None,
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

    tracing::debug!("messages_to_responses_input: {} items", items.len());

    items
}

/// Filter tool calls whose `arguments_json` is not valid JSON (e.g. truncated
/// mid-stream by the provider).  Returns the discarded calls so the caller can
/// surface the cropped JSON in error messages.
/// Providers (especially cheaper models via OpenAI-compatible APIs) sometimes
/// return incomplete `function.arguments` strings, which would cause tool
/// execution to fail with a JSON parse error and trigger an error-recovery
/// loop that inflates the context and eventually hits the provider's 400/500.
pub(crate) fn validate_tool_call_arguments(
    tool_calls: &mut Vec<crate::providers::types::ChatToolCall>,
) -> Vec<choreo_proto::DiscardedToolCall> {
    let all = std::mem::take(tool_calls);
    let (valid, discarded): (Vec<_>, Vec<_>) = all
        .into_iter()
        .partition(|tc| serde_json::from_str::<serde_json::Value>(&tc.arguments_json).is_ok());
    for tc in &discarded {
        warn!(
            name = %tc.name,
            args_len = tc.arguments_json.len(),
            "discarding tool call with invalid (truncated) arguments JSON",
        );
    }
    *tool_calls = valid;
    discarded
        .into_iter()
        .map(|tc| choreo_proto::DiscardedToolCall {
            name: tc.name,
            arguments_json: tc.arguments_json,
        })
        .collect()
}

// ── ProviderClient trait impl ───────────────────────────────────────────

use crate::providers::{ChatTurnRequest, ProviderClient};
use choreo_proto::InferenceError;

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
        on_event: &mut dyn FnMut(StreamEvent) -> io::Result<()>,
    ) -> Result<ChatTurnResult, InferenceError> {
        let api_start = std::time::Instant::now();
        let model = params.model;
        let result = self.chat_completion_turn_streaming(params, on_event);
        crate::providers::shared::timed_result(api_start, model, "openai", result)
    }

    fn list_models(&self) -> Result<Vec<String>, InferenceError> {
        let result = self.validate_and_list_models();
        result.map_err(crate::providers::shared::provider_error_to_inference)
    }

    fn supports_programmatic_tool_calling(&self, model: &str) -> bool {
        self.config.programmatic_tool_calling_for_model(model)
    }

    fn context_window_for_model(&self, model: &str) -> Option<u32> {
        self.config
            .context_window_config
            .context_window_for_model(model)
    }
}
