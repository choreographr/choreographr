mod requests;
#[cfg(test)]
mod tests;

use std::io;
use tracing::{debug, warn};

use serde::{Deserialize, Serialize};

use crate::openai::{ChatRequestMessage, ChatToolDefinition};
use crate::overrides::ProviderOverrides;
use crate::types::{
    ChatAssistantToolUse, ChatToolCall, ChatTurnResult, FinalTextResult, StreamEvent,
};
use crate::{ChatTurnRequest, ContextWindowConfig};
use choreo_proto::{ReasoningArtifact, TokenUsage};

const DEFAULT_BASE_URL: &str = "https://api.anthropic.com";
const DEFAULT_API_VERSION: &str = "2023-06-01";
const DEFAULT_MAX_TOKENS: u32 = 4096;

/// Configuration for the Anthropic Messages API client.
#[derive(Debug, Clone)]
pub struct AnthropicConfig {
    pub base_url: String,
    pub api_version: String,
    pub max_tokens: u32,
    pub context_window_config: ContextWindowConfig,
    pub streaming: bool,
    pub retry_max_attempts: u32,
    pub retry_initial_backoff_ms: u64,
    pub retry_max_backoff_ms: u64,
    pub connect_timeout_secs: u64,
    pub request_timeout_secs: u64,
    /// Hard wall-clock deadline for a single HTTP request attempt, including
    /// the streaming body read; 0 disables.  Unlike `request_timeout_secs` (an
    /// idle/no-progress timeout that resets per chunk), this fires even when a
    /// provider trickles keep-alive bytes, so it bounds a stalled SSE stream.
    /// It covers one attempt: each retry restarts the deadline, so retries
    /// plus their backoff can exceed this value in aggregate.
    pub total_timeout_secs: u64,
}

impl Default for AnthropicConfig {
    fn default() -> Self {
        Self {
            base_url: DEFAULT_BASE_URL.to_string(),
            api_version: DEFAULT_API_VERSION.to_string(),
            max_tokens: DEFAULT_MAX_TOKENS,
            context_window_config: ContextWindowConfig::default(),
            streaming: true,
            retry_max_attempts: 5,
            retry_initial_backoff_ms: 1000,
            retry_max_backoff_ms: 30000,
            connect_timeout_secs: 30,
            request_timeout_secs: 120,
            total_timeout_secs: 3600,
        }
    }
}

impl AnthropicConfig {
    /// Apply provider-agnostic overrides onto this config.
    ///
    /// The daemon converts its `AccountConfig` into a [`ProviderOverrides`]
    /// carrier before calling this, so this crate never depends on daemon
    /// types. `None` fields leave the provider default in place.
    pub fn apply_overrides(&mut self, overrides: &ProviderOverrides) {
        if let Some(base_url) = &overrides.base_url {
            self.base_url = base_url.clone();
        }
        if let Some(streaming) = overrides.streaming {
            self.streaming = streaming;
        }
        if let Some(retry) = overrides.retry_max_attempts {
            self.retry_max_attempts = retry;
        }
        if let Some(connect) = overrides.connect_timeout_secs {
            self.connect_timeout_secs = connect;
        }
        if let Some(request) = overrides.request_timeout_secs {
            self.request_timeout_secs = request;
        }
        if let Some(total) = overrides.total_timeout_secs {
            self.total_timeout_secs = total;
        }
        if let Some(ms) = overrides.retry_initial_backoff_ms {
            self.retry_initial_backoff_ms = ms;
        }
        if let Some(ms) = overrides.retry_max_backoff_ms {
            self.retry_max_backoff_ms = ms;
        }
        self.context_window_config.apply_overrides(
            overrides.context_window,
            overrides.model_context_windows.as_ref(),
        );
    }
}

/// Errors from the Anthropic Messages API.
pub use crate::shared::ProviderError as AnthropicError;

/// The Anthropic Messages API client.
#[derive(Clone)]
pub struct AnthropicClient {
    config: AnthropicConfig,
    api_key: zeroize::Zeroizing<String>,
    http: ureq::Agent,
}

