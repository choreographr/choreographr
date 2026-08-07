# Reasoning Round-Trip: Capture → Carry → Re-Emit

## Problem

Choreographr captures reasoning text and displays it in the TUI, but never
sends any reasoning data back to the provider. For most providers that is
harmless (the reasoning text is display-only). But it is **protocol-violating**
in the tool-call loop for several providers:

- **Anthropic**: thinking blocks (with encrypted `signature`) must be echoed
  back, complete and unmodified, alongside `tool_use` blocks. Modified or
  missing blocks → **400 error**.
- **DeepSeek / Kimi (OpenAI-compatible chat)**: `reasoning_content` must be
  passed back on every assistant tool-call message when the request carries
  `tools`. Missing → **400 error**.
- **Gemini**: thought-step `signature` (encrypted reasoning state) must be
  passed back; the summary text is display-only.
- **OpenAI / xAI Responses**: opaque reasoning items + `previous_response_id`
  preserve reasoning continuity across calls (recommended, quality).

Root cause (confirmed against `~/agents` implementations — pi, goose,
opencode, tau): the round-trip **artifact** (signature, encrypted payload,
reasoning field name) is discarded at the adapter parse boundary, so no
builder change can ever recover it.

## Reference behavior (what pi / goose / opencode / tau do)

- **Structured block, not text**: store `{ thinking, signature }` and
  `redacted_thinking { data }` per content block (goose
  `conversation/message.rs:170`; pi session JSONL; tau `ThinkingContent`).
- **"Whether to send" derived, not configured**:
  - same-model identity (`isSameModel` in pi `transform-messages.ts:95`)
  - assistant turn has tool calls / is in a tool loop (goose
    `tool_call_turn_reasoning` in `formats/openai.rs:226-528`)
  - thinking is currently enabled for the request (goose `!thinking_disabled`,
    `formats/anthropic.rs:345-362`)
- **"How to send" is the only per-model config** — a catalog enum, like
  goose's `ThinkingPreservationFormat` (`base.rs:228`):
  `ReasoningContent | ContentPrepend | ContentXml`, plus per-model nuance
  flags (`allow_empty_signature`, `requires_reasoning_content_on_assistant`).
- **Cross-model safety**: encrypted/redacted artifacts are dropped when the
  replaying model differs (pi `transform-messages.ts:101-133`).
- **Empty-field handling is per-model**: goose omits empty `reasoning_content`
  (Kimi rejects `""`); opencode always sends it (DeepSeek wants it).

## Design

### Core invariant

> The reasoning round-trip payload is an **opaque, provider-owned artifact**.
> The adapter captures it verbatim at parse time, the daemon stores and
> forwards it untouched, and the adapter re-emits it verbatim on the next
> request. Display text (`assistant_reasoning`) stays a separate field.

Three layers, each owning its concern:

| Layer | Owns |
|---|---|
| Catalog (`choreo-ai-protocols`) | `reasoning_passback` format enum (per-model, protocol-defaulted) |
| Adapters | capture artifact at parse time; re-emit verbatim on request build |
| Daemon (`build_chat_request_messages`) | derives *whether* to send (same-model + tool loop + thinking enabled); never interprets payload |

---

## Implementation Plan

### Phase 1 — Proto type for the artifact (foundation)

**File:** `choreo-proto/src/types.rs`

Add an opaque artifact enum, stored on `Turn`:

```rust
/// Opaque reasoning round-trip payload, captured verbatim by a provider
/// adapter and re-emitted verbatim on the next request. Only the producing
/// adapter may interpret the payload. Display text lives separately in
/// `Turn::assistant_reasoning`.
///
/// Stored as raw bytes so the proto type stays dependency-light and cannot
/// accidentally be interpreted: the producing adapter serializes its own
/// wire representation (e.g. Anthropic block JSON, Gemini signature string)
/// into `Vec<u8>` at parse time and deserializes it back at request-build
/// time. `kind` tags which adapter owns the payload.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", content = "payload")]
pub enum ReasoningArtifact {
    /// OpenAI-compatible chat: the `reasoning_content` string (verbatim).
    ChatReasoning(Vec<u8>),
    /// Anthropic: ordered thinking / redacted_thinking blocks, JSON as
    /// received (signatures + redacted data intact, order preserved).
    AnthropicThinking(Vec<u8>),
    /// Gemini: encrypted thought signatures to send back.
    GoogleSignatures(Vec<u8>),
    /// OpenAI/xAI Responses: opaque reasoning items (or encrypted_content).
    ResponsesItems(Vec<u8>),
}
```

