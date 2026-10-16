# Plan: Session trees (branching transcript, cursor undo)

**Status:** proposed — *not started*. Companion to [`append-only-transcript.md`](./append-only-transcript.md); read that first.
**Date:** 2026-10-16
**Targets:** reshapes the transcript model that `append-only-transcript.md` already introduces. **No additional** `SCHEMA_VERSION` or `PROTOCOL_VERSION` bump beyond that plan's — this document changes *what those bumps carry*, not how many there are.
**Touches:** `choreo-proto`, `choreo-daemon`, `choreo-client-core`, `choreo-tui`, `choreo-gui`, `choreo-im`, `choreo-acp`.

> **TL;DR.** [`append-only-transcript.md`](./append-only-transcript.md) makes a
> session's conversation a strict append-only log, with one sanctioned removal:
> `truncate_to(index)`, used for undo. Undo-then-new-input is *already a fork* —
> the truncated suffix and the new turn are siblings — but the plan **discards
> the losing branch**. This document keeps it. We replace "undo = truncate" with
> "undo = move a cursor", replace the single-slot `redo` vec with "the children
> of the cursor", and render the **path** root→cursor instead of the whole log.
> The cache argument is provably unchanged (the rendered path after undo is a
> byte-prefix of the previous path), the append-only invariant gets *stronger*
> (the log loses its one exception), and the only genuinely new state is a
> per-branch response-chain id. Session trees and undo become the same feature.

---

## Table of contents

