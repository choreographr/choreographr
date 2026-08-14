mod chat_completions;
mod config;
mod responses;
mod retry;
mod sse;
#[cfg(test)]
mod tests;
pub use crate::shared::MaxTokensField;
use crate::types::{ChatTurnResult, StreamEvent};
use tracing::{debug, warn};

use choreo_proto::{ChatReasoningField, ReasoningArtifact};

pub(crate) use config::endpoint_url;
pub use config::{ServiceConfig, completion, validate_and_list_models};
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
pub use crate::shared::ProviderError as OpenAiError;

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

#[derive(Debug, Clone)]
pub struct ChatRequestMessage {
    pub role: &'static str,
    pub content: Option<String>,
    pub tool_call_id: Option<String>,
    pub tool_calls: Option<Vec<AssistantToolCall>>,
    pub reasoning_content: Option<String>,
    pub reasoning: Option<String>,
    pub reasoning_text: Option<String>,
    /// Opaque reasoning round-trip artifact captured by the producing adapter
    /// at parse time (see `ReasoningArtifact`). Never serialized as a field of
    /// its own — each adapter re-emits it in ITS OWN wire format: OpenAI chat
    /// writes it back as the field recorded in the artifact
    /// (`reasoning_content` / `reasoning` / `reasoning_text`) on assistant
    /// messages (see the manual `Serialize` impl below), Responses pushes the
    /// items into `input`, and the Anthropic/Google builders interpret their
    /// own variants.
    pub reasoning_artifact: Option<ReasoningArtifact>,
}

impl Serialize for ChatRequestMessage {
    /// Manual impl: the wire shape is byte-identical to the derived one
    /// (role, content, tool_call_id, tool_calls, reasoning_content,
    /// reasoning, reasoning_text — `None` fields omitted) EXCEPT that an
    /// assistant message carrying a `ChatReasoning` artifact re-emits it as
    /// the wire field recorded at capture time (`reasoning_content` /
    /// `reasoning` / `reasoning_text`), decoded from the captured bytes — a
    /// provider that streamed `reasoning_text` must get `reasoning_text`
    /// back, not `reasoning_content` (the mis-routing this field tag
    /// prevents). The artifact field itself never appears on the wire.
    /// DeepSeek/Kimi reject a tool-loop turn whose assistant message drops
    /// `reasoning_content`, so the round-trip payload must survive to the
    /// next request.
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeStruct;

        // Explicit reasoning fields win when the daemon populated them
        // directly; the artifact is the fallback so a message built with only
        // the opaque payload still round-trips. Only assistant-role messages
        // may carry provider reasoning — user/tool/system messages never had
        // any, so the artifact is dropped for them.
        let mut reasoning_content = self.reasoning_content.clone();
        let mut reasoning = self.reasoning.clone();
        let mut reasoning_text = self.reasoning_text.clone();
        if self.role == "assistant"
            && let Some(ReasoningArtifact::ChatReasoning { field, bytes }) =
                &self.reasoning_artifact
        {
            // The artifact is opaque bytes; only the producing adapter may
            // interpret it. Chat artifacts are always captured from a Rust
            // String (so valid UTF-8), but a corrupted persisted blob must
            // not fail the whole request — log and drop the payload so the
            // request proceeds without the reasoning echo (the provider may
            // reject it, but that is a diagnosable 400 rather than a
            // serialization crash on every subsequent turn).
            if let Ok(text) = std::str::from_utf8(bytes) {
                debug!(
                    ?field,
                    payload_bytes = bytes.len(),
                    "re-emitting chat reasoning artifact"
                );
                // Write the decoded text to the field named by the artifact's
                // tag (only when that field is currently None — explicit
                // values still win), so re-emission targets the same wire
                // field the provider used at capture.
                let target = match field {
                    ChatReasoningField::ReasoningContent => &mut reasoning_content,
                    ChatReasoningField::Reasoning => &mut reasoning,
                    ChatReasoningField::ReasoningText => &mut reasoning_text,
                };
                if target.is_none() {
                    *target = Some(text.to_string());
                }
            } else {
                warn!(
                    ?field,
                    payload_bytes = bytes.len(),
                    "dropping corrupt chat reasoning artifact (not valid UTF-8)",
                );
            }
        }

        let mut msg = serializer.serialize_struct("ChatRequestMessage", 7)?;
        msg.serialize_field("role", &self.role)?;
        if let Some(content) = &self.content {
            msg.serialize_field("content", content)?;
        }
        if let Some(tool_call_id) = &self.tool_call_id {
            msg.serialize_field("tool_call_id", tool_call_id)?;
        }
        if let Some(tool_calls) = &self.tool_calls {
            msg.serialize_field("tool_calls", tool_calls)?;
        }
        if let Some(rc) = &reasoning_content {
            msg.serialize_field("reasoning_content", rc)?;
        }
        if let Some(reasoning) = &reasoning {
            msg.serialize_field("reasoning", reasoning)?;
        }
        if let Some(reasoning_text) = &reasoning_text {
            msg.serialize_field("reasoning_text", reasoning_text)?;
        }
        msg.end()
    }
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
            reasoning_artifact: None,
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

