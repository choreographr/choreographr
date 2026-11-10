//! Mechanical repair of the per-request wire copy of a session transcript.
//!
//! `reasoning` owns *policy* (whether and how an opaque artifact is replayed).
//! This module owns *mechanical wire repair*: the stored transcript keeps the
//! exact bytes a provider reply carried, but those bytes are not always
//! something a later request can legally replay. An empty or provider-invalid
//! function name, an empty call id, or tool arguments that are not a JSON
//! object each make a strict endpoint reject the WHOLE request (HTTP 400,
//! `invalid function name`-style errors), so one malformed reply would poison
//! the session for good. Every function here operates on the throwaway
//! message list the daemon builds each turn — the transcript is never touched.
//!
//! ## Purity and determinism
//!
//! These functions are pure transformations: no I/O, no locks, no shared state.
//! [`repair_replayed_calls`] mutates the list it is handed and *also returns
//! what it changed* ([`RepairCounts`]); the counts are its return value, never a
//! side effect, so the module stays free of any observer to notify. The
//! transformation is deterministic as well — the same input always produces the
//! same bytes and the same counts. That property is load-bearing, because the
//! request bytes double as the provider prompt-cache prefix — a cache warmer
//! snapshots `messages` before the tool window and re-sends it, so a repair that
//! varied between two builds of the same request would silently invalidate the
//! cached prefix on every turn.
//!
//! ## What the repair guarantees
//!
//! After [`repair_replayed_calls`], every assistant tool call carries a
//! provider-safe name, a non-blank id unique within the request, and a JSON
//! object for its arguments; every call is answered by exactly one `tool`
//! message (a stub when the transcript holds no result), and a `tool` message
//! that answers no call is dropped. A result whose recorded id no longer names
//! its call still answers it when the choice is unambiguous, so a mangled or
//! pre-mint id costs neither the tool's output nor a stub.

use std::borrow::Cow;
use std::collections::{HashMap, HashSet};

use choreo_ai_protocols::openai::ChatRequestMessage;
use choreo_ai_protocols::tool_name;
use serde_json::Value;

use crate::tools::{MAX_FUNCTION_NAME_LEN, sha256_hex};

/// Function name given to a replayed call whose recorded name cannot be
/// coerced into a provider-accepted one.
///
/// It matches `^[A-Za-z0-9_-]{1,64}$`, so a strict provider accepts it without
/// the tool needing to be registered: the call is a *past* one whose result the
/// transcript already holds, so nothing is dispatched under this name — it only
/// has to label the call for the wire.
pub(crate) const INVALID_TOOL_CALL_NAME: &str = "invalid_tool_call";

/// Content of the synthetic `tool` message that answers a recorded call the
/// transcript holds no result for. Every wire format requires one answer per
/// declared call (chat/Anthropic/Responses 400 on a missing answer, Google on a
/// count mismatch), so a placeholder is the only legal way to replay the turn.
const UNAVAILABLE_RESULT: &str = "[Result unavailable — the tool did not run.]";

/// The repaired form of one tool call.
pub(crate) struct NormalizedCall<'a> {
    /// Provider-safe function name (or [`INVALID_TOOL_CALL_NAME`]).
    pub name: Cow<'a, str>,
    /// The effective call id: the provider's own when it is usable, a minted
    /// one when blank, and never empty.
    pub id: Cow<'a, str>,
    /// A JSON object string — the original bytes when they already parsed as
    /// one, `"{}"` otherwise.
    pub arguments_json: Cow<'a, str>,
}

/// Normalize one tool call for replay.
///
/// `position` is the call's index within its reply; it only feeds the
/// deterministic id minted for a blank id, so two distinct blank-id calls in one
/// reply never collide.
///
/// `rewrite_id` says whether a non-blank id may be normalized. A chained
/// Responses route sets it to `false`: it sends only the post-assistant tail,
/// and the `call_id` in a `function_call_output` must stay byte-identical to
/// the id the service minted, or the output answers nothing. A blank id is
/// still minted there — a provider never mints a blank one.
#[must_use]
pub(crate) fn normalize_call<'a>(
    name: &'a str,
    id: &'a str,
    arguments_json: &'a str,
    position: usize,
    rewrite_id: bool,
) -> NormalizedCall<'a> {
    // Arguments first: a minted id derives from the *repaired* arguments, so a
    // blank-id call whose arguments were dropped to `{}` mints from `{}`.
    let arguments_json = normalize_arguments(arguments_json);
    let name = normalize_name(name);
    let id = normalize_id(id, &name, &arguments_json, position, rewrite_id);
    NormalizedCall {
        name,
        id,
        arguments_json,
    }
}

