# Plan: MCP Events as a client (react to events from connected servers)

**Status:** proposed — *awaiting a decision on the event→work mapping (§5 D8) and
on whether `push` or `poll` ships first (§5 D1).* The wire facts below are taken
from the draft MCP Events extension (the design sketch at
`modelcontextprotocol/experimental-ext-triggers-events`) and the OpenAI plugin
page that documents the ChatGPT integration of a subset of it. This plan covers
**only the client/consumer role**: choreographr subscribing to events that a
connected MCP server produces. The server/producer role (exposing choreographr's
own events to ChatGPT or another agent) is out of scope — see §12.
**Lifecycle:** this file is **deleted once the plan is implemented**. Nothing
written during implementation may reference it — rustdoc, `ARCHITECTURE.md`,
`README.md`, release notes, and commit messages must stand on their own, because
a reference to this plan would go stale the moment it is removed.
**Date:** 2026-10-06
**Targets:** `choreo-mcp` (protocol value types, engine methods, dispatcher
commands, the push-stream listener), `choreo-daemon` (`src/mcp/` manager +
config + a new event sink/forwarder, `daemon.rs` command loop, `sessions.rs`
turn ingress), `choreo-proto` (control-surface messages), `choreo-tui` /
`choreo-client-core` (later phase: `/mcp events` surface).
**Touches (when implemented):** `choreo-mcp` (`protocol.rs`, `config.rs`,
`engine/*`, `session/*`, `error.rs`, `lib.rs`), `choreo-daemon`
(`src/mcp/{mod,config,events}.rs`, `daemon.rs`, `server/core.rs`, `sessions.rs`,
`cli.rs`), `choreo-proto` (`types.rs`, `frame.rs` version note), `choreo-tui` /
`choreo-gui` / `choreo-im` (the `/mcp events` surface), the justfile
(`doc_crates`), `ARCHITECTURE.md`, `README.md`.

> **TL;DR.** The draft MCP Events extension lets an MCP client subscribe to
> *things happening in the server's upstream system* — a Slack message, a GitHub
> push, a PagerDuty incident — and have the agent react without a user present.
> A server advertises `capabilities.events`, lists event types via `events/list`
> (each with subscription-argument and payload JSON Schemas and a set of
> supported delivery modes), and delivers occurrences `{eventId, name, timestamp,
> data, cursor}` by one of **poll** (`events/poll`), **push**
> (`events/stream` + `notifications/events/*`), or **webhook**
> (`events/subscribe` + Standard Webhooks POSTs). choreographr is an MCP
> **client only** today, already speaks the required 2026-07-28 era, and already
> has the exact seams this needs: a per-server dispatcher/engine
> (`session/`, `engine/`) and a server-notification pipeline that feeds the
> daemon (`McpListChange` → catalogue hot-swap). This plan makes choreographr
> subscribe to and consume server events — **poll and push first** — and route
> each occurrence into a session as a new turn. **Webhook is deliberately
> deferred**: it is the only mode that requires choreographr to expose a
> publicly-reachable HTTPS callback, which a local daemon cannot do without a
> tunnel/relay; the draft explicitly allows a forward proxy to receive webhooks
> and re-serve them over poll/push, so that path is documented rather than
> built. The genuinely new design is the last mile — how an arriving event
> becomes *work* (§5 D8), which the draft itself leaves to the application.

---

## Table of contents

