# tai Architecture

## Overview

`tai` is a local-first AI assistant built as a Rust workspace. A **daemon** process
communicates with an OpenAI-compatible HTTP API, while **clients** (terminal and
desktop) connect to the daemon over a Unix domain socket using a custom
length-prefixed binary protocol.

```
┌──────────────┐    Unix socket     ┌──────────────┐    HTTP/SSE     ┌───────────────┐
│   tai-tui     │◄──────────────────►│              │◄──────────────►│  OpenAI API   │
│  (terminal)  │                    │  tai-daemon  │                │               │
├──────────────┤                    │              │                └───────────────┘
│ tai-dioxus   │◄──────────────────►│              │
│  (desktop)   │    Unix socket     │              │
└──────────────┘                    └──────┬───────┘
                                          │
                                   ┌──────┴───────┐
                                   │ tai-keystore │
                                   │ (encrypted)  │
                                   └──────────────┘
```

---

## Workspace topology

Six crates in a single Cargo workspace (resolver = "3"):

```
tai (workspace)
├── tai-proto           Wire protocol (shared types + framing)
├── tai-keystore        Encrypted credential storage (+ CLI binary)
├── tai-client-core     Shared client logic (parsing, markdown, images, history)
├── tai-daemon          Unix socket server — the core engine
├── tai-tui              Terminal UI client (ratatui + crossterm)
└── tai-dioxus          Desktop GUI client (Dioxus)
```

### Dependency graph

```
                    ┌─────────────────┐
                    │    tai-proto    │ (no workspace deps)
                    └────────┬────────┘
                             │
                    ┌────────▼────────┐
                    │  tai-keystore   │ (no workspace deps)
                    └────────┬────────┘
                             │
              ┌──────────────┼──────────────┐
              │              │              │
     ┌────────▼────────┐     │     ┌────────▼────────┐
     │ tai-client-core │     │     │    tai-daemon   │
     └────────┬────────┘     │     └─────────────────┘
              │              │
     ┌────────┼────────┐     │
     │        │        │     │
┌────▼───┐ ┌──▼───┐    │     │
│tai-tui  │ │tai-  │    │     │
│        │ │dioxus│    │     │
└────────┘ └──────┘    │     │
                       │     │
                 ┌─────▼─────▼──┐
                 │  tai-proto    │
                 │  tai-keystore │
                 └──────────────┘
```

---

## Crate details

### `tai-proto` — Wire protocol

Defines all shared message types and framing. No dependencies on other workspace crates.

**Key types:**

| Type | Purpose |
|---|---|
| `ClientMessage` | Enum of all messages a client can send |
| `DaemonMessage` | Enum of all messages the daemon can send |
| `SessionMessage` | A single turn in a conversation |
| `ImageMetadata` | Mime type, dimensions, byte length for streamed images |

`ClientMessage` variants:
`CreateSession`, `ListSessions`, `AttachSession`, `GetSessionState`, `RunInput`,
`TestImage`, `Cancel`, `Ping`, `ListModels`, `SetModel`, `Unlock`, `Lock`,
`AddApiKey`, `AddXCredential`, `RemoveCredential`

`DaemonMessage` variants:
- Session: `SessionCreated`, `Sessions`, `SessionAttached`, `SessionState`, `SessionMessageAppended`, `SessionFailed`
- Request lifecycle: `Started`, `OutputChunk`, `Done`, `Failed`, `Cancelled`
- Tool lifecycle: `ToolCallStarted`, `ToolCallFinished`, `ToolCallFailed`
- Image streaming: `ImageStart`, `ImageChunk`, `ImageEnd`
- Model management: `Models`, `ModelsFailed`, `ModelSelected`, `ModelSelectionFailed`
- Locking: `Unlocked`, `Locked`, `LockedError`
- Credential management: `CredentialAdded`, `CredentialAddFailed`, `CredentialRemoved`, `CredentialRemoveFailed`
- Misc: `Pong`

**Wire format:**

```
┌──────────────────┬────────────────────────────────────────┐
│ 4 bytes (BE u32) │ bincode((protocol_version: u32, msg)) │
│   payload len    │                                        │
└──────────────────┴────────────────────────────────────────┘
```

- Protocol version: `1`
- Max frame size: 1 MiB
- Framing functions: `encode_frame`, `decode_frame`, `read_message`, `write_message`


### `tai-keystore` — Credential storage

Stores API keys and other secrets encrypted on disk.