Add the fields to `Turn` (as trailing fields):

```rust
pub struct Turn {
    // ... existing fields ...
    pub displayed_images: Vec<DisplayedImageRecord>,
    /// Opaque reasoning round-trip artifact (None when never captured or
    /// when the provider exposes no reusable artifact).
    pub reasoning_artifact: Option<ReasoningArtifact>,
    /// Which provider+model produced `reasoning_artifact`. Set whenever the
    /// artifact is captured; used for the same-model check at build time
    /// (artifacts are model-bound and must be dropped after a model switch).
    pub reasoning_producer: Option<ReasoningProducer>,
}

/// Identity of the model that produced a reasoning artifact.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ReasoningProducer {
    pub provider_slug: String,
    pub model: String,
}
```

Note: `choreo-proto` stays dependency-light — the artifact payloads are raw
`Vec<u8>`; `serde_json`/string encoding happens only inside the adapter that
produces each payload (`choreo-ai-protocols`, which already depends on
serde_json). No new proto dependencies.

No migration concerns: the project is unreleased, so `Turn` can gain the
field freely — the postcard wire protocol and the redb blobs are all rebuilt
in lockstep from a single workspace. (If a developer's local dev database
holds pre-change turns, `read_turns`' existing skip-with-warning at
db.rs:396-401 is sufficient.)

**Tests** (`#[cfg(test)]` in `types.rs`): serde round-trip for each
`ReasoningArtifact` variant; `Turn` with and without artifact.

---

### Phase 2 — Adapters capture the artifact (root-cause fix)

Each adapter currently discards the round-trip artifact. Fix capture at the
parse boundary.

#### 2a. `choreo-ai-protocols/src/types.rs` (shared turn result)

Add to both result structs:

```rust
pub struct FinalTextResult {
    // ... existing ...
    pub reasoning_artifact: Option<ReasoningArtifact>,
}
pub struct ChatAssistantToolUse {
    // ... existing ...
    pub reasoning_artifact: Option<ReasoningArtifact>,
}
```

`ChatTurnResult` variants unchanged. Update every constructor site.

#### 2b. OpenAI chat completions (`openai/chat_completions.rs`)

- Non-streaming: `AssistantMessage` already parses `reasoning_content` /
  `reasoning` / `reasoning_text` (`take_reasoning`, line 70). Capture the
  chosen field's raw value into `ReasoningArtifact::ChatReasoning` (before
  `take_reasoning` consumes it).
- Streaming (`sse.rs`): accumulate `reasoning_content` and, at
  `ResponseCompleted`, set the artifact. The reasoning delta already flows as
  `StreamEvent::Reasoning`.

#### 2c. OpenAI Responses (`openai/responses.rs`)

- Non-streaming: collect `reasoning` output items (and `encrypted_content`
  in stateless mode) into `ReasoningArtifact::ResponsesItems`. Keep
  `response_id` (already captured).
- Streaming: `ResponsesStreamEvent::ReasoningSummary` already carries the
  summary items (`sse.rs:121`); additionally capture opaque reasoning items.

#### 2d. Anthropic (`anthropic/mod.rs` + `anthropic/requests.rs`)

This is the biggest change because the signature is currently dropped in two
places.

- `mod.rs`: `ContentBlock::Thinking` must keep `signature`;
  `RedactedThinking { data }` must no longer be skipped
  (`response_to_turn_result`, line 384-408). Emit blocks in original order as
  `ReasoningArtifact::AnthropicThinking(Vec<serde_json::Value>)` preserving
  `{"type":"thinking","thinking":…,"signature":…}` and
  `{"type":"redacted_thinking","data":…}`.
- `requests.rs` (streaming): `StreamContentBlock::Thinking` (line 272) and
  the `signature_delta` event (line ~667 in the SSE handler) must accumulate
  signature; `RedactedThinking` blocks must be retained. Reuse the same
  `AnthropicThinking` artifact assembly.

#### 2e. Google (`google/mod.rs` + `google/requests.rs`)

