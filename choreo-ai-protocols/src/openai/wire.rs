//! The chat-completions wire projection of a canonical message slice.
//!
//! This module owns the one place where a [`ChatRequestMessage`] becomes an
//! `OpenAI` Chat Completions payload: **which reasoning key** a message carries
//! (a scalar field, the structured `reasoning_details` array, or none), and
//! **whether the message survives the projection at all** (an assistant turn
//! that would carry nothing must not be sent).
//!
//! The reasoning emission is decided once, at lowering time, and stored on the
//! [`ChatWireMessage`]: the serializer and the drop predicate both read that one
//! value, so presence and destination cannot drift apart, and a structured
//! artifact is parsed once per request rather than once per reader. Retention of
//! the artifact capture/normalization rules lives in
//! [`super::reasoning_details`]; this module only projects what was captured.

use super::ChatRequestMessage;
use super::reasoning_details::{details_companion, details_display_text, details_wire_value};
use crate::catalog::ChatReasoningEcho;
use choreo_proto::{ChatReasoningField, ReasoningArtifact};
use serde::Serialize;
use tracing::{debug, warn};

/// Every chat-completions reasoning wire key, in the order diagnostics report
/// them. Shared so a diagnostic cannot drift from the lowering.
pub const CHAT_REASONING_WIRE_KEYS: [&str; 4] = [
    "reasoning_content",
    "reasoning",
    "reasoning_text",
    "reasoning_details",
];

/// One `OpenAI` Chat Completions wire message: a [`ChatRequestMessage`] lowered
/// under one route's [`ChatReasoningEcho`] policy.
///
/// Serialization lives here rather than on the message because reasoning
/// re-emission has two halves that must not be decided in different places:
///
/// - **which key** — the route's declared field, else the field the payload was
///   observed under, else nothing. A declared field always wins: a gateway that
///   streams `reasoning` while rejecting a history without `reasoning_content`
///   is real, so the observation is only a fallback hint.
/// - **whether the key must exist** — under a route that requires the field, an
///   assistant message with nothing to echo carries it as an empty string, or as
///   an empty `reasoning_details` array when the declared field is structured, so
///   no assistant message on such a route can omit it.
///
/// Exactly one wire shape is emitted per message — a scalar key or the
/// structured `reasoning_details` array — and never on a non-assistant role. The
/// only companion field is the Kimi-shaped summary text (`reasoning_content`)
/// emitted beside a bare-dialect details array.
pub struct ChatWireMessage<'a> {
    message: &'a ChatRequestMessage,
    /// The reasoning emission, decided once at lowering time and reused by both
    /// [`ChatWireMessage::is_empty`] and the `Serialize` impl.
    reasoning: Option<WireReasoning>,
}

/// The chat-completions wire view of a message slice under one route policy.
pub struct ChatWireMessages<'a> {
    /// The messages that will actually be sent, in order.
    pub messages: Vec<ChatWireMessage<'a>>,
    /// How many messages were dropped because they would have carried nothing.
    pub dropped: usize,
}

/// Lower a message slice for the chat-completions wire under one route policy.
///
/// Assistant messages that would carry nothing at all — no non-empty content,
/// no tool calls, no images, and no non-empty reasoning payload — are dropped
/// rather than sent empty, because a message with neither content nor tool calls
/// is rejected outright by OpenAI-compatible servers and by chat providers
/// generally, and an assistant turn that produced no visible output carries no
/// information the model can use.
///
/// Two things are deliberately preserved:
///
/// - a message whose only payload is a replayed **reasoning artifact** is not
///   empty and always survives (dropping it would starve the echo the route
///   requires — the failure this projection must not reintroduce);
/// - a required-field *empty* string is not a payload, so it does not keep an
///   otherwise-empty message alive.
///
/// Only the assistant role is dropped: the history always keeps at least the
/// user/system turns it was built from, so a request can never end up with an
/// empty `messages` array.
#[must_use]
pub fn chat_wire_messages(
    messages: &[ChatRequestMessage],
    echo: ChatReasoningEcho,
) -> ChatWireMessages<'_> {
    let mut kept = Vec::with_capacity(messages.len());
    let mut dropped = 0usize;
    for message in messages {
        let wire = ChatWireMessage {
            message,
            reasoning: lower_reasoning(message.role, message.reasoning_artifact.as_ref(), echo),
        };
        if wire.is_empty() {
            dropped += 1;
            continue;
        }
        kept.push(wire);
    }
    if dropped > 0 {
        debug!(
            dropped,
            sent = kept.len(),
            "dropped empty chat messages from the request"
        );
    }
    ChatWireMessages {
        messages: kept,
        dropped,
    }
}