1. [Relationship to append-only-transcript.md](#1-relationship-to-append-only-transcriptmd)
2. [The insight: undo already forks](#2-the-insight-undo-already-forks)
3. [The invariant, restated](#3-the-invariant-restated)
4. [Cache safety (why branching costs nothing new)](#4-cache-safety-why-branching-costs-nothing-new)
5. [Target architecture](#5-target-architecture)
6. [Data model & persistence](#6-data-model--persistence)
7. [Wire protocol](#7-wire-protocol)
8. [Undo / redo / fork semantics](#8-undo--redo--fork-semantics)
9. [Path-scoped context injection](#9-path-scoped-context-injection)
10. [Response chaining across branches](#10-response-chaining-across-branches)
11. [Concurrency](#11-concurrency)
12. [Client & UX](#12-client--ux)
13. [Reclamation (abandoned branches)](#13-reclamation-abandoned-branches)
14. [Side effects (what undo does not undo)](#14-side-effects-what-undo-does-not-undo)
15. [Work breakdown](#15-work-breakdown)
16. [Phased execution](#16-phased-execution)
17. [Testing strategy](#17-testing-strategy)
18. [Behavior-parity checklist](#18-behavior-parity-checklist)
19. [Risks & mitigations](#19-risks--mitigations)
20. [Out of scope / future work](#20-out-of-scope--future-work)
21. [Open questions](#21-open-questions)
22. [Verification / definition of done](#22-verification--definition-of-done)
23. [Appendix: diff against append-only-transcript.md](#23-appendix-diff-against-append-only-transcriptmd)

---

## 1. Relationship to append-only-transcript.md

This is a **companion**, not a replacement. It assumes that plan's phases land
(draft-in-flight, immutable entries, path-derived rendering, `ContextEntry`,
frozen prefix inputs) and amends four of its decisions:

| append-only plan | this plan |
|---|---|
| §2: append-only **+ `truncate_to`** exception | append-only **+ a cursor**; the `truncate_to` exception is **removed** (strictly stronger) |
| §3.6: `undo → truncate_to`; popped suffix → a single `redo: Vec`; new input **clears** `redo` | `undo → current_seq = parent`; successors are the cursor's **children**; new input **adds a sibling** and clears nothing |
| Decision H: session trees **deferred** | session trees **specified here** (data model folded into the same schema/protocol bump) |
| §7.1: `TranscriptTruncated { through_seq }` | `TranscriptNavigated { current_seq }` (seqs are global, so "drop `seq > through_seq`" computes the wrong set once a sibling branch exists) |

Everything else (the static prefix, the ephemeral overlay, `ContextEntry`
variants, `InjectionState`, image handling, the migration framework, the
frozen tool superset) is **unchanged** except where a later section says so.

> **Version numbers.** `append-only-transcript.md` targets `SCHEMA_VERSION 2 → 3`
> and `PROTOCOL_VERSION 5 → 6`; at HEAD the schema is still 2 but the protocol is
> already **9**, so those constants in the companion are stale. Read them as "the
> next bump, whenever it lands". This document adds **no** further bump: the tree
> shape rides the same 2 → 3 shape migration and the same protocol cutover.

---

## 2. The insight: undo already forks

Under the append-only model, "undo, then type a new message" is:

```
truncate_to(P)        # model view becomes  prefix(P)
append(new turn Y)    # model view becomes  prefix(P) + Y
```

That is **exactly a fork at P**: the truncated suffix (branch *X*) and *Y* are
siblings under P. The append-only plan computes the fork and then throws *X*
away (keeping at most one `redo` slot, cleared on the next input). A **session
tree** is the same computation minus the discard. undo-tree's whole point is
that you should never discard: keep the branch, and let the user navigate to it.

So the fork primitive is not new work. It is `truncate`-then-`append`, which the
companion plan already builds. This document changes one policy — **retain
instead of discard** — and adds the navigation UI the retained branches make
possible.

---

## 3. The invariant, restated

> **The transcript of a session is strictly append-only.** The codebase provides
> no operation that alters or removes a committed entry. The only mutable
> per-session state is a single cursor, `current_seq`, which selects the path
> **root → `current_seq`** that is rendered to the model and to clients.

Corollaries, replacing the companion's §2 corollaries:

- There is **no `truncate_to`**. The companion's sole sanctioned removal is
  **deleted**; undo is a cursor move, which mutates nothing durable.
- The **`system` head** is frozen (unchanged from the companion).
- The **ephemeral overlay and epilogue** remain the only non-persisted,
  after-the-prefix content (unchanged).
- **Navigation** (`undo`, `redo`, jump-to-node, switch-branch) is a cursor
  write. It never adds, edits, or removes an entry.
- The only durable *removals* in the whole system are **out-of-band
  reclamation** (§13) of entries not reachable from the cursor, and full
  session deletion — neither of which can ever touch the current path's prefix.

This is a **strengthening**: the companion needs an exception to its own
invariant; this plan needs none.

---

## 4. Cache safety (why branching costs nothing new)

Provider prefix caching keys on the **longest unchanged token prefix** of the
request. The companion's justification for `truncate_to` was: "the retained
prefix is a *prefix* of what the provider already cached, so undo does not bust
the cache." The identical argument holds — and is easier to state — for a
cursor, because the *rendered bytes* are the same:

| Step | Companion (truncate) | This plan (cursor) | Provider sees |
|---|---|---|---|
| start | `P, X` | path `P, X` | `P, X` |
| undo | `truncate_to(P)` → `P` | `current = P` → path `P` | `P` |
| new input | append `Y` → `P, Y` | append `Y` → path `P, Y` | `P, Y` |

The requests are **byte-identical**. The provider cache hits up to P and misses
on Y in both cases; the cost is the same. A branch switch is likewise a
`cursor → fork point` followed by rendering that branch's stored entries — each
of which was cached when it was originally appended. The only unavoidable cost
is the **first fill of a branch that has never been in flight**, which is the
cost of any new content and is bounded by the branch's length (a *partial miss
from the fork point*, never a full bust from position 0).

This is categorically different from the failure the companion guards against
(ironclaw's volatile system-head content → 82% → 29% collapse, a **full bust**).
**Per-branch append-only** is the precise statement: on a fixed branch with no
edit, request *N+1* is a verbatim item-extension of request *N*. Switching
branches is a prefix change *at the fork point only*, and the retained-prefix
property carries the cache.

---

## 5. Target architecture

Layers, extending the companion's §3.1:

```
SessionState
├── static_prefix_inputs : Option<PrefixInputs>   // frozen at first render (B′)
├── transcript           : Transcript             // append-only TREE, persisted
├── current_seq          : Option<u32>            // THE ONLY MUTABLE STATE
├── draft                : Option<Draft>          // in-flight turn, NOT in the log
├── injection            : InjectionView          // DERIVED from the current path (§9)
└── runtime…              // sockets, provider client, registries, discovery
```

`render_request` renders the **current path**, not the whole log:

```
render_request(session, provider_slug, model) -> WireRequest
  = render_prefix(session.static_prefix_inputs)
  + render_path(session.transcript, session.current_seq)   // §3.3 of the companion, path-scoped
  + render_epilogue(session)                               // unchanged
```

`render_path` walks `parent_seq` links from `current_seq` to the root, reverses,
then renders each entry exactly as the companion's `render_transcript` renders a
linear log. On a fresh linear session (no branches ever created) the path is the
whole log, so **behavior is identical to the companion** until a fork happens.

`Transcript` API (replacing the companion's §2.1):

```rust
Transcript::append(entry)          // sets entry.parent_seq = caller's base; advances nothing
Transcript::set_cursor(seq)        // the only mutation of navigation state
Transcript::path_to(seq)           // read-only: root..=seq
Transcript::children(seq)          // read-only: direct successors
Transcript::iter()                 // read-only: all entries
```

There is no `truncate_to`, no `set`/`insert_at`/`&mut`, no `redo` vec. A
`#[cfg(test)]` guard asserts the `Transcript` exposes no mutation beyond
`append` + `set_cursor`.

---

## 6. Data model & persistence

### 6.1 `TranscriptEntry` gains one field

```rust
pub enum TranscriptEntry {
    Turn(Turn),
    Context(ContextEntry),
}

pub struct TranscriptNode {          // the stored wrapper (companion: bare enum)
    pub parent_seq: Option<u32>,     // NEW: None only for the root entry
    pub entry: TranscriptEntry,
}
```

`parent_seq` is the only structural addition. In a never-forked session it is
trivially `previous_seq` — redundant but free, and it is what makes a fork a
one-line policy change rather than a re-migration. **Fold this into
`append-only-transcript.md`'s Phase 0** (the type is defined there anyway) so
the shape migration carries it from the start.

### 6.2 Table shape is unchanged

`session_transcript`: `(u64, u32) → zstd(msgpack(TranscriptNode))`. Still keyed
`(session_id, seq)`; a tree is just nodes with parent pointers. `seq` remains
one **global, monotonic append counter** (append order), *not* a path position —
the path is derived from `parent_seq` links. No key change, no data move; only
the **value type** changes, which is the codec change that already owns the
companion's `2 → 3` bump.

### 6.3 `SessionState` / `SessionRecord`

- Replace the companion's `redo: Vec<TranscriptEntry>` with `current_seq: Option<u32>`.
- Remove the companion's `truncate_to`-based undo machinery and `last_undo_turn_ids`
  (already removed by the companion; confirm nothing re-introduces it).
- Move `last_response_id` / `last_response_id_producer` **off the session** and
  **onto the entry** that produced them (§10). `SessionRecord` keeps the fields as
  a compat shim until the migration drops them (or keeps a denormalized copy of
  the *current path's* tip id for cheap reads — decide at implementation; the
  entry is the source of truth).

### 6.4 Migration (folded)

Because §6.1 lands in the companion's Phase 0, the companion's `2 → 3` migration
is unchanged: it re-encodes each old `Turn` as `TranscriptNode { parent_seq: <prev seq>, entry: Turn(..) }`. If trees land **after** the companion has
shipped (`SCHEMA_VERSION 3`), add a `3 → 4` migration that (a) wraps each row
with `parent_seq = seq − 1` (or `None` for seq 0 — linear chains are contiguous),
(b) sets `current_seq = max(seq)`, (c) strips the session-level response id if
entries carry it. Idempotent, single write txn, per the `Migration` contract.

---

## 7. Wire protocol

Rides the companion's `PROTOCOL_VERSION` cutover. Changes relative to the
companion's §7.1:

**Events.**
- **keep** `TranscriptAppended { seq, entry }` — now `entry` is
  `TranscriptNode` (carries `parent_seq`). This is the only way branches grow.
- **replace** `TranscriptTruncated { through_seq }` with
  **`TranscriptNavigated { current_seq: Option<u32> }`**.
  *Why:* seqs are global append-order, so after a fork a sibling branch's entries
  have *higher* seqs than the truncated tail's — "drop `seq > through_seq`"
  computes the wrong visible set. The client must reconstruct the path, which it
  can, since it holds every node's `parent_seq`.
- **`SessionState`** carries the **full node set** + `current_seq` + `draft`.

**Client → server.** Keep `Undo` / `Redo` (relative cursor moves). Add:
- `NavigateTranscript { seq: u32 }` — absolute jump (visualizer click, `/fork <n>`).

Redo at a fork is resolved deterministically to the **most-recently-active
child** (§8); choosing among children uses `NavigateTranscript` from the
visualizer. A separate `SwitchBranch` variant is therefore unnecessary.

---

## 8. Undo / redo / fork semantics

Definitions (undo-tree, adapted):

- **`undo`** → `set_cursor(parent(current_seq))`. If the cursor is the root, no-op.
- **`redo`** → if the cursor has children, `set_cursor(<most-recently-active child>)`;
  otherwise no-op. On a linear tail this is the classic single-child redo; at a
  fork it descends the branch you were last on.
- **fork** → append a new entry whose `parent_seq` is a **non-tip node** (or any
  node that already has a child). The append itself is an ordinary
  `TranscriptAppended`; the parent node simply gains a second child.
- **switch branch** → `NavigateTranscript` to a node on a sibling subtree.

**Fork triggers** (all the same underlying append-from-a-node):
- undo, then send a message (the classic case);
- `/fork <seq>` = `NavigateTranscript(seq)`, then compose;
- `/regenerate` = navigate to the parent of the last assistant turn on the path
  and re-run, producing a **sibling assistant node** (same user message, second
  answer — the most common LLM-chat fork);
- editing/retrying a prompt.

**What is deleted from the companion's behavior:**
- **No redo invalidation.** The companion's §3.6 "new user input after an undo
  clears `redo`" is **removed**. New input adds a sibling; every prior branch
  stays reachable. This is a deliberate, user-visible behavior change (release
  note).
- **No `undone` flag, no `TurnsUndone`/`TurnsRedone`, no `last_undo_turn_ids`** —
  as the companion already specifies; this plan only changes what replaces them.

---

## 9. Path-scoped context injection

The companion's `InjectionState` (§3.4) is a **persisted mutable set** of "already
injected" sources, tool groups, and fingerprints. That is wrong under a tree:
"AGENTS.md already injected" is only meaningful **relative to a path**. A sibling
branch that predates an injection must not have it, and a branch that crossed a
project-file edit must see the edit's `InstructionsUpdated`, not the original.

**Resolution: derive it.** Replace the persisted mutable `InjectionState` with an
`InjectionView` computed by folding the current path's entries:

- sources/tool-groups/fingerprints = the fold of every `ContextEntry` on the path;
- the "should I inject?" decision at a turn boundary compares against that fold.

Consequences:

- Every branch is self-consistent by construction; no cross-branch leakage.
- A persisted mutable field is **removed** (aligns with the companion's
  "no mutable slot" ethos), and replay on resume is automatic.
- The fold walks only the current path (bounded by path length), computed at the
  same turn boundaries the companion already uses.

`SessionRecord.injection_state` (companion §5.3) is dropped; nothing replaces it.

---

## 10. Response chaining across branches

`last_response_id` (OpenAI/xAI Responses) is a **linear, server-side** artifact.
The companion handles undo by *clearing* the id (and the producer). Under a tree
that is too blunt: switching back to a branch should restore *that branch's*
chain, and a fresh branch should start a new chain.

**Resolution: move the id onto the entry that produced it.**

```rust
Turn {
    // …existing…
    response_id: Option<String>,             // NEW: the provider response this turn produced
    response_producer: Option<ReasoningProducer>,  // NEW: provenance (same-model gate)
}
```

The request builder derives the chain id for the current request by walking the
path to the deepest entry that carries a `response_id` whose
`response_producer` matches the active provider+model. This makes:

- branch switch → automatic restore of that branch's chain (no explicit
  save/restore logic);
- fresh branch → no id on the new path → non-chained request (always safe);
- the companion's "undo clears `last_response_id`" → **subsumed**: an undo to a
  node whose path carries no matching id is simply a non-chained request.

`SessionRecord.last_response_id*` becomes a denormalized cache of the current
path's tip id (or is dropped). The entry is the source of truth.

---

## 11. Concurrency

The companion gates undo/redo on `refuse_unless_idle` because a worker owns a
private clone of the turn history and merges it at `RequestFinished`, so an
undone-flag flip races the merge (`choreo-daemon/src/sessions/handlers/undo_redo.rs`).
Two facts change:

1. **Navigation is a cursor write, not a history mutation.** It cannot race a
   merge because it never rewrites or resurrects turns.
2. **Appends are unconditional and parented to a base seq.** A request records
   `base_seq` (the tip it was launched from) and commits its draft as children of
   that base at iteration end. Commits from a worker whose base is no longer the
   tip simply attach to *their own* branch — correct, not a conflict. At most one
   active request per session, and the session command loop is the single
   mutator, so appends are serialized.

**What remains:** navigation and fork must still be refused while a **draft is in
flight**, because the draft's parent is the then-current tip and moving the cursor
would orphan it. So `refuse_unless_idle` (or its successor) **stays**, but its
rationale is rewritten from "the worker's clone would race the merge" to "a live
draft has no stable parent". The merge race itself disappears — that is the
parity item to retire.

---

## 12. Client & UX

**`choreo-client-core`.** `SessionView` holds the **node set** (ordered, with
`parent_seq`) + `current_seq` + `draft`. `TurnEventHandler` collapses to
`handle_transcript_appended` / `handle_transcript_navigated`. A `path()` helper
returns the ordered visible entries (root→cursor); rendering, token estimate, and
`session_inspect` all read the path, never the whole node set.

**`choreo-tui`.**
- The companion's `turns_undone_prunes_*` behaviors (`state/tests/undo_pruning.rs`)
  are replaced by: on `TranscriptNavigated`, re-render the path. Retained sibling
  entries are **not** dropped from the client's node set.
- `Alt+Up` / `Alt+Down` remain undo/redo (`state/keymap.rs`). At a **fork point**
  they navigate the current branch; branch selection uses the visualizer.
- **`/tree`** — an undo-tree-style ASCII visualizer: `o` nodes, `|`/`/`/`\`
  edges, the current node highlighted, the current path/branch highlighted;
  timestamps and a diff/effects toggle (see §14); click or arrow-select a node and
  Enter → `NavigateTranscript`.
- **`/fork [seq]`** — navigate to `seq` (default: current tip) to compose a new
  branch. **`/regenerate`** — sibling assistant node (§8).
- `Context` entries continue to render as collapsed rows (companion §7.2).

**`choreo-gui`, `choreo-im`, `choreo-acp`.** Adapt to the two events; render only
the path; ignore `Context` entries for v1 (as the companion specifies). No
visualizer in v1.

---

## 13. Reclamation (abandoned branches)

A tree can grow unboundedly. undo-tree bounds it via `undo-limit`; here, the
cost is **entries + attachments** on branches the user abandoned.

Policy (default; tunable):

- An entry is **live** if it is reachable from `current_seq` (the current path),
  or within a **retention window** (e.g. touched within N days), or on a branch
  the user marked kept.
- Reclamation deletes only **non-live** entries and their `session_attachments`
  rows. It **never** touches the current path, so the append-only invariant for
  the *rendered* transcript holds.
- It is **out-of-band** (a background sweep or a `/prune` command), never a
  transcript mutation, so it does not reopen the invariant's exception.
- Attachments dominate storage (companion §12), so the sweep is attachment-first.

This is the one place a durable delete of transcript entries happens; it is
gated on "not reachable from the cursor" and is documented as such in
`ARCHITECTURE.md`.

---

## 14. Side effects (what undo does not undo)

The companion (and undo-tree) are about **text/context** reversibility. An agent
turn can run shell commands, write files, `git commit`, hit the network. **A
cursor move cannot un-run a command.** Undo here rewinds the *model's context*,
not the *world*; a branch switch does not revert the repository to the state a
branch was last on.

The tree makes this **more** pressing (you can now hop between branches that each
mutated the repo), so the visualizer's diff toggle should show **effects**, not
just text: per-node side-effect markers (git SHAs touched, files written,
commands run), so a branch node reads "what this branch did to the world." This
is stronger than undo-tree's text diff and is the honest presentation of a
conversation tree over a stateful environment. The TUI must state plainly that
undo/fork rewind context, not the filesystem. (Recording effect markers is a
client/daemon feature to be scoped; a minimal version tags a `Turn` with the
`git_*`/`write_file` tool names it invoked.)

---

## 15. Work breakdown

Most of this is a **reshaping** of the companion's items, not net-new modules.

- **`choreo-proto`:** `TranscriptNode { parent_seq, entry }`; drop
  `TranscriptTruncated`, add `TranscriptNavigated`; add
  `ClientMessage::NavigateTranscript`; `SessionState` carries the node set +
  `current_seq`; `size.rs` arm for `TranscriptNode`; keep the companion's
  `SessionMessage`/`SessionMessageKind` doc cleanup.
- **`choreo-daemon/db`:** `session_transcript` value becomes `TranscriptNode`;
  `read_transcript` returns the node set; `write_entry` sets `parent_seq`;
  delete/purge cover the tree; the companion's `2 → 3` migration emits
  `parent_seq` (folded) — or a `3 → 4` wrap (standalone).
- **`choreo-daemon/sessions`:** `Transcript` with `set_cursor`/`path_to`/`children`;
  `current_seq` on `SessionState`; remove the `redo` vec and `truncate_to`;
  `handle_undo`/`handle_redo` become cursor moves; add node-children/`/fork`/`/regenerate`
  handlers; render via `render_path`; `refuse_unless_idle` rationale rewritten.
- **`choreo-daemon/requests`:** build via `render_path`; derive the chain id from
  the path (§10); `InjectionView` fold (§9) replacing `InjectionState`;
  per-entry `response_id`/producer.
- **`choreo-daemon/tools/session_inspect`:** render the path.
- **Clients:** `choreo-client-core` path/events; `choreo-tui` visualizer + `/tree`
  + `/fork` + `/regenerate`; gui/im/acp adapt.

---

## 16. Phased execution

The companion's phases are the backbone; these slot in **without adding a
phase before the migration**, so tree shaping rides the same bump.

- **Companion Phase 0 (+ this plan):** define `TranscriptNode` with `parent_seq`
  and `Transcript` with `set_cursor` from day one; `render_path` as a shim that
  reproduces today's linear rendering. *This is the cheap insurance: the data
  model is tree-shaped before anything ships.*
- **Companion Phase 5 (reshaped):** undo/redo as **cursor moves** (not
  `truncate_to`); keep `refuse_unless_idle` for the draft reason; remove
  `undone`/`last_undo_turn_ids`/redo-vec. No redo-invalidation.
- **New Phase 5.5 — Navigation surface:** `/fork`, `/regenerate`,
  `NavigateTranscript`, `TranscriptNavigated`; `choreo-client-core` path rendering;
  TUI branch-aware undo/redo. (Behavior-complete for branching; no visualizer yet.)
- **Companion Phase 7 (reshaped):** schema bump carries `parent_seq` +
  `current_seq`; migration as in §6.4.
- **Companion Phase 8 (reshaped):** protocol cutover carries
  `TranscriptNavigated` + `NavigateTranscript`.
- **New Phase 8.5 — Visualizer + context injection + chaining:** `/tree`
  visualizer, `InjectionView` fold, per-entry response id.
- **New Phase 8.6 — Reclamation:** background sweep / `/prune` (§13).
- **Companion Phase 9:** docs — update `ARCHITECTURE.md`'s undo/redo section to
  the cursor/tree model and this plan's invariant; mark both plans' status.

Dependencies: companion 0 → (1,2) → (3,4,5+5.5) → 6 → 7 → 8 (+8.5, 8.6) → 9.

---

## 17. Testing strategy

In addition to the companion's suite:

- **Path rendering:** a scripted fork renders `P, Y` after the fork and `P, X`
  after switching back — byte-identical to the pre-fork request on each branch.
- **Cursor is not a mutation:** a `Transcript` guard test asserts no API beyond
  `append`/`set_cursor` (and that `set_cursor` writes only `current_seq`).
- **Prefix-invariance per branch (goose-style):** two consecutive requests on one
  branch are a verbatim item-extension; switching branches changes bytes only at
  and after the fork point.
- **Branch retention:** undo + new input leaves the prior branch's entries
  present and reachable via `NavigateTranscript`.
- **Chain per branch:** a chained model restores the correct
  `previous_response_id` on branch switch, and sends non-chained on a fresh
  branch.
- **InjectionView:** a sibling branch predating an AGENTS.md edit does not see the
  edit; a branch crossing it does; no cross-branch leakage.
- **Navigation vs. draft:** navigation is refused while a draft is in flight;
  permitted on idle.
- **Event correctness:** with a sibling branch present, `TranscriptNavigated`
  yields the correct visible set (the case `TranscriptTruncated` got wrong).
- **Migration:** `parent_seq` populated correctly for a linear v-old DB (chain is
  contiguous); round-trip.
- **Reclamation:** the sweep deletes only non-live entries and never the path;
  attachments cleaned; a kept branch survives.

---

## 18. Behavior-parity checklist

Carried from the companion, with these diffs:

- **Retire** the "undo while a request worker is in flight" clone-merge race —
  replaced by draft-parenting (§11). Re-express the parity item as "navigation
  refused while a draft is live".
- **Retire** "undo clears `last_response_id`" as an *explicit* step — subsumed by
  path-derived chain ids (§10). Keep the provenance gate.
- **Change (deliberate):** redo is **no longer invalidated** by new input. Release
  note it.
- Unchanged: prefix determinism, image decay, token accounting/`last_prompt_tokens`,
  context-window display, OSC progress, `session_inspect` dry-run fidelity,
  streaming-draft visibility on attach, "`get_session`/`list_sessions` do not leak
  context bodies."

---

## 19. Risks & mitigations

- **Cache regression from branch thrash** (highest product risk): if a user
  alternates branches often, the provider re-fills at each fork point. Mitigation:
  per-branch append-only is the common path; an untouched branch keeps its cache;
  document the cost. Not a full bust (a partial miss from the fork point) — the
  category the companion already accepts.
- **Tree size / storage** (highest ops risk): mitigated by §13 reclamation and the
  attachment-first sweep.
- **Client state divergence**: the client must reconstruct paths from
  `parent_seq`; a single `path()` helper shared by render/token-estimate/inspect
  prevents drift (same pattern as the TUI's `chat_page_layout`).
- **Migration correctness** if landed post-companion: the linear chain is
  contiguous, so `parent_seq = seq − 1` is exact; test against a real DB.
- **Scope creep into subsessions**: keep trees (history-sharing) and subsessions
  (delegation, fresh log) **distinct** — see §20.

---

## 20. Out of scope / future work

- **Subsessions remain fresh, independent sessions** (companion Decision H
  unchanged for *delegation*). A subsession is a parallel agent, not a
  conversation branch; keep the two semantically separate so "fork" never silently
  means "spawn a child agent."
- **Cross-session COW trees** — a child session whose effective transcript is a
  *reference* to a parent's prefix (`(parent_session_id, fork_point_seq)`). This is
  the "persistent forest" unification of trees and subsessions; deferred. Note the
  free benefit it would give (a subsession's prefix is often byte-identical to the
  parent's, giving cross-session prefix reuse — the companion already anticipates
  this in Decision H).
- **Compaction** (a deliberate reset), as in the companion.
- **GUI/IM `Context` rendering** and their visualizers.
- **Effect-marker recording** beyond the minimal turn tagging in §14.

---

## 21. Open questions

*None blocking*; settle at implementation:

- **Fork granularity**: a *turn* (agent-loop iteration) or a *user round*
  (user message + its assistant/tool turns)? undo-tree nodes are single changes;
  the natural fork boundary for chat is the user round, which matches
  `undo_turns()`'s existing `user_text: Some(..)` scan. A "regenerate" forks at the
  user node; a "fork here" on the visualizer should target a round boundary.
- **Redo's default child** at a fork: most-recently-active (proposed) vs.
  most-recently-created vs. always open the chooser.
- **Retention policy defaults** for §13 (window length, per-session node budget).
- **`SessionRecord.last_response_id`**: denormalized cache vs. dropped entirely.
- **Visualizer placement** in the TUI (modal overlay vs. a page) and its key
  bindings alongside the readline/`Alt+` map.

---

## 22. Verification / definition of done

- The append-only invariant holds **with no exception**: the `Transcript` exposes
  only `append`/`set_cursor`; no durable edit/truncate on any code path.
- A scripted fork → switch → switch-back renders each branch byte-identically to
  its original request; the per-branch prefix-invariance test passes across
  protocols.
- A two-request session on one branch shows a cache-hit ratio
  (`cached_tokens`) no worse than the companion's baseline.
- The `SCHEMA_VERSION`/`PROTOCOL_VERSION` bumps (shared with the companion)
  pass migration against a real DB and all clients build; full `just pre-commit`
  green.
- `ARCHITECTURE.md` undo/redo section rewritten to the cursor/tree model;
  `README.md` updated; both plans' status lines updated.

---

## 23. Appendix: diff against append-only-transcript.md

**Decisions changed**

| # | Companion | This plan |
|---|---|---|
| §2 | append-only **+ `truncate_to`** | append-only **+ cursor** (strictly stronger) |
| §3.6 | `undo → truncate_to`; `redo: Vec`; new input clears redo | `undo → cursor = parent`; successors = children; nothing cleared |
| D (H) | session trees deferred | specified (same schema/protocol bump) |
| §7.1 | `TranscriptTruncated` | `TranscriptNavigated` |
| §3.4 | persisted `InjectionState` | derived `InjectionView` |
| §3.7/§11 | undo clears `last_response_id` | per-entry id; path-derived (§10) |
| §11 | undo-vs-worker merge race | draft-parenting; race retired (§11) |

**Glossary additions** (extend the companion's §16.3)

- **Node** — a `TranscriptNode` entry (a `Turn` or `Context`) carrying a
  `parent_seq`.
- **Path** — the entries on root → `current_seq`; exactly what is rendered to the
  model and clients. The tree's *visible projection*.
- **Cursor** — `current_seq`; the single mutable navigation state.
- **Fork** — a node with more than one child; created by appending from a non-tip
  node. The surviving form of the companion's discarded truncation.

**Non-goals of this document**

- It does not restate the companion's contracts; it amends them.
- It does not specify final Rust signatures (they will drift); it specifies the
  **cursor-as-mutation** invariant, the tree shape, and the navigation semantics.