#[derive(Clone)]
pub struct OpenAiClient {
    config: ServiceConfig,
    api_key: zeroize::Zeroizing<String>,
    http: ureq::Agent,
}

// Manual Debug impl: derived Debug would print the raw API key if a client is
// ever logged.  Redact the key (`***`) while still delegating the other fields
// (config, http) so the struct stays useful in logs.
impl std::fmt::Debug for OpenAiClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OpenAiClient")
            .field("config", &self.config)
            .field("api_key", &"***")
            .field("http", &self.http)
            .finish()
    }
}

impl OpenAiClient {
    pub fn new(config: ServiceConfig, api_key: String) -> io::Result<Self> {
        let http = crate::shared::build_agent(
            config.connect_timeout_secs,
            config.request_timeout_secs,
            config.total_timeout_secs,
        );
        // Zeroizing<String> wipes the key bytes from memory when the client is
        // dropped; `new` keeps its `String` signature so callers are unaffected.
        Ok(Self {
            config,
            api_key: zeroize::Zeroizing::new(api_key),
            http,
        })
    }

    pub fn config(&self) -> &ServiceConfig {
        &self.config
    }

    pub fn api_key(&self) -> &str {
        // `Zeroizing<String>` derefs to `String`, so `as_str()` works directly.
        self.api_key.as_str()
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
        // Hoist the no-op retry callback into a named local: a bare `&mut None`
        // temporary would be dropped before the retry call below (E0716).
        let mut no_retry = None;
        let mut ctx = retry::AttemptContext::new(&mut no_retry, None, None);
        let response = retry::retry_send_get(
            &self.http,
            &url,
            &self.api_key,
            &self.config,
            &retry,
            &mut ctx,
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
            RequestFormat::Responses => responses::responses_request(
                &self.http,
                &self.config,
                &self.api_key,
                model,
                prompt,
                None,
            ),
            RequestFormat::ChatCompletions => chat_completions::chat_completions_request(
                &self.http,
                &self.config,
                &self.api_key,
                model,
                prompt,
                None,
            ),
        }
    }