/// Lower one message for the chat-completions wire under one route policy.
///
/// Prefer [`chat_wire_messages`] for a request build: it also drops the
/// assistant messages that would carry nothing. This projection keeps the
/// message as given, for diagnostics that must see every turn.
#[must_use]
pub fn chat_wire_message(
    message: &ChatRequestMessage,
    echo: ChatReasoningEcho,
) -> ChatWireMessage<'_> {
    ChatWireMessage {
        message,
        reasoning: lower_reasoning(message.role, message.reasoning_artifact.as_ref(), echo),
    }
}

/// Whether an assistant message carrying exactly these payloads would be
/// dropped from the chat-completions projection.
///
/// This is the **single** drop predicate: [`ChatWireMessage::is_empty`] answers
/// through it, and diagnostics (`session_inspect`) call it directly, so a
/// diagnostic can neither invent nor miss a drop the real projection makes.
/// Blank (whitespace-only) content counts as none, and a presence-marker
/// emission (an empty scalar string or an empty `reasoning_details` array) is
/// not a payload.
#[must_use]
pub fn chat_assistant_payload_is_empty(
    content: Option<&str>,
    has_tool_calls: bool,
    has_images: bool,
    artifact: Option<&ReasoningArtifact>,
    echo: ChatReasoningEcho,
) -> bool {
    let reasoning = lower_reasoning("assistant", artifact, echo);
    payload_is_empty(content, has_tool_calls, has_images, reasoning.as_ref())
}

/// The drop decision over an already-lowered reasoning emission.
///
/// Split out so the projection reuses the emission it computed at lowering time
/// instead of lowering a second time, while still routing every caller through
/// one decision.
fn payload_is_empty(
    content: Option<&str>,
    has_tool_calls: bool,
    has_images: bool,
    reasoning: Option<&WireReasoning>,
) -> bool {
    let has_content = content.is_some_and(|content| !content.trim().is_empty());
    let has_reasoning = reasoning.is_some_and(WireReasoning::has_payload);
    !has_content && !has_tool_calls && !has_images && !has_reasoning
}

/// The wire key for one chat reasoning field.
///
/// [`ChatReasoningField::ReasoningDetails`] maps to the structured
/// `reasoning_details` array; it is never used as a scalar key (see
/// [`WireReasoning`]). This is the lowering's own mapping — a new
/// `ChatReasoningField` variant is deliberately a two-place edit, because
/// [`CHAT_REASONING_WIRE_KEYS`] (what diagnostics scan for) must list the same
/// keys.
#[must_use]
fn reasoning_wire_key(field: ChatReasoningField) -> &'static str {
    match field {
        ChatReasoningField::ReasoningContent => "reasoning_content",
        ChatReasoningField::Reasoning => "reasoning",
        ChatReasoningField::ReasoningText => "reasoning_text",
        ChatReasoningField::ReasoningDetails => "reasoning_details",
    }
}

/// How one assistant message re-emits its reasoning on the chat wire.
///
/// Two shapes, never mixed: a scalar field (`reasoning_content`, `reasoning`,
/// or `reasoning_text`) or the structured `reasoning_details` array. Both are
/// decided in exactly one place ([`lower_reasoning`]), so a message never
/// carries two reasoning keys and the two catalog facts (destination, presence)
/// are applied consistently.
#[derive(Debug, Clone)]
enum WireReasoning {
    /// A scalar chat field carrying the artifact's text (or an empty string as
    /// a presence marker on a declaring route).
    Scalar {
        /// The wire key (`reasoning_content` and friends).
        key: &'static str,
        /// The value to write under that key.
        value: String,
    },
    /// The structured `reasoning_details` array, replayed verbatim for the
    /// `OpenRouter` dialect and rebuilt minimally for Kimi-shaped entries.
    Details(serde_json::Value),
}