1. [Motivation & evidence](#1-motivation--evidence)
2. [What "MCP Events (client role)" means here](#2-what-mcp-events-client-role-means-here)
3. [The extension, as it affects a client](#3-the-extension-as-it-affects-a-client)
4. [Gap analysis: choreographr today vs the extension](#4-gap-analysis-choreographr-today-vs-the-extension)
5. [Design decisions](#5-design-decisions)
6. [Target architecture](#6-target-architecture)
7. [Cross-crate change inventory](#7-cross-crate-change-inventory)
8. [Work breakdown](#8-work-breakdown)
9. [Testing strategy](#9-testing-strategy)
10. [Security & trust model](#10-security--trust-model)
11. [Configuration & control surface](#11-configuration--control-surface)
12. [Out of scope / future work](#12-out-of-scope--future-work)
13. [Risks & mitigations](#13-risks--mitigations)
14. [Open questions](#14-open-questions)
15. [Definition of done](#15-definition-of-done)
16. [References](#16-references)

---

## 1. Motivation & evidence

### 1.1 What the extension is

The "MCP Events" feature documented at
`developers.openai.com/plugins/build/mcp-events` is the ChatGPT-side integration
of a draft experimental MCP extension, **MCP Events** (design sketch dated
2026-02-19, authors Peter Alexander), living in the `modelcontextprotocol`
`experimental-ext-triggers-events` repository. It is **not base MCP**: it is an
opt-in extension layered on the **2026-07-28** protocol era ("MCP 2.0" in the
OpenAI page's phrasing). Its purpose is to let an agent react to occurrences in
an upstream system *without the user being present* — the draft's own examples
are "monitor a Slack channel for bug reports" and "watch a document for review
comments."

The extension defines:

- a **capability** a server advertises (`capabilities.events`),
- a **discovery** method (`events/list`) returning event-type descriptors,
- three **delivery modes** — **poll** (`events/poll`), **push**
  (`events/stream` + `notifications/events/*`), **webhook** (`events/subscribe`
  + signed HTTP POSTs), and
- **occurrences** `{eventId, name, timestamp, data, cursor}` with opaque cursors
  for resumability and `eventId` for dedup.

The OpenAI page documents a **narrow profile** of this: ChatGPT supports only
**webhook** delivery and does *not* support polling, streaming, or the draft's
`gap`/`terminated` notifications ("MCP Events in ChatGPT requires MCP 2.0
(protocol version `2026-07-28`) ... ChatGPT supports webhook delivery ... Polling,
streaming, and the draft's `gap` and `terminated` control notifications are not
supported by this integration.").

### 1.2 Why this matters to choreographr

choreographr is an MCP **client** (`choreo-mcp`), consumed by the daemon behind
the (default-on) `mcp` feature. It already:

- negotiates the **2026-07-28** era — the era MCP Events requires — via
  `McpProtocolMode::{Auto,Modern}` and `ProtocolVersion::V_2026_07_28`
  (`choreo-mcp/src/engine/mod.rs`, `config.rs`);
- opens a long-lived server→client stream and feeds it to the daemon:
  `subscriptions/listen` events become `McpListChange` and trigger a catalogue
  hot-swap (`engine/mod.rs::spawn_list_change_listener`, `McpListChange` in
  `protocol.rs`, `McpManager::take_list_change_rx`, the
  `spawn_mcp_list_change_forwarder` thread in `server/core.rs`); and
- owns a sidecar tokio runtime and a per-server dispatcher that isolate the
  daemon from `rmcp` (`runtime.rs`, `session/`).

The 2026-07-28 work deliberately did **not** include the MCP-server role or any
event support (see `docs/plans/mcp-modernization.md` §13). Events are the natural
next increment on the client side, and the daemon already has the exact plumbing
shape (an engine stream → a crossbeam sink → a forwarder thread → a
`DaemonCommand`) that a first cut can reuse.

### 1.3 Why the client role specifically

There are two roles in the extension, and they are not the same project:

| Role | What it needs | Cost |
|---|---|---|
| **Client / consumer** — react to events from servers choreographr connects to | Discover event types, subscribe, consume poll/push (webhook via proxy), route occurrences into a session | Fits the existing `choreo-mcp` + daemon seams; no new inbound network surface for poll/push |
| **Server / producer** — expose choreographr's own events to ChatGPT/other agents | A `server/discover` server, `events/*` handlers, webhook POSTs with Standard Webhooks signing, SSRF-safe delivery, endpoint verification, durable subscription/TTL storage | The unbuilt "MCP server role" (§12) **plus** a new public HTTPS ingress |

This plan is the client role. Webhook is included only as a documented,
proxy-mediated path, because it is the sole mode that forces choreographr to
expose an inbound endpoint — and because the draft *intends* for that to be
optional (a client behind NAT uses poll; a client with a proxy uses webhook).

---

## 2. What "MCP Events (client role)" means here

Scope of this plan:

1. **Detect** that a connected server supports events, from its advertised
   capability.
2. **List** its event types (`events/list`, paginated) and expose them so a user
   or session can choose what to monitor.
3. **Subscribe** to a chosen event type with filter `arguments`, using a
   delivery mode the client can actually serve — **poll** and **push** in the
   first cut.
4. **Consume** occurrences: run the poll loop, or hold the `events/stream`
   stream; dedup by `eventId`; persist and advance the cursor; handle
   `truncated`, `notifications/events/{active,error,terminated}`, and heartbeats.
5. **Route** each occurrence into a session as a new turn (the last-mile design
   in §5 D8), treating the payload as untrusted data.
6. **Manage** subscriptions: list, add, remove, refresh — over a `/mcp events`
   control surface, and cleaned up on shutdown.

Explicitly **not** in this plan (see §12): the server/producer role; webhook
delivery as a first-class client mode (documented via the forward-proxy
pattern, deferred); `events/poll` and `events/stream` *server-side* SDK
concerns (lease tables, ring buffers, emit hooks) which are the server author's
responsibility; and durable/replayable delivery beyond what a server offers.

---

## 3. The extension, as it affects a client

This section records the wire facts a client must honour. They are the basis for
the types and methods in §5/§6. **The extension is a draft**; the exact
capability location and error codes may still change, so the client must be
tolerant of a moving target (see D4).

### 3.1 Capability declaration

The design sketch advertises event support in the server's capabilities:

```jsonc
{ "capabilities": { "events": { "listChanged": true } } }
```

The OpenAI page shows the same shape minimally (`{"tools": {}, "events": {}}`).
Note this is a **top-level `events` capability**, not an entry in the SEP-1724
`extensions` map. That matters for detection (§4, D4).

### 3.2 Methods and notifications

| Method / notification | Direction | Purpose |
|---|---|---|
| `events/list` | client → server, request/response | List event types (`cursor`/`nextCursor` paginated). |
| `events/poll` | client → server, request/response | Poll mode: `{name, arguments, cursor, maxAgeMs?, maxEvents?}` → `{events[], cursor, truncated, hasMore, nextPollMs}`. |
| `events/stream` | client → server, long-lived request | Push mode: server returns notifications on the stream until cancelled. |
| `events/subscribe` | client → server, request/response | Webhook mode: register/refresh a callback URL. |
| `events/unsubscribe` | client → server, request/response | Webhook mode: eager teardown. |
| `notifications/events/list_changed` | server → client | Event-type set/descriptors changed; re-call `events/list`. |
| `notifications/events/active` | server → client (push) | Subscription confirmed; carries `cursor`, `truncated`, and `_meta.subscriptionId`. |
| `notifications/events/event` | server → client (push) | One occurrence. |
| `notifications/events/error` | server → client (push) | Transient per-occurrence failure; stream stays open. |
| `notifications/events/heartbeat` | server → client (push) | Liveness, carries current `cursor`. |
| `notifications/events/terminated` | server → client (push) | Subscription ended (e.g. access revoked). |

Push notifications are correlated by `params._meta["io.modelcontextprotocol/subscriptionId"]`,
which echoes the `events/stream` request's JSON-RPC `id` (per SEP-2575) so a
client with several concurrent streams can demultiplex them.

### 3.3 Event type descriptor (`events/list`)

Each entry has `name`, `description`, `delivery` (a non-empty subset of
`"poll"`, `"push"`, `"webhook"`), `inputSchema` (JSON Schema for subscription
`arguments`), and `payloadSchema` (JSON Schema for each occurrence's `data`).
A client that cannot use any advertised mode cannot subscribe. **Schema
evolution is additive**: the client must treat both schemas as untrusted and
bounded, exactly as it already treats tool schemas.

### 3.4 Occurrence

`{eventId, name, timestamp (ISO 8601), data (object), cursor?}`. `eventId` is the
dedup key (server-assigned; may be the upstream's stable id). `cursor` is opaque
and server-defined; `null` means the event type does not support replay.

### 3.5 Cursors and replay

- A cursor is a position in the stream, opaque to the client.
- `null` in a request means "start from now."
- `maxAgeMs` bounds replay: replay from the later of the supplied cursor and
  `now − maxAgeMs`.
- `truncated: true` means the server started later than the supplied cursor
  (events were skipped); the client persists the fresh cursor and continues —
  it does **not** reconnect.
- Cursors must advance during quiet periods: poll responses carry `cursor` even
  when `events: []`; push heartbeats carry it; the webhook refresh response
  carries it. The client persists the most recent value.

### 3.6 Errors

The extension defines general-purpose codes in the `[-32000, -32099]` range with
typed `data` discriminators:

| Code | Message | Meaning |
|---|---|---|
| `-32602` | `InvalidParams` | Arguments don't match `inputSchema`; malformed callback URL/secret. |
| `-32011` | `NotFound` | Unknown event name, or no matching subscription. `data.kind` = `"event"`/`"subscription"`. |
| `-32012` | `Forbidden` | Principal not permitted, or access revoked. |
| `-32013` | `ResourceExhausted` | A server-imposed limit/quota (`data.limit`, `data.max?`). |
| `-32014` | `Unsupported` | Well-formed but unsupported option (e.g. a delivery mode the event type omits). `data` identifies it. |
| `-32015` | `CallbackEndpointError` | Webhook-only: callback failed verification/reachability; `data.reason` a fixed category. |

### 3.7 Security posture (from the draft)

- **Event payloads are untrusted data**, with the same prompt-injection caution
  as tool results; clients should sanitize/sandbox before presenting to a model.
- **Payload minimality**: servers should send triage fields, not full content;
  the client fetches detail via tools.
- **Action-time authorization**: receiving an event is *not* authority to act;
  any tool the agent calls still goes through normal authorization.

### 3.8 Delivery-mode selection (client SDK guidance)

The draft's client guidance intersects the event type's `delivery` list with the
modes the client is configured for, preferring **webhook → push → poll**, and
raises `NoCompatibleDeliveryMode` when nothing matches. For our local-first
client the preference is inverted for the first cut (see D1): **poll → push**,
with webhook only when a proxy callback is configured.

---

## 4. Gap analysis: choreographr today vs the extension

Severity: **S1** = blocks the capability outright; **S2** = needed for a usable
first cut; **S3** = robustness/quality; **S4** = polish.

| # | Gap | Severity | Evidence | Fix |
|---|---|---|---|---|
| E1 | No event capability detection | S2 | `engine/mod.rs::server_has_resources` reads only `capabilities.resources`; there is no `events` check | D4 |
| E2 | No event value types at the crate boundary | S2 | `protocol.rs` has `McpTool`/`McpResource`/`McpListChange` only | Add `McpEventSpec`/`McpEventOccurrence`/`McpSubscription` |
| E3 | No `events/*` methods | S2 | `McpEngine` (`session/mod.rs`) has `list_tools`/`call_tool`/`list_resources`/`read_resource` only | Add engine methods + `McpCommand` variants |
| E4 | No push stream listener | S2 | `spawn_list_change_listener` exists for `subscriptions/listen`; no events equivalent | D6 |
| E5 | No event sink into the daemon | S2 | `McpListChange` is the only server→daemon datum; the daemon has no event path | Add an `McpEvent` sink + forwarder |
| E6 | No event→turn ingress | **S1** | `SessionCommand` has `RunInput`/`RunChildInput` but no internal-trigger variant; nothing starts a turn without a user | D8 (the hard one) |
| E7 | No cursor / dedup persistence | S2 | no per-subscription state anywhere in `mcp/` | A bounded state store (D7) |
| E8 | No capability parsing for a top-level `events` key | S2 | `rmcp` 3.5 `ServerCapabilities` has no `events` field and does not deny unknown fields, so a `server/discover` result's `events` is silently dropped by the typed parse | D4 |
| E9 | No control surface | S3 | `/mcp` covers status/reconnect/reload; no event subscriptions | Add `/mcp events` (P4) |
| E10 | No event-specific config keys | S3 | `mcp.json` has `timeout`/`protocol`/`cwd`/`disabledTools`/`shared` | Add `events` (D5) |
| E11 | No error mapping for the extension codes | S3 | `McpError` has `JsonRpcError { code, message }` only | Map `-32011…-32015` to typed, actionable variants |
| E12 | No untrusted-payload bounding for events | S2 | tool schemas/text are bounded; event payloads would be a new unbounded input | D9 |

---

## 5. Design decisions

### D1 — Scope to the **client/consumer** role; ship **poll** first, then **push**; **webhook** deferred to the proxy path

The producer role is out of scope (§12). Among delivery modes:

- **Poll** is the floor: it works from behind NAT with no server-held state, no
  held connection, and no inbound endpoint, and it maps cleanly onto the
  existing request/response dispatcher. **Ship it first.**
- **Push** is low-latency but holds a per-subscription stream. On **stdio** it is
  just JSON-RPC notifications on stdout (trivial for us — the transport already
  delivers server notifications). On **Streamable HTTP** it is a long-lived SSE
  response, the same shape `subscriptions/listen` already uses. Ship it second.
- **Webhook** requires choreographr (or a proxy acting for it) to expose an
  **https** callback and verify Standard Webhooks signatures. A local daemon has
  no public endpoint, so webhook is **not implemented as a first-class mode**;
  instead the documented path is the draft's own forward-proxy pattern: the
  proxy receives webhooks and re-serves events to choreographr over poll or
  push. A user wanting webhook stands up the proxy; choreographr consumes it
  like any other event source. (If a future need justifies it, a webhook
  receiver is a separate, security-reviewed component — see §13.)

Configurable preference: `"events": {"delivery": "auto|poll|push"}`, default
`auto` = the order poll → push, intersected with each event type's `delivery`
list.

### D2 — Carry our own event types at the crate boundary; never leak an `rmcp` type

Consistent with the crate's existing rule (`protocol.rs` module docs: "The daemon
never sees an `rmcp` type"). Add to `protocol.rs`:

- `McpEventSpec { name, description, delivery: Vec<McpDeliveryMode>, input_schema, payload_schema }`
- `McpDeliveryMode { Poll, Push, Webhook }`
- `McpEventOccurrence { event_id, name, timestamp, data: serde_json::Value, cursor: Option<String> }`
- `McpSubscription { slug, name, arguments, mode, id: Option<u64> }`
- new `McpServerEvent` (the daemon sink datum, alongside `McpListChange`) carrying
  the slug, the subscription, and the occurrence — plus a variant for
  `terminated`/`truncated` lifecycle events.

Both schemas are normalized/bounded with the existing helpers
(`normalize_input_schema`, `MAX_SCHEMA_BYTES`, `MAX_SCHEMA_DEPTH`) and capped
per server (a new `MAX_EVENTS_PER_SERVER`, mirroring `MAX_TOOLS_PER_SERVER`).

### D3 — Send the methods over `rmcp`'s generic custom-request escape hatch

`rmcp` 3.5 does **not** type the extension. It does provide a generic path:
`ClientRequest::CustomRequest(CustomRequest)` / `CustomResult` (a
`serde_json::Value` catch-all) and `ServerNotification::CustomNotification` routed
to the `ClientHandler`. So:

- `events/list`, `events/poll`, `events/subscribe`, `events/unsubscribe` are
  sent as `CustomRequest::new("events/list", Some(params))` and the result parsed
  from `CustomResult` into our own types.
- Push `notifications/events/*` arrive as `CustomNotification` (method + params)
  and are demultiplexed by `_meta.subscriptionId`.
- The engine remains the only `rmcp`-coupled module; the conversions live in
  `engine/convert.rs`, as for tools/resources.

This keeps us unblocked on upstream typing. When `rmcp` adds typed events
support, the engine's conversions swap over without changing the daemon-facing
types.

### D4 — Detect the capability from the raw `server/discover` payload

`rmcp` 3.5's `model/capabilities.rs::ServerCapabilities` has fields
`experimental`, `extensions`, `logging`, `completions`, `prompts`, `resources`,
`tools` — **no `events`**, and the struct does not `deny_unknown_fields`, so an
`events` member in a `server/discover` result is silently dropped at parse time.
Detection therefore needs a raw look at the discover result. Options, in
preference order:

1. **Upstream**: `rmcp` gains a typed `events` field (or folds it into
   `extensions`). Ideal, not in our control.
2. **Raw capture**: during `connect`, alongside the typed handshake, issue a
   `server/discover`/`initialize` capture that preserves the capabilities object
   verbatim, and read `capabilities.events` from the raw JSON. The connect path
   already has the running service; a small, additive capture that does not
   disturb the negotiated era.

Decision: implement (2) behind a helper (`events_capability(peer_info)`) that
reads the raw capability map when available, with a documented TODO to delete it
when (1) lands. If the raw payload is unavailable (a legacy peer), treat events
as unsupported. Gate the whole feature on the **stateless era** — the extension
is defined for 2026-07-28; a `has_initialize()` peer opens nothing.

### D5 — Config keys on the existing `mcp.json` per-server entry

Extend the de-facto `mcpServers` entry, never replace it:

```jsonc
{
  "mcpServers": {
    "github": {
      "url": "https://mcp.example.com/mcp",
      "headers": { "Authorization": "Bearer ${GH_TOKEN}" },
      "events": {
        "delivery": "auto",            // auto | poll | push
        "autosubscribe": [             // optional: subscribe at connect, no user action
          { "name": "pull_request.opened", "arguments": { "repo": "acme/webapp" } }
        ],
        "route": "new-session",        // new-session | session   (see D8)
        "targetSessionId": 42,         // required when route == "session"
        "pollFloorMs": 1000,           // clamp on server nextPollMs
        "maxInFlightPerSubscription": 8
      }
    }
  }
}
```

Unknown sub-keys are collected, logged, and ignored — never fatal, matching the
existing config policy. Absent `events`, a server is not asked for events and
opens no stream. `autosubscribe` is the headless path (the daemon subscribes at
connect); the interactive path is the `/mcp events` surface (§11).

### D6 — Engine/dispatcher shape: mirror the list-change pipeline, add an events sink

The `subscriptions/listen` design is the template and should be followed
closely, split into a **request/response** half (poll, list, subscribe) and a
**stream** half (push):

- **Value flow:** the engine owns a crossbeam `Sender<McpServerEvent>` (like the
  list-change `Sender<McpListChange>`), created in `McpManager` and threaded
  through `connect`/`factory` so a reconnect re-establishes subscriptions. The
  daemon takes the receiver once (`take_event_rx`) and pumps it into a new
  `DaemonCommand::McpServerEvent` over a forwarder thread — the exact shape of
  `spawn_mcp_list_change_forwarder`.
- **Request/response:** add `McpEngine::list_events`, `poll_events` (and later
  `subscribe_events`/`unsubscribe_events`), plus `McpCommand::{ListEvents,
  PollEvents, …}` and `McpServerHandle` methods that block on the per-request
  reply channel, matching `list_tools`/`call_tool`.
- **Poll loop:** a background task per active poll subscription, spawned on the
  sidecar runtime (like `spawn_list_change_listener`), calling `poll_events`
  every `max(nextPollMs, pollFloorMs)`, draining immediately while
  `hasMore: true`. Waits are event-driven (`crossbeam_channel::after` for the
  timer, or a `tokio::time::sleep` on the runtime) — never a flag-poll loop.
- **Push stream:** `spawn_event_stream_listener(peer, subscription, slug, sink)`
  mirrors `spawn_list_change_listener`: send the `events/stream` custom request,
  keep it alive, route `notifications/events/event` to the sink, apply
  `active`/`truncated` cursor updates, ignore heartbeats beyond advancing the
  cursor, terminate on `terminated`/error, and cancel the stream on shutdown.
  On Streamable HTTP this depends on `rmcp` surfacing a streaming response for a
  custom request — the same capability `subscriptions/listen` relies on; P2
  verifies it and, if the generic path cannot stream, falls back to poll until
  upstream exposes one (a scoped, documented limitation).

Reduced by reusing existing machinery: the crossbeam-sender-threaded-through-
connect pattern, the forwarder-thread-into-`DaemonCommand` pattern, the
sidecar-runtime stream-listener pattern, and the engine-only-touches-`rmcp` rule.
No new locks; the only shared state is whatever cursor store D7 picks.

### D7 — Persist cursors and the dedup window

Cursors are client-owned and must survive a daemon restart so a resubscribe
resumes rather than restarting "from now". Store per `(slug, subscription-key)`:

- the last-persisted `cursor` (opaque string, or absent when `cursor: null`),
- a bounded ring of recent `eventId`s for dedup (size-capped, e.g. the last N),
- the subscription descriptor (name, canonicalized arguments, mode) so a
  reconnect can re-subscribe.

Location: a small JSON state file per server under the app state/data dir (the
`choreo-shared::paths` convention — `data_dir()`), written atomically, owner-only
— **not** the config dir (this is runtime state, not configuration) and not the
main `state.redb` (kept small and separate; revisit if volume warrants). Reads
are fail-closed: a missing/corrupt state file means "no cursor," which is safe
(resubscribe from now).

### D8 — Event → work: the last mile (the genuinely new design)

The extension deliberately leaves "how an event reaches the LLM" to the
application. choreographr has no internal "start a turn" path today: sessions are
driven by `SessionCommand::RunInput`/`RunChildInput` from a client. So an event
must be turned into a turn. Proposed model:

- **A new daemon command** `DaemonCommand::McpServerEvent { slug, occurrence, ... }`
  (single writer of any state) receives forwarded occurrences, applies the
  per-subscription **route** policy, and then either:
  - **`route: session`** — enqueue a new `SessionCommand` (a new
    `SessionCommand::RunEvent { event }` variant) into the target session's
    command channel; or
  - **`route: new-session`** — create a session (the `CreateSession` path) whose
    first turn is the event, inheriting a configured template (working dir,
    model, tool groups) so the event has a place to run.
- **Idle-gating:** if the target session is mid-turn, the event is **queued**,
  not interleaved. The draft itself allows client-side prioritization; the
  simplest correct rule is FIFO, delivered when the session goes idle.
- **Payload rendering:** the occurrence is rendered as an untrusted
  **event-attachment** on the new turn, with an explicit provenance header
  (`[MCP event <name> from <slug> at <timestamp>]`) and the `data` object as
  data, never as instructions (D9). The user's standing instruction for the
  subscription ("do X when this fires") is what supplies intent; the event
  supplies data.
- **Default OFF.** Route requires an explicit per-subscription choice (config or
  the `/mcp events` surface). A subscription can also be **observe-only**
  (occurrences are logged/streamed to attached clients but start no turn) — the
  safe default for a subscription created just to inspect a server's events.

Open (see §14): whether `route: session` should be a dedicated "events session"
per server (its own conversation, so event turns never pollute a working session)
rather than an arbitrary existing session. The plan's default is a dedicated
session when `route: new-session`, and an explicit `targetSessionId` otherwise.

### D9 — Untrusted payloads and bounds

Event payloads are external, potentially attacker-controlled text. Apply the
existing tool-result posture:

- bound each occurrence's `data` (bytes and JSON depth, reusing
  `MAX_SCHEMA_BYTES`/`MAX_SCHEMA_DEPTH`-style caps) before it enters a prompt;
- render with a provenance header and as data, never as instructions;
- apply the daemon's existing untrusted-content handling to the rendered turn;
- cap subscriptions per server and per session, and cap in-flight events per
  subscription (`maxInFlightPerSubscription`) so a burst cannot flood a session;
- never log secret-bearing fields; a server's `headers`/`env` remain redacted as
  today.

### D10 — Error taxonomy

Map the extension's `-32011…-32015` (and `-32602`) to typed, actionable
`McpError` variants (a new `McpError::Event { code, message, kind }` or a small
set), so the `/mcp events` surface can say *why* a subscription failed
(authorized? unknown name? quota? unsupported mode?) rather than surfacing a raw
JSON-RPC code — the same treatment `AuthRequired` already gives `401/403`.

---

## 6. Target architecture

As proposed:

```
choreo-daemon (thread-only)
  └── mcp/
        ├── config.rs   mcp.json per-server `events` block →
        │               McpServerConfig.event_config: Option<McpEventConfig>
        ├── events.rs   the event runtimes: poll loops + push streams (sidecar),
        │               subscription registry, route policy application
        ├── mod.rs      McpManager gains event_tx/event_rx (crossbeam), the
        │               event sink thread-through-connect, take_event_rx(),
        │               subscribe/unsubscribe/list-events fan-out
        └── tool.rs     unchanged
  command loop:
    spawn_mcp_event_forwarder:  event_rx → DaemonCommand::McpServerEvent
    handle_mcp_server_event:    apply route policy
      → route "session":      SessionCommand::RunEvent (queued, idle-gated)
      → route "new-session":  DaemonCommand::CreateSession + first event turn
    cursor/dedup store under the app data dir (atomic, owner-only, fail-closed)

choreo-mcp (library; owns tokio + rmcp)
  ├── protocol.rs   McpEventSpec, McpDeliveryMode, McpEventOccurrence,
  │                 McpSubscription, McpServerEvent, normalize_* reuse,
  │                 MAX_EVENTS_PER_SERVER
  ├── config.rs     McpEventConfig { delivery, autosubscribe, poll_floor, … }
  ├── engine/
  │   ├── mod.rs    list_events / poll_events / subscribe / unsubscribe via
  │   │             CustomRequest→CustomResult; events_capability() raw detect;
  │   │             spawn_event_stream_listener (mirrors list-change listener)
  │   └── convert.rs CustomResult/params ⇆ our event types
  ├── session/      McpCommand::{ListEvents, PollEvents, Subscribe, Unsubscribe};
  │                 McpEngine trait methods; McpServerHandle methods;
  │                 event sink threaded through connect/factory
  └── error.rs      typed event-error variants

choreo-proto (control plane)
  /mcp events [list|add|remove] + ClientMessage::{McpEventList, McpEventSubscribe,
  McpEventUnsubscribe} ⇄ DaemonMessage::{McpEventList, McpEventSubscribed,
  McpEventUnsubscribed, McpEventError}
```

Data flow for one occurrence (poll or push):

1. Poll loop / push listener (sidecar) obtains an `McpEventOccurrence`.
2. Engine dedups by `eventId` against the persisted window, advances and persists
   the cursor, and `try_send`s an `McpServerEvent` on the crossbeam sink
   (best-effort: a full sink drops and counts, as progress does).
3. The daemon's `spawn_mcp_event_forwarder` thread blocks on `event_rx` and sends
   `DaemonCommand::McpServerEvent` to the command loop.
4. The command loop applies the route policy: queue a `SessionCommand::RunEvent`
   (or create a session whose first turn is the event).
5. The session turn runs with the occurrence attached as untrusted data; any tool
   call it makes still goes through normal authorization and cancellation.

Lifecycle: subscriptions are established at connect for `autosubscribe` entries
and on demand from the surface; a transport failure is rebuilt under the existing
restart policy, and the rebuilt engine re-establishes subscriptions from the
persisted store and cursors; shutdown cancels streams and joins boundedly, like
`shutdown_all`.

---

## 7. Cross-crate change inventory

- **`choreo-mcp`**
  - `protocol.rs`: event value types, delivery-mode enum, caps/bounds, tests.
  - `config.rs`: `McpEventConfig` (+ parse/validate), tests.
  - `engine/mod.rs` + `engine/convert.rs`: capability detection, the four
    methods over `CustomRequest`, the push listener, conversions, tests.
  - `session/mod.rs` + `session/dispatch.rs`: new `McpCommand` variants, the
    `McpEngine` trait methods, `McpServerHandle` methods, the event sink threaded
    through `connect`/`factory`, tests.
  - `error.rs`: typed event-error variants + display tests.
  - `lib.rs`: re-exports.
- **`choreo-daemon`**
  - `src/mcp/mod.rs`: `event_tx`/`event_rx`, `take_event_rx`, subscribe/
    unsubscribe/list fan-out, re-subscribe on reconnect.
  - `src/mcp/events.rs` (new): the poll-loop + push-stream runtime owner and the
    subscription registry.
  - `src/mcp/config.rs`: the `events` config block.
  - `src/daemon.rs`: `DaemonCommand::McpServerEvent`, `handle_mcp_server_event`,
    the route policy.
  - `src/server/core.rs`: `spawn_mcp_event_forwarder` (mirror the list-change
    forwarder).
  - `src/sessions.rs`: `SessionCommand::RunEvent` + idle-gated queuing.
  - `src/cli.rs`: `choreographr mcp events …`.
- **`choreo-proto`**: the event control messages (a `PROTOCOL_VERSION` note if
  needed; version bumps happen at release time per house policy).
- **`choreo-tui` / `choreo-gui` / `choreo-im`**: the `/mcp events` surface.
- **Docs**: `ARCHITECTURE.md` (the `choreo-mcp` module table, the `mcp/` daemon
  row, the threading model), `README.md` (capability lines + the `events` config
  reference), crate rustdoc (`#![warn(missing_docs)]` stays green; `doc_crates`
  list), the justfile.

---

## 8. Work breakdown

Each phase is independently shippable and lands with tests + docs.

### P0 — Value types, capability detection, `events/list` (read-only)

- Add the event value types, delivery-mode enum, and caps to `protocol.rs`.
- Add raw capability detection (`events_capability`) gated on the stateless era
  (D4).
- Add `McpEngine::list_events` + `McpCommand::ListEvents` + the handle method,
  following `events/list` pagination, with the same schema normalization/bounds
  as tools.
- Add `McpManager::list_events(slug)` so the surface can show a server's event
  types without subscribing.
- Show event types in `mcp status`/`session_inspect` (count, names).
- Tests: fixture server advertises `events`; `events/list` paginated; a server
  without the capability is untouched; a legacy peer opens nothing.

### P1 — Poll delivery + event sink + cursor store

- Add `McpEngine::poll_events`, `McpCommand::PollEvents`, the handle method.
- Add the crossbeam `McpServerEvent` sink threaded through `connect`/`factory`
  and `take_event_rx`.
- Add `src/mcp/events.rs`: the poll-loop owner (one task per subscription,
  `max(nextPollMs, pollFloorMs)`, `hasMore` drain), dedup, cursor advance+persist
  (D7).
- Add `DaemonCommand::McpServerEvent` + the forwarder thread; **observe-only**
  routing (log + broadcast to attached clients) in this phase — no turn start
  yet.
- Tests: fixture `events/poll` scripted responses incl. empty batch, `hasMore`
  drain, `truncated`, `cursor: null`; dedup across a restart via the store;
  reconnect resumes from the persisted cursor.

### P2 — Push delivery

- Add the `events/stream` custom request + `spawn_event_stream_listener`
  (mirror `spawn_list_change_listener`): route `notifications/events/event`,
  apply `active`/`truncated`, advance cursor on heartbeat, terminate on
  `terminated`/error, cancel on shutdown; demultiplex by `subscriptionId`.
- Verify `rmcp`'s generic custom-request path can hold a streaming response over
  Streamable HTTP; if not, document the poll-only limitation and ship push on
  stdio only for now.
- Delivery-mode preference resolution (D1) across the event type's `delivery`
  list.
- Tests: fixture push stream (consent, events, heartbeat, gap via `active`,
  termination); cancel closes the stream; reconnect reopens with the cursor.

### P3 — Event → turn (the last mile)

- Add `SessionCommand::RunEvent` and idle-gated queueing.
- Add the route policy to `handle_mcp_server_event` (`session` vs
  `new-session`), default OFF, with `observe-only` as the safe default.
- Render the occurrence as an untrusted event-attachment with a provenance
  header; bound the payload (D9).
- Tests: an occurrence to an idle session starts exactly one turn; to a busy
  session it queues and runs after the current turn; `new-session` creates and
  runs; `observe-only` starts no turn.

### P4 — Control surface, config polish, docs

- `ClientMessage`/`DaemonMessage` event variants; `choreographr mcp events
  list|add|remove`; the `/mcp events` TUI/GUI/IM surface (list event types,
  create/remove subscriptions, choose route, show last error).
- `autosubscribe` config wiring; `events` block validation + unknown-key
  logging.
- Typed event errors surfaced in the UI (D10).
- Docs: `ARCHITECTURE.md`, `README.md`, rustdoc, justfile `doc_crates`.
- Release note: a `feat(choreo-mcp)`/`feat(choreo-daemon)` commit (or a short
  series) with user-facing prose.

---

## 9. Testing strategy

Per AGENTS.md: unit tests in `src/**/#[cfg(test)]` (no time-based waits);
integration tests in crate-level `tests/it/` (one binary, `#[ignore]`), run via
the nextest aliases.

- **Fixture server** (`choreo-mcp/tests/fixtures/fixture_server.rs`): extend the
  scripted stdio server with an `events` capability and scenarios for
  `events/list` (incl. pagination), `events/poll` (empty, batch, `hasMore`,
  `truncated`, `cursor: null`), `events/stream` (consent, events, heartbeat,
  gap, termination), a legacy peer that never advertises events, and a
  capability-only server (advertises but errors on list). The daemon suite
  reuses the same source via `include!`.
- **HTTP fixture** (`choreo-mcp/tests/it/mcp_http_integration.rs`): if P2 confirms
  a streaming custom request, add an SSE `events/stream` fixture; otherwise a
  documented poll-only note.
- **Unit tests**: value-type conversion, schema normalization/bounds for event
  descriptors, delivery-mode resolution, poll-interval math (`max(nextPollMs,
  floor)`), dedup-ring behavior, capability detection against raw payloads,
  error-code mapping, config parsing (`events` block + unknown keys).
- **Integration tests**: end-to-end poll (fixture → sink → `DaemonCommand` →
  session turn), push on stdio, cursor resume across a simulated restart, reconnect
  re-subscribe, unsubscribe stops delivery, `observe-only` starts no turn.
- **Determinism**: poll timers use injectable values in unit tests; integration
  tests may use a bounded marker poll but no unbounded sleeps. Reuse the
  existing 120 s watchdog.

---

## 10. Security & trust model

1. **Untrusted payloads.** Event `data` is external text with the same
   prompt-injection surface as tool results; bounded, provenance-labelled,
   rendered as data, never as instructions (D9).
2. **Action-time authorization.** Receiving an event grants no privilege; any
   tool the resulting turn calls goes through normal authorization,
   cancellation, and the tool policy.
3. **No new inbound surface (first cut).** Poll and push make no inbound
   connection; webhook — the only mode that would — is deferred to the
   proxy pattern (D1). This is the single biggest reason to start with
   poll/push.
4. **Bounded fan-in.** Per-server and per-session subscription caps, per-
   subscription in-flight caps, and a bounded cursor/dedup store, so a hostile or
   chatty server cannot exhaust memory or flood a session.
5. **Schema safety.** Event `inputSchema`/`payloadSchema` get the same treatment
   as tool schemas: JSON-object enforced, size/depth bounded, no network `$ref`
   dereference (already the client default).
6. **Least privilege.** A stdio event source inherits the server subprocess's
   existing authority (unchanged); the event payload is not a channel for
   privilege escalation because the turn's tools are authorized as usual.
7. **Secrets.** Server `headers`/`env` remain redacted in logs and
   `session_inspect`; the cursor/dedup store holds no secrets.

---

## 11. Configuration & control surface

Config: the `events` block on a server entry (§5 D5). Control surface:

- CLI: `choreographr mcp events list [slug]`, `choreographr mcp events add <slug>
  <name> --arg k=v … [--route session|new-session] [--session N]`,
  `choreographr mcp events remove <slug> <name>`.
- TUI/GUI/IM: `/mcp events` (list event types per server; create/remove
  subscriptions; pick delivery mode and route; show last error / delivery
  health).
- Protocol: `ClientMessage::{McpEventList, McpEventSubscribe, McpEventUnsubscribe}`
  ⇄ `DaemonMessage::{McpEventList, McpEventSubscribed, McpEventUnsubscribed,
  McpEventError}`.

A subscription created from the surface defaults to **observe-only**; starting
turns requires an explicit route choice.

---

## 12. Out of scope / future work

- **Server / producer role** (expose choreographr's own events to ChatGPT or
  another agent): the unbuilt MCP-server-role project. MCP Events adds a
  `server/discover` surface, `events/*` handlers, Standard Webhooks signing,
  SSRF-hardened outbound delivery, endpoint verification, and durable TTL-backed
  subscription storage — plus a public HTTPS ingress. A separate architecture
  project, not a client feature.
- **First-class webhook client mode**: needs an inbound https endpoint + Standard
  Webhooks verification. Deferred to the forward-proxy pattern (§5 D1); a native
  receiver is a separate security-reviewed component if ever justified.
- **Replay beyond the server's offer**: durable/guaranteed delivery is the
  server's contract; the client resumes from cursors and reports `truncated`.
- **`events/poll`/`events/stream` server SDK concerns** (lease tables, emit
  buffers, `on_subscribe` hooks): the server author's responsibility.
- **Event-bound prompts** (draft "future work") and folding
  `resources/subscribe`/`list_changed` into events (draft open questions):
  revisit only if servers demand them.

---

## 13. Risks & mitigations

| Risk | Mitigation |
|---|---|
| The extension is a draft; the capability location and error codes may change | Isolate all wire shapes in `engine/convert.rs` + `protocol.rs`; tolerate an unknown/absent capability; treat codes by best-effort mapping (D10). No daemon-facing type depends on the wire form. |
| `rmcp` does not type the extension, and its `ServerCapabilities` drops the `events` key | Use the generic `CustomRequest`/`CustomResult`/`CustomNotification` path (D3) and raw capability capture (D4); a documented helper to remove when `rmcp` adds typed support. |
| `rmcp`'s generic custom-request path may not support a streaming response over Streamable HTTP | P2 verifies it early; if unsupported, ship push on stdio only and keep poll as the HTTP mode, with the limitation documented (and reported upstream). |
| The event→turn mapping floods a session or starts unwanted work | Default OFF; `observe-only` default for surface-created subscriptions; idle-gated FIFO queueing; per-session in-flight caps (D8/D9). |
| A hostile server floods occurrences or ships an oversized payload | Bounded payloads, per-subscription in-flight caps, per-server/per-session subscription caps, best-effort (drop-not-block) sinks (D9). |
| Cursor/dedup state is lost or corrupt | Fail-closed reads (missing ⇒ subscribe from now); atomic owner-only writes; the store holds no secrets (D7). |
| Webhook expectations from ChatGPT interop work | Explicitly documented as proxy-mediated/deferred, with the forward-proxy pattern described; the producer role is called out as separate (§12). |
| Reconnect drops subscriptions | Subscriptions re-established from the persisted store with cursors on every rebuilt engine, exactly as list-change re-subscribes today (D6). |

---

## 14. Open questions

1. **Event→work mapping (D8).** Dedicated "events session" per server vs an
   arbitrary `targetSessionId`? Default `new-session` creates a dedicated
   session; confirm before P3.
2. **Delivery preference (D1).** Poll-first (lowest footprint) vs push-first
   (lowest latency) as the `auto` default. Lean poll-first; revisit with a real
   server.
3. **Cursor store location (D7).** A per-server JSON file under the data dir vs a
   table in `state.redb`. Lean JSON file (small, separate, easy to inspect);
   revisit if volume grows.
4. **Push on Streamable HTTP (P2).** Does `rmcp`'s generic path stream? Decides
   whether P2 ships both transports or stdio-only.
5. **Webhook proxy guidance.** Document the forward-proxy pattern as the
   supported path, or leave it to the draft's doc? Lean: a short `README.md`
   note, no client code.

---

## 15. Definition of done

- [ ] A server advertising `capabilities.events` has its event types listed via
      `events/list` and shown in `mcp status`/`session_inspect`; a server without
      the capability (or a legacy peer) is untouched.
- [ ] **Poll** subscriptions deliver occurrences end-to-end into the daemon, with
      dedup, cursor persistence, `hasMore`/`truncated` handling, and resume across
      a daemon restart.
- [ ] **Push** subscriptions hold an `events/stream` (stdio at minimum), route
      `notifications/events/*`, advance the cursor on heartbeats, and terminate
      cleanly on `terminated`/cancel; reconnect reopens with the cursor.
- [ ] An occurrence can start a turn per the route policy (default OFF;
      observe-only default), rendered as untrusted, bounded data; a busy session
      queues rather than interleaves.
- [ ] Subscriptions are manageable over `/mcp events` + `choreographr mcp events`
      and cleaned up on shutdown.
- [ ] Caps and bounds enforced and tested (payloads, subscriptions,
      in-flight); typed event errors surfaced in the UI.
- [ ] Hermetic fixture coverage for the capability, `events/list`, poll, push,
      and the event→turn path; `just pre-commit` green.
- [ ] `ARCHITECTURE.md`, `README.md`, and rustdoc updated; release notes written
      from the commit messages.
- [ ] **This plan document is deleted** once implemented; no source, doc,
      comment, or commit message references it.

---

## 16. References

- ChatGPT plugin docs — MCP Events:
  `https://developers.openai.com/plugins/build/mcp-events`
- MCP Events design sketch (draft):
  `https://github.com/modelcontextprotocol/experimental-ext-triggers-events/blob/main/docs/design-sketch-proposal.md`
- Standard Webhooks specification:
  `https://github.com/standard-webhooks/standard-webhooks/blob/main/spec/standard-webhooks.md`
- The 2026-07-28 MCP specification:
  `https://modelcontextprotocol.io/specification/2026-07-28`
- In-tree context: `docs/plans/mcp-modernization.md` (§13 lists the MCP-server
  role as future work); `choreo-mcp/src/engine/mod.rs`
  (`spawn_list_change_listener`); `choreo-daemon/src/server/core.rs`
  (`spawn_mcp_list_change_forwarder`).