- `mod.rs`: `ResponsePart::Thinking` already has `signature: Option<String>`
  (line 383) but is `#[allow(dead_code)]`. Collect signatures →
  `ReasoningArtifact::GoogleSignatures`.
- `requests.rs` (streaming): capture `thoughtSignature` from thinking parts
  (pi's `google-shared.ts:141-155` is the reference: signature can appear on
  any part type; preserve as-is).

**Tests** (per adapter): feed a canned provider response; assert the produced
`ReasoningArtifact` is byte-exact (Anthropic block JSON incl. signature,
DeepSeek `reasoning_content`, Gemini signatures, Responses items).

---

### Phase 3 — Catalog: per-model passback format

**Files:** `choreo-ai-protocols/src/catalog/mod.rs`, `loader.rs`, and the
`catalog/*.toml` files.

Add to `ModelEntry` (mod.rs:27-39), mirroring `reasoning_supported` /
`responses`:

```rust
/// How reasoning is replayed back to the provider on subsequent turns.
/// Default derived from protocol in `model_reasoning_passback`.
#[serde(default)]
pub reasoning_passback: ReasoningPassback,
```

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReasoningPassback {
    /// Never send reasoning back (display-only providers / fields).
    None,
    /// Echo reasoning on assistant messages that had tool calls
    /// (DeepSeek/Kimi chat, and the minimum for Anthropic tool loops).
    ToolLoop,
    /// Echo reasoning across all turns of the session (Anthropic keep-all,
    /// GPT-5.6 all_turns).
    AllTurns,
    /// Send back encrypted thought signatures (Gemini).
    Signature,
    /// Chain via previous_response_id / opaque reasoning items
    /// (OpenAI/xAI Responses).
    ResponseId,
}
```

Add a resolver `model_reasoning_passback(slug, model) -> ReasoningPassback`
in `catalog/mod.rs` using the same known-model-else-protocol-default pattern
as `model_reasoning_capability` (mod.rs:118). Protocol defaults:

- `openai` protocol with `responses = true` → `ResponseId`
- `openai` protocol with `responses = false` → `ToolLoop`
- `anthropic` → `AllTurns` (per-model override for last-turn-only models)
- `google` → `Signature`
- anything else → `None`

Explicit per-model overrides in TOMLs only where nuance matters (e.g.
DeepSeek = `ToolLoop`, Anthropic keep-all models = `AllTurns`, Cerebras-style
providers that reject replayed `reasoning_content` → a `None`/inline variant).

**Tests:** `model_reasoning_passback` resolution for known/unknown models and
per-protocol defaults (mirror `model_reasoning_capability` tests at
mod.rs:319+).

---

### Phase 4 — Request path: capture, carry, re-emit

#### 4a. `ChatRequestMessage` gains the artifact

`openai/mod.rs:99`:

```rust
pub struct ChatRequestMessage {
    // ... existing ...
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning_artifact: Option<ReasoningArtifact>,
}
```

Each adapter serializes its artifact payload as `Vec<u8>`:

- **OpenAI chat** (`chat_completions.rs`): `ChatReasoning` holds the
  `reasoning_content` string's bytes; re-emit as `reasoning_content` on the
  assistant message (decode to `String` at build time).
- **Anthropic** (`anthropic/mod.rs` + `requests.rs`): `AnthropicThinking`
  holds the serialized JSON array of thinking/redacted_thinking blocks
  (signatures intact, original order); re-emit by deserializing and pushing
  the blocks verbatim into the assistant `content` array. Never rebuild or
  reorder.
- **Google**: `GoogleSignatures` holds the concatenated/encoded thought
  signatures; re-emit by attaching them to the appropriate parts.
- **Responses**: `ResponsesItems` holds the serialized opaque reasoning
  items; re-emit via `previous_response_id` / `input` (4c).

#### 4b. Daemon builder derives *whether* to send

`choreo-daemon/src/requests.rs` — `build_chat_request_messages` (line 1532).
Replace the hardcoded nulling of the three reasoning fields with a policy
check:

```rust
let passback = model_reasoning_passback(provider_slug, model);
// (provider slug + model must be plumbed into this fn — see signature note)

