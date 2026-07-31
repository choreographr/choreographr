# Choreographr Architecture

## Overview

`Choreographr` is a local-first AI assistant built as a Rust workspace. A **daemon** process
communicates with multiple LLM providers through a pluggable trait-based provider
system, while **clients** (terminal, desktop, and IM platforms) connect to the daemon
over a Unix domain socket (or Noise IK encrypted TCP for remote connections) using a custom length-prefixed binary protocol.

```
┌──────────────┐    Unix socket     ┌──────────────┐    HTTP/SSE     ┌──────────────────────┐
│   choreo-tui     │◄──────────────────►│              │◄──────────────►│  OpenAI API          │
│  (terminal)  │                    │              │                ├──────────────────────┤
├──────────────┤                    │  choreographr  │◄──────────────►│  Anthropic Messages   │
│ choreo-gui       │◄──────────────────►│              │                ├──────────────────────┤
│  (desktop)   │    Unix socket     │              │◄──────────────►│  Google Gemini API    │
├──────────────┤                    │              │                ├──────────────────────┤
│   choreo-im     │◄──────────────────►│              │◄──────────────►│  Mistral API          │
│ (IM bridge)  │    Unix socket     │              │                ├──────────────────────┤
└──────────────┘                    └──────────────┘                └──────────────────────┘
                                                                    │  30+ OpenAI-compat    │
                                                                    │  providers via catalog│
                                                                    └──────────────────────┘
```

---

## Workspace topology

Eleven crates in a single Cargo workspace (resolver = "3"):

```
Choreographr (workspace)
├── choreo-proto           Wire protocol (shared types + framing)
├── choreo-keystore        X25519 + ECDH keypair crypto, encrypted storage primitives
├── choreo-transport       Noise IK encrypted TCP transport abstraction
├── choreo-client-core     Shared client logic (parsing, images, history, credentials)
├── choreo-markdown        Markdown parser and HTML renderer (pulldown-cmark + ammonia)
├── choreo-mcp             MCP (Model Context Protocol) client — spawns subprocess servers,
│                       discovers tools, and dispatches tool calls over JSON-RPC stdio
├── choreographr          Unix socket server — the core engine
├── choreo-acp             ACP bridge — translates the Agent Communication Protocol
│                       (JSON-RPC over stdin/stdout) into choreo-proto messages over the
│                       daemon's Unix socket, enabling ACP-compatible editors (Claude
│                       Code, Cline, etc.) to interact with Choreographr sessions
├── choreo-tui             Terminal UI client (ratatui + crossterm)
├── choreo-gui             Desktop GUI client (Dioxus)
└── choreo-im              IM platform bridge (Telegram)
```

### Dependency graph

```
                    ┌─────────────────┐
                    │    choreo-proto    │ (no workspace deps)
                    └────────┬────────┘
                             │
                    ┌────────▼────────┐
                    │  choreo-keystore   │ (no workspace deps)
                    └────────┬────────┘
                             │
              ┌──────────────┼──────────────────────┐
              │              │                      │
      ┌────────▼────────┐   │       ┌───────────────▼──────────┐
      │ choreo-client-core │   │       │        choreographr        │
      └────┬───────┬────┘   │       └───────────────┬──────────┘
           │       │        │                       │
           │  ┌────▼────────▼────┐                  │
           │  │  choreo-transport   │◄─────────────────│───────────┐
           │  └────────┬────────┘                  │           │
           │           │                   ┌───────▼──────┐ ┌──▼────────┐
      ┌────▼───┐ ┌────▼────┐ ┌────▼───┐   │   choreo-mcp    │ │  choreo-acp  │
      │choreo-tui  │ │choreo-gui   │ │choreo-im  │   │ (MCP client)│ │(ACP bridge)│
      └────────┘ └─────────┘ └────────┘   └──────────────┘ └───────────┘
```

---

## Crate details

### `choreo-proto` — Wire protocol

Defines all shared message types and framing. No dependencies on other workspace crates.

**Key types:**

| Type | Purpose |
|---|---|
| `ClientMessage` | Enum of all messages a client can send |
| `DaemonMessage` | Enum of all messages the daemon can send |
| `SessionMessage` | A single turn in a conversation with `message_id: u32` (monotonically increasing per-session), `parent_id: Option<u32>` (links to the triggering user/ATU message for undo subtree traversal), `deleted: bool` (soft-delete for undo), a `created_at: TimestampMs` field and a `kind: SessionMessageKind` enum. Variants (`SessionMessageKind`): `SystemText`, `UserText`, `AssistantText`, `AssistantToolUse`, `ToolResult`, `DisplayedImage` (persisted image replay) |
| `ImageMetadata` | Mime type, dimensions, byte length for streamed images |
| `DisplayedImageRecord` | Binary image data + `ImageMetadata` for persisted image replay (carried inside `SessionMessageKind::DisplayedImage`) |
| `ReasoningCapability` | Struct with `available_effort_levels: Vec<String>` — the reasoning effort slugs a model supports (e.g. `"off"`, `"low"`, `"medium"`, `"high"`, `"max"`). Empty means reasoning is not supported. Cycle helper validates/rotates through slugs. |
| `TokenUsage` | Tracks LLM token consumption (`input_tokens`, `output_tokens`, `total_tokens`). Embedded in `SessionMessageKind::AssistantText` and `SessionMessageKind::AssistantToolUse` for per-turn accounting, in `SessionSummary` and `DaemonMessage::SessionState` for session-level totals, and in `DaemonMessage::Done` for per-request usage. |
| `last_prompt_tokens` | `Option<u32>` field on session metadata and protocol messages tracking the `input_tokens` from the most recent API response — the actual context size being sent to the model, used for context-window progress displays. |
| `SessionStatus` | Enum representing the current session state: `Inactive`, `Inference`, `ToolCall(String)`, `Retrying {…}`, `Sleeping`. Included in `SessionSummary` and `DaemonMessage::SessionState` for live status display in client toolbars. |
| `ToolResultRecord` | Persisted tool result with fields `call_id`, `name`, `content`, `is_error`, and `invocation_description` — a human-readable sentence from `Tool::describe_invocation` describing what the tool did. Used for UI display only; excluded from LLM message construction. |

`ClientMessage` variants:
`CreateSession`, `ListSessions`, `AttachSession`, `GetSessionState`, `RunInput`,
`TestImage`, `Cancel`, `Ping`, `GetCredential`, `ListModels`, `SetModel`, `Unlock`,
`Lock`, `AddCredential`, `RemoveCredential`, `AddAccount`, `RemoveAccount`,
`ListAccounts`, `SetSessionAccount`, `SetReasoningEffort`, `GetReasoningEffort`,
`Undo`, `Redo`, `ContinueGeneration`
- `CreateSession` now carries optional `context_config`, `account_name`, `selected_model`, and `reasoning_effort` (slug string) fields

`DaemonMessage` variants:
- Session: `SessionCreated`, `Sessions`, `SessionAttached`, `SessionState`, `SessionStatusChanged`, `SessionMessageAppended`, `SessionFailed`, `SessionDeleted`, `SessionDeleteFailed`
- Undo/Redo: `SessionMessagesUndone { message_ids }`, `SessionMessagesRedone { messages }`
- Request lifecycle: `Started` (now carries `message_id: u32` for client-side ordering), `OutputChunk`, `Done`, `Failed`, `Cancelled`
- Tool lifecycle: `ToolCallStarted` (now carries `message_id: u32` matching the subsequent `ToolResult`), `ToolCallFinished` (output removed — content delivered via `ToolResultChunk`), `ToolCallFailed`, `ToolCallOutput`, `ToolResultChunk`
- Model management: `Models`, `ModelsFailed`, `ModelSelected`, `ModelSelectionFailed`
- Locking: `Unlocked`, `Locked`, `LockedError`
- Credential management: `CredentialAdded`, `CredentialAddFailed`, `CredentialRemoved`, `CredentialRemoveFailed`, `Credential`
- Account management: `AccountAdded`, `AccountAddFailed`, `AccountRemoved`, `AccountRemoveFailed`, `Accounts`, `AccountListFailed`, `SessionAccountSet`
- Reasoning effort: `ReasoningEffortSet`, `ReasoningEffortSetFailed`
- Misc: `Pong`, `ShuttingDown`

**Wire format:**

```
┌──────────────────┬────────────────────────────────────────┐
│ 4 bytes (BE u32) │ postcard((protocol_version: u8, msg)) │
│   payload len    │                                        │
└──────────────────┴────────────────────────────────────────┘
```

- Protocol version: `1`
- Max frame size: 32 MiB
- Framing functions: `encode_frame`, `decode_frame`, `read_message`, `write_message`
- **Error type**: `ProtoError` (thiserror enum) — `Postcard`, `FrameTooLarge`, `TrailingBytes`, `UnsupportedVersion`, `Io`


### `choreo-keystore` — Identity keypair & credential crypto

Provides the cryptographic primitives for credential management. No longer a standalone
CLI binary — it is a library used by `choreo-client-core` and `choreographr`.

**Identity keypair (X25519):**
The daemon's identity is an X25519 keypair stored as two files:
- `~/.config/choreographr/identity.pk` — raw 32-byte private key
- `~/.config/choreographr/public.pk` — raw 32-byte public key

The private key can be stored encrypted at rest:
- `~/.config/choreographr/identity.pk.enc` — Argon2 + AES-256-GCM encrypted private key

**Credential encryption pipeline (client-side):**
```
credential ──► postcard serialize ──► ECDH (ephemeral + recipient pubkey)
  ──► HKDF ──► AES-256-GCM encrypt ──► encrypted payload
```

Output format for each credential:
```
eph_public(32) || salt(32) || nonce(12) || ciphertext(rest)
```

Credentials are encrypted per-credential, using ECDH key agreement so only the
daemon (holder of the private key) can decrypt them. The encrypted blobs are stored
in the `redb` database alongside sessions.

**Modules:**

| Module | Purpose |
|---|---|
| `crypto.rs` | X25519 keypair generation, ECDH + HKDF + AES-256-GCM encrypt/decrypt, passphrase-based private key encryption; shared AES-256-GCM helpers |
| `paths.rs` | Resolves filesystem paths for identity key files |
| `error.rs` | `KeystoreError` enum |

**Credential types:** `ApiKey` (OpenAI), `X` (Twitter OAuth 1.0a credentials)
**Error type:** `KeystoreError` (thiserror enum) — `Io`, `TooShort`, `InvalidKeyLength`, `EncryptionFailed`, `DecryptionFailed`, `ConfigDirNotFound`


### `choreo-transport` — Noise IK encrypted transport

A small crate providing Noise IK handshake and encrypted message I/O over
TCP.  Used by both `choreo-client-core` (client side) and `choreographr` (server side).

| Module | Purpose |
|---|---|
| `noise.rs` | `NoiseStream` — wraps `TcpStream` + `snow::TransportState` with length-prefixed AES-256-GCM framing. `handshake_initiator()` (client) and `handshake_responder()` (server) implement the Noise IK handshake with X25519 key agreement. |
| `error.rs` | `TransportError` enum — `Io`, `Noise`, `Protocol`, `AuthFailed`, `ConnectionClosed`. |

The server-side TCP/Noise handler lives in `choreographr/src/server/connection.rs`
(`tcp_client_thread`), where the Noise IK handshake is performed and the
encrypted stream enters the same dispatch loop as Unix socket clients.


### `choreo-client-core` — Shared client logic

Used by `choreo-tui`, `choreo-gui`, and `choreo-im`.

| Module | Purpose |
|---|---|---|
| `shell.rs` | Parses terminal input into `ShellCommand`: `/ping`, `/models`, `/model` (alias), `/cancel`, `/unlock`, `/lock`, `/image`, `/add-key`, `/add-x`, `/remove-key`, or `RunInput(prompt)`. All commands use `/` prefix exclusively; `parse_command()` is the single dispatch point. |
| `credentials.rs` | Shared helpers: `resolve_private_key()` (read or decrypt the identity key), `build_add_credential_message()` (encrypt and package a credential for the daemon), `read_public_key_bytes()`. Eliminates duplicated logic across `choreo-tui`, `choreo-gui`, and `choreo-im`. |
| `image.rs` | `ImageAssembler` — kept for legacy `choreo-im` use. No longer used by TUI/Dioxus (images delivered mid-turn as `DisplayedImage` via `SessionMessageAppended`). |
| `history.rs` | `ClientHistory` ring buffer of `HistoryItem` entries (text, images, session messages, streaming text, structured diffs) |
| `diff.rs` | Types for structured unified diff representation (`DiffLineKind`, `DiffLine`, `DiffHunk`, `FileDiff`) |
| `dispatch.rs` | `DaemonMessageHandler` trait + `dispatch_daemon_message()` — categorizes incoming `DaemonMessage` variants into sub-dispatchers (`dispatch_session`, `dispatch_stream_lifecycle`, `dispatch_model`, `dispatch_keystore`, `dispatch_credential`, `dispatch_account`, `dispatch_reasoning`, `dispatch_misc`). Used by all UI clients to avoid duplicating the routing logic. Includes 50+ unit tests covering every message variant. |
| `connection.rs` | Daemon connection helpers: `run_daemon_connection()` (Unix socket), `run_daemon_tcp_connection()` (Noise IK), `run_daemon_connection_with_mode()` (dispatch), `run_daemon_reader()` (blocking reader). `ConnectionMode` enum (`UnixSocket` | `Tcp`) selects the transport. |

