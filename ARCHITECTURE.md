# tai Architecture

## Overview

`tai` is a local-first AI assistant built as a Rust workspace. A **daemon** process
communicates with an OpenAI-compatible HTTP API, while **clients** (terminal,
desktop, and IM platforms) connect to the daemon over a Unix domain socket using a
custom length-prefixed binary protocol.

```
┌──────────────┐    Unix socket     ┌──────────────┐    HTTP/SSE     ┌───────────────┐
│   tai-tui     │◄──────────────────►│              │◄──────────────►│  OpenAI API   │
│  (terminal)  │                    │              │                │               │
├──────────────┤                    │  tai-daemon  │                └───────────────┘
│ tai-dioxus   │◄──────────────────►│              │
│  (desktop)   │    Unix socket     │              │
├──────────────┤                    │              │
│   tai-im     │◄──────────────────►│              │
│ (IM bridge)  │    Unix socket     │              │
└──────────────┘                    └──────┬───────┘
                                          │
                                   ┌──────┴───────┐
                                   │ tai-keystore │
                                   │ (encrypted)  │
                                   └──────────────┘
```

---

## Workspace topology

Seven crates in a single Cargo workspace (resolver = "3"):

```
tai (workspace)
├── tai-proto           Wire protocol (shared types + framing)
├── tai-keystore        Encrypted credential storage (+ CLI binary)
├── tai-client-core     Shared client logic (parsing, markdown, images, history)
├── tai-daemon          Unix socket server — the core engine
├── tai-tui              Terminal UI client (ratatui + crossterm)
├── tai-dioxus          Desktop GUI client (Dioxus)
└── tai-im              IM platform bridge (Telegram)
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
     ┌────────┼────────┬─────┘
     │        │        │
┌────▼───┐ ┌──▼───┐ ┌──▼───┐
│tai-tui  │ │tai-  │ │tai-  │
│        │ │dioxus│ │im    │
└────────┘ └──────┘ └──────┘
                        │
                  ┌─────▼─────┐
                  │  tai-proto │
                  └───────────┘
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
`TestImage`, `Cancel`, `Ping`, `GetCredential`, `ListModels`, `SetModel`, `Unlock`,
`Lock`, `AddApiKey`, `AddXCredential`, `RemoveCredential`

`DaemonMessage` variants:
- Session: `SessionCreated`, `Sessions`, `SessionAttached`, `SessionState`, `SessionMessageAppended`, `SessionFailed`
- Request lifecycle: `Started`, `OutputChunk`, `Done`, `Failed`, `Cancelled`
- Tool lifecycle: `ToolCallStarted`, `ToolCallFinished`, `ToolCallFailed`
- Image streaming: `ImageStart`, `ImageChunk`, `ImageEnd`
- Model management: `Models`, `ModelsFailed`, `ModelSelected`, `ModelSelectionFailed`
- Locking: `Unlocked`, `Locked`, `LockedError`
- Credential management: `CredentialAdded`, `CredentialAddFailed`, `CredentialRemoved`, `CredentialRemoveFailed`, `Credential`
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
- **Error type**: `ProtoError` (thiserror enum) — `Bincode`, `FrameTooLarge`, `TrailingBytes`, `UnsupportedVersion`, `Io`


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
**Error type:** `KeystoreError` (thiserror enum) — `Io`, `TooShort`, `InvalidMagic`, `UnsupportedVersion`, `InvalidKeyLength`, `EncryptionFailed`, `DecryptionFailed`, `InvalidData`, `AlreadyExists`, `ConfigDirNotFound`

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

`DaemonMessageHandler` trait uses `ClientError` (thiserror enum) — `Proto`, `Io`, `Utf8`, `ImageTooLarge`, `ImageExceedsSize`, `DuplicateImage`, `UnknownImage`, `ImageSizeMismatch`.


### `tai-daemon` — Core server

Entry point: `src/main.rs` → initializes tracing, creates `DaemonState`, runs socket server.

**Module breakdown:**

| Module | Purpose |
|---|---|
| `server.rs` | Accepts Unix connections, spawns per-client `handle_client` tasks. Dispatches all `ClientMessage` variants. Implements lock/unlock flow. |
| `sessions.rs` | `SessionState` management: CRUD, subscriptions, broadcasting, persistence. Sessions form a tree (parent → child sub-sessions), each with an optional CWD. |
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


### `tai-im` — IM platform bridge

Entry point: `src/main.rs`

Single binary (`tai-im`) that bridges IM platforms to the daemon.
The binary accepts a platform subcommand: `tai-im telegram`.

**Credentials:** The daemon serves platform credentials via the `GetCredential` wire
message, so `tai-im` does not depend on `tai-keystore`. The admin first stores the
bot token in the keystore (`tai-keystore add telegram <token>`) and unlocks the daemon.

**Module breakdown:**

| Module | Purpose |
|---|---|
| `main.rs` | CLI entry, daemon handshake (Unlock, GetCredential), platform dispatch |
| `bridge.rs` | `DaemonBridge` — Unix socket read/write tasks, text buffering, image assembly, `BridgeEvent` enum |
| `telegram.rs` | Telegram bot: teloxide polling, admin-only filter, command dispatch, HTML formatting |

**Data flow:**

```
Telegram user → teloxide polling → handle_message()
  → parse_input_line() → ClientMessage → bridge.send()
  → daemon
  → DaemonMessage → bridge reader task → BridgeEvent
  → send_daemon_event() → Telegram HTML/photo
