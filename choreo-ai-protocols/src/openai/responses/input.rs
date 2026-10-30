//! Lowering of the canonical message list into the Responses API `input` array.
//!
//! The Responses wire format does not take a `messages` list: it takes a flat
//! `input` array of typed items (`message`, `function_call`,
//! `function_call_output`, and the provider's own replayed `reasoning` items),
//! so the canonical
//! [`ChatRequestMessage`](super::super::ChatRequestMessage) history has to be
//! lowered item by item. The item types themselves live in [`super::wire`]; this
//! module is the messages→input builder both Responses request builders call.

use std::collections::HashMap;
use std::io;

use choreo_proto::ReasoningArtifact;
use tracing::{debug, warn};

use super::super::reasoning_details::MAX_REASONING_BYTES;
use super::super::{ChatRequestMessage, OpenAiError};
use super::wire::ResponsesInputItem;
use crate::types::CallerInfo;

/// Where a `system` message belongs on the Responses wire.
///
/// The standard Responses API accepts `{role: "system"}` items inline in
/// `input`, while the stateless ChatGPT/Codex dialect rejects them outright and
/// takes the prompt in the top-level `instructions` field instead — so the
/// lowering is told which of the two it is building for rather than re-deriving
/// it per message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SystemPrompt {
    /// Lower `system` messages into `input` items (the standard API).
    Inline,
    /// Skip them: the caller has hoisted the prompt into `instructions`.
    Hoisted,
}

/// Serialize a Responses input item to its wire JSON value. Kept as a tiny
/// helper because the Responses adapter owns the item type but the conversion
/// happens here, in the shared messages→input builder.
fn responses_input_item_value(item: &ResponsesInputItem) -> Result<serde_json::Value, OpenAiError> {
    serde_json::to_value(item).map_err(|e| OpenAiError::Io(io::Error::other(e)))
}

/// Decode one persisted `ResponsesItems` artifact for replay, or `None` when it
/// cannot be replayed.
///
/// Three refusals, all payload-free and all non-fatal (see the caller): the
/// payload is over the crate's reasoning byte budget — the same number the chat
/// adapter enforces, so a heavy turn cannot ride every later request — it is not
/// the JSON array the capture wrote, or every item in it is refused by
/// [`reasoning_only`].
fn decode_reasoning_items(bytes: &[u8]) -> Option<Vec<serde_json::Value>> {
    if bytes.len() > MAX_REASONING_BYTES {
        warn!(
            payload_bytes = bytes.len(),
            limit = MAX_REASONING_BYTES,
            "persisted responses reasoning artifact exceeds the reasoning byte budget; \
             not replaying it"
        );
        return None;
    }
    if let Ok(items) = serde_json::from_slice::<Vec<serde_json::Value>>(bytes) {
        let items = reasoning_only(items);
        debug!(
            item_count = items.len(),
            "re-emitting responses reasoning items from artifact"
        );
        Some(items)
    } else {
        warn!(
            payload_bytes = bytes.len(),
            "persisted responses reasoning artifact is not a JSON item array; \
             not replaying it"
        );
        None
    }
}

/// Keep only the items that are actually `reasoning` items.
///
/// The replay splices these values into `input` verbatim, ahead of the assistant
/// message they belong to, so an item of any other kind would be an input the
/// request never legitimately produced — a `message` item could inject a turn
/// the conversation never had, and a `function_call_output` would answer a call
/// the API never saw. The capture path only ever stores reasoning items, so this
/// is a shape check on our own cache rather than on provider input: a corrupted
/// or tampered artifact loses its non-reasoning items (with a count logged)
/// instead of steering the request.
fn reasoning_only(items: Vec<serde_json::Value>) -> Vec<serde_json::Value> {
    let total = items.len();
    let kept: Vec<serde_json::Value> = items
        .into_iter()
        .filter(|item| item.get("type").and_then(serde_json::Value::as_str) == Some("reasoning"))
        .collect();
    if kept.len() < total {
        warn!(
            dropped = total - kept.len(),
            kept = kept.len(),
            "persisted responses reasoning artifact carries non-reasoning items; \
             not replaying them"
        );
    }
    kept
}

/// A call id → attribution index, built by the caller from the history it holds.
///
/// The `function_call_output` answering a call has no attribution of its own:
/// the `caller` is provider data that rode the assistant's `function_call`
/// item. A rebuilt history can be unable to see that item — a chained tool
/// round sends only the outputs — so the lookup is passed in rather than
/// derived here, from a slice wide enough to contain the call.
pub(crate) type CallerIndex<'a> = HashMap<&'a str, &'a CallerInfo>;