`DaemonMessageHandler` trait uses `ClientError` (thiserror enum) — `Proto`, `Io`, `Utf8`, `ImageTooLarge`, `ImageExceedsSize`, `DuplicateImage`, `UnknownImage`, `ImageSizeMismatch`, `PrivateKeyRead`, `PrivateKeyInvalid`, `PrivateKeyEncRead`, `PrivateKeyDecrypt`, `PublicKeyRead`, `PublicKeyInvalid`, `CredentialParse`, `Postcard`, `Encryption`.


### `choreo-mcp` — MCP client (Model Context Protocol)

Communicates with MCP server subprocesses over JSON-RPC 2.0 stdio transport.
Used by `choreographr` to spawn external MCP servers and register their tools.

| Module | Purpose |
|---|---|
| `client.rs` | `McpClient` — spawns a subprocess, performs MCP initialize handshake, discovers tools (`list_tools`), and dispatches tool calls (`call_tool`) |
| `protocol.rs` | JSON-RPC 2.0 wire types (`JsonRpcRequest`, `JsonRpcResponse`) and MCP protocol types (`McpTool`, `CallToolResult`, `McpContent`) |
| `transport.rs` | `StdioTransport` — manages a subprocess stdin/stdout, routes incoming JSON-RPC lines to response/notification channels |
| `error.rs` | `McpError` enum — `SpawnFailed`, `InitializeFailed`, `JsonRpcError`, `ProtocolError`, `Timeout`, `Io`, `ServerShutdown`, `ToolNotFound`, `InvalidParams` |


### `choreo-acp` — ACP bridge (Agent Communication Protocol)

Entry point: `src/main.rs` → initializes logging, connects to the daemon's Unix socket,
spawns I/O threads, runs the main event loop.

The ACP bridge translates the **Agent Communication Protocol** (JSON-RPC 2.0 over
stdin/stdout) into `choreo-proto` messages sent to the daemon over its Unix socket.
This allows ACP-compatible editors (Claude Code, Cline, etc.) to manage Choreographr sessions,
send prompts, and receive streaming responses as if they were native Choreographr clients.

**Thread topology:**

```
main()
├── acp-reader thread: reads newline-delimited JSON-RPC lines from stdin,
│   parses them, sends parsed RpcMessage into the shared event channel
├── daemon-reader thread: reads DaemonMessages from the daemon socket,
│   forwards them into the shared event channel
├── daemon-writer thread: receives ClientMessages via mpsc and writes
│   length-prefixed postcard frames to the daemon socket
└── main thread: event loop — receives from the shared event channel
    and dispatches to the appropriate handler
```

**Modules:**

| Module | Purpose |
|---|---|
| `main.rs` | CLI arg parsing (socket path, log file), thread spawning, logging setup |
| `acp_jsonrpc.rs` | JSON-RPC 2.0 wire types (`JsonRpcRequest`, `JsonRpcResponse`, `JsonRpcNotification`, `RpcMessage`) and ACP protocol payload types (`InitializeResult`, `ConfigOption`, `ContentBlock`, `SessionUpdateParams`, etc.) |
| `acp_reader.rs` | Blocking stdin reader thread with 1 MiB line-length limit |
| `acp_handler.rs` | Event loop dispatch — routes `RpcMessage` to ACP method handlers (`session/new`, `session/prompt`, `session/delete`, etc.), routes `DaemonMessage` to streaming or sync response handlers |
| `daemon_client.rs` | Unix socket connection, daemon reader/writer thread spawning, shared `Event` enum |
| `sessions.rs` | `SessionManager` — bidirectional map (ACP session ID ↔ daemon session ID), active-prompt guard, request ID counter |
| `pending.rs` | `PendingRequests` — tracks in-flight sync requests and streaming prompts |
| `config.rs` | Builds ACP `ConfigOption` objects from daemon state (model list, reasoning effort, tool groups) |
| `streaming.rs` | Translates daemon `OutputChunk`/`ToolCallStarted`/etc. into ACP `session/update` notifications |
| `client_capabilities.rs` | Stores editor-declared capabilities from the `initialize` handshake |
| `error.rs` | `AcpError` enum — JSON-RPC errors, daemon connection, session errors, I/O, proto, serde |

**Key behaviors:**

- **Concurrency:** Pure OS threads with `mpsc` message passing. No `Arc<Mutex>` shared state.
  Threads communicate exclusively through a single shared event channel (`mpsc::Receiver<Event>`).
- **Session lifecycle:** Sessions are created on the daemon via `CreateSession` and tracked locally
  in `SessionManager`. `session/close` cleans up local state only (the daemon keeps sessions alive
  until explicitly deleted). `session/delete` sends `DeleteSession` to the daemon and waits for
  confirmation before removing local state.
- **Streaming:** Prompt responses are streamed from the daemon as `OutputChunk` events, translated
  to ACP `session/update` notifications, and finalized with a JSON-RPC response on `Done`/`Failed`/`Cancelled`.


### `choreographr` — Core server

Entry point: `src/main.rs` → initializes tracing, creates `DaemonState`, runs socket server.

**Concurrency model:** Pure OS threads with message passing (actor model). No async code
in the daemon's own logic. All I/O uses blocking `std` APIs on dedicated threads.

**Module breakdown:**

| Module | Purpose |
|---|---|---|
| `server/lifecycle.rs` | Accept loop (non-blocking `UnixListener` + 50ms poll), signal handling (`signal_hook::flag`), shutdown orchestration. |
| `server/connection.rs` | Per-client `client_thread` — reads `ClientMessages` from socket, dispatches via `daemon_tx` mpsc channel. |
| `daemon.rs` | `DaemonCommand` handler loop on a dedicated thread — session CRUD, attach/detach, listing, locking, account management. `DaemonState` is owned by this thread only (no shared state). |
| `accounts/` | `AccountManager` — loads/saves `accounts.toml`, manages named inference accounts with per-account config overrides. |
| `providers/` | `ProviderClient` trait + `ProviderCatalog` system. `InferenceProvider` struct wraps `Arc<dyn ProviderClient>`. Static `PROVIDER_CATALOG` maps ~30 slugs to protocol type, default base URL, and default model. Dispatches to the correct client based on protocol. |
| `providers/shared.rs` | Shared provider infrastructure: `ProviderError` (unified error type used by all providers), `build_agent()` (ureq Agent construction with timeouts), `error_type_label()`, `provider_error_to_inference()`, `timed_result()` (metrics instrumentation wrapper), `emit_non_streaming_events()` (converts a non-streaming `ChatTurnResult` into `StreamEvent` callbacks so non-streaming configurations reuse the same event-driven path). Eliminates duplicated error types, `From<ProviderHttpError>` impls, and error conversion functions across provider implementations. |
| `anthropic/` | Anthropic Messages API client (`AnthropicClient`). Implements `ProviderClient`. |
| `google/` | Google Gemini API client (`GoogleClient`). Implements `ProviderClient`. Uses its own SSE reader for streaming. |

| `retry/` | Shared HTTP retry logic extracted from the OpenAI module. `ProviderHttpError` enum captures HTTP error codes generically; `retry_loop()` provides exponential backoff with jitter, retryable status detection, and cancellation support. All provider modules use this via the shared `ProviderError` type conversion. |
| `sessions.rs` | `SessionState` (split into `SessionConfig` for persisted fields + runtime state), `RequestContext` dependency bundle, `SessionCommand` enum and its handler functions. Each session has a control thread running `session_main()`; request work runs on separate worker threads via `run_request_worker()`. Sessions form a tree (parent → child sub-sessions), each with an optional working directory. |
| `chat_completions.rs` / `responses.rs` | Chat Completions and Responses API wire types, request functions, SSE streaming, tool call accumulation. |
| `context.rs` | Context file discovery, skills, fingerprint-based refresh. |
| `metrics.rs` | Prometheus/OpenMetrics gauges, counters, histograms; HTTP server for `/metrics` endpoint. |
| `openai/` | HTTP integration with OpenAI-compatible APIs, SSE streaming, service config loading, programmatic tool calling (Responses API). |
| `tools/` | `Tool` trait (with `output_schema` for programmatic tool calling, `allowed_callers` for caller-level gating), `ToolRegistry` (with injectable `FffStateCache` replacing a global `OnceLock`), and 30+ registered tools (including `list_sessions`, `get_session`, `load_skill` via `admin/`). |
| `tools/context.rs` | `ToolContext` — session-scoped context (session ID, `Arc<Database>`, `mpsc::Sender<DaemonCommand>`, active tool groups, reasoning effort, selected model, working directory) passed to tools that need DB or daemon access or parent config for sub-sessions. |
| `tools/db/` | Session-scoped KV database tools (`db_set`, `db_get`, `db_delete`, `db_delete_range`, `db_get_range`, `db_list`, `db_count`), one file per tool (`set.rs`, `get.rs`, `delete.rs`, `delete_range.rs`, `get_range.rs`, `list.rs`, `count.rs`) with shared `DbError`/`DbValue` in `db/mod.rs`. |
| `tools/fs/` | Core filesystem tools (`list_files`, `line_count`, `write_file`, `edit_file`, `delete_files`), one file per tool with shared write helpers in `fs/mod.rs`. |
| `tools/x/` | X/Twitter API tools (`x_post`, `x_search_recent`, `x_user_lookup`), one file per tool with shared OAuth1/HTTP plumbing in `x/mod.rs`. |
| `tools/admin/` | Session-admin tools (`list_sessions`, `get_session`, `load_skill`), one file per tool. |
| `tools/glob_util.rs` | `GlobFilter` — shared glob-matching utility used by `delete_files` and `grep` that follows gitignore conventions (patterns without `/` match basename, patterns with `/` match full path). |
| `tools/vm.rs` | RISC-V sandbox: compiles Rust → ELF via rustc, executes in `ckb-vm` with custom syscall handler (`ChoreographrSyscall`) for tool dispatch. |
| `mcp/` | `McpManager` — loads MCP server config from `mcp_servers.json`, spawns subprocesses via `McpClient`, wraps discovered tools as `McpToolWrapper` (implements `ToolDyn`) and registers them in the `ToolRegistry` under a `mcp/<slug>` group. |

### Provider Architecture

The provider system has three layers:

**1. `ProviderClient` trait (`providers/traits.rs`):**
```rust
/// Holds the common parameters for a chat completion turn.
pub struct ChatTurnRequest<'a> {
    pub model: &'a str,
    pub messages: &'a [ChatRequestMessage],
    pub tools: &'a [ChatToolDefinition],
    pub thinking_effort: String,
    pub on_retry: &'a mut Option<RetryCallback>,
    pub cancel_rx: Option<&'a mpsc::Receiver<()>>,
    pub previous_response_id: Option<&'a str>,
    pub tool_results: &'a [ToolResultItem],
    pub programmatic_tool_calling: bool,
}

pub trait ProviderClient: Debug + Send + Sync {
    fn provider_slug(&self) -> &'static str;
    fn chat_completion_turn(&self, params: ChatTurnRequest<'_>) -> Result<ChatTurnResult, InferenceError>;
    fn chat_completion_turn_streaming(&self, params: ChatTurnRequest<'_>, on_event: &mut dyn FnMut(StreamEvent) -> io::Result<()>) -> Result<ChatTurnResult, InferenceError>;
    fn list_models(&self) -> Result<Vec<String>, InferenceError>;
    fn supports_programmatic_tool_calling(&self, model: &str) -> bool;
    fn context_window_for_model(&self, model: &str) -> Option<u32>;
}
```

`ChatTurnRequest` consolidates the per-turn parameters into a single struct
to eliminate repetitive argument passing across all provider implementations.
Uses `&mut dyn FnMut` for the streaming callback to keep the trait object-safe.
`context_window_for_model()` returns the model's context window size, using a
resolution chain: per-model config → global fallback → static catalog.
Each client implementation maps the `&str` effort slug to its wire format:
- **OpenAI**: `reasoning_effort` field (`None` for `"off"`, `"low"`/`"medium"`/`"high"` slug → API string)
- **Anthropic**: `thinking` block with `budget_tokens` (slug ≠ `"off"` enables thinking, clamping to `max_tokens - 1024`)
- **Google**: `thinkingConfig` with `includeThoughts: true` (slug ≠ `"off"` enables thinking)
- **Mistral**: `reasoning_effort` field (`"off"` omits the field, otherwise slug → `"low"`/`"medium"`/`"high"`)

