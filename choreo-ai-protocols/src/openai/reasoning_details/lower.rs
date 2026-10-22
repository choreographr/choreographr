//! Lowering: projecting a persisted `reasoning_details` payload back onto the
//! wire.
//!
//! The lowering is the mirror of [`super::capture`], and it is deliberately
//! conservative: the upstreams require the replayed sequence of reasoning blocks
//! to match what the model produced, so an entry this module does not model is
//! replayed verbatim rather than rebuilt. Only a bare (undocumented) entry — the
//! one shape no upstream specifies — loses its stream-only `index`.

use serde_json::Value;
use tracing::warn;

use super::{MAX_REASONING_BYTES, classify};

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
    if bytes.len() > MAX_REASONING_BYTES {
        warn!(
            payload_bytes = bytes.len(),
            limit = MAX_REASONING_BYTES,
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

/// The display text of a persisted details payload, for replay onto a scalar
/// route (a catalog/observation mismatch: the route declares a scalar field but
/// the artifact is structured). `None` when nothing textual was captured.
///
/// The same byte budget the replay path enforces applies here: an over-budget
/// payload is not parsed at all, so a corrupt or hostile persisted blob cannot
/// cost a large allocation on every request of the loop.
pub(crate) fn details_display_text(bytes: &[u8]) -> Option<String> {
    if bytes.len() > MAX_REASONING_BYTES {
        warn!(
            payload_bytes = bytes.len(),
            limit = MAX_REASONING_BYTES,
            "persisted reasoning_details artifact exceeds the per-turn cap; \
             deriving no display text"
        );
        return None;
    }
    let value = serde_json::from_slice::<Value>(bytes).ok()?;
    let entries = value.as_array()?;
    let text = display_text(entries);
    (!text.is_empty()).then_some(text)
}