impl WireReasoning {
    /// Whether this emission carries a real payload (as opposed to a presence
    /// marker: an empty scalar string or an empty details array).
    fn has_payload(&self) -> bool {
        match self {
            WireReasoning::Scalar { value, .. } => !value.trim().is_empty(),
            WireReasoning::Details(details) => details
                .as_array()
                .is_some_and(|entries| !entries.is_empty()),
        }
    }
}

/// The reasoning emission one message carries, if any — the single decision
/// point for reasoning re-emission: exactly one wire shape.
fn lower_reasoning(
    role: &str,
    artifact: Option<&ReasoningArtifact>,
    echo: ChatReasoningEcho,
) -> Option<WireReasoning> {
    // Only assistant messages may carry provider reasoning. An artifact on any
    // other role is a builder bug, so it is dropped rather than re-emitted.
    if role != "assistant" {
        return None;
    }
    // The destination: the route's declared field outranks the field the
    // payload was observed under (the observation is only a fallback hint).
    let observed = match artifact {
        Some(ReasoningArtifact::ChatReasoning { field, .. }) => Some(*field),
        _ => None,
    };
    let destination = echo.field.or(observed);

    // Structured `reasoning_details` destination.
    if destination == Some(ChatReasoningField::ReasoningDetails) {
        let details = match artifact {
            Some(ReasoningArtifact::ChatReasoning {
                field: ChatReasoningField::ReasoningDetails,
                bytes,
            }) => details_wire_value(bytes),
            // A scalar artifact on a details route: re-encode its text as one
            // `reasoning.text` entry. A details route can see a scalar artifact
            // when the catalog/overlay flips a model's declared field (the
            // capture recorded the field it was observed under), and a
            // round-trip beats discarding recoverable reasoning text.
            Some(ReasoningArtifact::ChatReasoning { bytes, .. }) => {
                lower_scalar_text_as_details(bytes)
            }
            // Nothing structured to echo (no artifact, or a foreign variant):
            // whether the key goes out empty is the presence decision below.
            _ => serde_json::Value::Array(Vec::new()),
        };
        // An empty array is a presence marker, not a payload, so it goes out
        // only on a route whose catalog fact REQUIRES the field. Everywhere
        // else — an undeclared route that reached this arm only through the
        // observation, or a details route with no presence requirement whose
        // artifact lowered away (corrupt, over cap, or blank text) — the key is
        // omitted rather than inventing a `reasoning_details: []` the route
        // never asked for: an unverified array sent to a gateway that is strict
        // about its contents is worse than omitting it.
        if !echo.require_present && details.as_array().is_some_and(Vec::is_empty) {
            return None;
        }
        debug!(
            entries = details.as_array().map_or(0, Vec::len),
            declared = echo.field.is_some(),
            required = echo.require_present,
            "re-emitting structured reasoning_details"
        );
        return Some(WireReasoning::Details(details));
    }

    // Scalar destination: the artifact's text, if it has any.
    let mut scalar_text: Option<String> = None;
    if let Some(ReasoningArtifact::ChatReasoning { field, bytes }) = artifact {
        match std::str::from_utf8(bytes) {
            Ok(_) if *field == ChatReasoningField::ReasoningDetails => {
                // A structured payload on a scalar route: use its derived
                // display text rather than replaying raw JSON as a string.
                scalar_text = details_display_text(bytes);
            }
            Ok(text) => scalar_text = Some(text.to_string()),
            Err(_) => {
                // A corrupted persisted blob must not fail the whole
                // request: drop the payload so the required-field arm below
                // still keeps the message valid, and log the loss.
                warn!(
                    field = ?destination,
                    payload_bytes = bytes.len(),
                    "dropping corrupt chat reasoning artifact (not valid UTF-8)",
                );
            }
        }
    }
    if let Some(value) = scalar_text {
        let key = reasoning_wire_key(destination.unwrap_or(ChatReasoningField::ReasoningContent));
        debug!(
            field = ?destination,
            declared = echo.field.is_some(),
            payload_bytes = value.len(),
            "re-emitting chat reasoning artifact"
        );
        return Some(WireReasoning::Scalar { key, value });
    }
    // Nothing to echo. On a route that requires the field, emit it empty so
    // the assistant message never omits it (the DeepSeek/GLM 5.x rule);
    // `reasoning_content` is the chat-completions default when the policy
    // names no field of its own.
    echo.require_present.then(|| WireReasoning::Scalar {
        key: reasoning_wire_key(echo.field.unwrap_or(ChatReasoningField::ReasoningContent)),
        value: String::new(),
    })
}