**2. `InferenceProvider` struct (`providers/mod.rs`):**
```rust
pub struct InferenceProvider {
    client: Arc<dyn ProviderClient>,
}
```
Created via `from_account_config()` which looks up the provider slug in the catalog and dispatches to the appropriate client constructor by protocol type.

**3. Provider Catalog (`providers/catalog.rs`):**
```rust
pub enum ProviderProtocol {
    OpenAiCompatible,
    AnthropicMessages,
    GoogleGenerativeAi,
    Mistral,
}
```

**`StreamEvent`** (`providers/mod.rs`) replaces the old `(CompletionChunkKind, String)` callback tuple:
```rust
pub enum StreamEvent {
    Answer(String),
    Reasoning(String),
}
```
Each variant carries its data inline so the streaming callback is self-describing and extensible.
`emit_non_streaming_events()` in `providers/shared.rs` converts a `ChatTurnResult` into the
equivalent sequence of `StreamEvent`s, allowing non-streaming configurations to reuse the
same event-driven path as streaming ones without duplication across providers.

**3. Provider Catalog (`providers/catalog.rs`):**
```rust
pub enum ProviderProtocol {
    OpenAiCompatible,
    AnthropicMessages,
    GoogleGenerativeAi,
    Mistral,
}
```

A static `PROVIDER_CATALOG: &[ProviderEntry]` maps each provider slug to:
- `display_name` — human-readable name for UIs
- `protocol` — which wire protocol to use
- `default_base_url` — well-known API endpoint
- `default_model` — sensible default model name
- `reasoning` — `ReasoningSupport` variant declaring which reasoning parameter protocol the provider speaks
- `model_context_windows` — static list of `(model_slug, window)` pairs for known models

`ReasoningSupport` enum: `None`, `ReasoningEffort` (OpenAI-style), `AnthropicThinking` (thinking budget block), `GoogleThinkingConfig` (thinkingConfig field). Model-level reasoning is declared via a `ModelReasoningEntry { model, levels }` list on each `ProviderEntry.model_reasoning_levels` field, and resolved at runtime by `model_reasoning_capability()` which returns a `ReasoningCapability` with the model's available effort slugs. Providers without explicit entries fall back to heuristics inside `resolve_reasoning_effort()`.

Currently supports ~30 providers. Adding a new OpenAI-compatible provider requires only a catalog entry — zero client code.

**Supported providers by protocol:**

| Protocol | Providers |
|---|---|
| OpenAI-compatible | OpenAI, DeepSeek, xAI/Grok, Groq, Together AI, Ollama (local/cloud), OpenRouter, HuggingFace, GitHub Models, NVIDIA NIM, Cerebras, Fireworks AI, Xiaomi MiMo, DashScope, Moonshot AI, Perplexity, Z.ai, Venice AI, Novita AI, LM Studio, OpenCode Zen, OpenCode Go, Atomic Chat, and custom OpenAI-compatible |
| Anthropic Messages | Anthropic Claude, MiniMax, custom Anthropic-compatible |
| Google Generative AI | Google Gemini |
| Mistral | Mistral |

> **Note:** Amazon Bedrock support is deferred pending multi-field credential support (needs AWS access key + secret key + region).

**Per-client architecture (OS threads):**

```
client_thread(socket)
├── reads ClientMessages from socket via choreo-proto read_message_sync
├── sends DaemonCommands via daemon_tx mpsc channel
└── receives DaemonMessages via per-client mpsc receiver → writes to socket
```

**Thread topology:**

```
main()
├── listener thread — UnixListener accept loop (non-blocking poll)
│   └── per client: spawns client_thread (std::thread::spawn)
├── metrics HTTP thread — (optional) serves /metrics at `--metrics-addr`
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
        └► tool-call loop (max configurable iterations, default 25; 0 = unlimited):
           0.5. build system content (skills + context with fingerprint cache + loaded skills + subdirectory hints)
           1. send messages + tools → model
           2. receive response
           3. if tool_call → execute Tool → persist loaded skill bodies → collect subdirectory hints → goto 0.5
           4. else → emit final text, Done
     └► if responses or chat_completions (no tools):
        └► stream chunks via SSE → emit OutputChunk per token → Done
```

### Token Usage & Context Window Tracking

Token usage flows from providers through the daemon to all clients.
Each session also tracks the model's **context window size**, resolved when the model
is selected. The TUI displays context usage as a fraction
(`last_prompt_tokens / context_window`), showing the actual context size sent in the
most recent request rather than accumulated totals.

Additionally, each session tracks `last_prompt_tokens: Option<u32>` — the `input_tokens`
from the most recent API response. This is stored separately from `accumulated_usage`
(the billing counter) and reflects the actual context payload the model sees on the
latest turn. When an existing session is loaded from the database but has no stored
`context_window`, the daemon re-resolves it from the provider catalog.

```
LLM provider (API response)
  └► usage extracted per-turn in provider client
     ├─ OpenAI non-streaming:    ChatCompletionsResponse.usage
     ├─ OpenAI streaming:        final SSE chunk with usage (stream_options.include_usage=true)
     ├─ Anthropic non-streaming: MessagesResponse.usage (input + output tokens)
     ├─ Anthropic streaming:     message_start (input) + message_delta (output)
     ├─ Mistral:                 ChatCompletionResponse.usage
     └─ Google:                 Not yet supported (usage = None)
       │
       ▼
     ChatTurnResult (FinalText | ToolUse).usage: Option<TokenUsage>
       │
       ▼
      run_agent_loop (choreographr/src/requests.rs)
        ├─ embeds per-turn TokenUsage into SessionMessageKind::AssistantText / SessionMessageKind::AssistantToolUse
        ├─ tracks last_prompt_tokens = Some(usage.input_tokens) for context-window display
        └─ accumulates into SessionState.config.accumulated_usage (TokenUsage)
        │
        ▼
      SessionState (choreographr/src/sessions.rs)
        ├─ persisted via SessionRecord.accumulated_usage (through SessionConfig)
        ├─ sent to subscribers via DaemonMessage::SessionState.token_usage
        ├─ sent to clients via DaemonMessage::Done.token_usage
        ├─ included in SessionSummary.token_usage (listing / get-session)
        ├─ last_prompt_tokens flows through the same channels (SessionRecord,
        │  SessionState, DaemonMessage::SessionState, DaemonMessage::Done,
        │  SessionSummary)
        └─ status flows through DaemonMessage::SessionState.status and
           DaemonMessage::SessionStatusChanged for live toolbar display
        │
        ▼
     Clients (choreo-tui, choreo-gui, choreo-im)
       ├─ choreo-tui: displays in session detail view (render.rs:render_session_detail_view)
       │  as "Context:  current / limit (pct%)"
       └─ choreo-tui: terminal progress bar uses last_prompt_tokens vs context_window
          for the OSC 9;4 percentage sequence
```

**Key type** — `TokenUsage` (choreo-proto/src/types.rs):
```rust
pub struct TokenUsage {
    pub input_tokens: u32,
    pub output_tokens: u32,
    pub total_tokens: u32,
}
```

**Context window resolution chain (per session):**

```
handle_set_model / handle_set_account
  └► InferenceProvider::resolve_context_window(model)
       ├─ ProviderClient::context_window_for_model(model)
       │    ├─ model_context_windows (exact model name match)
       │    └─ context_window (global fallback)
       └─ catalog::lookup_context_window(provider_slug, model)
             └─ model_context_windows (exact model slug match)
       │
       ▼
     SessionConfig.context_window: Option<u32>
       │
       ▼
     Re-resolved on session startup if None
       (session_main + handle_run_input both call
        SessionState::resolve_context_window_if_missing(),
        handling sessions created before the model was
        in the catalog or providers resolved after unlock)
       │
       ▼
     Client display (e.g. "Context: 45,000 / 128,000 (35%)")
```

The `ContextWindowConfig` struct (shared across all provider configs) holds the
per-model map and global fallback. Provider configs embed this struct; `AccountConfig`
applies its overrides through the shared `apply_overrides()` method.

All new fields use `#[serde(default)]` so old persisted sessions remain compatible (deserialize to zero usage).

### `choreo-tui` — Terminal client

Entry point: `src/main.rs`

**Thread topology:**

```
main()
├── reader task: read DaemonMessages from socket → push to UI event channel
├── writer task: receive ClientMessages from mpsc → write to socket
├── terminal-event thread: mio::Poll on three sources —
│   ├── stdin (fd 0) → crossterm events (keyboard, mouse, resize)
│   ├── notification pipe → clean shutdown signal
│   └── signal pipe (self-pipe trick) → SIGCONT/SIGTSTP (suspend/resume)
│       └── forwards signals as ResumeCommand via crossbeam channel
└── UI loop: crossbeam select! on five event sources + ratatui rendering
```

**Signal handling (suspend/resume):**

`SIGCONT` and `SIGTSTP` are caught using the self-pipe trick for POSIX
portability (Linux and macOS). A pair of pipe fds (FD_CLOEXEC) is created;
the read end is registered with `mio::Poll` in the terminal-event thread,
and `signal_hook::low_level::pipe` installs signal handlers that atomically
write the signal number to the write end. The terminal-event thread reads
from the pipe and forwards `ResumeCommand` messages through a crossbeam
channel to the UI loop. The UI loop handles `PrepareForSuspend`
(disable raw mode, leave alternate screen, `raise(SIGSTOP)`) and
`ReinitTerminal` (re-enable raw mode, re-enter alternate screen, clear).

> **Note:** In raw mode, `termios` `ISIG` is disabled, so pressing Ctrl+Z in the
> terminal sends byte `0x1A` to stdin as a regular character — it does **not**
> generate a `SIGTSTP` signal. The self-pipe suspend only catches external
> `SIGTSTP` (e.g. from `kill`, shell job control, or another terminal).
> To support Ctrl+Z keyboard suspend from within the TUI, the page event
> handlers (`handle_chat_event`, etc.) would need an explicit
> `KeyCode::Char('z')` + `KeyModifiers::CONTROL` match that calls
> `handle_resume_command(PrepareForSuspend, …)`.

**Per-frame sequence (UI loop):**

```
while !app.should_quit:
  1. Block in crossbeam select! until any event arrives:
     - terminal events (keyboard, mouse, resize)
     - daemon messages from the reader task
     - image encoding results from the worker thread
     - resume commands from the terminal-event thread
  2. Drain all five event sources (non-blocking try_recv):
     - crossterm events
     - UI event channel (daemon messages)
     - image result channel
     - resume commands
  3. If nothing was dirty, skip render (continue)
  4. Consume scroll accumulator → apply batched delta
  5. Update history viewport dimensions from terminal size
  6. Clamp scroll state to valid range
  7. Render via ratatui terminal.draw()
  8. If `progress_dirty`, emit OSC 9;4 terminal progress bar
     (percentage of `last_prompt_tokens / context_window` for the
     attached session, or clear/indeterminate if no data)
```

**Module breakdown:**