    pub fn completion_stream<F>(
        &self,
        model: &str,
        prompt: &str,
        mut on_event: F,
    ) -> Result<(), OpenAiError>
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
                &self.http,
                &self.config,
                &self.api_key,
                model,
                prompt,
                None,
                &mut on_event,
            ),
            RequestFormat::ChatCompletions => chat_completions::chat_completions_request_streaming(
                &self.http,
                &self.config,
                &self.api_key,
                model,
                prompt,
                None,
                None,
                &mut on_event,
            ),
        }
    }

    pub fn chat_completion_turn(
        &self,
        params: crate::ChatTurnRequest<'_>,
    ) -> Result<ChatTurnResult, OpenAiError> {
        let reasoning_effort = reasoning_effort_api_value(&params.thinking_effort);
        tracing::debug!(effort = %params.thinking_effort, ?reasoning_effort, "chat_completion_turn");
        match self.config.request_format_for_model(params.model) {
            RequestFormat::Responses => responses::responses_request_with_tools(
                &self.http,
                &self.config,
                &self.api_key,
                params.model,
                params.messages,
                params.tools,
                reasoning_effort,
                params.previous_response_id,
                params.tool_results,
                params.on_retry,
                params.cancel_rx,
                params.programmatic_tool_calling,
            ),
            RequestFormat::ChatCompletions => {
                chat_completions::chat_completions_request_with_tools(
                    &self.http,
                    &self.config,
                    &self.api_key,
                    params.model,
                    params.messages,
                    params.tools,
                    reasoning_effort,
                    params.on_retry,
                    params.cancel_rx,
                )
            }
        }
    }

    pub fn chat_completion_turn_streaming<F>(
        &self,
        params: crate::ChatTurnRequest<'_>,
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
            let result = crate::shared::emit_non_streaming_events(result, &mut on_event)?;
            return Ok(result);
        }

        match self.config.request_format_for_model(params.model) {
            RequestFormat::Responses => responses::responses_request_streaming_with_tools(
                &self.http,
                &self.config,
                &self.api_key,
                params.model,
                params.messages,
                params.tools,
                reasoning_effort,
                params.previous_response_id,
                params.tool_results,
                params.on_retry,
                params.cancel_rx,
                params.programmatic_tool_calling,
                &mut on_event,
            ),
            RequestFormat::ChatCompletions => {
                chat_completions::chat_completions_request_streaming_with_tools(
                    &self.http,
                    &self.config,
                    &self.api_key,
                    params.model,
                    params.messages,
                    params.tools,
                    reasoning_effort,
                    params.on_retry,
                    params.cancel_rx,
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

/// Serialize a Responses input item to its wire JSON value. Kept as a tiny
/// helper because the Responses adapter owns the item type but the conversion
/// happens here, in the shared messages→input builder.
fn responses_input_item_value(
    item: responses::ResponsesInputItem,
) -> Result<serde_json::Value, OpenAiError> {
    serde_json::to_value(&item).map_err(|e| OpenAiError::Io(io::Error::other(e)))
}

/// Convert ChatRequestMessage slice to Responses API input format.
/// System messages go into `input` as `{role: "system"}` items (not the
/// `instructions` field); the `instructions` field is a separate top-level
/// parameter set via explicit provider configuration.
///
/// Assistant messages additionally re-emit the opaque reasoning items from
/// the round-trip artifact (if any) directly into `input`, BEFORE the message
/// content item — mirroring the provider's output ordering where a reasoning
/// item precedes its message. `reasoning_content` is never emitted here:
/// that field is chat-completions-only and invalid on Responses messages.
pub(crate) fn messages_to_responses_input(
    messages: &[ChatRequestMessage],
) -> Result<Vec<serde_json::Value>, OpenAiError> {
    let mut items: Vec<serde_json::Value> = Vec::new();

    for msg in messages {
        match msg.role {
            "system" => {
                if let Some(ref content) = msg.content {
                    items.push(responses_input_item_value(
                        responses::ResponsesInputItem::Message {
                            role: "system".to_string(),
                            content: content.clone(),
                        },
                    )?);
                }
            }
            "user" | "assistant" => {
                // Assistant turns replay their opaque reasoning items (type
                // tag, id, summary, encrypted_content) verbatim, placed ahead
                // of the message content exactly as the provider emitted them
                // in the original output array.
                if msg.role == "assistant"
                    && let Some(ReasoningArtifact::ResponsesItems(bytes)) = &msg.reasoning_artifact
                {
                    let reasoning_items: Vec<serde_json::Value> = serde_json::from_slice(bytes)
                        .map_err(|e| OpenAiError::Io(io::Error::other(e)))?;
                    debug!(
                        item_count = reasoning_items.len(),
                        "re-emitting responses reasoning items from artifact"
                    );
                    items.extend(reasoning_items);
                }
                if let Some(ref content) = msg.content {
                    items.push(responses_input_item_value(
                        responses::ResponsesInputItem::Message {
                            role: msg.role.to_string(),
                            content: content.clone(),
                        },
                    )?);
                }
            }
            "tool" => {
                // Tool results become function_call_output items
                if let Some(ref call_id) = msg.tool_call_id
                    && let Some(ref content) = msg.content
                {
                    items.push(responses_input_item_value(
                        responses::ResponsesInputItem::FunctionCallOutput {
                            call_id: call_id.clone(),
                            output: content.clone(),
                            caller: None,
                        },
                    )?);
                }
            }
            _ => {
                warn!(
                    "unexpected message role in messages_to_responses_input: {}",
                    msg.role
                );
            }
        }
    }

    debug!("messages_to_responses_input: {} items", items.len());

    Ok(items)
}

/// Filter tool calls whose `arguments_json` is not valid JSON (e.g. truncated
/// mid-stream by the provider).  Returns the discarded calls so the caller can
/// surface the cropped JSON in error messages.
/// Providers (especially cheaper models via OpenAI-compatible APIs) sometimes
/// return incomplete `function.arguments` strings, which would cause tool
/// execution to fail with a JSON parse error and trigger an error-recovery
/// loop that inflates the context and eventually hits the provider's 400/500.
pub(crate) fn validate_tool_call_arguments(
    tool_calls: &mut Vec<crate::types::ChatToolCall>,
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

use crate::{ChatTurnRequest, ProviderClient};
use choreo_proto::InferenceError;

impl ProviderClient for OpenAiClient {
    fn provider_slug(&self) -> &str {
        "openai"
    }

    fn chat_completion_turn(
        &self,
        params: ChatTurnRequest<'_>,
    ) -> Result<ChatTurnResult, InferenceError> {
        self.chat_completion_turn(params)
            .map_err(crate::shared::provider_error_to_inference)
    }

    fn chat_completion_turn_streaming(
        &self,
        params: ChatTurnRequest<'_>,
        on_event: &mut dyn FnMut(StreamEvent) -> io::Result<()>,
    ) -> Result<ChatTurnResult, InferenceError> {
        self.chat_completion_turn_streaming(params, on_event)
            .map_err(crate::shared::provider_error_to_inference)
    }

    fn list_models(&self) -> Result<Vec<String>, InferenceError> {
        let result = self.validate_and_list_models();
        result.map_err(crate::shared::provider_error_to_inference)
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
