//! The canonical chat message and tool-definition types every adapter
//! translates from.
//!
//! A [`ChatRequestMessage`] is provider-neutral: it carries the role, text,
//! images, requested tool calls, and the opaque reasoning round-trip artifact,
//! and each adapter renders it in its own wire format (chat completions,
//! Responses, Anthropic Messages, Google Gemini). The types live here rather
//! than beside the `OpenAI` client because the client is only one of their
//! consumers — the Anthropic and Google clients and the daemon build the same
//! structures — so this module owns the shared vocabulary while [`super`] owns
//! the client that happens to define it first.
//!
//! [`validate_tool_call_arguments`] lives here too: it is a message-level
//! check (are the accumulated tool calls parseable?) that both the chat and the
//! Responses adapters apply to their own turn results.

use choreo_proto::ReasoningArtifact;
use serde::{Deserialize, Serialize};
use tracing::warn;

use super::AllowedCaller;
use crate::CallerInfo;

/// A tool definition advertised to an OpenAI-compatible provider.
#[derive(Debug, Clone, Serialize)]
pub struct ChatToolDefinition {
    #[serde(rename = "type")]
    kind: &'static str,
    /// The single `function` entry the definition wraps.
    pub function: ChatToolFunction,
}

/// The function a [`ChatToolDefinition`] wraps: its name, description, and
/// JSON-Schema parameters, plus optional programmatic-calling metadata.
#[derive(Debug, Clone, Serialize)]
pub struct ChatToolFunction {
    /// Function name the model must call.
    pub name: String,
    /// Human-readable description guiding the model's use of the tool.
    pub description: String,
    /// JSON Schema for the function's arguments.
    pub parameters: serde_json::Value,
    /// Optional JSON Schema for the tool's output (programmatic tool calling).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output_schema: Option<serde_json::Value>,
    /// Optional allowlist of callers permitted to invoke the tool.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub allowed_callers: Option<Vec<AllowedCaller>>,
}

/// A tool call echoed back to the provider on an assistant message.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AssistantToolCall {
    /// Provider-assigned id of the call.
    pub id: String,
    /// The call type tag (always `"function"`).
    #[serde(rename = "type")]
    pub kind: String,
    /// The function the model called.
    pub function: AssistantToolFunction,
    /// The provider's attribution for the call, when it sent one (programmatic
    /// tool calling: `<https://developers.openai.com/api/docs/guides/tools-programmatic-tool-calling>`).
    ///
    /// This is the canonical message type, so the field lives here for the
    /// adapters to lower into their own wire shapes; it is deliberately
    /// skipped when absent, because a chat-completions `tool_calls` entry has
    /// no such field and must never gain one from a message that carries no
    /// attribution.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub caller: Option<CallerInfo>,
}

/// The function half of an [`AssistantToolCall`]: name plus the raw argument
/// string as the provider emitted it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AssistantToolFunction {
    /// Function name.
    pub name: String,
    /// The arguments as a raw JSON string (kept verbatim, never re-encoded).
    pub arguments: String,
}

/// One image carried by a user chat message, sent to the provider as a
/// vision content part. The daemon attaches these (attached images, or a
/// `read_image` tool result re-read at request time); each provider adapter
/// renders them in its own wire format:
///
/// - `OpenAI` Chat Completions → `{"type":"image_url","image_url":{"url":"data:…;base64,…","detail":"auto"}}`
/// - `OpenAI` Responses API → `{"type":"input_image","image_url":"data:…;base64,…"}` (flat string)
/// - Anthropic Messages → `{"type":"image","source":{"type":"base64","media_type":…,"data":…}}`
/// - Google Gemini → `{"inline_data":{"mime_type":…,"data":…}}`
///
/// Bytes are the raw encoded image (PNG/JPEG/WebP/GIF), not base64; the
/// serializers encode them. Only `user`-role messages carry images (per
/// provider restrictions), which the daemon guarantees by deferring tool
/// images to a synthetic user message.
#[derive(Debug, Clone)]
pub struct ChatImagePart {
    /// Raw encoded image bytes.
    pub data: Vec<u8>,
    /// Image MIME type (e.g. `image/png`).
    pub mime_type: String,
}

impl ChatImagePart {
    /// `data:<mime>;base64,<bytes>` URL — the wire value for `OpenAI`
    /// `image_url.url` (chat and Responses).
    #[must_use]
    pub fn data_url(&self) -> String {
        use base64::Engine as _;
        let b64 = base64::engine::general_purpose::STANDARD.encode(&self.data);
        format!("data:{};base64,{}", self.mime_type, b64)
    }
}

