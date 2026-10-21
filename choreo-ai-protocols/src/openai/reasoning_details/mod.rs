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
//! ## Contracts this module holds
//!
//! - **Capture normalizes, replay preserves.** A streamed block arrives as many
//!   deltas, so consecutive same-kind text/summary entries of one stream slot are
//!   merged into one block (equal `index`, or no `index` on both entries of
//!   *different* deltas); entries carried by one delta are distinct blocks, a
//!   mixed index presence is ambiguous stays unmerged, and encrypted entries
//!   never merge — they are opaque and individually signed. A merge is refused
//!   when the two entries state conflicting identity (a different `id`,
//!   `format`, or `signature`): that is two blocks whose deltas merely share a
//!   slot number.
//! - **Nothing is dropped or rewritten on the way back out.** The upstreams
//!   require the replayed sequence to match what the model produced (see
//!   [preserving
//!   reasoning](https://openrouter.ai/docs/guides/best-practices/reasoning-tokens#preserving-reasoning);
//!   GLM states the same rule for its preserved-thinking blocks,
//!   [docs](https://docs.z.ai/guides/capabilities/thinking-mode)), so an entry
//!   whose `type` this module does not model is kept and replayed verbatim.
//! - **Three answers, three owners.** What the model *said* (display text) is a
//!   fold over the fragments owned by the turn's reasoning state and unbounded
//!   here; what may be *replayed* is [`DetailsStore`], bounded by
//!   [`MAX_REASONING_BYTES`]; and whether the model produced output *at all* is a
//!   fact about the response reported by the fold, never inferred from whether a
//!   payload survived. Collapsing the three is what once let an over-cap payload
//!   blank the transcript and read as an empty response.
//!
//! The end-to-end story (catalog facts, request budget, wire destination) lives
//! in `ARCHITECTURE.md` → *Reasoning round-trip*; this header states only what a
//! reader of these two modules must hold true.

mod capture;
mod lower;

pub(crate) use capture::DetailsStore;
pub(crate) use lower::{details_display_text, details_wire_value};

use serde_json::Value;

/// The reasoning byte budget, in artifact bytes — **one** number for both the
/// per-turn capture cap and the per-request replay budget.
///
/// Two facts size it. Reasoning payloads **compound** — each turn's payload is
/// persisted and replayed on every later request of the tool loop — so one turn
/// must not be allowed to persist an unbounded blob; and an encrypted block is
/// produced alongside the reasoning tokens, so a model with a large output
/// budget can legitimately exceed a small cap in one turn: a cap far below a
/// max-length turn would fire on exactly the heavy-reasoning turns this payload
/// exists for.
///
/// The two bounds are deliberately the same number. A per-turn cap larger than
/// the request budget would have no replay path — such an artifact could only
/// ever ride as that budget's "newest always replays" overshoot — so the request
/// budget is the largest per-turn cap with a meaning, and taking it is what lets
/// a legitimate heavy turn's payload survive at all. What differs between the
/// two is *behaviour*, not size: capture drops the whole array of the one turn
/// that exceeds it (a partial sequence is not something the upstreams accept),
/// while replay drops the *oldest* artifacts and always keeps the newest.
///
/// The turn keeps its display text and its "produced output" fact regardless —
/// both live outside this store, so a payload the cap refuses costs the echo and
/// nothing else.
pub(crate) const MAX_REASONING_BYTES: usize = 256 * 1024;

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
/// they are one stream slot), or neither entry carries an `index` at all AND
/// they arrived in different deltas (the provider does not distinguish slots,
/// so a later delta's same-kind fragment is the block being assembled — leaving
/// them unmerged would produce a word-per-entry sequence some upstreams reject
/// and that can overflow the per-turn cap). A mixed presence is ambiguous and
/// stays unmerged, because fusing on absent evidence could rewrite a sequence
/// the upstream requires to match the original output.
///
/// `same_delta` marks two entries carried by ONE delta's array: those are
/// distinct blocks by the array contract (`index` is documented as optional, so
/// a route may omit it), and fusing them would edit the very sequence the
/// upstream requires to be replayed unmodified. Only the equal-`index` case can
/// merge within a delta. Independently of `same_delta`, neither case qualifies
/// when the two entries disagree about their identity — a different `id`,
/// `format`, or `signature` — since that is two distinct blocks sharing a slot
/// number (see the module docs).
fn is_continuation(existing: &Value, incoming: &Value, same_delta: bool) -> bool {
    let identity_conflict = ["id", "format", "signature"]
        .iter()
        .any(|key| stated_values_differ(existing, incoming, key));
    if identity_conflict {
        return false;
    }
    match (entry_index(existing), entry_index(incoming)) {
        (Some(a), Some(b)) => a == b,
        (None, None) => !same_delta,
        (Some(_), None) | (None, Some(_)) => false,
    }
}

#[cfg(test)]
mod tests;
