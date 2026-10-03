# Plan: MCP modernization — stateless protocol (2026-07-28) and a first-class client

**Status:** proposed — design complete; nothing implemented (no source changes in this
change).
**Date:** 2026-10-03
**Targets:** `choreo-mcp` (protocol engine), `choreo-daemon` (`src/mcp/` manager +
`McpToolWrapper`), `choreo-tui` / `choreo-client-core` (later phases: `/mcp` control
surface), root `Cargo.toml` + `Cargo.lock` (new dependencies).
**Touches (when implemented):** `choreo-mcp` (rewritten around `rmcp`; new
`runtime.rs` / `session.rs` / `auth.rs` / `config.rs` modules), `choreo-daemon`
(`src/mcp/{mod,config,tool}.rs`, `src/daemon/open.rs`, `src/server/core.rs`,
`src/sessions.rs` for cancel plumbing), `choreo-tui` (`/mcp` UI), `choreo-proto`
(only if a new client message is added), the justfile (`doc_crates` stays current),
`ARCHITECTURE.md`, `README.md`, `packaging/`.

> **TL;DR.** The MCP specification's current revision is **2026-07-28**, a
> *stateless* protocol: the `initialize` handshake is gone, every request carries
> its protocol version and capabilities in `_meta`, servers advertise themselves
> through `server/discover`, server-to-client interaction happens through
> Multi Round-Trip Requests (MRTR) instead of server-initiated requests, and the
> Streamable HTTP transport dropped sessions, the GET stream, and resumability.
> Choreographr's client (`choreo-mcp`) still speaks the **2024-11-05** era: a
> hard-coded handshake, stdio only, tools only, no pagination, no cancellation,
> no resources/prompts, no HTTP servers, no OAuth, no output-limit or restart
> handling — and the daemon serializes every call to a server through one
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

### 1.1 What choreographr has today

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

Doc comment in `choreo-mcp/src/lib.rs` claims the crate speaks "over a transport
(stdio child process or HTTP)" — there is no HTTP path. Several declared error
variants (`ToolNotFound`, `InvalidParams`) are never constructed.

### 1.2 The protocol delta

The specification's revisions are `2024-11-05`, `2025-03-26`, `2025-06-18`,
`2025-11-25`, and the current **`2026-07-28`**. Choreographr sits three revisions
behind, which matters because:

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

### 1.3 Why now

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
**S4** = polish.

| # | Gap | Severity | Evidence |
|---|---|---|---|
| G1 | Cannot talk to 2026-07-28 (stateless-only) servers at all | S1 | `make_initialize_request` pins `2024-11-05` |
| G2 | No HTTP transport; no remote servers, no OAuth | S1 | `transport.rs` is stdio-only |
| G3 | Server crash is permanent for the daemon's lifetime | S1 | no restart anywhere in `mcp/` |
| G4 | One `Mutex` per server serializes calls and blocks shutdown | S1 | `Arc<Mutex<McpClient>>`, `tool.rs` |
| G5 | MCP images are never attached to the model (rendered as text) | S1 | `image_tx` ignored; `[Image: …]` placeholder |
| G6 | `inputSchema` default is JSON `null` when a server omits the key; invalid per spec | S2 | `#[serde(default)] input_schema: Value` in `protocol.rs` |
| G7 | `tools/list` ignores `nextCursor` → large servers lose tools silently | S2 | `client.rs::list_tools` |
| G8 | No `subscriptions/listen`; new tools require a daemon restart | S2 | notifications discarded in `transport.rs` |
| G9 | No `structuredContent`/`outputSchema` capture into `ToolOutput.result_json` | S2 | `tool.rs` mapping |
| G10 | No resources, prompts | S2 | no protocol types |
| G11 | No cancellation; a cancelled session leaves the MCP call running | S2 | `ctx.cancelled` never consulted |
| G12 | No progress notifications; `supports_streaming_output` emits one final chunk | S2 | `execute_streaming_json` |
| G13 | Unbounded stdout line + unbounded channels (memory DoS from a hostile server) | S2 | `transport.rs` |
| G14 | No per-server timeouts in config; fixed 60 s | S3 | `client.rs` constants |
| G15 | `auto_load` parsed but ignored | S3 | `config.rs` |
| G16 | `output_schema()` hard-codes `{"type":"string"}` | S3 | `tool.rs` |
| G17 | `isError` dropped on the postcard path; `describe_invocation_json` appends a stray period | S3 | `tool.rs` |
| G18 | Tool names unsanitized (`mcp/<slug>/<tool>` may exceed provider name limits or contain bad chars) | S3 | `tool.rs::new` |
| G19 | No startup budget: a hung server can hold up `DaemonState::open` | S3 | `open.rs` joins spawn threads |
| G20 | Integration tests depend on Node/npx + network | S3 | `tests/it/*` |
| G21 | No user surface: no `/mcp`, no status, no login/logout | S3 | nothing in `choreo-tui` |
| G22 | `lib.rs` claims HTTP support that does not exist; unused error variants | S4 | `lib.rs`, `error.rs` |

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