/// One message in a chat conversation, in the canonical `OpenAI` shape the
/// other adapters translate from.
///
/// Deliberately **not** `Serialize`: the chat-completions wire shape is
/// [`ChatWireMessage`](super::ChatWireMessage), because the reasoning field a
/// route demands belongs to
/// the *route*, not to the message. A message therefore cannot reach the wire
/// without an echo policy — the mistake this split exists to prevent.
#[derive(Debug, Clone)]
pub struct ChatRequestMessage {
    /// Message role (`"system"`, `"user"`, `"assistant"`, or `"tool"`).
    pub role: &'static str,
    /// Text content of the message, when it has any.
    pub content: Option<String>,
    /// Images attached to this message (user role). Empty for text-only
    /// messages. Rendered per-provider by the serializers; see
    /// [`ChatImagePart`].
    pub images: Vec<ChatImagePart>,
    /// For a `"tool"` message, the id of the call it answers.
    pub tool_call_id: Option<String>,
    /// For an `"assistant"` message, the tool calls it requested.
    pub tool_calls: Option<Vec<AssistantToolCall>>,
    /// Opaque reasoning round-trip artifact captured by the producing adapter
    /// at parse time (see [`ReasoningArtifact`]). Never serialized as a field
    /// of its own — each adapter re-emits it in ITS OWN wire format: chat
    /// writes it under the route's declared reasoning field (see
    /// [`ChatWireMessage`](super::ChatWireMessage)), Responses pushes the items
    /// into `input`, and the
    /// Anthropic/Google builders interpret their own variants.
    pub reasoning_artifact: Option<ReasoningArtifact>,
}

impl ChatRequestMessage {
    /// Build a text-only message of the given role.
    #[must_use]
    pub fn simple(role: &'static str, content: String) -> Self {
        ChatRequestMessage::with_images(role, content, Vec::new())
    }

    /// Build a text + image user message (vision input). `content` is the
    /// lead-in text rendered ahead of the images; each provider adapter emits
    /// the images in its own wire format. Only `user`-role messages carry
    /// images (the daemon guarantees this by deferring tool images to
    /// synthetic user messages).
    #[must_use]
    pub fn with_images(role: &'static str, content: String, images: Vec<ChatImagePart>) -> Self {
        ChatRequestMessage {
            role,
            content: Some(content),
            images,
            tool_call_id: None,
            tool_calls: None,
            reasoning_artifact: None,
        }
    }

    /// The `OpenAI` chat-completions `content` value for this message: a plain
    /// string when it carries no images, or an array of `{type:"text"}` and
    /// `{type:"image_url"}` parts when it does. `detail` is fixed to `"auto"`
    /// (the provider decides; see the vision plan).
    #[must_use]
    pub fn openai_chat_content_value(&self) -> serde_json::Value {
        if self.images.is_empty() {
            serde_json::Value::String(self.content.clone().unwrap_or_default())
        } else {
            let mut parts: Vec<serde_json::Value> = Vec::new();
            if let Some(text) = self.content.as_deref().filter(|t| !t.is_empty()) {
                parts.push(serde_json::json!({ "type": "text", "text": text }));
            }
            for image in &self.images {
                parts.push(serde_json::json!({
                    "type": "image_url",
                    "image_url": {
                        "url": image.data_url(),
                        "detail": "auto",
                    }
                }));
            }
            serde_json::Value::Array(parts)
        }
    }

    /// The Responses API `input` content value for a user/assistant message:
    /// a plain string when image-free, or an array of `input_text` and
    /// `input_image` parts when it carries images. `input_image.image_url` is a
    /// **flat string** (the Responses gotcha — it is not the nested object the
    /// chat-completions `image_url` uses). `detail` fixed to `"auto"`.
    #[must_use]
    pub fn responses_content_value(&self) -> serde_json::Value {
        if self.images.is_empty() {
            serde_json::Value::String(self.content.clone().unwrap_or_default())
        } else {
            let mut parts: Vec<serde_json::Value> = Vec::new();
            if let Some(text) = self.content.as_deref().filter(|t| !t.is_empty()) {
                parts.push(serde_json::json!({ "type": "input_text", "text": text }));
            }
            for image in &self.images {
                parts.push(serde_json::json!({
                    "type": "input_image",
                    "image_url": image.data_url(),
                    "detail": "auto",
                }));
            }
            serde_json::Value::Array(parts)
        }
    }
}

impl ChatToolDefinition {
    /// Build a plain function tool definition (no output schema, no caller
    /// allowlist).
    #[must_use]
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

    /// Create a tool definition with `output_schema` and `allowed_callers`.
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