// Same-model check: the artifact is model-bound — it must be dropped after
// a model switch (pi stores provider/api/model per message and computes
// isSameModel; Anthropic requires stripping thinking blocks on model change).
let same_model = turn.reasoning_producer.as_ref()
    == Some(&ReasoningProducer { provider_slug: provider_slug.to_string(), model: model.to_string() });

let include_artifact = same_model && match passback {
    ReasoningPassback::None => false,
    ReasoningPassback::ToolLoop => turn_has_tool_involvement(turn),
    ReasoningPassback::AllTurns => true,
    ReasoningPassback::Signature => true,
    ReasoningPassback::ResponseId => false, // artifact flows via response_id
};
```

```rust
// Assistant message:
reasoning_content: artifact_chat_string_or_none(&turn, include_artifact),
reasoning: None,
reasoning_text: None,
reasoning_artifact: include_artifact.then(|| turn.reasoning_artifact.clone()).flatten(),
```

Notes:
- `build_chat_request_messages` currently takes `(session, system_prompt)`.
  Add `provider_slug: &str` and `model: &str` params (both available in
  `run_agent_loop`). Keep the signature change mechanical.
- `ResponseId` handling lives in the loop (4c), not the builder; the builder
  still passes `previous_response_id` as today.
- The old `estimate_prompt_tokens` comment (line 396-398) must be updated:
  reasoning fields ARE populated now for tool-loop/all-turns policies. Update
  the token estimate to count reasoning content when
  `include_artifact == true` (it is billed as input on keep-all models).

#### 4c. Agent loop: `prev_resp_id` persistence + artifact write-through

`choreo-daemon/src/requests.rs`:

- **Stop resetting `prev_resp_id` to `None` at request start** (line 767) for
  `ResponseId` policy. Persist the last `response_id` on the session config
  or turn, and restore it at the top of each `run_agent_loop` invocation so
  it chains across user turns (matches OpenAI's `previous_response_id` +
  `reasoning.context: all_turns` guidance).
- In `set_assistant_response` call sites (sessions.rs:590, requests.rs:941 /
  956): also store `final_text.reasoning_artifact` / `tool_use.reasoning_artifact`
  into `turn.reasoning_artifact`, and record the producing
  `ReasoningProducer { provider_slug, model }` into
  `turn.reasoning_producer` (both available in `run_agent_loop`). Extend
  `set_assistant_response`'s signature with
  `reasoning_artifact: Option<ReasoningArtifact>` and
  `reasoning_producer: Option<ReasoningProducer>`.
- **Precondition guard**: before sending a request whose policy requires
  echo (`ToolLoop`/`AllTurns`/`Signature`), verify every tool-involving turn
  in history has its artifact. If missing (pre-migration session), `warn!`
  clearly ("reasoning artifact missing; provider may reject this tool-loop
  request"). Optionally, for Anthropic, disable thinking for that request
  (the API's documented graceful degradation) instead of shipping a broken
  turn.

---

### Phase 5 — GUI / client surface

No user-visible change required: `Turn::assistant_reasoning` (display text)
is untouched; `reasoning_artifact` is opaque and ignored by clients. Verify:

- `choreo-tui/src/state.rs` and `markdown_render.rs` construct `Turn`
  literals in tests — add `reasoning_artifact: None` to those literals
  (compile fix only).
- `choreo-proto` `Turn` is sent to clients on `TurnAppended` /
  `TurnFinalized` / `SessionState`; clients already tolerate unknown-by-field
  structs? **No** — postcard is positional. Clients must be rebuilt in lockstep
  (single workspace, same release). Note this in the changelog.

---

### Phase 6 — Testing (full suite)

Per AGENTS.md discipline:

- **Unit tests** (in `src/`): adapter capture tests (Phase 2); catalog
  resolver tests (Phase 3); builder policy tests (Phase 4b):
  - `ToolLoop` attaches artifact only for tool-involving turns
  - `AllTurns` attaches always
  - `None` never
  - undone turns skipped (existing behavior preserved)
  - same-model mismatch drops artifact
  - **model switch mid-session**: turns produced under the previous
    model keep `reasoning_producer` pointing at the old model; builder
    drops their artifacts, so a session that switched
    deepseek → claude doesn't replay `ChatReasoning` into an Anthropic
    request (and vice-versa)
- **Integration tests** (`tests/`, `#[ignore]`, via nextest aliases): a mock
  provider asserting a two-tool-call turn with thinking enabled receives the
  reasoning artifact on the second request (Anthropic block JSON byte-identical;
  DeepSeek `reasoning_content` present; Gemini signature present).
