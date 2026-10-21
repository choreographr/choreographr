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
//! Three answers, three owners — the separation this module exists to keep:
//!
//! - **What the model said** (display text) is a fold over the fragments, owned
//!   by the turn's reasoning state, unbounded by anything here.
//! - **What may be replayed** (the payload) is this module's [`DetailsStore`],
//!   bounded by [`MAX_REASONING_DETAILS_BYTES`] per turn and by the request
//!   budget across a request. Over the cap the whole array is dropped — a
//!   partial sequence is not something the upstreams accept.
//! - **Whether the model produced output at all** is a fact about the response,
//!   reported by the fold, never inferred from whether a payload survived.
//!
//! Collapsing those three into one flag is what made an over-cap payload blank
//! the transcript and read as an empty response; they stay separate so that a
//! storage decision can only ever cost the echo.
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

/// Per-turn cap on the serialized `reasoning_details` payload this store keeps.
///
/// Two facts size it. Reasoning payloads **compound** — each turn's payload is
/// persisted and replayed on every later request of the tool loop — so one turn
/// must not be allowed to persist an unbounded blob; the aggregate is bounded
/// separately, by the request budget
/// ([`MAX_REQUEST_REASONING_BYTES`](super::wire::MAX_REQUEST_REASONING_BYTES),
/// which drops the *oldest* artifacts and keeps the newest). And an encrypted
/// block is produced alongside the reasoning tokens, so a model with a large
/// output budget can legitimately exceed a small cap in one turn: a cap far
/// below a max-length turn would fire on exactly the heavy-reasoning turns this
/// payload exists for.
///
/// The two constants are therefore equal: an artifact larger than the request
/// budget could only ever ride as that budget's "newest always replays"
/// overshoot, so a larger per-turn cap would have no replay path. Beyond the cap
/// the whole payload is dropped and logged — the whole array, because a partial
/// sequence is not something the upstreams accept (the replayed blocks must match
/// the original output) — while the turn keeps its display text and its "produced
/// output" fact, which live outside this store.
pub(crate) const MAX_REASONING_DETAILS_BYTES: usize = 256 * 1024;

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

