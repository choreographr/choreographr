# Plan: MCP modernization — stateless protocol (2026-07-28) and a first-class client

**Status:** **M1 implemented — `mcp` ships by default, P6 landed (commit
`0a85184`), and the official conformance suite now runs both protocol eras
(2025-11-25 and 2026-07-28) in CI with a committed baseline.**
Commits: `ab3dc2e` (hardening), `976edf6` (the rmcp engine swap), `9f6209a`
(Streamable HTTP), `274f736` (progress/MRTR/resources), `1d7d239` (stdio frame cap
+ concurrency cap), `15aa3ff` (`subscriptions/listen` + registry hot-swap),
`f037383`/`8d27006`/`a348c89` (P5: name hygiene, config, surfaces, logs),
`0a85184` (D9 flip + P6 hardening), and the 2026-07-28-era conformance run. What
remains is **post-ship**: OAuth (P3) and the fast-follows. See §1.2 and §7.
**Lifecycle:** this file is **deleted once the plan is fully implemented**. Nothing
written during implementation may reference it — rustdoc, `ARCHITECTURE.md`,
`README.md`, release notes, and commit messages must stand on their own, because a
reference to this plan would go stale the moment it is removed.
**Date:** 2026-10-03
**Targets:** `choreo-mcp` (protocol engine), `choreo-daemon` (`src/mcp/` manager +
`McpToolWrapper`), `choreo-tui` / `choreo-client-core` (later phases: `/mcp` control
surface), root `Cargo.toml` + `Cargo.lock` (new dependencies; `mcp` moves into
the default feature set for M1 — D9).
**Touches (when implemented):** `choreo-mcp` (rewritten around `rmcp`; new
`runtime.rs` / `session.rs` / `engine.rs` / `config.rs` / `stdio.rs` / `naming.rs`
modules landed through P5; `auth.rs` and the OAuth glue are post-ship),
`choreo-daemon` (`src/mcp/{mod,config,tool}.rs` and `src/daemon.rs` cancel
plumbing landed through P1–P5; `src/daemon/open.rs`, `src/server/core.rs` carry
the registry swap; the CLI lives in `cli.rs`), `choreo-tui` (`/mcp` status +
reconnect landed; sign-in post-ship), `choreo-proto` (the
`McpStatusRequest`/`McpReconnect` client request and `McpStatus` reply landed in
P5), the justfile (`doc_crates` stays current), `ARCHITECTURE.md`, `README.md`,
`packaging/`.

> **TL;DR.** The MCP specification's current revision is **2026-07-28**, a
> *stateless* protocol: the `initialize` handshake is gone, every request carries
> its protocol version and capabilities in `_meta`, servers advertise themselves
> through `server/discover`, server-to-client interaction happens through
> Multi Round-Trip Requests (MRTR) instead of server-initiated requests, and the
> Streamable HTTP transport dropped sessions, the GET stream, and resumability.
> At plan time, Choreographr's client (`choreo-mcp`) spoke the **2024-11-05** era:
> a hard-coded handshake, stdio only, tools only, no pagination, no cancellation,
> no resources/prompts, no HTTP servers, no OAuth, no output-limit or restart
> handling — and the daemon serialized every call to a server through one
> `Mutex<McpClient>`. The plan: replace the hand-rolled protocol engine with the
> official Rust SDK **`rmcp` 3.5** behind a blocking facade (one sidecar tokio
> runtime, one dispatcher thread per server — the same sanctioned pattern
> `choreo-content` and `choreo-blockchain` already use), negotiate protocol eras
> per the spec's backwards-compatibility rules (discover-first on stdio with
> fallback), grow the config to the de-facto `mcpServers` schema
> (`url`/`headers`/`oauth`/`timeout`/`cwd`/`${ENV}` expansion), and then wire the
> features into the daemon: real image attachments, `structuredContent`,
> subscriptions-driven tool-list refresh, progress → streaming chunks,
> cancellation, OAuth sign-in, tool-name hygiene, bounds, and the official
> conformance suite in CI.
>
> **Update (2026-10-04):** P0–P6 and D9 are implemented — the client negotiates
> the stateless and legacy eras on stdio **and** over Streamable HTTP behind the
> dispatcher facade, with cancellation, restart, pagination, content mapping,
> live tool-list refresh, bounds, tool-name hygiene, config layers, a `/mcp`
> status surface + `choreographr mcp` CLI, per-server logs, and the official
> conformance suite running both protocol eras in CI. **M1 is complete**; what
> remains is **post-ship**: OAuth (P3) and the fast-follows. See §1.2 and §7.

---

## Table of contents

