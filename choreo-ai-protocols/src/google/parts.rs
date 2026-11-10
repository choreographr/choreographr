//! The Gemini `generateContent` request body.
//!
//! Owns the half of the adapter that the API's documented request schema
//! defines: the `contents` array and the `parts` inside it (text, an echoed
//! `thoughtSignature`, `functionCall`, `functionResponse`, and `inlineData`),
//! the top-level `systemInstruction`, the `tools` wrapper, and the
//! `thinkingConfig` / `generationConfig` knobs.
//!
//! The client, the HTTP transport and the response parsing stay in the parent
//! module. The request shape is kept apart for two reasons. It is the half of
//! the adapter that the provider's own documentation pins down — the wire keys
//! here (`systemInstruction`, `functionCall`, `inlineData`, …) are Gemini's,
//! not ours, so reading this one file answers "what exactly do we send". And it
//! is the half that is *lowered into*: the daemon's message list
//! ([`ChatRequestMessage`]) is translated into these payloads, so the layout
//! documented here is what every chat turn becomes before it leaves the process.
//!
//! Every item is `pub(super)`: the parent re-exports the ones its siblings
//! reach for, so `requests` and `tests` keep importing them through `super::`,
//! exactly as when they lived in `mod.rs`.

use std::collections::HashMap;

use serde::Serialize;
use tracing::{debug, warn};

use super::{
    ChatImagePart, ChatRequestMessage, ChatToolDefinition, GoogleError, attach_thought_signatures,
};
use itertools::Itertools;

/// Request body for POST /v1beta/models/{model}:generateContent.
#[derive(Debug, Serialize)]
pub(super) struct GenerateContentRequest<'a> {
    pub(super) contents: Vec<ContentPayload<'a>>,
    /// The API's own field name is `systemInstruction`; the Rust identifier is
    /// `snake_case`, so the rename is what puts the documented key on the wire.
    #[serde(skip_serializing_if = "Option::is_none", rename = "systemInstruction")]
    pub(super) system_instruction: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) tools: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none", rename = "thinkingConfig")]
    pub(super) thinking_config: Option<ThinkingConfigPayload>,
    /// Output-length cap. `None` keeps the model default (so ordinary turns are
    /// unchanged); the cache-warming ping sets `maxOutputTokens` to cap its
    /// output. `skip_serializing_if` keeps the field off the wire otherwise.
    #[serde(skip_serializing_if = "Option::is_none", rename = "generationConfig")]
    pub(super) generation_config: Option<GenerationConfigPayload>,
}

#[derive(Debug, Serialize)]
pub(super) struct GenerationConfigPayload {
    #[serde(rename = "maxOutputTokens")]
    pub(super) max_output_tokens: u32,
}

#[derive(Debug, Serialize)]
pub(super) struct ThinkingConfigPayload {
    #[serde(rename = "includeThoughts")]
    pub(super) include_thoughts: bool,
}

#[derive(Debug, Serialize)]
pub(super) struct ContentPayload<'a> {
    pub(super) role: &'a str,
    pub(super) parts: Vec<PartPayload<'a>>,
}

#[derive(Debug, Serialize)]
#[serde(untagged)]
pub(super) enum PartPayload<'a> {
    Text {
        text: &'a str,
        /// Encrypted thought signature echoed back from a previous turn
        /// (Gemini allows `thoughtSignature` on ANY part; see
        /// `super::attach_thought_signatures`). Absent on ordinary parts.
        #[serde(skip_serializing_if = "Option::is_none", rename = "thoughtSignature")]
        thought_signature: Option<String>,
    },
    FunctionCall {
        #[serde(rename = "functionCall")]
        function_call: FunctionCallPayload<'a>,
        #[serde(skip_serializing_if = "Option::is_none", rename = "thoughtSignature")]
        thought_signature: Option<String>,
    },
    FunctionResponse {
        #[serde(rename = "functionResponse")]
        function_response: FunctionResponsePayload<'a>,
    },
    /// Gemini vision image: `{"inlineData":{"mimeType":…,"data":<base64>}}`.
    /// `data` is owned (the base64 string is built at request time);
    /// `mime_type` borrows from the message and serializes as `mimeType`.
    InlineData {
        #[serde(rename = "inlineData")]
        inline_data: InlineDataPayload<'a>,
    },
}

#[derive(Debug, Serialize)]
pub(super) struct InlineDataPayload<'a> {
    #[serde(rename = "mimeType")]
    pub(super) mime_type: &'a str,
    pub(super) data: String,
}