// Manual Debug impl: derived Debug would print the raw API key if a client is
// ever logged.  Redact the key (`***`) while still delegating the other fields
// (config, http) so the struct stays useful in logs.
impl std::fmt::Debug for AnthropicClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AnthropicClient")
            .field("config", &self.config)
            .field("api_key", &"***")
            .field("http", &self.http)
            .finish()
    }
}

// ── ProviderClient trait impl ───────────────────────────────────────────

use crate::ProviderClient;
use choreo_proto::InferenceError;

impl ProviderClient for AnthropicClient {
    fn provider_slug(&self) -> &'static str {
        "anthropic"
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

    fn context_window_for_model(&self, model: &str) -> Option<u32> {
        self.config
            .context_window_config
            .context_window_for_model(model)
    }
}

impl AnthropicClient {
    pub fn new(config: AnthropicConfig, api_key: String) -> io::Result<Self> {
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

    pub fn config(&self) -> &AnthropicConfig {
        &self.config
    }

    pub fn api_key(&self) -> &str {
        // `Zeroizing<String>` derefs to `String`, so `as_str()` works directly.
        self.api_key.as_str()
    }

    /// List available models from the API, falling back to the curated static list
    /// if the endpoint is unreachable or the API key lacks permission.
    pub fn validate_and_list_models(&self) -> Result<Vec<String>, AnthropicError> {
        crate::shared::list_models_with_fallback(
            || requests::list_models_request(&self.http, &self.config, &self.api_key),
            KNOWN_CLAUDE_MODELS,
            "Anthropic",
        )
    }

    /// Non-streaming chat completion turn via the Messages API.
    pub fn chat_completion_turn(
        &self,
        params: ChatTurnRequest<'_>,
    ) -> Result<ChatTurnResult, AnthropicError> {
        debug!(
            effort = %params.thinking_effort,
            "Anthropic chat completion turn"
        );
        requests::messages_request(
            &self.http,
            &self.config,
            &self.api_key,
            params.model,
            params.messages,
            params.tools,
            &params.thinking_effort,
            false,
            params.on_retry,
            params.cancel_rx,
        )
    }

    /// Streaming chat completion turn via the Messages API.
    pub fn chat_completion_turn_streaming<F>(
        &self,
        params: ChatTurnRequest<'_>,
        mut on_event: F,
    ) -> Result<ChatTurnResult, AnthropicError>
    where
        F: FnMut(StreamEvent) -> io::Result<()>,
    {
        debug!(effort = %params.thinking_effort, "anthropic chat_completion_turn_streaming");
        if !self.config.streaming {
            let result = self.chat_completion_turn(params)?;
            let result = crate::shared::emit_non_streaming_events(result, &mut on_event)?;
            return Ok(result);
        }

        requests::messages_request_streaming(
            &self.http,
            &self.config,
            &self.api_key,
            params.model,
            params.messages,
            params.tools,
            &params.thinking_effort,
            params.on_retry,
            params.cancel_rx,
            on_event,
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
    /// Provider-owned thinking / redacted_thinking block, replayed verbatim
    /// from the round-trip artifact (never rebuilt or reordered). Serializes
    /// as the embedded JSON value; untagged serialization delegates to the
    /// actual variant, so the raw block passes through unchanged.
    Raw(serde_json::Value),
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
#[expect(dead_code)]
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
    Thinking {
        thinking: String,
        /// Encrypted signature the provider requires back on replay. Anthropic
        /// always sends it on thinking blocks; default to empty so an unusual
        /// (or third-party) provider that omits it still parses.
        #[serde(default)]
        signature: String,
    },
    #[serde(rename = "redacted_thinking")]
    RedactedThinking { data: String },
}

/// One thinking / redacted_thinking content block captured for the opaque
/// round-trip artifact, kept in original wire order.
///
/// The payload is provider-owned: only the Anthropic adapter may interpret it
/// (re-emission happens in Phase 4a). Display text lives separately in
/// `FinalTextResult::reasoning`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum ThinkingArtifactBlock {
    /// `{"type":"thinking","thinking":…,"signature":…}`
    Thinking { thinking: String, signature: String },
    /// `{"type":"redacted_thinking","data":…}`
    RedactedThinking { data: String },
}

/// Serialize the ordered thinking / redacted_thinking blocks into the opaque
/// [`ReasoningArtifact::AnthropicThinking`] payload, or `None` when nothing was
/// captured. The payload is the JSON serialization of the block array exactly
/// as received — block order preserved, signatures and redacted data intact —
/// so a later passback can replay it verbatim.
fn anthropic_thinking_artifact(
    blocks: &[ThinkingArtifactBlock],
) -> Result<Option<ReasoningArtifact>, AnthropicError> {
    if blocks.is_empty() {
        return Ok(None);
    }
    let values: Vec<serde_json::Value> = blocks
        .iter()
        .map(|block| match block {
            ThinkingArtifactBlock::Thinking {
                thinking,
                signature,
            } => serde_json::json!({
                "type": "thinking",
                "thinking": thinking,
                "signature": signature,
            }),
            ThinkingArtifactBlock::RedactedThinking { data } => serde_json::json!({
                "type": "redacted_thinking",
                "data": data,
            }),
        })
        .collect();
    let bytes = serde_json::to_vec(&values).map_err(|e| AnthropicError::Io(io::Error::other(e)))?;
    debug!(
        block_count = blocks.len(),
        payload_bytes = bytes.len(),
        "captured anthropic thinking artifact",
    );
    Ok(Some(ReasoningArtifact::AnthropicThinking(bytes)))
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
    // Thinking / redacted_thinking blocks in original wire order, captured for
    // the opaque round-trip artifact (signatures + redacted data intact).
    let mut artifact_blocks: Vec<ThinkingArtifactBlock> = Vec::new();

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
                    caller: None,
                });
            }
            ContentBlock::Thinking {
                thinking,
                signature,
            } => {
                reasoning_parts.push(thinking.clone());
                artifact_blocks.push(ThinkingArtifactBlock::Thinking {
                    thinking,
                    signature,
                });
            }
            ContentBlock::RedactedThinking { data } => {
                // Retained for the round-trip artifact: redacted blocks carry
                // opaque encrypted data the provider requires back on replay,
                // even though they contain no displayable text.
                artifact_blocks.push(ThinkingArtifactBlock::RedactedThinking { data });
            }
        }
    }

    let reasoning_artifact = anthropic_thinking_artifact(&artifact_blocks)?;

    let reasoning = if reasoning_parts.is_empty() {
        None
    } else {
        Some(reasoning_parts.join("\n"))
    };

    // Convert Anthropic's usage info (input_tokens + output_tokens) to our
    // canonical TokenUsage struct. Anthropic does not provide total_tokens,
    // so we compute it.
    let usage: Option<TokenUsage> = response.usage.map(|u| {
        let total = u.input_tokens + u.output_tokens;
        debug!(
            input_tokens = u.input_tokens,
            output_tokens = u.output_tokens,
            total_tokens = total,
            "Anthropic turn usage"
        );
        TokenUsage {
            input_tokens: u.input_tokens,
            output_tokens: u.output_tokens,
            total_tokens: total,
        }
    });

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
            usage,
            response_id: None,
            reasoning_artifact,
        }));
    }

    let content = text_parts.join("");
    if content.is_empty() {
        return Err(AnthropicError::EmptyResponse);
    }

    Ok(ChatTurnResult::FinalText(FinalTextResult {
        content,
        reasoning,
        usage,
        response_id: None,
        reasoning_artifact,
    }))
}