1. [Motivation & evidence](#1-motivation--evidence)
2. [The 2026-07-28 protocol, as it affects a client](#2-the-2026-07-28-protocol-as-it-affects-a-client)
3. [How the agents in `~/agents` implement MCP](#3-how-the-agents-in-agents-implement-mcp)
4. [Gap analysis: choreographr today vs spec vs peers](#4-gap-analysis-choreographr-today-vs-spec-vs-peers)
5. [Design decisions](#5-design-decisions)
6. [Target architecture](#6-target-architecture)
7. [Work breakdown](#7-work-breakdown)
8. [Testing strategy](#8-testing-strategy)
9. [Security & trust model](#9-security--trust-model)
10. [Configuration & control surface](#10-configuration--control-surface)
11. [Documentation deliverables](#11-documentation-deliverables)
12. [Risks & mitigations](#12-risks--mitigations)
13. [Out of scope / future work](#13-out-of-scope--future-work)
14. [Open questions](#14-open-questions)
15. [Definition of done](#15-definition-of-done)

---

## 1. Motivation & evidence

### 1.1 Starting point (before P0/P1)

The table below describes the client as it stood when this plan was written; it is
retained as the rationale for the phases. P0/P1 replaced most of it — §1.2 lists
what landed.

`choreo-mcp` is a library-only crate (1,330 lines including tests) consumed only by
`choreo-daemon` behind the `mcp` cargo feature (off by default). Its whole surface is
a stdio child process speaking JSON-RPC 2.0:

| Area | Current behavior | Where |
|---|---|---|
| Handshake | Hard-coded `initialize` with `protocolVersion: "2024-11-05"`, `clientInfo: {name: "choreographr", version: "0.1.0"}`, capability `tools.listChanged: true`; then `notifications/initialized` | `choreo-mcp/src/protocol.rs::make_initialize_request`, `client.rs::spawn` |
| Transport | Stdio only. Reader thread + stderr thread, process-group SIGKILL on shutdown, bounded joins (5 s). **Unbounded** stdout line length and **unbounded** response/notification channels | `choreo-mcp/src/transport.rs` |
| Methods | `tools/list` (no `cursor` pagination), `tools/call` | `choreo-mcp/src/client.rs` |
| Notifications | Drained and logged, then discarded (`recv_response` drains before waiting) | `transport.rs::recv_response` |
| Timeouts | Fixed: 10 s initialize, 10 s list, 60 s call; no per-server config | `client.rs` constants |
| Concurrency | One `Arc<Mutex<McpClient>>` per server shared by all its tools; a call holds the lock for up to 60 s, serializing all calls to that server **and blocking shutdown** | `choreo-daemon/src/mcp/mod.rs`, `tool.rs::call_with_args` |
| Lifecycle | All enabled servers spawn (in parallel threads) during `DaemonState::open` and are joined before open returns; no restart on crash — a dead server is dead for the daemon's lifetime | `choreo-daemon/src/daemon/open.rs:153`, `mcp/mod.rs::from_config` |
| Content mapping | Text joined with `\n`; image rendered as the literal string `[Image: <mime> (<bytes>)]` (**not** attached as a vision image even though `image_tx` exists); resource rendered as `[Resource: …]`; `structuredContent` ignored; `isError` dropped on the postcard path | `tool.rs::mcp_result_to_text_parts`, `execute_postcard` |
| Registry integration | Group `mcp/<slug>`, tool `mcp/<slug>/<tool>`, description `[MCP <slug>] <desc>`; dynamic group registered at startup; `output_schema()` hard-codes `{"type":"string"}` | `tool.rs`, `tools/mod.rs::register_dynamic*` |
| Config | `<config>/choreographr/mcp_servers.json` with `{command, args, env, enabled, auto_load}`; no `url`, `headers`, `cwd`, `timeout`; `auto_load` is parsed then ignored | `choreo-daemon/src/mcp/config.rs` |
| Policy | `ToolPolicy::Mobile` never spawns MCP; feature `mcp` off by default; a plain build has a no-op stub | `daemon/open.rs`, `mcp/mod.rs` stub |
| Tests | Two `#[ignore]` integration tests that `npx -y @modelcontextprotocol/server-everything` (Node + network required); unit tests cover serialization only | `choreo-mcp/tests/it/`, `choreo-daemon/tests/it/` |

Doc comment in `choreo-mcp/src/lib.rs` claimed the crate spoke "over a transport
(stdio child process or HTTP)" — there was no HTTP path. Several declared error
variants (`ToolNotFound`, `InvalidParams`) were never constructed. Both were fixed
in P0.

### 1.2 Implementation progress (P0–P2, P4–P5, P6, D9)

**P0** (`ab3dc2e`) landed the correctness and safety fixes against the then-current
hand-rolled engine; **P1** (`976edf6`) replaced that engine with `rmcp` 3.5 behind
the blocking dispatcher facade; **P2** (`9f6209a`) added the Streamable HTTP
transport. Together they closed G1, G3–G7, G9, G11, G14–G17, G19–G20, and G22
(§4) and left G2 (OAuth half), G8–G10, G12, G13, G18, and G21 for the later
phases. What exists now:

- `choreo-mcp` is six modules: `protocol` (daemon-facing value types, the
  empty-schema fallback, `normalize_input_schema`), `config`
  (`McpServerConfig`, `McpProtocolMode::{Auto,Legacy,Modern}`), `runtime`
  (sidecar tokio runtime with `init`/`get`/`handle`/`block_on`), `session`
  (per-server dispatcher thread and the `McpServer`/`McpServerHandle` blocking
  facade), `engine` (the only `rmcp`-coupled module), `error`.
- Protocol eras are negotiated per server: `auto` (default) probes
  `server/discover` and falls back to `initialize` only on a legacy peer or a
  timeout — never on a recognized modern rejection; `legacy` and `modern` pin one
  era (`2026-07-28` is accepted as an alias for `modern`). Both eras are covered
  by the integration suite, including the `auto` fallback.
- Calls run concurrently per server (the dispatcher spawns each call onto the
  sidecar runtime), are deadline-bounded (per-server `timeout`, `DEFAULT_TIMEOUT`
  60 s; request-scoped progress resets the deadline), and are cancellable
  end-to-end: the daemon's session-cancel path calls
  `McpManager::cancel_session` (including for child sessions), the dispatcher
  cancels the matching in-flight calls, and the engine sends
  `notifications/cancelled` so the server can stop cooperatively.
- A dead transport is rebuilt under a bounded restart policy: 3 attempts,
  `500 ms · 2^(n-1)` backoff capped at 60 s, reset on success. Startup failures
  are terminal for that server (it is dropped), so a bad config never retries in
  a loop.
- `tools/list` follows `nextCursor` (bounded by the server timeout); a tool's real
  `outputSchema` is advertised; `structuredContent` lands in
  `ToolOutput.result_json`; images attach through the daemon image pipeline;
  audio/blob resources and resource links are described rather than dropped;
  text is truncated at 256 KiB with an explicit marker; `isError` survives the
  JSON, streaming, and postcard paths.
- Remote servers (P2): `mcp_servers.json` gained `url`, `headers`, and
  `transport` (`auto` infers HTTP from `url` and stdio from `command`; both or
  neither warns and skips the server), with `${VAR}` expansion in `env`/`headers`
  values (an unset variable expands to empty, with a warning). The engine
  connects over rmcp's `StreamableHttpClientTransport` with an injected `reqwest`
  (rustls) client — connect timeout from the server's request timeout, idle
  pooling and redirects disabled — validates config headers up front (reserved
  MCP headers are rejected), bounds SSE events at 16 MiB, retries a transient
  connect (`408`/`429`/`5xx`; 3 attempts, 500 ms·2^(n-1) capped at 60 s), and
  rejects a 2024-11-05 HTTP+SSE endpoint with a typed `UnsupportedTransport`.
- Startup is bounded by a 2 s budget for the whole batch (`STARTUP_BUDGET`); a
  server that misses it is logged and skipped, so a hung server cannot stall
  `DaemonState::open`.
- The test suites are hermetic and fixture-driven (no Node/npx, no network):
  `choreo-mcp`'s scripted stdio server covers both eras, `auto` fallback,
  crash-on-call, garbage lines, oversized lines, a rejected `initialize`,
  structured content, and cancellation; a local `TcpListener` HTTP fixture
  covers JSON and SSE responses, generated-header validation, the `auto`
  fallback, a retried `503`, and the HTTP+SSE rejection; the daemon's suite
  shares the stdio fixture source via `include!`.
- Server→client notifications, MRTR, and resources (P4, `274f736`): a real
  `ClientHandler` replaces the bare service config, so server notifications
  reach the client — `notifications/progress` is forwarded to the in-flight
  call's chunk sink (rate-limited via `PROGRESS_MIN_INTERVAL`, best-effort
  `try_send`), `notifications/message` goes to tracing, and tool/resource
  list-changed events surface as `ServerEvent`s. An `input_required`
  `tools/call` result drives the bounded MRTR loop: elicitation and roots are
  declined and the call is re-issued with `inputResponses` plus the echoed
  opaque `requestState`; sampling is refused with a clear error; a peer that
  keeps asking fails cleanly instead of hanging. Resources are readable
  (`list_resources`/`read_resource`, paginated, behind a `supports_resources`
  capability flag), and a server that declares the capability gets
  `mcp/<slug>/list_resources` and `mcp/<slug>/read_resource` catalogue tools.
  Tests cover progress→chunk (client and daemon), MRTR, resources, and a
  fixture that records the server-observed `notifications/cancelled`. The
  subscription half landed in `15aa3ff`: a stateless server that declares
  list-changed gets a `subscriptions/listen` stream (rmcp's `Peer::listen`),
  each `toolsListChanged`/`resourcesListChanged` is forwarded to the daemon,
  and the command loop rebuilds the whole catalogue (`build_tool_registry` →
  `register_all`) into the shared `Arc<ArcSwap<ToolRegistry>>` — a live session
  sees the refreshed `mcp/<slug>` group on its next request, no restart.
- Bounds (P6, `1d7d239`): the stdio transport is built over a crate-local
  `BoundedLineReader` — a single newline-delimited frame over 8 MiB
  (`MAX_STDIO_FRAME_BYTES`) fails the read and drops the connection, which the
  restart policy can rebuild; the write side and the JSON-RPC framing remain
  rmcp's. The dispatcher admits at most `maxConcurrentCalls` (default 4,
  configurable, `0` clamped to 1) in-flight calls per server and queues the
  excess, starting each as a slot frees and waking on commands or completions
  via `select!` (no polling); a session cancel still reaches queued calls.

- P5 (`f037383`, `8d27006`, `a348c89`): tool names are sanitized to the
  provider-safe alphabet and capped at 64 chars with a stable hash suffix on
  collision; a server entry gains `cwd` (with `~` expansion) and
  `disabledTools`; a project config layer (`<root>/.choreographr/mcp_servers.json`)
  overrides the user file per slug; a 401/403 at connect or on a request maps to
  an actionable `McpError::AuthRequired` naming the static-token and OAuth
  options; `McpManager::status`/`reconnect` back a `/mcp` status surface and a
  `choreographr mcp list|add|remove|reconnect` CLI; `session_inspect` reports
  each server's state and tool count; and each stdio server's `stderr` is
  captured to a size-capped per-server log file.

- D9 + P6 (`0a85184`): the `mcp` feature is in the default set at both layers
  (the daemon's `default` and the root package's dependency), so a plain build
  links `choreo-mcp`/`rmcp`/`reqwest` and the sidecar runtime — measured at
  ~+11 MB (~+21.7%, 50.9 → 61.9 MB) on a local release build; the iOS embedded
  daemon keeps `default-features = false`, and the daemon's `mcp` module still
  degrades to the no-op stub. Every server-supplied bound is enforced: tool
  schemas ≤ 256 KiB and ≤ 32 nesting levels (an over-bounds tool is dropped),
  a catalogue capped at 1024 tools, an 8 MiB stdio frame cap, a bounded SSE
  reconnect policy, rate-limited server logging notifications, and a per-server
  `maxRestarts` (default 3; `0` disables reconnect). The official
  `@modelcontextprotocol/conformance` client suite runs in CI
  (`scripts/mcp-conformance.sh`, pinned `0.2.0-alpha.12`, committed
  expected-failures baseline) against **both** protocol eras — the stateful
  2025-11-25 wire and the stateless 2026-07-28 wire — as a CI matrix; a
  "stubborn server" fixture proves `shutdown_all` stays bounded even when a
  server ignores stdin EOF; and the config parser and tool-name sanitizer
  gained fuzz-style property tests.
- Post-M1 fast-follow: `mcp reload` landed (`850cb4e`, protocol v9) — the daemon
  re-reads the user+project config and reconciles the running server set
  (connect added, disconnect removed, rebuild changed or previously-failed;
  an unchanged server keeps its live connection), then rebuilds the catalogue.
  Exposed as `ClientMessage::McpReload` ⇄ `DaemonMessage::McpReloaded` /
  `McpReloadFailed`, `/mcp reload` in the TUI/GUI/IM, and
  `choreographr mcp reload` over the running daemon's socket. A malformed
  config is a hard error with no state change.

Deltas the plan now tracks (detailed in §7): the config key `exposure` is not
implemented (it defers with the deferred-tool-loading work); `auto_load` was
removed (reported as an unknown key) rather than mapped to a deferred exposure;
`Retry-After` is not honored by the HTTP retry policy (upstream-blocked, P2
residuals); OAuth is not implemented — deferred to post-ship (P3), with static
tokens via config `headers` as the M1 credential path. The conformance suite
runs both protocol eras in CI; every non-auth client scenario passes on both
wires — the 2026-07-28 run additionally covers `server/discover`-era
`request-metadata` (`_meta`), the SEP-2243 standard/custom header mirroring and
malformed-tool rejection, SEP-2106 network-`ref` non-dereferencing and JSON
Schema 2020-12 preservation, and the SEP-2322 MRTR request-state echo — and the
baseline holds only the OAuth `auth/*` scenarios (plus, on the legacy wire, the
un-advertised elicitation scenario). The suite was bumped from the 0.1 line to
`0.2.0-alpha.12` because only the 0.2 line can express the 2026-07-28 era.
Everything else — D9, the P6 bounds and caps, fuzzing, supply-chain, security,
and release verification — landed in `0a85184`.

### 1.3 The protocol delta

The specification's revisions are `2024-11-05`, `2025-03-26`, `2025-06-18`,
`2025-11-25`, and the current **`2026-07-28`**. At plan time Choreographr sat
three revisions behind, which mattered because:

- Modern servers (the 2026-07-28 era) **reject `initialize`** with
  `UnsupportedProtocolVersionError` (`-32022`) or method-not-found. A client that
  only handshakes cannot talk to them at all.
- The fastest-growing deployment shape is **remote Streamable HTTP** servers with
  OAuth (Sentry, Slack, GitHub, Figma, …). Choreographr cannot reach any of them.
- The spec's security posture has moved on: `inputSchema`/`outputSchema` are
  bounded JSON Schema 2020-12, `$ref` network dereferencing is forbidden by
  default, client capabilities are validated per request
  (`MissingRequiredClientCapabilityError`), and Streamable HTTP mirrors routing
  fields into validated headers (`HeaderMismatch`, `-32020`).
- Feature coverage (resources, prompts, `structuredContent`, progress,
  cancellation, `subscriptions/listen`, tasks extension) is what makes an MCP
  client *usable* with real servers, not just with the `echo` reference server.

### 1.4 Why now

Nothing is blocked on this (the feature is off by default and the current client
works against legacy stdio servers), but every month of drift makes the eventual
migration larger and the interop matrix wider. The peer survey in §3 shows the
Rust-portable answer already exists (`rmcp`), is maintained by the MCP project
itself, and is what both Rust peers in `~/agents` (codex, goose) already build on.

---

## 2. The 2026-07-28 protocol, as it affects a client

Authoritative sources: the [2026-07-28 specification](https://modelcontextprotocol.io/specification/2026-07-28),
its [key changes](https://modelcontextprotocol.io/specification/2026-07-28/changelog),
[`server/discover`](https://modelcontextprotocol.io/specification/2026-07-28/server/discover),
[stdio transport](https://modelcontextprotocol.io/specification/2026-07-28/basic/transports/stdio),
[Streamable HTTP](https://modelcontextprotocol.io/specification/2026-07-28/basic/transports/streamable-http),
and [tools](https://modelcontextprotocol.io/specification/2026-07-28/server/tools).

### 2.1 Statelessness

> The protocol is **stateless**: all information needed to process a request is in
> the request itself. Servers **MUST NOT** rely on prior requests on the same
> connection (capabilities, protocol version, client identity). Clients
> **SHOULD NOT** use a task/thread/conversation as the lifetime boundary for a
> stdio process. Cross-request state (e.g. a basket, a browser context) is an
> explicit server-minted handle passed as an ordinary tool argument.

Consequences for choreographr: a session is not a connection. One long-lived
child process per configured server is correct; the client must never treat
"handshaken once" as a licence to skip per-request metadata.

**Connection lifetime is separate from config/visibility scope.** The two axes
are orthogonal:

- **Connection lifetime.** An *unshared* (`shared: true`, the default) server has
  one pooled connection per resolved config, shared across every session that
  references it — a project's server is pooled by `(project_root, slug)` and a
  daemon-tier server by `slug`. A session is never the connection lifetime
  boundary, per the spec. The one explicit escape hatch is `shared: false`: a
  stateful server that must not share state between sessions gets a private
  connection *per session that uses it* — the modern-era way to scope state (the
  old stdio ``as a conversation`` lifetime is still not restored).
- **Config/visibility scope.** *Which* servers a session can see is a separate
  question from how their connections are pooled: the daemon-tier `mcp.json` is
  visible to every session; a session's own project `.mcp.json` (found by walking
  up from its working directory) is visible only to that session, and never to
  any other project's. A project server replaces a daemon-tier server of the same
  slug *for that session only* (replace by group, not union).

See §5 D13 for the settled design of the two-tier, per-session MCP config and
the trust store that gates a project's servers.

### 2.2 Per-request metadata (`_meta`)

Every client request **MUST** carry:

| Key | Required | Notes |
|---|---|---|
| `io.modelcontextprotocol/protocolVersion` | yes | e.g. `"2026-07-28"` |
| `io.modelcontextprotocol/clientCapabilities` | yes | capability object relevant to this request |
| `io.modelcontextprotocol/clientInfo` | no (SHOULD) | `{name, version}` |
| `io.modelcontextprotocol/logLevel` | no | minimum level for `notifications/message` |

Every result **SHOULD** carry `io.modelcontextprotocol/serverInfo`. A request
missing a required field is malformed: `-32602` (HTTP 400). A server needing an
undeclared capability returns `MissingRequiredClientCapabilityError` (`-32021`)
listing `data.requiredCapabilities`. `clientInfo`/`serverInfo` are self-reported
and **must not** drive client behavior or security decisions.

### 2.3 `server/discover`

Servers **MUST** implement it. A client MAY call it up-front; on stdio it **SHOULD**
probe with it before any other request when it supports both eras. Response
(`DiscoverResult`): `supportedVersions`, `capabilities`, `_meta.serverInfo`,
optional `instructions`, plus cache fields (`ttlMs`, `cacheScope`).

The stdio fallback rules are explicit and must not be keyed to one error code:

- `DiscoverResult` → modern; continue with a mutually supported version.
- A recognized modern error (e.g. `UnsupportedProtocolVersionError`) → modern;
  retry with an advertised version; **do not** fall back to `initialize`.
- Any other error, or timeout → legacy; fall back to `initialize`.

### 2.4 Results, `resultType`, and MRTR

- Every result has `resultType`: `"complete"` or `"input_required"`. Absent (from
  an earlier-era server) **MUST** be treated as `complete`.
- The server no longer sends JSON-RPC *requests* for sampling/elicitation/roots.
  It returns an `InputRequiredResult` whose `inputRequests` map names the
  requests; the client gathers the input and **retries the original request**
  (new JSON-RPC id) with `inputResponses` and the opaque `requestState` echoed
  back. `requestState` is server-defined, potentially large, and must be treated
  as opaque.

### 2.5 Transports

**stdio.** Newline-delimited JSON, no embedded newlines, `stdout` never carries
non-MCP output, `stderr` is free-form logging. Cancellation is a
`notifications/cancelled` notification; shutdown is "close stdin, wait, then
kill" (POSIX SIGTERM→SIGKILL, Windows TerminateProcess/Job Objects). Unexpected
exit → the client **SHOULD** restart; because the protocol is stateless, in-flight
requests are simply re-issued.

**Streamable HTTP.** One POST per message to a single endpoint; the server
answers with `application/json` or a per-request SSE stream. Required on every
POST:

- `MCP-Protocol-Version: <version>` (**MUST** match the body).
- `Mcp-Method: <method>` for all requests, `Mcp-Name: <name|uri>` for
  `tools/call`, `resources/read`, `prompts/get`.
- Values outside visible-ASCII header syntax are carried as
  `Mcp-Name: =?base64?<b64>?=` (and the same sentinel for `Mcp-Param-*`).
- Tool parameters annotated `x-mcp-header` in `inputSchema` **MUST** be mirrored
  into `Mcp-Param-<name>` headers; clients **MUST** reject tool definitions whose
  annotations violate the constraints.

Gone since 2025-11-25: `Mcp-Session-Id`, the GET stream, `resources/subscribe`,
`Last-Event-ID` resumability. Cancellation on HTTP = closing the response stream.
Unknown method → 404 + `-32601`; a legacy HTTP+SSE server is distinguished by
GETting the URL and looking for an `endpoint` SSE event (the deprecated
2024-11-05 transport).

**`subscriptions/listen`** replaces the GET stream: a long-lived request whose
response stream carries only the opted-in notification types
(`toolsListChanged`, `promptsListChanged`, `resourcesListChanged`,
`resourceSubscriptions`), acknowledged with
`notifications/subscriptions/acknowledged` and tagged with
`io.modelcontextprotocol/subscriptionId`. Request-scoped notifications
(`notifications/progress`, `notifications/message`) still flow on the request's
own stream.

### 2.6 Tools and content

- `tools/list` is paginated (`cursor`/`nextCursor`), cacheable
  (`ttlMs` + `cacheScope` are required on the result), and servers **SHOULD**
  return tools deterministically.
- `inputSchema` **MUST** be a valid JSON Schema object — never `null`; empty-arg
  tools are `{"type":"object","additionalProperties":false}`. Default dialect is
  2020-12; network `$ref` dereferencing is opt-in and off by default; validators
  should bound depth/sub-schema count/time.
- Content blocks: `text`, `image` (`data` + `mimeType`), `audio`,
  `resource_link`, embedded `resource`, plus `structuredContent` (any JSON value,
  conforming to `outputSchema` when present). `isError: true` marks tool
  execution errors (actionable by the model) vs protocol errors.
- Tool names SHOULD be 1–128 chars of `[A-Za-z0-9_.-]`; name collisions across
  servers are the *client's* problem to disambiguate (prefixing recommended).

### 2.7 Errors, caching, deprecations, extensions

- Error codes: `-32020` HeaderMismatch, `-32021` MissingRequiredClientCapability,
  `-32022` UnsupportedProtocolVersion. `-32002` (resource not found) became
  `-32602`; clients should still accept `-32002` from older servers.
- `icons` may appear on implementations/tools/prompts/resources; consumers must
  treat them as untrusted (HTTPS or `data:` only, no credentials, size caps).
- **Deprecated** (do not adopt): Roots, Sampling, Logging (`logging/setLevel`
  removed; use `stderr`/OTel), the 2024-11-05 HTTP+SSE transport, `includeContext`.
- **Extensions** (opt-in, negotiated): `io.modelcontextprotocol/tasks` (polling
  `tasks/get`, `tasks/update`), Skills over MCP, MCP Apps (inline UI).
- OpenTelemetry trace context (`traceparent`/`tracestate`/`baggage`) rides in
  `_meta` on every request.

---

## 3. How the agents in `~/agents` implement MCP

Survey method: file-level search for modelcontextprotocol/mcp across all 27 agent
repos, then a deep read of every repo with a real implementation. Findings below
are from source at the paths given.

### 3.1 Landscape

| Agent | Lang | Transport(s) | Protocol era | Notable design |
|---|---|---|---|---|
| **codex** | Rust | stdio, Streamable HTTP, in-process, executor-process | **2026-07-28** behind feature flag, `Auto` lifecycle, legacy 2025-06-18 | Uses a pinned git rev of the official SDK (`rmcp`, v3.3.0). `protocol_mode.rs` selects `Legacy` vs `V20260728` once per session; `ClientLifecycleMode::Auto { preferred: [2026-07-28], legacy: Some(2025-06-18) }`. `CODEX_MCP_PROTOCOL_VERSION` env opts a stdio server in. Bounded stdio transport (8 MiB line cap). Retry/redirect/`WWW-Authenticate` handling, OAuth + enterprise-managed auth, MRTR tests (`tests/mcp_2026_mrtr`), elicitation/user-verification plumbing. |
| **fx** | Zig | stdio, Streamable HTTP, legacy HTTP+SSE | **2026-07-28** + 4 legacy versions, selectable per server | The most complete hand-rolled client surveyed (~51k lines in `src/core/mcp/`): explicit era model (`protocol_negotiation.zig`), env override `FX_MCP_PROTOCOL_VERSION`, health state machine, subscriptions, MRTR with bounded `requestState`, auth store, tool search, access policy, docker-run support. |
| **goose** | Rust | stdio, Streamable HTTP, in-process builtins | **2026-07-28** via `rmcp` 3.4.1 `Auto` (falls back to 2025-11-25) | Platform tools are themselves MCP servers compiled in. `extension_manager/` + `mcp_client.rs`; elicitation handler, MCP Apps UI extension, dynamic tool notifications, streamable-HTTP extension manager. |
| **hermes-agent** | Python | stdio, Streamable HTTP, legacy SSE | **2026-07-28** *and* legacy via explicit interop ladder | Wraps the official `mcp` SDK defensively: three negotiated modes (`auto` = initialize-first with modern-only fallback, `stateless` = discover-first, `legacy`); detects "modern-only server rejected initialize" (`-32022`/`-32601`); seeds the `MCP-Protocol-Version` header from the handshake version (not latest); distinguishes Streamable HTTP from SSE by 400/405/406/411; rejects non-MCP 2xx endpoints as non-retryable; backoff reconnects; npx cached-binary detection. |
| **pi** | TypeScript | stdio, Streamable HTTP, in-memory (tests) | 2025-11-25 + older (2026-07-28 explicitly not implemented) | Standalone `@earendil-works/pi-mcp` package (no SDK dependency): OAuth (DCR + CIMD + scope step-up + `iss` validation), exposure modes (`direct`/`deferred`/`codemode`/`hidden`), tool search integration, config layering (user + project, project overrides), `${ENV}` and `!command` expansion, per-request timeout that progress notifications reset, retry of 408/429/5xx, reconnect-on-next-call, tool-name sanitization with hash suffix on collision, `/mcp` and `pi mcp` commands, per-server log file, and CI against the **official `@modelcontextprotocol/conformance` suite** with a committed baseline. |
| **opencode** | TypeScript | stdio, Streamable HTTP | via `@modelcontextprotocol/sdk` 1.29 | `src/mcp/index.ts` (1004 lines): OAuth provider + loopback callback, remote/local connect, per-server headers, resources/prompts, session recovery tests, timeouts. |
| **ironclaw** | Rust | Streamable HTTP (host-mediated egress) | present-era HTTP client | Strongest *security* architecture: a chartered crate split (`contract`/`runtime`/`client`/`jsonrpc`/`discovery`/`egress`/`diagnostics`), tools/list paging with host ceilings, tool-name grammar, description bounding, schema bounds, and a single egress seam where only the host can send bytes. |
| **zero** | Go | stdio, HTTP(s) | 2025-era (uses `Mcp-Session-Id`) | Hand-rolled client with OAuth protected-resource metadata, permissions gating per tool, prompts/resources, redirect policy, non-text content that is *described* rather than dropped, TUI `mcp add` wizard. |
| **jcode** | Rust | stdio | legacy-era | A daemon-wide **shared pool**: M servers total for N sessions (not N×M), per-session `McpHandle` clones, request correlation by id, failed-connect cooldown, 30 s connect-on-call timeout, `shared: false` escape hatch for stateful servers, schema cache. |
| **openwork** | TypeScript | HTTP, in-process app host | modern-aware (`server/discover`) | MCP Apps host (`mcp-app-host.ts`), managed local MCP (`local-managed-mcp.ts`), cloud MCP health/monitoring. |
| **maka-agent** | TypeScript | stdio, Streamable HTTP, SSE, auto | legacy/auto/**2026-07-28** per server | Config `McpProtocolPreference = legacy/auto/2026-07-28`, transport `auto`, OAuth config, desktop + TUI protocol editor. |
| **deepseek-harness** | TypeScript | stdio, Streamable HTTP via ACP bridge | 2025-11-25 + `server/discover` probe | `packages/mcp/{mcp-client,mcp-resources}` + ACP `mcp.ts` bridge. |
| **t3code** | TypeScript | ACP-bridged stdio | 2026-aware (effect-smol `McpServer`) | MCP-over-ACP bridges (`AcpMcpStdioBridge`) and an Effect-native `McpServer` with conformance tests. |
| **turnstone** | Python | HTTP | present-era | Multi-tenant MCP gateway: per-user OAuth (OBO to Entra/Keycloak), admin bulk-revoke API, deferred tool loading (native or BM25 fallback). |
| **OpenMinis** | Python | stdio, HTTP | legacy/current | On-device `minis-mcp-cli` with startup timeout, call args, HTTP reinit. |
| **oar** | TypeScript | stdio **server** | 2025-11-25 + older | Exposes its own subagent orchestration as an MCP server. |
| **buzz** | Rust | stdio **server** | present-era | `buzz-dev-mcp` exposes the harness's dev tools (read/rg/shell/todo/view_image) as an MCP server. |
| **ax** | Go | — | — | MCP servers/registries materialized into workspace setup (API types + roadmap), not a client yet. |
| open-dots, langgraph, mercury-agent, rtk, tau, skills, headlong, herdr | — | — | — | No MCP implementation (mentions only). |

### 3.2 Patterns the best implementations share

1. **Era negotiation is a state machine with an explicit fallback**, probed with
   `server/discover` on stdio and 400-body inspection on HTTP; the fallback is
   never keyed to a single error code (fx, codex, goose, hermes; required by the
   spec).
2. **Per-request metadata is carried on every request** — no client skips
   `_meta` after connecting (fx, codex; rmcp does it structurally).
3. **Multiple transports** with one client core: stdio + Streamable HTTP
   (codex, fx, goose, hermes, pi, opencode, zero), plus in-process transports for
   built-in tool servers (goose) and test doubles (pi's in-memory transport).
4. **The de-facto `mcpServers` JSON is the config interop currency** — a
   superset of Claude/Cursor/Codex fields: `command`/`args`/`env`/`cwd` or
   `url`/`headers`/`oauth`, plus `enabled`, `timeout`, `${ENV}`/`!command`
   expansion (pi, maka, hermes, opencode, zero).
5. **Tool identity is sanitized and collision-proofed** (pi's `mcp__<server>__<tool>`
   + hash suffix; codex/rmcp default naming uses similar schemes).
6. **Large tool sets are not dumped into context**: exposure/deferred loading
   (pi `direct`/`deferred`/`codemode`/`hidden`; turnstone deferred; goose dynamic
   tools; choreographr's own `load_tools` groups are the same idea).
7. **Process supervision**: connect in background, restart on crash, backoff,
   failed-connect cooldown (jcode, pi, hermes, fx, codex).
8. **Everything is bounded**: line length (codex 8 MiB), tool count and schema
   size (ironclaw), request-state depth (fx), pagination page limits.
9. **Cancellation and deadlines are first-class** (codex; rmcp cancel; the spec's
   `notifications/cancelled` and HTTP stream-close semantics).
10. **OAuth is client-side and per-(server, URL, issuer)**: DCR *and* CIMD,
    `iss` validation, scope step-up, token refresh, logout (pi, hermes, opencode,
    codex, fx).
11. **Content fidelity**: text and images pass through to the model; embedded
    resources unwrap; `structuredContent` is preserved; audio/links degrade to
    described placeholders, never vanish (pi `toLlmContent`, zero's non-text
    handling).
12. **Interop is tested, not assumed**: yes-servers/fixture servers in-tree
    (codex's mock servers, goose's `mcp_fixture_server`, ironclaw's
    `mock_mcp_server`), fixture-driven scenario scripts (fx), and the official
    conformance suite with a baseline file (pi).
13. **A user-facing control surface** exists: `/mcp` status, login/logout,
    reconnect, enable/disable, per-server logs (pi, opencode, hermes, maka,
    fx `fx mcp add`).
14. **Security posture**: treat annotations/descriptions/icons as untrusted,
    bound schemas, gate side-effectful tools, keep secrets out of logs, and
    prefer explicit user consent (codex user verification, ironclaw egress seam,
    zero permissions, the spec's own security section).

---

## 4. Gap analysis: choreographr today vs spec vs peers

Severity: **S1** = broken/dead-end behavior a user can hit today; **S2** =
missing capability that blocks real servers; **S3** = robustness/quality;
**S4** = polish. Status is as of `976edf6` (P0–P1); evidence cites the pre-P0 tree.

| # | Gap | Severity | Evidence | Status |
|---|---|---|---|---|
| G1 | Cannot talk to 2026-07-28 (stateless-only) servers at all | S1 | `make_initialize_request` pins `2024-11-05` | **Closed (P1)** — `auto`/`modern` negotiate `server/discover` via rmcp |
| G2 | No HTTP transport; no remote servers, no OAuth | S1 | `transport.rs` is stdio-only | **Partial (P2/P5)** — Streamable HTTP landed; M1 ships with static-token `headers` + an actionable auth-required error (`f037383`), OAuth is post-ship (P3) |
| G3 | Server crash is permanent for the daemon's lifetime | S1 | no restart anywhere in `mcp/` | **Closed (P1)** — bounded restart policy rebuilds a dead transport |
| G4 | One `Mutex` per server serializes calls and blocks shutdown | S1 | `Arc<Mutex<McpClient>>`, `tool.rs` | **Closed (P1)** — per-server dispatcher, concurrent calls, bounded joins |
| G5 | MCP images are never attached to the model (rendered as text) | S1 | `image_tx` ignored; `[Image: …]` placeholder | **Closed (P0)** — base64 decode + image pipeline; placeholder only without a sink/invalid |
| G6 | `inputSchema` default is JSON `null` when a server omits the key; invalid per spec | S2 | `#[serde(default)] input_schema: Value` in `protocol.rs` | **Closed (P0/P1)** — `normalize_input_schema` + rmcp's object-shape enforcement |
| G7 | `tools/list` ignores `nextCursor` → large servers lose tools silently | S2 | `client.rs::list_tools` | **Closed (P1)** — rmcp `list_all_tools` follows cursors |
| G8 | No `subscriptions/listen`; new tools require a daemon restart | S2 | notifications discarded in `transport.rs` | **Closed (P4, 15aa3ff)** — `subscriptions/listen` opens for list-changed servers; the daemon rebuilds the catalogue and swaps it into the shared `Arc<ArcSwap<ToolRegistry>>`, so live sessions see refreshed groups without a restart |
| G9 | No `structuredContent`/`outputSchema` capture into `ToolOutput.result_json` | S2 | `tool.rs` mapping | **Closed (P1)** — `result_json` set; real `outputSchema` advertised |
| G10 | No resources, prompts | S2 | no protocol types | **Partial (P4)** — resource tools (`list_resources`/`read_resource`) landed; prompts remain deferred |
| G11 | No cancellation; a cancelled session leaves the MCP call running | S2 | `ctx.cancelled` never consulted | **Closed (P1/P4)** — cancel wired end-to-end incl. child sessions; the server-observed `notifications/cancelled` test landed in P4 |
| G12 | No progress notifications; `supports_streaming_output` emits one final chunk | S2 | `execute_streaming_json` | **Closed (P4)** — progress forwarded as rate-limited streaming chunks (client + daemon tests) |
| G13 | Unbounded stdout line + unbounded channels (memory DoS from a hostile server) | S2 | `transport.rs` | **Closed (P6, 1d7d239)** — the stdio transport reads through a crate-local `BoundedLineReader`: a frame over 8 MiB fails the read and drops the connection (the restart policy rebuilds it); SSE events were already capped at 16 MiB |
| G14 | No per-server timeouts in config; fixed 60 s | S3 | `client.rs` constants | **Closed (P0)** — `timeout` key + `DEFAULT_TIMEOUT` |
| G15 | `auto_load` parsed but ignored | S3 | `config.rs` | **Closed (P0)** — key removed; unknown keys are logged |
| G16 | `output_schema()` hard-codes `{"type":"string"}` | S3 | `tool.rs` | **Closed (P1)** — the server's real `outputSchema`, else `None` |
| G17 | `isError` dropped on the postcard path; `describe_invocation_json` appends a stray period | S3 | `tool.rs` | **Closed (P0/P1)** — `is_error` on all paths; description returned verbatim |
| G18 | Tool names unsanitized (`mcp/<slug>/<tool>` may exceed provider name limits or contain bad chars) | S3 | `tool.rs::new` | **Closed (P5)** — segments sanitized to `[A-Za-z0-9_-]`, name capped at 64 chars, hash suffix on collision |
| G19 | No startup budget: a hung server can hold up `DaemonState::open` | S3 | `open.rs` joins spawn threads | **Closed (P1)** — 2 s batch budget, stragglers skipped |
| G20 | Integration tests depend on Node/npx + network | S3 | `tests/it/*` | **Closed (P0)** — in-tree fixture server, shared via `include!` |
| G21 | No user surface: no `/mcp`, no status, no login/logout | S3 | nothing in `choreo-tui` | **Closed (P5)** — `/mcp` status + `mcp reconnect <slug>` in the TUI and `choreographr mcp list/add/remove/reconnect` CLI (login/logout is P3) |
| G22 | `lib.rs` claims HTTP support that does not exist; unused error variants | S4 | `lib.rs`, `error.rs` | **Closed (P0)** |

---

## 5. Design decisions

### D1 — Adopt `rmcp` (official Rust SDK) as the protocol engine — 3.5.0, crates.io

Alternatives considered:

1. **Extend the hand-rolled client** (the fx approach). Rejected: full
   coverage now needs MRTR + `requestState`, `subscriptions/listen`, SSE
   parsing, header mirroring with the base64 sentinel, OAuth (DCR + CIMD +
   `iss` validation + refresh), caching semantics, and per-era behavioural
   switching — that is a 5–10k-line SDK with an upstream spec to track monthly.
   fx's version is ~51k lines and is a full-time project.
2. **Keep stdio-only, add just stateless handshake support.** Rejected: leaves
   G2 (remote servers) and G5–G13 untouched, and duplicates what `rmcp` gives.
3. **Write a thin client on another language's SDK via subprocess.** Rejected:
   process and dependency overhead; wrong direction for a Rust workspace.

`rmcp` 3.5.0 (crates.io, Apache-2.0, MSRV 1.88 — workspace floor is 1.99) is
maintained by the MCP project, supports all of 2026-07-28 (discover lifecycle,
MRTR, subscriptions, tasks, elicitation, request-state key rotation, OAuth with
CIMD support, Streamable HTTP client/server, stdio child-process transport,
in-process and worker transports), and is already the base for **both Rust peers**
in the survey (codex pins a git rev of it; goose uses the crates.io release).
Codex's git pin exists for unreleased enterprise-auth work only; we do not need
it.

Feature set is enabled **per phase** with `default-features = false`. P1 enabled
`client` + `transport-child-process` (plus `process-wrap` for the process group);
P2 enabled `transport-streamable-http-client-reqwest` and declared `reqwest` 0.13
directly in the workspace — feature-unified with rmcp's own copy and with the
alloy stack that already used 0.13 (see D11); P3 (post-ship) adds `auth`; P4 adds
`elicitation`/`request-state` only if the pipeline drives them. `tokio`,
`process-wrap`, and `reqwest` were promoted to `[workspace.dependencies]` in
P1/P2.

### D2 — Keep the daemon thread-only: sidecar runtime + per-server dispatcher thread

`rmcp` is async; the daemon is not. The workspace already sanctions this exact
bridge twice: `choreo-content::runtime` and `choreo-blockchain` own a process-wide
tokio runtime used only to drive subxt, while the daemon calls blocking
`execute_*` functions. `choreo-mcp` gets the same treatment, plus one
**dispatcher thread per server** so that:

- calls no longer serialize behind a per-server `Mutex` (G4),
- cancellation can be delivered as a channel message while a call is in flight
  (G11),
- a crashed server is detected on the dispatcher thread and can be restarted
  (G3),
- the daemon's blocking contract (`execute_*`-style functions) is unchanged.

The dispatcher owns the `RunningService` and a current-thread runtime; commands
cross a crossbeam channel (`Call`, `List`, `Cancel`, `Shutdown`); replies cross a
per-call channel. This satisfies the channel policy (no shared mutable state
beyond the existing exceptions; no sleep-poll loops).

### D3 — Per-server protocol mode `legacy | auto | modern`, default `auto`

`auto` = spec-standard: probe `server/discover` first (stdio), fall back on
"any other error or timeout", never fall back on a recognized modern error. This
matches `rmcp`'s `ClientLifecycleMode::Auto` (10 s probe timeout) with
`preferred_versions: [2026-07-28]` and a legacy fallback of `2025-11-25` (goose's
choice). `legacy` forces `initialize` (needed for testing and for a handful of
legacy servers that mis-handle pre-init traffic); `modern` refuses legacy servers.
Config key: `"protocol": "auto" | "legacy" | "2026-07-28"` (maka-agent's shape,
plus `auto` as default).

### D4 — Config: extend the MCP config in place

The same `mcpServers` top level, extended in place. D13 later renamed the
daemon-tier file to `<config>/choreographr/mcp.json` and replaced the
globally-scoped project overlay with a per-session `<PROJECT_ROOT>/.mcp.json`
tier (gated by the trust store). Landed: `timeout` and `protocol` (P0/P1); `url`,
`headers`, `transport` (`auto` infers HTTP from `url` and stdio from `command`;
both or neither warns and skips the server) and `${VAR}` expansion in
`env`/`headers` values (P2; an unset variable expands to empty with a warning);
`cwd` and `disabledTools` (P5; a leading `~` in `cwd` is expanded); a per-server
`shared` key (D13, default `true`, `false` = a private connection per session);
and a per-session project tier (D13). Unknown keys are collected, logged, and
ignored — never fatal; `auto_load` was
removed in P0 and is now reported like any other unknown key.

Still to come: `oauth` is post-ship (P3) and `exposure` moves with the
deferred-loading work. When
`exposure` lands, a legacy `auto_load: false` can be mapped to
`exposure: "deferred"`; there is no such mapping on purpose today, because
deferred registration does not exist yet.

### D5 — Tool identity stays `mcp/<slug>/<tool>`

Persisted sessions store group names; renaming would strand them. Rules:
sanitize each segment to `[A-Za-z0-9_-]`, cap the full name at the provider limit
(64 chars for OpenAI; use 64 as the safe ceiling), append `-<6-hex hash>` when
sanitization or truncation collides. Description prefix `[MCP <slug>] ` is kept
(stable prompt text). `title`/`icons`/`annotations` are captured into group
metadata for later UI use but never fed to the model as instructions (untrusted).

Status: the name format and description prefix landed in P0/P1; the sanitizer,
64-char cap, and collision hash landed in P5 (`f037383`, G18). `title`/`icons`/
`annotations` capture is not yet implemented (post-ship UI work).

### D6 — Concurrency, deadlines, cancellation

Landed in P1:

- Per-server config `timeout` (seconds; default 60) bounds the handshake,
  listing, and calls; a call's deadline is request-scoped and resets while
  progress notifications arrive, so a long tool that reports progress is not
  killed mid-work.
- Cancellation: the daemon's session-cancel path calls
  `McpManager::cancel_session(session_id)` (a channel send, including for child
  sessions), the dispatcher cancels the matching in-flight calls via a per-call
  `CancelToken`, the engine sends `notifications/cancelled`, and the caller gets
  a typed `Cancelled`. The call wrapper also checks `ctx.cancelled` before
  starting.
- Shutdown: `McpManager::shutdown_all` drops each `ServerSlot`; `McpServer`'s
  `Drop` sends `Shutdown` and joins the dispatcher with a bounded wait, so no
  MCP path can block Ctrl-C.

Landed in P6 (`1d7d239`): a per-server `maxConcurrentCalls` cap (default 4,
configurable, `0` clamped to 1) — the dispatcher admits calls up to the cap and
queues the excess, promoting each as a slot frees and waking on commands or
completions via `select!` (no polling). A session cancel reaches queued calls
too. Nothing remains open in this decision.

### D7 — Credentials

OAuth tokens are stored per (issuer, resource) under
`<state>/choreographr/mcp-auth/` (0600 files, one per credential), never in the
DB, never in logs, never shared across issuers (SEP-2352). Config-file
`headers`/`env` secrets are redacted in logs and `session_inspect` output.
Remote-server auth is opt-in per server; a server without `oauth` simply gets no
`Authorization` header (explicit `headers` percolate as configured).

Status: **post-ship (P3)**. M1 has no `oauth` key and no credential store — the
supported credential path is an explicit `Authorization` (or other) header in
`mcp_servers.json`, `${VAR}`-expanded. This D7 design is what P3 implements.

### D8 — Elicitation: advertise only with a UI

Advertise the `elicitation` capability only once a client surface can render a
prompt (TUI dialog / GUI modal). Until then, `input_required` results are
answered by retrying with `decline` responses (valid per MRTR) and a clear tool
error that names what the server asked for. This avoids promising a capability
we cannot honor — the spec forbids servers from demanding undeclared
capabilities, and a hard failure is better than a silent hang.

Status: the bounded decline-and-retry loop landed in P4 (`274f736`) — elicitation
and roots are declined, sampling is refused with a clear error, and a peer that
keeps asking fails rather than hanging. Elicitation is still **not advertised**
(no prompt surface yet).

### D9 — MCP becomes a default feature for M1

M1 flips the `mcp` cargo feature into the default set, at both layers the shipped
binary is built from: the daemon's `default` gains `mcp`, and the root package
re-enables it the way it already re-enables `pdf` (the root consumes
`choreo-daemon` with `default-features = false`, so the workspace default alone
would not reach the binary). Rationale: MCP is a core capability of the product,
not an experiment — a feature that ships disabled is one nobody uses, and the
M1 work that makes the default safe (bounds, name hygiene, status surface,
conformance) landed alongside the flip in `0a85184`.

Trade-offs accepted, with their mitigations:

- Every default build now compiles `choreo-mcp`, `rmcp`, `reqwest`/rustls, and
the sidecar tokio runtime. `rmcp` stays `default-features = false` with only the
needed transports (D1/D11); the named feature remains for opt-out, so an
embedder that cannot host subprocesses or the dependency tree uses
`--no-default-features` (and re-enables what it needs).
- The iOS embedded daemon already consumes the daemon with
`default-features = false` (the `pdf` precedent), so it stays MCP-free at compile
time, and `ToolPolicy::Mobile` keeps filtering MCP at registration time
regardless.
- Release jobs pass explicit `--features` lists but keep defaults on, so
`scripts/release.sh` gains MCP automatically once the default flips; P6 verifies
the static-musl build with it and records the size/build-time delta.
- The "off by default" statements in `ARCHITECTURE.md`, `README.md`, and the
`Cargo.toml` feature comments all flip in the same change (§11).

`choreo-mcp` remains the only crate in the tree with MCP dependencies.

Status: **landed in `0a85184`** — the flip is in place at both layers
(`default = ["pdf", "mcp"]`; `features = ["pdf", "mcp"]` on the root's daemon
dependency), the "off by default" statements in `ARCHITECTURE.md`, `README.md`,
and both `Cargo.toml` comments are updated, and the measured release delta
(~+11 MB, ~+21.7%) is recorded in `scripts/release.sh`. The release jobs gain MCP
automatically (defaults still apply to their explicit `--features` lists).

### D10 — Content mapping

MCP content maps onto `ToolOutput` as follows:

| MCP | Mapping |
|---|---|
| `text` | joined with `\n` (current behavior) |
| `image` | base64 → `PreparedImage` via the daemon's image pipeline → `image_tx`; text placeholder only when no `image_tx` |
| `audio` | text descriptor `[Audio: <mime>, <bytes> — not attached]` |
| `resource_link` | text line with URI/name; future: register as a session resource |
| embedded `resource` | text content when inline; else descriptor |
| `structuredContent` | `ToolOutput.result_json` (programmatic path); not duplicated as a model-visible text block (the spec already tells servers to mirror it into a text block) |
| `annotations` | metadata only; never model-visible as instructions |
| `isError` | `ToolOutput.is_error` on every path (JSON, streaming, postcard) |

Status: this mapping landed in P0/P1 (image attachment, audio/blob/resource-link
descriptors, `result_json`, truncation); only the progress and logging
notification paths remain (P4).

### D11 — Streamable HTTP runs on rmcp's `reqwest` transport, not `ureq`

`choreo-mcp` uses **`reqwest` 0.13 (rustls)** for the MCP HTTP transport, not the
daemon's synchronous `ureq`. This is not a preference over ureq — it follows from
D1 plus the async/streaming shape of the transport:

1. **rmcp's Streamable HTTP client is reqwest-based.** The crate enables
   `transport-streamable-http-client-reqwest`; rmcp offers no `ureq` (or any
   sync) client. Its alternatives are the Unix-socket-only hyper transport and
   the raw `async_rw` framing — neither is a TCP HTTP client.
2. **The transport is async and streaming.** It runs on the sidecar tokio
   runtime and must consume per-request SSE streams (progress now,
   `subscriptions/listen` in P4) and treat stream close as cancellation. `ureq`
   is synchronous and runtime-less: a ureq-based transport would need an
   `spawn_blocking` bridge per request plus a hand-rolled SSE parser, header
   generation (`Mcp-Method`/`Mcp-Name`/`Mcp-Param-*`), and cancellation —
   re-implementing exactly the layer rmcp was adopted to own (D1).
3. **No new HTTP stack.** `reqwest` 0.13 was already in the lockfile via
   `alloy` (choreo-blockchain's RPC stack). Declaring it directly — rather than
   only through rmcp's feature — makes choreo-mcp, rmcp, and alloy share one
   compiled reqwest 0.13.5. A ureq transport here would have put two HTTP
   clients on the MCP path. (The lock's *other* reqwest major, 0.12.28, comes
   from `blitz-net` → `dioxus-native` → `choreo-gui` and predates MCP;
   `deny.toml` keeps `multiple-versions = "warn"`.)
4. **We build the client for policy, not plumbing.** `choreo-mcp` constructs
   the `reqwest::Client` itself to set `connect_timeout` (the server's request
   timeout), disable idle pooling and redirects (matching rmcp's defaults; a
   redirect would replay config headers to a new host), bound SSE events at
   16 MiB (`max_sse_event_size`), and run the deprecated-HTTP+SSE GET probe —
   rmcp accepts the injected client via
   `StreamableHttpClientTransport::with_client`.

`ureq` remains the right client for the daemon's synchronous HTTP (AI providers,
the `http` tool); this decision is scoped to the MCP transport. Not done: the
retry policy honors its exponential backoff only, not a `Retry-After` header
(see the P2 residuals).

### D12 — the daemon-tier MCP config joins the unified config watcher

The daemon already hot-reloads config-dir files through ONE transport:
`config_watch::ConfigWatcher` is a single `notify` watcher over
`$XDG_CONFIG_HOME/choreographr` that fans normalized, per-basename events to
consumers, built explicitly for "`models-overlay.toml`, `accounts.toml`, and
future files". `mcp reload` (`850cb4e`) gave MCP the explicit command; the watcher
is the "like other config" half, and it is a pattern-following addition, not a
new mechanism.

Decision: **yes — subscribe `mcp_servers.json` on the existing transport**,
rather than leaving reload manual-only:

- The consumer mirrors the overlay/accounts pattern: re-read the file,
  fingerprint the raw contents to collapse editor save-event storms (the same
  deterministic fingerprint-compare as `overlay_fingerprint_changed`, no
  debounce timer), then forward the reload to the command loop (the single
  writer). It reuses `McpManager::reload`'s reconcile, so an unchanged config
  is a no-op and live connections are kept.
- Failure policy: a malformed or half-written file logs a warning and keeps the
  current server set — reload errors never mutate state. The explicit command
  remains the path that reports errors to the user (`McpReloadFailed`).
- Scope: the **daemon-tier file only** (`<config>/choreographr/mcp.json`, the
  rename of `mcp_servers.json` — see D13). The per-session project `.mcp.json`
  lives outside the watched config directory (project roots are unbounded), so
  project edits use `/mcp reload` explicitly. D13 also watches the trust store
  `trust.toml` (same config dir) here.
- Gating matches the existing watchers: the embedded daemon's
  `config_watchers: false` path disables it wholesale (mobile-safe).

Trade-off stated: an auto-reload can start (or restart) server subprocesses as a
side effect of saving a file. Reconcile keeps unchanged servers live, so the
blast radius is the servers actually edited, and a restart can interrupt an
in-flight call only for a just-changed server. That is acceptable — but it is
why the fingerprint gate and the malformed-file no-op matter.

What already hot-reloads, and why MCP differs:

- **Project context files / `AGENTS.md`** (the config-dir `AGENTS.md`,
  `~/.agents/AGENTS.md`, `~/.claude/CLAUDE.md`, and the project chain of
  `context_file_names` up to the git root): re-read on **every agent-loop
  iteration** — `build_system_content` calls `discover_context` each turn and
  fingerprints the result, so the session `context_cache` only skips
  re-assembly. Edits land on the next turn, no watcher needed, and a newly
  touched subdirectory's `AGENTS.md`/`CLAUDE.md` is injected as a hint as
  tools reach it.
- **Skills**: a session-scoped snapshot — lazily discovered once per session
  and invalidated only by `set_working_dir`. The listing (name/description) is
  frozen for the session, while `load_skill` reads the body fresh from disk at
  call time. A skill *added* mid-session is therefore invisible until the
  working directory changes or a new session starts (a deliberate "they don't
  change during a session" assumption, not a watcher gap).
- **`mcp_servers.json`** is different: its contents are **connection state**
  (subprocesses, transports, a registered catalogue), which cannot be re-read
  per request — hence explicit `mcp reload` plus this watcher decision.

None of these is wired through `ConfigWatcher`; that transport exists for files
whose consumers are long-lived threads (catalog, accounts, and now MCP). The
skills snapshot asymmetry is recorded here for visibility; changing it is
outside this plan.

Follow-up item: land the watcher subscription (P5's post-ship list).

### D13 — project-scoped MCP config, per-session visibility, and an MCP trust store

Two-tier, per-session MCP configuration is the settled design (it supersedes the
single-file overlay of D4/D12 for the project tier):

- **Daemon tier:** `<config>/choreographr/mcp.json` (renamed from
  `mcp_servers.json`; no release has shipped, so there is no legacy alias).
  Visible to every session, trusted unconditionally (the user authored it), and
  the only tier registered into the daemon-wide tool catalogue.
- **Project tier:** `<PROJECT_ROOT>/.mcp.json`, where `PROJECT_ROOT` is found by
  walking **up** from a session's working directory to the git root; the first
  `.mcp.json` wins and its owning directory is the project's identity **and**
  trust key. No project tier without a working directory.
- **Visibility:** a session sees the daemon-tier servers ∪ its own project's
  servers. A project server replaces a daemon-tier server of the same slug *for
  that session only* (replace by group, not union); no other project's servers
  are ever visible.
- **Trust:** a project's `.mcp.json` travels with a checkout the user may not
  have written, so its servers — and any `${VAR}`/header/env expansion they
  request — are gated behind an explicit **whole-project** trust decision
  (`/mcp trust`), keyed on the EXACT canonical project root (absolute + symlinks
  resolved) with no ancestor inheritance. Trust is content-agnostic (a root with
  no `.mcp.json` yet may be trusted) and whole-project (one decision authorizes
  every server the root's file declares). An untrusted project's file is read so
  `/mcp status` can report what is ignored, but is never spawned and never
  expanded. The store is `<config>/choreographr/trust.toml` (TOML; the config
  dir so the existing watcher can watch it by basename), written atomically with
  owner-only permissions and read **fail-closed**.
- **Pooling & lifetime:** connections are ref-counted. Project-shared servers
  (default) are keyed by `(project_root, slug)`; daemon-tier shared servers by
  `slug`; a per-session `shared: false` server gets a private connection per
  session that uses it. A project's pool entries drop when the last session
  referencing them leaves, and immediately on untrust or on a `set_working_dir`
  that leaves the project (which also cancels that session's in-flight calls to
  the old project's servers).
- **Activation & overlay:** a session's daemon-tier AND own-project MCP groups
  are active by default (no `load_tools` needed). Project groups are computed
  per session and NEVER persisted into `active_tool_groups`; the shared registry
  holds only core + daemon-tier shared servers + static groups, and each session
  carries its own project tool wrappers. The request path merges the registry
  definitions (active ∪ protected, minus every daemon-tier `mcp/<slug>` group the
  session's project shadows) with the session's own project tools; the execution
  path consults the session's project tools BEFORE the shared registry.
- **Hot-reload:** `mcp.json` and `trust.toml` are watched via the existing
  `ConfigWatcher` (config-dir basename subscriptions); consumers re-read +
  fingerprint-gate and forward `McpServerTierReload` / `McpTrustReload`. The
  project `.mcp.json` is NOT watched — `/mcp reload` reconciles the active
  session's project file.
- **Trust surface:** a TUI slash command (`/mcp trust`, `/mcp untrust`,
  `/mcp trust list`) over new `ClientMessage`/`DaemonMessage` variants. No
  per-server approve/reject and no separate credential approval (whole-project
  trust covers both). `PROTOCOL_VERSION` is not bumped here — version bumps
  happen at release time.

---

## 6. Target architecture

As built through P6 (M1 complete):

```
choreo-daemon (thread-only)
  └── mcp/
        ├── config.rs   mcp_servers.json (user + project layer) →
        │               Vec<McpServerConfig> (cwd/disabledTools/log path)
        ├── mod.rs      McpManager { servers: HashMap<slug, ServerSlot> },
        │               ServerSlot { handle, server }, 2 s startup budget,
        │               cancel_session(session_id) fan-out, register_all +
        │               take_list_change_rx (subscriptions), status() /
        │               reconnect(slug), per-server stderr log capture
        └── tool.rs     McpToolWrapper: ToolDyn — sanitized name/group,
                        schema, content mapping, image sink, resource tools,
                        256 KiB truncation

choreo-daemon catalog plane:
  command loop ← DaemonCommand::McpListChanged (forwarder thread)
    → build_tool_registry (core tools + platform bridge + register_all)
    → Arc<ArcSwap<ToolRegistry>> store (single writer; readers load lock-free)

control plane (choreo-proto v9):
  TUI `/mcp` + `/mcp reconnect <slug>` + `/mcp reload`,
  `choreographr mcp list|add|remove|reconnect|reload`
    → ClientMessage::{McpStatusRequest, McpReconnect, McpReload}
    → McpManager::status / reconnect / reload
    → DaemonMessage::{McpStatus, McpReconnectFailed, McpReloaded, McpReloadFailed}

choreo-mcp (library; owns tokio + rmcp)
  ├── runtime.rs    sidecar Runtime: init / get / handle / block_on
  ├── config.rs     McpServerConfig + McpProtocolMode + maxConcurrentCalls
  │                 + maxRestarts
  ├── naming.rs     sanitize_segment / build_tool_name (64-char cap,
  │                 collision hash) / group_name
  ├── protocol.rs   daemon-facing types, normalize_input_schema, McpListChange
  ├── error.rs      McpError (incl. AuthRequired)
  ├── stdio.rs      capped child transport: BoundedLineReader (8 MiB per frame);
  │                 child stderr → per-server log file
  ├── session.rs    per-server dispatcher thread:
  │                   McpCommand::{ListTools, Call, CancelSession, Shutdown}
  │                   McpServerHandle { list_tools, call_tool,
  │                   call_tool_streaming, cancel_session }
  │                   in-flight registry + concurrency gate (cap, queued),
  │                   CancelToken, RestartPolicy (3 attempts, backoff ≤ 60 s),
  │                   connect_with_list_changes → McpListChange sink
  └── engine.rs     the only rmcp-coupled module:
                      connect → stdio (capped) | StreamableHttpClientTransport,
                      lifecycle (discover/initialize), list_all_tools,
                      call_tool (deadline + cancel + progress chunks + MRTR),
                      resources, subscriptions/listen, ClientHandler,
                      auth-required mapping
```

Data flow for one tool call:

1. Session thread executes `mcp/<slug>/<tool>` → `McpToolWrapper::execute_json`.
2. The wrapper checks the session's cancel flag, sends
   `McpCommand::Call { session_id, request, reply }` over the dispatcher channel,
   and blocks on the per-call reply channel.
3. The dispatcher registers the call (`call_id` + `CancelToken`), spawns the
   call task on the sidecar runtime, and keeps serving commands; the engine
   sends the request with a deadline (`PeerRequestOptions::with_timeout(...)`
   with `reset_timeout_on_progress`), racing the response against the token and
   sending `notifications/cancelled` to the server when cancelled.
4. Reply → content mapping (D10) → `ToolOutput`. A call that failed on the
   transport sends a completion notice back; the dispatcher rebuilds the engine
   under the restart policy before serving the next command.
5. `CancelSession` walks the in-flight registry and cancels every call with the
   matching session id (queued ones included); `Shutdown` closes the connection
   and exits the thread (joined with a bounded wait from `McpServer::drop`).

Data flow for a tool-list change: the engine's `ClientHandler` receives a
`toolsListChanged` / `resourcesListChanged` on the connection's
`subscriptions/listen` stream, forwards it as an `McpListChange` to
`McpManager`'s receiver; a daemon forwarder thread translates it into
`DaemonCommand::McpListChanged`, and the command loop rebuilds the catalogue and
swaps the `Arc<ArcSwap<ToolRegistry>>` — every session and request worker sees
the refreshed `mcp/<slug>` group on its next load.

Lifecycle per server: connect at startup under the 2 s batch budget (`ready`, or
skipped with a log); at runtime a transport failure triggers a reconnect under
the restart policy (attempts reset on success, never retried in a loop after the
budget is spent); `McpManager::shutdown_all` drains the slots with bounded joins.
The subscriptions-driven refresh landed (P4). Remaining lifecycle work: a
`needs_auth` state (P3, post-ship).

## 7. Work breakdown

Each phase is independently shippable and lands with tests + docs. **P0–P2 and
P4–P6 are done** and **D9 landed in `0a85184`**; their sections come first for
history. The only remaining phase is the deferred **P3** (post-ship).

### M1 — ship without OAuth (complete)

MCP ships without OAuth: everything in M1 has landed. **OAuth is explicitly not
part of M1** — it is P3,
post-ship. Servers that need credentials
are covered in the interim by the path that already works: a static token in
`mcp_servers.json` headers, e.g. `"headers": {"Authorization": "Bearer ${DOCS_TOKEN}"}` (only
`accept`, `mcp-session-id`, and `last-event-id` are reserved, and `${VAR}` is
expanded at load). A server that insists on OAuth and has no static token fails
with a clear, actionable error instead of a raw status string (`f037383`).

M1 includes, at minimum:

- **Default on**: **done** (`0a85184`) — the `mcp` feature is in the daemon's
  `default` and re-enabled on the root package's dependency; docs and feature
  comments flipped; measured release delta ~+11 MB (~+21.7%).
- **P6**: **complete** — the caps (tool count 1024, schema
  depth 32, `maxRestarts`, notification rate limit), the bounded SSE retry
  policy, fuzz-style property tests, the stubborn-server shutdown test,
  supply-chain (the gate is enforced by `pre-release` and the release CI, and
  the dependency notes landed), the §9 security posture (recorded as the
  "MCP client trust boundary" section in `ARCHITECTURE.md`), the release
  verification (size delta recorded in `scripts/release.sh`), and the
  conformance suite for **both protocol eras** in CI with a committed baseline.
- **P5**: **complete** — tool-name sanitization, `cwd`/`disabledTools`, the
  `/mcp` status surface + `choreographr mcp` CLI + per-server logs +
  `session_inspect`, the auth-required error, config layers, and user docs
  (`f037383`, `8d27006`, `a348c89`).
- **P4**: **complete** — progress chunks, MRTR handling, resource tools, and the
  server-observed cancellation test (`274f736`), plus `subscriptions/listen` +
  the registry hot-swap (`15aa3ff`).

Explicitly deferred past M1 (not ship blockers): **OAuth (P3)**, `exposure` +
[tool-search-driven deferred loading](#13-out-of-scope--future-work), and the
MCP-server role (§13). (`mcp reload` has since landed — see below.)

M1 has no remaining items. The **2026-07-28-era conformance run** is closed
(see P6): the harness now carries the modern-era scenario handling (a
`tools/list` plus per-tool exercise, the `MCP_CONFORMANCE_CONTEXT`
scenario-supplied calls, and the JSON-Schema-preservation echo), and CI runs both
protocol eras as a matrix against the committed baseline. Every non-auth client
scenario passes on both wires; the baseline holds only the OAuth `auth/*`
scenarios plus the legacy-wire elicitation scenario. Everything else — D9, the
caps, fuzzing, supply-chain, security, and release verification — landed in
`0a85184`.

M1 shipping does **not** delete this plan: the Lifecycle rule ties deletion to
full implementation, and P3 remains here for post-ship work.

### M2 — two-tier per-session MCP config + trust store

Implements D13. Splits MCP configuration into a daemon tier
(`<config>/choreographr/mcp.json`) and a per-session project tier
(`<PROJECT_ROOT>/.mcp.json`), gates the project tier behind a whole-project
trust store (`<config>/choreographr/trust.toml`), adds the per-server `shared`
key, ref-counts pooled connections, computes each session's project overlay
(never persisted into `active_tool_groups`), and adds the `/mcp trust|
untrust|trust list` slash command over new protocol messages (no
`PROTOCOL_VERSION` bump). The daemon-tier file and the trust store hot-reload
via the existing config watcher; the project file reloads via `/mcp reload`.

### P0 — Correctness and safety on the current engine (small, no new deps)

**Done** — `ab3dc2e`. All items landed: camelCase wire parsing fixed (a tool's
`inputSchema`, `isError`, and image `mimeType` were being read as snake_case and
silently dropped); `normalize_input_schema` (missing/`null` → empty-object
schema; non-object or > 256 KiB drops only that tool); 8 MiB stdout line cap;
bounded notification channel; per-server `timeout`; `clientInfo.version` from the
crate version; image attachment via the daemon pipeline; 256 KiB text truncation
with a marker; `isError` on the postcard path; honest `output_schema`;
`auto_load` removed (reported as an unknown key); `lib.rs`/dead-variant cleanup;
and the hermetic in-tree fixture server replacing npx.

One P0 guarantee was later superseded: the 8 MiB line cap lived in the
hand-rolled transport, which P1 deleted. The bound was reinstated against rmcp's
transport in P6 (`1d7d239`, `choreo-mcp/src/stdio.rs`), so the guarantee stands.

### P1 — `rmcp` engine behind the blocking facade (core swap)

**Done** — `976edf6`. `rmcp` 3.5 (workspace dep, `default-features = false`) with
`client` + `transport-child-process`; `runtime.rs` sidecar (`init`/`get`/
`handle`/`block_on`); `session.rs` dispatcher + `McpServer`/`McpServerHandle`
facade replacing the `Mutex`; `engine.rs` as the only rmcp-coupled module;
`ClientLifecycleMode::Auto` (preferred `[2026-07-28]`, legacy `2025-11-25`) with a
per-server `protocol` override; `McpManager` server slots with a 2 s startup
budget and bounded shutdown; `cancel_session` wired into the daemon's cancel
path (including child sessions); mock-engine unit tests (wait-free) and the
fixture-driven integration suite across both eras, `auto` fallback, crash,
garbage, oversized, rejected `initialize`, structured content, and cancellation.

Carried forward: the per-server concurrency cap (D6) and the bounded frame reader
(P6).

### P2 — Streamable HTTP transport

**Done** — `9f6209a`. Config grew `url`, `headers`, and `transport: "http" |
"stdio" | "auto"` (with `${VAR}` expansion in `env`/`headers` values; `auto`
infers HTTP from `url` and stdio from `command`, and both-or-neither skips the
server with a warning). The engine connects over rmcp's
`StreamableHttpClientTransport` with an injected `reqwest`/rustls client
(connect timeout, pooling/redirects off — D11) and negotiates the same
`Auto`/`modern`/`legacy` eras; config headers are validated up front (invalid or
reserved names fail the connect), and SSE events are bounded at 16 MiB. A
connect that fails with `408`/`429`/`5xx` is retried (3 attempts, 500 ms·2^(n-1)
capped at 60 s); a `4xx` is settled; an endpoint that answers a GET with the
removed 2024-11-05 HTTP+SSE transport is rejected with a typed
`UnsupportedTransport`. Hermetic integration tests drive a local `TcpListener`
HTTP fixture (JSON and SSE responses, generated-header validation, `auto`
fallback, a retryable `503`, and the HTTP+SSE rejection).

Residuals:

- [ ] `Retry-After` is not honored — **blocked upstream**: rmcp 3.5's
      `StreamableHttpError` surfaces a non-2xx response as
      `UnexpectedServerResponse(String)` only, with no headers, so the client
      cannot read the header from the failed connect. Fixing it means rmcp grows
      a structured response error, or the connect probe stops going through
      rmcp's transport (which contradicts D1/D11); report it upstream. Impact is
      low: the backoff is bounded and stops after 3 attempts.
- [ ] No explicit idle-read timeout for a long-lived SSE response stream beyond
      the per-request deadline. `subscriptions/listen` has landed (P4) without
      one; rmcp exposes an `SseRetryPolicy` hook (`retry_config` on the
      transport config) for exactly this layer — decide whether to set it (P6).

### P4 — Feature plumbing into the daemon (M1)

**Done** — content mapping (D10) landed in P0/P1; notifications, MRTR, progress,
resources, and the server-observed cancellation test in `274f736`; the
`subscriptions/listen` + registry hot-swap pair in `15aa3ff`.

- [x] D10 content mapping, including `image_tx` and `result_json` — landed P0/P1.
- [x] Progress → `ToolResultChunk`s (rate-limited via `PROGRESS_MIN_INTERVAL`,
      best-effort); logging notifications → tracing; a real `ClientHandler` so
      notifications reach the client instead of being dropped — landed P4.
- [x] MRTR loop for `input_required`: elicitation/roots are declined and the
      call re-issued with `inputResponses` + the echoed `requestState`; sampling
      is refused with a clear error; a peer that keeps asking fails cleanly —
      landed P4 (D8).
- [x] Resource tools (per server, when the `resources` capability is declared):
      `mcp/<slug>/list_resources` + `mcp/<slug>/read_resource`, paginated —
      landed P4; prompt tools remain deferred.
- [x] `outputSchema` → `ToolDyn::output_schema()` (real schema) and
      `structuredContent` → `result_json` — landed P1.
- [x] Cancellation end-to-end test asserting the *server* observed
      `notifications/cancelled` — landed P4 (the fixture records it).
- [x] `subscriptions/listen` + registry refresh — landed. The client opens
      a stream when the server declares list-changed and forwards each event to
      the daemon, which rebuilds and swaps its shared tool catalogue:
  - [x] Client wiring: the engine opens a `subscriptions/listen` stream
        (rmcp's `Peer::listen`) when the negotiated era is stateless and the
        server advertises a list-changed capability, and forwards each
        `toolsListChanged` / `resourcesListChanged` as an `McpListChange` over
        a crossbeam sink; a legacy peer, or one without the capability, opens
        no stream.
  - [x] Registry hot-swap: `McpManager` exposes the shared list-change
        receiver; the daemon command-loop assembly spawns a forwarder thread
        that turns each event into `DaemonCommand::McpListChanged`, and the
        command loop rebuilds the whole catalogue (`build_tool_registry`,
        which re-registers every server via `register_all`) and stows it into
        the shared `Arc<ArcSwap<ToolRegistry>>` — so an already-running
        session observes the refreshed `mcp/<slug>` group on its next request,
        no restart and no session respawn. Every session and request worker
        shares that one `ArcSwap` (single writer: the command loop).

### P5 — Configuration, UX, observability (M1)

**Done** (`f037383`, `8d27006`, `a348c89`).

- [x] Remaining config keys: `cwd` and `disabledTools` landed (`f037383`);
      `exposure` defers with the deferred-tool-loading work below.
- [x] Tool-name sanitization and collision hashing (G18, D5) — `f037383`.
- [x] Actionable "authorization required" error: a `401`/`403` at connect (or on
      a request) maps to `McpError::AuthRequired` naming the server and the two
      options — a static token via `headers`, or OAuth (post-ship) — instead of
      a raw `HTTP 401` string — `f037383` (extends `parse_http_status`).
- [x] Config layers: user file + project file (`.choreographr/mcp_servers.json`),
      project overrides user per server — `f037383`.
- [x] `choreographr mcp list/add/remove/reconnect` CLI + `/mcp` status surface
      (server state, tool counts, last error — no auth state yet) rendered from
      `McpManager::status` — `8d27006`. `enable`/`disable` are not offered (no
      runtime add/remove command); the TUI reports them as unsupported.
- [x] `session_inspect` includes MCP server status/tool counts (no secrets) —
      `f037383`.
- [x] Per-server log file (`mcp-<slug>.log`, size-capped) — `a348c89`.
- [x] User docs: `mcp_servers.json` reference incl. the static-token pattern and
      the "OAuth not yet supported" limitation (post-ship P3) — `f037383`/
      `a348c89`.

Post-ship (fast-follow, around P3):

- [ ] `exposure` + deferred exposure/tool search for servers with many tools
      (reuse `load_tools` groups; measure first — open question 1).
- [x] `mcp reload` (re-read config, add/remove/restart servers without a daemon
      restart) — landed: `McpManager::reload` re-reads the user+project config,
      reconciles the running server set (add/remove/restart), and rebuilds the
      catalogue; exposed as `ClientMessage::McpReload` ⇄ `DaemonMessage::McpReloaded`/
      `McpReloadFailed`, `/mcp reload` in the TUI, and `choreographr mcp reload`.
- [ ] `mcp_servers.json` joins the unified config watcher for auto-reload (D12):
      subscribe on `ConfigWatcher` alongside overlay/accounts, fingerprint-gate
      the re-read, and forward the reload to the command loop; user file only
      (the project layer stays on the explicit command).

### P6 — Bounds, conformance, hardening (M1)

- [x] Reinstate the bounded stdio frame reader — landed (`1d7d239`): `stdio.rs`
      builds the stdio transport over a crate-local `BoundedLineReader` (8 MiB
      per frame, `MAX_STDIO_FRAME_BYTES`); an overlong frame fails the read and
      drops the connection, which the bounded restart policy can rebuild. The
      write side and the JSON-RPC framing stay rmcp's. Worth reporting upstream
      that `AsyncRwTransport` still defaults to an unbounded line.
- [x] Per-server concurrent calls — landed (`1d7d239`): `maxConcurrentCalls`
      (default 4, configurable, `0` clamped to 1), with a dispatcher-side queue
      and `select!` promotion.
- [x] Remaining caps — landed (`0a85184`): a 1024-tool catalogue cap, a
      32-level schema-depth bound (256 KiB size bound already), a per-server
      `maxRestarts` (default 3; `0` disables reconnect), and a rate limit on
      server logging notifications.
- [x] Adopt the official `@modelcontextprotocol/conformance` suite for the
      client — landed: `scripts/mcp-conformance.sh` runs the pinned
      `0.2.0-alpha.12` client suite against the `mcp-conformance-client`
      harness with the committed `expected-failures` baseline, wired into CI
      (`.github/workflows/mcp-conformance.yml`) for **both protocol eras** —
      the stateful 2025-11-25 wire and the stateless 2026-07-28 wire — as a
      matrix. The harness derives the lifecycle from
      `MCP_CONFORMANCE_PROTOCOL_VERSION`, issues the calls a scenario supplies
      in `MCP_CONFORMANCE_CONTEXT`, and otherwise exercises the advertised
      catalogue; every non-auth client scenario passes on both wires
      (`request-metadata`, the SEP-2243 header mirroring and malformed-tool
      rejection, SEP-2106 non-dereferencing and JSON Schema 2020-12
      preservation, and the SEP-2322 MRTR request-state echo on the modern
      wire; `sse-retry` now passes on the legacy wire too). The baseline holds
      only the OAuth `auth/*` scenarios plus the legacy-wire elicitation
      scenario. The suite moved from the 0.1 line to `0.2.0-alpha.12` because
      only the 0.2 line can express the 2026-07-28 era.
- [x] Fuzz/config hardening — landed (`0a85184`): deterministic fuzz-style
      property corpora for the `mcp_servers.json` parser and the tool-name
      sanitizer (no external fuzz target).
- [x] Supply-chain — `check-supply-chain` is part of `just pre-release` and the
      release workflow, and the dependency notes (rmcp/process-wrap/reqwest)
      landed in the Cargo comments and the ARCHITECTURE dependency table.
- [x] Default-feature release verification — the measured delta is recorded in
      `scripts/release.sh` (~+11 MB, ~+21.7%); release jobs gain MCP via their
      defaults; the opt-out paths (iOS `default-features = false`;
      `--no-default-features`) are documented.
- [x] Security checklist from §9 — recorded as the "MCP client trust boundary"
      section in `ARCHITECTURE.md`, covering all eight points.
- [x] Decide the SSE idle-timeout handling — landed (`0a85184`): the SSE
      reconnect policy is bounded at the transport (the crate default retries a
      dropped stream forever).
- [x] Add the "no MCP path wedges shutdown" test — landed (`0a85184`): the
      stubborn-server fixture backs both a `choreo-mcp` and a daemon test
      proving shutdown returns within its bounded wait.

### P3 — OAuth for remote servers (post-ship, deferred)

**Not part of M1.** This is the natural first post-ship phase: it closes the last
S1 gap (the auth half of G2) and unlocks managed remote servers. Until it lands,
a static token in `headers` is the supported credential path and an
OAuth-requiring server fails with the P5 actionable error.

- [ ] rmcp `auth`; protected-resource metadata discovery (RFC 9728) + AS
      metadata (RFC 8414) + `iss` validation (RFC 9207).
- [ ] Client registration: DCR per SEP-837 with `application_type`, CIMD support,
      pre-registered client config, callback on loopback with paste-URL fallback.
- [ ] Credential store (D7) + refresh + logout; keyed by issuer/resource.
- [ ] TUI/CLI: `mcp login <slug>`, `mcp logout <slug>`, plus auth state in the
      `/mcp` status surface (the surface itself ships in M1).
- [ ] Tests: in-repo mock authorization server (borrow the shape of pi's
      `mcp-oauth-server.ts` and hermes' e2e fixtures), scope step-up, refresh,
      revocation.

## 8. Testing strategy

Per AGENTS.md: unit tests live in `src/**/#[cfg(test)]` (no time-based waits);
integration tests live in `tests/it/` (one binary per crate, `#[ignore]`).

- **Fixture server.** Landed in P0 and extended in P1: an in-tree Rust binary
  (`choreo-mcp/tests/fixtures/fixture_server.rs`, spawned via
  `env!("CARGO_BIN_EXE_mcp-fixture-server")`) speaking scripted scenarios through
  a scenario argument. Covered today: modern `server/discover`, legacy
  `initialize`, the `auto` fallback, a rejected `initialize`, crash-on-call,
  garbage lines, an oversized line, the tool catalogue (echo/boom/image/
  structured/slow) with a real `outputSchema`, and a slow call for cancellation.
  The daemon suite re-uses the same source through an `include!` in
  `choreo-daemon/tests/fixtures/mcp_fixture_server.rs`. No Node/npx, no network.
  P4 added scenarios for progress, MRTR, resources, and a fixture-recorded
  `notifications/cancelled`, and a `modern-list-changed` server that opens a
  `subscriptions/listen` stream. Still to add: paged `tools/list` and bad-schema
  variants (P6).
- **HTTP fixture.** `choreo-mcp/tests/it/mcp_http_integration.rs` (P2) drives a
  local `TcpListener` fixture: JSON and SSE responses, generated-header
  validation (`Mcp-Method`/`Mcp-Name`/`MCP-Protocol-Version` and custom
  headers), the `auto` fallback to a legacy server, a retried `503`, and the
  deprecated HTTP+SSE rejection. Still to add: an OAuth challenge
  (`401`/`WWW-Authenticate`) in P3 and a long-lived `subscriptions/listen`
  stream when that P4 item lands.
- **Unit tests** cover: schema normalization, content mapping, name/desc
  formatting, error mapping, the dispatcher protocol + cancellation + restart
  backoff against a mock engine, config parsing (transport inference,
  `${VAR}` expansion, timeout/protocol/`maxConcurrentCalls`/unknown keys),
  the HTTP retry policy (status classification + capped backoff), the P4
  progress rate-limit and MRTR decline helpers, the `BoundedLineReader` frame
  cap, the concurrency gate (admit/queue/promote, `0` clamp), P5's tool-name
  sanitizer/collision/group-name cases, the config layers (project override,
  `cwd`/`disabledTools`), log-stem sanitization, the `AuthRequired` mapping,
  the TUI `/mcp` command parsing, the reload reconcile (no-config and
  malformed-config cases), and runtime init. All wait-free — the restart
  tests exercise the backoff math without sleeping, and the cancellation test
  synchronizes over channels (mock signals `started`, then the test cancels).
- **Integration tests** cover: stdio against the fixture server per era, the
  `auto` fallback, structured content, crash/garbage/oversized/no-init,
  cancellation, P4's progress→chunk path (client and daemon), MRTR,
  resource list/read, the server-observed `notifications/cancelled`, and a live
  catalogue refresh (a `subscriptions/listen` list-changed event re-registers
  the server's `mcp/<slug>` group); HTTP against the `TcpListener` fixture
  (above). The stdio cancellation case waits for the fixture's in-flight marker
  with a bounded 5 ms poll (integration-only; the unit-test wait-free rule is
  intact), and the daemon case asserts the image sink path end-to-end.
- **Conformance** runs the official client suite against the
  `mcp-conformance-client` harness (`scripts/mcp-conformance.sh`, pinned
  `0.2.0-alpha.12`) with a committed expected-failures baseline, in CI for both
  protocol eras (2025-11-25 and 2026-07-28) as a matrix; every non-auth client
  scenario passes on both wires, and only the OAuth `auth/*` scenarios (plus the
  legacy-wire elicitation scenario) are baselined until P3.
- **Manual interop matrix** (documented, run at release): current
  `@modelcontextprotocol/server-everything`, a filesystem server, a remote OAuth
  server (e.g. an MCP provider available to the project), and one legacy server.
- **Determinism**: unit tests use no `sleep`; deadlines and backoff are exercised
  through injected values/config, not wall-clock waits. Integration tests keep
the pre-existing 120 s watchdog and one bounded marker poll (above).

## 9. Security & trust model

1. **Untrusted by default**: tool descriptions, annotations, `title`, `icons`,
   `instructions` and resource text are inputs, not instructions. They are
   rendered to the model as data (the wrapper already prefixes tool descriptions;
   keep that) and are never executed.
2. **Schema safety**: JSON Schema 2020-12; no network `$ref` dereferencing;
   depth/size/time bounds before validating; reject tools with invalid schemas
   (exclude the tool, keep the rest — per spec).
3. **Header discipline**: only `Mcp-Method`/`Mcp-Name`/`Mcp-Param-*` are
   generated, from the request body, with the spec's base64 sentinel; user
   `headers` are passed through but never echoed into logs unredacted; config
   values never become header names.
4. **Process safety**: child in its own process group (already), spawn through an
   explicit executable + args (never a shell string), env-only credentials for
   stdio, bounded reads/writes, kill escalation on shutdown.
5. **Capability honesty**: never advertise elicitation/sampling/roots we cannot
   honor (D8); handle `MissingRequiredClientCapabilityError` by surfacing the
   missing capability, not by silently declaring more.
6. **Least privilege**: an MCP server runs with the daemon user's full authority
   today. Note in docs; the sandboxed-execution story (`ToolPolicy::Mobile`
   exclusion, future OS sandboxing) applies. No auto-approval is added by this
   plan; tool invocation keeps the same broadcast/visibility semantics as every
   other tool.
7. **Egress**: remote servers are contacted only when configured; no automatic
   discovery of servers from arbitrary content.
8. **Secrets**: never logged; OAuth store 0600; tokens keyed by issuer; explicit
   logout deletes them.

## 10. Configuration & control surface

Target config shape (superset, all keys optional except command/url). Landed in
P0–P6: `command`/`args`/`env`/`cwd`, `url`/`headers`, `transport`, `protocol`,
`timeout`, `enabled`, `maxConcurrentCalls`, `maxRestarts`, `disabledTools`,
`${VAR}` expansion, and the project-config layer. Still pending: `exposure`
(post-ship) and `oauth` (post-ship, with P3).

```json
{
  "mcpServers": {
    "filesystem": {
      "command": "npx",
      "args": ["-y", "@modelcontextprotocol/server-filesystem", "."],
      "env": { "TOKEN": "${FS_TOKEN}" },
      "cwd": "~/work",
      "disabledTools": ["noisy_tool"],
      "protocol": "auto",
      "timeout": 60,
      "exposure": "direct",
      "enabled": true
    },
    "docs": {
      "url": "https://example.com/mcp",
      "headers": { "Authorization": "Bearer ${DOCS_TOKEN}" },
      "oauth": { "clientId": "choreographr", "callbackPort": 8765 },
      "protocol": "auto",
      "timeout": 120
    }
  }
}
```

Config layers: the user file at `<config>/choreographr/mcp_servers.json` and a
project file at `<root>/.choreographr/mcp_servers.json`, which overrides the
user file per server slug.

Surfaces: the CLI (`choreographr mcp list|add|remove|reconnect`) and the TUI
`/mcp` status surface (server state, tool counts, last error, reconnect) ship in
M1; `mcp reload` (CLI + `/mcp reload`) landed as a post-ship fast-follow
(`850cb4e`). Sign-in (`login`/`logout`, auth state) arrives with P3 post-ship;
`exposure` moves with its deferred-loading work; `enable`/`disable` from the
surface need a runtime add/remove command and are not offered (the TUI reports
them as unsupported).

## 11. Documentation deliverables

- `ARCHITECTURE.md`: rewrite the `choreo-mcp` module table (§`choreo-mcp`), the
  `mcp/` daemon row, the feature list row, the test-coverage table rows, and the
  threading-model paragraph (sidecar + dispatcher). The feature row flips to
  default-on for M1 (D9) — no "off by default" claim remains.
- `README.md`: update the MCP capability lines (transport, features, OAuth).
- `choreo-mcp` rustdoc: module headers per the migration rules; keep
  `#![warn(missing_docs)]` green (`doc-check` covers the crate).
- Flip the "off by default" statements for M1 (D9): the `ARCHITECTURE.md` and
  `README.md` feature/module rows and the `Cargo.toml` feature comments in the
  root and `choreo-daemon` all describe `mcp` as opt-in today; they change with
  the default flip, and the opt-out paths (iOS `default-features = false`,
  `--no-default-features`) are stated where the default is.
- Release note: one `feat(choreo-mcp):` commit (or a small series: `feat`,
  `fix`, `refactor`) with user-facing prose. `choreo-mcp` and `choreo-daemon`
  scopes preferred.
- **No plan references.** Everything above must stand on its own: do not mention,
  link, or paraphrase this plan in any of it. The plan is deleted once
  implemented, so a reference to it (`see docs/plans/mcp-modernization.md`) is a
  future broken link and stale context.

Status: P0/P1 kept `ARCHITECTURE.md` (module tables, `mcp/` row, threading model,
test-coverage rows) and `README.md` in step, and the tree contains no reference to
this plan — verified at `850cb4e`. P4/P5/P6 kept both in step too (the
`choreo-mcp` module table gained `naming.rs` and the auth/`cwd`/`disabledTools`
notes, the `mcp/` row gained the config layers, name hygiene, status/reconnect,
and per-server logs, and `README.md` gained the `mcp_servers.json` reference).
The feature-row flip (D9) still waits on the default-on change.

## 12. Risks & mitigations

| Risk | Mitigation |
|---|---|
| Dependency weight is paid by default builds (tokio/reqwest/rustls via rmcp, D9) | `rmcp` stays `default-features = false` with only the needed transports; the named feature remains the opt-out (`--no-default-features`; iOS keeps `default-features = false`). The cost is measured, not assumed: ~+11 MB (~+21.7%, 50.9 → 61.9 MB) on a local release build, recorded in `scripts/release.sh`. P1 scoped rmcp to `client` + `transport-child-process`; P2 added `transport-streamable-http-client-reqwest` and `reqwest` 0.13 (shared with rmcp and alloy — D11). |
| Tool-list refresh needs the shared `Arc<ToolRegistry>` replaced | `DaemonState::tool_registry` is a process-wide `Arc<ArcSwap<ToolRegistry>>` shared into every session and request worker; the daemon command loop is its single writer, rebuilds it (`build_tool_registry`: core tools + platform bridge + `McpManager::register_all`) on an `McpListChanged` event, and stores it once atomically. Readers load lock-free, so in-flight holders keep the old `Arc` and every live session observes the refreshed catalogue on its next request — no restart. (The same single-writer `ArcSwap` exception as the provider catalog.) |
| Two `reqwest` majors in the lockfile (0.12.28 via `blitz-net`/`dioxus-native` → `choreo-gui`; 0.13.5 shared by `alloy` + `rmcp` + `choreo-mcp`) | The duplicate predates MCP and belongs to the GUI renderer; the MCP path shares one 0.13 build (D11). `deny.toml` keeps `multiple-versions = "warn"`. |
| rmcp API churn (3.x is moving fast) | Pin `3.5`, upgrade deliberately; the blocking facade isolates the daemon from rmcp types (rmcp types do not cross the crate boundary). |
| Sidecar runtime + threads complicate shutdown | Follow the `choreo-content` runtime pattern; dispatcher replies are bounded; `shutdown_all` joins with deadlines. **Landed** (`0a85184`): a stubborn-server fixture (ignores stdin EOF) backs both a `choreo-mcp` and a daemon test proving shutdown returns within its bounded wait. |
| (M1) Remote servers that require OAuth have no credential path until P3 | Document the static-token `headers` workaround (P5 docs); the P5 actionable `401`/`403` error names both options, so the failure explains itself. |
| (Post-ship, P3) OAuth UX on headless devices (TUI over SSH, Termux) | Paste-the-redirected-URL fallback (pi's flow), device-code path only if a provider requires it; document. |
| rmcp licenses/advisories | Apache-2.0; `cargo deny check` already gates the tree. |
| Fixture server drifts from real servers | The in-tree fixtures plus the official client conformance suite in CI (pinned suite version; a bump is a deliberate change that re-triages the baseline). |

## 13. Out of scope / future work

- **OAuth is not out of scope** — it is deliberately deferred to post-ship (P3,
  §7): M1 ships without it, with static-token `headers` and an actionable
  auth-required error as the interim.
- **MCP server role** (expose choreographr tools to other agents, the buzz/oar
  pattern) — natural follow-on once the client core exists; not in this plan.
- **Sampling, roots, logging feature support** — deprecated; do not adopt.
- **MCP Apps / tasks extensions** — only if real servers demand them; rmcp
  already models tasks, so enabling is a small later step.
- **Tool-search-driven deferred exposure** — measure server sizes first.
- **Android/iOS sandboxing of MCP subprocesses** — mobile policy excludes MCP.

## 14. Open questions

1. Should `exposure: "deferred"` combine with an existing `load_tools`
   mechanism or introduce a `tool_search` tool (pi/goose style)? Decide in P5
   with data from real servers.
2. *(Resolved by D13.)* The daemon keeps exactly one process per **resolved
   config per project scope**: a daemon-tier server is pooled by `slug`, a
   project's server by `(project_root, slug)`, and the pool ref-counts
   referencing sessions. Visibility is project-scoped (a session sees only the
   daemon tier plus its own project's servers). The per-server `shared: false`
   escape hatch gives a stateful server a private connection per session that
   uses it.
3. *(Post-ship, P3)* Where does `mcp login` live for the GUI (embedded daemon) —
   GUI modal or loopback browser? Follow the existing provider-OAuth plan
   (`docs/plans/provider-oauth.md`) precedent.
4. Is the 2 s startup budget right, or should server availability be fully lazy
   (tools appear when connected)? P1 shipped the 2 s batch budget (stragglers
   skipped and logged); revisit if real configs routinely miss it.
5. *(Resolved by D13.)* The config transport stays single-directory. The
   daemon-tier `mcp.json` and the trust store `trust.toml` live in the config
   dir and hot-reload via the existing watcher; the per-session project
   `.mcp.json` is NOT watched (project roots are unbounded) and reloads via the
   explicit `/mcp reload`.

## 15. Definition of done

Two sets: **M1 — ship without OAuth** (the active goal) and **post-ship** (P3).
P0–P6 and D9 are complete.

### M1 — ship without OAuth

- [x] A 2026-07-28 server (`server/discover`, per-request `_meta`, `resultType`)
      and a 2024-11-05…2025-11-25 server both work, selectable per server,
      covered by integration tests on **both** transports (P1 stdio, P2 HTTP).
- [x] The official conformance suite's client scenarios run green against the
      supported eras, with a committed baseline (P6). *(Both the legacy
      (2025-11-25) and 2026-07-28 eras run in CI as a matrix; every non-auth
      client scenario passes on both wires, with only the OAuth scenarios
      baselined.)*
- [x] Authentication without OAuth: a static token in `headers` reaches an
      authenticated server; a server that requires OAuth fails with the P5
      actionable error, not a hang or a raw status string. No credential store,
      no sign-in flow (P3, post-ship) — `f037383`.
- [x] Tool calls: parallel per server, cancellable, deadline-bounded, restart
      on crash, progress-streamed, with images attached, structured content
      preserved, and typed errors. *(Met.)*
- [x] Resources readable via wrapper tools (`list_resources`/`read_resource`,
      P4).
- [x] Tool and resource list changes propagate without a daemon restart
      (`subscriptions/listen` + registry hot-swap — P4).
- [x] Bounds enforced and tested: schema/text caps, the stdio frame cap, the
      per-server concurrency cap, a 1024-tool catalogue cap, a 32-level schema
      depth bound, a per-server `maxRestarts`, and a notification-rate limit
      (`1d7d239`, `0a85184`).
- [x] Tool names sanitized and collision-proofed (P5) — `f037383`.
- [x] `/mcp` status surface + `mcp list/add/remove/reconnect` CLI, per-server
      logs, and a `session_inspect` section (P5) — `8d27006`, `a348c89`,
      `f037383`.
- [x] `mcp` is a default cargo feature: a plain build has MCP; the opt-out
      paths (iOS embedded daemon, `--no-default-features`) are documented
      (D9, `0a85184`).
- [x] Release artifacts verified with the default (MCP-on) feature set; the
      static-musl build covers it and the size delta is recorded
      (`0a85184`; ~+11 MB / ~+21.7%).
- [x] `ARCHITECTURE.md`/`README.md`/rustdoc updated; `just pre-commit` green;
      release notes written from the commit messages. *(P0–P6 did exactly this,
      each commit its own release note.)*

### Post-ship

- [ ] Remote OAuth server sign-in works end-to-end with refresh and logout
      (P3).
- [ ] The remaining fast-follows land: `exposure` + deferred tool search, and
      the config-watcher auto-reload for `mcp_servers.json` (D12).
- [ ] **This plan document is deleted** once everything above — including P3 —
      is implemented. No source file, doc, comment, or commit message in the
      tree references it (grep for `mcp-modernization` returns nothing).