Feature set to enable: `client`, `transport-child-process`,
`transport-streamable-http-client-reqwest`, `client-side-sse` (via the
transport), `auth`, `elicitation`, `request-state`, `base64`, `schemars`;
`default-features = false`. TLS rides reqwest → rustls, whose crates
(`rustls`/`ring`/`webpki-roots`) are already in the lockfile.

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

### D4 — Config: extend `mcp_servers.json` in place; keep the path

Same file, same directory (`<config>/choreographr/mcp_servers.json`), same
`mcpServers` top level. New optional keys: `url`, `headers`, `oauth`, `timeout`,
`cwd`, `exposure`, `protocol`, `disabledTools`, with `${VAR}` expansion in
`env`/`headers` values. Existing keys keep their meaning. Unknown keys are
reported and ignored (never fatal). `auto_load` is replaced by `exposure`
(`direct` default here, `deferred`/`hidden` later); `auto_load: false` is honored
as `exposure: "deferred"` for backwards compatibility.

### D5 — Tool identity stays `mcp/<slug>/<tool>`

Persisted sessions store group names; renaming would strand them. Rules:
sanitize each segment to `[A-Za-z0-9_-]`, cap the full name at the provider limit
(64 chars for OpenAI; use 64 as the safe ceiling), append `-<6-hex hash>` when
sanitization or truncation collides. Description prefix `[MCP <slug>] ` is kept
(stable prompt text). `title`/`icons`/`annotations` are captured into group
metadata for later UI use but never fed to the model as instructions (untrusted).

### D6 — Concurrency, deadlines, cancellation

- Per-server config `timeout` (seconds; default 60, progress resets the deadline
  as pi does) and a global per-call ceiling.
- `maxConcurrentCalls` per server (default 4; HTTP servers may run parallel,
  stdio requests multiplex on rmcp).
- Cancellation: the daemon's session-cancel path calls
  `McpManager::cancel_session(session_id)` (a channel send), the dispatcher
  cancels the in-flight rmcp request (`notifications/cancelled` on stdio, stream
  close on HTTP) and replies a typed `Cancelled` error. The call wrapper also
  checks `ctx.cancelled` before starting (cheap, correct at the boundary).
- Shutdown: `McpManager::shutdown_all` sends `Shutdown` commands and joins
  dispatchers with a bounded wait; no MCP lock can block Ctrl-C.

### D7 — Credentials

OAuth tokens are stored per (issuer, resource) under
`<state>/choreographr/mcp-auth/` (0600 files, one per credential), never in the
DB, never in logs, never shared across issuers (SEP-2352). Config-file
`headers`/`env` secrets are redacted in logs and `session_inspect` output.
Remote-server auth is opt-in per server; a server without `oauth` simply gets no
`Authorization` header (explicit `headers` percolate as configured).

### D8 — Elicitation: advertise only with a UI

Advertise the `elicitation` capability only once a client surface can render a
prompt (TUI dialog / GUI modal). Until then, `input_required` results are
answered by retrying with `decline` responses (valid per MRTR) and a clear tool
error that names what the server asked for. This avoids promising a capability
we cannot honor — the spec forbids servers from demanding undeclared
capabilities, and a hard failure is better than a silent hang.

