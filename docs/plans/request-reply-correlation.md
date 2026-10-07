# Plan: uniform request/reply correlation for the wire protocol

**Status:** proposed — design complete; nothing implemented (no source changes in this
change).
**Lifecycle:** this file is **deleted once the plan is implemented**. Nothing written
during implementation may reference it — rustdoc, `ARCHITECTURE.md`, `README.md`,
release notes, and commit messages must stand on their own, because a reference to this
plan would go stale the moment it is removed.
**Date:** 2026-10-07
**Targets:** `choreo-proto` (reshape both message envelopes; rename `request_id` →
`stream_id`), `choreo-daemon` (reply plumbing, the `ReplyHandle` guard, session-thread
acks, daemon-assigned `stream_id`), `choreo-client-core` (the shared `PendingReplies`
table + connection seam), `choreo-tui`, `choreo-gui` (adopt the table, delete the
shape/UI-state matching), `choreo-im`, `choreo-acp` (id allocation adapters), docs
(`ARCHITECTURE.md`, `README.md`).
**Touches:** `choreo-proto/src/{types,lib,size,tests}.rs`;
`choreo-daemon/src/{server/connection.rs,broadcast.rs,sessions.rs,sessions/*,daemon.rs,daemon/*,embedded.rs,requests/*}`;
`choreo-client-core/src/{connection.rs,dispatch.rs,credentials.rs,lib.rs}`;
`choreo-tui/src/{connection/mod.rs,connection/daemon.rs,connection/chat.rs,state/mod.rs,state/*}`;
`choreo-gui/src/client.rs`; `choreo-im/src/bridge.rs`; `choreo-acp` (wherever it
constructs `ClientMessage`); the shared integration harness under each crate's
`tests/it/`; `ARCHITECTURE.md`; `README.md` (`doc_crates` already covers the crates).

> **TL;DR.** Today only the *turn stream* is correlated (`request_id`, client-generated,
> `u32`, `CANCEL_ALL = 0`), and every other request/reply exchange is matched by
> **shape, arrival order, and client UI state** — e.g. `choreo-tui` decides what a
> `Models` reply means by checking whether the model selector is open, and
> `resolve_daemon_session(None)` guesses "the attached session" for a connection-level
> reply. That heuristic layer is where sessions "occasionally fall out of sync" with no
> reproducible cause. This plan makes correlation **structural and universal**: every
> `ClientMessage` is `{ id, inner }` with a mandatory per-connection id, every request
> gets **exactly one** `DaemonMessage { id: Some(..), .. }` reply, broadcasts are
> `{ id: None, .. }`, and the client owns a single pending-request table. The server owns
> no correlation state. The change is pre-release (the wire version is unreleased), so
> it is amended in place with no version bump and no back-compat shim. It also fixes a
> latent cross-client `request_id` collision by moving stream-id assignment to the
> daemon, renames that axis to `stream_id`, and collapses roughly a dozen pieces of
> bespoke matching plumbing into one mechanism.

---

## Table of contents