/// Re-encode a scalar artifact's text as the single-entry `reasoning_details`
/// array a structured route replays (see [`lower_reasoning`]).
///
/// Blank text yields the empty array: a whitespace-only artifact is not a
/// payload worth inventing an entry for.
fn lower_scalar_text_as_details(bytes: &[u8]) -> serde_json::Value {
    let text = std::str::from_utf8(bytes)
        .ok()
        .filter(|text| !text.trim().is_empty());
    text.map_or_else(
        || serde_json::Value::Array(Vec::new()),
        |text| {
            serde_json::Value::Array(vec![serde_json::json!({
                "type": "reasoning.text",
                "text": text,
            })])
        },
    )
}

impl ChatWireMessage<'_> {
    /// The message role (`"system"`, `"user"`, `"assistant"`, or `"tool"`).
    #[must_use]
    pub fn role(&self) -> &'static str {
        self.message.role
    }

    /// Whether this message would carry nothing at all on the wire.
    ///
    /// "Nothing" means: no non-empty content (whitespace-only counts as empty —
    /// strict servers treat it as an empty message), no tool calls, no images,
    /// and no non-empty reasoning payload. A required-field *empty* string is
    /// not a payload, so an assistant message that only carries that is empty
    /// too.
    ///
    /// Only assistant messages can be emptied by the projection: every other
    /// role is part of the history the request was built from.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        if self.message.role != "assistant" {
            return false;
        }
        // The shared predicate, fed the emission lowering already computed so a
        // structured artifact is not parsed twice.
        payload_is_empty(
            self.message.content.as_deref(),
            self.message
                .tool_calls
                .as_ref()
                .is_some_and(|calls| !calls.is_empty()),
            !self.message.images.is_empty(),
            self.reasoning.as_ref(),
        )
    }
}