/// Coerce `arguments_json` into a JSON object string.
///
/// A valid object is returned byte-identical — re-serializing would reorder or
/// respace the bytes and churn the prompt-cache prefix. Anything else (a parse
/// failure, `null`, an array, a scalar, a bare string) becomes `"{}"`, the
/// universal "no arguments" object every provider accepts.
fn normalize_arguments(arguments_json: &str) -> Cow<'_, str> {
    if let Ok(Value::Object(_)) = serde_json::from_str::<Value>(arguments_json) {
        Cow::Borrowed(arguments_json)
    } else {
        // Not an object: a parse failure, `null`, an array, or a scalar. The
        // replacement is counted by the caller (see [`RepairCounts`]) and
        // exported as a metric; there is deliberately no per-call log line,
        // because the same recorded call is re-repaired on every later send and
        // would spam. AGENTS.md forbids logging tool arguments regardless.
        Cow::Borrowed("{}")
    }
}

/// Coerce `name` into a provider-safe function name.
///
/// A valid name is returned unchanged (byte-identical, so the cache prefix is
/// untouched). Otherwise invalid bytes become `_`, runs of `_` collapse, the
/// result is trimmed of `_` and capped at [`MAX_FUNCTION_NAME_LEN`]; if nothing
/// survives, [`INVALID_TOOL_CALL_NAME`] labels the call.
fn normalize_name(name: &str) -> Cow<'_, str> {
    // A call *name* is looked up in the tool catalog, so it is judged by the
    // provider's function-name rule itself.
    if tool_name::is_provider_safe(name) {
        return Cow::Borrowed(name);
    }
    // `sanitize_name` yields an ASCII, charset-clean, underscore-collapsed
    // string of at most `MAX_FUNCTION_NAME_LEN` bytes (or empty).
    let sanitized = sanitize_name(name);
    if sanitized.is_empty() {
        Cow::Borrowed(INVALID_TOOL_CALL_NAME)
    } else {
        Cow::Owned(sanitized)
    }
}

/// Strip every byte outside `[A-Za-z0-9_-]` to `_`, collapse runs of `_`, cap
/// at [`MAX_FUNCTION_NAME_LEN`], and trim leading/trailing `_`.
///
/// The cap is applied before the trim so the result can only ever shrink; all
/// output bytes are ASCII, so the byte cap is also a character cap and the
/// string never exceeds the provider's limit.
fn sanitize_name(name: &str) -> String {
    let mut out = String::with_capacity(name.len().min(MAX_FUNCTION_NAME_LEN));
    let mut prev_underscore = false;
    for byte in name.bytes() {
        let mapped = if byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-' {
            byte
        } else {
            b'_'
        };
        if mapped == b'_' {
            if prev_underscore {
                continue;
            }
            prev_underscore = true;
        } else {
            prev_underscore = false;
        }
        if out.len() >= MAX_FUNCTION_NAME_LEN {
            break;
        }
        // `mapped` is ASCII by construction, so the char cast is lossless.
        out.push(char::from(mapped));
    }
    out.trim_matches('_').to_string()
}