### D9 — Feature gating and policy stay as they are

`choreo-mcp` remains the only crate in the tree with MCP dependencies; the
daemon's `mcp` feature remains off by default; `ToolPolicy::Mobile` continues to
exclude MCP entirely (subprocesses + network are the mobile threat surface). The
new dependency tree (reqwest/rustls etc.) only compiles with the feature on.

### D10 — Content mapping

MCP content maps onto `ToolOutput` as follows:

| MCP | Mapping |
|---|---|
| `text` | joined with `\n` (current behavior) |
| `image` | base64 → `PreparedImage` via the daemon's image pipeline → `image_tx`; text placeholder only when no `image_tx` |
| `audio` | text descriptor `[Audio: <mime>, <bytes> — not attached]` |
| `resource_link` | text line with URI/name; future: register as a session resource |
| embedded `resource` | text content when inline; else descriptor |
| `structuredContent` | `ToolOutput.result_json` (programmatic path) + a JSON text block for the model |
| `annotations` | metadata only; never model-visible as instructions |
| `isError` | `ToolOutput.is_error` on every path (JSON, streaming, postcard) |

---

## 6. Target architecture

```
choreo-daemon (thread-only)
  └── mcp/  McpManager                      (config → servers, groups, status)
        ├── config.rs   mcp_servers.json → Vec<McpServerConfig>   [parsed once, reloadable]
        ├── tool.rs     McpToolWrapper: ToolDyn ≈ { name, group, schema, handle }
        └── server.rs   ServerSlot { status, handle: McpServerHandle, restart_policy }

choreo-mcp (library; owns tokio + rmcp)
  ├── runtime.rs    sidecar Runtime (init/get/block_on)            [choreo-content precedent]
  ├── config.rs     McpServerConfig (stdio | http fields, protocol mode, timeouts, oauth)
  ├── error.rs      McpError (spawn/connect/protocol/transport/auth/cancelled/timeout/…)
  ├── auth.rs       credential store + rmcp OAuth glue            [P3]
  └── session.rs    per-server dispatcher thread:
        McpCommand::{Discover, ListTools, ListPrompts, ListResources, Call, Cancel, Shutdown}
        ┌─────────────── dispatcher thread (current-thread tokio) ───────────────┐
        │ RunningService<RoleClient> (rmcp)                                      │
        │ cmd_rx.recv() → spawn(fut) on runtime → reply_tx.send(result)          │
        │ notifications → SubscriptionEvent channel (call progress / list change)│
        └────────────────────────────────────────────────────────────────────────┘
```

Data flow for one tool call:

1. Session thread executes `mcp/<slug>/<tool>` → `McpToolWrapper::execute_json`.
2. Wrapper sanitizes args (JSON string), sends `McpCommand::Call { tool, args,
   deadline, reply_tx, cancel_token }` to the server's dispatcher.
3. Dispatcher calls `peer.call_tool(...)` in a task; progress notifications are
   forwarded to the wrapper's `output_tx` (as text chunks) via the subscription
   event channel.
4. Reply → content mapping (D10) → `ToolOutput`.
5. Timeout/cancel: dispatcher cancels the rmcp request; wrapper returns a typed
   error; nothing is left running.

Lifecycle state machine per server: `disabled → connecting → ready → (needs_auth)
→ failed → retrying → ready`, with a restart budget (3 attempts, exponential
backoff to 60 s; a failed-connect cooldown so a bad server is not retried per
call). `list_changed` notifications trigger `ListTools` refresh and a
`ToolRegistry` group update (new tools registered, withdrawn ones dropped from
the group but existing session group membership unchanged).

## 7. Work breakdown

Each phase is independently shippable and lands with tests + docs.

### P0 — Correctness and safety on the current engine (small, no new deps)

- [ ] `inputSchema` fallback to `{"type":"object","additionalProperties":false}`;
      reject non-object schemas; cap schema bytes.