**Encryption pipeline:**
```
passphrase ──► argon2 KDF ──► 256-bit key ──► AES-256-GCM encrypt(credentials)
```

**File format:**
```
┌──────┬───────┬──────┬───────┬────────────┐
│ TAIK │  ver  │ salt │ nonce │ ciphertext │
│  4B  │  1B   │ 16B  │  12B  │   ...      │
└──────┴───────┴──────┴───────┴────────────┘
```

**Credential types:** `ApiKey` (OpenAI), `X` (Twitter OAuth 1.0a credentials)

**CLI binary (`tai-keystore`):** `init`, `add`, `remove`, `list` subcommands.
Stored at `~/.config/tai-daemon/credentials.enc`. Override path via `TAI_KEYSTORE_PATH` env var.
Credentials can also be managed at runtime via `/add-key`, `/add-x`, and `/remove-key` shell
commands, which require the keystore passphrase as a parameter and do not depend on daemon
lock state.


### `tai-client-core` — Shared client logic

Used by both `tai-tui` and `tai-dioxus`.

| Module | Purpose |
|---|---|
| `shell.rs` | Parses terminal input into `ShellCommand`: `/ping`, `/models`, `/model` (alias), `/cancel`, `/unlock`, `/lock`, `/image`, or `RunInput(prompt)`. All commands use `/` prefix exclusively; `parse_command()` is the single dispatch point. |
| `markdown.rs` | Parses markdown into structured `MarkdownDocument` (paragraphs, headings, code blocks, lists, tables) via `pulldown-cmark`; `render_markdown_html()` sanitizes via `ammonia` |
| `image.rs` | `ImageAssembler` reconstructs images from chunked stream protocol (`ImageStart` → `ImageChunk`* → `ImageEnd`), validating byte count |
| `history.rs` | `ClientHistory` ring buffer of `HistoryItem` entries (text, images, session messages, streaming text) |


### `tai-daemon` — Core server

Entry point: `src/main.rs` → initializes tracing, creates `DaemonState`, runs socket server.

**Module breakdown:**

| Module | Purpose |
|---|---|
| `server.rs` | Accepts Unix connections, spawns per-client `handle_client` tasks. Dispatches all `ClientMessage` variants. Implements lock/unlock flow. |
| `sessions.rs` | `SessionState` management: CRUD, subscriptions, broadcasting. Sessions are stored as `HashMap<u64, Arc<Mutex<SessionState>>>`. |
| `requests.rs` | Prompt execution: builds messages from session history, runs model requests, drives tool-call loop. |
| `openai/` | HTTP integration with OpenAI-compatible APIs, SSE streaming, service config loading. |
| `tools/` | Tool trait, registry, and 28 registered tools. |

**Per-client architecture (async tasks):**

```
handle_client(socket)
├── reader task: reads ClientMessages from socket → sends to dispatch
├── writer task: receives DaemonMessages from mpsc → writes to socket
└── main loop: dispatches messages, modifies DaemonState, publishes events
```

**Request flow:**

```
RunInput received
  └► extract/validate session, check active requests
     └► if chat_completions + tools:
        └► tool-call loop (max 8 iterations):
           1. send messages + tools → model
           2. receive response
           3. if tool_call → execute Tool → append ToolResult → goto 1
           4. else → emit final text, Done
     └► if responses or chat_completions (no tools):
        └► stream chunks via SSE → emit OutputChunk per token → Done
```

### `tai-tui` — Terminal client

Entry point: `src/main.rs`

Three concurrent tasks:
```
main()
├── reader task: read DaemonMessages from socket → push to UI event channel
├── writer task: receive ClientMessages from mpsc → write to socket
└── UI loop: crossterm events (keyboard/mouse) + ratatui rendering
```

**Module breakdown:**

| Module | Purpose |
|---|---|
| `connection.rs` | Socket setup, event loop, input handling, daemon message dispatch |
| `state.rs` | `App` struct: input buffer, request tracking, `ClientHistory`, scroll state |
| `render.rs` | Ratatui rendering: history pane (top) + command input (bottom), word wrap, Unicode width |
| `lib.rs` | SVG rasterization (resvg), PNG/JPEG decoding (image crate), ratatui-image protocol picker |


### `tai-dioxus` — Desktop client

Entry point: `src/main.rs`

Same Unix socket transport, but rendered via Dioxus components. Uses hooks to spawn
async reader/writer tasks inside the Dioxus runtime.

**Module breakdown:**