/// The turn's **replay payload**: the normalized `reasoning_details` entries the
/// next request may send back.
///
/// Owns one question only — what may be persisted and replayed — and answers it
/// under this module's per-turn byte cap. It deliberately does *not* own the
/// turn's display text or the fact that the model produced output: those are
/// derived from the deltas as they arrive (see
/// [`ReasoningStreamState`](super::chat_completions::reasoning_stream::ReasoningStreamState)),
/// so a payload the cap refuses can never blank what the session shows or turn
/// a real response into an empty one.
#[derive(Debug, Default)]
pub(crate) struct DetailsStore {
    entries: Vec<Value>,
    /// Running serialized byte total across the deltas folded in so far.
    ///
    /// Deliberately an over-estimate, never an under-estimate: every incoming
    /// delta is counted as it arrives *before* any merge, and a merge never
    /// shrinks the total (it only moves a fragment's bytes into an existing
    /// entry). The cap can therefore trip early, never late.
    bytes: usize,
    /// Set once the per-turn cap is exceeded; the whole payload is then dropped
    /// at finish rather than failing the request. Only storage is affected —
    /// the turn keeps its display text and its "produced output" fact.
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

impl DetailsStore {
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
    /// Each newly recognised text/summary fragment is appended to `display_text`
    /// and handed to `on_fragment` in arrival order, so the caller keeps the
    /// turn's display text and can forward the fragment as a live display event.
    /// The two are separate parameters on purpose: `display_text` is the turn's
    /// own record (unbounded, and unaffected by this store's cap) while
    /// `on_fragment` is only the live-render hook, which the non-streaming fold
    /// passes as a no-op.
    ///
    /// Returns whether the model produced **any** structured reasoning entry,
    /// which is a fact about the response, not about this store: an entry the cap
    /// refused, and an encrypted entry with nothing displayable, still count. A
    /// value that is not an entry at all (a bare number, a typeless object, a
    /// string of garbage) does not.
    pub(crate) fn push(
        &mut self,
        raw: &Value,
        display_text: &mut String,
        on_fragment: &mut impl FnMut(&str),
    ) -> bool {
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
                    return false;
                }
            },
            other => other,
        };
        match raw {
            Value::Array(entries) => {
                // Fold every entry: `any` would stop at the first one that
                // reports output, dropping the rest of the array.
                let mut saw_entry = false;
                for entry in entries {
                    saw_entry |= self.push_entry(entry, display_text, on_fragment);
                }
                saw_entry
            }
            // A single entry object is the same tolerance one level down.
            Value::Object(_) => self.push_entry(raw, display_text, on_fragment),
            other => {
                warn!(
                    kind = json_kind(other),
                    "ignoring a reasoning_details value that is not an array or object"
                );
                false
            }
        }
    }

    /// Fold one entry into the capture (see [`push`](Self::push)).
    ///
    /// Split out so both incoming shapes — a streamed array and a lone entry
    /// object — reach the same per-entry rules without first unifying them into a
    /// collection: a delta is on the hot path of every streamed chunk, so the
    /// shape dispatch allocates nothing.
    fn push_entry(
        &mut self,
        entry: &Value,
        display_text: &mut String,
        on_fragment: &mut impl FnMut(&str),
    ) -> bool {
        // A typeless object is not a reasoning entry at all; keeping it would
        // replay a value the provider never described. Log the shape (never
        // the contents) so a malformed stream is still diagnosable.
        if entry.get("type").and_then(Value::as_str).is_none() {
            warn!(
                field_count = entry.as_object().map_or(0, Map::len),
                "dropping a reasoning_details entry with no type tag"
            );
            return false;
        }
        let kind = classify(entry);
        // Display comes first and unconditionally: the fragment is part of what
        // the model said, so the turn's text and the live render must not depend
        // on whether this entry fits the replay cap below. A recognised kind with
        // a non-empty textual payload is displayable; an encrypted or
        // unrecognised entry has nothing to show and nothing to append.
        if let Some(kind) = kind
            && kind.is_textual()
            && let Some(fragment) = entry
                .get(kind.payload_key())
                .and_then(Value::as_str)
                .filter(|fragment| !fragment.is_empty())
        {
            display_text.push_str(fragment);
            on_fragment(fragment);
        }
        // From here on the only question is storage: everything below may refuse
        // the entry without affecting what the turn displays or the fact — which
        // this return value reports — that the model produced output.
        self.bytes = self.bytes.saturating_add(serialized_len(entry));
        if self.bytes > MAX_REASONING_DETAILS_BYTES {
            if !self.overflowed {
                warn!(
                    limit = MAX_REASONING_DETAILS_BYTES,
                    "reasoning_details payload exceeds the per-turn cap; dropping the replay                      payload (the turn keeps its display text and still counts as output)"
                );
            }
            self.overflowed = true;
            return true;
        }
        let Some(kind) = kind else {
            // The catalog does not model this `type`, but the upstream
            // requires the sequence it sent to come back unchanged — so it
            // is stored exactly as it arrived and only display ignores it.
            debug!(
                field_count = entry.as_object().map_or(0, Map::len),
                "keeping an unrecognised reasoning_details entry verbatim"
            );
            self.entries.push(entry.clone());
            return true;
        };
        if kind.is_textual()
            && let Some(last) = self
                .entries
                .last_mut()
                .filter(|last| classify(last) == Some(kind) && is_continuation(last, entry))
        {
            merge_textual(last, entry, kind.payload_key());
            return true;
        }
        self.entries.push(entry.clone());
        true
    }

    /// Consume the store into the opaque round-trip artifact. `None` when
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