#[derive(Debug, Serialize)]
pub(super) struct FunctionCallPayload<'a> {
    pub(super) name: &'a str,
    pub(super) args: serde_json::Value,
}

#[derive(Debug, Serialize)]
pub(super) struct FunctionResponsePayload<'a> {
    pub(super) name: &'a str,
    pub(super) response: serde_json::Value,
}

/// Build a Gemini `inlineData` part from a vision image part, or `None` for
/// an unsupported MIME type (the caller keeps the text it already emitted).
/// Gemini's `inlineData` is the user-role image carrier; `data` is the
/// base64 payload (owned), `mimeType` borrows from the `mime_type` field.
pub(super) fn google_inline_image(image: &ChatImagePart) -> Option<PartPayload<'_>> {
    // Gemini accepts PNG/JPEG/WEBP/GIF/HEIC/HEIF inline. The allowlist below is
    // a superset of what the daemon's `image_prep` normalization can produce
    // (it only ever re-encodes to PNG or JPEG, never WEBP), kept permissive so
    // GIF/WEBP could pass through should they ever arrive, while still
    // rejecting unsupported blobs before they reach the provider.
    const SUPPORTED: [&str; 4] = ["image/png", "image/jpeg", "image/webp", "image/gif"];
    // Base64 engine trait, hoisted to the first statement per
    // clippy::items_after_statements.
    use base64::Engine as _;
    if !SUPPORTED.contains(&image.mime_type.as_str()) {
        return None;
    }
    let data = base64::engine::general_purpose::STANDARD.encode(&image.data);
    Some(PartPayload::InlineData {
        inline_data: InlineDataPayload {
            mime_type: &image.mime_type,
            data,
        },
    })
}

