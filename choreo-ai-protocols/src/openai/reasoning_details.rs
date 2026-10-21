//! Structured `reasoning_details` capture and lowering for the chat-completions
//! adapter.
//!
//! Several OpenAI-compatible routes (`OpenRouter` and its resellers, a few
//! gateway brands) carry reasoning as a **structured array** rather than a
//! scalar string. The array shape differs by dialect:
//!
//! - **`OpenRouter`** — namespaced entries whose `type` starts with
//!   `reasoning.`: `reasoning.text` (a `text` field), `reasoning.summary` (a
//!   `summary` field), and `reasoning.encrypted` (a `data` field). An entry
//!   also carries provider-opaque metadata (`index`, `format`, `id`,
//!   `signature`) that must survive the round-trip verbatim.
//! - **Kimi-shaped** — bare entries: `{"type":"summary","summary":…,"index":0}`
//!   and `{"type":"encrypted","data":…,"index":0}`. `index` is a stream
//!   bookkeeping field, not part of the replay payload.
//!
//! Two rules shape this module:
//!
//! - **Capture normalizes, replay preserves.** Consecutive same-kind
//!   text/summary entries are merged into one (a streamed block arrives as many
//!   deltas), encrypted entries stay discrete (they are opaque and
//!   individually signed), and unrecognised `type`s are dropped at capture
//!   time. What is persisted is the normalized JSON array.
//! - **Display is derived, never invented.** When no scalar reasoning field
//!   streamed, the turn's display text is the concatenation of the text/summary
//!   entries. An encrypted-only turn still counts as model output even though
//!   it yields no display text.

use choreo_proto::{ChatReasoningField, ReasoningArtifact};
use serde_json::{Map, Value};
use tracing::{debug, warn};

/// Per-turn cap on the serialized `reasoning_details` payload.
///
/// The payload is replayed on every subsequent request of a tool loop, so an
/// unbounded array would compound request size turn over turn. Beyond this
/// limit the details are dropped (and logged) rather than failing the request:
/// losing the reasoning echo degrades a route that requires it, but a
/// multi-megabyte body would fail the whole turn.
pub(crate) const MAX_REASONING_DETAILS_BYTES: usize = 1024 * 1024;

/// The kind of a captured reasoning-details entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DetailKind {
    /// A plain thinking-text block (`reasoning.text` / bare `text`).
    Text,
    /// A summarized thinking block (`reasoning.summary` / bare `summary`).
    Summary,
    /// An opaque encrypted block (`reasoning.encrypted` / bare `encrypted`).
    Encrypted,
}

impl DetailKind {
    /// The entry field carrying this kind's payload.
    fn payload_key(self) -> &'static str {
        match self {
            DetailKind::Text => "text",
            DetailKind::Summary => "summary",
            DetailKind::Encrypted => "data",
        }
    }

    /// Whether entries of this kind can be textually merged.
    fn is_textual(self) -> bool {
        matches!(self, DetailKind::Text | DetailKind::Summary)
    }
}

/// Classify one entry by its `type` tag, accepting both dialects. An unknown
/// `type` yields `None` and is dropped at capture.
fn classify(entry: &Value) -> Option<DetailKind> {
    match entry.get("type").and_then(Value::as_str)? {
        "reasoning.text" | "text" => Some(DetailKind::Text),
        "reasoning.summary" | "summary" => Some(DetailKind::Summary),
        "reasoning.encrypted" | "encrypted" => Some(DetailKind::Encrypted),
        _ => None,
    }
}

/// Whether an entry belongs to the namespace-prefixed `OpenRouter` dialect.
fn is_namespaced(entry: &Value) -> bool {
    entry
        .get("type")
        .and_then(Value::as_str)
        .is_some_and(|ty| ty.starts_with("reasoning."))
}

/// The stream `index` an entry carries, when present.
fn entry_index(entry: &Value) -> Option<u64> {
    entry.get("index").and_then(Value::as_u64)
}

