# Plan: a per-turn payload budget for inbound provider responses

**Status:** proposed — design complete; nothing implemented (no source changes in this
change).
**Lifecycle:** this file is **deleted once the plan is fully implemented**. Nothing
written during implementation may reference it — rustdoc, `ARCHITECTURE.md`,
`README.md`, release notes, and commit messages must stand on their own, because a
reference to this plan would go stale the moment it is removed.
**Date:** 2026-10-30
**Targets:** `choreo-ai-protocols` (all four adapters' streaming accumulators, the
shared budget type, and the inference body read); `choreo-proto` (one
`InferenceError` variant plus its metric label and the two exhaustive label tests);
`ARCHITECTURE.md` (the provider bounds paragraph and the metrics label list).
**Touches:** `choreo-ai-protocols/src/shared.rs` (budget type + constant),
`src/openai/responses/{accumulator,stream}.rs`,
`src/openai/chat_completions/accumulator.rs`, `src/anthropic/requests.rs`,
`src/google/requests.rs` (text/reasoning only),
`src/retry/mod.rs` + `src/shared.rs` (body cap, Phase 5 — not yet traced to an exact
call site), `choreo-proto/src/types/common.rs`, `choreo-proto/src/tests.rs`, new unit
tests in each adapter's `tests/` module and one integration test,
`ARCHITECTURE.md`.

**Line references are as of commit `360de1b`.** They are pointers for the
implementer, not contracts; re-derive them before editing.

> **TL;DR.** Every provider response this client reads is assembled into memory under
> a **per-event** ceiling (`MAX_SSE_EVENT_BYTES`, 8 MiB) and a **per-call count**
> ceiling (`MAX_TOOL_CALLS`, 128) — but there is **no per-*turn* byte ceiling** on the
> channels those events accumulate into: assistant text, reasoning display text, and
> tool-call arguments each grow without bound for the life of one request. A hostile,
> buggy or merely runaway peer can therefore make one turn retain arbitrarily much
> memory (and, for arguments, persist it with the session turn and replay it on every
> later request). Add one small shared budget value type, charge each accumulation
> channel against a per-turn limit as bytes arrive, and fail the turn with a
> dedicated terminal `TurnPayloadTooLarge` error when a channel passes it. The same
> change closes the neighbouring gap that non-streaming inference bodies are read
> with no size cap at all. Limits are 2 MiB per channel per turn — roughly four times
> the largest plausible response at a 128k-token output ceiling — so no legitimate
> turn is affected.

---

## Table of contents

