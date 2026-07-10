mod requests;
#[cfg(test)]
mod tests;

use std::io;
use std::sync::mpsc;
use tracing::{debug, warn};

use serde::{Deserialize, Serialize};

use crate::openai::{
    ChatAssistantToolUse, ChatRequestMessage, ChatToolCall, ChatToolDefinition, ChatTurnResult,
    CompletionChunkKind, FinalTextResult,
};

const DEFAULT_BASE_URL: &str = "https://api.anthropic.com";
const DEFAULT_API_VERSION: &str = "2023-06-01";
const DEFAULT_MAX_TOKENS: u32 = 4096;

/// Configuration for the Anthropic Messages API client.
#[derive(Debug, Clone)]
pub struct AnthropicConfig {
    pub base_url: String,
    pub api_version: String,
    pub max_tokens: u32,
    pub streaming: bool,
    pub retry_max_attempts: u32,
    pub retry_initial_backoff_ms: u64,
    pub retry_max_backoff_ms: u64,
    pub connect_timeout_secs: u64,
    pub request_timeout_secs: u64,
}

impl Default for AnthropicConfig {
    fn default() -> Self {
        Self {
            base_url: DEFAULT_BASE_URL.to_string(),
            api_version: DEFAULT_API_VERSION.to_string(),
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

impl AnthropicConfig {
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

/// Errors from the Anthropic Messages API.
pub use crate::providers::shared::ProviderError as AnthropicError;

/// The Anthropic Messages API client.
#[derive(Clone, Debug)]
pub struct AnthropicClient {
    config: AnthropicConfig,
    api_key: String,
    http: reqwest::blocking::Client,
}

// ── ProviderClient trait impl ───────────────────────────────────────────

use crate::providers::ProviderClient;
use tai_proto::{InferenceError, ThinkingEffort};

impl ProviderClient for AnthropicClient {
    fn provider_slug(&self) -> &'static str {
        "anthropic"
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
        crate::providers::shared::timed_result(api_start, model, "anthropic", result)
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
        crate::providers::shared::timed_result(api_start, model, "anthropic", result)
    }

    fn list_models(&self) -> Result<Vec<String>, InferenceError> {
        let result = self.validate_and_list_models();
        result.map_err(crate::providers::shared::provider_error_to_inference)
    }
}

impl AnthropicClient {
    pub fn new(config: AnthropicConfig, api_key: String) -> io::Result<Self> {
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

    pub fn config(&self) -> &AnthropicConfig {
        &self.config
    }

    pub fn api_key(&self) -> &str {
        &self.api_key
    }

    /// List available models from the API, falling back to the curated static list
    /// if the endpoint is unreachable or the API key lacks permission.
    pub fn validate_and_list_models(&self) -> Result<Vec<String>, AnthropicError> {
        crate::providers::list_models_with_fallback(
            || requests::list_models_request(&self.http, &self.config, &self.api_key),
            KNOWN_CLAUDE_MODELS,
            "Anthropic",
        )
    }

    /// Non-streaming chat completion turn via the Messages API.
    pub fn chat_completion_turn(
        &self,
        model: &str,
        messages: &[ChatRequestMessage],
        tools: &[ChatToolDefinition],
        thinking_effort: ThinkingEffort,
        on_retry: &mut Option<crate::retry::RetryCallback>,
        cancel_rx: Option<&mpsc::Receiver<()>>,
    ) -> Result<ChatTurnResult, AnthropicError> {
        debug!(
            effort = %thinking_effort.as_label(),
            "Anthropic chat completion turn"
        );
        requests::messages_request(
            &self.http,
            &self.config,
            &self.api_key,
            model,
            messages,
            tools,
            thinking_effort,
            false,
            on_retry,
            cancel_rx,
        )
    }

    /// Streaming chat completion turn via the Messages API.
    pub fn chat_completion_turn_streaming<F>(
        &self,
        model: &str,
        messages: &[ChatRequestMessage],
        tools: &[ChatToolDefinition],
        thinking_effort: ThinkingEffort,
        on_retry: &mut Option<crate::retry::RetryCallback>,
        cancel_rx: Option<&mpsc::Receiver<()>>,
        on_chunk: F,
    ) -> Result<ChatTurnResult, AnthropicError>
    where
        F: FnMut(CompletionChunkKind, String) -> io::Result<()>,
    {
        debug!(?thinking_effort, "anthropic chat_completion_turn_streaming");
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

        requests::messages_request_streaming(
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

// ── API types ──────────────────────────────────────────────────────────

/// Response from GET /v1/models.
#[derive(Debug, Deserialize)]
pub(super) struct ModelListResponse {
    data: Vec<ModelInfo>,
}

#[derive(Debug, Deserialize)]
pub(super) struct ModelInfo {
    id: String,
}

/// Known Claude models (curated static list).
const KNOWN_CLAUDE_MODELS: &[&str] = &[
    "claude-sonnet-4-20250514",
    "claude-sonnet-4",
    "claude-haiku-3-5-20241022",
    "claude-haiku-3-5",
    "claude-opus-4-20250514",
    "claude-opus-4",
    "claude-sonnet-3-5-20241022",
    "claude-sonnet-3-5",
    "claude-3-haiku-20240307",
    "claude-3-opus-20240229",
];

/// Request body for POST /v1/messages.
#[derive(Debug, Serialize)]
struct MessagesRequest<'a> {
    model: &'a str,
    max_tokens: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    system: Option<&'a str>,
    messages: Vec<MessagePayload<'a>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tools: Option<Vec<ToolPayload<'a>>>,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    stream: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    thinking: Option<ThinkingPayload>,
}

#[derive(Debug, Serialize)]
struct MessagePayload<'a> {
    role: &'a str,
    content: Vec<ContentBlockPayload<'a>>,
}

#[derive(Debug, Serialize)]
#[serde(untagged)]
#[allow(dead_code)]
enum ContentBlockPayload<'a> {
    Text {
        r#type: &'a str,
        text: &'a str,
    },
    ToolUse {
        r#type: &'a str,
        id: &'a str,
        name: &'a str,
        input: serde_json::Value,
    },
    ToolResult {
        r#type: &'a str,
        tool_use_id: &'a str,
        content: &'a str,
    },
}

#[derive(Debug, Serialize)]
struct ToolPayload<'a> {
    name: &'a str,
    description: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    input_schema: Option<serde_json::Value>,
}

/// Thinking payload for Anthropic's extended thinking API.
#[derive(Debug, Serialize)]
pub(super) struct ThinkingPayload {
    #[serde(rename = "type")]
    kind: &'static str,
    budget_tokens: u32,
}

/// Response body from POST /v1/messages.
#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct MessagesResponse {
    id: String,
    r#type: String,
    role: String,
    content: Vec<ContentBlock>,
    #[serde(default)]
    stop_reason: Option<String>,
    #[serde(default)]
    stop_sequence: Option<String>,
    model: String,
    #[serde(default)]
    usage: Option<UsageInfo>,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type")]
enum ContentBlock {
    #[serde(rename = "text")]
    Text { text: String },
    #[serde(rename = "tool_use")]
    ToolUse {
        id: String,
        name: String,
        input: serde_json::Value,
    },
    #[serde(rename = "thinking")]
    Thinking { thinking: String },
    #[serde(rename = "redacted_thinking")]
    #[allow(dead_code)]
    RedactedThinking { data: String },
}

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct UsageInfo {
    #[serde(default)]
    input_tokens: u32,
    #[serde(default)]
    output_tokens: u32,
}

/// Convert the content blocks from a Messages API response into a
/// [`ChatTurnResult`].
fn response_to_turn_result(response: MessagesResponse) -> Result<ChatTurnResult, AnthropicError> {
    let mut text_parts: Vec<String> = Vec::new();
    let mut tool_uses: Vec<ChatToolCall> = Vec::new();
    let mut reasoning_parts: Vec<String> = Vec::new();

    for block in response.content {
        match block {
            ContentBlock::Text { text } => {
                text_parts.push(text);
            }
            ContentBlock::ToolUse { id, name, input } => {
                tool_uses.push(ChatToolCall {
                    id,
                    name,
                    arguments_json: input.to_string(),
                });
            }
            ContentBlock::Thinking { thinking } => {
                reasoning_parts.push(thinking);
            }
            ContentBlock::RedactedThinking { .. } => {
                // Redacted thinking blocks are skipped — they contain no usable text.
            }
        }
    }

    let reasoning = if reasoning_parts.is_empty() {
        None
    } else {
        Some(reasoning_parts.join("\n"))
    };

    if !tool_uses.is_empty() {
        let content = if text_parts.is_empty() {
            None
        } else {
            Some(text_parts.join(""))
        };
        return Ok(ChatTurnResult::ToolUse(ChatAssistantToolUse {
            content,
            tool_calls: tool_uses,
            reasoning,
        }));
    }

    let content = text_parts.join("");
    if content.is_empty() {
        return Err(AnthropicError::EmptyResponse);
    }

    Ok(ChatTurnResult::FinalText(FinalTextResult {
        content,
        reasoning,
    }))
}

/// Convert a list of messages + tools into the format expected by the
/// Anthropic Messages API.
fn build_message_payloads<'a>(
    messages: &'a [ChatRequestMessage],
    _tools: &'a [ChatToolDefinition],
) -> (Vec<MessagePayload<'a>>, Option<String>) {
    let mut system: Option<String> = None;
    let mut payloads: Vec<MessagePayload> = Vec::new();

    for msg in messages {
        match msg.role {
            "system" => {
                // Collect system messages — Anthropic uses a top-level "system"
                // field instead of a system message in the messages array.
                if let Some(ref content) = msg.content {
                    let text = system.get_or_insert_with(String::new);
                    if !text.is_empty() {
                        text.push('\n');
                    }
                    text.push_str(content);
                }
            }
            "tool" => {
                // Tool results in Anthropic format: role: "user", content: [{type: "tool_result", ...}]
                let text = msg.content.as_deref().unwrap_or("");
                payloads.push(MessagePayload {
                    role: "user",
                    content: vec![ContentBlockPayload::ToolResult {
                        r#type: "tool_result",
                        tool_use_id: msg.tool_call_id.as_deref().unwrap_or(""),
                        content: text,
                    }],
                });
            }
            "assistant" => {
                // Assistant messages may contain text + tool_use content blocks.
                let mut blocks: Vec<ContentBlockPayload<'a>> = Vec::new();
                // Add text content if present.
                if let Some(text) = msg.content.as_deref().filter(|t| !t.is_empty()) {
                    blocks.push(ContentBlockPayload::Text {
                        r#type: "text",
                        text,
                    });
                }
                // Add tool_use blocks for each tool call.
                if let Some(ref calls) = msg.tool_calls {
                    for tc in calls {
                        blocks.push(ContentBlockPayload::ToolUse {
                            r#type: "tool_use",
                            id: &tc.id,
                            name: &tc.function.name,
                            input: serde_json::from_str(&tc.function.arguments).unwrap_or_default(),
                        });
                    }
                }
                payloads.push(MessagePayload {
                    role: "assistant",
                    content: blocks,
                });
            }
            role => {
                // User or other roles: wrap content as text blocks.
                let content = msg.content.as_deref().unwrap_or("");
                payloads.push(MessagePayload {
                    role,
                    content: vec![ContentBlockPayload::Text {
                        r#type: "text",
                        text: content,
                    }],
                });
            }
        }
    }

    // Remove the last message if it's empty (sometimes happens with
    // user messages that contain only tool results).
    if let Some(last) = payloads.last()
        && last.content.is_empty()
    {
        payloads.pop();
    }

    (payloads, system)
}

/// Map tool definitions to Anthropic tool format.
fn build_tool_payloads(tools: &[ChatToolDefinition]) -> Vec<ToolPayload<'_>> {
    tools
        .iter()
        .map(|t| {
            // Each ChatToolDefinition has a single "function" entry.
            ToolPayload {
                name: t.function.name,
                description: t.function.description,
                input_schema: Some(t.function.parameters.clone()),
            }
        })
        .collect()
}

/// Map ThinkingEffort to Anthropic thinking budget tokens.
/// Off → None (no thinking block sent).
/// Low → 2048, Medium → 4096, High → 16384.
/// Clamps budget_tokens so that max_tokens >= budget_tokens + 1024.
pub(super) fn thinking_payload(effort: ThinkingEffort, max_tokens: u32) -> Option<ThinkingPayload> {
    match effort {
        ThinkingEffort::Off => None,
        _ => {
            let desired = match effort {
                ThinkingEffort::Off => unreachable!(),
                ThinkingEffort::Low => 2048,
                ThinkingEffort::Medium => 4096,
                ThinkingEffort::High => 16384,
            };
            let max_possible = max_tokens.saturating_sub(1024);
            let budget = desired.min(max_possible);
            if budget < desired {
                warn!(
                    desired,
                    budget,
                    max_tokens,
                    "clamped Anthropic thinking budget_tokens to fit within max_tokens"
                );
            }
            Some(ThinkingPayload {
                kind: "enabled",
                budget_tokens: budget,
            })
        }
    }
}