/// Whether two entries describe the same stream slot and may be merged. A
/// missing `index` on either side is treated as compatible: the bare dialect
/// sometimes omits it, and the kind + adjacency already disambiguate.
fn share_slot(left: &Value, right: &Value) -> bool {
    match (entry_index(left), entry_index(right)) {
        (Some(a), Some(b)) => a == b,
        _ => true,
    }
}

/// Serialized length of one JSON value, used for the per-turn byte cap.
fn serialized_len(value: &Value) -> usize {
    serde_json::to_vec(value).map_or(0, |bytes| bytes.len())
}

/// A running capture of `reasoning_details` deltas.
///
/// The streaming accumulator owns one for the whole turn; the non-streaming
/// path folds its single response value through the same type, so both capture
/// paths normalize identically.
#[derive(Debug, Default)]
pub(crate) struct DetailsCapture {
    entries: Vec<Value>,
    /// Running serialized byte estimate across `entries`.
    bytes: usize,
    /// Set once the per-turn cap is exceeded; the whole payload is then dropped
    /// at finish rather than failing the request.
    overflowed: bool,
}

impl DetailsCapture {
    /// Fold one streamed value (an array of entries, or a single entry object
    /// for tolerance) into the capture.
    ///
    /// Returns the newly recognised text/summary fragments in order, so the
    /// caller can forward them as live display events. Unrecognised `type`s are
    /// dropped (and debug-logged) here.
    pub(crate) fn push(&mut self, raw: &Value) -> Vec<String> {
        let items: Vec<&Value> = match raw {
            Value::Array(entries) => entries.iter().collect(),
            Value::Object(_) => vec![raw],
            _ => return Vec::new(),
        };
        let mut fragments = Vec::new();
        for entry in items {
            let Some(kind) = classify(entry) else {
                if entry.get("type").is_some() {
                    debug!("dropping unrecognised reasoning_details entry type");
                }
                continue;
            };
            self.bytes = self.bytes.saturating_add(serialized_len(entry));
            if self.bytes > MAX_REASONING_DETAILS_BYTES {
                if !self.overflowed {
                    warn!(
                        limit = MAX_REASONING_DETAILS_BYTES,
                        "reasoning_details payload exceeds the per-turn cap; dropping it"
                    );
                }
                self.overflowed = true;
                continue;
            }
            if kind.is_textual()
                && let Some(fragment) = entry
                    .get(kind.payload_key())
                    .and_then(Value::as_str)
                    .filter(|fragment| !fragment.is_empty())
            {
                fragments.push(fragment.to_string());
            }
            if kind.is_textual()
                && let Some(last) = self
                    .entries
                    .last_mut()
                    .filter(|last| classify(last) == Some(kind) && share_slot(last, entry))
            {
                merge_textual(last, entry, kind.payload_key());
                continue;
            }
            self.entries.push(entry.clone());
        }
        fragments
    }

    /// Whether the capture holds any recognised entry (an encrypted-only turn
    /// counts as output).
    pub(crate) fn has_entries(&self) -> bool {
        !self.entries.is_empty() && !self.overflowed
    }

    /// The display text derived from the captured text/summary entries.
    pub(crate) fn display(&self) -> String {
        display_text(&self.entries)
    }

    /// Consume the capture into the opaque round-trip artifact. `None` when
    /// nothing was recognised or the payload overflowed the per-turn cap.
    pub(crate) fn finish(self) -> Option<ReasoningArtifact> {
        if self.overflowed || self.entries.is_empty() {
            return None;
        }
        let value = Value::Array(self.entries);
        let bytes = match serde_json::to_vec(&value) {
            Ok(bytes) => bytes,
            Err(error) => {
                warn!(%error, "failed to serialize captured reasoning_details; dropping it");
                return None;
            }
        };
        if bytes.len() > MAX_REASONING_DETAILS_BYTES {
            warn!(
                payload_bytes = bytes.len(),
                limit = MAX_REASONING_DETAILS_BYTES,
                "serialized reasoning_details exceeds the per-turn cap; dropping it"
            );
            return None;
        }
        debug!(
            entry_count = value.as_array().map_or(0, Vec::len),
            payload_bytes = bytes.len(),
            "captured structured reasoning_details"
        );
        Some(ReasoningArtifact::ChatReasoning {
            field: ChatReasoningField::ReasoningDetails,
            bytes,
        })
    }
}