/// Resolve the effective call id.
///
/// A blank id is always minted (see [`mint_call_id`]); a non-blank id is left
/// byte-identical when `rewrite_id` is `false` or when it is already a valid
/// id, and normalized to `[A-Za-z0-9_-]` otherwise. A non-blank id that
/// sanitizes to nothing is minted rather than left empty, because an empty id
/// is exactly what a provider rejects.
fn normalize_id<'a>(
    id: &'a str,
    name: &str,
    arguments_json: &str,
    position: usize,
    rewrite_id: bool,
) -> Cow<'a, str> {
    if id.trim().is_empty() {
        return Cow::Owned(mint_call_id(name, arguments_json, position));
    }
    // A call *id* draws on the same `[A-Za-z0-9_-]` alphabet and the same byte
    // cap as a function name, because it is an opaque token the provider must
    // accept and echo back — never a name looked up in the catalog. That is why
    // the shared function-name rule is the right predicate here: it is what the
    // provider enforces on the id bytes, not a claim that an id is a tool name.
    if !rewrite_id || tool_name::is_provider_safe(id) {
        return Cow::Borrowed(id);
    }
    let sanitized = sanitize_name(id);
    if sanitized.is_empty() {
        Cow::Owned(mint_call_id(name, arguments_json, position))
    } else {
        Cow::Owned(sanitized)
    }
}

/// Mint a deterministic call id for a call that has none:
/// `call_` + the first 12 lowercase hex chars of
/// `sha256(name ␟ arguments_json ␟ position)`.
///
/// Deterministic on purpose: the same recorded call mints the same id on every
/// request, so the cache prefix never shifts and the answer stubbed for it
/// lines up across turns. This is also the id the agent loop mints at ingest
/// for a blank-id call, so a blank id never reaches the transcript in the first
/// place.
#[must_use]
pub(crate) fn mint_call_id(name: &str, arguments_json: &str, position: usize) -> String {
    // `\u{1f}` (unit separator) cannot appear in a tool name or a JSON string
    // of arguments in a way that would collide across the three fields.
    let digest = sha256_hex(&format!("{name}\u{1f}{arguments_json}\u{1f}{position}"));
    // The digest is 64 ASCII hex chars, so taking 12 chars cannot overflow it.
    let short: String = digest.chars().take(12).collect();
    format!("call_{short}")
}

/// Make an id unique within the request, appending `_d2`, `_d3`, … to the
/// second and later occurrence of an id already used earlier in the request.
fn disambiguate(used: &mut HashSet<String>, id: String) -> String {
    if used.insert(id.clone()) {
        return id;
    }
    let mut n = 2u32;
    loop {
        let suffix = format!("_d{n}");
        // Keep the suffixed id inside the provider ceiling by trimming the base,
        // never the suffix (the suffix is what makes it unique). The ceiling is
        // the shared function-name cap because a call id is bounded by exactly
        // the same byte limit and the same alphabet (see [`normalize_id`]).
        let base_len = MAX_FUNCTION_NAME_LEN.saturating_sub(suffix.len());
        // Truncate by BYTES on a char boundary so the suffixed id stays within
        // the byte ceiling even for a non-ASCII id (a chained Responses route
        // keeps such ids byte-identical, so they can reach here).
        let base = truncate_to_bytes(&id, base_len);
        let candidate = format!("{base}{suffix}");
        if used.insert(candidate.clone()) {
            return candidate;
        }
        n = n.saturating_add(1);
    }
}

/// What one [`repair_replayed_calls`] pass changed, per repair kind.
///
/// Pure data: the caller decides what to do with it (log a line, bump a metric),
/// so the repair itself keeps no side effect and stays byte-deterministic. Each
/// counter aggregates every site of its kind in the request — a pass that
/// repaired two names reports `2`, not `1`.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct RepairCounts {
    /// Calls whose emitted name differs from the recorded one (coerced into a
    /// provider-safe name, or replaced by [`INVALID_TOOL_CALL_NAME`]).
    pub names: u64,
    /// Calls whose emitted id differs from the recorded one (minted for a blank
    /// id, normalized, or suffixed to break a duplicate).
    pub ids: u64,
    /// Calls whose emitted argument string differs from the recorded one (the
    /// `{}` replacement — a valid object is never re-serialized).
    pub arguments: u64,
    /// Synthetic results inserted for calls the transcript holds no answer for.
    pub stubbed: u64,
    /// `role: "tool"` messages removed because they answer no call, plus
    /// assistant messages removed because they were left empty.
    pub dropped: u64,
}

