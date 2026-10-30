//! The chat-completions message projection: what one message looks like on the
//! wire under a route's reasoning-echo policy.
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

use super::reasoning_details::{MAX_REASONING_BYTES, details_display_text, details_wire_value};
use super::{AssistantToolCall, ChatRequestMessage};
use crate::catalog::ChatReasoningEcho;
use choreo_proto::{ChatReasoningField, ReasoningArtifact};
use serde::Serialize;
use tracing::{debug, warn};

// The per-request replay budget is the crate's one reasoning byte budget: see
// `reasoning_details::MAX_REASONING_BYTES` for why capture and replay share the
// number, and `reasoning_replay_budget` below for how replay spends it.

/// Every chat-completions reasoning wire key, in
/// [`ChatReasoningField::ALL`] order — re-exported from [`choreo_proto`], where
/// it sits beside the field-to-key mapping so a diagnostic, the lowering, and
/// the catalog parser cannot spell one key two ways.
pub use choreo_proto::CHAT_REASONING_WIRE_KEYS;

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
/// structured `reasoning_details` array — and never on a non-assistant role.
/// Never two: sending a bare-dialect array *and* a scalar mirror beside it would
/// be a shape no upstream documents, so the array is the only reasoning field a
/// details route emits.
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
///
/// The reasoning artifacts are also budgeted here rather than per message: the
/// total replayed across the request is what the provider sees, so a turn whose
/// artifact the budget skips is lowered with no artifact at all (see
/// `MAX_REASONING_BYTES`). Per-message projection
/// ([`chat_wire_message`]) deliberately has no budget — it has no request to
/// budget.
#[must_use]
pub fn chat_wire_messages(
    messages: &[ChatRequestMessage],
    echo: ChatReasoningEcho,
) -> ChatWireMessages<'_> {
    let replays = reasoning_replay_budget(messages);
    let mut kept = Vec::with_capacity(messages.len());
    let mut dropped = 0usize;
    for (index, message) in messages.iter().enumerate() {
        // A skipped turn keeps its place in the request and its echo policy; only
        // the payload is withheld, so a route that requires the key still emits
        // its presence marker (an empty string, or the empty details array) and
        // the wire shape stays valid.
        let artifact = message
            .reasoning_artifact
            .as_ref()
            .filter(|_| replays.get(index).copied().unwrap_or(false));
        let wire = ChatWireMessage {
            message,
            reasoning: lower_reasoning(message.role, artifact, echo),
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

/// Decide, per message position, whether that message's reasoning artifact is
/// replayed under `MAX_REASONING_BYTES`.
///
/// The walk is newest-first because the recent echo is the one that matters: the
/// most recent artifact-carrying assistant message always replays, and every
/// older one replays only while the bytes already accepted stay at or below the
/// budget. Because the check happens before an older artifact's bytes are
/// added, the accepted total can reach `MAX_REASONING_BYTES` plus one
/// artifact; that one-artifact overshoot is deliberate (it lets an older turn
/// in while keeping the total bounded by the budget plus the newest turn).
/// Non-assistant messages and messages with no artifact consume no budget, so
/// the decision depends only on the artifacts themselves.
fn reasoning_replay_budget(messages: &[ChatRequestMessage]) -> Vec<bool> {
    let mut replays = vec![false; messages.len()];
    let mut used = 0usize;
    let mut accepted = false;
    let mut skipped = 0usize;
    // The reverse walk pairs each decision slot with its message, so the newest
    // artifact is considered first and no position has to be indexed.
    for (replay, message) in replays.iter_mut().zip(messages).rev() {
        // Only an assistant message carrying a chat artifact replays at all (the
        // lowering drops reasoning on every other role and on every other
        // artifact variant), so only those are charged against the budget.
        let Some(ReasoningArtifact::ChatReasoning { bytes, .. }) = &message.reasoning_artifact
        else {
            continue;
        };
        if message.role != "assistant" {
            continue;
        }
        if accepted && used > MAX_REASONING_BYTES {
            skipped += 1;
            continue;
        }
        *replay = true;
        accepted = true;
        used = used.saturating_add(bytes.len());
    }
    if skipped > 0 {
        // One line per request, never per turn, and payload-free: the counts are
        // what a diagnostic needs, the reasoning itself is never logged.
        warn!(
            skipped,
            budget = MAX_REASONING_BYTES,
            used,
            "replayed reasoning exceeds the per-request budget; dropping the oldest artifacts"
        );
    }
    replays
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
    /// `OpenRouter` dialect and stripped of only the stream `index` for a bare
    /// (undocumented) entry.
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
        // The mapping lives on the field, so every layer spells a key the same way.
        let key = destination
            .unwrap_or(ChatReasoningField::ReasoningContent)
            .wire_name();
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
        key: echo
            .field
            .unwrap_or(ChatReasoningField::ReasoningContent)
            .wire_name(),
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
    /// reasoning field, with `None` fields omitted. An assistant message carrying
    /// a `ChatReasoning` artifact re-emits it under the route's field (see
    /// [`ChatWireMessage`]); the artifact itself never appears on the wire.
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeStruct;

        // The count has to be exact — a `serde` struct serializer that is told
        // the wrong length either truncates the output or panics.
        let field_count = 1 // role
            + usize::from(!self.message.images.is_empty() || self.message.content.is_some())
            + usize::from(self.message.tool_call_id.is_some())
            + usize::from(self.message.tool_calls.is_some())
            + usize::from(self.reasoning.is_some());
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
            msg.serialize_field("tool_calls", &ChatWireToolCalls(tool_calls))?;
        }
        if let Some(reasoning) = &self.reasoning {
            match reasoning {
                WireReasoning::Scalar { key, value } => msg.serialize_field(key, value)?,
                WireReasoning::Details(details) => {
                    msg.serialize_field("reasoning_details", details)?;
                }
            }
        }
        msg.end()
    }
}

/// The `tool_calls` array of an assistant chat message, with each call's
/// provider attribution removed.
///
/// `caller` is a Responses-API concept: it names the program that generated a
/// call so the service can resume it. A Chat Completions `tool_calls` entry has
/// no such field, so the projection strips it — a session that captured an
/// attribution under a Responses model and later sent the same history to a
/// chat provider must not hand that provider a field its wire does not define.
struct ChatWireToolCalls<'a>(&'a [AssistantToolCall]);

impl Serialize for ChatWireToolCalls<'_> {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeSeq;

        let mut seq = serializer.serialize_seq(Some(self.0.len()))?;
        for call in self.0 {
            seq.serialize_element(&ChatWireToolCall(call))?;
        }
        seq.end()
    }
}

/// One chat-completions `tool_calls` entry: exactly `id`, `type`, and
/// `function`, so the attribution of [`ChatWireToolCalls`]'s source call cannot
/// leak onto this wire.
struct ChatWireToolCall<'a>(&'a AssistantToolCall);

impl Serialize for ChatWireToolCall<'_> {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeStruct;

        let mut call = serializer.serialize_struct("ChatWireToolCall", 3)?;
        call.serialize_field("id", &self.0.id)?;
        call.serialize_field("type", &self.0.kind)?;
        call.serialize_field("function", &self.0.function)?;
        call.end()
    }
}

#[cfg(test)]
mod tests;