/// Convert a list of messages into Gemini contents format.
///
/// System messages are collected into the top-level `systemInstruction` field
/// and are NOT added to `contents` because Gemini (unlike `OpenAI`) uses a
/// separate top-level field for system instructions, and putting them in
/// contents would cause a rejection.
///
/// Assistant messages use role "model" (Gemini's term for the model, matching
/// the API's role enum). Tool results use role "user" with functionResponse
/// parts because Gemini requires tool responses to be sent under the user
/// role. A call turn's results are emitted together in one content, each named
/// after the call it answers, and a result that answers no declared call is
/// dropped.
pub(super) fn build_message_payloads<'a>(
    messages: &'a [ChatRequestMessage],
) -> Result<(Vec<ContentPayload<'a>>, Option<String>), GoogleError> {
    // System texts borrowed verbatim and joined once at the end — `.format`
    // places the newline separators, so nothing needs cloning into an owned
    // string (and multiple system segments can never drift apart).
    let mut system_texts: Vec<&str> = Vec::new();
    let mut payloads: Vec<ContentPayload<'a>> = Vec::new();

    // First pass: index every function call an assistant turn declared, so a
    // tool result can be named after the call it answers. Gemini matches a
    // `functionResponse` against the `functionCall` by NAME, not by id, so the
    // id alone is useless on the wire. The name is resolved here from the
    // declaring message rather than copied onto the result, so the two can
    // never drift apart.
    let mut call_names: HashMap<&'a str, &'a str> = HashMap::new();
    for msg in messages {
        if let Some(calls) = &msg.tool_calls {
            for tc in calls {
                call_names.insert(tc.id.as_str(), tc.function.name.as_str());
            }
        }
    }

    // Tool results accumulate here so a call turn's whole response set is
    // emitted as ONE user content. Gemini requires the content immediately
    // after a `functionCall` turn to carry exactly one `functionResponse` per
    // call of that turn; splitting them across contents violates that count.
    let mut pending_tool_parts: Vec<PartPayload<'a>> = Vec::new();

    for msg in messages {
        // Tool results are grouped, not emitted inline: hold this one until the
        // run of tool messages ends, then flush the batch as a single content.
        if msg.role == "tool" {
            // Resolve the answering call's name through the first-pass index. A
            // result that answers no declared call is dropped rather than sent
            // nameless (Gemini rejects a `functionResponse` with no name, and a
            // mismatched name is an INVALID_ARGUMENT).
            let name = msg
                .tool_call_id
                .as_deref()
                .and_then(|id| call_names.get(id).copied());
            let Some(name) = name else {
                debug!("dropping a tool result that answers no declared call");
                continue;
            };
            let text = msg.content.as_deref().unwrap_or("");
            pending_tool_parts.push(PartPayload::FunctionResponse {
                function_response: FunctionResponsePayload {
                    name,
                    response: serde_json::json!({"content": text}),
                },
            });
            continue;
        }

        // Any non-tool message ends the run of tool results: flush the batch
        // immediately BEFORE the current message's own content, preserving the
        // call-turn-then-response-turn adjacency Gemini requires.
        if !pending_tool_parts.is_empty() {
            payloads.push(ContentPayload {
                role: "user",
                parts: std::mem::take(&mut pending_tool_parts),
            });
        }

        match msg.role {
            "system" => {
                if let Some(ref content) = msg.content {
                    system_texts.push(content);
                }
            }
            "assistant" => {
                let mut parts: Vec<PartPayload<'a>> = Vec::new();
                if let Some(text) = msg.content.as_deref().filter(|t| !t.is_empty()) {
                    parts.push(PartPayload::Text {
                        text,
                        thought_signature: None,
                    });
                }
                if let Some(ref calls) = msg.tool_calls {
                    for tc in calls {
                        // Gemini requires `args` to be a JSON object (a Struct);
                        // a malformed or non-object payload would serialize as
                        // `null`/a scalar and be rejected, so fall back to an
                        // empty object instead.
                        let args: serde_json::Value =
                            match serde_json::from_str(&tc.function.arguments) {
                                Ok(value @ serde_json::Value::Object(_)) => value,
                                Ok(_) | Err(_) => serde_json::json!({}),
                            };
                        parts.push(PartPayload::FunctionCall {
                            function_call: FunctionCallPayload {
                                name: &tc.function.name,
                                args,
                            },
                            thought_signature: None,
                        });
                    }
                }
                // Echo the previous turn's encrypted thought signatures back
                // to the provider (Gemini requires them for reasoning
                // continuity across turns). See `super::attach_thought_signatures`
                // for the attachment policy.
                attach_thought_signatures(&mut parts, msg.reasoning_artifact.as_ref())?;
                // A message with no parts at all (no text, no tool calls, no
                // replayed signatures) carries nothing: providers reject an
                // empty parts array, and an assistant turn that produced no
                // visible output gives the model nothing to read.
                if parts.is_empty() {
                    debug!("skipping an assistant message with no content parts");
                    continue;
                }
                payloads.push(ContentPayload {
                    role: "model",
                    parts,
                });
            }
            role => {
                // User or other roles: text parts, plus an `inlineData` part
                // per attached image (vision). Images ride user messages (the
                // daemon defers tool-result images to synthetic user messages).
                let mut parts: Vec<PartPayload<'a>> = Vec::new();
                let content = msg.content.as_deref().unwrap_or("");
                if !content.is_empty() {
                    parts.push(PartPayload::Text {
                        text: content,
                        thought_signature: None,
                    });
                }
                for image in &msg.images {
                    if let Some(part) = google_inline_image(image) {
                        parts.push(part);
                    }
                }
                payloads.push(ContentPayload { role, parts });
            }
        }
    }

    // A conversation that ends on a tool result never reaches the mid-loop
    // flush, so drain any trailing batch here.
    if !pending_tool_parts.is_empty() {
        payloads.push(ContentPayload {
            role: "user",
            parts: pending_tool_parts,
        });
    }

    let system_instruction = if system_texts.is_empty() {
        None
    } else {
        Some(system_texts.iter().format("\n").to_string())
    };

    Ok((payloads, system_instruction))
}

/// Map tool definitions to Gemini's tool format.
///
/// Gemini wraps function declarations inside a `functionDeclarations` array per
/// tool entry (rather than `OpenAI`'s flat `tools` array), because the Gemini API
/// supports non-function tool types (like code execution and retrieval) that
/// share the same tool wrapper.
pub(super) fn build_tool_payloads(tools: &[ChatToolDefinition]) -> serde_json::Value {
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

/// Map thinking effort slug to Google's thinkingConfig.
/// Only `"on"` enables thinking; `"off"` omits the config block.
/// Any other slug is rejected (returns None) and logged as a warning.
pub(super) fn thinking_config_payload(slug: &str) -> Option<ThinkingConfigPayload> {
    match slug {
        "off" => {
            debug!("Google thinking: off, omitting thinkingConfig");
            None
        }
        "on" => {
            debug!("Google thinking: on, sending thinkingConfig");
            Some(ThinkingConfigPayload {
                include_thoughts: true,
            })
        }
        other => {
            warn!("Google thinking: unsupported slug '{other}', valid values are 'on'/'off'");
            None
        }
    }
}