impl RepairCounts {
    /// Whether the pass changed nothing at all (every counter is zero).
    pub(crate) fn is_empty(&self) -> bool {
        *self == Self::default()
    }
}

/// Truncate `s` to at most `max` bytes without splitting a UTF-8 char, so a
/// byte-bounded id stays a valid `&str`.
fn truncate_to_bytes(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut end = max;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    // `end` is a char boundary by construction, so `get` yields `Some`; the
    // fallback keeps this panic-free even if that invariant were ever violated.
    s.get(..end).unwrap_or(s)
}

/// One repaired call, tracked so the tool messages can be re-paired to it.
struct CallSlot {
    /// The id the transcript recorded, before repair — the pairing key.
    original_id: String,
    /// The id the wire will carry.
    final_id: String,
    /// Whether a `tool` message was paired to this call.
    answered: bool,
}

/// The still-open call whose recorded id is exactly `original_id`.
///
/// Only a **non-blank** id can select a call by equality: a blank id is a
/// placeholder carrying no information, and two blank ids would compare equal,
/// so equality would pair an arbitrary call rather than the one the result
/// answers.
fn slot_with_id(slots: &[CallSlot], original_id: &str) -> Option<usize> {
    if original_id.trim().is_empty() {
        return None;
    }
    slots
        .iter()
        .position(|slot| !slot.answered && slot.original_id == original_id)
}

/// Choose the still-open call an *unpaired* `tool` message answers, in three
/// tiers, most informative first.
///
/// The tiers exist because a result's recorded id is not always usable: a
/// mangling proxy can rewrite it, and a transcript recorded before ids were
/// minted carries a blank one. Dropping such a result loses the tool's real
/// output and replaces it with a stub, so the fallbacks below recover it where
/// the answer is unambiguous; where it is not, they decline and let pass C drop
/// the message.
#[must_use]
fn next_slot(slots: &[CallSlot], original_id: &str) -> Option<usize> {
    // Tier 1 — the id identifies the call outright. This is the only tier that
    // can be trusted unconditionally, so it always wins.
    if let Some(pos) = slot_with_id(slots, original_id) {
        return Some(pos);
    }
    // Tier 2 — a blank id is a placeholder carrying no information, so the first
    // open call is the best guess available and cannot be contradicted by
    // anything the result itself says.
    if original_id.trim().is_empty() {
        return slots.iter().position(|slot| !slot.answered);
    }
    // Tier 3 — an unmatched non-blank id, with exactly one call still open: the
    // choice is unambiguous, so no other call can be the one this result
    // answers. Pairing here is what keeps the tool's output instead of a stub.
    //
    // Only callers that reach this point after every exact id match has been
    // attempted may use it (see [`pair_results`]): with two or more candidates
    // still open a stray result could take the slot a genuine answer later needs,
    // so this tier declines rather than guess.
    let mut open = slots.iter().enumerate().filter(|(_, slot)| !slot.answered);
    match (open.next(), open.next()) {
        (Some((pos, _)), None) => Some(pos),
        _ => None,
    }
}

/// Pair the still-unpaired `tool` messages to the calls of the assistant
/// message each one follows, picking the call with `choose`.
///
/// The whole observable effect is the plumbing: the chosen slot is marked
/// `answered`, the message's `tool_call_id` is rewritten to that call's *final*
/// id, and `paired[idx]` records that the message now answers a call. `choose`
/// only decides *which* slot — see [`slot_with_id`] for the exact-id sweep and
/// [`next_slot`] for the fallback sweep.
fn pair_results(
    messages: &mut [ChatRequestMessage],
    slots: &mut HashMap<usize, Vec<CallSlot>>,
    paired: &mut [bool],
    choose: fn(&[CallSlot], &str) -> Option<usize>,
) {
    let mut owner: Option<usize> = None;
    for (idx, msg) in messages.iter_mut().enumerate() {
        if msg.role == "tool" {
            let Some(owner_idx) = owner else { continue };
            // A message an earlier sweep already paired keeps its answer, and
            // cannot be paired a second time.
            if paired.get(idx).copied().unwrap_or(false) {
                continue;
            }
            let original = msg.tool_call_id.clone().unwrap_or_default();
            let Some(message_slots) = slots.get_mut(&owner_idx) else {
                continue;
            };
            let chosen =
                choose(message_slots, &original).and_then(|pos| message_slots.get_mut(pos));
            if let Some(slot) = chosen {
                slot.answered = true;
                msg.tool_call_id = Some(slot.final_id.clone());
                if let Some(flag) = paired.get_mut(idx) {
                    *flag = true;
                }
            }
        } else {
            // Only an assistant message that declares calls owns the `tool`
            // messages immediately after it; any other message closes the
            // pairing window.
            owner = (msg.role == "assistant"
                && msg
                    .tool_calls
                    .as_ref()
                    .is_some_and(|calls| !calls.is_empty()))
            .then_some(idx);
        }
    }
}