| Module | Purpose |
|---|---|
| `client.rs` | `run_client()` — socket split, reader/writer, daemon message dispatch |
| `state.rs` | `AppState` with input, request tracking, `ClientHistory` |
| `render.rs` | RSX rendering of history items: markdown → sanitized HTML, images via `data:` URLs |
| `main.rs` | Dioxus `App` component, toolbar, history pane, textarea composer, CSS |


---

## Security model

### Lock/Unlock flow

The daemon starts in a **locked** state. No OpenAI client is constructed until
a client sends `/unlock <passphrase>`.

```
startup                    /unlock <passphrase>
   │                              │
   │  locked                      │  decrypt keystore
   │  (no OpenAI client)          │  extract API key
   │                              │  build OpenAiClient
   │                              │  validate credentials
   │                              │  → Unlocked (ready)
   ▼                              ▼
```

- Credentials never appear in config files, environment variables, or command-line args
- The keystore uses argon2 + AES-256-GCM for authenticated encryption
- `/lock` destroys the in-memory OpenAiClient, returning to locked state
- `LockedError` is sent if any client attempts a request while locked


---

## Tool system

### Tool trait

```rust
trait Tool {
    fn name(&self) -> &str;
    fn description(&self) -> &str;
    fn schema(&self) -> serde_json::Value;       // JSON Schema for the model
    async fn execute(&self, args: Value) -> ToolResult;
}
```

### Registry

Tools are registered in a `ToolRegistry` backed by a global `OnceLock<ToolRegistry>` singleton.
The daemon retrieves the global registry at startup and passes tool definitions to the model.

### Available tools (28 total)

| Category | Tools |
|---|---|
| **Filesystem** | `read_file`, `read_file_range`, `write_file`, `edit_file`, `list_files`, `line_count` |
| **HTTP** | `http_request` (GET/POST/HEAD with headers, body, timeout) |
| **Image** | `display_image` (from path, URL, base64, or SVG text) |
| **Git** | `git_status`, `git_diff`, `git_log`, `git_add`, `git_commit`, `git_push` |
| **EVM** | `evm_chain`, `evm_balance`, `evm_token_balance`, `evm_block`, `evm_transaction`, `evm_call`, `evm_gas`, `evm_logs`, `evm_nonce`, `evm_resolve` |
| **Substrate** | `subxt_chain`, `subxt_balance`, `subxt_query`, `subxt_block` |
| **File search** | `fff` (file finding) |
| **X/Twitter** | `x_post`, `x_search_recent`, `x_user_lookup` |


---

## Configuration

**Service config:** `~/.config/tai-daemon/config.toml`

```toml
base_url = "https://api.openai.com/v1"
model_list_path = "/models"
responses_path = "/responses"
chat_completions_path = "/chat/completions"
default_request_format = "chat_completions"     # or "responses"
chat_completions_max_tokens = 4096
streaming = true

[model_request_formats]                          # per-model overrides
gpt-5 = "responses"

[model_max_tokens]                               # per-model token caps
big-model = 4096
```

**Credential storage:** `~/.config/tai-daemon/credentials.enc` (encrypted, managed via `tai-keystore` CLI)

**Socket path:** `/tmp/tai.sock` (override via `TAI_SOCKET_PATH` env var)


---

## Data flow: a prompt from input to response

```
1. User types "hello" in tai-tui
        │
2. tai-client-core::shell::parse_input_line("hello")
   → ClientMessage::RunInput { request_id: 1, input: "hello" }
        │
3. tai-tui writer task serializes + frames → Unix socket → tai-daemon
        │
4. tai-daemon server.rs handles RunInput:
   - validates session exists and is attached
   - checks no duplicate active request_id
   - sends DaemonMessage::Started { request_id: 1 }
   - appends SessionMessage::UserText("hello") to session
   - calls requests.rs to execute
        │
5. requests.rs builds message array from session history
   → calls openai::requests to hit the API
        │
6. openai::requests streams SSE chunks
   → per chunk: DaemonMessage::OutputChunk { request_id: 1, stream: true, data: "Hello" }
        │
7. DaemonMessage is serialized + framed → socket → tai-tui
        │
8. tai-tui reader task receives OutputChunk
   → pushes to UI event stream
        │
9. UI loop consumes event → updates ClientHistory → re-renders
        │
10. Final chunk arrives → DaemonMessage::Done { request_id: 1 }
    tai-tui marks request complete, adds session message
```

### Image flow (tool-triggered)

