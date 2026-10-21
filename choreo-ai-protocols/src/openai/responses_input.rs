//! Lowering of the canonical message list into the Responses API `input` array.
//!
//! The Responses wire format does not take a `messages` list: it takes a flat
//! `input` array of typed items (`message`, `function_call`,
//! `function_call_output`, and the provider's own replayed `reasoning` items),
//! so the canonical
//! [`ChatRequestMessage`] slice has to be lowered item by item. The item types
//! themselves belong to the Responses adapter (see [`super::responses`]); this
//! module is only the shared messages→input builder both Responses request
//! builders call.

use std::io;

use choreo_proto::ReasoningArtifact;
use tracing::{debug, warn};

use super::responses;
use super::{ChatRequestMessage, OpenAiError};

/// Serialize a Responses input item to its wire JSON value. Kept as a tiny
/// helper because the Responses adapter owns the item type but the conversion
/// happens here, in the shared messages→input builder.
fn responses_input_item_value(
    item: &responses::ResponsesInputItem,
) -> Result<serde_json::Value, OpenAiError> {
    serde_json::to_value(item).map_err(|e| OpenAiError::Io(io::Error::other(e)))
}

/// Convert `ChatRequestMessage` slice to Responses API input format.
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
                        &responses::ResponsesInputItem::Message {
                            role: "system".to_string(),
                            content: serde_json::Value::String(content.clone()),
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
                // Content is a plain string, or an array of input_text/
                // input_image parts when the message carries images. A message
                // with neither text nor images is skipped (an empty input item
                // would be meaningless) — and for an assistant turn the
                // replayed reasoning items above still ride, so a
                // reasoning-only turn keeps its continuity payload without an
                // empty message beside it.
                if msg
                    .content
                    .as_deref()
                    .is_some_and(|content| !content.trim().is_empty())
                    || !msg.images.is_empty()
                {
                    items.push(responses_input_item_value(
                        &responses::ResponsesInputItem::Message {
                            role: msg.role.to_string(),
                            content: msg.responses_content_value(),
                        },
                    )?);
                }
                // An assistant turn's tool requests are their own input items,
                // placed after its message exactly as the provider emitted
                // them. They are what every later `function_call_output` calls
                // back to by `call_id`: a rebuilt history that omitted them
                // would send an output for a call the API never saw introduced.
                if msg.role == "assistant" {
                    for call in msg.tool_calls.iter().flatten() {
                        items.push(responses_input_item_value(
                            &responses::ResponsesInputItem::FunctionCall {
                                call_id: call.id.clone(),
                                name: call.function.name.clone(),
                                arguments: call.function.arguments.clone(),
                                caller: None,
                            },
                        )?);
                    }
                }
            }
            "tool" => {
                // Tool results become function_call_output items
                if let Some(ref call_id) = msg.tool_call_id
                    && let Some(ref content) = msg.content
                {
                    items.push(responses_input_item_value(
                        &responses::ResponsesInputItem::FunctionCallOutput {
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