/// Convert a `ChatRequestMessage` source to Responses API input format.
///
/// Takes an ITERATOR of borrowed messages rather than a slice: both callers
/// have to select which messages go out (a chained turn sends only the
/// post-assistant tail, a stateless one sends the whole history minus the
/// hoisted system prompt), and neither should have to clone the conversation to
/// do it — an earlier slice-shaped signature forced a full deep copy of every
/// prior turn (image bytes included) on every tool-loop iteration.
///
/// `callers` resolves a tool result's attribution by its `call_id`, because the
/// message being lowered is the OUTPUT and the attribution rode the assistant
/// call that is not necessarily in this iteration at all (see [`CallerIndex`]).
///
/// `system` decides what happens to `system`-role messages: emitted as
/// `{role: "system"}` items, or dropped because the caller hoisted them into
/// the top-level `instructions` field. The `instructions` field is otherwise a
/// separate top-level parameter set via explicit provider configuration.
///
/// Assistant messages additionally re-emit the opaque reasoning items from
/// the round-trip artifact (if any) directly into `input`, BEFORE the message
/// content item — mirroring the provider's output ordering where a reasoning
/// item precedes its message. `reasoning_content` is never emitted here:
/// that field is chat-completions-only and invalid on Responses messages.
pub(crate) fn messages_to_responses_input<'a>(
    messages: impl IntoIterator<Item = &'a ChatRequestMessage>,
    system: SystemPrompt,
    callers: &CallerIndex<'a>,
) -> Result<Vec<serde_json::Value>, OpenAiError> {
    let mut items: Vec<serde_json::Value> = Vec::new();

    for msg in messages {
        match msg.role {
            "system" => {
                if system == SystemPrompt::Hoisted {
                    continue;
                }
                if let Some(ref content) = msg.content {
                    items.push(responses_input_item_value(&ResponsesInputItem::Message {
                        role: "system".to_string(),
                        content: serde_json::Value::String(content.clone()),
                    })?);
                }
            }
            "user" | "assistant" => {
                // Assistant turns replay their opaque reasoning items (type
                // tag, id, summary, encrypted_content) verbatim, placed ahead
                // of the message content exactly as the provider emitted them
                // in the original output array.
                //
                // A corrupted or over-budget persisted payload is dropped with a
                // warning rather than failing the request: the artifact is this
                // client's own cache of the provider's output, so losing it costs
                // reasoning continuity for that turn, while a hard error would
                // make every later turn of the session unsendable. The chat
                // adapter lowers a bad payload the same way.
                if msg.role == "assistant"
                    && let Some(ReasoningArtifact::ResponsesItems(bytes)) = &msg.reasoning_artifact
                    && let Some(reasoning_items) = decode_reasoning_items(bytes)
                {
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
                    items.push(responses_input_item_value(&ResponsesInputItem::Message {
                        role: msg.role.to_string(),
                        content: msg.responses_content_value(),
                    })?);
                }
                // An assistant turn's tool requests are their own input items,
                // placed after its message exactly as the provider emitted
                // them. They are what every later `function_call_output` calls
                // back to by `call_id`: a rebuilt history that omitted them
                // would send an output for a call the API never saw introduced.
                //
                // The call's own `caller` rides along verbatim: the service
                // matches it against the output that answers the call to resume
                // the generated program.
                if msg.role == "assistant" {
                    for call in msg.tool_calls.iter().flatten() {
                        items.push(responses_input_item_value(
                            &ResponsesInputItem::FunctionCall {
                                call_id: call.id.clone(),
                                name: call.function.name.clone(),
                                arguments: call.function.arguments.clone(),
                                caller: call.caller.clone(),
                            },
                        )?);
                    }
                }
            }
            "tool" => {
                // Tool results become function_call_output items, each carrying
                // the attribution of the call it answers. The attribution is
                // provider data the tool message never holds, so it comes from
                // the index the caller built over the FULL history: in a chained
                // tool round the assistant message that carried the call is not
                // part of this iteration, and dropping the caller there would
                // leave the service unable to resume the program that made the
                // call.
                if let Some(ref call_id) = msg.tool_call_id
                    && let Some(ref content) = msg.content
                {
                    items.push(responses_input_item_value(
                        &ResponsesInputItem::FunctionCallOutput {
                            call_id: call_id.clone(),
                            output: content.clone(),
                            caller: callers.get(call_id.as_str()).map(|c| (*c).clone()),
                        },
                    )?);
                }
            }
            _ => {
                warn!(
                    role = %msg.role,
                    "unexpected message role in messages_to_responses_input"
                );
            }
        }
    }

    debug!(item_count = items.len(), "messages_to_responses_input");

    Ok(items)
}