/// Apply the wire repair to a fully-assembled request message list.
///
/// `rewrite_ids` is passed straight to [`normalize_call`] (see its doc for why
/// a chained Responses route passes `false`).
///
/// Three passes, in order, because each depends on the previous one:
///
/// * **A — normalize calls.** Every assistant call gets a provider-safe name,
///   a JSON-object argument string, and a non-blank id unique within the
///   request (the duplicate suffix is applied here).
/// * **B — pair results.** Each `role: "tool"` message is matched to the call
///   it answers and its `tool_call_id` is rewritten to that call's *final* id.
///   Every result is first given the chance to claim a call by its recorded id;
///   only then do the fallbacks run (a blank id answers the first open call, an
///   unmatched non-blank id the only open call) — see [`next_slot`] for the
///   ordering rationale and the unambiguity each fallback requires. Each call is
///   consumed at most once.
/// * **C — complete the turn.** Every declared call with no result gets a stub
///   answer (in call order, immediately after the assistant message), a `tool`
///   message that answers no call is dropped, and an assistant message left
///   with no text, no calls and no artifact is dropped too.
///
/// Returns what each pass changed (see [`RepairCounts`]). Counting is
/// observation only: it never alters which messages are emitted or their order,
/// so the same input yields the same messages *and* the same counts.
pub(crate) fn repair_replayed_calls(
    messages: &mut Vec<ChatRequestMessage>,
    rewrite_ids: bool,
) -> RepairCounts {
    let mut counts = RepairCounts::default();
    // ── Pass A ──
    let mut used: HashSet<String> = HashSet::new();
    let mut slots: HashMap<usize, Vec<CallSlot>> = HashMap::new();
    for (idx, msg) in messages.iter_mut().enumerate() {
        if msg.role != "assistant" {
            continue;
        }
        let Some(calls) = msg.tool_calls.as_mut() else {
            continue;
        };
        let mut message_slots = Vec::with_capacity(calls.len());
        for (position, call) in calls.iter_mut().enumerate() {
            let original_id = call.id.clone();
            // Consume the borrowed repairs into owned strings before mutating
            // the call, so the borrows of its fields end first.
            let (name, id, arguments_json) = {
                let normalized = normalize_call(
                    &call.function.name,
                    &call.id,
                    &call.function.arguments,
                    position,
                    rewrite_ids,
                );
                (
                    normalized.name.into_owned(),
                    normalized.id.into_owned(),
                    normalized.arguments_json.into_owned(),
                )
            };
            let final_id = disambiguate(&mut used, id);
            // Count the divergence from the recorded bytes before overwriting
            // them: a blank id counts as repaired (minted), and a duplicate id
            // counts because the suffix changed it.
            if name != call.function.name {
                counts.names = counts.names.saturating_add(1);
            }
            if final_id != original_id {
                counts.ids = counts.ids.saturating_add(1);
            }
            if arguments_json != call.function.arguments {
                counts.arguments = counts.arguments.saturating_add(1);
            }
            call.id.clone_from(&final_id);
            call.function.name = name;
            call.function.arguments = arguments_json;
            message_slots.push(CallSlot {
                original_id,
                final_id,
                answered: false,
            });
        }
        slots.insert(idx, message_slots);
    }

    // ── Pass B ──
    // `paired[i]` marks a tool message that was matched to a call; an unpaired
    // tool message is an orphan and is dropped in pass C.
    let mut paired = vec![false; messages.len()];
    // Two sweeps: every exact id match claims its call first, then the fallback
    // tiers pick up the leftovers. Running the exact matches to completion over
    // the whole list first is what stops a fallback from taking a call that a
    // genuine answer later in the same block can still claim by id — the genuine
    // answer would otherwise find the slot gone and be dropped as the orphan.
    // `next_slot` re-checks the id, so the second sweep leaves a message the
    // first one paired untouched (it skips already-paired messages outright).
    pair_results(messages, &mut slots, &mut paired, slot_with_id);
    pair_results(messages, &mut slots, &mut paired, next_slot);

    // ── Pass C ──
    // Walk the (now owned) list once, MOVING each kept message into `out`
    // instead of cloning it — the transcript can carry large image/content
    // payloads, and this runs on every send.
    let original = std::mem::take(messages);
    let mut out: Vec<ChatRequestMessage> = Vec::with_capacity(original.len());
    let mut iter = original.into_iter().enumerate().peekable();
    while let Some((i, msg)) = iter.next() {
        if msg.role == "assistant" {
            let has_calls = msg
                .tool_calls
                .as_ref()
                .is_some_and(|calls| !calls.is_empty());
            let assistant_index = i;
            out.push(msg);
            if !has_calls {
                continue;
            }
            // Consume the assistant's contiguous `tool` block, keeping only the
            // messages that answer one of its calls. An orphan in the block is
            // dropped here.
            while matches!(iter.peek(), Some((_, next)) if next.role == "tool") {
                let Some((j, answer)) = iter.next() else {
                    break;
                };
                if paired.get(j).copied().unwrap_or(false) {
                    out.push(answer);
                } else {
                    // An unpaired result inside the block answers no call.
                    counts.dropped = counts.dropped.saturating_add(1);
                }
            }
            // Every declared call needs exactly one answer: stub the ones that
            // have none, in call order, so the turn is complete on the wire.
            if let Some(message_slots) = slots.get(&assistant_index) {
                for slot in message_slots {
                    if !slot.answered {
                        out.push(tool_stub(&slot.final_id));
                        counts.stubbed = counts.stubbed.saturating_add(1);
                    }
                }
            }
        } else if msg.role == "tool" {
            // A `tool` message not consumed by an assistant block answers no
            // call in the message it follows — an ornament a strict provider
            // rejects. Drop it.
            counts.dropped = counts.dropped.saturating_add(1);
        } else {
            out.push(msg);
        }
    }
    // An assistant message that lost everything (no text, no calls, no
    // reasoning artifact, no image) is itself invalid on the wire.
    let before_retain = out.len();
    out.retain(assistant_message_is_nonempty);
    // Only assistant messages can fail the predicate, so every removed message
    // here is an emptied assistant message; it counts as dropped too.
    counts.dropped = counts
        .dropped
        .saturating_add((before_retain - out.len()) as u64);

    *messages = out;
    counts
}

/// A synthetic `tool` message answering `call_id` with the unavailable-result
/// placeholder.
fn tool_stub(call_id: &str) -> ChatRequestMessage {
    ChatRequestMessage {
        role: "tool",
        content: Some(UNAVAILABLE_RESULT.to_string()),
        images: Vec::new(),
        tool_call_id: Some(call_id.to_string()),
        tool_calls: None,
        reasoning_artifact: None,
    }
}

/// Whether a message may stay on the wire. Non-assistant messages always may;
/// an assistant message must carry something a provider will accept.
fn assistant_message_is_nonempty(msg: &ChatRequestMessage) -> bool {
    msg.role != "assistant"
        || !msg.images.is_empty()
        || msg.reasoning_artifact.is_some()
        || msg
            .tool_calls
            .as_ref()
            .is_some_and(|calls| !calls.is_empty())
        || msg
            .content
            .as_deref()
            .is_some_and(|text| !text.trim().is_empty())
}

#[cfg(test)]
mod tests;