| Module | Purpose |
|---|---|---|
| `connection.rs` | Socket setup, event loop, shutdown signal handling, input/keyboard/mouse dispatch, daemon message routing, terminal suspend/resume signal handling. Mouse scroll events are accumulated per-frame rather than applied immediately — the delta is consumed in batch before each render (see `apply_scroll_delta`). Left-clicking a turn's reasoning header toggles that turn's collapsible reasoning section. |
| `state.rs` | `App` struct: input buffer, request tracking, `HashMap<u64, SessionDisplayState>` for per-session display state (session view, scroll state, height prefix-sum array, render cache, markers, streaming state, active requests, live token estimates, per-turn reasoning-collapse overrides, etc.), `active_session_id` for the currently active session, and the per-frame scroll accumulator (`scroll_accumulator`) consumed by `apply_scroll_delta()`. |
| `render.rs` | Ratatui rendering: history pane (top) + command input + status bar (bottom), word wrap, Unicode width. Does **not** mutate scroll state or viewport dimensions — those are updated in the event loop before `terminal.draw()`. |
| `syntax.rs` | Shared syntect helpers (`syntax_set`, `highlight_theme`, `to_ratatui_color`). Used by `markdown_render.rs` for code-block syntax highlighting. |
| `markdown_render.rs` | Terminal markdown renderer. Parses markdown (via `choreo-client-core`'s `pulldown-cmark` wrapper), renders blocks (paragraphs, headings, code, lists, tables, block quotes) into styled `ratatui::text::Line` vectors. Code blocks are syntax-highlighted via `syntect` (shared setup from `syntax.rs`). Tool call labels (`tool: name(args)`) have been removed from the assistant block — tool invocations are now visible through the `invocation_description` rendered as markdown before each tool result label, and through streaming output. The assistant block renders the response first, followed by a collapsible reasoning section: a dimmed header line (arrow glyph + "Reasoning") is always shown when reasoning content exists, with the reasoning body below it only when expanded (▼), or hidden behind a right-pointing arrow (▶) once the response arrives. |
| `lib.rs` | `RenderedImage` struct, `build_picker()` helper, public re-exports |
| `image_worker.rs` | Background worker thread for SVG rasterization and terminal protocol encoding. Communicates with the UI thread via `mpsc` channels; raw image data shared through `Arc<Vec<u8>>` to avoid copies. |
| `terminal_progress.rs` | Terminal-native progress bar via OSC 9;4 escape sequences. Cached capability detection, percentage/indeterminate/remove modes based on `last_prompt_tokens` vs `context_window`. |


### `choreo-gui` — Desktop client

Entry point: `src/main.rs`

Unix socket or Noise IK encrypted TCP transport (selected via `--tcp-addr` / `--server-pk` CLI flags),
rendered via Dioxus components. Uses hooks to spawn async reader/writer tasks inside
the Dioxus runtime.

**Module breakdown:**

| Module | Purpose |
|---|---|
| `client.rs` | `run_client()` — socket split, reader/writer, daemon message dispatch |
| `state.rs` | `AppState` with input, request tracking, `ClientHistory` |
| `render.rs` | RSX rendering of history items: markdown → sanitized HTML, images via `data:` URLs, structured diffs via `format_diff_file` |
| `main.rs` | Dioxus `App` component, toolbar, history pane, textarea composer, CSS |


### `choreo-im` — IM platform bridge

Entry point: `src/main.rs`

Single binary (`choreo-im`) that bridges IM platforms to the daemon.
The binary accepts a platform subcommand: `choreo-im telegram`.

**Credentials:** The daemon serves platform credentials via the `GetCredential` wire
message. The admin stores credentials via `/add-key` or `/add-x` at runtime, which
encrypts them with the daemon's public key. On unlock (`/unlock`) the daemon decrypts
all stored credentials into memory using its private key.

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

The daemon starts in a **locked** state. The client resolves the private key (reading
`identity.pk` directly or decrypting `identity.pk.enc` with a passphrase) and sends
it to the daemon via `ClientMessage::Unlock { private_key }`.

```
startup                    /unlock [passphrase]
   │                              │
   │  locked                      │  read identity.pk (or decrypt identity.pk.enc)
   │  (no credentials)            │  decrypt all credential blobs from database
   │                              │  load accounts from accounts.toml
   │                              │  resolve InferenceProvider per account
   │                              │  → Unlocked (ready)
   ▼                              ▼
```

- Credentials are encrypted per-credential with ECDH (X25519) + HKDF + AES-256-GCM
- The private key is sent over the Unix socket; zeroized after use by the daemon
- `/lock` destroys all in-memory credentials, returning to locked state
- `LockedError` is sent if any client attempts a request that requires credentials while locked
- Session lifecycle operations (CreateSession, AttachSession, ListSessions, etc.) succeed even
  when locked — credentials are only needed at RunInput time. Provider resolution is lazy:
  when RunInput is called, the session thread resolves the InferenceProvider from the daemon's
  provider registry. If no credential is available for the session's account, a clear error is
  returned telling the user to add a key.
- There is no global "default account". Each session carries its own `account_name: Option<String>`
  field. When RunInput is issued, the daemon resolves the provider from the session's own account name.
  This avoids a global mutable fallback and lets different sessions use different accounts.


---

## Tool system

### Generic `Tool` trait

The tool trait is generic over argument and return types. Each tool declares its own
`type Args` (must implement `DeserializeOwned + JsonSchema`) and `type Return`
(must implement `Serialize + JsonSchema`). Both `schema()` and `output_schema()` are
auto-derived via `schemars` by default, eliminating the need for hand-written JSON schemas.
The generated schemas are then sanitized — `$schema`, `title`, and `$defs`/`$ref` are
stripped/resolved for compatibility with providers that do not support Draft 2020-12
meta-schema features, and `additionalProperties: false` is injected for parameter schemas.

```rust
pub trait Tool: Send + Sync {
    type Args: DeserializeOwned + JsonSchema + 'static;
    type Return: Serialize + JsonSchema + 'static;
    /// Error type — each tool defines its own. Simple tools use `ToolExecError`
    /// (a string-wrapper). Tools whose errors are consumed by VM guests (e.g.
    /// `DbError`, `HttpError`) define a `thiserror` enum that is serde-serializable,
    /// enabling the guest to pattern-match on specific variants.
    type Error: std::error::Error + Send + Sync + Serialize + DeserializeOwned + 'static;

    fn name(&self) -> &'static str;
    fn group(&self) -> &'static str { "core" }
    fn description(&self) -> &'static str;

    /// Auto-derived JSON Schema for the tool's input arguments.
    /// Sanitized via `sanitize_params_schema` (strips `$schema`/`title`/`$defs`,
    /// resolves `$ref`s inline, injects `additionalProperties: false`, converts
    /// unit-arg `{"type":"null"}` to empty object).
    fn schema(&self) -> serde_json::Value {
        sanitize_params_schema(
            serde_json::to_value(schemars::schema_for!(Self::Args)).unwrap_or_default(),
        )
    }

    /// JSON Schema for the tool's return value (for Programmatic Tool Calling).
    /// Auto-derived from the return type. Override for types schemars cannot represent.
    /// Sanitized via `sanitize_output_schema` (same as above but without
    /// `additionalProperties`).
    fn output_schema(&self) -> Option<serde_json::Value> {
        Some(sanitize_output_schema(
            serde_json::to_value(schemars::schema_for!(Self::Return)).unwrap_or_default(),
        ))
    }

    /// Controls which callers can invoke this tool
    /// (`Direct`, `Programmatic`, or both).
    fn allowed_callers(&self) -> Vec<AllowedCaller> {
        vec![AllowedCaller::Direct, AllowedCaller::Programmatic]
    }

    fn execute(
        &self,
        args: Self::Args,
        x_credentials: Option<&ServiceCredential>,
        working_dir: Option<&Path>,
        ctx: Option<&ToolContext>,
    ) -> Result<Self::Return, Self::Error>;

    fn execute_streaming(
        &self,
        args: Self::Args,
        x_credentials: Option<&ServiceCredential>,
        working_dir: Option<&Path>,
        _output_tx: mpsc::Sender<Vec<u8>>,
        ctx: Option<&ToolContext>,
    ) -> Result<Self::Return, Self::Error> {
        // Non-streaming tools deliver their result via TurnAppended —
        // no ToolResultChunk traffic needed.
        self.execute(args, x_credentials, working_dir, ctx)
    }

    fn extract_image(&self, _ret: &Self::Return) -> Option<PreparedImage> { None }

    /// Produce a human-readable description of what the tool is about to do,
    /// using every supplied argument for detail (e.g. "Reading file `main.rs`.",
    /// "Making POST HTTP request to `https://api.example.com/data`.").
    /// Returns a natural English sentence. There is no default — every tool
    /// must provide one. The value is stored in `ToolOutput.invocation_description`
    /// and flowes through to `ToolResultRecord.invocation_description` for the
    /// TUI to render as the first line of the tool result block.
    fn describe_invocation(&self, args: &Self::Args) -> String;

    /// Produce a human-readable string from the return value.
    /// The default implementation JSON-encodes the value.
    /// Tools whose `Return` is `String` override this to return
    /// the raw string directly (e.g. shell tools, macro-defined tools).
    fn return_string(ret: &Self::Return) -> String {
        serde_json::to_string(ret).unwrap_or_default()
    }
}
```

`ToolOutput` replaces the old `ToolExecutionOutput` + `ToolResult` pair:

```rust
pub enum ToolOutputFormat { Text, Json }
pub struct ToolOutput {
    pub content: String,
    pub is_error: bool,
    /// Human-readable sentence describing what the tool is about to do,
    /// produced by `Tool::describe_invocation()` before execution.
    /// Empty string when the description is unavailable (e.g. spawned
    /// thread error paths before the description could be generated).
    pub invocation_description: String,
    /// The tool's structured return value (`serde_json::to_value(ret)`),
    /// populated by the blanket `ToolDyn` impl after a successful execution.
    /// `None` for error/timeout outputs.  The request worker reads this to
    /// mirror session-config mutations (e.g. `set_working_dir`'s canonical
    /// path) onto its config copy without re-executing the tool.
    pub result_json: Option<serde_json::Value>,
}
```

`Text` format is used for LLM-facing tool results (human-readable, uses `return_string`).
`Json` format is used for Programmatic Tool Calling (PTC) — JSON-encodes the return via `serde_json::to_string`.
`invocation_description` is stored in `ToolResultRecord` and streamed as the first chunk in
`execute_streaming_json` for real-time TUI display. It is explicitly excluded from LLM message
construction — the model never sees it.

Tools that need session context (`ToolContext` — used by `list_sessions`, `get_session`)
receive it in the `ctx` parameter. Tools that return structured data override
`output_schema()` to describe their return JSON shape, enabling the model to call
them programmatically (see [Programmatic Tool Calling](#114-programmatic-tool-calling-responses-api-gpt-56)).
Tools can restrict callers via `allowed_callers()`, gating whether the model calls
them directly, from generated JavaScript, or both.

Tools that produce images (e.g. `display_image`) override `extract_image()` to return a
`PreparedImage` from the typed return value. The conversion layer (see `ToolDyn` below)
sends the image through an out-of-band `image_tx: Option<mpsc::Sender<PreparedImage>>`
channel rather than embedding it in the response struct. The agent loop drains this
channel after execution to persist and broadcast the image.

### `ToolDyn` — type-erased dispatch trait

The `ToolDyn` trait erases the generic parameters so tools can be stored in a `HashMap`:

```rust
pub trait ToolDyn: Send + Sync {
    fn name(&self) -> &str;
    fn group(&self) -> &str;
    fn description(&self) -> &str;
    fn schema(&self) -> serde_json::Value;
    fn output_schema(&self) -> Option<serde_json::Value>;
    fn allowed_callers(&self) -> Vec<AllowedCaller>;

    /// Human-readable invocation description from JSON args.
    /// Delegates to `Tool::describe_invocation` via the blanket impl.
    /// Returns the static `description()` fallback when args fail to parse.
    fn describe_invocation_json(&self, args_json: &str) -> String;

    /// JSON path — takes JSON args, returns Result so callers can distinguish
    /// infrastructure errors (deserialisation failures) from tool errors.
    fn execute_json(&self, args_json: &str, format: ToolOutputFormat, ...) -> Result<ToolOutput, ToolError>;
    /// Streaming JSON path.
    fn execute_streaming_json(&self, args_json: &str, format: ToolOutputFormat, ...) -> Result<ToolOutput, ToolError>;
    /// Postcard binary path (VM ecall). Returns bytes encoding
    /// `Result<Result<T::Return, T::Error>, ToolError>` — all outcomes
    /// (infra error, tool error, tool success) are contained in the buffer.
    fn execute_postcard(&self, args_bytes: &[u8], ...) -> Vec<u8>;
}
```

A blanket impl `impl<T: Tool> ToolDyn for T` provides `describe_invocation_json` (deserializes
args and delegates to `Tool::describe_invocation`, falling back to `description()` on parse
failure) and all three dispatch paths:

| Path | Input | Output | Used by |
|---|---|---|---|---|
| `execute_json` | `&str` (JSON) + `ToolOutputFormat` | `Result<ToolOutput, ToolError>` | LLM tool calls (OpenAI/Anthropic etc.) |
| `execute_streaming_json` | `&str` (JSON) + `ToolOutputFormat` | `Result<ToolOutput, ToolError>` | Streaming shell/VM tools via LLM |
| `execute_postcard` | `&[u8]` (postcard) | `Vec<u8>` (postcard of `Result<Result<R, E>, ToolError>`) | RISC-V VM tool calls |

The JSON path deserializes arguments with `serde_json`, calls `Tool::execute()`, then
returns a `ToolOutput`. Both `execute_json` and `execute_streaming_json` first call
`T::describe_invocation(self, &args)` to produce the invocation description, then store
it on the returned `ToolOutput`. In the streaming path, the description is also sent as
the first chunk via `output_tx` so the TUI can display it in real time before the tool
produces any output. When `format` is `Text`, the content is produced via
`T::return_string()` (human-readable). When `format` is `Json`, the return value
is JSON-encoded via `serde_json::to_string()` (for PTC responses).
The binary path uses `postcard` for both deserialization and serialization,
enabling compact cross-VM communication.

### `define_tool!` macro

The `define_tool!` macro reduces boilerplate for the common tool case
(`Return = String`, no credentials needed). It lives in `choreographr/src/tools/mod.rs`.
The JSON schema is auto-derived from the args type via `schemars`, so no manual
schema parameter is needed. The macro now takes 7 arguments — the 7th is a
`fn(&Args) -> String` path that provides the invocation description:

```rust
define_tool!(MyTool, "my_tool", "Description...", MyToolArgs,
    execute_my_tool, "core", describe_my_tool_invocation);
