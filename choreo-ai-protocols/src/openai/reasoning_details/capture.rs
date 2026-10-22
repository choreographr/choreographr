//! Capture: folding streamed `reasoning_details` deltas into the turn's replay
//! payload.
//!
//! [`DetailsStore`] owns one question only — what may be persisted and replayed
//! — and answers it under the crate's reasoning byte budget. It deliberately
//! does not own the turn's display text or the fact that the model produced
//! output: those are accumulated from the deltas as they arrive (see
//! [`ReasoningStreamState`](super::super::chat_completions::reasoning_stream::ReasoningStreamState)),
//! so a payload the cap refuses can never blank what the session shows or turn a
//! real response into an empty one.

use std::collections::HashMap;
use std::io::{self, Write};

use choreo_proto::{ChatReasoningField, ReasoningArtifact};
use serde_json::{Map, Value};
use tracing::{debug, warn};

use super::{DetailKind, MAX_REASONING_BYTES, classify, entry_index, is_continuation};

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
    /// The delta generation each stored entry was folded in.
    ///
    /// Two entries carried by ONE delta are distinct blocks; only entries from
    /// different deltas can be the same block arriving in fragments. Tracked as
    /// a generation number rather than a flag so the rule needs no bookkeeping
    /// at the end of a delta.
    entry_delta: Vec<u64>,
    /// Where the fragments of an indexed block resume: the position of the
    /// accumulating entry for each (`kind`, `index`) stream slot.
    ///
    /// Per-slot rather than "the newest entry" so a route that interleaves two
    /// slots across deltas still merges both, instead of starting a fresh entry
    /// for each fragment the moment the other slot intervenes.
    indexed: HashMap<(DetailKind, u64), usize>,
    /// The same for a provider that names no slot: the newest index-less entry
    /// of each textual kind is the block being assembled.
    unindexed: HashMap<DetailKind, usize>,
    /// The current delta generation, bumped by every [`push`](Self::push).
    delta: u64,
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
        // Every call is one delta: entries folded by a previous call may be
        // continued by this one, entries folded below are not continuations of
        // each other.
        self.delta = self.delta.saturating_add(1);
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
        // this return value reports — that the model produced output. Once the
        // cap has refused the payload the array is gone for this turn, so a
        // later delta's bytes are neither needed nor measured.
        if self.overflowed {
            return true;
        }
        self.bytes = self.bytes.saturating_add(serialized_len(entry));
        if self.bytes > MAX_REASONING_BYTES {
            warn!(
                limit = MAX_REASONING_BYTES,
                "reasoning_details payload exceeds the per-turn cap; dropping the replay \
                 payload (the turn keeps its display text and still counts as output)"
            );
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
            self.store(entry.clone(), None);
            return true;
        };
        if kind.is_textual()
            && let Some(position) = self.continuation(kind, entry)
            && let Some(existing) = self.entries.get_mut(position)
        {
            merge_textual(existing, entry, kind.payload_key());
            return true;
        }
        self.store(entry.clone(), Some(kind));
        true
    }

    /// Store one entry, recording the delta generation it arrived in and, for a
    /// textual kind, the stream slot later fragments of the same block resolve
    /// to.
    fn store(&mut self, entry: Value, kind: Option<DetailKind>) {
        let position = self.entries.len();
        let index = entry_index(&entry);
        self.entries.push(entry);
        self.entry_delta.push(self.delta);
        let Some(kind) = kind.filter(|kind| kind.is_textual()) else {
            return;
        };
        match index {
            Some(index) => {
                self.indexed.insert((kind, index), position);
            }
            None => {
                self.unindexed.insert(kind, position);
            }
        }
    }

    /// The position of the stored entry `entry` continues, if any.
    ///
    /// An indexed entry continues the newest stored entry of its kind at the
    /// same stream slot; an index-less entry continues the newest index-less
    /// entry of its kind, because a provider that names no slot has one block
    /// being assembled. Meeting the slot is necessary but not sufficient:
    /// [`is_continuation`] still refuses a merge when the two entries disagree
    /// about their identity or when both arrived in one delta, so two distinct
    /// blocks that merely share a slot number stay distinct.
    fn continuation(&self, kind: DetailKind, entry: &Value) -> Option<usize> {
        let position = match entry_index(entry) {
            Some(index) => *self.indexed.get(&(kind, index))?,
            None => *self.unindexed.get(&kind)?,
        };
        let existing = self.entries.get(position)?;
        let same_delta = self.entry_delta.get(position) == Some(&self.delta);
        (is_continuation(existing, entry, same_delta) && classify(existing) == Some(kind))
            .then_some(position)
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
        if bytes.len() > MAX_REASONING_BYTES {
            warn!(
                payload_bytes = bytes.len(),
                limit = MAX_REASONING_BYTES,
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
/// metadata field the accumulated entry does not yet state.
///
/// The metadata carry-over is what makes this safe to restrict to same-index
/// continuations: a `signature` (or `format`/`id`) legitimately arrives in a
/// later delta of the *same* stream slot, and only then does grafting it onto the
/// accumulated entry describe one block rather than fuse two.
///
/// "Does not yet state" uses the same reading [`is_continuation`] does for an
/// identity conflict: an absent key, an explicit `null`, and an empty string all
/// mean the provider has not given the field a value yet. This matters because
/// `OpenRouter`'s documented stream opens a block with `"signature": null` and
/// delivers the real signature on a later fragment of the same slot; treating
/// that `null` as an established value would shadow the signature and replay an
/// unsigned block.
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
    // needed on the per-delta path. A field is grafted only when the accumulated
    // entry does not yet STATE it (absent, `null`, or empty — the same rule
    // `is_continuation` uses to decide an identity conflict), so a later delta's
    // real `signature`/`id`/`format` lands on the block instead of being shadowed
    // by an earlier `null`; a value already stated is never overwritten. The
    // payload key is skipped: its text was just merged above.
    for (name, value) in incoming_obj {
        if name == key {
            continue;
        }
        let unstated = existing_obj
            .get(name)
            .is_none_or(|prior| prior.is_null() || prior.as_str().is_some_and(str::is_empty));
        if unstated {
            existing_obj.insert(name.clone(), value.clone());
        }
    }
}
