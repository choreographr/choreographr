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
|---|---|---|
| `shell.rs` | Parses terminal input into `ShellCommand`: `/ping`, `/models`, `/model` (alias), `/cancel`, `/unlock`, `/lock`, `/image`, or `RunInput(prompt)`. All commands use `/` prefix exclusively; `parse_command()` is the single dispatch point. |
| `markdown.rs` | Parses markdown into structured `MarkdownDocument` (paragraphs, headings, code blocks, lists, tables) via `pulldown-cmark`; `render_markdown_html()` sanitizes via `ammonia` |
| `image.rs` | `ImageAssembler` reconstructs images from chunked stream protocol (`ImageStart` → `ImageChunk`* → `ImageEnd`), validating byte count |
| `history.rs` | `ClientHistory` ring buffer of `HistoryItem` entries (text, images, session messages, streaming text, structured diffs) |
| `diff.rs` | Types for structured unified diff representation (`DiffLineKind`, `DiffLine`, `DiffHunk`, `FileDiff`) |

`DaemonMessageHandler` trait uses `ClientError` (thiserror enum) — `Proto`, `Io`, `Utf8`, `ImageTooLarge`, `ImageExceedsSize`, `DuplicateImage`, `UnknownImage`, `ImageSizeMismatch`.


### `tai-daemon` — Core server

Entry point: `src/main.rs` → initializes tracing, creates `DaemonState`, runs socket server.

**Concurrency model:** Pure OS threads with message passing (actor model). No async code
in the daemon's own logic. All I/O uses blocking `std` APIs on dedicated threads.

**Module breakdown:**

| Module | Purpose |
|---|---|
| `server/lifecycle.rs` | Accept loop (non-blocking `UnixListener` + 50ms poll), signal handling (`signal_hook::flag`), shutdown orchestration. |
| `server/connection.rs` | Per-client `client_thread` — reads `ClientMessages` from socket, dispatches via `daemon_tx` mpsc channel. |
| `daemon.rs` | `DaemonCommand` handler loop on a dedicated thread — session CRUD, attach/detach, listing, locking. `DaemonState` is owned by this thread only (no shared state). |
| `sessions.rs` | `SessionState` management: CRUD, subscriptions, broadcasting, persistence. Each session has a control thread; request work runs on separate worker threads. Sessions form a tree (parent → child sub-sessions), each with an optional CWD. |
| `requests.rs` | Prompt execution: builds messages from session history, runs model requests, drives tool-call loop. |
| `context.rs` | Context file discovery, skills, fingerprint-based refresh. |
| `openai/` | HTTP integration with OpenAI-compatible APIs, SSE streaming, service config loading. |
| `tools/` | Tool trait, registry, and 20 registered tools. |
| `tools/vm.rs` | RISC-V sandbox: compiles Rust → ELF via rustc, executes in `ckb-vm` with custom syscall handler (`TaiSyscall`) for tool dispatch. |

**Per-client architecture (OS threads):**

```
client_thread(socket)
├── reads ClientMessages from socket via tai-proto read_message_sync
├── sends DaemonCommands via daemon_tx mpsc channel
└── receives DaemonMessages via per-client mpsc receiver → writes to socket
```

**Thread topology:**

```
main()
├── listener thread — UnixListener accept loop (non-blocking poll)
│   └── per client: spawns client_thread (std::thread::spawn)
├── command thread — DaemonCommand receiver loop (daemon_tx mpsc)
│   └── owns DaemonState (exclusive access, no Arc<Mutex>)
├── per-session threads — spawned on CreateSession, reaped on Shutdown
│   └── owns SessionState (exclusive access)
└── main thread — polls shutdown flag every 200ms, orchestrates clean exit
```

**Request flow:**