```

The describe function is also used by the blanket `ToolDyn::describe_invocation_json`
implementation and by `ToolRegistry::describe_invocation`.

Tools that need custom `output_schema()`, `allowed_callers()`, non-`String` return types,
session context (`ToolContext`), or credentials (`ServiceCredential`) are written as
manual `impl Tool` blocks instead. Examples:
`DbGet`/`DbGetRange`/`DbList`/`DbCount` (custom `output_schema`),
`GetCurrentTime` (`Return = u64`), `DisplayImage` (overrides `extract_image`),
`ListSessions`/`GetSession` (need `ToolContext`).

### Registry

Tools are registered in a `ToolRegistry` stored as `Box<dyn ToolDyn>`. The registry is
owned by `DaemonStateInner`, constructed once at daemon startup. The agent loop extracts
an `Arc<ToolRegistry>` from the daemon state to list available tool definitions and
dispatch tool execution.

The registry provides `describe_invocation()`, `describe_invocation_for()`,
`execute_json()`, `execute_streaming_json()`, and `execute_postcard()` for dispatch:

```rust
pub fn describe_invocation(&self, tool_call: &ChatToolCall) -> String;
pub fn describe_invocation_for(&self, name: &str, args_json: &str) -> Option<String>;
pub fn execute_json(&self, tool_call: &ChatToolCall, format: ToolOutputFormat, ...) -> ToolOutput;
pub fn execute_streaming_json(&self, tool_call: &ChatToolCall, format: ToolOutputFormat, ...) -> ToolOutput;
pub fn execute_postcard(&self, name: &str, args_bytes: &[u8], ...) -> Vec<u8>;
```

`describe_invocation` returns the invocation description for a tool call by name + JSON args,
falling back to the tool name for unknown tools. `describe_invocation_for` returns `None`
for unknown tools. These are used by `run_agent_loop` to generate the description before
spawning tool threads, so error paths (timeout, panic) can include it in the `ToolOutput`.

`execute_postcard` replaces the old `execute_dyn` and calls `ToolDyn::execute_postcard()`.
`execute_json` and `execute_streaming_json` accept a `ToolOutputFormat` parameter so
callers can choose between `Text` (LLM) and `Json` (PTC) output formats.

Each tool receives an optional `working_dir: Option<&Path>` parameter that represents the session's
working directory. Filesystem and Git tools resolve relative paths against this working directory.
A leading `~` or `~/` in any path argument is expanded to the user's home directory via
`expand_tilde()` inside `resolve_path()`, so callers can write `~/project` instead of the
full absolute path. The `~user` form is *not* expanded and is passed through unchanged.

### File-read tool limits

`read_file` and `read_file_range` share a streaming, memory-bounded design. Each tool
lives in its own module (`tools/read_file.rs`, `tools/read_file_range.rs`); the shared
streaming and binary-sniff helpers (`open_text_reader`, `TextStream`, `render_streamed_line`,
`OutputBudget`, `read_line_capped`, `drain_rest_of_line`) live in `tools/mod.rs` alongside
`confine_path` and `truncate_tool_output`. `TextStream` yields one capped line at a time
with byte accounting; `render_streamed_line` validates and renders a single line (NUL /
UTF-8 checks, CRLF normalization, truncation marker); `OutputBudget` enforces the shared
byte cap across appended lines.

- **Binary rejection:** both tools peek the first 8 KiB (`BINARY_SNIFF_BYTES`) and reject
  files containing a NUL byte with a friendly `"appears to be a binary file"` error,
  mirroring ripgrep's heuristic. Returned content is always valid UTF-8 — invalid UTF-8
  in the head or in a returned line yields an explicit `"not valid UTF-8"` error rather
  than a raw std I/O error. The head is *always* sniffed, regardless of the requested
  `read_file_range` window; beyond the head, only lines that are actually returned are
  validated, so invalid content outside the requested range is skipped, not rejected.
- **Line cap:** `read_file_range` returns at most 500 lines per call
  (`MAX_READ_FILE_RANGE_LINES`); larger requests fail with a validation error, while
  requests that run past EOF clamp to the last line.
- **Output budget:** tool output is capped at 128 KiB **bytes**
  (`MAX_TOOL_OUTPUT_BYTES`). Bytes are used rather than chars so the effective token cost
  is roughly uniform across scripts (ASCII and CJK are both ~3-4 bytes per token).
  Truncation always reports totals — `showing X of Y bytes; file has N line(s)` — so the
  agent knows what it is missing and can switch to `read_file_range`. For
  `read_file_range`, X counts the returned content up to the marker — body +
  prepended header + separator newline — so the reported figure matches the
  bytes actually returned (the marker text itself is appended past the budget).
- **Per-line cap:** a single line longer than 64 KiB (`MAX_LINE_DISPLAY_BYTES`) is shown
  as a truncated prefix with a `...[line truncated]` marker; the remainder is drained
  (counted for totals, never buffered).
- **Memory:** both tools stream via `BufReader` + `TextStream` (`read_line_capped`),
  holding at most one capped line plus the output budget in memory regardless of file
  size (previously the whole file was loaded via `read_to_string`).

### Postcard binary encoding

Tools communicate with the RISC-V sandbox via a `postcard`-encoded binary protocol:

- **Arguments:** Encoded as `postcard::to_allocvec(&args)` where `args: Self::Args`
- **Return value:** Encoded as `postcard::to_allocvec(&Result::<Result<R, E>, ToolError>::Ok(Ok(ret)))`
  — a nested postcard `Result` where:
  - `Ok(Ok(ret))` — tool succeeded, `ret: R` (serialized return value)
  - `Ok(Err(e))` — tool failed, `e: E` (structured, per-tool error type)
  - `Err(e)` — infrastructure failure, `e: ToolError`
  The outer layer captures infrastructure failures (arg deserialisation); the inner
  layer captures tool-defined errors.  This allows VM guests to pattern-match on
  specific error variants (e.g. `DbError::NotFound`, `HttpError::InvalidUrl`).
- **Tool call frame (VM → host):** `[tool_name: postcard String][args: postcard-encoded Args]`

### Available tools (up to 35 total, some dependent on installed binaries)

| Group | Tools |
|---|---|
| **Core** | `list_sessions`, `get_session`, `load_skill`, `set_session_title`, `set_working_dir`, `load_tools`, `unload_tools`, `read_file`, `read_file_range`, `write_file`, `edit_file`, `list_files`, `delete_files`, `line_count`, `random` (integers, floats, booleans, bytes, UUID v4 — with optional seed), `get_current_time` (Unix millisecond timestamp) |
| **HTTP** | `http_request` (GET/POST/HEAD with headers, body, timeout) |
| **Image** | `display_image` (from path, URL, base64, or SVG text) |
| **Git** | `git_status`, `git_diff`, `git_log`, `git_add`, `git_commit`, `git_push`, `git_show` |
> **`git_diff` output:** Always returns a line-by-line unified diff wrapped in a ````diff` fenced code block. The old `full` parameter (which previously toggled between summary-only and full diff modes) has been removed — the tool now always produces full diffs. The diff output for each file change is enclosed in ````diff` ... ```` fences for clear markdown formatting.
| **EVM** | `evm_chain`, `evm_balance`, `evm_token_balance`, `evm_block`, `evm_transaction`, `evm_call`, `evm_gas`, `evm_logs`, `evm_nonce`, `evm_resolve` |
| **File search** | `grep` (file content search), `find` (file name search) |
| **RISC-V VM** | `run_riscv` (compile & run Rust code in a sandboxed RISC-V VM with access to all registered tools) |
| **Shell** | `exec` (direct program execution), `sh` (bash/dash/zsh — detected at startup), `nushell` (if `nu` is installed), `fish` (if `fish` is installed) |
| **X/Twitter** | `x_post`, `x_search_recent`, `x_user_lookup` |
| **DB** | `db_set`, `db_get`, `db_delete`, `db_delete_range`, `db_get_range`, `db_list`, `db_count` |
| **Desktop** | `notify_send` (desktop notifications with configurable summary, body, urgency, and icon) |
| **Sub-session** | `spawn_subsession` (spawns an autonomous child session with its own tool-calling loop) |

### Tool groups

Tools are organized into groups to reduce context overhead. Each tool declares its group
via `fn group() -> &'static str` on the `Tool` trait. Groups are:

| Group | Default | Description |
|---|---|---|
| `core` | always on | File system, HTTP, images, file search, random values, and time queries |
| `desktop` | off | Desktop notifications via notify-send |
| `db` | off | Session-scoped key-value database |
| `git` | on | Local Git operations |
| `shell` | on | Shell and exec |
| `x` | off | X/Twitter API |
| `vm` | off | RISC-V sandboxed code execution |

The system prompt lists all groups and their descriptions. The model uses `load_tools` to
activate additional groups and `unload_tools` to deactivate them. **core** cannot be unloaded.

Groups affect only tool **availability** in the API `tools` array — they are a discovery
mechanism, not access control. The RISC-V VM (`run_riscv`) always has access to all registered
tools regardless of group state.

Implementation details:
- `ToolRegistry::available_definitions(active)` returns definitions for all registry
  tools in the active set; every tool — including `load_tools`, `unload_tools`, and
  `set_working_dir` — is a proper `Tool` trait implementation registered in the
  default registry via `ToolRegistry::build()`
- The former meta-tools were converted from inline `&mut SessionState` handlers in
  `execute_tool_with_timeout()` to registry tools (see `tools/set_working_dir.rs`,
  `tools/load_tools.rs`, `tools/unload_tools.rs`).  They follow the
  `set_session_title` pattern: validate in the tool, then route the mutation
  through `DaemonCommand` → daemon → `SessionCommand` → the session's main loop,
  which applies it to the authoritative `SessionConfig` (broadcast + persist).
  This fixes a lost-update bug where the old inline handlers mutated the request
  worker's throwaway snapshot, which was discarded at request end
- `set_working_dir` supports tilde expansion in its `path` argument (inherited from
  `resolve_path`) and canonicalizes the target (rejecting non-existent paths and
  symlink escapes); `load_tools`/`unload_tools` carry a weak reference to the
  registry so their `groups` schema enum reflects the live group catalog
  (including dynamic MCP groups) at definition time
- `set_working_dir` performs a synchronous reply round-trip like
  `load_tools`/`unload_tools`: the daemon replies with an error immediately if
  the session is inactive, and the session main loop replies after applying the
  change — so a tool success means the authoritative state was actually updated
- `load_tools`/`unload_tools` validate their group names against the live
  registry catalog before sending (the schema enum is advisory): unknown groups
  are rejected with a clear error instead of being silently persisted into the
  session's active set.  The session handlers re-validate as defense-in-depth
  (see `unknown_group_names` / `ToolRegistry::known_group_names`)
- The three tools are restricted to `AllowedCaller::Direct` (model only) and are
  kept in the serial dispatch phase to preserve same-turn ordering of
  session-config mutations
- `list_sessions`, `get_session`, `load_skill`, and `spawn_subsession` are also
  proper `Tool` trait implementations registered in the default registry via
  `ToolRegistry::build()`, using `ToolContext.daemon_tx` to communicate with the
  daemon command loop
- Session state stores `active_tool_groups: HashSet<String>` (default: `{core, git, shell}`)
- `ToolGroup` struct and `GROUPS` constant live in `choreographr/src/tools/mod.rs`
- Group metadata is appended to the system prompt in `context::build_base_prompt()`

### Concurrent tool dispatch

`run_agent_loop` in `requests.rs` partitions tool calls into two groups before execution:

- **Serial (session-config)** — `load_tools`, `unload_tools`, `set_working_dir`.
  These no longer require `&mut SessionState` (they route mutations to the
  session main loop via `DaemonCommand`), but they still execute serially so
  same-turn ordering of session-config mutations is preserved.