1. [Problem](#1-problem)
2. [What is bounded today](#2-what-is-bounded-today)
3. [Inventory of accumulation sites](#3-inventory-of-accumulation-sites)
4. [Design: the budget type](#4-design-the-budget-type)
5. [Limits and why these numbers](#5-limits-and-why-these-numbers)
6. [Failure semantics](#6-failure-semantics)
7. [Wiring, phase by phase](#7-wiring-phase-by-phase)
8. [Alternatives considered](#8-alternatives-considered)
9. [Testing strategy](#9-testing-strategy)
10. [Observability](#10-observability)
11. [Documentation](#11-documentation)
12. [Risks & mitigations](#12-risks--mitigations)
13. [Out of scope](#13-out-of-scope)
14. [Decisions needed](#14-decisions-needed)
15. [Definition of done](#15-definition-of-done)

---

## 1. Problem

A streaming inference turn is built by appending every delta the provider sends. The
only ceiling on that growth is `MAX_SSE_EVENT_BYTES` (8 MiB), and it is **per event**:
a peer that sends a hundred million small deltas passes every check and the turn's
buffers grow without bound. Three channels are affected in every adapter:

1. **Assistant text** (`full_content` / `full_text`) — displayed live and persisted
   with the turn.
2. **Reasoning display text** (`full_reasoning`) — displayed live and persisted. The
   *replay artifact* is separately capped at `MAX_REASONING_BYTES` (256 KiB), which
   does not bound this channel.
3. **Tool-call arguments** — the accumulated JSON string per call, capped **only by
   call count** (`MAX_TOOL_CALLS` = 128), never by bytes. This is the channel that
   matters most: it is persisted with the turn (assistant tool-call record) and
   **replayed on every later request** for the rest of the session, so one runaway
   turn inflates every subsequent prompt and every client session snapshot.

Credible triggers, in decreasing likelihood:

- **A model in a runaway tool-call loop** that emits one enormous
  `write_file`-style argument and repeats it. Not adversarial; models do overshoot.
- **A buffering proxy or gateway** that replays or duplicates a response it already
  buffered, inflating one turn several times over.
- **A misbehaving or compromised gateway** — untrusted input this client already
  treats as hostile at the SSE framing layer, so it should be treated as hostile here
  too.
- **A pathological provider** that marks a response `incomplete` but keeps streaming
  (the read loop continues until the terminal event or the deadline).

The deadline (`retry::AttemptDeadline`) bounds *time*, not bytes: a fast peer can
deliver gigabytes inside the configured window.

**Why this is worth fixing even though it predates the reviewed change.** Reviewing
`31b97a0` (the streaming Responses tool-call accumulator) surfaced the gap directly:
the accumulator's own doc comment says the call count is bounded, and says nothing
about the argument bytes it concatenates. The fix is small, shared, and mechanical —
exactly the kind of bound that is much cheaper to add once, centrally, than to
retrofit into four adapters after an incident.

---

## 2. What is bounded today

| Bound | Value | Scope | Where |
|---|---|---|---|
| `MAX_SSE_EVENT_BYTES` | 8 MiB | **one SSE event** (accumulated `data:` lines), all streaming adapters | `choreo-ai-protocols/src/openai/sse/reader.rs:19` |
| `MAX_TOOL_CALLS` | 128 | **call count** per turn (and per-index bounds in chat/anthropic) | `choreo-ai-protocols/src/shared.rs:14` |
| `MAX_REASONING_BYTES` | 256 KiB | the **replay artifact** only, not the display text | `choreo-ai-protocols/src/openai/reasoning_details/mod.rs:102` |
| `MAX_BODY_BYTES` | 32 MiB | the models.dev **catalog fetch** body only — a local, per-call `.take(…)` | `choreo-ai-protocols/src/catalog/refresh.rs:35` |
| `MAX_FRAME_SIZE` | 64 MiB | one wire frame (so a session snapshot is rejected, not truncated, above it) | `choreo-proto/src/frame.rs` |
| `AttemptDeadline` | configured | wall clock for one HTTP attempt | `choreo-ai-protocols/src/retry/mod.rs` |

So: per event yes, per call count yes, per turn **no** — and non-streaming inference
bodies have no size cap at all (only `catalog/refresh.rs` takes its own).

---

## 3. Inventory of accumulation sites

Every `String` that grows from provider deltas, at `360de1b`:

| Adapter | Channel | Site | Bound today |
|---|---|---|---|
| OpenAI Responses (streaming) | arguments (append) | `responses/accumulator.rs`, `absorb` → `FunctionCallArgumentsDelta` arm | count only |
| OpenAI Responses (streaming) | arguments (replace) | same, `FunctionCallArgumentsDone` + `FunctionCallItem` (item's own copy) | count only |
| OpenAI Responses (streaming) | text / reasoning | `responses/stream.rs:111`, `:119` | none |
| OpenAI Chat Completions (streaming) | arguments | `chat_completions/accumulator.rs:168` (raw deltas collected), then merged at `:58` | count only |
| OpenAI Chat Completions (streaming) | text | `chat_completions/accumulator.rs:141` | none |
| OpenAI Chat Completions (streaming) | reasoning | `self.reasoning.apply(…)` — capture capped at `MAX_REASONING_BYTES`, **display text not** | partial |
| Anthropic (streaming) | arguments | `anthropic/requests.rs` `messages_request_streaming` (fn begins `:195`), `InputJsonDelta` arm → `call.arguments.push_str` (`:395`–`:409`) | count only |
| Anthropic (streaming) | text / reasoning | same fn, `full_text` / `full_reasoning` (`:337`, `:374`, `:391`, `:420`) | none |
| Google (streaming) | text / reasoning | `google/requests.rs:379`, `:389` | none |
| Google (streaming) | tool calls | **none** — `functionCall` parts arrive complete (`google/mod.rs:557`), no delta concatenation | n/a (whole-part size, see Phase 5) |
| Any (non-streaming) | whole response body | `retry::retry_send` → `response.into_body().read_json()` | **none** |

Two notes for the implementer:

- **The Responses argument channel replaces as well as appends.** A charge that only
  counts appended deltas would double-count a stream whose `arguments.done` (or whose
  `output_item.done` item copy) restates the whole string. Charge **net growth**:
  `new_len.saturating_sub(old_len)`.
- **Chat Completions holds the argument bytes twice** — `raw_tool_call_deltas`
  (a clone per delta, `:168`) plus the merged strings from
  `accumulate_tool_calls_from_deltas` (`:58`). Charging at the **ingest** site
  (`:168`) bounds the raw vector, which is the larger copy; the merged copy is then
  bounded by the same charged total. Merging incrementally into
  `AccumulatingToolCall` instead of collecting raw deltas would remove the double
  storage — worth doing, but it is a separate concern (see §13).

---

## 4. Design: the budget type

One small, allocation-free value type in `choreo-ai-protocols/src/shared.rs`, beside
the existing `MAX_TOOL_CALLS`:

```rust
/// Ceiling on the bytes one channel of one turn may accumulate.
///
/// The SSE reader bounds a single event, and `MAX_TOOL_CALLS` bounds how many calls
/// a turn may open, but neither bounds the *sum* of what those events accumulate
/// into: a peer that streams many small deltas grows a turn's buffers without limit.
/// This is that missing bound, applied per channel (text, reasoning text, tool-call
/// arguments) so one runaway channel cannot starve the others.
pub(crate) const MAX_TURN_CHANNEL_BYTES: usize = 2 * 1024 * 1024;

/// Which accumulated channel a budget governs — used in the error message and the
/// warning field, never as a metrics label (labels stay low-cardinality).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PayloadChannel {
    Text,
    Reasoning,
    ToolArguments,
}

impl PayloadChannel {
    pub(crate) const fn as_str(self) -> &'static str { /* "text" | "reasoning" | "tool_arguments" */ }
}

/// Running byte total for one channel of one turn.
///
/// Charge on **growth**, not on every write: a channel that restates its whole
/// value (a `function_call_arguments.done`, an item's own arguments copy) must be
/// charged only for the net increase, or a legal replacement that shrinks the string
/// would be billed twice.
#[derive(Debug)]
pub(crate) struct PayloadBudget {
    channel: PayloadChannel,
    used: usize,
}

/// The channel that passed its ceiling.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct PayloadOverflow {
    pub(crate) channel: PayloadChannel,
    pub(crate) used: usize,
    pub(crate) limit: usize,
}

impl PayloadBudget {
    pub(crate) const fn new(channel: PayloadChannel) -> Self;
    /// Charge `growth` bytes; `Err` once the ceiling is passed.
    ///
    /// The limit is inclusive (`used > limit` trips, `used == limit` passes), so the
    /// name "2 MiB" means a 2 MiB channel is legal.
    pub(crate) fn charge(&mut self, growth: usize) -> Result<(), PayloadOverflow>;
    pub(crate) const fn used(&self) -> usize;
}
```

Each adapter's error type maps `PayloadOverflow` into its own variant (see §6), so the
shared type carries no error-type dependency and each crate keeps its own taxonomy.

**Charging rules, stated once so they cannot drift per adapter:**

| Event | Charge |
|---|---|
| text / reasoning delta | `delta.len()` before `push_str` |
| arguments delta | `delta.len()` before `push_str` |
| `arguments.done` | `new.len().saturating_sub(old.len())` before `clone_from` |
| item's own `arguments` copy (`output_item.done`) | `new.len().saturating_sub(old.len())`, skipped entirely when `new` is empty |
| chat raw-delta ingest | `function.arguments.len()` as the delta is collected |

---

## 5. Limits and why these numbers

`MAX_TURN_CHANNEL_BYTES = 2 MiB` per channel, per turn. Worst-case retained payload
for one turn is therefore bounded at **6 MiB** (text + reasoning + arguments), against
*six* figures of unbounded growth today.

Why 2 MiB is generous enough to be invisible:

- A response's total output is bounded provider-side by the request's
  `max_output_tokens`. The most generous catalogue ceiling is a few hundred thousand
  tokens; at ~4 bytes per token that is well under 1 MiB of UTF-8 for *text plus
  reasoning plus every call's arguments combined*.
- 2 MiB of assistant text is roughly 500k tokens — a single response cannot approach
  it.
- The existing artifact cap (`MAX_REASONING_BYTES`, 256 KiB) sets the precedent for
  this order of magnitude; 2 MiB is 8× looser, because the replay artifact can afford
  to be dropped while a turn's live arguments cannot.

Why not tie it to `MAX_SSE_EVENT_BYTES` (8 MiB): one legitimate event can *be* 8 MiB
(a complete tool-argument string, an encrypted reasoning item), so a per-turn ceiling
at or above it bounds nothing useful.

If field data later shows a legitimate oversized turn, the constant is the knob
(§14, third question).

---

## 6. Failure semantics

**Terminal, one new variant, rendered as a turn error.** Nothing about the turn can
safely be used once a channel has overflowed: the accumulated text is truncated mid
sentence and the accumulated arguments are invalid JSON.

`choreo-ai-protocols/src/shared.rs`:

```rust
/// The provider streamed more payload than one turn may accumulate.
///
/// Terminal: the affected channel is truncated, so what arrived cannot be shown or
/// run, and the peer that sent it will do it again on a retry of the same request.
#[error("{channel} for this turn exceeded {limit} bytes (saw at least {used})")]
TurnPayloadTooLarge {
    channel: &'static str,
    used: usize,
    limit: usize,
},
```

`choreo-proto/src/types/common.rs`: the mirrored `InferenceError::TurnPayloadTooLarge`
with `channel: String`, plus `metric_label` → `"turn_payload_too_large"`.
`provider_error_to_inference` maps it 1:1 like every other variant.

Consequences, deliberately:

- The daemon's existing generic error arm (`choreo-daemon/src/requests.rs`, the
  `other =>` arm after the `TruncatedToolCall` special case) marks the turn failed and
  finalizes it, so the client renders a red error block and the failure survives a
  restart. No daemon change is needed.
- **It must not be conflated with `TruncatedToolCall`.** That variant drives the
  daemon's bounded smaller-steps recovery loop (`MAX_TRUNCATION_RECOVERIES`), which
  tells the model "you were cut off at the output-token limit, retry in smaller steps"
  and clears the response-id chain. A payload overflow has a different cause and a
  different remedy, so reusing that machinery would misinform the model. See §8.
- **The metric label must be added to both `choreo-proto` label tests.** They are
  exhaustive as of `8421414` and hardcode a variant count, so the gate fails until the
  new variant is listed in both — by design.

---

## 7. Wiring, phase by phase

Each phase is a self-contained commit (own tests, own gate run). Phases 1–4 are the
plan; Phase 5 closes the adjacent body-cap gap; Phase 6 is optional cleanup.

### Phase 1 — the shared type and the error plumbing

`choreo-ai-protocols/src/shared.rs`: `MAX_TURN_CHANNEL_BYTES`, `PayloadChannel`,
`PayloadBudget`, `PayloadOverflow`, `ProviderError::TurnPayloadTooLarge`, the
`From<PayloadOverflow>`-shaped mapping helper used by the adapters, and the
`provider_error_to_inference` arm. `choreo-proto/src/types/common.rs`: the
`InferenceError` variant + label. `choreo-proto/src/tests.rs`: both label tests.
Unit tests for `PayloadBudget` (`charge` accumulation, inclusive limit, zero growth,
overflow fields, `used()`).

No behaviour change yet; nothing charges the budget. This phase exists so the
remaining phases are pure call-site edits.

### Phase 2 — OpenAI Responses, streaming

`responses/accumulator.rs`: `AccCalls` gains one `PayloadBudget` for the turn's
argument bytes, charged at the four argument points in `absorb` (§4 table). Note the
item's-own-copy rule: an empty copy is already skipped there, and must not be charged.
`responses/stream.rs`: two budgets, charged at `:111` (text) and `:119` (reasoning).

Tests (unit, in `responses/tests/accumulator.rs` — the existing module):
arguments overflow across two calls; a shrinking `arguments.done` replacement does not
trip; a turn exactly at the limit passes; the item-copy replacement is charged only
for net growth. Plus one integration test in `tests/it/responses_stream.rs`
(`#[ignore = "integration"]`, mock provider) asserting a stream that crosses the
ceiling yields `TurnPayloadTooLarge` and the stream loop does not keep reading.

### Phase 3 — OpenAI Chat Completions, streaming

`chat_completions/accumulator.rs`: `ChatCompletionsStreamAccumulator` gains a
`PayloadBudget` for arguments charged at ingest (`:168`), and one for text (`:141`).
The reasoning channel is bounded by the existing capture cap for the artifact; charge
the **display** text too, which is not currently capped.

Tests in `chat_completions/tests/`: overflow at ingest (proving the raw-delta vector
is what is bounded), at-limit passes, and the merge path reporting the mapped error
from `into_turn_result`.

### Phase 4 — Anthropic, streaming

`anthropic/requests.rs`, `messages_request_streaming`: budgets for `pending_tool_calls[i].arguments`,
`full_text`, `full_reasoning`.

Tests in `anthropic/tests.rs` — note that adapter's existing tests are wire-level, so
add the budget cases at the same level the current streaming tests use.

### Phase 5 — cap the non-streaming inference body

Streaming is bounded by the per-turn budgets; non-streaming reads a whole body into
`serde_json::Value` with no cap, and the Google adapter's complete `functionCall`
parts only arrive this way. Add `MAX_RESPONSE_BODY_BYTES = 64 * 1024 * 1024` (mirroring
`MAX_FRAME_SIZE`, so a body this client accepts is one it can also put on the wire) and
apply it where the inference response body is consumed, following the pattern
`catalog/refresh.rs` already uses: read through `.take(limit + 1)` and fail only when
`len > limit`, so a body of exactly the limit is accepted and distinguishable from
one truncated at it (`refresh.rs:173`–`:189`, whose doc comment states that
reasoning). The same inclusive-limit rule as `PayloadBudget::charge`. Trace the exact
site first: `retry::retry_send` hands back a `ureq::Response`, and both `read_json()`
and `into_reader()` are used downstream, so the cap likely belongs on the body handle
`retry_send` returns rather than on each caller — one place, not six.

Test: a mock provider returning a body over the ceiling fails with the typed error; a
body at the ceiling passes.

### Phase 6 (optional) — remove the chat adapter's double storage

Merge chat tool-call deltas incrementally into `AccumulatingToolCall` as they arrive
instead of collecting `raw_tool_call_deltas` and merging at the end. Halves that
adapter's peak argument memory and lets the budget guard a single buffer. Independent
of the bound (the budget is correct either way), which is why it is last.

---

## 8. Alternatives considered

1. **Reuse `TruncatedToolCall` so the daemon's smaller-steps recovery fires.**
   Tempting: that machinery already exists and a model that emits a 3 MiB file write
   *should* be told to split it. Rejected as the default because the recovery
   instruction ("cut off at the model's output-token limit… retry in smaller steps")
   states a cause that is false here, and it clears the response-id chain — a side
   effect justified for a truncated response, not for a size refusal. Kept as an open
   decision (§14) for the arguments channel specifically.
2. **Drop the oversized call and keep the rest of the turn.** Rejected: silently
   discarding model output is exactly the failure mode the reviewed change fixed.
3. **Bound only tool-call arguments.** Rejected as incomplete: assistant text and
   reasoning display text grow from the same events with the same absence of a
   ceiling, so leaving them unbounded fixes a third of the problem.
4. **Lower `MAX_SSE_EVENT_BYTES`.** Rejected: it is per event, and the largest
   legitimate event (a complete argument string, an encrypted reasoning item) legitimately
   approaches it.
5. **Truncate the channel and mark the turn truncated instead of failing.** Rejected:
   a truncated-but-successful turn persists broken JSON arguments and half a sentence,
   which then replays; failing is honest and the error is visible in the transcript.
6. **Make the limits operator-configurable from the start.** Rejected for Phase 1: a
   bound nobody has needed to change does not need a config surface, and the constant
   is one line to promote later (§14).

---

## 9. Testing strategy

House rules apply: unit tests in `src/` `#[cfg(test)]` modules and **never**
time-based; anything exercising a socket or process goes in the crate's single
`tests/it/` binary marked `#[ignore = "integration"]`.

Required coverage:

- **The type** (Phase 1): accumulation, inclusive limit (`used == limit` passes,
  `used > limit` fails), zero growth, overflow payload fields, `PayloadChannel::as_str`.
- **Per adapter, per channel**: a stream that crosses the ceiling fails with that
  adapter's mapped error; a legal multi-call turn comfortably under the ceiling still
  succeeds; a replacement that shrinks a channel is charged only net growth; a call
  exactly at the ceiling passes. These are the cases that catch a wrong charging rule,
  which is the likeliest implementation defect.
- **One integration test per streaming adapter** (Phase 2 mandatory, 3–4 as each phase
  lands): a mock provider stream that exceeds a ceiling, asserting the typed error
  rather than an empty answer.
- **`choreo-proto`**: both metric-label tests gain the variant (they fail until they
  do, which is the point).
- **Daemon**: none needed — the generic error arm already renders any
  `InferenceError` as a failed turn. Confirm by inspection, not by a new test.

---

## 10. Observability

- `warn!` at the overflow point with `channel`, `used`, `limit` — counts and a tag
  only; never any accumulated bytes.
- Metric label `turn_payload_too_large` (constant, low cardinality). `channel` stays
  out of the label set: three more label values buy little and cost dashboard churn.
- The transcript already shows the error text, which names the channel and the limit.

---

## 11. Documentation

`ARCHITECTURE.md`: extend the provider-bounds discussion (the paragraph that already
explains `MAX_TOOL_CALLS` and the SSE event ceiling) with the per-turn channel budget,
its arithmetic against `max_output_tokens`, and the non-streaming body cap from
Phase 5. Add the new metric label to the metrics list where the other
`InferenceError` labels are described, if such a list exists. In-source: the constant
and type carry the rationale (§4's doc comments), so the markdown points at the code
rather than restating it.

---

## 12. Risks & mitigations

| Risk | Mitigation |
|---|---|
| A legitimate oversized turn now fails where it used to work (a model genuinely writing a multi-megabyte file) | 2 MiB is ~4× the largest plausible response at a generous output ceiling; if field data contradicts it, raise the constant (one line) or route the arguments channel to smaller-steps recovery (§14) |
| Charging applied per *write* instead of per *growth* double-bills a stream that restates its arguments, failing legal turns | Stated once in §4 and pinned by a shrinking-replacement test in every adapter |
| A channel is missed (e.g. the Chat adapter's `raw_tool_call_deltas` charged only after the merge) | The per-adapter inventory in §3 is the checklist; each phase's test crosses the ceiling through the real ingest path |
| The new error variant is mistaken for retryable | It is terminal by construction (no retry arm), and the label tests force it into the enumeration |
| Phase 5's body cap is applied per caller instead of once on the body handle | §7 says to trace the site first; one cap, not six |

---

## 13. Out of scope

- **Per-event and per-call-count bounds** — already present.
- **Provider-supplied identifier lengths** (`item_id`, `call_id`): cloned per event,
  bounded only by the per-event cap. Real, but small and harmless under the
  trusted-provider model; genuinely hostile peers are already bounded by it.
- **Outbound request-size limits** — a separate concern (compression, upload paths).
- **Persisted turn size limits** — the 64 MiB frame cap already rejects an absurd
  session snapshot rather than truncating it. This plan prevents the payload getting
  there in the first place.
- **The chat adapter's double storage** beyond Phase 6.
- **Config surface for the limits** (see §14).

---

## 14. Decisions needed

1. **Per-channel (3 × 2 MiB) or a single per-turn total (6 MiB)?** Per-channel is
   simpler to explain and stops one greedy channel starving the others; a single
   total is stricter but lets a legal text-heavy turn with tiny arguments borrow the
   whole budget. Recommendation: per-channel.
2. **Terminal error, or the daemon's smaller-steps recovery for the arguments
   channel?** Recommendation: terminal, because the recovery instruction asserts a
   cause that is false here.
3. **Should `MAX_TURN_CHANNEL_BYTES` be an overlay/config knob?** Recommendation: no
   until someone needs it; promote the constant in a later change.

---

## 15. Definition of done

- [ ] `MAX_TURN_CHANNEL_BYTES`, `PayloadChannel`, `PayloadBudget`, `PayloadOverflow`
      exist in `choreo-ai-protocols/src/shared.rs` with the rationale documented.
- [ ] `ProviderError::TurnPayloadTooLarge` and `InferenceError::TurnPayloadTooLarge`
      exist, convert 1:1, and carry the `turn_payload_too_large` label.
- [ ] Both `choreo-proto` metric-label tests list it; the count is correct.
- [ ] Every site in the §3 inventory charges a budget, correctly (net growth where a
      channel can be replaced).
- [ ] Non-streaming inference bodies are capped (Phase 5), at one place.
- [ ] Unit tests per adapter per channel, including the shrinking-replacement and
      at-limit cases; one integration test per streaming adapter.
- [ ] `ARCHITECTURE.md` describes the per-turn budget, the arithmetic behind it, and
      the body cap; no other file references this plan.
- [ ] `just pre-commit` green in one pass for each phase commit.
- [ ] This plan file is deleted, and nothing references it.