impl Serialize for ChatWireMessage<'_> {
    /// Manual impl: role, content, `tool_call_id`, `tool_calls`, and at most one
    /// reasoning field (plus the Kimi companion beside a details array), with
    /// `None` fields omitted. An assistant message carrying a `ChatReasoning`
    /// artifact re-emits it under the route's field (see [`ChatWireMessage`]);
    /// the artifact itself never appears on the wire.
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeStruct;

        // The count has to be exact — a `serde` struct serializer that is told
        // the wrong length either truncates the output or panics — and it is not
        // a constant: the details arm can write both `reasoning_content` (the
        // Kimi companion) and `reasoning_details`.
        let field_count = 1 // role
            + usize::from(!self.message.images.is_empty() || self.message.content.is_some())
            + usize::from(self.message.tool_call_id.is_some())
            + usize::from(self.message.tool_calls.is_some())
            + match &self.reasoning {
                None => 0,
                Some(WireReasoning::Scalar { .. }) => 1,
                Some(WireReasoning::Details(details)) => {
                    1 + usize::from(details_companion(details).is_some())
                }
            };
        let mut msg = serializer.serialize_struct("ChatWireMessage", field_count)?;
        msg.serialize_field("role", &self.message.role)?;
        // When the message carries images, `content` becomes an array of
        // `{type:"text"/"image_url"}` parts (the OpenAI chat vision shape);
        // otherwise it stays a plain string. Only user messages carry images
        // (the daemon guarantees this), but the branch is unconditional so a
        // misconstructed message still serializes rather than 400-ing later.
        if !self.message.images.is_empty() {
            msg.serialize_field("content", &self.message.openai_chat_content_value())?;
        } else if let Some(content) = &self.message.content {
            msg.serialize_field("content", content)?;
        }
        if let Some(tool_call_id) = &self.message.tool_call_id {
            msg.serialize_field("tool_call_id", tool_call_id)?;
        }
        if let Some(tool_calls) = &self.message.tool_calls {
            msg.serialize_field("tool_calls", tool_calls)?;
        }
        if let Some(reasoning) = &self.reasoning {
            match reasoning {
                WireReasoning::Scalar { key, value } => msg.serialize_field(key, value)?,
                WireReasoning::Details(details) => {
                    // Kimi-shaped payloads also carry the summary text as
                    // `reasoning_content`; the one-channel rule holds only among
                    // the scalar fields, so this companion is deliberate.
                    if let Some(companion) = details_companion(details) {
                        msg.serialize_field("reasoning_content", &companion)?;
                    }
                    msg.serialize_field("reasoning_details", details)?;
                }
            }
        }
        msg.end()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn presence_pin_without_a_destination_emits_an_empty_key() {
        // A route whose catalog fact demands the field without naming one: the
        // lowering falls back to the chat default `reasoning_content` and writes
        // it EMPTY as a presence marker, so the message never omits the key.
        let messages = vec![ChatRequestMessage::simple("assistant", String::new())];
        let echo = ChatReasoningEcho {
            field: None,
            require_present: true,
        };
        let wire = chat_wire_message(&messages[0], echo);
        let json = serde_json::to_value(&wire).expect("serialize");
        assert_eq!(
            json.get("reasoning_content"),
            Some(&serde_json::Value::String(String::new())),
            "the presence pin must put the key on the wire, empty"
        );
        // The empty scalar is a presence marker, not a payload, so it does not
        // keep an otherwise-empty turn alive: the request projection still drops
        // it. The pin only matters on a message that survives for another reason.
        assert!(wire.is_empty());
        assert_eq!(chat_wire_messages(&messages, echo).dropped, 1);
    }

    #[test]
    fn scalar_text_on_a_details_route_becomes_one_text_entry() {
        let artifact = ReasoningArtifact::ChatReasoning {
            field: ChatReasoningField::ReasoningContent,
            bytes: b"recoverable thought".to_vec(),
        };
        let details = lower_reasoning(
            "assistant",
            Some(&artifact),
            ChatReasoningEcho {
                field: Some(ChatReasoningField::ReasoningDetails),
                require_present: true,
            },
        );
        let Some(WireReasoning::Details(details)) = details else {
            panic!("expected a details emission");
        };
        assert_eq!(
            details,
            serde_json::json!([{"type":"reasoning.text","text":"recoverable thought"}])
        );
    }

    #[test]
    fn blank_scalar_text_is_not_a_details_entry() {
        assert_eq!(
            lower_scalar_text_as_details(b"  \n"),
            serde_json::json!([]),
            "blank text must not invent an entry"
        );
        assert_eq!(
            lower_scalar_text_as_details(&[0xFF, 0xFE]),
            serde_json::json!([]),
            "non-UTF-8 bytes must not invent an entry"
        );
    }

    #[test]
    fn wire_keys_cover_every_field_variant() {
        for field in [
            ChatReasoningField::ReasoningContent,
            ChatReasoningField::Reasoning,
            ChatReasoningField::ReasoningText,
            ChatReasoningField::ReasoningDetails,
        ] {
            assert!(
                CHAT_REASONING_WIRE_KEYS.contains(&reasoning_wire_key(field)),
                "every field variant's key must be listed"
            );
        }
    }
}
