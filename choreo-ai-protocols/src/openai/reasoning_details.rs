//! Structured `reasoning_details` capture and lowering for the chat-completions
//! adapter.
//!
//! Several OpenAI-compatible routes (`OpenRouter` and its resellers, a few
//! gateway brands) carry reasoning as a **structured array** rather than a
//! scalar string. The documented shape is `OpenRouter`'s — three namespaced
//! entry types, each with provider-opaque metadata that must survive the
//! round-trip verbatim
//! ([docs](https://openrouter.ai/docs/guides/best-practices/reasoning-tokens#reasoning_details-array-structure)):
//!
//! - `reasoning.text` (a `text` field, optional `signature`)
//! - `reasoning.summary` (a `summary` field)
//! - `reasoning.encrypted` (a `data` field)
//!
//! plus the common fields `id` (string or null), `format` (an enumerated
//! provider-format tag) and `index` (the sequential index of the entry).
//!
//! A second, **undocumented** shape also arrives in practice: bare entries whose
//! `type` is a single word (`summary`, `encrypted`, and occasionally `text`)
//! with the payload under the same-named field and a stream-only `index`. No
//! upstream documents it — Kimi's own API documentation uses only
//! `reasoning_content`
//! ([docs](https://platform.kimi.ai/docs/guide/use-thinking-models)) and
//! models.dev's snapshot vocabulary is `reasoning_content` or
//! `reasoning_details`, never a bare entry type
//! ([schema](https://github.com/anomalyco/models.dev/blob/dev/packages/core/src/schema.ts)) —
//! so this module treats it as a tolerance case rather than a dialect it claims
//! to know: a bare entry is replayed with only its stream-only `index` removed
//! and every other field preserved exactly as received, including the payload
//! key it arrived under.
//!
//! Two rules shape this module:
//!
//! - **Capture normalizes, replay preserves.** Consecutive same-kind
//!   text/summary entries are merged into one (a streamed block arrives as many
//!   deltas), encrypted entries stay discrete (they are opaque and
//!   individually signed), and an entry whose `type` this module does not model
//!   is kept **verbatim** — the upstream requires the whole sequence to match
//!   what the model produced during the original request, so an unknown block
//!   can never be silently dropped. What is persisted is the normalized JSON
//!   array.
//! - **Display is derived, never invented.** When no scalar reasoning field
//!   streamed, the turn's display text is the concatenation of the text/summary
//!   entries. An encrypted-only (or unrecognised-only) turn still counts as
//!   model output even though it yields no display text.
//!
//! A **streamed block arrives as many deltas, each carrying the next fragment**
//! of that block, so the block is the concatenation of its deltas in arrival
//! order — the same rule the upstream states when it says the complete reasoning
//! sequence is built by concatenating every chunk in order. Consecutive
//! same-kind text/summary entries therefore merge into one block when the
//! provider marks them as the same stream slot (equal `index`) **or** leaves the
//! `index` off entirely (it does not distinguish slots, so adjacency plus kind is
//! the only signal); a mixed presence is ambiguous and stays unmerged. Encrypted
//! entries never merge — they are individually opaque and signed.
//!
//! A merge is further refused when two entries state **conflicting identity** —
//! a different `id`, `format`, or `signature` — because that identifies two
//! different blocks whose deltas merely share a slot number, and the upstream
//! requires the replayed sequence to be exactly the sequence the model produced
//! ([`OpenRouter`](https://openrouter.ai/docs/guides/best-practices/reasoning-tokens#preserving-reasoning)
//! forbids reordering or modifying it; GLM states the same rule for its
//! preserved-thinking blocks,
//! [docs](https://docs.z.ai/guides/capabilities/thinking-mode)). Fusing such a
//! pair would edit the sequence; keeping them discrete preserves it.

use std::io::{self, Write};

use choreo_proto::{ChatReasoningField, ReasoningArtifact};
use serde_json::{Map, Value};
use tracing::{debug, warn};

/// Per-turn cap on the serialized `reasoning_details` payload.
///
/// The cap is deliberately small because the payload **compounds**: it is
/// persisted per turn and replayed on every subsequent request of the tool loop,
/// so a request body grows to roughly `turns × cap` and a session's accumulated
/// history has no cumulative budget of its own. Beyond this limit the details are
/// dropped (and logged) rather than failing the request: losing the reasoning
/// echo degrades a route that requires it, but an oversized body would fail the
/// whole turn.
pub(crate) const MAX_REASONING_DETAILS_BYTES: usize = 64 * 1024;

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
    /// The entry field carrying this kind's payload, for display derivation.
    ///
    /// Only the textual kinds are ever read through this: the encrypted kind
    /// contributes nothing to display, and its payload key is dialect-specific
    /// (`data` for `OpenRouter`'s `reasoning.encrypted`, the same-named field
    /// for an undocumented bare entry), so the lowering never consults it —
    /// replay copies the entry's own fields instead (see [`lower_entry`]).
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
/// `type` yields `None`: such an entry is preserved verbatim rather than
/// classified, and contributes nothing to the derived display text.
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