1. [Motivation & evidence](#1-motivation--evidence)
2. [The invariant (normative)](#2-the-invariant-normative)
3. [Wire protocol design](#3-wire-protocol-design)
4. [Reply/broadcast taxonomy](#4-replybroadcast-taxonomy)
5. [Overlap policy](#5-overlap-policy)
6. [Daemon mechanics](#6-daemon-mechanics)
7. [Client mechanics](#7-client-mechanics)
8. [The streaming axis](#8-the-streaming-axis)
9. [Subscriptions](#9-subscriptions)
10. [Per-crate work breakdown](#10-per-crate-work-breakdown)
11. [Deletions & simplifications enabled](#11-deletions--simplifications-enabled)
12. [Decisions log](#12-decisions-log)
13. [Phased execution plan](#13-phased-execution-plan)
14. [Testing strategy](#14-testing-strategy)
15. [Documentation](#15-documentation)
16. [Risks & mitigations](#16-risks--mitigations)
17. [Out of scope / future work](#17-out-of-scope--future-work)
18. [Open questions](#18-open-questions)
19. [Verification / definition of done](#19-verification--definition-of-done)
20. [Appendix: full variant classification](#20-appendix-full-variant-classification)

---

## 1. Motivation & evidence

### 1.1 The problem

The delivery layer is already lossless and ordered per connection
(`choreo-daemon/src/broadcast.rs`: unbounded per-subscriber queues, FIFO, lag-based
eviction). So the desync bugs are **not** packet loss — they are **attribution and
epoch** errors. Four evidence points from the tree:

- **Shape/UI-state matching.** `choreo-tui/src/connection/daemon.rs` decides what a
  `Models` reply means by testing `app.model_selector.is_open()`; it interleaves
  "selector closed: fall through" logic with reply handling. `ListSessions`/`Sessions`
  and `ModelsRefreshed` are likewise matched by variant shape and arrival order.
- **`resolve_daemon_session(None)` guessing.** `choreo-tui/src/state/mod.rs` maps a
  `session_id: None` reply to "whatever is attached *now*", which can differ from what
  was attached when the request was sent. Six `SessionEvent`s are None-capable and
  dispatched through this guess.
- **Silent success / silent drop.** `SetSessionPinned`/`SetSessionArchived` have "no
  targeted success reply — the broadcast is the success signal" (`types.rs`), an empty
  `/undo` sends nothing at all, and `dispatch_client_message` ends in
  `_ => warn!("unhandled client message")` — which is exactly why `GetSessionState`
  (defined at `types.rs:563`, sent by `choreo-client-core/src/shell.rs`) has been dead
  on the wire with no error.
- **Cross-client `request_id` collision.** `request_id` is client-generated
  (`choreo-tui/.../chat.rs`, `wrapping_add`) and fanned to *all* session subscribers, so
  two clients attached to one session can both claim stream 5, and one client's
  `Done{5}` clears the other's `active` entry. Masked today only because the TUI is
  usually the sole attached client.

The codebase has already been moving toward targeted replies by hand, one case at a
time: v5 split the keystore broadcast (`DaemonMessage::Keystore`) from the operation
replies (`Unlocked`/`Locked`); v7 split `SessionCreated` (broadcast) from
`SessionCreatedForRequester` (direct reply); v9 added MCP trust/reload request/reply
pairs; the just-landed vision-image work added a hand-rolled composite echo key
(`Image { session_id, turn_id, key }`) documented as "so a client with several fetches
in flight can route the reply." Each is a fragment of the same missing mechanism.

### 1.2 The relief

One uniform frame both directions, one client-side table, one server-side guard removes
the heuristic layer entirely and makes every exchange self-identifying and
timeout-able — which is what turns "occasionally out of sync" into a logged, attributable
event.

---

## 2. The invariant (normative)

1. Every client→server message is `ClientMessage { id: u64, inner: ClientMessageType }`.
   `id` is a **per-connection** counter starting at **0**, incremented per send, **never
   reused** for the life of the connection. There is no sentinel and no uncorrelated
   send.
2. Every request receives **exactly one terminal** `DaemonMessage` with
   `id: Some(the same id)` — success or failure.
3. Every broadcast is `DaemonMessage { id: None, inner: DaemonMessageType }` and never
   resolves a request.
4. The **client** owns a pending-request table; the **daemon** owns **no** correlation
   state — it echoes the id and nothing more.
5. Two orthogonal axes: the **reply axis** (`id`, per-connection, one-shot) and the
   **stream axis** (`stream_id`, per-session, many events). They are never merged.
6. Every request is tagged with a `MessageKind`, surfaced in `Accepted`/`Failed` and in
   client logs, so every exchange self-identifies.

---

## 3. Wire protocol design

### 3.1 Client message

```rust
/// Every client→server message. `id` is a per-connection request id, starting
/// at 0, incremented per send, never reused for the connection's life. The
/// daemon MUST answer with exactly one `DaemonMessage { id: Some(self.id), .. }`.
pub struct ClientMessage {
    pub id: u64,
    pub inner: ClientMessageType,
}

/// The request payloads. Bodies are exactly the current `ClientMessage`
/// variants — unchanged.
pub enum ClientMessageType { /* ~40 variants, §20.1 */ }
```

Rejected alternatives:

- **Self-containing `ClientMessage { id, inner: ClientMessage }` (the earlier sketch)**:
  recursive, needs a `Box`, needs a "no nested Request" runtime guard. The dedicated
  `*Type` enum removes the smell by construction. **This is the design.**
- **An optional `id: Option<u64>` with 0/`None` = fire-and-forget**: rejected — the
  whole point is that no request is fire-and-forget.

### 3.2 Daemon message

```rust
/// Every daemon→client message. `Some(id)` answers the request with that id;
/// `None` is a broadcast.
pub struct DaemonMessage {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<u64>,
    pub inner: DaemonMessageType,
}
```

- **`Option<u64>`, not `id == 0`-means-broadcast.** The sentinel is unworkable: request
  ids start at 0, so the reply to the first request would also be `0` and collide with
  "broadcast." `Option` is self-documenting, serde-compact (absent field), and makes a
  stray `0` harmless.
- **The daemon stays stateless about ids** — it copies `id` onto the reply.
- Constructors `DaemonMessage::reply(id, inner)` / `DaemonMessage::broadcast(inner)`
  make the correct shape the easy path.

### 3.3 Serde & framing

- Named MessagePack (the workspace wire format, see `frame.rs`). `skip_serializing_if`
  keeps broadcasts compact; `default` decodes a missing field tolerantly.
- `read_message`/`write_message` (`choreo-proto/src/io.rs`) are generic and need no
  change; the embedded transport (`choreo-daemon/src/embedded.rs`) forwards values, so
  it only picks up the new struct shape.
- **No version bump.** The wire version (`PROTOCOL_VERSION = 9`) is unreleased — the
  last release (v0.2.3) shipped v8 — so the frame is amended in place per the documented
  pre-release policy (`frame.rs`). This plan does not bump; if a release happens first,
  bump then.

### 3.4 Size gauge

`choreo-proto/src/size.rs` (`approx_wire_size`) must add the `id` width (8 bytes) to
both types and the `Option` tag on the daemon side. This feeds lag accounting, so it must
never under-count; extend the `approx_wire_size_never_underestimates_encoded_payload`
pin test with an `Accepted`/`Failed` and an id-bearing reply/broadcast sample.

---

## 4. Reply/broadcast taxonomy

Every current variant is classified reply-only, broadcast-only, or dual. This is the
specification the implementation follows (full table in §20). Summary:

- **Client requests: all 40.** Each becomes a correlated request with a terminal reply.
  Requests whose success is *currently* silent (all of §20.2 class B) gain
  `Accepted`/`Failed`.
- **Daemon replies (id `Some`)**: `Pong`, `Sessions`, `Models`/`ModelsFailed`,
  `Unlocked`/`Locked`/`Bound`/`LockedError`/`KeystoreUnbound`,
  `CredentialAdded`/`Failed`, `CredentialRemoved`/`Failed`, `Credential`,
  `AclAddResult`, `AccountAdded`/`Removed`/`Failed`, `Accounts`/`AccountListFailed`,
  `ModelsRefreshed`/`Failed`, `Image`, `McpStatus`/`McpReconnectFailed`,
  `McpReloaded`/`McpReloadFailed`, `McpTrustUpdated`, `McpTrustList`, plus the session
  replies (`SessionCreatedForRequester`, `SessionAttached`, `SessionState` when it
  answers an attach, `SessionDeleteFailed`, `ReasoningEffortSet` when it answers
  `GetReasoningEffort`) and the new `Accepted`/`Failed`.
- **Daemon broadcasts (id `None`)**: `Keystore { state }`, `CatalogUpdated`,
  `AclUpdated`, `ShuttingDown`, `Evicted`, and the session broadcasts
  (`SessionCreated` notification, `TurnAppended`, `SessionStatusChanged`,
  `SessionFlagsChanged`, `TurnsUndone`/`TurnsRedone`, `ModelSelected`,
  `SessionAccountSet`, `ContextWindowResolved`, `SessionWorkingDirSet`,
  `SessionTitleSet`, `ReasoningEffortSet` on a set, `TokenUsageUpdate`,
  `LiveOutputTokenCount`, and the whole stream: `Started`, `ToolCall*`,
  `ToolResultChunk`, `OutputChunk`, `Done`, `Failed`, `Cancelled`).

---

## 5. Overlap policy

Several payload types are currently used **both** as a reply and as a broadcast:
`SessionState`, `ReasoningEffortSet`, `ModelSelected`, `ModelSelectionFailed`,
`Done`/`Failed`. The invariant resolves this cleanly:

> **Reply-ness is a property of the send, not the payload type.** The same `inner`
> (`SessionState`, `ReasoningEffortSet`) may be emitted with `id: Some` (reply → resolves
> the requester's slot) or `id: None` (broadcast → does not). The client always applies
> the payload's state effect, and *additionally* resolves a pending slot iff `id` is
> `Some`.

Split a variant into two types **only** when the reply carries *requester-relative
intent* that no broadcast may carry — the `SessionCreatedForRequester` precedent (a
frontend may move its own view). `SessionState` has an identical payload for both
audiences, so it stays one type distinguished by `id`.

Enforcement: a dispatch `debug_assert!` that reply-shaped variants carry `id: Some`, and
a client-side `warn!` when a `Some(id)` matches no pending slot (turning a mislabeled
send into a named log line).

---

## 6. Daemon mechanics

### 6.1 The `ReplyHandle` guard (exactly-once reply)

`choreo-daemon/src/server/connection.rs` mints one handle per request and hands it to
whatever produces the reply. `send` consumes it; `Drop` `debug_assert!`s it was used —
so "exactly one reply" is enforced, not commented.

```rust
/// Owns the obligation to answer one request. `send` consumes it; dropping an
/// unused handle trips the debug guard, so a handler cannot silently forget.
struct ReplyHandle {
    id: u64,
    sink: SubscriberSink,          // the acting client's delivery sink
    global: Arc<AtomicUsize>,      // lag accounting (see broadcast.rs)
    sent: bool,
}
impl ReplyHandle {
    fn send(mut self, inner: DaemonMessageType) {
        self.sent = true;
        self.sink.send_unchecked(&DaemonMessage::reply(self.id, inner), &self.global);
    }
}
impl Drop for ReplyHandle {
    fn drop(&mut self) {
        debug_assert!(self.sent, "request {} left unacknowledged", self.id);
    }
}
```

### 6.2 Connection thread (synchronous replies)

Every handler that already computes a value synchronously (`Ping`, `ListSessions`,
`ListModels`, `RefreshModels`, `ListAccounts`, `GetCredential`, `AclAdd`,
`AddAccount`/`RemoveAccount`, `Mcp*`, `GetImage`, `GetSessionState`) replaces
`send_to_writer(ctx, reply)` with `handle.send(reply)`. Mechanical.

### 6.3 Threading the id to the session thread

Commands processed on a session thread (`CreateSession`, `AttachSession`, `RunInput`,
`ContinueGeneration`, `SetModel`, `SetReasoningEffort`, `SetSessionAccount`, `Undo`,
`Redo`, `SetTitle`, `SetWorkingDir`, `SetSessionPinned`, `SetSessionArchived`,
`DeleteSession`) must carry the reply obligation. Give the relevant
`SessionCommand`/`DaemonCommand` variants a `reply: Option<ReplyHandle>` (or a
`ReplyTarget { id, sink }`). The session thread calls `handle.send(..)` for the outcome.
The reference pattern already exists: `Unlock`/`SaveCredential` already pass
`client_writer: Some(ctx.writer.clone())`.

- **CreateSession/AttachSession**: already reply to the requester
  (`SessionCreatedForRequester`, `SessionAttached`); route the handle so those carry the
  id.
- **Class-B mutations**: on success, send `Accepted { kind }` to the requester *in
  addition to* the existing broadcast (which stays `id: None`); on failure, `Failed {
  kind, error }`. This closes the "success is silent" and empty-`/undo` gaps.
- **RunInput/ContinueGeneration**: the session thread sends the acceptance reply
  (`Started` or `Failed`) to the requester *in addition to* the stream broadcast.

### 6.4 Broadcasts & the writer thread

`broadcast()`, `fan_out_evicting`, the writer thread, and lag accounting are unchanged
except for the `{ id: None, .. }` wrapper. The writer serializes the new frame.

### 6.5 Ordering invariants

"`Bound` before the lock-state broadcast" and "`SessionAttached` before `SessionState`"
stay *true* but become **non-load-bearing** — the client routes by id regardless of
position. Keep the ordering; rewrite the comments from "this ordering is why X works" to
"routed by id regardless of position."

### 6.6 Daemon-assigned `stream_id`

The daemon allocates `stream_id` when it accepts a `RunInput`/`ContinueGeneration` and
returns it in the acceptance reply (`Started { stream_id, turn_id, .. }`). This removes
the client-side counter, the cross-client collision, and the `u32` wraparound. `Cancel {
stream_id }` cancels one stream; the `CANCEL_ALL` sentinel (cancel whatever is active on
the attached session) remains for the pre-`Started` window. `RunInput`'s payload loses
its `request_id` (now `stream_id`, daemon-owned). See §8.

---

## 7. Client mechanics

### 7.1 `PendingReplies` (shared in `choreo-client-core`)

Following the `KeystoreAutoBind` precedent (shared so the TUI and GUI cannot drift):

```rust
pub struct PendingReplies {
    next_id: u64,                       // starts at 0
    inflight: HashMap<u64, Pending>,
}
struct Pending {
    kind: MessageKind,
    sent_at: Instant,
    deadline: Instant,                  // sent_at + per-kind budget
    context: PendingContext,            // pending unlock key, image key, etc.
}
```

Methods (all synchronous, on the UI thread):

- `send(&mut self, tx, inner) -> u64` — allocate id, record `Pending`, wrap, send. **The
  one path every outbound message takes.**
- `resolve(id) -> Option<Pending>` — remove the slot; `warn!` on an unknown/duplicate/late
  id.
- `expire(now) -> Vec<Timeout>` — called from the UI loop tick (event-driven; no timer
  channel, per the workspace "no sleep-poll" rule).
- `clear()` — on connection reset (all in-flight void).

### 7.2 The connection seam

`choreo-client-core/src/connection.rs` stays transport-only (it already just pumps
`DaemonMessage`). The frontend owns the table and resolves in `handle_daemon_message`
**before** falling through to today's state handling. Correlation is a side table, so
**no state semantics move**.

### 7.3 Timeouts

Per-kind budgets (fast queries ~10 s; `RefreshModels`/`CreateSession` ~30 s; `RunInput`
acceptance ~10 s). On expiry: `warn!` with `id`+`kind`+elapsed, surface a status line,
optionally fire a resync (`ListSessions`/`GetSessionState`).

### 7.4 TUI ownership

`App` is owned solely by the UI thread (`run_ui_loop` takes `&mut app`); the connection
thread only delivers `UiEvent::Daemon` over an unbounded channel and every send goes
through `client_tx` from the UI thread. So the table is plain single-threaded state — no
locks, no cross-thread plumbing.

---

## 8. The streaming axis

`stream_id` (renamed from `request_id`) is a **separate system** and stays separate:

| | Reply axis | Stream axis |
|---|---|---|
| Cardinality | exactly one terminal message | many messages over the request's life |
| Audience | the requester only | **every** session subscriber (incl. mid-stream joiners) |
| Correlation | the envelope `id` | in-payload `stream_id` |
| Key space | **per-connection** | **per-session** (shared across clients) |

The per-connection vs per-session key space is *why they cannot merge*: two clients each
use their own message id 0, 1, … but a stream fanned to all subscribers needs an id
unique in a namespace all of them share.

Changes:

- Rename `request_id` → `stream_id` across `types.rs` and every consumer (`SessionEvent`
  stream variants, `ClientMessage::Cancel`, the client `request_to_turn` maps).
- **Daemon-assigned** (see §6.6). Widen to `u64` (removes wraparound); keep `CANCEL_ALL`
  for the pre-`Started` window.
- The client's pending slot for `RunInput` resolves on the acceptance reply (`Started`
  or `Failed`); subsequent stream events are broadcasts carrying `stream_id`, routed by
  the client's `stream_id → turn_id` map exactly as today.
- Keep `stream_id` distinct from `turn_id` (`Started` carries both): the stream id is
  live-only and assigned at acceptance; the turn id is the durable `BTreeMap` key.

---

## 9. Subscriptions

- `SubscribeSessionsSummary` / `UnsubscribeSessionsSummary` / `SubscribeAllActivity` /
  `UnsubscribeAllActivity` become ordinary correlated requests, so they are acked
  (`Accepted { kind }`). Today they have **no reply at all** — the first fire-and-forget
  hole this closes.
- **No subscription ids on delivered messages.** Streaming subscriptions are out of
  scope; the client demuxes broadcasts by `session_id` + stream kind as it does now. A
  `subscription: Option<u64>` can be added to the daemon envelope later without touching
  existing clients, so this is forward-compatible.
- The daemon's subscriber registry stays keyed by `client_id` for now (the `sub_id`
  re-key is deferred with the subscription-id idea).

---

## 10. Per-crate work breakdown

### 10.1 `choreo-proto`

- Reshape `ClientMessage`/`DaemonMessage` into `{ id, inner }` structs +
  `ClientMessageType`/`DaemonMessageType` enums (§3.1–§3.2).
- Add `MessageKind` (Copy enum) and `DaemonMessageType::Accepted`/`Failed`.
- Rename `request_id` → `stream_id` (§8); `u64`.
- `GetSessionState`: implement as a correlated `SessionState` reply **or** delete (see
  §18 Q1). The wildcard escape becomes an explicit `Failed`.
- `lib.rs` re-exports; `size.rs` id width + pin-test samples; `tests.rs` round-trips
  (struct-ish shape, `id` absent on broadcast, present on reply) and the size pin.
- Document the frame, the overlap policy, and the reply/stream axis split in rustdoc.

### 10.2 `choreo-daemon`

- `server/connection.rs`: `ReplyHandle` (§6.1); dispatch destructures the request once,
  mints the handle, and every handler takes it (§6.2); the `_ => warn!` arm becomes an
  explicit `Failed`.
- `sessions.rs` + `sessions/*`: thread `ReplyHandle` through the session commands
  (§6.3); add class-B acks; send the `RunInput` acceptance reply; assign `stream_id`.
- `daemon.rs` + `daemon/*`: command-loop replies (unlock/credential/pin/archive) carry
  ids — generalize the existing `client_writer` pattern.
- `embedded.rs`: values-only, updated to the new structs.
- `broadcast.rs`: tests updated to the new frame.

### 10.3 `choreo-client-core`

- New `pending` module: `PendingReplies`, `Pending`, `PendingContext`, `MessageKind`
  mapping, `Timeout` (§7.1). Re-exported.
- `connection.rs`: unchanged transport (reader already generic on `DaemonMessage`).
- `dispatch.rs`/`TurnEventHandler`: the by-value dispatch stays; the frontend resolves
  the pending slot before calling it.
- `credentials.rs`: `KeystoreAutoBind` unchanged; the pending unlock key folds into
  `PendingContext`.

### 10.4 `choreo-tui`

- `state/mod.rs`: add `pending: PendingReplies` to `App`; delete `next_request_id`.
- `connection/mod.rs`: `handle_ui_event`'s `Daemon` arm resolves the slot first, then
  falls through; the UI tick sweeps `expire`; render ack/timeout on the status line.
- `connection/daemon.rs`: delete the shape/UI-state matching (§11); collapse
  `SessionUpdateRouting`/`route_session_update` (ids tell "mine" from "theirs").
- `connection/chat.rs` + image fetch: route every send through `PendingReplies::send`;
  carry the `ImageKey` in `PendingContext` and drop the reply echo reliance.

### 10.5 `choreo-gui`

- Same table (shared type); egui-side ack/timeout feedback.

### 10.6 `choreo-im`, `choreo-acp`

- Allocate ids through a small adapter (ids are mandatory). These clients may ignore
  acks; if per-ack overhead is judged heavy for the IM bridge, add an internal `no_ack`
  adapter that still allocates ids but drops replies (no wire change).

### 10.7 Shared / integration

- Every crate's `tests/it/` binary that speaks the protocol updated; a shared
  `test_support` helper to build requests with ids and drain replies.

---

## 11. Deletions & simplifications enabled

- The `_ => warn!("unhandled client message")` wildcard → compile-forced ack.
- `resolve_daemon_session(None)` guessing and most `None`-origin plumbing.
- The `Suppress`/`FallThrough` routing heuristic and `SessionUpdateRouting`.
- The vision-image composite echo key (`Image { session_id, turn_id, key }`) → the
  request's `PendingContext`.
- The `pending_unlock_key` special-case → one `Pending` context.
- The client `next_request_id` counter and its `wrapping_add`.
- Ordering comments that exist only to justify reply-vs-broadcast races.
- The dead `GetSessionState` (implement it, or delete it — §18 Q1).

---

## 12. Decisions log

| # | Decision | Rationale |
|---|---|---|
| D1 | `ClientMessage { id: u64, inner: ClientMessageType }`; id starts at **0**, per-connection, never reused | Mandatory, uniform, un-forgettable by construction; the sentinel is gone so 0 is a normal value |
| D2 | `DaemonMessage { id: Option<u64>, inner: DaemonMessageType }`; `None` = broadcast | A 0-sentinel collides with request id 0; `Option` is self-documenting and compact |
| D3 | Every request gets exactly one terminal reply | Liveness + attribution; server stays stateless (echo only) |
| D4 | `ReplyHandle` guard: `send` consumes, `Drop` debug-asserts | Exactly-once is enforced, not commented |
| D5 | Streaming is a separate axis; daemon owns it; renamed `stream_id` (`u64`) | Per-connection vs per-session key spaces cannot merge; fixes cross-client collision + wraparound |
| D6 | Overlap resolved by `id` presence; split a variant only for requester-relative intent | One payload type where the payload is identical; type-safety where intent differs |
| D7 | Subscriptions are acked requests; no sub ids on delivered messages | Streaming subscriptions out of scope; `subscription` can be added later |
| D8 | No version bump; amend the unreleased version in place | Pre-release policy (`frame.rs`); no mixed-version peers |
| D9 | Rename `request_id` → `stream_id` in the same change | Avoids a name that reads as belonging to the reply axis |
| D10 | One continuous refactor, five bisectable commits | The mandatory id field means no wire-compatible incremental cut-over |

---

## 13. Phased execution plan

Each phase is one commit, gated by `just pre-commit`.

1. **P1 — proto reshape.** `{ id, inner }` structs + `*Type` enums; `MessageKind`;
   `Accepted`/`Failed`; `request_id` → `stream_id`; constructors; `size.rs` + tests.
   Compiles the workspace to the new types with ids threaded but behavior otherwise
   identical (every send allocates an id; the daemon replies as today, stamped). Large
   mechanical diff; no semantic change yet.
2. **P2 — `ReplyHandle` + ack enforcement.** Every synchronous request provably acked;
   the wildcard becomes an explicit `Failed`.
3. **P3 — session-thread acks.** Thread id-bearing commands; class-B acks; `RunInput`
   acceptance reply; daemon-assigned `stream_id`.
4. **P4 — client `PendingReplies` + TUI adoption.** The shared table, the TUI
   integration, the deletions of §11.
5. **P5 — GUI/ACP/IM + docs.** Remaining adapters; `ARCHITECTURE.md`/`README.md`; final
   cleanup.

---

## 14. Testing strategy

- **proto**: round-trip both structs (MessagePack + JSON); a broadcast encodes without
  an `id`; the size gauge ≥ encoded for every sample incl. `Accepted`/`Failed` and an
  id-bearing reply.
- **daemon**: for every correlatable request, drain the sink and assert **exactly one**
  reply carrying the matching id, and that any triggered broadcast carries `id: None`;
  a cfg(test) path drives the `ReplyHandle` drop guard to prove the assertion fires.
- **client-core**: `PendingReplies` unit tests with an **injected clock** (no time-based
  waits, per the test discipline): resolve / timeout / unknown / duplicate / out-of-order
  / clear-on-reset, and "resolve then fall-through still applies state".
- **integration** (`tests/it`): two correlated requests plus an interleaved broadcast
  arriving out of order → exact attribution; a dropped reply → `Timeout` + resync send; a
  mislabeled reply → `warn!` naming the id.

---

## 15. Documentation

- **`ARCHITECTURE.md`**: a new wire-format section (the two structs, the id invariant,
  the reply/stream axis split, the overlap policy); the version-history note (amended in
  place); module rows for `server/connection.rs` (`ReplyHandle`) and the client-core
  `pending` module.
- **Rustdoc**: `choreo-proto` carries `#![warn(missing_docs)]`; document the new types
  fully and keep `doc_crates` in the justfile green.
- **Commit messages** are the release notes (see AGENTS.md): user-facing prose, one
  commit per paragraph.

---

## 16. Risks & mitigations

- **Mechanical churn is large** — ~405 `ClientMessage::` and ~685 `DaemonMessage::`
  sites across non-proto crates, plus `tests.rs`. Mitigate with constructors
  (`request`/`reply`/`broadcast`), a `test_support` wrapper, and doing the mass migration
  in P1 as one review-sized commit.
- **Enforcement is debug-only for "reply sent".** The `ReplyHandle` guard is a
  `debug_assert!`; release builds only warn. Acceptable: the invariant is a
  contract-enforcement aid, and the client timeout is the runtime backstop.
- **A mislabeled reply/broadcast is a runtime, not compile-time, error** (that is the
  cost of the uniform frame). Mitigated by constructors + the dispatch `debug_assert!` +
  the client's unknown-id `warn!`.
- **Concurrent committer.** The tree advanced mid-analysis (HEAD moved to the
  vision-image work). Land P1 on a quiet tree and rebase the mechanical mass-migration.

---

## 17. Out of scope / future work

- **Streaming event subscriptions** (server-push streams with their own subscription
  ids) — explicitly out of scope; the `subscription` field can be added later.
- **Server→client requests** (the daemon asking the client something). The daemon has no
  such need today; the symmetric `Option<u64>` frame leaves room.
- **`sub_id`-keyed subscriber registries** — deferred with the subscription-id idea.

---

## 18. Open questions

1. **`GetSessionState`**: implement it as a correlated `SessionState` reply, or delete it
   and the `/session info` command? (Recommend: implement — the attach path already
   produces exactly this payload.)
2. **`stream_id` width**: `u64` (recommended) vs keep `u32` to match `turn_id`.
3. **`MessageKind` granularity**: one variant per `ClientMessageType`, or coarse groups?
   (Recommend: one per variant — it is the log/timeout key.)
4. **IM/ACP ack handling**: ignore acks, or the internal `no_ack` adapter? (Recommend:
   ignore; add the adapter only if profiling shows overhead.)

---

## 19. Verification / definition of done

- Every `ClientMessage` carries an id; every request yields exactly one id-bearing reply;
  every broadcast carries `id: None`. Enforced by tests, not convention (§14).
- No client code matches a reply by shape, arrival order, or UI state; no
  `resolve_daemon_session(None)` heuristic remains.
- `stream_id` is daemon-assigned; `request_id` no longer exists; no client-side request
  counter remains.
- The §11 deletions are gone.
- `just pre-commit` is green; `ARCHITECTURE.md`/`README.md` updated; the release-note
  commits written.
- This plan file is deleted.

---

## 20. Appendix: full variant classification

### 20.1 `ClientMessage` (all 40 → correlated requests)

CreateSession, ListSessions, SubscribeSessionsSummary, UnsubscribeSessionsSummary,
AttachSession, GetSessionState, RunInput, Cancel, Ping, GetCredential, ListModels,
RefreshModels, SetModel, Unlock, Lock, BindKeystore, AddCredential, RemoveCredential,
AclAdd, DeleteSession, SetSessionPinned, SetSessionArchived, AddAccount, RemoveAccount,
ListAccounts, SetSessionAccount, SetReasoningEffort, GetReasoningEffort, Undo, Redo,
ContinueGeneration, GetImage, McpStatusRequest, McpReconnect, McpReload, McpTrust,
McpUntrust, McpTrustList, SubscribeAllActivity, UnsubscribeAllActivity.

### 20.2 Reply behaviour per request

**Class A — already reply with data** (reply body = the existing variant; id-stamped):
Ping→Pong; ListSessions→Sessions; ListModels→Models/ModelsFailed;
RefreshModels→ModelsRefreshed/ModelsRefreshFailed; GetCredential→Credential;
ListAccounts→Accounts/AccountListFailed; AddAccount/RemoveAccount→Account*;
Unlock→Unlocked/KeystoreUnbound/LockedError; Lock→Locked/LockedError;
BindKeystore→Bound/KeystoreUnbound/LockedError;
AddCredential→Unlocked (auxiliary) then CredentialAdded/Failed (terminal);
RemoveCredential→CredentialRemoved/Failed; AclAdd→AclAddResult;
CreateSession→SessionCreatedForRequester/SessionFailed;
AttachSession→SessionAttached/SessionState/SessionFailed;
GetImage→Image; Mcp*→McpStatus/McpReloaded/McpTrustUpdated/… .

**Class B — success is currently silent** (gain `Accepted`/`Failed{kind}`):
SetModel (broadcast `ModelSelected` stays), SetReasoningEffort (broadcast stays),
SetSessionAccount (broadcast stays), Undo, Redo, SetTitle (broadcast stays),
SetWorkingDir (broadcast stays), SetSessionPinned (broadcast `SessionFlagsChanged`
stays), SetSessionArchived (broadcast stays), DeleteSession (broadcast
`SessionDeleted` stays), Subscribe*/Unsubscribe* (new ack).

**Special — streaming** (reply is the acceptance; the stream stays broadcast):
RunInput/ContinueGeneration → `Started{stream_id, turn_id}` (accept) or `Failed`
(reject), plus the `stream_id`-tagged broadcasts; `Cancel{stream_id}` →
`Cancelled{stream_id}` (broadcast) is the existing stream behaviour, and the request
itself is acked with `Accepted`.

### 20.3 `DaemonMessage` / `SessionEvent` classification

- **Reply-only (id Some)**: Sessions, Pong, Models, ModelsFailed, Unlocked, Locked,
  Bound, LockedError, KeystoreUnbound, CredentialAdded, CredentialAddFailed,
  CredentialRemoved, CredentialRemoveFailed, Credential, AclAddResult, AccountAdded,
  AccountAddFailed, AccountRemoved, AccountRemoveFailed, Accounts, AccountListFailed,
  ModelsRefreshed, ModelsRefreshFailed, Image, McpStatus, McpReconnectFailed,
  McpReloaded, McpReloadFailed, McpTrustUpdated, McpTrustList, Accepted, Failed;
  SessionEvent: SessionCreatedForRequester, SessionAttached, SessionDeleteFailed.
- **Broadcast-only (id None)**: Keystore, CatalogUpdated, AclUpdated, ShuttingDown,
  Evicted; SessionEvent: SessionCreated, TurnAppended, SessionStatusChanged,
  SessionFlagsChanged, TurnsUndone, TurnsRedone, ModelSelected, SessionAccountSet,
  ContextWindowResolved, SessionWorkingDirSet, SessionTitleSet, TokenUsageUpdate,
  LiveOutputTokenCount, ContextWindowResolved, Started, ToolCallStarted,
  ToolCallFinished, ToolCallFailed, ToolResultChunk, OutputChunk, Done, Failed,
  Cancelled.
- **Dual (id distinguishes)**: SessionEvent::SessionState (attach reply vs
  load/unload-tools broadcast), SessionEvent::ReasoningEffortSet (GetReasoningEffort
  reply vs SetReasoningEffort broadcast), SessionEvent::ModelSelected /
  ModelSelectionFailed (SetModel confirmation vs fan-out), Done/Failed (stream terminal
  vs fan-out).