```
Model calls display_image tool
  → daemon executes tool
  → ToolExecutionOutput with PreparedImage
  → DaemonMessage::ImageStart { request_id, metadata }
  → DaemonMessage::ImageChunk { request_id, data (≤64 KiB) } × N
  → DaemonMessage::ImageEnd { request_id }
  → client ImageAssembler reconstructs full image
  → client renders image in terminal (ratatui-image) or desktop (data: URL)
```


---

## Design decisions

1. **Unix sockets, not HTTP for client↔daemon** — keeps everything local, avoids port conflicts,
   leverages OS-level access control.

2. **Binary protocol (bincode), not JSON** — compact, typed, versioned. Length-prefixed framing
   avoids parsing ambiguities. Version field allows protocol evolution.

3. **Lock/Unlock security** — the daemon starts without credentials in memory. The passphrase is
   never stored. This avoids secrets in env vars, config files, or command-line arguments.

4. **Per-client sessions, not global** — each client connection manages its own sessions via a
   `HashMap`, isolated from other connections. Model selection is per-connection.

5. **Tool-call loop in the daemon** — the daemon drives multi-turn tool interactions (up to 8
   iterations) rather than pushing that complexity to the client or model. The client just sees
   `ToolCallStarted`/`ToolCallFinished` events.

6. **Chunked image streaming** — images are streamed in ≤64 KiB chunks to avoid blocking the
   socket on large payloads. The client assembles and validates on receipt.

7. **Session subscription model** — multiple clients can subscribe to the same session. Events
   are broadcast to all subscribers except the originator, enabling shared session viewing.

8. **SSE streaming** — a custom `SseReader` (not a library) handles `data:` lines and `[DONE]`,
   giving full control over parsing and buffering behavior.

9. **Markdown as the intermediate format** — all text (tool output, assistant text, error
   messages) is treated as markdown and rendered as HTML (desktop) or shaped to terminal output
   (tai-tui), providing a consistent rendering layer.

10. **Flexible API format** — the daemon supports both OpenAI Chat Completions and Responses
    endpoints, selectable per-model. This lets users route different models to their best-supported
    endpoint.


---

## Testing strategy

| Layer | What's tested | Location |
|---|---|---|
| Protocol | Framing, version handling, round-trip encode/decode | `tai-proto/src/tests.rs` |
| Client core | Shell parsing, markdown→HTML, image assembly, history | `tai-client-core/src/tests.rs` |
| Daemon | Request lifecycle, session CRUD, cancellation, tool calls, model listing | `tai-daemon/src/tests.rs`, `tai-daemon/tests/integration.rs` |
| Daemon OpenAI | SSE parsing, HTTP request construction, config loading | `tai-daemon/src/openai/tests.rs` |
| tai-tui | SVG rasterization, Unicode width, app state | `tai-tui/src/app_tests.rs`, `tai-tui/src/lib_tests.rs` |
| tai-dioxus | App state, render helpers | `tai-dioxus/src/app_tests.rs` |

**Test infrastructure:** Tests use `UnixStream::pair()` for socket-less daemon↔client
communication, and mock HTTP servers for API simulation.

Run all tests:
```bash
cargo test
```


---

## Build and run

```bash
# Build everything
cargo build

# Build release
cargo build --release

# Run daemon
cargo run -p tai-daemon

# Run terminal client
cargo run -p tai-tui

# Run desktop client
cargo run -p tai-dioxus

# Run credential manager
cargo run -p tai-keystore -- init
cargo run -p tai-keystore -- add openai
cargo run -p tai-keystore -- list
```


---

## External dependencies (key crates)

| Crate | Used by | Purpose |
|---|---|---|
| `tokio` | all | Async runtime |
| `serde` + `bincode` | proto, daemon, clients | Serialization |
| `reqwest` (rustls) | daemon | HTTP client |
| `pulldown-cmark` + `ammonia` | client-core | Markdown parsing, HTML sanitization |
| `ratatui` + `crossterm` | tai-tui | Terminal UI |
| `dioxus` | tai-dioxus | Desktop UI |
| `image` + `resvg` | daemon, tai-tui | Image decoding, SVG rasterization |
| `aes-gcm` + `argon2` | keystore | Encryption, key derivation |
| `gix` | daemon | Git operations |
| `alloy` | daemon | EVM blockchain tools |
| `subxt` | daemon | Substrate blockchain tools |
| `tracing` | daemon | Structured logging |