- **Worker-copy mirror (Phase 3)** — after every tool in the response has
  executed, `run_agent_loop` mirrors successful session-config mutations onto
  its own worker config copy so the next agent-loop iteration observes them
  (tool definitions, system content, working-dir-relative file ops).  The
  mutations are captured in Phase 1 as a typed `PendingConfigChange`
  (`LoadTools(Vec<String>)`, `UnloadTools(Vec<String>)`,
  `SetWorkingDir(Option<PathBuf>)`) and applied in call order in Phase 3:
  the shared `apply_load_tools`/`apply_unload_tools` for the group sets, and
  for `set_working_dir` the tool's **executed result** — the canonical path is
  carried on `ToolOutput.result_json` (populated by the blanket `ToolDyn` impl
  from the tool's typed return), so the mirror reproduces exactly what the
  main loop applied with no re-resolution and therefore no TOCTOU window.  A
  rarely-reachable fallback re-runs the shared `resolve_working_dir_path`
  helper (against the working directory in effect when the response was
  planned); if even that fails, the worker still invalidates its `discovered_skills`
  cache so a stale skill set can never leak across the request boundary.  The
  mirror is deferred until the end of the response because the model planned
  every tool call in the batch against the pre-change state (parallel
  semantics) — `set_working_dir` therefore takes effect on the next
  agent-loop turn, matching its advertised description.  The worker copy is
  discarded at request end, so it cannot drift from the main loop's
  authoritative state across requests.
- **Concurrent** — all remaining tools (shell, filesystem, VM, HTTP, Git,
  `spawn_subsession`, etc.) — tools whose execution is independent of session state.
  These are dispatched across multiple OS threads in parallel using `spawn_single_tool()`.

For concurrent tools, each call gets:
1. A dedicated **execution thread** that runs the tool via `ToolDyn::execute_streaming_json()`.
2. A **forwarding thread** that relays streaming output chunks to session subscribers in
   real time through the session command channel.
3. A **wait-loop thread** that enforces the per-tool timeout (300s for shell tools, 60s for
   others, no limit for sub-sessions).
4. A dedicated **image channel** — the tool emits any produced image through this channel,
   which the wait-loop drains after execution completes.

**Thread count:** Because each concurrent tool spawns three threads (execution, forwarding,
wait-loop), dispatching N tools simultaneously creates up to 3N + 1 additional threads
(the +1 is the agent loop's main thread). The kernel scheduler handles these efficiently
for typical N (< 10), but callers should be aware of the resource footprint.

Results are collected in source-call order (the order the LLM issued them) so the
conversation history remains deterministic regardless of which thread finishes first.
If a tool thread panics, the error is caught and reported as a `ToolOutput` with
`is_error: true` instead of crashing the daemon. The `invocation_description` is generated
before spawning (via `ToolRegistry::describe_invocation`) and passed through `SpawnToolArgs`,
so even timeout and panic error paths carry a meaningful description in the `ToolOutput`.

### spawn_subsession

`spawn_subsession` is a core-group `Tool` trait implementation registered in `ToolRegistry`.
It runs in the concurrent dispatch path alongside other tools. When invoked:

1. A child session is created via `DaemonCommand::CreateSession` with the parent as
   `parent_session_id` and inheriting the parent's working directory and tool groups.
2. The prompt argument is pushed as a `SystemText` message into the child session.
3. The child session runs its own `run_agent_loop()` (model → tools → model, up to 8 iterations).
4. The child's assistant text output is collected and returned to the parent as the tool result.
5. The child session persists in the database and is listable/attachable like any other session.

The daemon maintains a `children: HashMap<u64, Vec<u64>>` on `DaemonState` tracking the
parent→child relationship. This is used for **cancellation propagation** and **cascade
deletion**:

- **Cancellation:** When a client sends `Cancel`, the daemon routes it through
  `DaemonCommand::CancelRequest` rather than sending `SessionCommand::Cancel` directly to the
  session thread. After forwarding the cancel to the target session, the daemon also calls
  `cancel_children_of()` to propagate the cancel to all active child sessions, so they stop
  their work without polling.
- **Session exit:** When a parent session exits (sleeps), `handle_session_exited` calls
  `cancel_and_shutdown_child()` on each child to shut them down gracefully.
- **Session deletion:** `handle_delete_session` cascade-deletes children before the parent by
  calling `delete_session_inner()` on each child, logging but continuing if a child's DB
  delete fails.

The child session uses `ToolContext` (`active_tool_groups`, `reasoning_effort`, `working_dir`,
`daemon_tx`) to inherit parent config and communicate with the daemon command loop.


---

## Session architecture

### Data model

Sessions are persisted to a `redb` (v4) embedded key-value store at
`~/.local/share/choreographr/state.redb`. Five tables:

| Table | Key | Value |
|---|---|---|
| `sessions` | `u64` session ID | postcard(`SessionRecord`) |
| `session_turns` | `(u64, u32)` (session ID, turn ID) | postcard(`Turn`) |
| `credentials` | `&str` service name | encrypted blob |
| `session_kv` | `(u64, String)` (session ID, key) | `Vec<u8>` |
| `meta` | `&str` | `u64` counter |

`SessionRecord` fields: `title`, `selected_model`, `parent_session_id`, `working_dir`,
`turn_count`, `created_at`, `context_config`, `account_name`.

### Session state (in-memory)

Each active session has a `SessionState` owned by its control thread. Persistent
configuration fields are extracted into `SessionConfig` to avoid duplication
across snapshot/restore, metadata conversion, and record persistence:

**`SessionConfig` (persisted):**

- `title: Option<String>` — display name
- `selected_model: Option<String>` — AI model for this session
- `reasoning_effort: Option<String>` — per-session reasoning effort slug (e.g. `"off"`, `"low"`, `"medium"`, `"high"`)
- `parent_session_id: Option<u64>` — parent session for sub-sessions
- `working_dir: Option<PathBuf>` — working directory for filesystem tools
- `max_turns: Option<u32>` — per-session tool loop iteration cap (inherits from parent). `0` = unlimited (loop runs until final answer, cancellation, or error).
- `created_at: i64` — Unix timestamp of creation
- `status: SessionStatus` — current status (Inactive, Inference, Retrying, Sleeping, …)
- `active_tool_groups: HashSet<String>` — tool groups active for this session
- `context_config: ContextConfig` — file discovery settings (context file names, max bytes)
- `account_name: Option<String>` — inference account assigned to this session
- `accumulated_usage: TokenUsage` — session-level token counter
- `context_window: Option<u32>` — model's context window size, resolved at model selection
- `last_prompt_tokens: Option<u32>` — `input_tokens` from the most recent API response;
  used for context-window progress displays (separate from the billing counter)

**Runtime fields (not persisted directly):**

- `turns: BTreeMap<u32, Turn>` — conversation turns (persisted to DB separately)
- `next_turn_id: u32` — monotonically increasing counter for turn IDs
- `last_undo_turn_ids: Option<Vec<u32>>` — stores the turn IDs from the most recent undo, enabling `/redo` to restore exactly those turns; cleared when new user input is appended after an undo
- `subscribers: HashMap<u64, mpsc::Sender<DaemonMessage>>` — attached clients
- `active_requests: HashMap<u32, ActiveRequest>` — running request cancel flags
- `provider: Option<InferenceProvider>` — resolved inference provider for the account
- `loaded_skill_bodies: Vec<LoadedSkill>` — accumulated skill bodies from `load_skill` tool calls, injected into the system prompt on every turn
- `context_cache: Option<(u64, String)>` — cached context bundle fingerprint and assembled text, avoiding re-reading context files from disk when unchanged

### Hierarchy and working directory inheritance

Sessions form a tree: a session can have a `parent_session_id` pointing to another
session. When creating a child session, if no explicit `working_dir` or `max_turns` is
provided, it inherits the parent's value. This allows sub-sessions (subagents) to operate
in the same directory as their parent with a default iteration cap.

### Persistence lifecycle

- **Startup**: `new_daemon_state()` reads all sessions and messages from the DB,
  reconstructing the in-memory `HashMap`. If the DB is empty, a default session #1
  is created.
- **Session creation**: Writes a `SessionRecord` to the DB immediately.
- **Message append**: Each `SessionMessage` (including `DisplayedImage` records for
  persisted images) is written to the DB alongside the in-memory push via
  `append_message_and_persist()`.
- **Shutdown**: The daemon sends `SessionCommand::Shutdown` to each active session, waits for request workers to drain, then exits cleanly.

### Multiple concurrent sessions

Multiple sessions can be active at the same time. Each session control thread stays
responsive while at most one request worker runs for that session. Request workers own a
snapshot of the session state and use cooperative cancellation via an `AtomicBool`.

### Undo/Redo

Sessions support undo/redo via an `undone` boolean flag on each `Turn`:

**Turn model:** Each `Turn` carries:
- `turn_id: u32` — monotonically increasing, assigned by `SessionState::start_turn()`.
- `undone: bool` — soft-delete flag; set to `true` on undo, back to `false` on redo.
- `user_text: Option<String>` — present for user-initiated turns, `None` for follow-up tool-loop turns.
- `assistant_text`, `assistant_reasoning`, `tool_calls`, `tool_results`, `displayed_images` — the assistant response.

**Undo flow (`/undo` → `ClientMessage::Undo` → `SessionCommand::Undo` → `handle_undo`):**
1. `SessionState::undo_turns()` finds the most recent non-undone turn with `user_text: Some(...)` via reverse scan.
2. Marks that turn and all higher-ID turns as `undone = true`.
3. Stores the undone turn IDs in `last_undo_turn_ids` for potential redo.
4. Persists each updated turn to the database.
5. Broadcasts `DaemonMessage::TurnsUndone { turn_ids }` to all subscribers.
6. The client removes the turns from its local history view.

**Redo flow** (`/redo` → `ClientMessage::Redo` → `SessionCommand::Redo` → `handle_redo`):
1. `SessionState::redo_turns()` restores the turn IDs stored in `last_undo_turn_ids` from the prior undo.
2. Sets `undone = false` on those turns.
3. Returns the restored turns as a `BTreeMap<u32, Turn>`.
4. Persists each restored turn.
5. Broadcasts `DaemonMessage::TurnsRedone { turns }` with full `Turn` objects so the client re-inserts them.

**Redo invalidation:** Starting a new turn with `user_text: Some(...)` after an undo clears
`last_undo_turn_ids`, making the redo unavailable — new user input starts a fresh editing session.

**Turn ordering on the client:** The `Started` and `ToolCallStarted` daemon messages
carry a `turn_id` that predicts the ID of the subsequent `Turn`. The client
uses `turn_id` to maintain a globally ordered history.

---


**Service config:** `~/.config/choreographr/config.toml`

```toml
max_turns = 25      # 0 = unlimited (loop runs until final answer or error)

[context]
context_file_names = ["AGENTS.md", "CLAUDE.md"]
context_file_max_bytes = 32768
```

> **Note:** Provider-level settings (`base_url`, `streaming`, `retry_*`, timeouts, endpoint paths, request format) have moved to per-account overrides in `accounts.toml`. See `README.md` for the full list.

**Credential storage:** Credentials are encrypted per-credential in the `redb` database (`state.redb`). Identity keys reside in `~/.config/choreographr/identity.pk` (private), `~/.config/choreographr/public.pk` (public), and optionally `~/.config/choreographr/identity.pk.enc` (passphrase-encrypted private key).

**Database:** `~/.local/share/choreographr/state.redb` (override via `CHOREOGRAPHR_DB_PATH` env var)

**Socket path:** `/tmp/Choreographr.sock` (override via `CHOREOGRAPHR_SOCKET_PATH` env var)

**Tool loop limit:** `CHOREOGRAPHR_MAX_TURNS` env var overrides `config.toml` `max_turns`. Resolution
chain: per-session `max_turns` → `CHOREOGRAPHR_MAX_TURNS` env var → `config.toml` → default 25.
A value of `0` means *unlimited* — the agent loop runs indefinitely until the model
produces a final answer, is cancelled, or hits an error.
The `spawn_subsession` tool accepts an optional `max_turns` parameter; if not set, the
child inherits the parent's value.

**Logging:** `choreographr` uses `tracing` with `tracing-subscriber`. Default level is `info`.
CLI flags `-v` (debug), `-vv` (trace), or `-q` (warn) override the level.
`RUST_LOG` env var takes precedence over CLI flags.

**Session persistence:** On daemon start, sessions are loaded from the database into
`session_metadata` (in-memory). Model selection (`/models <name>`) updates both the
in-memory metadata and the database via `UpdateMetadata → db::write_session`. The
`AttachSession` handler also populates `session_metadata` when re-loading a session
from the database, ensuring `ListModels` and metadata queries see the correct
`selected_model`.

---

## Metrics / OpenMetrics monitoring

The daemon can expose a `/metrics` HTTP endpoint in the OpenMetrics format
(suitable for Prometheus scraping).

**CLI flag:** `--metrics-addr <ADDR>` (e.g. `127.0.0.1:9464`). When the flag is
absent no metrics server is started — the daemon runs exactly as before.

**Endpoint:** `GET /metrics` returns `Content-Type: text/plain; version=0.0.4; charset=utf-8`.

### Exposed metrics

| Metric | Type | Labels | Description |
|---|---|---|---|
| `choreo_sessions_active` | Gauge | — | Number of active sessions |
| `choreo_connections_active` | Gauge | — | Number of active client connections |
| `choreo_requests_total` | Counter | `status` (`done`, `failed`, `cancelled`) | Total requests processed |
| `choreo_tool_executions_total` | Counter | `tool`, `status` (`ok`, `error`) | Tool call count |
| `choreo_api_calls_total` | Counter | `model`, `endpoint` | API call count |
| `choreo_api_errors_total` | Counter | `model`, `error_type` | API error breakdown |
| `choreo_connections_total` | Counter | — | Total connections accepted |
| `choreo_turns_total` | Counter | `model` | Agent loop turns |
| `choreo_request_duration_seconds` | Histogram | `status` | Request latency |
| `choreo_tool_execution_duration_seconds` | Histogram | `tool` | Per-tool execution time |
| `choreo_api_call_duration_seconds` | Histogram | `model`, `endpoint` | API round-trip time |

Process-level metrics (RSS, CPU, FD count) are also exposed via the `prometheus`
crate's `process` feature.

### Implementation

The metrics module (`src/metrics.rs`) uses `std::sync::LazyLock` for a single
static `Metrics` struct that wraps Prometheus counters/gauges/histograms. All
operations are atomic (no `Arc<Mutex>` needed). A dedicated thread serves the
`/metrics` endpoint via `tiny_http`; it polls the shutdown flag every 1 second
and exits cleanly when the daemon shuts down.

### Instrumentation points

| Location | Function | Metrics recorded |
|---|---|---|
| `daemon.rs` — `CreateSession` handler | `record_session_created` | `choreo_sessions_active +1` |
| `daemon.rs` — `SessionExited` handler | `record_session_exited` | `choreo_sessions_active -1` |
| `server/connection.rs` — `client_thread` start | `record_client_connected` | `choreo_connections_active +1` |
| `server/connection.rs` — `client_thread` end | `record_client_disconnected` | `choreo_connections_active -1` |
| `server/lifecycle.rs` — accept loop | `record_connection_accepted` | `choreo_connections_total +1` |
| `sessions.rs` — `run_request_worker` | `record_request_total`, `record_request_duration` | request status + latency |
| `requests.rs` — `run_agent_loop` turn | `record_turn` | turn count per model |
| `requests.rs` — `execute_tool_with_timeout` | `record_tool_execution` | tool duration + status |
| `providers/shared.rs` — `timed_result` | `record_api_call`, `record_api_error` | API latency + errors (all providers) |

---

## Data flow: a prompt from input to response

```
1. User types "hello" in choreo-tui
        │
2. choreo-client-core::shell::parse_input_line("hello")
   → ClientMessage::RunInput { request_id: 1, input: "hello" }
        │
3. choreo-tui writer task serializes + frames → Unix socket → choreographr
        │
4. choreographr server.rs handles RunInput:
   - validates session exists and is attached
   - checks no duplicate active request_id
   - sends DaemonMessage::Started { request_id: 1 }
   - appends SessionMessageKind::UserText("hello") to session
   - calls requests.rs to execute
        │
5. requests.rs builds message array from session history
   → calls openai::chat_completions or openai::responses (based on request_format_for_model)
        │
6. openai::chat_completions / openai::responses streams SSE chunks
   → per chunk: DaemonMessage::OutputChunk { request_id: 1, stream: true, data: "Hello" }
        │
7. DaemonMessage is serialized + framed → socket → choreo-tui
        │
8. choreo-tui reader task receives OutputChunk
   → pushes to UI event stream
        │
9. UI loop consumes event → updates ClientHistory → re-renders
        │
10. Final chunk arrives → DaemonMessage::Done { request_id: 1 }
    choreo-tui marks request complete, adds session message
```

### Image flow (tool-triggered)

Images are delivered out-of-band via a one-shot `mpsc` channel rather than embedded in
`ToolOutput`:

```
Model calls display_image tool
  → daemon creates (image_tx, image_rx) channel
  → passes image_tx to ToolDyn::execute_json → execute_streaming_json
  → tool extracts PreparedImage from typed return → sends via image_tx
  → agent loop drains image_rx after tool completion
  → emit_and_persist_image creates SessionMessageKind::DisplayedImage
  → broadcasts it to live subscribers mid-turn via
    SessionCommand::Broadcast(SessionMessageAppended { DisplayedImage })
  → client push_session_message converts DisplayedImage → RenderedImage
  → also persists to DB + pushes to session messages for replay
  → after request completes, handle_request_finished skips DisplayedImage
    entries in its snapshot delta (already delivered mid-turn) and
    broadcasts only non-image messages (AssistantText, ToolResult, …)
```

### Session switch flow (updated)

```
User presses Enter on a session in the session manager
  → history cleared (client.history, render_cache, scroll, in_progress)
  → AttachSession sent to daemon
  → daemon responds with SessionState { messages: Vec<SessionMessage>, … }
    where messages may include SessionMessageKind::DisplayedImage for persisted images
  → for each message:
    DisplayedImage → RenderedImage::new_placeholder(metadata, Arc::from(data)) → HistoryItem::Image
    other         → classify_session_message → HistoryItem::{SessionMessage,Text,Diff}
```


---

## Design decisions

1. **Unix sockets, not HTTP for client↔daemon** — keeps everything local, avoids port conflicts,
   leverages OS-level access control.

2. **Binary protocol (postcard), not JSON** — compact, typed, versioned. Length-prefixed framing
   avoids parsing ambiguities. Version field allows protocol evolution.

3. **Lock/Unlock security** — the daemon starts without credentials in memory. The private key is
   sent over the Unix socket and zeroized after use. Credentials are encrypted per-credential so
   they can be stored in the database without a global passphrase.

4. **Sessions, not per-client state** — sessions are independent from client connections. A
    session has its own model, working directory, and messages. Clients subscribe/unsubscribe from sessions
   via the broadcast system. Sessions persist in a redb database and survive daemon restarts.
   Session lifecycle operations (create, attach, list, delete) succeed even when the daemon
   is locked — credentials are only needed to run models. Sessions carry an optional
   `account_name` field that determines which provider credential to use at request time;
   there is no global "default account" fallback. The provider is resolved lazily from the
   daemon's provider registry when the first RunInput is issued.

5. **Session hierarchy** — sessions can have parent sessions (`parent_session_id`), forming a
    tree. Child sessions inherit their parent's working directory unless explicitly overridden. The
   `spawn_subsession` tool creates autonomous child sessions that run their own tool-calling
   loop and report results back to the parent.

6. **Tool-call loop in the daemon** — the daemon drives multi-turn tool interactions (up to a
   configurable `max_turns` per session, default 25 (0 = unlimited) rather than pushing that complexity to
   the client or model. The client just sees `ToolCallStarted`/`ToolCallFinished` events.

7. **Session subscription model** — multiple clients can subscribe to the same session. Events
   are broadcast to all subscribers except the originator, enabling shared session viewing.

8. **SSE streaming** — a custom `SseReader` (not a library) handles `data:` lines and `[DONE]`
   for OpenAI SSE, giving full control over parsing and buffering behavior. The Anthropic
   module has its own `AnthropicSseReader` that handles both `event:` and `data:` lines
   (required by the Anthropic Messages streaming format) and yields `(event_type, data)` pairs.

9. **Markdown as the intermediate format** — all text (tool output, assistant text, error
    messages) is treated as markdown and rendered as HTML (desktop) or shaped to terminal output
    (choreo-tui), providing a consistent rendering layer.

10. **Flexible API format** — both OpenAI Chat Completions and Responses are first-class
    citizens, selectable per-model via a `RequestFormat` enum (`ChatCompletions` / `Responses`).
    The dispatch mechanism lives in `ServiceConfig::request_format_for_model()`: it checks
    the provider catalog's per-model `openai_responses` flag and falls back to `default_request_format`.
    Every entry point (`completion`, `completion_stream`, `chat_completion_turn`,
    `chat_completion_turn_streaming`) matches on the resolved format and calls the appropriate
    request builder. Input/output mapping differs between the two: system messages go into the
    `input` array as `{role: "system"}` items (rather than the `instructions` field), tool results
    become `function_call_output` items, and the `input` is an array of typed items rather than a
    flat messages list. Multi-turn chaining uses `previous_response_id` to link turns together,
    while Chat Completions relies on the full message history.

    **Programmatic tool calling (Responses API, gpt-5.6+):** When enabled, the Responses
    request body includes a `programmatic_tool_calling` tool with `type: "programmatic_tool_calling"`.
    The model responds with `response.program.code.delta` and `response.program.code.done` events
    carrying generated JavaScript, plus `response.program_output.done` with execution results.
    The daemon's `Tool` trait exposes `output_schema()` (JSON Schema describing each tool's
    return value) and `allowed_callers()` (whether a tool is callable directly by the model,
    programmatically, or both). These are plumbed through `ChatToolDefinition::function_with_options()`
    and `ResponsesTool` into the wire format. Per-model auto-enablement is controlled by
    `ServiceConfig::programmatic_tool_calling_for_model()`; account-level override via
    `accounts.toml`'s `programmatic_tool_calling` field.

11. **Pluggable providers via `InferenceProvider`** — the provider system supports OpenAI-compatible,
    Anthropic Messages, Google Gemini, and Mistral APIs. Each provider implements the same interface
    (`chat_completion_turn`, `chat_completion_turn_streaming`, `list_models`) and is constructed
    from an `AccountConfig` + credential. Accounts are defined in `accounts.toml` with a
    `provider` field (`"openai"`, `"opencode"`, `"anthropic"`, etc.). The TUI new-account form
    offers all registered provider options via a shared `PROVIDER_OPTIONS` array.

12. **OS threads with sidecar async runtime** — the daemon avoids async Rust everywhere except
    where third-party libraries (alloy) require it. A global `OnceLock<tokio::runtime::Runtime>`
    serves as a sidecar for those async calls via `block_on()`. This simplifies the mental model
    (each thread owns its data, no `Send` bounds on shared state, no `Pin<Box<dyn Future>>`),
    improves stack traces, and avoids the complexity of async cancellation.




---

## Context file discovery

The daemon automatically discovers and injects project-specific context files
(`AGENTS.md`, `CLAUDE.md`) and skills at session creation, and refreshes them
before every model call (every turn of the tool-call loop).

### System prompt construction

Each turn in the agent loop calls `build_system_content()` which constructs the
system prompt from four sources:

1. **Base prompt** — identity, tool group listing, available skill metadata, and
   any loaded skill bodies (accumulated via `load_skill` calls).
2. **Project context files** (`AGENTS.md`, `CLAUDE.md`, etc.) — discovered by
   `discover_context()` and assembled by `assemble_context()`. Results are cached
   on `SessionState::context_cache` (fingerprint + assembled text) and reused
   when the fingerprint is unchanged.
3. **Subdirectory hints** — hints accumulated from filesystem tool calls in the
   previous turn, appended under "## New context from project subdirectories".
4. **Loaded skills** — `<skill name="...">...</skill>` blocks injected after the
   "Available skills" listing.

The system prompt is rebuilt every turn so that newly loaded skills and newly
discovered subdirectory hints are visible to the model immediately.

### Discovery algorithm

1. **Global files** (loaded first, prepended):
   - `~/.config/choreographr/AGENTS.md`
   - `~/.claude/CLAUDE.md` (unless `disable_claude_code_prompt` is set)
   - `~/.agents/AGENTS.md`
2. **Project files** (walking from session working directory up to the git repository root):
   - At each ancestor directory, checks `AGENTS.md` first, then `CLAUDE.md`.
   - Only one file per directory (first match in the configured `context_file_names` list).
   - Collected bottom-up (outermost first), then rendered in reverse order so
     closer-to-working-directory instructions appear last.

### Subdirectory hints

When filesystem tools (`read_file`, `list_files`, `grep`, `find`, etc.) access a file
in a subdirectory below the session working directory, the daemon walks up from that file's
parent toward the working directory and checks for `AGENTS.md`/`CLAUDE.md` files not already in
the main context. Any found hint content is appended to the tool result message
(not the system prompt), preserving prompt cache stability.

### Skills (Agent Skills standard)

Skills are discovered from:
- `~/.agents/skills/<name>/SKILL.md` (global)
- `.agents/skills/<name>/SKILL.md` (project, relative to session working directory)

Each `SKILL.md` must have YAML frontmatter with `name` and `description`.

**Progressive disclosure:** At session start, only metadata (name + description)
is included in the stable prompt (`messages[0]`). When the model calls the
`load_skill` tool with a skill name, the full `SKILL.md` body is loaded and
injected as a new `SystemText` message.

### Fingerprint-based refresh

Before each turn in the tool-call loop, the daemon computes a fingerprint via
`compute_fingerprint()` of all known context file paths and their mtimes. If the
fingerprint matches the cached value on `SessionState::context_cache`, the assembled
context string is reused without re-reading files from disk. If it differs (file
added, removed, or modified), the context is rebuilt and the cache is updated.

### Configuration

```toml
# ~/.config/choreographr/config.toml
[context]
context_file_names = ["AGENTS.md", "CLAUDE.md"]   # ordered list; first match per directory
context_file_max_bytes = 32768                     # max combined context size
disable_claude_code_prompt = false                 # skip ~/.claude/CLAUDE.md
```

### User system prompt override

The stable base prompt (`messages[0]`) is loaded from
`~/.config/choreographr/system.md` if it exists. Otherwise, a built-in default is
used. The default lives at `choreographr/system.md` in the repository and is
embedded at compile time via `include_str!`.

### Module

Implementation lives in `choreographr/src/context.rs`. Key entry points:

| Function | Purpose |
|---|---|
| `discover_context(working_dir, config)` | Walk filesystem, return `ContextBundle` with all discovered files |
| `discover_skills(working_dir)` | Scan Agent Skills directories, return `Vec<SkillMeta>` |
| `assemble_context(bundle)` | Render discovered files into an XML-like format for injection |
| `build_base_prompt(skills, groups, loaded_skills)` | Build the stable system prompt (identity + tool groups + skill metadata + loaded skill bodies) |
| `recheck_context(working_dir, config, old_fp)` | Re-discover and compare fingerprints |
| `subdirectory_hints(tool_name, args, working_dir, known)` | Return `Option<(String, Vec<PathBuf>)>` — subdirectory hint text and newly discovered paths |
| `load_skill_body(name, working_dir)` | Load the full body of a SKILL.md, stripping YAML frontmatter |

### Tool: `load_skill`

Registered alongside other tools in the tool loop (core group). When the model calls
`load_skill(name)`, the daemon:

1. Finds the matching `SKILL.md` from `~/.agents/skills/` or `.agents/skills/`
2. Strips the YAML frontmatter
3. Returns `"Loaded skill: <name>"` as the tool result

**Persistence:** After the tool result is collected, `run_agent_loop` detects the
`load_skill` call and pushes a `LoadedSkill { name, body }` into
`SessionState::loaded_skill_bodies`. On every subsequent turn, the
`build_system_content` helper includes all loaded skill bodies in the system prompt,
wrapped in `<skill name="...">` XML tags. This ensures skill instructions remain
visible to the model even as tool results scroll out of the context window.

### `run_riscv` — RISC-V sandboxed code execution

`run_riscv` is a tool that compiles Rust source code into a RISC-V ELF binary and executes it
inside a sandboxed virtual machine powered by `ckb-vm`. It is registered as a manual
`impl Tool` (not via `define_tool!`) to pass `x_credentials` and `working_dir` through
to the guest syscall handler.

**Execution flow:**

1. Accepts either Rust `source` or pre-compiled base64 `program`.
2. If `source` is provided, it is first formatted via `rustfmt` (silently skipped
   if `rustfmt` is unavailable).  The formatted source is then prepended with a
    `#![no_std]` boilerplate (panic handler, entry point, `Choreographr` module with
     `tool_call`, `write`, `exit` syscall wrappers, dynamically-sized linked-list allocator)
    and compiled via a single
    `rustc +stable --target riscv64imac-unknown-none-elf` invocation in a temp
   directory.
3. Creates a `DefaultCoreMachine<u64, FlatMemory<u64>>` with 4 MB of flat memory.
4. Registers a `ChoreographrSyscall` handler that intercepts three guest syscalls:
   - **Syscall #0 (TOOL_CALL)** — reads a postcard-encoded frame `[tool_name: String][args: bytes]`
     from guest memory, dispatches it via the `ToolRegistry::execute_dyn()`, and writes the
     postcard-encoded `Result<Return, String>` result to the guest's output buffer.
   - **Syscall #1 (WRITE)** — copies guest data into an accumulator buffer that becomes the tool's
     output upon VM exit.
   - **Syscall #93 (EXIT)** — stops the VM. Uses the Linux exit syscall number
     so that CKB-VM's `DefaultMachine::ecall()` handles it natively, properly
     propagating the exit code from register A0.
5. Loads the ELF via `TraceMachine::load_program` and runs via `TraceMachine::run()`.
6. After execution, the machine is dropped and the output channel is drained with
   a blocking `recv()` loop (deterministic — no buffered-item race).
7. Returns the formatted source wrapped in a `rust` markdown fenced code block,
   followed by the accumulated WRITE output, then a `[VM: exited with code N in M cycles]`
   summary line.  The TUI renders the code block with syntect syntax highlighting.

**Guest ABI** (auto-generated in the boilerplate):

```rust
pub mod Choreographr {
    pub unsafe fn tool_call(request: &[u8], output: &mut [u8]) -> usize;
    pub fn write(data: &[u8]);
    pub fn exit(code: i32) -> !;
}
```

A `#[global_allocator]` linked-list allocator is always included, enabling `alloc` crate
types (`Vec`, `String`, `format!`, `Box`, etc.), and `args()` is injected as a free function
returning `Vec<Vec<u8>>`:

```rust
pub fn args() -> Vec<Vec<u8>>;
```

**Safety:** The guest runs in an isolated VM with 4 MB of flat memory. All tool access goes
through the same `ToolRegistry` as the host agent, respecting the same `x_credentials` and `working_dir`.
The guest cannot access host memory, syscalls, or files outside the VM without going through
registered tools.

### `exec` — direct program execution (no shell)

`exec` spawns a program directly without shell interpretation. The command is split into argv (program + args array) and passed to `execvp` — no pipes, redirects, glob expansion, or environment variable interpolation.

Sandboxing is identical to the shell tools: timeout, rlimits, env sanitization, path confinement, output truncation, and non-interactive stdin.

Use `exec` when the command is a single program with explicit arguments. Prefer it over `sh` when you don't need shell features — it avoids shell-injection surface.

### `sh` — POSIX shell command execution

`sh` runs shell commands via a POSIX-compatible shell (`bash`, `dash`, or `zsh`). The `shell` parameter lists all three variants unconditionally (the manual `JsonSchema` impl emits a flat `"enum"` array instead of `oneOf`/`const` for wider provider compatibility). The `shell` parameter must be explicitly specified (no default), and `sh` itself is intentionally excluded — use `bash`, `dash`, or `zsh` directly.

Sandboxing (shared across all shell/exec tools via `shell_util.rs`):

1. **Timeout** — the command is killed after a configurable timeout (default 30s, max 300s). A watchdog thread enforces the inner timeout; the outer tool loop timeout is extended to 300s for this tool.

2. **Resource limits** — set via `setrlimit` in the child (pre-exec): `RLIMIT_AS` (4 GB) prevents runaway memory allocation, `RLIMIT_FSIZE` (100 MB) prevents disk-filling writes.

3. **Environment sanitization** — dangerous env vars (`LD_PRELOAD`, `LD_LIBRARY_PATH`, `LD_AUDIT`, `LD_DEBUG`, `PYTHONPATH`, `PERL5LIB`, `RUBYLIB`, `DYLD_INSERT_LIBRARIES`) are stripped in the child before exec.

4. **Path confinement** — the resolved working directory is canonicalized and must be at or below the session working directory. Absolute paths or `..` traversals that escape the project directory are rejected.

5. **Output limits** — stdout/stderr are combined and truncated to 16 KB via `truncate_tool_output`, preventing context overflow.

6. **Non-interactive** — stdin is not connected. Commands that attempt to read from stdin will hang until the timeout.

### `nushell` — nushell command execution with sandboxing

`nushell` runs commands in a child `nu -c` process with the same sandboxing as `sh`. Registered only when the `nu` binary is found in `PATH`.

### `fish` — fish shell command execution with sandboxing

`fish` runs commands in a child `fish -c` process with the same sandboxing as `sh`. Registered only when the `fish` binary is found in `PATH`.


| Layer | What's tested | Location |
|---|---|---|
| Protocol | Framing, version handling, round-trip encode/decode | `choreo-proto/src/tests.rs` |
| Client core | Shell parsing, markdown→HTML, image assembly, history | `choreo-client-core/src/tests.rs` |
| Daemon | Request lifecycle, session CRUD, cancellation, tool calls, model listing | `choreographr/src/tests.rs`, `choreographr/tests/integration.rs` |
| MCP (choreo-mcp) | Server spawn, tool discovery, echo tool call/response | `choreo-mcp/tests/mcp_integration.rs` |
| MCP (daemon) | McpManager + ToolRegistry integration, dynamic group registration, tool execution | `choreographr/tests/mcp_integration.rs` |
| Daemon OpenAI | SSE parsing, HTTP request construction, config loading | `choreographr/src/openai/tests.rs`, `choreographr/src/openai/chat_completions.rs`, `choreographr/src/openai/config.rs` |
| Daemon Anthropic | Content block deserialisation, response→turn result conversion, message payload building, config overrides | `choreographr/src/anthropic/tests.rs` |
| choreo-tui | SVG rasterization, Unicode width, app state | `choreo-tui/src/app_tests.rs`, `choreo-tui/src/lib_tests.rs` |
| choreo-gui | App state, render helpers | `choreo-gui/src/app_tests.rs` |

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
cargo run -p choreographr

# Run terminal client
cargo run -p choreo-tui

# Run desktop client
cargo run -p choreo-gui

# Run IM bridge (Telegram)
cargo run -p choreo-im -- telegram
```


---

## External dependencies (key crates)

| Crate | Used by | Purpose |
|---|---|---|
| `tokio` | tui, dioxus, im | Async runtime |
| `serde` + `postcard` | proto, clients, daemon | Wire protocol framing and internal storage |
| `snow` | daemon, client-core, transport | Noise IK handshake and transport encryption |
| `ureq` | daemon | HTTP client |
| `pulldown-cmark` + `ammonia` | client-core | Markdown parsing, HTML sanitization |
| `ratatui` + `crossterm` | choreo-tui | Terminal UI |
| `dioxus` | choreo-gui | Desktop UI |
| `image` + `resvg` | daemon, choreo-tui | Image decoding, SVG rasterization |
| `syntect` | choreo-tui | Syntax highlighting for code blocks (uses Sublime Text grammar files) |
| `aes-gcm` + `argon2` | keystore | Encryption, key derivation |
| `x25519-dalek` + `hkdf` + `sha2` | keystore | X25519 ECDH key agreement, HKDF key derivation |
| `ckb-vm` | daemon | RISC-V VM interpreter for sandboxed code execution |
| `postcard` | daemon | Compact binary serialization (internal storage, VM↔host tool communication) |
| `thiserror` | proto, keystore, client-core, daemon | Structured library error types |
| `anyhow` | daemon, tui, dioxus, im, keystore | Application error context & propagation |


---

## Error handling strategy

### Library crates — `thiserror`

Each library crate defines a structured error enum:

| Crate | Error type | Key variants |
|---|---|---|
| `choreo-proto` | `ProtoError` | `Postcard`, `FrameTooLarge`, `TrailingBytes`, `UnsupportedVersion`, `Io` |
| `choreo-keystore` | `KeystoreError` | `Io`, `TooShort`, `DecryptionFailed`, `InvalidKeyLength`, `EncryptionFailed`, `ConfigDirNotFound` |
| `choreo-client-core` | `ClientError` | `Proto`, `Io`, `Utf8`, `ImageTooLarge`, `ImageExceedsSize`, `DuplicateImage`, `UnknownImage`, `ImageSizeMismatch` |

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

Each tool defines its own error type via the `type Error` associated type on the `Tool` trait.
Simple tools use `ToolExecError` (a string-wrapper newtype). Tools whose errors are consumed by
VM guests (e.g. `DbError`, `HttpError`) define a `thiserror` enum that is `Serialize` +
`Deserialize`, enabling the guest to pattern-match on specific variants.

The `ToolError` enum (thiserror) covers *infrastructure* failures that happen around tool execution:
argument deserialisation, I/O, postcard encoding. It is never returned by a tool's `execute()`
directly — only by the `ToolDyn` conversion layer.

The `ToolDyn::execute_json()` and `execute_streaming_json()` methods return
`Result<ToolOutput, ToolError>` so callers can distinguish infrastructure errors
(JSON deserialisation) from tool execution errors. The caller converts `Err(e)` into a
`ToolOutput { is_error: true }` for the LLM path, preserving the structured error
for programmatic consumers.

The postcard binary path encodes all outcomes as a nested
`Result<Result<R, E>, ToolError>`: `Ok(Ok(ret))` for success, `Ok(Err(e))` for a
structured tool error, and `Err(e)` for an infrastructure failure. The `encode_outer()`
helper in `choreographr/src/tools/mod.rs` handles this serialization.

`ToolOutput` replaces the old `ToolExecutionOutput` and `ToolResult` types. The `ToolOutputFormat`
enum lets callers choose between `Text` (human-readable via `return_string`) and `Json`
(JSON-encoded via `serde_json::to_string`) output formats.
| `gix` | daemon | Git operations |
| `teloxide` | choreo-im | Telegram Bot API client |
| `prometheus` | daemon | OpenMetrics instrumentation, process metrics |
| `tiny_http` | daemon | Metrics HTTP server for `/metrics` endpoint |
| `tracing` | daemon | Structured logging |