/// Merge `incoming`'s textual payload onto `existing`, carrying over any
/// metadata field the accumulated entry does not yet have (a `signature`
/// often arrives in a later delta than the text it signs).
fn merge_textual(existing: &mut Value, incoming: &Value, key: &str) {
    let Some(existing_obj) = existing.as_object_mut() else {
        return;
    };
    let fragment = incoming
        .get(key)
        .and_then(Value::as_str)
        .unwrap_or_default();
    let merged = existing_obj.get(key).and_then(Value::as_str).map_or_else(
        || fragment.to_string(),
        |prior| format!("{prior}{fragment}"),
    );
    existing_obj.insert(key.to_string(), Value::String(merged));
    let Some(incoming_obj) = incoming.as_object() else {
        return;
    };
    let missing: Vec<(String, Value)> = incoming_obj
        .iter()
        .filter(|(name, _)| name.as_str() != key && !existing_obj.contains_key(name.as_str()))
        .map(|(name, value)| (name.clone(), value.clone()))
        .collect();
    for (name, value) in missing {
        existing_obj.insert(name, value);
    }
}

/// Concatenate the text/summary payloads of a normalized entry slice.
fn display_text(entries: &[Value]) -> String {
    entries
        .iter()
        .filter_map(|entry| {
            let kind = classify(entry)?;
            if kind.is_textual() {
                entry.get(kind.payload_key()).and_then(Value::as_str)
            } else {
                None
            }
        })
        .collect()
}

/// Lower a persisted details artifact into the `reasoning_details` wire array.
///
/// - Namespaced (`reasoning.*`) entries are replayed **verbatim** — index,
///   format, and signature intact.
/// - Bare Kimi-shaped entries are rebuilt minimally: `type` plus the payload
///   field, with the stream-only `index` stripped.
/// - Unrecognised types are dropped.
///
/// An empty result is a deliberate `[]` (a declaring route emits the key even
/// with nothing to echo).
pub(crate) fn details_wire_value(bytes: &[u8]) -> Value {
    if bytes.len() > MAX_REASONING_DETAILS_BYTES {
        warn!(
            payload_bytes = bytes.len(),
            limit = MAX_REASONING_DETAILS_BYTES,
            "persisted reasoning_details artifact exceeds the per-turn cap; replaying an empty array"
        );
        return Value::Array(Vec::new());
    }
    let Ok(value) = serde_json::from_slice::<Value>(bytes) else {
        warn!(
            payload_bytes = bytes.len(),
            "persisted reasoning_details artifact is not valid JSON; replaying an empty array"
        );
        return Value::Array(Vec::new());
    };
    let Some(entries) = value.as_array() else {
        warn!(
            payload_bytes = bytes.len(),
            "persisted reasoning_details artifact is not a JSON array; replaying an empty array"
        );
        return Value::Array(Vec::new());
    };
    Value::Array(entries.iter().filter_map(lower_entry).collect())
}

/// Lower one persisted entry to its wire shape (see [`details_wire_value`]).
fn lower_entry(entry: &Value) -> Option<Value> {
    let ty = entry.get("type").and_then(Value::as_str)?;
    if ty.starts_with("reasoning.") {
        return Some(entry.clone());
    }
    let kind = classify(entry)?;
    let mut object = Map::new();
    object.insert("type".to_string(), Value::String(ty.to_string()));
    if let Some(payload) = entry.get(kind.payload_key()) {
        object.insert(kind.payload_key().to_string(), payload.clone());
    }
    Some(Value::Object(object))
}