/// Whether two entries state *different* values for one metadata field.
///
/// Only two *stated* values can disagree: an absent key and an explicit `null`
/// both mean the provider said nothing, so neither conflicts with a stated
/// value.
fn stated_values_differ(existing: &Value, incoming: &Value, key: &str) -> bool {
    fn stated<'a>(value: &'a Value, key: &str) -> Option<&'a str> {
        value
            .get(key)
            .and_then(Value::as_str)
            .filter(|text| !text.is_empty())
    }
    match (stated(existing, key), stated(incoming, key)) {
        (Some(a), Some(b)) => a != b,
        _ => false,
    }
}

/// Whether `incoming` continues the stream slot of `existing`, i.e. whether the
/// two entries may be merged.
///
/// Two cases qualify: both entries name the same `index` (the provider told us
/// they are one stream slot), or neither entry carries an `index` at all (the
/// provider does not distinguish slots, so consecutive same-kind fragments are
/// the single block being assembled — leaving them unmerged would produce a
/// word-per-entry sequence some upstreams reject and that can overflow the
/// per-turn cap). A mixed presence is ambiguous and stays unmerged, because
/// fusing on absent evidence could rewrite a sequence the upstream requires to
/// match the original output.
///
/// Neither case qualifies when the two entries disagree about their identity —
/// a different `id`, `format`, or `signature` — since that is two distinct
/// blocks sharing a slot number, and merging them would edit a sequence the
/// upstream requires to be replayed unmodified (see the module docs).
fn is_continuation(existing: &Value, incoming: &Value) -> bool {
    let identity_conflict = ["id", "format", "signature"]
        .iter()
        .any(|key| stated_values_differ(existing, incoming, key));
    if identity_conflict {
        return false;
    }
    match (entry_index(existing), entry_index(incoming)) {
        (Some(a), Some(b)) => a == b,
        (None, None) => true,
        (Some(_), None) | (None, Some(_)) => false,
    }
}

/// A counting [`io::Write`] sink.
///
/// Measuring an entry needs only its serialized *length*, never a copy of its
/// bytes; a sink that drops the bytes and accumulates the count replaces the
/// temporary `Vec` that serializing to a buffer would allocate and discard on
/// the hot path of every streamed delta.
#[derive(Debug, Default)]
struct ByteCounter(usize);

impl Write for ByteCounter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0 = self.0.saturating_add(buf.len());
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Serialized length of one JSON value, used for the per-turn byte cap. `0` on a
/// serialization error: a measurement never fails a request.
fn serialized_len(value: &Value) -> usize {
    let mut counter = ByteCounter::default();
    match serde_json::to_writer(&mut counter, value) {
        Ok(()) => counter.0,
        Err(_) => 0,
    }
}

/// A running capture of `reasoning_details` deltas.
///
/// The streaming accumulator owns one for the whole turn; the non-streaming
/// path folds its single response value through the same type, so both capture
/// paths normalize identically.
#[derive(Debug, Default)]
pub(crate) struct DetailsCapture {
    entries: Vec<Value>,
    /// Running serialized byte total across the deltas folded in so far.
    ///
    /// Deliberately an over-estimate, never an under-estimate: every incoming
    /// delta is counted as it arrives *before* any merge, and a merge never
    /// shrinks the total (it only moves a fragment's bytes into an existing
    /// entry). The cap can therefore trip early, never late.
    bytes: usize,
    /// Set once the per-turn cap is exceeded; the whole payload is then dropped
    /// at finish rather than failing the request.
    overflowed: bool,
}