/// Decode the opaque Anthropic thinking artifact into verbatim content blocks
/// (thinking + redacted_thinking, signatures and redacted data intact, order
/// preserved). Returns an empty vec when the artifact is absent or owned by a
/// different adapter — payloads stay opaque until their producer decodes them.
fn artifact_thinking_blocks(
    artifact: Option<&ReasoningArtifact>,
) -> Result<Vec<ContentBlockPayload<'static>>, AnthropicError> {
    let Some(ReasoningArtifact::AnthropicThinking(bytes)) = artifact else {
        return Ok(Vec::new());
    };
    let blocks: Vec<serde_json::Value> =
        serde_json::from_slice(bytes).map_err(|e| AnthropicError::Io(io::Error::other(e)))?;
    debug!(
        block_count = blocks.len(),
        "re-emitting anthropic thinking blocks from artifact"
    );
    Ok(blocks.into_iter().map(ContentBlockPayload::Raw).collect())
}

/// Convert a list of messages + tools into the format expected by the
/// Anthropic Messages API.
///
/// `thinking_enabled` gates the replay of thinking / redacted_thinking blocks
/// from the round-trip artifact: Anthropic rejects thinking blocks sent
/// without a matching thinking config, so they are dropped when thinking is
/// off for this request (goose's `!thinking_disabled` gate).
fn build_message_payloads<'a>(
    messages: &'a [ChatRequestMessage],
    _tools: &'a [ChatToolDefinition],
    thinking_enabled: bool,
) -> Result<(Vec<MessagePayload<'a>>, Option<String>), AnthropicError> {
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
                // Assistant messages may contain thinking + text + tool_use
                // content blocks. The thinking / redacted_thinking blocks are
                // replayed VERBATIM from the round-trip artifact, in original
                // order, ahead of text/tool_use — Anthropic requires thinking
                // blocks to precede tool_use within a turn and the encrypted
                // signature must be echoed back unmodified (a missing or
                // altered block is a 400 on the next tool-loop request).
                let mut blocks: Vec<ContentBlockPayload<'a>> = Vec::new();
                if thinking_enabled {
                    for block in artifact_thinking_blocks(msg.reasoning_artifact.as_ref())? {
                        blocks.push(block);
                    }
                }
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

    Ok((payloads, system))
}