```


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

Each tool receives an optional `cwd: Option<&Path>` parameter that represents the session's
working directory. Filesystem and Git tools resolve relative paths against this CWD.

### Available tools (29 total)

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
| **Sub-session** | `spawn_subsession` (spawns an autonomous child session with its own tool-calling loop) |

### spawn_subsession

`spawn_subsession` is a special tool: it does not implement the `Tool` trait. Instead, it is
intercepted in `run_agent_loop()` and handled with full access to `DaemonState` and the
`OpenAiClient`. When invoked:

1. A child session is created via `create_session_internal()` with the parent as
   `parent_session_id` and inheriting the parent's CWD.
2. The prompt argument is pushed as a `SystemText` message into the child session.
3. The child session runs its own `run_agent_loop()` (model → tools → model, up to 8 iterations).
4. The child's assistant text output is collected and returned to the parent as the tool result.
5. The child session persists in the database and is listable/attachable like any other session.


---

## Session architecture

### Data model

Sessions are persisted to a `redb` (v4) embedded key-value store at
`~/.local/share/tai-daemon/state.redb`. Three tables:

| Table | Key | Value |
|---|---|---|
| `sessions` | `u64` session ID | bincode(`SessionRecord`) |
| `session_messages` | `(u64, u32)` (session ID, index) | bincode(`SessionMessage`) |
| `meta` | `&str` | `u64` counter |

`SessionRecord` fields: `title`, `selected_model`, `parent_session_id`, `cwd`,
`message_count`, `created_at`.

### Session state (in-memory)

Each active session has a `SessionState` (wrapped in `Arc<Mutex<>>`):

- `title: Option<String>` — display name
- `selected_model: Option<String>` — AI model for this session
- `parent_session_id: Option<u64>` — parent session for sub-sessions
- `cwd: Option<PathBuf>` — working directory for filesystem tools
- `created_at: i64` — Unix timestamp of creation
- `messages: Vec<SessionMessage>` — conversation history (also persisted to DB)
- `active_requests: HashMap<u32, ActiveRequest>` — running request handles
- `subscribers: HashMap<u64, mpsc::Sender<DaemonMessage>>` — attached clients

### Hierarchy and CWD inheritance

Sessions form a tree: a session can have a `parent_session_id` pointing to another
session. When creating a child session, if no explicit CWD is provided, it inherits
the parent's CWD. This allows sub-sessions (subagents) to operate in the same
directory as their parent.

### Persistence lifecycle

- **Startup**: `new_daemon_state()` reads all sessions and messages from the DB,
  reconstructing the in-memory `HashMap`. If the DB is empty, a default session #1
  is created.
- **Session creation**: Writes a `SessionRecord` to the DB immediately.
- **Message append**: Each `SessionMessage` is written to the DB alongside the
  in-memory push via `append_message_and_persist()`.
- **Shutdown**: No explicit shutdown needed; redb commits are durable on write.

### Multiple concurrent sessions

Multiple sessions can have active requests running simultaneously. Each `SessionState`
tracks its own `active_requests: HashMap<u32, ActiveRequest>`. Tokio tasks spawned for
requests hold independent `Arc` references to their session, so sessions can run
concurrently and complete independently.

---


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

**Database:** `~/.local/share/tai-daemon/state.redb` (override via `TAI_DB_PATH` env var)

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

4. **Sessions, not per-client state** — sessions are independent from client connections. A
   session has its own model, CWD, and messages. Clients subscribe/unsubscribe from sessions
   via the broadcast system. Sessions persist in a redb database and survive daemon restarts.

5. **Session hierarchy** — sessions can have parent sessions (`parent_session_id`), forming a
   tree. Child sessions inherit their parent's CWD unless explicitly overridden. The
   `spawn_subsession` tool creates autonomous child sessions that run their own tool-calling
   loop and report results back to the parent.

6. **Tool-call loop in the daemon** — the daemon drives multi-turn tool interactions (up to 8
   iterations) rather than pushing that complexity to the client or model. The client just sees
   `ToolCallStarted`/`ToolCallFinished` events.

7. **Chunked image streaming** — images are streamed in ≤64 KiB chunks to avoid blocking the
   socket on large payloads. The client assembles and validates on receipt.

8. **Session subscription model** — multiple clients can subscribe to the same session. Events
   are broadcast to all subscribers except the originator, enabling shared session viewing.

9. **SSE streaming** — a custom `SseReader` (not a library) handles `data:` lines and `[DONE]`,
   giving full control over parsing and buffering behavior.

10. **Markdown as the intermediate format** — all text (tool output, assistant text, error
    messages) is treated as markdown and rendered as HTML (desktop) or shaped to terminal output
    (tai-tui), providing a consistent rendering layer.

11. **Flexible API format** — the daemon supports both OpenAI Chat Completions and Responses
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

# Run IM bridge (Telegram)
cargo run -p tai-im -- telegram
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
| `thiserror` | proto, keystore, client-core, daemon | Structured library error types |
| `anyhow` | daemon, tui, dioxus, im, keystore | Application error context & propagation |


---

## Error handling strategy

### Library crates — `thiserror`

Each library crate defines a structured error enum:

| Crate | Error type | Key variants |
|---|---|---|
| `tai-proto` | `ProtoError` | `Bincode`, `FrameTooLarge`, `TrailingBytes`, `UnsupportedVersion`, `Io` |
| `tai-keystore` | `KeystoreError` | `Io`, `TooShort`, `InvalidMagic`, `DecryptionFailed`, `AlreadyExists`, `ConfigDirNotFound`, … |
| `tai-client-core` | `ClientError` | `Proto`, `Io`, `Utf8`, `ImageTooLarge`, `ImageExceedsSize`, `DuplicateImage`, `UnknownImage`, `ImageSizeMismatch` |

Every library error type implements `From<ErrorType> for io::Error` for backward compatibility
with code that still uses `io::Result`. Binary crates convert library errors to `anyhow::Error`
automatically via the blanket `From<E: Error> for anyhow::Error` impl.

### Binary crates — `anyhow`

All binary `main()` functions return `anyhow::Result<()>`. Key boundaries (socket bind,
keystore load, config parse) use `.context()` to attach meaningful messages. Internal
functions use domain error types (`io::Result`, `ProtoError`, etc.) and `?` auto-converts to
`anyhow::Error`.

The `send_or_warn` fire-and-forget broadcast helper uses `anyhow::Error` formatting for
warning logs when a subscriber is disconnected.

### Tool errors

The `ToolError` enum (thiserror) covers common tool failure modes: invalid arguments, I/O
errors, network failures, etc. Tools return `Result<String, ToolError>` and convert to
`ToolResult` at the `Tool::execute()` boundary via `From<ToolError> for ToolResult`.
| `gix` | daemon | Git operations |
| `alloy` | daemon | EVM blockchain tools |
| `subxt` | daemon | Substrate blockchain tools |
| `teloxide` | tai-im | Telegram Bot API client |
| `tracing` | daemon | Structured logging |