/// A payload-free kind tag for a JSON value, so a rejected shape is diagnosable
/// without ever logging its contents.
fn json_kind(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "bool",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

impl DetailsCapture {
    /// Fold one streamed value (an array of entries, or a single entry object
    /// for tolerance) into the capture.
    ///
    /// A JSON **string** value is parsed once and folded when it decodes to an
    /// array or object: some proxies double-encode the array as a JSON string,
    /// and the payload is otherwise lost outright. Anything that is still not an
    /// array or object is ignored with a payload-free log line (a kind tag and,
    /// where cheap, the byte length), because a bare number or a string of
    /// garbage is not a reasoning payload and must not be invented into one.
    ///
    /// Each newly recognised text/summary fragment is handed to `on_fragment` in
    /// arrival order, so a streaming caller can forward it as a live display
    /// event. Display rides a callback rather than a returned list because the
    /// capture itself has nothing to do with display: a caller that wants none
    /// (the non-streaming fold) passes a no-op and allocates nothing, instead of
    /// receiving a fragment list it drops. An entry whose `type` is not modelled
    /// is kept verbatim and yields no fragment; an entry with no `type` at all is
    /// not an entry and is dropped.
    pub(crate) fn push(&mut self, raw: &Value, on_fragment: &mut impl FnMut(&str)) {
        // A double-encoded string is parsed once and then treated exactly like the
        // array/object it decodes to; `decoded` keeps the parsed value alive for
        // the shape dispatch below.
        let decoded;
        let raw = match raw {
            Value::String(text) => match serde_json::from_str::<Value>(text) {
                Ok(value) if value.is_array() || value.is_object() => {
                    debug!(
                        payload_bytes = text.len(),
                        "reasoning_details arrived as a double-encoded JSON string; \
                         folding the decoded value"
                    );
                    decoded = value;
                    &decoded
                }
                _ => {
                    warn!(
                        kind = json_kind(raw),
                        payload_bytes = text.len(),
                        "ignoring a reasoning_details value that is not an array or object"
                    );
                    return;
                }
            },
            other => other,
        };
        match raw {
            Value::Array(entries) => {
                for entry in entries {
                    self.push_entry(entry, on_fragment);
                }
            }
            // A single entry object is the same tolerance one level down.
            Value::Object(_) => self.push_entry(raw, on_fragment),
            other => warn!(
                kind = json_kind(other),
                "ignoring a reasoning_details value that is not an array or object"
            ),
        }
    }

    /// Fold one entry into the capture (see [`push`](Self::push)).
    ///
    /// Split out so both incoming shapes — a streamed array and a lone entry
    /// object — reach the same per-entry rules without first unifying them into a
    /// collection: a delta is on the hot path of every streamed chunk, so the
    /// shape dispatch allocates nothing.
    fn push_entry(&mut self, entry: &Value, on_fragment: &mut impl FnMut(&str)) {
        // A typeless object is not a reasoning entry at all; keeping it would
        // replay a value the provider never described. Log the shape (never
        // the contents) so a malformed stream is still diagnosable.
        if entry.get("type").and_then(Value::as_str).is_none() {
            warn!(
                field_count = entry.as_object().map_or(0, Map::len),
                "dropping a reasoning_details entry with no type tag"
            );
            return;
        }
        self.bytes = self.bytes.saturating_add(serialized_len(entry));
        if self.bytes > MAX_REASONING_DETAILS_BYTES {
            if !self.overflowed {
                warn!(
                    limit = MAX_REASONING_DETAILS_BYTES,
                    "reasoning_details payload exceeds the per-turn cap; dropping it"
                );
            }
            self.overflowed = true;
            return;
        }
        let Some(kind) = classify(entry) else {
            // The catalog does not model this `type`, but the upstream
            // requires the sequence it sent to come back unchanged — so it
            // is stored exactly as it arrived and only display ignores it.
            debug!(
                field_count = entry.as_object().map_or(0, Map::len),
                "keeping an unrecognised reasoning_details entry verbatim"
            );
            self.entries.push(entry.clone());
            return;
        };
        if kind.is_textual()
            && let Some(fragment) = entry
                .get(kind.payload_key())
                .and_then(Value::as_str)
                .filter(|fragment| !fragment.is_empty())
        {
            on_fragment(fragment);
        }
        if kind.is_textual()
            && let Some(last) = self
                .entries
                .last_mut()
                .filter(|last| classify(last) == Some(kind) && is_continuation(last, entry))
        {
            merge_textual(last, entry, kind.payload_key());
            return;
        }
        self.entries.push(entry.clone());
    }

    /// Whether the capture holds at least one entry.
    ///
    /// Any kept entry counts, including an entry whose `type` is not modelled and
    /// an encrypted-only array: such a turn produced model output and must not be
    /// reported as an `EmptyResponse`.
    pub(crate) fn has_entries(&self) -> bool {
        !self.entries.is_empty() && !self.overflowed
    }

    /// The display text derived from the captured text/summary entries.
    ///
    /// Empty when the capture overflowed its per-turn cap: the payload is dropped
    /// wholesale, so the session must not display reasoning it can never replay.
    /// This keeps [`display`](Self::display) in agreement with
    /// [`has_entries`](Self::has_entries) and [`finish`](Self::finish), which both
    /// report nothing for an overflowed turn.
    pub(crate) fn display(&self) -> String {
        if self.overflowed {
            return String::new();
        }
        display_text(&self.entries)
    }

    /// Consume the capture into the opaque round-trip artifact. `None` when
    /// nothing was recognised or the payload overflowed the per-turn cap.
    ///
    /// The serialized array is measured again even though [`push_entry`] already
    /// tracked a running total: that total sums each entry's own length, while
    /// the array it becomes adds the brackets and one comma per separator, so a
    /// capture sitting just under the cap can serialize just over it. The
    /// re-check is what makes the persisted artifact fit the same limit a reader
    /// enforces (`details_wire_value`), not a redundant second look.
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
/// metadata field the accumulated entry does not yet have.
///
/// The metadata carry-over is what makes this safe to restrict to same-index
/// continuations: a `signature` (or `format`/`id`) legitimately arrives in a
/// later delta of the *same* stream slot, and only then does grafting it onto the
/// accumulated entry describe one block rather than fuse two.
fn merge_textual(existing: &mut Value, incoming: &Value, key: &str) {
    let Some(existing_obj) = existing.as_object_mut() else {
        return;
    };
    let fragment = incoming
        .get(key)
        .and_then(Value::as_str)
        .unwrap_or_default();
    // Append in place: a block arrives as many deltas, so rebuilding
    // `format!("{prior}{fragment}")` would copy the whole accumulated block once
    // per fragment (a 30 KiB block in ~500 deltas copies megabytes). `get_mut`'s
    // else arm cannot touch the same map — the borrow the match holds is still
    // live there — so the non-string shapes only record that they need the
    // replace/insert afterwards: an existing non-string payload is replaced by
    // the fragment, and a missing payload is inserted as the fragment.
    let appended = match existing_obj.get_mut(key) {
        Some(Value::String(prior)) => {
            prior.push_str(fragment);
            true
        }
        _ => false,
    };
    if !appended {
        existing_obj.insert(key.to_string(), Value::String(fragment.to_string()));
    }
    let Some(incoming_obj) = incoming.as_object() else {
        return;
    };
    // `incoming` is a separate value from `existing`, so the incoming pairs can be
    // walked while grafting into the existing map — no intermediate collection is
    // needed on the per-delta path. Only fields the accumulated entry does not
    // already have are grafted (a later delta's metadata must not overwrite what
    // an earlier one established), and the payload key is skipped: its text was
    // just merged above.
    for (name, value) in incoming_obj {
        if name != key && !existing_obj.contains_key(name) {
            existing_obj.insert(name.clone(), value.clone());
        }
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
///   format, and signature intact, exactly as the upstream documents the array
///   ([`OpenRouter`](https://openrouter.ai/docs/guides/best-practices/reasoning-tokens#reasoning_details-array-structure)).
/// - A bare (undocumented) entry keeps every field it arrived with and loses
///   only the stream-only `index`; nothing is rebuilt from a guessed payload
///   key, because no upstream documents the shape and a wrong guess would drop
///   an opaque block outright.
/// - Any other entry (an unrecognised `type`, namespaced or bare) is replayed
///   **verbatim**: the upstream requires the sequence to match the original
///   output, so a lowerer may not rewrite or drop a block it does not model.
/// - A typeless entry is not an entry and is dropped.
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
    // Namespaced entries and anything this module does not model both replay
    // verbatim — dropping an unknown block would again modify the sequence.
    if ty.starts_with("reasoning.") || classify(entry).is_none() {
        return Some(entry.clone());
    }
    // A bare entry replays as itself minus the stream-only `index` (the one
    // transformation the shape is documented to need), so the payload survives
    // under whichever key the provider used rather than a key this module
    // guessed.
    let mut lowered = entry.clone();
    if let Some(object) = lowered.as_object_mut() {
        object.remove("index");
    }
    Some(lowered)
}

/// The scalar companion for a lowered details array: its concatenated summary
/// text, emitted as `reasoning_content`.
///
/// `None` for the `OpenRouter` dialect (whose routes read the array alone), for
/// an empty array, and for an encrypted-only array (nothing displayable to
/// mirror). The one-channel rule holds among the scalar fields; this companion
/// is the deliberate exception the bare shape requires, since a route that
/// streams it expects the summary beside the array.
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
mod tests;