/// Map tool definitions to Anthropic tool format.
fn build_tool_payloads(tools: &[ChatToolDefinition]) -> Vec<ToolPayload<'_>> {
    tools
        .iter()
        .map(|t| {
            // Each ChatToolDefinition has a single "function" entry.
            ToolPayload {
                name: &t.function.name,
                description: &t.function.description,
                input_schema: Some(t.function.parameters.clone()),
            }
        })
        .collect()
}

/// Map reasoning slug to Anthropic thinking config.
/// "off" → None (no thinking block).
/// Others → enabled thinking with budget_tokens.
///
/// The catalog advertises the Anthropic effort set as `off` / `minimal` /
/// `low` / `medium` / `high` / `xhigh`; every slug except `off` must map to a
/// real thinking budget — an unmapped slug used to silently disable thinking
/// (only a warn), which made the advertised `minimal`/`xhigh` levels no-ops.
pub(super) fn thinking_payload(slug: &str, max_tokens: u32) -> Option<ThinkingPayload> {
    match slug {
        "off" => None,
        "minimal" => Some(budget_payload(1024, max_tokens)),
        "low" => Some(budget_payload(2048, max_tokens)),
        "medium" => Some(budget_payload(4096, max_tokens)),
        "high" => Some(budget_payload(16384, max_tokens)),
        "xhigh" => Some(budget_payload(32768, max_tokens)),
        // Future: adaptive thinking for newer models
        other => {
            warn!(
                slug = %other,
                "unknown Anthropic reasoning slug, disabling thinking"
            );
            None
        }
    }
}

fn budget_payload(desired: u32, max_tokens: u32) -> ThinkingPayload {
    let budget = desired.min(max_tokens.saturating_sub(1024));
    if budget < desired {
        warn!(
            desired,
            budget, max_tokens, "clamped Anthropic thinking budget_tokens to fit within max_tokens"
        );
    }
    ThinkingPayload {
        kind: "enabled",
        budget_tokens: budget,
    }
}