- Run `cargo test-all` (or `cargo nextest run -p <crates>` per crate while
  iterating). Run `cargo clippy` + `cargo fmt`.

### Phase 7 — Documentation

Per AGENTS.md: update `ARCHITECTURE.md` (new `ReasoningArtifact` type, the
`reasoning_passback` catalog field, the capture→carry→re-emit data flow) and
`README.md` (reasoning round-trip behavior, the tool-loop 400 caveat now
fixed).

---

## Recommended implementation order

| # | Step | Crates | Est. effort |
|---|---|---|---|
| 1 | `ReasoningArtifact` + `Turn.reasoning_artifact` field | choreo-proto | 1–2h |
| 2 | Adapters capture artifact (2a–2e) | choreo-ai-protocols | 4–6h (Anthropic is the bulk) |
| 3 | Catalog `reasoning_passback` + resolver | choreo-ai-protocols | 2h |
| 4a | `ChatRequestMessage.reasoning_artifact` + adapter re-emit | choreo-ai-protocols | 3h |
| 4b | Builder policy + token estimate | choreo-daemon | 2–3h |
| 4c | `set_assistant_response` + `prev_resp_id` persistence + guard | choreo-daemon | 2–3h |
| 5 | Client `Turn` literal fixes | choreo-tui, choreo-gui | 1h |
| 6 | Test suite (unit + `#[ignore]` integration) | all | 3–4h |
| 7 | ARCHITECTURE.md / README.md | docs | 1h |

Total: ~18–23h across 3 crates (choreo-proto, choreo-ai-protocols,
choreo-daemon) + client compile fixes. Per AGENTS.md, implement **one
subagent task at a time, in series**, verifying with
`cargo nextest run -p <crates>` on only the touched crates after each step.

## Risks / open questions

1. **`Vec<u8>` payloads** — resolved: `ReasoningArtifact` holds raw bytes;
   `choreo-proto` gains no serde_json dependency. Each adapter (de)serializes
   its own payload. `kind` tags ownership so a mis-wired adapter is caught by
   the round-trip tests.
2. **Same-model provenance — resolved (per-turn provenance required).**
   Model switching mid-session is fully supported (TUI `/model` + Ctrl+M;
   `handle_set_model` at sessions.rs:1239 swaps `selected_model` in place,
   keeps all turns, and already re-validates `reasoning_effort` as a
   capability boundary). Therefore the builder cannot compare against the
   session's *current* model — old turns were produced by a different model.
   Each `Turn` carries `reasoning_producer: Option<ReasoningProducer>`
   (provider_slug + model), and the builder drops the artifact when it does
   not match the current provider+model. This mirrors pi storing
   provider/api/model on every message for `isSameModel`
   (transform-messages.ts:95), and Anthropic's requirement to strip thinking
   blocks on model switch.
3. **`ResponseId` policy artifact flow** — the artifact bytes themselves are
   not replayed for Responses providers; continuity comes from
   `previous_response_id`. Confirm `responses.rs` accepts a response-id from a
   *previous user turn* (it currently only threads within one request) — the
   Responses API requires `store: true` for this to work across calls; verify
   the daemon's store setting.
4. **Anthropic empty-signature edge** — pi/goose differ on whether to replay
   a `signature: ""` (pi `allowEmptySignature`, goose `preserve_unsigned_thinking`).
   Default to "convert to text if no signature" unless a provider needs the
   block; expose as a catalog boolean only if a real provider requires it.
5. **Cross-model drops** — encrypted artifacts are model-bound (pi drops them
   cross-model). The builder's same-model check (4b) covers this; verify it
   also applies to `Signature`/`ResponseId` policies.

## Definition of done

- Anthropic tool loop with thinking enabled no longer risks 400 (artifact
  echoed verbatim).
- DeepSeek tool loop echoes `reasoning_content`.
- Gemini re-emits thought signatures.
- Responses providers chain `previous_response_id` across user turns.
- `cargo test-all`, `cargo clippy`, `cargo fmt` pass.
- ARCHITECTURE.md + README.md updated.