/// The Kimi-style scalar companion for a lowered details array: its
/// concatenated summary text, emitted as `reasoning_content`.
///
/// `None` for the `OpenRouter` dialect (whose routes read the array alone), for
/// an empty array, and for an encrypted-only array (nothing displayable to
/// mirror). The one-channel rule holds among the scalar fields; this companion
/// is the deliberate exception Kimi-shaped routes require.
pub(crate) fn details_companion(details: &Value) -> Option<String> {
    let entries = details.as_array()?;
    if entries.is_empty() || entries.iter().any(is_namespaced) {
        return None;
    }
    let text: String = entries
        .iter()
        .filter_map(|entry| {
            if classify(entry) == Some(DetailKind::Summary) {
                entry.get("summary").and_then(Value::as_str)
            } else {
                None
            }
        })
        .collect();
    (!text.is_empty()).then_some(text)
}

/// The display text of a persisted details payload, for replay onto a scalar
/// route (a catalog/observation mismatch: the route declares a scalar field but
/// the artifact is structured). `None` when nothing textual was captured.
pub(crate) fn details_display_text(bytes: &[u8]) -> Option<String> {
    let value = serde_json::from_slice::<Value>(bytes).ok()?;
    let entries = value.as_array()?;
    let text = display_text(entries);
    (!text.is_empty()).then_some(text)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn capture(raw: &Value) -> DetailsCapture {
        let mut capture = DetailsCapture::default();
        capture.push(raw);
        capture
    }

    #[test]
    fn openrouter_text_entries_are_kept_verbatim_and_merged() {
        // Two streamed fragments of the same text block, same index, with the
        // signature arriving on the second delta: merge into one entry that
        // keeps every field (index, format, signature).
        let capture = capture(&json!([
            {"type":"reasoning.text","text":"Let me ","index":0,"format":"openai-responses-v1"},
            {"type":"reasoning.text","text":"think","index":0,"signature":"sig_abc"}
        ]));
        let artifact = capture.finish().expect("artifact");
        let ReasoningArtifact::ChatReasoning { field, bytes } = &artifact else {
            panic!("expected ChatReasoning");
        };
        assert_eq!(*field, ChatReasoningField::ReasoningDetails);
        let entries: Value = serde_json::from_slice(bytes).expect("json");
        assert_eq!(
            entries,
            json!([{
                "type":"reasoning.text",
                "text":"Let me think",
                "index":0,
                "format":"openai-responses-v1",
                "signature":"sig_abc"
            }])
        );
        assert_eq!(capture_display(&artifact), "Let me think");
    }

    #[test]
    fn encrypted_entries_stay_discrete() {
        let capture = capture(&json!([
            {"type":"reasoning.encrypted","data":"AAA","index":0},
            {"type":"reasoning.encrypted","data":"BBB","index":1}
        ]));
        let artifact = capture.finish().expect("artifact");
        let ReasoningArtifact::ChatReasoning { bytes, .. } = &artifact else {
            panic!("expected ChatReasoning");
        };
        let entries: Value = serde_json::from_slice(bytes).expect("json");
        assert_eq!(entries.as_array().map(Vec::len), Some(2));
        assert_eq!(capture_display(&artifact), "");
    }

    #[test]
    fn kimi_bare_entries_merge_and_drop_unknown_types() {
        let capture = capture(&json!([
            {"type":"summary","summary":"step ","index":0},
            {"type":"summary","summary":"one","index":0},
            {"type":"notes","text":"ignored","index":0},
            {"type":"encrypted","data":"ZZZ","index":1}
        ]));
        let artifact = capture.finish().expect("artifact");
        let ReasoningArtifact::ChatReasoning { bytes, .. } = &artifact else {
            panic!("expected ChatReasoning");
        };
        let entries: Value = serde_json::from_slice(bytes).expect("json");
        assert_eq!(
            entries,
            json!([
                {"type":"summary","summary":"step one","index":0},
                {"type":"encrypted","data":"ZZZ","index":1}
            ])
        );
        assert_eq!(capture_display(&artifact), "step one");
    }

    #[test]
    fn unrecognised_only_captures_nothing() {
        let capture = capture(&json!([{"type":"notes","text":"ignored"}]));
        assert!(!capture.has_entries());
        assert!(capture.finish().is_none());
    }

    #[test]
    fn display_derives_from_text_and_summary_entries() {
        let capture = capture(&json!([
            {"type":"reasoning.text","text":"alpha","index":0},
            {"type":"reasoning.summary","summary":" beta","index":0},
            {"type":"reasoning.encrypted","data":"opaque","index":1}
        ]));
        assert_eq!(capture.display(), "alpha beta");
    }

    #[test]
    fn replay_keeps_namespaced_entries_verbatim() {
        let persisted =
            br#"[{"type":"reasoning.text","text":"hi","signature":"s","format":"f","index":3}]"#;
        let wire = details_wire_value(persisted);
        assert_eq!(
            wire,
            json!([{"type":"reasoning.text","text":"hi","signature":"s","format":"f","index":3}])
        );
    }

    #[test]
    fn replay_rebuilds_bare_entries_without_index() {
        // A persisted Kimi payload carries the stream `index`; replay must
        // rebuild it minimally, stripping the index.
        let persisted = br#"[{"type":"summary","summary":"sum","index":0},{"type":"encrypted","data":"enc","index":1}]"#;
        let wire = details_wire_value(persisted);
        assert_eq!(
            wire,
            json!([{"type":"summary","summary":"sum"},{"type":"encrypted","data":"enc"}])
        );
    }

    #[test]
    fn companion_mirrors_only_kimi_summary_text() {
        let kimi = details_wire_value(br#"[{"type":"summary","summary":"a","index":0},{"type":"summary","summary":"b","index":0}]"#);
        assert_eq!(details_companion(&kimi).as_deref(), Some("ab"));
        // OpenRouter dialect: the array is read alone.
        let openrouter =
            details_wire_value(br#"[{"type":"reasoning.summary","summary":"a","index":0}]"#);
        assert_eq!(details_companion(&openrouter), None);
        // Encrypted-only: nothing textual to mirror.
        let encrypted = details_wire_value(br#"[{"type":"encrypted","data":"x","index":0}]"#);
        assert_eq!(details_companion(&encrypted), None);
        // Empty array injects the key without a companion.
        assert_eq!(details_companion(&json!([])), None);
    }

    #[test]
    fn oversized_payload_is_dropped_at_capture() {
        let big = "x".repeat(MAX_REASONING_DETAILS_BYTES + 1);
        let capture = capture(&json!([{"type":"reasoning.text","text":big,"index":0}]));
        assert!(!capture.has_entries());
        assert!(capture.finish().is_none());
    }

    #[test]
    fn oversized_persisted_artifact_replays_empty() {
        let mut bytes = Vec::from(br#"[{"type":"reasoning.text","text":""#);
        bytes.extend(std::iter::repeat_n(b'x', MAX_REASONING_DETAILS_BYTES));
        bytes.extend_from_slice(br#""}]"#);
        assert_eq!(details_wire_value(&bytes), json!([]));
    }

    #[test]
    fn corrupt_artifact_replays_empty() {
        assert_eq!(details_wire_value(b"not json"), json!([]));
        assert_eq!(
            details_wire_value(br#"{"type":"reasoning.text"}"#),
            json!([])
        );
    }

    fn capture_display(artifact: &ReasoningArtifact) -> String {
        let ReasoningArtifact::ChatReasoning { bytes, .. } = artifact else {
            return String::new();
        };
        details_display_text(bytes).unwrap_or_default()
    }
}
