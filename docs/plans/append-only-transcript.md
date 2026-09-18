# Plan: Append-only transcript (cache-stable conversation)

**Status:** proposed — *not started*. Approved design; implementation deferred.
**Date:** 2026-09-17
**Targets:** `SCHEMA_VERSION 2 → 3` (redb), `PROTOCOL_VERSION 5 → 6` (`choreo-proto`).
**Touches:** `choreo-proto`, `choreo-daemon`, `choreo-client-core`, `choreo-tui`,
`choreo-gui`, `choreo-im`, `choreo-acp`, `choreo-ai-protocols` (adapters).

> **TL;DR.** Provider prompt caching only pays off when a session's request is a
> byte-stable growing prefix. Today the daemon rebuilds the system prompt every
> agent-loop iteration and injects per-turn-volatile content (the session title
> and subdirectory hints) into it, so any change there invalidates the cache for
> the **entire** conversation. We make the conversation a strict **append-only
> transcript**: a single canonical log that is never mutated, rendered together
> with a frozen prefix and an ephemeral tail. Anything that changes is
> *appended as a new entry*, never edited. This document is the complete,
> self-contained specification; it is written to survive to a later implementer
> with no prior context.

---

## Table of contents

1. [Motivation & evidence](#1-motivation--evidence)
2. [The invariant (normative)](#2-the-invariant-normative)
3. [Target architecture](#3-target-architecture)
4. [Decisions log](#4-decisions-log)
5. [Data model & persistence](#5-data-model--persistence)
6. [Migration specification](#6-migration-specification)
7. [Wire protocol changes](#7-wire-protocol-changes)
8. [Work breakdown](#8-work-breakdown)
9. [Phased execution plan](#9-phased-execution-plan)
10. [Testing strategy](#10-testing-strategy)
11. [Behavior-parity checklist](#11-behavior-parity-checklist)
12. [Risks & mitigations](#12-risks--mitigations)
13. [Out of scope / future work](#13-out-of-scope--future-work)
14. [Open questions](#14-open-questions)
15. [Verification / definition of done](#15-verification--definition-of-done)
16. [Appendices](#16-appendices)

---

## 1. Motivation & evidence

### 1.1 The cost problem

Every turn resends the whole conversation (except on chained Responses models,
see §3.7). Providers bill the *uncached* portion of the prompt at full input
rate; a provider-side **prefix cache** lets a session reuse the tokens it has
already sent. Caching is keyed on the **longest unchanged token prefix** of the
request. If the very first bytes (`system`, then `tools`, then early messages)
change, the cache misses for the entire conversation and the whole transcript is
re-billed at full price. On long sessions this dominates cost and latency.

### 1.2 The concrete defect

`build_system_content` (`choreo-daemon/src/requests/system_content.rs`) rebuilds
`messages[0]` on **every agent-loop iteration** from:

1. `system.md` base prompt (`choreo-daemon/system.md`, `include_str!`);
2. tool-groups listing;
3. available skills (name/description/location);
4. loaded-skill bodies;
5. project context files (AGENTS.md/CLAUDE.md, fingerprint-cached);
6. **`## Current Session Title`** — the live session title;
7. **`## New context from project subdirectories`** — subdirectory hints.

Items **6 and 7 are volatile**: (6) changes on every `set_session_title`, and
(7) is produced for one build then cleared (`pending_hints.clear()` in
`requests.rs`), so it appears on turn *N* and vanishes on *N+1*. Either change
mutates `messages[0]` → the whole conversation's cache is thrown away. Even
without a change, rebuilding a large prompt every iteration is wasteful.

The pending work to *increase* title-setting frequency (stronger prompting,
a deterministic nudge) makes this worse unless the delivery mechanism moves.

### 1.3 What every comparable agent does (survey of `~/agents`)

Verified in source; this is the evidence base for the design:

- **`pi`** places exactly three Anthropic `cache_control` breakpoints (system
  prompt, **last tool definition**, last user block) and the system prompt
  contains **no timestamp/date/git state**; old tool results are never pruned
  ("rewriting history would invalidate the cache every turn"). OpenAI uses a
  stable `prompt_cache_key = sessionId` + session-affinity headers.
- **`goose`** encodes the invariant as a test:
  `crates/goose-provider-types/tests/prefix_invariance.rs` — *"Across the
  consecutive requests of a session, the cache-relevant bytes a provider has
  already seen must never change… request N must be a verbatim item-prefix of
  request N+1."*
- **`codex`** pins `prompt_cache_key` to the session id and has
  `core/tests/suite/prompt_caching.rs` asserting *"the entire prefix from the
  first request should be identical and reused"*; it keeps `reasoning.effort`
  constant *"to preserve the prompt prefix for caching."*
- **`deepseek-harness`** documents per-feature **"KV Cache effect: Append-only;
  newly visible content follows the reusable request prefix and does not
  invalidate existing KV Cache entries."** Workspace instructions, nested
  AGENTS.md, and edits/removals are all durable `user/message` events; a
  removal is an *appended* notice, never a rewrite. Injected file text is
  escaped so `</system-reminder>` cannot break the frame.
- **`hermes-agent`** (hooks doc): *"Context injection location: always injected
  into the USER MESSAGE, not the system prompt. This preserves the prompt
  cache."* It persists the exact bytes sent (`api_content`) so replay is
  byte-stable, and uses deterministic tool-call ids because *"random UUIDs would
  break the provider prompt cache."*
- **`opencode`** injects `<system-reminder>` blocks into the message stream via
  `session/reminders.ts`; system prompts tell the model these are harness
  instructions.
- **`ironclaw`** is the cautionary tale (`docs/internal/research/pi-agent-deep-dive.md`):
  prepending inline control nudges *before* identity plus a minute-precision
  timestamp in the system block caused a measured **82% → 29% cache-hit
  collapse**. Their fix: move all of it to the transcript tail.

**Conclusion:** the fix is not "prompt the model harder to set the title"; it is
to stop putting *any* volatile content before the conversation.

---

## 2. The invariant (normative)

> **The non-ephemeral transcript of a session is append-only.** The codebase
> must provide no operation that can alter it except *appending*. Each request's
> transcript is either an **append-extension** or a **tail-truncation** of the
> previous request's — never an edit, insert, reorder, or interior change. When
> something changes, the change is expressed as a **new appended entry**.

Corollaries:

- The **`system` head** is part of the non-ephemeral transcript and is therefore
  frozen for the session's lifetime.
- There is **no mutable slot** for volatile content; volatility is expressed as
  an appended entry (persistent change) or the ephemeral tail (§3.5).
- The only permitted deviation from "append" is **`truncate_to(index)`** — a pop
  from the tail. This is allowed because it is cache-safe: the retained prefix
  is a *prefix* of what the provider already cached, so undo does not bust the
  cache.
- The only content outside the invariant is the **ephemeral overlay** (§3.5),
  which is rendered *after* the entire non-ephemeral prefix and is never
  persisted.

### 2.1 The two operations

```
Transcript::append(entry)          // the only mutation that adds
Transcript::truncate_to(index)     // the only mutation that removes (undo)
Transcript::iter()                 // read-only
```

No `set`, no `insert_at`, no `swap`, no `&mut` accessor, no index-assign. The
`Vec` is private to the `Transcript` type. A `#[cfg(test)]`/build guard test
asserts the type exposes no mutation API beyond the two above.

### 2.2 Sanctioned exceptions (each documented, none an in-place edit)

| # | Behaviour | Status under the invariant |
|---|---|---|
| 1 | **Ephemeral epilogue** (title nudge, future transients) | Outside the transcript; rendered after the prefix; bounded, escaped |
| 2 | **Undo / redo** | `truncate_to` (pop) + re-append; cache-safe |
| 3 | **Model / provider switch** | Rendering reset; transcript unchanged; drop foreign-model reasoning artifacts + `last_response_id` |
| 4 | **Image decay (model-facing)** | Ephemeral overlay: bytes ride the in-flight request only (§3.5); durable transcript stores refs, unchanged |
| 5 | **Tool-set change** | **Not** an exception in the normal case (frozen superset, §4.D); a genuine hot-add appends a `Context::Tool…` entry and may bust the *prologue* on non-deferral protocols (documented, §4.D) |
| 6 | **Future compaction** | Out of scope; if ever added it is a deliberate reset |

---

## 3. Target architecture

### 3.1 Layers

```
SessionState
├── static_prefix_inputs : Option<PrefixInputs>   // B′: frozen at first render (§4.E)
├── transcript           : Transcript             // append-only, persisted (§5)
├── draft                : Option<Draft>          // in-flight turn, NOT in the log (§3.3)
├── redo                 : Vec<TranscriptEntry>   // tail popped by undo (§3.6)
├── injection_state      : InjectionState         // dedup bookkeeping (§3.4)
└── runtime…             // sockets, provider client, registries, discovery
```

Rendering is a pure function of the session + model identity:

```
render_request(session, provider_slug, model) -> WireRequest
  = render_prefix(session.static_prefix_inputs)        // §3.2
  + render_transcript(session.transcript)              // §3.3, with image overlay
  + render_epilogue(session)                           // §3.5
```

`render_request` is the *single* path used by the agent loop, `session_inspect`,
and token estimation.

### 3.2 Static prefix (B′)

The prefix is `system.md` + ordered tool list (+ schemas) + ordered skills
catalog. Under **B′** we persist the prefix **inputs** (not a rendered blob) and
re-derive deterministically each render. Requirements:

- **No timestamp / date / git / cwd / random** anywhere in the prefix.
- **Sorted** tool list and skills catalog; stable project-file ordering.
- A test asserts **two renders of the same session are byte-identical**.
- A test asserts the prefix contains no timestamp-like content.

Derivation is pure, so a re-derive is always self-consistent with the transcript
(no mixed-format request is structurally possible). Within a session (same
binary) bytes are stable → cache holds. Across an upgrade bytes may change → one
clean bust, no corruption.

> **Why B′ and not "persist the rendered blob + `render_format_version`" (B):**
> B stores a derived artifact that can drift from the renderer and needs a
> hand-maintained version constant to detect it. B′ makes a mixed-format request
> *structurally impossible* and removes a persisted artifact and a version
> field. The only B′ cost — determinism — is something we must guarantee anyway.

### 3.3 Transcript, entries, and the draft

```rust
// daemon-internal canonical type (re-exported into choreo-proto under the
// protocol bump, §7)
pub enum TranscriptEntry {
    Turn(Turn),            // one agent-loop iteration (see draft rule)
    Context(ContextEntry), // durable injected context (§3.4)
}

pub enum ContextEntry {
    ProjectInstructions { source: String, body: String }, // AGENTS.md/CLAUDE.md at start
    InstructionsUpdated { source: String, body: String }, // on edit
    InstructionsRemoved { source: String },               // on delete
    ScopeInstructions   { source: String, body: String }, // subdir AGENTS.md, first touch
    SkillLoaded         { name: String, body: String },   // via load_skill tool result (§4.C)
    ToolGroupEnabled    { name: String },
    ToolGroupDisabled   { name: String },
    ToolAvailable       { name: String, schema_json: String }, // hot-add discovery
    WorkingDirChanged   { path: String },
}
```

**Draft rule.** Today a `Turn` is mutated in place throughout the agent loop
(`start_turn` → stream → `set_assistant_response` → run tools → append
`tool_results`). Under append-only, an entry is **immutable once appended**.
Therefore:

- the in-flight turn is a **draft** owned by the session/worker, **not** in the
  log;
- streaming (`OutputChunk`, `ToolResultChunk`) mutates the **draft** only;
- the draft is **committed** (appended) at the **end of each agent-loop
  iteration** — i.e. one `TranscriptEntry::Turn` per iteration, matching today's
  per-iteration `start_turn`/`TurnAppended`;
- `SessionState` sent on attach **includes the current draft** so a mid-stream
  attacher sees the live turn.

**Placement.** `Context` entries are appended **only at turn boundaries** (after
all of a turn's tool results) so the assistant `tool_calls` → `tool_result`
adjacency is never broken.

### 3.4 Injection & dedup (`InjectionState`)

Persisted bookkeeping so the same context is never appended twice:

- set of context **sources** already injected (project files, subdir paths);
- per-source **fingerprint** (last-injected content hash / mtime) for change
  detection → append `InstructionsUpdated`/`InstructionsRemoved`;
- set of **tool groups** currently enabled (for tool-change detection).

New subdir AGENTS.md → one `ScopeInstructions` entry on first touch, then
deduped. Project-file edit → one `InstructionsUpdated`. This replaces the
runtime-only `pending_hints`/`known_hint_paths` and `context_cache`, and — unlike
today — is **persisted**, so a resumed session replays the identical entries and
stays cache-consistent.

### 3.5 Ephemeral overlay (rendered, never persisted)

Two things live here, both rendered **after** the whole non-ephemeral prefix:

1. **Image overlay.** `TranscriptEntry::Turn` stores image **references**
   (path + metadata), not model-facing bytes. The overlay supplies the *bytes*
   for turns belonging to the request in flight (today's `in_flight_from_turn` /
   `tool_result_image_messages` decay gate). Once the request ends the overlay
   drops them; the durable transcript is unchanged and the next request renders
   placeholders. (Client *display* bytes are a separate, persisted concern,
   §5.4.)
2. **Epilogue.** A single optional, self-clearing tail message: the **title
   nudge** (while the title is unset), and room for future transients. Content
   is recomputed each request; it is bounded and `<system-reminder>`-wrapped
   with `</system-reminder>` escaped in any repo-controlled text.

The epilogue is the **only** channel for content that must appear/disappear
without being appended (a durable nudge cannot "stop").

### 3.6 Undo / redo

- `undo` → `transcript.truncate_to(idx)`; the popped suffix is moved to `redo`.
- `redo` → append the `redo` entries back (identical bytes → identical rows).
- New user input after an undo **clears** `redo` (today's behavior).
- Persistence is **insert** (append) and **tail range-delete** (truncate) only —
  never an in-place row update. The disk log is append-only too.

### 3.7 Chained protocols (Responses `previous_response_id`)

Unchanged in spirit: chained requests send only the messages that postdate the
last assistant message. The **epilogue** must ride that tail (it does — it is
appended last). Undo clears `last_response_id` (already implemented). A session
with no chaining sends the full transcript and relies on prefix caching.

### 3.8 Reset conditions (no stored "epoch")

We do **not** persist an "epoch" struct. The reset conditions are derived:

- **model/provider switch** → drop foreign-model reasoning artifacts and
  `last_response_id` (already the behavior via `ReasoningProducer` provenance);
  accept one re-read. Transcript untouched.
- **`render_format_version`** — *not needed under B′* (no stored rendered blob
  to mix formats with). If B were chosen instead, this constant would be
  required. **B′ is chosen; omit it.**
- **tool-set change** → frozen superset means nothing changes normally; a
  genuine hot-add appends a `Context::Tool…` entry and may bust on non-deferral
  protocols (§4.D).
- **`reasoning_effort` change** → **not** a reset. It is a top-level request
  parameter, not part of the prefix; on providers where it interacts with the
  prefix (GPT-5/Responses), use the provider's delta/configuration mechanism.
  It remains a plain mutable session field.

---

## 4. Decisions log

Format: **decision** — rationale — *alternatives rejected*.

**A. `TranscriptEntry` is canonical across memory, disk, and wire.**
One type; no daemon-local/proto split; no projection layer. *Rejected: keep
`Turn` as wire type and project from a daemon-local entry type (permanent
translation layer, dual model).*

**B. Bump `PROTOCOL_VERSION 5 → 6`.** Enables collapsing four turn-scoped events
into two transcript events and deleting the `undone` soft-delete machinery.
*Rejected: no bump + wire-stays-`Turn` projection (carries the dual model
forever).* Cost: hard cutover — daemon + all clients ship together (no version
negotiation; the frame decoder rejects mismatched versions). Acceptable: a small
user base on coordinated releases.

**C. `Turn.undone` is removed.** Undo is truncation, observable as
`TranscriptTruncated`. *Rejected: keep the flag (vestigial, keeps soft-delete
logic and redo-payload plumbing).*

**D. Tools: freeze-superset + native deferral + documented hot-add bust.**
- Freeze the tool set for the session to the superset discovered **before the
  first model request** (local tools + MCP servers). `tools[]` is byte-stable →
  never busts, on every protocol.
- Where supported, prefer **native deferral** as an optimization: Anthropic
  `defer_loading` (+ the stable deferred placeholder from request 1, or the
  first deferred tool full-misses) and OpenAI Responses `additional_tools` /
  `tool_search`. These keep the top-level array stable by design.
- A genuinely new tool appearing mid-session (hot-added MCP/plugin): append a
  `Context::ToolAvailable` entry; on deferral protocols no bust; on
  chat-completions/Gemini/Mistral/compat the `tools[]` array changes → **one
  documented full re-read**. The log is appended; the DB is not rewritten.

*Rejected: a universal undeclared-tool "meta-tool" that injects schemas as text
and calls tools by name — breaks providers (the tool isn't in the tool-calling
grammar; strict gateways 400). No surveyed agent does this. Rejected: a declared
`invoke_tool` dispatcher (universal-safe but changes UX and loses native schema
validation) — revisit only if mid-session hot-adds must never bust.*

**E. Prefix storage: B′.** Persist frozen prefix **inputs**; re-derive
deterministically; **no `render_format_version`.** See §3.2. *Rejected: B
(persisted rendered blob + version constant) and B″ (persist nothing,
re-derive from the current binary — no record of why a session missed).*

**F. Canonical on disk (Option 2), with a real migration.** One canonical log
table; `SCHEMA_VERSION 2 → 3`; the migration re-encodes rows and is exercised
against a real v2 DB. *Rejected: Option 1 (two tables, memory-only unification —
not canonical).*

**G. Context entries are broadcast to all clients; only TUI renders them
(initially).** The TUI cannot render/expand an entry it isn't sent. Bodies are
included (images dominate the frame budget, §12). GUI/IM receive but ignore for
v1.

**H. Subsessions are fresh, independent sessions** (inherited config, empty
log). Their prefix derives from the same environment → likely byte-identical to
the parent's, giving free cross-session prefix reuse, but no log copy.
*Deferred: "session trees"/forking that copies a transcript prefix.*

**I. The title value is not in the model context at all.** It is UI metadata; the
`set_session_title` call+result already record it in history. Only the ephemeral
nudge is delivered.

**J. `SCHEMA_VERSION` shape migration is eager (whole-DB, first run); prefix-input
materialization is lazy (per session, first render).** The shape rewrite is a
pure data transform and the migration framework mandates whole-DB. Prefix inputs
are environment-derived and must NOT run inside a migration. See §6.

**K. `MAX_FRAME_SIZE` is unchanged** (32 MiB). It is a single-frame allocation
bound (codec + Noise reassembly), distinct from the per-client lag-eviction
byte counter. Context bodies ride `SessionState`; images dominate.

---

## 5. Data model & persistence

### 5.1 Tables (redb)

| Table | Key → Value | Change |
|---|---|---|
| `sessions` | `u64 → msgpack(SessionRecord)` | add `#[serde(default)]` fields (§5.3) |
| **`session_transcript`** | `(u64, u32) → zstd(msgpack(TranscriptEntry))` | **new**, replaces `session_turns` |
| `session_attachments` | `(u64, u32, String) → &[u8]` | key's middle element renamed `turn_id → seq` (same numeric space) |
| **`session_prefix`** | `u64 → zstd(msgpack(PrefixInputs))` | **new** (B′ inputs) |
| `session_kv`, `meta`, `catalog_state`, `credentials`, `deleted_sessions`, `keystore` | unchanged | — |

`seq` is one monotonic counter for **all** entries. For history entries
`seq == turn_id` (preserves attachment keys with no data move); `Context`
entries take new `seq` values. The key type `(u64, u32)` is unchanged — only the
**value type** changes, which is the breaking codec change that owns the 2 → 3
bump (§6).

**Listing hot path:** `read_all_sessions` decodes only `sessions`; it must
**never** decode the log or prefix.

### 5.2 SessionState (runtime)

Replace `turns: BTreeMap<u32, Turn>` with the new fields (§3.1). Keep
`next_turn_id` (now `next_seq`). Remove `last_undo_turn_ids` (redo is the popped
suffix). `SessionConfig` keeps its persisted scalars.

### 5.3 `SessionRecord` diff

- Add: `#[serde(default)] prefix_inputs: Option<PrefixInputs>` (or store in
  `session_prefix` — decide at implementation; both are additive-compatible).
- Add: `#[serde(default)] injection_state: InjectionState`.
- Remove: nothing (leave `last_response_id` etc.).

All additions use `#[serde(default)]` so old records decode. The transcript
itself is NOT a `SessionRecord` field (separate table).

### 5.4 Images (two consumers, one field)

- **Client display bytes**: persisted in `session_attachments`, as today, so
  reattach still renders images.
- **Model-facing bytes**: ephemeral overlay only (§3.5).
- `TranscriptEntry::Turn` stores image references; `read_transcript` re-attaches
  display bytes into the reference (unchanged reader logic, `turn_id → seq`).

### 5.5 Delete / purge

`delete_session`, `delete_session_turns`, and tombstone purge must cover
`session_transcript` and `session_prefix` (or the record field), using the same
`(session_id, …)..(session_range_end(session_id), …)` bounds. No orphaned rows.

---

## 6. Migration specification

Two distinct steps; do not conflate.

### 6.1 Shape migration (`SCHEMA_VERSION 2 → 3`) — eager, whole-DB

Per `db/mod.rs`'s `Migration` contract:

- exactly **one redb write transaction**;
- decode historical rows with a **frozen local copy** of the old `Turn` struct;
- **idempotent** under re-run;
- add `Migration { from: 2, run: migrate_turns_to_transcript }` to `MIGRATIONS`,
  set `SCHEMA_VERSION = 3`, update the chain test
  (`production_migration_chain_matches_schema_version`).

Algorithm:
```
for each (sid, seq, bytes) in session_turns:
    old = decode_turn_frozen_v2(zstd_decode(bytes))
    new = zstd_encode(msgpack(TranscriptEntry::Turn(old)))
    session_transcript.insert((sid, seq), new)
# attachments: keys unchanged (mid key already the seq)
# idempotence: tolerate an already-populated session_transcript row (skip or overwrite identically)
```

Run at startup, before any table access, after the pre-migration backup (the
existing runner takes the backup; keep it).

### 6.2 Prefix-input materialization (B′) — lazy, per session

**Not** part of the migration (it needs the live environment: working dir,
AGENTS.md on disk, skills, tool registry — and a migration must be pure).

```
render_request(session):
    if session.prefix_inputs is None:
        session.prefix_inputs = Some(derive_prefix_inputs(session))  // reads env
        persist(session, "prefix inputs materialized")
    prefix = render_prefix(session.prefix_inputs)     // pure from here on
```

Consequences:
- The first render of a resumed old session is a **guaranteed one-time bust**
  (binary changed; old hints/skills were runtime-only). Expected and logged.
- A transient "shape migrated, inputs not yet materialized" state is a normal
  `None` branch, not corruption.
- Sessions never reopened are never materialized (cheap).

### 6.3 Backup / version gate

Keep the existing `migration_backup_version` / pre-lock backup flow. A newer DB
(downgrade) is refused as today.

---

## 7. Wire protocol changes

`PROTOCOL_VERSION 5 → 6` (`choreo-proto/src/frame.rs`). The decoder rejects a
mismatched version → daemon and clients upgrade atomically (release note).

### 7.1 Events

Replace on `SessionEvent`:
- **remove** `TurnAppended { turn_id, turn }`
- **remove** `TurnsUndone { turn_ids }`
- **remove** `TurnsRedone { turns }`
- **add** `TranscriptAppended { seq, entry }`
- **add** `TranscriptTruncated { through_seq }`

`SessionState` carries the transcript (entries) plus the current **draft**.

Redo = `TranscriptAppended` of the restored entries (no special event). Undo is
observable as a truncation (client drops `seq > through_seq`).

### 7.2 Clients

- **`choreo-client-core`**: `SessionView.turns: BTreeMap<u32,Turn>` →
  `entries` (ordered by `seq`) + `draft`. `TurnEventHandler` collapses to
  `handle_transcript_appended` / `handle_transcript_truncated`.
- **`choreo-tui`**: render `Context` entries as **collapsed rows** (reuse
  tool-result collapse hit-testing; **no dim styling** — a small label prefix +
  expand-to-body). The four `turns_undone_prunes_*` behaviours collapse to "drop
  entries with `seq > through_seq`."
- **`choreo-gui`**, **`choreo-im`**, **`choreo-acp`**: adapt to the new events;
  **ignore** `Context` entries for v1.
- **`choreo-proto`**: `Turn.undone` removed; `TranscriptEntry`/`ContextEntry`
  defined here.

### 7.3 Size accounting

`choreo-proto/src/size.rs` gains `TranscriptEntry`/`ContextEntry` arms; the
per-client lag gauge must still never under-estimate.

---

## 8. Work breakdown

- **`choreo-proto`**: `TranscriptEntry`, `ContextEntry`; drop `Turn.undone`;
  remove `TurnAppended`/`TurnsUndone`/`TurnsRedone`; add
  `TranscriptAppended`/`TranscriptTruncated`; `PROTOCOL_VERSION 6`; `SessionState`
  carries entries + draft; `size.rs`; remove the stale `SessionMessage`/
  `SessionMessageKind` rows from `ARCHITECTURE.md`.
- **`choreo-daemon/db`**: `session_transcript` + `session_prefix` tables;
  `read/write/delete` entry fns; the 2 → 3 migration; attachments key rename;
  list-path untouched.
- **`choreo-daemon/sessions`**: `SessionState` new fields; `Transcript` type;
  draft lifecycle; undo/redo via `truncate_to` + redo vec; snapshot/merge;
  `turn_for_client` → `entry_for_client` (strip reasoning artifacts); the
  undo-during-in-flight-request race re-expressed against `seq` bounds;
  subsession inheritance unchanged in shape.
- **`choreo-daemon/requests` + `reasoning`**: build via `render_request`;
  `build_session_reminder` (epilogue); `Context` emission at turn boundaries;
  delete `pending_hints`/`known_hint_paths`/`loaded_skill_bodies`/`context_cache`
  and the system-content title/hint blocks; `InjectionState`.
- **`choreo-daemon/requests/system_content`**: shrink to the static prefix.
- **`choreo-daemon/tools/session_inspect`**: render via `render_request`.
- **`choreo-daemon/tools/admin/load_skill`**: ensure the tool **result** carries
  the skill body (so it rides the transcript, §4.C).
- **Clients**: as §7.2.
- **`choreo-ai-protocols`**: cache breakpoints (Anthropic system + last tool +
  last user; OpenAI-compat `prompt_cache_key`); native deferral where supported;
  a per-protocol capability table.

---

## 9. Phased execution plan

Each phase is independently shippable, tested, and **committed** (per
`AGENTS.md`: one subsession per task, in series, full `just pre-commit` gate,
commit before returning).

- **Phase 0 — Foundations (no behavior change).** `Transcript`, `TranscriptEntry`,
  `ContextEntry`, `PrefixInputs`, `InjectionState`; `render_request` as a shim
  reproducing today's output from the current fields behind the new types.
- **Phase 1 — Prompting + Option-A core.** `system.md` title section →
  imperative first-action; `set_session_title` description + arg doc → imperative;
  remove title/hint blocks from system content; add the **epilogue**.
- **Phase 2 — Durable context.** Project files, subdir hints, skill bodies
  (via `load_skill` result) become appended `Context` entries; delete
  `pending_hints`/`loaded_skill_bodies`/`context_cache`; `InjectionState` dedup.
- **Phase 3 — Tools.** Freeze-superset at session start (MCP discovered
  pre-first-request); `load_tools`/`unload_tools` → append + dispatch gating;
  native deferral in adapters; `Context::Tool…`.
- **Phase 4 — Images as ephemeral overlay.** Refs in the entry; bytes in the
  overlay; delete `in_flight_from_turn` from the builder.
- **Phase 5 — Undo/redo via `truncate_to`.** Remove `undone` flags and
  `last_undo_turn_ids`.
- **Phase 6 — Provider caching.** Breakpoints + `prompt_cache_key`; capability
  table.
- **Phase 7 — Persistence + migration.** New tables, `SCHEMA_VERSION 3`, the
  2 → 3 migration, lazy prefix materialization, delete/purge coverage.
- **Phase 8 — Wire bump + clients.** `PROTOCOL_VERSION 6`; all clients; TUI
  collapsed `Context` rows.
- **Phase 9 — Docs + gate.** `ARCHITECTURE.md` (new "Prompt-cache discipline &
  append-only transcript" section; loop-flow update; remove stale `SessionMessage`
  rows), `CHANGELOG.md`, this plan marked done.

Dependencies: 0 → (1,2) → (3,4,5) → 6 → 7 → 8 → 9. Phases 1–2 deliver the
immediate caching win; 7–8 are the heavyweight ones.

---

## 10. Testing strategy

- **Prefix-invariance regression** (goose-style): two consecutive requests of a
  session — assert `tools` + system prefix + every non-tail message are
  byte-identical; only the tail differs. Across protocols (anthropic,
  openai-chat, openai-responses, google).
- **No-mutation guard**: the `Transcript` exposes no mutation API beyond
  `append`/`truncate_to`.
- **Change-is-append**: editing AGENTS.md / toggling a tool group / loading a
  skill each **append** and leave prior messages byte-identical.
- **Ephemeral isolation**: epilogue + image overlay never appear in the durable
  transcript and are absent on replay/resume.
- **Undo/redo** truncate/append round-trips; prefix-invariance across undo.
- **Determinism**: two renders of one session are byte-identical; the prefix
  contains no timestamp.
- **Resume**: a resumed session renders byte-identically to the pre-save request
  (after migration).
- **Migration**: v2 → v3 round-trip; chain-contiguity test updated.
- **Frame size**: a large `SessionState` (images + context) encodes within
  `MAX_FRAME_SIZE` or the acceptable-diff is documented.
- **Per-protocol wire**: `TranscriptAppended`/`TranscriptTruncated` and
  `SessionState` round-trips.

---

## 11. Behavior-parity checklist

Must not regress:

- Undo processed while a request worker is in flight (today compares `undone`
  flags; re-express as "live log shrank below the snapshot's base `seq`" and
  spec the worker's behavior on committing into a truncated suffix).
- `last_response_id` clearing on undo + provenance gate on replay.
- Reasoning-artifact provenance (drop foreign-model artifacts).
- Image decay semantics (model-facing) unchanged in effect.
- Token accounting, `last_prompt_tokens`, context-window display, OSC progress.
- `session_inspect` dry-run fidelity (must equal the real request).
- Streaming draft visibility on attach.
- `get_session`/`list_sessions` do not leak context bodies unnecessarily.

---

## 12. Risks & mitigations

- **Persisted-shape migration** (highest risk): single write txn, frozen old
  struct, idempotent, tested against a real v2 DB, pre-migration backup.
- **Prefix determinism** (B′ depends on it): sorted lists, no env/time in the
  prompt, determinism test.
- **Client lockstep**: `PROTOCOL_VERSION` gate makes mismatch fail fast; release
  note; ship together.
- **Frame size**: context bodies add to `SessionState`; images dominate. If a
  session ever exceeds 32 MiB, ship labels-only + fetch-on-expand (fallback
  documented).
- **Security/exposure**: context bodies (AGENTS.md, skill text) are now sent to
  all clients on the wire, including IM/GUI that don't render them. Same class as
  tool outputs already sent; note it explicitly and consider a redaction/limit
  policy later.

---

## 13. Out of scope / future work

- **Session trees / forking** (copy a parent's transcript prefix into a child).
- **Compaction** (a deliberate reset if added).
- **Native deferral details** beyond the capability matrix (Fireworks `ToolSearch`,
  Gemini `CachedContent`, etc.).
- **Client rendering of `Context`** in GUI/IM (TUI only for v1).
- Revisiting `MAX_FRAME_SIZE` / attach pagination.

---

## 14. Open questions

*None blocking.* Minor items to settle at implementation:

- Exact rendered framing of each `ContextEntry` variant (proposed: a durable
  `user` message wrapped in `<system-reminder>…</system-reminder>`, escaped).
- Whether `prefix_inputs` / `injection_state` live in `SessionRecord` or their
  own tables (both additive; §5.3).

---

## 15. Verification / definition of done

- The invariant holds under a scripted stress scenario (append + context change
  + undo + resume); the prefix-invariance test passes across protocols.
- A two-turn scripted session shows a **higher cache-hit ratio** than the
  pre-change baseline (provider-reported `cached_tokens`).
- `SCHEMA_VERSION 3` + migration passes against a real v2 DB; `PROTOCOL_VERSION 6`
  with all clients building; full `just pre-commit` green.
- `ARCHITECTURE.md`, `README.md`, `CHANGELOG.md` updated; stale
  `SessionMessage`/`SessionMessageKind` docs removed.

---

## 16. Appendices

### 16.1 Provider cache-capability matrix

| Protocol | Prefix cache | Explicit tool deferral | Tool change w/o bust |
|---|---|---|---|
| Anthropic Messages | `cache_control` breakpoints | yes (`defer_loading` + placeholder) | yes |
| OpenAI Responses | `prompt_cache_key` + retention | yes (`additional_tools`/`tool_search`) | yes |
| OpenAI Chat Completions | automatic + `prompt_cache_key` | no | no — freeze-superset or documented bust |
| Gemini | implicit + `CachedContent` | no | no |
| Mistral / compat gateways | automatic (best-effort) | no | no |

### 16.2 Survey citations (file:line)

- pi: `packages/ai/README.md:1409`; `packages/ai/src/api/anthropic-messages.ts:187-200,1256-1268`;
  `packages/ai/src/types.ts:449-452`.
- goose: `crates/goose-provider-types/tests/prefix_invariance.rs:1-8,231-285`.
- codex: `codex-rs/core/tests/suite/prompt_caching.rs:556-562,826-832`;
  `codex-rs/core/src/client.rs:552-573`; `codex-rs/tools/src/tool_search.rs`.
- deepseek-harness: `packages/context/agent-instructions/README.md:149-205`;
  `packages/skill/tool-skill/src/index.ts:256-305`.
- hermes-agent: `website/docs/user-guide/features/hooks.md:555-557`;
  `website/docs/user-guide/configuration.md:1314`;
  `website/docs/developer-guide/context-compression-and-caching.md`.
- opencode: `packages/opencode/src/session/reminders.ts`;
  `packages/opencode/src/session/prompt/*.txt`.
- ironclaw: `docs/internal/research/pi-agent-deep-dive.md:284-337,415-473`.

### 16.3 Glossary

- **Entry** — one immutable element of the transcript (`Turn` or `Context`).
- **Draft** — the in-flight turn, not yet in the log; committed at iteration end.
- **Prologue / prefix** — the request head: `tools[]` + `system`. A *rendering*,
  not the log.
- **Epilogue** — the ephemeral tail (title nudge + future transients).
- **Overlay** — ephemeral per-request content (image bytes) rendered over the log.
- **Epoch** — *not a stored thing*; shorthand for the conditions that force a
  rendering reset (model/provider switch, tool-set change).
- **Bust** — a provider cache miss from a given position; a *rendering* property,
  never a DB write.

### 16.4 Non-goals of this document

- It does not specify final Rust signatures (they will drift); it specifies
  **contracts and invariants**.
- It does not duplicate `ARCHITECTURE.md`; that file is updated during Phase 9.