```
RunInput received
  └► extract/validate session, check active requests
     └► if chat_completions + tools:
        └► tool-call loop (max configurable iterations, default 25):
           0.5. re-check context fingerprint, rebuild volatile context if changed
           1. send messages + tools → model
           2. receive response
           3. if tool_call → execute Tool → append subdirectory hints to ToolResult → goto 0.5
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
               │
               └── per-frame sequence:
                   1. Drain all pending crossterm events (zero-timeout poll)
                   2. Drain UI event channel (daemon messages)
                   3. Consume scroll accumulator → apply batched delta
                   4. Update history viewport dimensions from terminal size
                   5. Clamp scroll state to valid range
                   6. Render via ratatui terminal.draw()
                   7. Blocking poll (~16 ms) to pace frame rate
```

**Module breakdown:**

| Module | Purpose |
|---|---|---|
| `connection.rs` | Socket setup, event loop, shutdown signal handling, input/keyboard/mouse dispatch, daemon message routing. Mouse scroll events are accumulated per-frame rather than applied immediately — the delta is consumed in batch before each render (see `apply_scroll_delta`). |
| `state.rs` | `App` struct: input buffer, request tracking, `ClientHistory`, scroll state (`HistoryScrollState`), and the per-frame scroll accumulator (`scroll_accumulator`) consumed by `apply_scroll_delta()`. |
| `render.rs` | Ratatui rendering: history pane (top) + command input (bottom), word wrap, Unicode width. Includes side-by-side diff rendering with syntax-highlighted per-token spans overlaid on red/green structural diff backgrounds. Does **not** mutate scroll state or viewport dimensions — those are updated in the event loop before `terminal.draw()`. |
| `syntax.rs` | Shared syntect helpers (`syntax_set`, `theme_set`, `highlight_theme`, `to_ratatui_color`, `language_for_path`). Extracted to avoid duplication between `markdown_render.rs` and `diff_render.rs`. |
| `diff_render.rs` | Diff parser and side-by-side pane builder. Detects unified diff text, parses into `FileDiff` structs, builds aligned left/right display rows. Applies per-token syntax highlighting via the two-bucket algorithm (same approach as opencode's `@pierre/diffs`): all deletion lines are concatenated into one pseudo-file and highlighted as a whole, all addition lines into another, giving syntect the sequential context it needs for accurate tokenization. |
| `markdown_render.rs` | Terminal markdown renderer. Parses markdown (via `tai-client-core`'s `pulldown-cmark` wrapper), renders blocks (paragraphs, headings, code, lists, tables, block quotes) into styled `ratatui::text::Line` vectors. Code blocks are syntax-highlighted via `syntect` (shared setup from `syntax.rs`). |
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
| `render.rs` | RSX rendering of history items: markdown → sanitized HTML, images via `data:` URLs, structured diffs via `format_diff_file` |
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
trait Tool: Send + Sync {
    fn name(&self) -> &str;
    fn description(&self) -> &str;
    fn schema(&self) -> serde_json::Value;       // JSON Schema for the model
    fn execute(&self, arguments_json: &str, x_credentials: Option<&XCredentials>, cwd: Option<&Path>) -> ToolExecutionOutput;
}
```

### Registry

Tools are registered in a `ToolRegistry` owned by `DaemonStateInner`, constructed once
at daemon startup. The agent loop extracts an `Arc<ToolRegistry>` from the daemon state to
list available tool definitions and dispatch tool execution.

Each tool receives an optional `cwd: Option<&Path>` parameter that represents the session's
working directory. Filesystem and Git tools resolve relative paths against this CWD.

### Available tools (up to 34 total, some dependent on installed binaries)

| Category | Tools |
|---|---|
| **Filesystem** | `read_file`, `read_file_range`, `write_file`, `edit_file`, `list_files`, `line_count` |
| **HTTP** | `http_request` (GET/POST/HEAD with headers, body, timeout) |
| **Image** | `display_image` (from path, URL, base64, or SVG text) |
| **Git** | `git_status`, `git_diff`, `git_log`, `git_add`, `git_commit`, `git_push` |
| **EVM** | `evm_chain`, `evm_balance`, `evm_token_balance`, `evm_block`, `evm_transaction`, `evm_call`, `evm_gas`, `evm_logs`, `evm_nonce`, `evm_resolve` |
| **File search** | `fff` (file finding) |
| **RISC-V VM** | `run_riscv` (compile & run Rust code in a sandboxed RISC-V VM with access to all registered tools) |
| **Shell** | `exec` (direct program execution), `sh` (bash/dash/zsh — detected at startup), `nushell` (if `nu` is installed), `fish` (if `fish` is installed) |
| **X/Twitter** | `x_post`, `x_search_recent`, `x_user_lookup` |
| **Sub-session** | `spawn_subsession` (spawns an autonomous child session with its own tool-calling loop) |
| **Skills** | `load_skill` (loads the full instructions for a skill by name, following the Agent Skills standard) |

### Tool categories

Tools are organized into categories to reduce context overhead. Each tool declares its category
via `fn category() -> &'static str` on the `Tool` trait. Categories are:

| Category | Default | Description |
|---|---|---|
| `core` | always on | File system, HTTP, images, file search |
| `git` | on | Local Git operations |
| `shell` | on | Shell and exec |
| `x` | off | X/Twitter API |
| `vm` | off | RISC-V sandboxed code execution |

The system prompt lists all categories and their descriptions. The model uses `load_tools` to
activate additional categories and `unload_tools` to deactivate them. **core** cannot be unloaded.

Categories affect only tool **availability** in the API `tools` array — they are a discovery
mechanism, not access control. The RISC-V VM (`run_riscv`) always has access to all registered
tools regardless of category state.

Implementation details:
- `ToolRegistry::available_definitions(active)` filters by `active: &HashSet<String>`
- `load_tools`/`unload_tools` are intercepted in `execute_tool_with_timeout()` (same pattern as
  `spawn_subsession` and `load_skill`)
- Session state stores `active_categories: HashSet<String>` (default: `{core, git, shell}`)
- `ToolCategory` struct and `CATEGORIES` constant live in `tai-daemon/src/tools/mod.rs`
- Handler functions live in `tai-daemon/src/tools/categories.rs`
- Category metadata is appended to the system prompt in `context::build_base_prompt()`

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

Each active session has a `SessionState` owned by its control thread:

- `title: Option<String>` — display name
- `selected_model: Option<String>` — AI model for this session
- `parent_session_id: Option<u64>` — parent session for sub-sessions
- `cwd: Option<PathBuf>` — working directory for filesystem tools
- `max_turns: Option<u32>` — per-session tool loop iteration cap (inherits from parent)
- `created_at: i64` — Unix timestamp of creation
- `messages: Vec<SessionMessage>` — conversation history (also persisted to DB)
- `active_requests: HashMap<u32, ActiveRequest>` — running request cancel flags
- `subscribers: HashMap<u64, mpsc::Sender<DaemonMessage>>` — attached clients
- `active_categories: HashSet<String>` — tool categories active for this session

### Hierarchy and CWD inheritance

Sessions form a tree: a session can have a `parent_session_id` pointing to another
session. When creating a child session, if no explicit CWD or `max_turns` is provided,
it inherits the parent's value. This allows sub-sessions (subagents) to operate in the
same directory as their parent with a default iteration cap.

### Persistence lifecycle

- **Startup**: `new_daemon_state()` reads all sessions and messages from the DB,
  reconstructing the in-memory `HashMap`. If the DB is empty, a default session #1
  is created.
- **Session creation**: Writes a `SessionRecord` to the DB immediately.
- **Message append**: Each `SessionMessage` is written to the DB alongside the
  in-memory push via `append_message_and_persist()`.
- **Shutdown**: The daemon sends `SessionCommand::Shutdown` to each active session, waits for request workers to drain, then exits cleanly.

### Multiple concurrent sessions

Multiple sessions can be active at the same time. Each session control thread stays
responsive while at most one request worker runs for that session. Request workers own a
snapshot of the session state and use cooperative cancellation via an `AtomicBool`.

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
max_turns = 25                                   # default tool loop iteration cap

[model_request_formats]                          # per-model overrides
gpt-5 = "responses"

[model_max_tokens]                               # per-model token caps
big-model = 4096
```

**Credential storage:** `~/.config/tai-daemon/credentials.enc` (encrypted, managed via `tai-keystore` CLI)

**Database:** `~/.local/share/tai-daemon/state.redb` (override via `TAI_DB_PATH` env var)

**Socket path:** `/tmp/tai.sock` (override via `TAI_SOCKET_PATH` env var)

**Tool loop limit:** `TAI_MAX_TURNS` env var overrides `config.toml` `max_turns`. Resolution
chain: per-session `max_turns` → `TAI_MAX_TURNS` env var → `config.toml` → default 25.
The `spawn_subsession` tool accepts an optional `max_turns` parameter; if not set, the
child inherits the parent's value.

**Logging:** `tai-daemon` uses `tracing` with `tracing-subscriber`. Default level is `info`.
CLI flags `-v` (debug), `-vv` (trace), or `-q` (warn) override the level.
`RUST_LOG` env var takes precedence over CLI flags.

**Session persistence:** On daemon start, sessions are loaded from the database into
`session_metadata` (in-memory). Model selection (`/models <name>`) updates both the
in-memory metadata and the database via `UpdateMetadata → db::write_session`. The
`AttachSession` handler also populates `session_metadata` when re-loading a session
from the database, ensuring `ListModels` and metadata queries see the correct
`selected_model`.

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

6. **Tool-call loop in the daemon** — the daemon drives multi-turn tool interactions (up to a
   configurable `max_turns` per session, default 25) rather than pushing that complexity to
   the client or model. The client just sees `ToolCallStarted`/`ToolCallFinished` events.

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

12. **OS threads with sidecar async runtime** — the daemon avoids async Rust everywhere except
    where third-party libraries (alloy) require it. A global `OnceLock<tokio::runtime::Runtime>`
    serves as a sidecar for those async calls via `block_on()`. This simplifies the mental model
    (each thread owns its data, no `Send` bounds on shared state, no `Pin<Box<dyn Future>>`),
    improves stack traces, and avoids the complexity of async cancellation.




---

## Context file discovery

The daemon automatically discovers and injects project-specific context files
(`AGENTS.md`, `CLAUDE.md`) and skills at session creation, and refreshes them
before every model call.

### Split-tier system prompt

Each session starts with two `SystemText` messages:

```
messages[0] = stable base prompt (identity, tool guidance, skill metadata)
messages[1] = volatile project context (AGENTS.md, CLAUDE.md, etc.)
```

- `messages[0]` never changes within a session — fully cacheable by the model provider.
- `messages[1]` is re-checked before every tool-loop iteration. If any context file
  changed on disk (new file, deletion, or modified mtime), it is rebuilt in-place
  without touching the stable tier.

### Discovery algorithm

1. **Global files** (loaded first, prepended):
   - `~/.config/tai-daemon/AGENTS.md`
   - `~/.claude/CLAUDE.md` (unless `disable_claude_code_prompt` is set)
   - `~/.agents/AGENTS.md`
2. **Project files** (walking from session CWD up to the git repository root):
   - At each ancestor directory, checks `AGENTS.md` first, then `CLAUDE.md`.
   - Only one file per directory (first match in the configured `context_file_names` list).
   - Collected bottom-up (outermost first), then rendered in reverse order so
     closer-to-CWD instructions appear last.

### Subdirectory hints

When filesystem tools (`read_file`, `list_files`, `fff`, etc.) access a file
in a subdirectory below the session CWD, the daemon walks up from that file's
parent toward CWD and checks for `AGENTS.md`/`CLAUDE.md` files not already in
the main context. Any found hint content is appended to the tool result message
(not the system prompt), preserving prompt cache stability.

### Skills (Agent Skills standard)

Skills are discovered from:
- `~/.agents/skills/<name>/SKILL.md` (global)
- `.agents/skills/<name>/SKILL.md` (project, relative to session CWD)

Each `SKILL.md` must have YAML frontmatter with `name` and `description`.

**Progressive disclosure:** At session start, only metadata (name + description)
is included in the stable prompt (`messages[0]`). When the model calls the
`load_skill` tool with a skill name, the full `SKILL.md` body is loaded and
injected as a new `SystemText` message.

### Fingerprint-based refresh

Before each turn in the tool-call loop, the daemon computes a SHA-256 fingerprint
of all known context file paths and their mtimes. If the fingerprint matches the
stored value, nothing changes. If it differs (file added, removed, or modified),
`messages[1]` is rebuilt with the new content and the fingerprint is updated.

### Configuration

```toml
# ~/.config/tai-daemon/config.toml
[context]
context_file_names = ["AGENTS.md", "CLAUDE.md"]   # ordered list; first match per directory
context_file_max_bytes = 32768                     # max combined context size
disable_claude_code_prompt = false                 # skip ~/.claude/CLAUDE.md
```

### User system prompt override

The stable base prompt (`messages[0]`) is loaded from
`~/.config/tai-daemon/system.md` if it exists. Otherwise, a built-in default is
used. The default lives at `tai-daemon/system.md` in the repository and is
embedded at compile time via `include_str!`.

### Module

Implementation lives in `tai-daemon/src/context.rs`. Key entry points:

| Function | Purpose |
|---|---|
| `discover_context(cwd, config)` | Walk filesystem, return `ContextBundle` with all discovered files |
| `discover_skills(cwd)` | Scan Agent Skills directories, return `Vec<SkillMeta>` |
| `assemble_context(bundle)` | Render discovered files into an XML-like format for injection |
| `build_base_prompt(skills)` | Build the stable system prompt (identity + skill metadata) |
| `recheck_context(cwd, config, old_fp)` | Re-discover and compare fingerprints |
| `subdirectory_hints(tool_name, args, cwd, known)` | Return subdirectory hint content for tool results |
| `load_skill_body(name, cwd)` | Load the full body of a SKILL.md, stripping YAML frontmatter |

### New tool: `load_skill`

Registered alongside other tools in the tool loop. When the model calls
`load_skill(name)`, the daemon:

1. Finds the matching `SKILL.md` from `~/.agents/skills/` or `.agents/skills/`
2. Strips the YAML frontmatter
3. Appends a `SystemText` message with the skill body to the session
4. Returns `"Loaded skill: <name>"` as the tool result

The skill body is available to the model on all subsequent turns.

### `run_riscv` — RISC-V sandboxed code execution

`run_riscv` is a tool that compiles Rust source code into a RISC-V ELF binary and executes it
inside a sandboxed virtual machine powered by `ckb-vm`. It is registered manually (not via
`define_tool!`) to pass `x_credentials` and `cwd` through to the guest syscall handler.

**Execution flow:**

1. Accepts either Rust `source` or pre-compiled base64 `program`.
2. If `source` is provided, prepends a `#![no_std]` boilerplate (panic handler, entry point,
   `tai` module with `tool_call`, `write`, `exit` syscall wrappers, optional
   128 KB bump allocator enabled via the `allocator` parameter) and compiles via a single
   `rustc +nightly --target riscv64imac-unknown-none-elf` invocation in a temp directory.
3. Creates a `DefaultCoreMachine<u64, FlatMemory<u64>>` with 4 MB of flat memory.
4. Registers a `TaiSyscall` handler that intercepts three guest syscalls:
   - **Syscall #0 (TOOL_CALL)** — reads a JSON `ChatToolCall` from guest memory, dispatches it
     via the `ToolRegistry` (initialized into a process-level `OnceLock` by `ToolRegistry::build()`),
     and writes the result to the guest's output buffer.
   - **Syscall #1 (WRITE)** — copies guest data into an accumulator buffer that becomes the tool's
     output upon VM exit.
   - **Syscall #2 (EXIT)** — stops the VM.
5. Loads the ELF via `TraceMachine::load_program` and runs via `TraceMachine::run()`.
6. Returns the accumulated WRITE output as the tool result.

**Guest ABI** (auto-generated in the boilerplate):

```rust
pub mod tai {
    pub unsafe fn tool_call(request: &[u8], output: &mut [u8]) -> usize;
    pub fn write(data: &[u8]);
    pub fn exit(code: i32) -> !;
}
```

When `allocator: true` (the default), a `#[global_allocator]` bump allocator is also
included, enabling `alloc` crate types (`Vec`, `String`, `format!`, `Box`, etc.), and
`args()` is injected as a free function returning `Vec<Vec<u8>>`:

```rust
pub fn args() -> Vec<Vec<u8>>;
```

With `allocator: false`, `args()` is not available and the guest must access `argc`/`argv`
directly from the RISC-V ABI registers (`a0`/`a1` at `_start`).

**Safety:** The guest runs in an isolated VM with 4 MB of flat memory. All tool access goes
through the same `ToolRegistry` as the host agent, respecting the same `x_credentials` and `cwd`.
The guest cannot access host memory, syscalls, or files outside the VM without going through
registered tools.

### `exec` — direct program execution (no shell)

`exec` spawns a program directly without shell interpretation. The command is split into argv (program + args array) and passed to `execvp` — no pipes, redirects, glob expansion, or environment variable interpolation.

Sandboxing is identical to the shell tools: timeout, rlimits, env sanitization, path confinement, output truncation, and non-interactive stdin.

Use `exec` when the command is a single program with explicit arguments. Prefer it over `sh` when you don't need shell features — it avoids shell-injection surface.

### `sh` — POSIX shell command execution

`sh` runs shell commands via a POSIX-compatible shell (`bash`, `dash`, or `zsh`). The available variants are detected at startup by probing `PATH`; only installed shells are listed in the tool schema. The `shell` parameter must be explicitly specified (no default), and `sh` itself is intentionally excluded — use `bash`, `dash`, or `zsh` directly.

Sandboxing (shared across all shell/exec tools via `shell_util.rs`):

1. **Timeout** — the command is killed after a configurable timeout (default 30s, max 300s). A watchdog thread enforces the inner timeout; the outer tool loop timeout is extended to 300s for this tool.

2. **Resource limits** — set via `setrlimit` in the child (pre-exec): `RLIMIT_AS` (4 GB) prevents runaway memory allocation, `RLIMIT_FSIZE` (100 MB) prevents disk-filling writes.

3. **Environment sanitization** — dangerous env vars (`LD_PRELOAD`, `LD_LIBRARY_PATH`, `LD_AUDIT`, `LD_DEBUG`, `PYTHONPATH`, `PERL5LIB`, `RUBYLIB`, `DYLD_INSERT_LIBRARIES`) are stripped in the child before exec.

4. **Path confinement** — the resolved working directory is canonicalized and must be at or below the session CWD. Absolute paths or `..` traversals that escape the project directory are rejected.

5. **Output limits** — stdout/stderr are combined and truncated to 16 KB via `truncate_tool_output`, preventing context overflow.

6. **Non-interactive** — stdin is not connected. Commands that attempt to read from stdin will hang until the timeout.

### `nushell` — nushell command execution with sandboxing

`nushell` runs commands in a child `nu -c` process with the same sandboxing as `sh`. Registered only when the `nu` binary is found in `PATH`.

### `fish` — fish shell command execution with sandboxing

`fish` runs commands in a child `fish -c` process with the same sandboxing as `sh`. Registered only when the `fish` binary is found in `PATH`.


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
| `tokio` | tui, dioxus, im | Async runtime |
| `serde` + `bincode` | proto, daemon, clients | Serialization |
| `reqwest` (rustls) | daemon | HTTP client |
| `pulldown-cmark` + `ammonia` | client-core | Markdown parsing, HTML sanitization |
| `ratatui` + `crossterm` | tai-tui | Terminal UI |
| `dioxus` | tai-dioxus | Desktop UI |
| `image` + `resvg` | daemon, tai-tui | Image decoding, SVG rasterization |
| `syntect` | tai-tui | Syntax highlighting for code blocks (uses Sublime Text grammar files) |
| `aes-gcm` + `argon2` | keystore | Encryption, key derivation |
| `ckb-vm` | daemon | RISC-V VM interpreter for sandboxed code execution |
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
| `teloxide` | tai-im | Telegram Bot API client |
| `tracing` | daemon | Structured logging |