- [ ] Bounded stdout lines (8 MiB, codex's value) → close transport with a typed
      error instead of allocating unboundedly.
- [ ] Bounded channels: notification channel may drop (log at debug);
      response channel stays unbounded per-response (bounded by `next_id`
      discipline) or is bounded with backpressure.
- [ ] `clientInfo.version` from `env!("CARGO_PKG_VERSION")`; protocol version
      named constant.
- [ ] `isError` on the postcard path; `describe_invocation_json` cleanup;
      `output_schema()` → `None` when unknown.
- [ ] Per-server `timeout` in config (seconds) applied to all three operations.
- [ ] `auto_load` honored (deferred registration) or removed with a warning.
- [ ] Image content attached via `image_tx` (base64 decode → existing image
      pipeline), placeholder only when no sink exists.
- [ ] Explicit `max response bytes` truncation marker for text content.
- [ ] Fix the `lib.rs` HTTP claim; remove or use the dead error variants.
- [ ] Replace the npx integration tests with an in-tree fixture server (see §8)
      — unit + `#[ignore]` integration.

### P1 — `rmcp` engine behind the blocking facade (core swap)

- [ ] Add `rmcp` 3.5 to `[workspace.dependencies]`; `choreo-mcp` enables the
      feature set from D1 (`default-features = false`).
- [ ] `runtime.rs`: sidecar init/get/block_on (copy the `choreo-content`
      doc/error shape).
- [ ] `session.rs`: dispatcher thread + command/reply channels + restart policy;
      `McpServerHandle` (cloneable) replaces `Arc<Mutex<McpClient>>`.
- [ ] `McpClient` public API evolves to `McpServer::connect(config) →
      McpServerHandle`; `list_tools` with pagination; `call_tool` with deadline +
      cancellation; typed errors mapped from rmcp.
- [ ] `ClientLifecycleMode::Auto` wiring (preferred `[2026-07-28]`, legacy
      `2025-11-25`), per-server `protocol` override.
- [ ] Daemon: `McpManager` holds `ServerSlot`s; spawn in background with a
      startup budget (do not block `open()` past `startup_timeout_ms`, default
      2 s); `shutdown_all` sends commands and joins bounded.
- [ ] Cancel plumbing: `McpManager::cancel_session` from the daemon's cancel path.
- [ ] Unit tests: dispatcher protocol (mock engine), error mapping, config
      parsing; no sleeps (channels/synchronization primitives only).
- [ ] Integration tests: Rust fixture server speaking (a) 2026-07-28 discover,
      (b) 2025-11-25 initialize, (c) a misbehaving server (garbage line, oversized
      line, crash), (d) slow call for cancellation.

### P2 — Streamable HTTP transport

- [ ] Config: `url`, `headers`, `transport: "http" | "stdio" | "auto"`;
      `${ENV}` expansion; TLS via rmcp/reqwest defaults.
- [ ] Connect logic: modern POST probe; fall back per spec (recognized modern
      errors vs everything else); optional legacy SSE rejection with a clear
      error (we do not implement HTTP+SSE; it is deprecated).
- [ ] Retry policy: 408/429/5xx with capped exponential backoff and
      `Retry-After`; no retry on 4xx protocol errors.
- [ ] Timeouts: connect/read; SSE stream to final response.
- [ ] Integration tests against a local HTTP fixture (JSON + SSE responses,
      header validation, error taxonomy).

### P3 — OAuth for remote servers

- [ ] rmcp `auth`; protected-resource metadata discovery (RFC 9728) + AS
      metadata (RFC 8414) + `iss` validation (RFC 9207).
- [ ] Client registration: DCR per SEP-837 with `application_type`, CIMD support,
      pre-registered client config, callback on loopback with paste-URL fallback.
- [ ] Credential store (D7) + refresh + logout; keyed by issuer/resource.
- [ ] TUI/CLI: `mcp login <slug>`, `mcp logout <slug>`, `mcp list`; `/mcp`
      status page showing auth state.
- [ ] Tests: in-repo mock authorization server (borrow the shape of pi's
      `mcp-oauth-server.ts` and hermes' e2e fixtures), scope step-up, refresh,
      revocation.

### P4 — Feature plumbing into the daemon

- [ ] D10 content mapping, including `image_tx` and `result_json`.
- [ ] `subscriptions/listen` per server when `listChanged` is declared; tool-list
      refresh updates the registry group in place.
- [ ] Progress → `ToolResultChunk`s (rate-limited); logging notifications →
      per-server log file + tracing.
- [ ] MRTR loop for `input_required`: decline policy (D8) now; pluggable handler
      later.
- [ ] Resource tools (per server, when `resources` capability declared):
      `mcp/<slug>/read_resource` (+ `list_resources`), paginated; prompts tools
      deferred.
- [ ] `outputSchema` → `ToolDyn::output_schema()` (real schema) and
      `structuredContent` → `result_json` so programmatic callers and the VM see
      structured data.
- [ ] Cancellation end-to-end test (cancel a session mid-call; assert the server
      observed `notifications/cancelled` on stdio / stream close on HTTP).

### P5 — Configuration, UX, observability

- [ ] Config layers: user file + project file (`.choreographr/mcp_servers.json`),
      project overrides user per server (pi's merge rules).
- [ ] `choreographr mcp add/remove/list/login/logout/reconnect` CLI + `/mcp` TUI
      command; status rendered from `ServerSlot`.
- [ ] `session_inspect` includes MCP server status/tool counts (no secrets).
- [ ] Per-server log file (`mcp-<slug>.log`, size-capped rotation).
- [ ] Optional deferred exposure + tool search integration for servers with many
      tools (reuse `load_tools` groups; measure first).
- [ ] `mcp reload` (re-read config, add/remove/restart servers without a daemon
      restart).

### P6 — Bounds, conformance, hardening

- [ ] Caps: max tools per server (e.g. 1,024), max schema bytes/depth, max text
      bytes returned to a model, max concurrent calls, max restarts, max
      notification rate.
- [ ] Adopt the official `@modelcontextprotocol/conformance` suite for the
      client, run in CI for the eras we support, with a committed baseline
      (script under `scripts/`, results under `choreo-mcp/tests/conformance/`).
- [ ] Fuzz/config hardening: parse-time fuzzing for `mcp_servers.json` and the
      base64 header sentinel; property tests for the tool-name sanitizer.
- [ ] Supply-chain: lockfile review, `cargo deny` stays green, note `rmcp` in
      the dependency policy docs.
- [ ] Security checklist from §9 executed and recorded in the PR.

## 8. Testing strategy

Per AGENTS.md: unit tests live in `src/**/#[cfg(test)]` (no time-based waits);
integration tests live in `tests/it/` (one binary per crate, `#[ignore]`).

- **Fixture server.** A small Rust binary in `choreo-mcp/tests/fixtures/` (built
  by the test harness or invoked via `env!("CARGO_BIN_EXE_…")`) that speaks
  scripted scenarios: `discover`, `initialize`, `tools/list` (paged, with
  `list_changed`, with bad schemas, with 2026-style `_meta` validation),
  `tools/call` (text/image/audio/resource/structured/isError), slow call,
  oversized line, crash-on-call, `notifications/progress`, and a `subscriptions/
  listen` stream. Scenario selection via argv/env. This removes the Node/npx
  dependency from CI.
- **Unit tests** cover: config parsing/migration, name sanitization, content
  mapping, error mapping, dispatcher state machine with a mock engine, backoff
  math, deadline handling.
- **Integration tests** cover: stdio against the fixture server per era; HTTP
  against a local `tiny_http`-style fixture; cancellation; restart; shutdown
  under load; the full `McpManager` → `ToolRegistry` path (the existing
  `choreo-daemon/tests/it/mcp_integration.rs` grows these cases).
- **Conformance** runs the official suite (P6) and diffs against a baseline.
- **Manual interop matrix** (documented, run at release): current
  `@modelcontextprotocol/server-everything`, a filesystem server, a remote OAuth
  server (e.g. an MCP provider available to the project), and one legacy server.
- **Determinism**: no `sleep`-based tests; deadlines exercised with short
  injected timeouts through config, not wall-clock sleeps.

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

Target config shape (superset, all keys optional except command/url):

```json
{
  "mcpServers": {
    "filesystem": {
      "command": "npx",
      "args": ["-y", "@modelcontextprotocol/server-filesystem", "."],
      "env": { "TOKEN": "${FS_TOKEN}" },
      "cwd": "~/work",
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

Surfaces (P5): `choreographr mcp list|add|remove|login|logout|reconnect`,
TUI `/mcp` (status, sign-in, reconnect, enable/disable, exposure), a
`session_inspect` section, and per-server log files.

## 11. Documentation deliverables

- `ARCHITECTURE.md`: rewrite the `choreo-mcp` module table (§`choreo-mcp`), the
  `mcp/` daemon row, the feature list row, the test-coverage table rows, and the
  threading-model paragraph (sidecar + dispatcher). Keep the "off by default"
  statement.
- `README.md`: update the MCP capability lines (transport, features, OAuth).
- `choreo-mcp` rustdoc: module headers per the migration rules; keep
  `#![warn(missing_docs)]` green (`doc-check` covers the crate).
- Release note: one `feat(choreo-mcp):` commit (or a small series: `feat`,
  `fix`, `refactor`) with user-facing prose. `choreo-mcp` and `choreo-daemon`
  scopes preferred.

## 12. Risks & mitigations

| Risk | Mitigation |
|---|---|
| Dependency weight (tokio/reqwest/hyper via rmcp) | `mcp` feature stays off by default; verify both `--no-default-features` and `--all-features` builds; static-musl release job with the feature enabled (P6). |
| rmcp API churn (3.x is moving fast) | Pin `3.5`, upgrade deliberately; the blocking facade isolates the daemon from rmcp types (rmcp types do not cross the crate boundary). |
| Sidecar runtime + threads complicate shutdown | Follow the `choreo-content` runtime pattern; dispatcher replies are bounded; `shutdown_all` joins with deadlines; add the "no MCP lock can wedge Ctrl-C" test. |
| OAuth UX on headless devices (TUI over SSH, Termux) | Paste-the-redirected-URL fallback (pi's flow), device-code path only if a provider requires it; document. |
| Tool-name collisions/limits change prompt text vs persisted sessions | Keep names stable; sanitize only what is invalid; hash only on collision; pin with tests. |
| rmcp licenses/advisories | Apache-2.0; `cargo deny check` already gates the tree. |
| Fixture server drifts from real servers | Keep the one `npx`-based interop test as an opt-in ignored test plus the official conformance suite. |

## 13. Out of scope / future work

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
2. Do we want the daemon to keep exactly one process per server even for many
   sessions (jcode-style shared pool), or per-session processes for stateful
   servers (`shared: false`)? Default stays one global process; add a per-server
   `"shared": false` escape hatch if a stateful server needs it.
3. Where does `mcp login` live for the GUI (embedded daemon) — GUI modal or
   loopback browser? Follow the existing provider-OAuth plan
   (`docs/plans/provider-oauth.md`) precedent.
4. Is the 2 s startup budget right, or should server availability be fully lazy
   (tools appear when connected)? Leaning eager-with-budget + lazily updated
   groups.

## 15. Definition of done

- A 2026-07-28 server (`server/discover`, per-request `_meta`, `resultType`)
  and a 2024-11-05…2025-11-25 server both work, selectable per server, proven
  by integration tests and the official conformance suite baseline.
- Stdio and Streamable HTTP transports work; remote OAuth server sign-in works
  end-to-end with refresh and logout.
- Tool calls: parallel per server, cancellable, deadline-bounded, restart on
  crash, progress-streamed, with images attached, structured content preserved,
  and typed errors.
- Tool list changes propagate without a daemon restart; resources readable via
  wrapper tools.
- Bounds (lines, tools, schemas, bytes, concurrency) enforced and tested.
- `/mcp` + CLI surfaces report status and manage auth/reload.
- `ARCHITECTURE.md`/`README.md`/rustdoc updated; `just pre-commit` green; release
  notes written from the commit messages.
