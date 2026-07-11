# tai Architecture

## Overview

`tai` is a local-first AI assistant built as a Rust workspace. A **daemon** process
communicates with multiple LLM providers through a pluggable trait-based provider
system, while **clients** (terminal, desktop, and IM platforms) connect to the daemon
over a Unix domain socket (or Noise IK encrypted TCP for remote connections) using a custom length-prefixed binary protocol.

```
┌──────────────┐    Unix socket     ┌──────────────┐    HTTP/SSE     ┌──────────────────────┐
│   tai-tui     │◄──────────────────►│              │◄──────────────►│  OpenAI API          │
│  (terminal)  │                    │              │                ├──────────────────────┤
├──────────────┤                    │  tai-daemon  │◄──────────────►│  Anthropic Messages   │
│ tai-dioxus   │◄──────────────────►│              │                ├──────────────────────┤
│  (desktop)   │    Unix socket     │              │◄──────────────►│  Google Gemini API    │
├──────────────┤                    │              │                ├──────────────────────┤
│   tai-im     │◄──────────────────►│              │◄──────────────►│  Mistral API          │
│ (IM bridge)  │    Unix socket     │              │                ├──────────────────────┤
└──────────────┘                    └──────────────┘                └──────────────────────┘
                                                                    │  30+ OpenAI-compat    │
                                                                    │  providers via catalog│
                                                                    └──────────────────────┘
```

---

## Workspace topology

Nine crates in a single Cargo workspace (resolver = "3"):

```
tai (workspace)
├── tai-proto           Wire protocol (shared types + framing)
├── tai-keystore        X25519 + ECDH keypair crypto, encrypted storage primitives
├── tai-transport       Noise IK encrypted TCP transport abstraction
├── tai-client-core     Shared client logic (parsing, images, history, credentials)
├── tai-markdown        Markdown parser and HTML renderer (pulldown-cmark + ammonia)
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
              ┌──────────────┼──────────────────────┐
              │              │                      │
      ┌────────▼────────┐   │       ┌───────────────▼──────────┐
      │ tai-client-core │   │       │        tai-daemon        │
      └────┬───────┬────┘   │       └───────────────┬──────────┘
           │       │        │                       │
           │  ┌────▼────────▼────┐                  │
           │  │  tai-transport   │◄─────────────────┘
           │  └────────┬────────┘
           │           │
      ┌────▼───┐ ┌────▼────┐ ┌────▼───┐
      │tai-tui  │ │tai-dioxus│ │tai-im  │
      └────────┘ └─────────┘ └────────┘
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
| `SessionMessage` | A single turn in a conversation. Variants: `SystemText`, `UserText`, `AssistantText`, `AssistantToolUse`, `ToolResult`, `DisplayedImage` (persisted image replay) |
| `ImageMetadata` | Mime type, dimensions, byte length for streamed images |
| `DisplayedImageRecord` | Binary image data + `ImageMetadata` for persisted image replay (carried inside `SessionMessage::DisplayedImage`) |
| `ThinkingEffort` | Enum controlling how much reasoning/thinking the model performs: `Off`, `Low`, `Medium`, `High`. Stored per-session and passed through to each provider's wire format. |
| `TokenUsage` | Tracks LLM token consumption (`input_tokens`, `output_tokens`, `total_tokens`). Embedded in `SessionMessage::AssistantText` and `AssistantToolUse` for per-turn accounting, and in `SessionSummary` and `DaemonMessage::SessionState` for session-level totals. |

`ClientMessage` variants:
`CreateSession`, `ListSessions`, `AttachSession`, `GetSessionState`, `RunInput`,
`TestImage`, `Cancel`, `Ping`, `GetCredential`, `ListModels`, `SetModel`, `Unlock`,
`Lock`, `AddCredential`, `RemoveCredential`, `AddAccount`, `RemoveAccount`,
`ListAccounts`, `SetSessionAccount`, `SetReasoningEffort`, `GetReasoningEffort`
- `CreateSession` now carries optional `context_config` and `account_name` fields

`DaemonMessage` variants:
- Session: `SessionCreated`, `Sessions`, `SessionAttached`, `SessionState`, `SessionMessageAppended`, `SessionFailed`, `SessionDeleted`, `SessionDeleteFailed`
- Request lifecycle: `Started`, `OutputChunk`, `Done`, `Failed`, `Cancelled`
- Tool lifecycle: `ToolCallStarted`, `ToolCallFinished`, `ToolCallFailed`, `ToolCallOutput`
- Image streaming: `ImageStart`, `ImageChunk`, `ImageEnd`
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


### `tai-keystore` — Identity keypair & credential crypto

Provides the cryptographic primitives for credential management. No longer a standalone
CLI binary — it is a library used by `tai-client-core` and `tai-daemon`.

**Identity keypair (X25519):**
The daemon's identity is an X25519 keypair stored as two files:
- `~/.config/tai-daemon/identity.pk` — raw 32-byte private key
- `~/.config/tai-daemon/public.pk` — raw 32-byte public key

The private key can be stored encrypted at rest:
- `~/.config/tai-daemon/identity.pk.enc` — Argon2 + AES-256-GCM encrypted private key

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


### `tai-transport` — Noise IK encrypted transport

A small crate providing Noise IK handshake and encrypted message I/O over
TCP.  Used by both `tai-client-core` (client side) and `tai-daemon` (server side).

| Module | Purpose |
|---|---|
| `noise.rs` | `NoiseStream` — wraps `TcpStream` + `snow::TransportState` with length-prefixed AES-256-GCM framing. `handshake_initiator()` (client) and `handshake_responder()` (server) implement the Noise IK handshake with X25519 key agreement. |
| `error.rs` | `TransportError` enum — `Io`, `Noise`, `Protocol`, `AuthFailed`, `ConnectionClosed`. |

The server-side TCP/Noise handler lives in `tai-daemon/src/server/connection.rs`
(`tcp_client_thread`), where the Noise IK handshake is performed and the
encrypted stream enters the same dispatch loop as Unix socket clients.


### `tai-client-core` — Shared client logic

Used by `tai-tui`, `tai-dioxus`, and `tai-im`.

| Module | Purpose |
|---|---|---|
| `shell.rs` | Parses terminal input into `ShellCommand`: `/ping`, `/models`, `/model` (alias), `/cancel`, `/unlock`, `/lock`, `/image`, `/add-key`, `/add-x`, `/remove-key`, or `RunInput(prompt)`. All commands use `/` prefix exclusively; `parse_command()` is the single dispatch point. |
| `credentials.rs` | Shared helpers: `resolve_private_key()` (read or decrypt the identity key), `build_add_credential_message()` (encrypt and package a credential for the daemon), `read_public_key_bytes()`. Eliminates duplicated logic across `tai-tui`, `tai-dioxus`, and `tai-im`. |
| `image.rs` | `ImageAssembler` reconstructs images from chunked stream protocol (`ImageStart` → `ImageChunk`* → `ImageEnd`), validating byte count |
| `history.rs` | `ClientHistory` ring buffer of `HistoryItem` entries (text, images, session messages, streaming text, structured diffs) |
| `diff.rs` | Types for structured unified diff representation (`DiffLineKind`, `DiffLine`, `DiffHunk`, `FileDiff`) |
| `connection.rs` | Daemon connection helpers: `run_daemon_connection()` (Unix socket), `run_daemon_tcp_connection()` (Noise IK), `run_daemon_connection_with_mode()` (dispatch), `run_daemon_reader()` (blocking reader). `ConnectionMode` enum (`UnixSocket` | `Tcp`) selects the transport. |

`DaemonMessageHandler` trait uses `ClientError` (thiserror enum) — `Proto`, `Io`, `Utf8`, `ImageTooLarge`, `ImageExceedsSize`, `DuplicateImage`, `UnknownImage`, `ImageSizeMismatch`, `PrivateKeyRead`, `PrivateKeyInvalid`, `PrivateKeyEncRead`, `PrivateKeyDecrypt`, `PublicKeyRead`, `PublicKeyInvalid`, `CredentialParse`, `Postcard`, `Encryption`.


### `tai-daemon` — Core server

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
| `providers/` | `ProviderClient` trait + `ProviderCatalog` system. `InferenceProvider` struct wraps `Arc<dyn ProviderClient>`. Static `PROVIDER_CATALOG` maps ~31 slugs to protocol type, default base URL, and default model. Dispatches to the correct client based on protocol. |
| `providers/shared.rs` | Shared provider infrastructure: `ProviderError` (unified error type used by all providers), `build_http_client()`, `error_type_label()`, `provider_error_to_inference()`, `timed_result()` (metrics instrumentation wrapper). Eliminates duplicated error types, `From<ProviderHttpError>` impls, and error conversion functions across provider implementations. |
| `anthropic/` | Anthropic Messages API client (`AnthropicClient`). Implements `ProviderClient`. |
| `google/` | Google Gemini API client (`GoogleClient`). Implements `ProviderClient`. Uses its own SSE reader for streaming. |
| `mistral/` | Mistral API client (`MistralClient`). Implements `ProviderClient`. Uses OpenAI-compatible SSE reader for streaming. |
| `retry/` | Shared HTTP retry logic extracted from the OpenAI module. `ProviderHttpError` enum captures HTTP error codes generically; `retry_loop()` provides exponential backoff with jitter, retryable status detection, and cancellation support. All provider modules use this via the shared `ProviderError` type conversion. |
| `sessions.rs` | `SessionState` (split into `SessionConfig` for persisted fields + runtime state), `RequestContext` dependency bundle, `SessionCommand` enum and its handler functions. Each session has a control thread running `session_main()`; request work runs on separate worker threads via `run_request_worker()`. Sessions form a tree (parent → child sub-sessions), each with an optional CWD. |
| `requests.rs` | Prompt execution: builds messages from session history, runs model requests, drives tool-call loop. |
| `context.rs` | Context file discovery, skills, fingerprint-based refresh. |
| `metrics.rs` | Prometheus/OpenMetrics gauges, counters, histograms; HTTP server for `/metrics` endpoint. |
| `openai/` | HTTP integration with OpenAI-compatible APIs, SSE streaming, service config loading. |
| `tools/` | `Tool` trait, `ToolRegistry` (with injectable `FffStateCache` replacing a global `OnceLock`), and 30 registered tools (including `list_sessions`, `get_session`, `load_skill` via `admin.rs`). |
| `tools/context.rs` | `ToolContext` — session-scoped context (session ID + `Arc<Database>` + `mpsc::Sender<DaemonCommand>`) passed to tools that need DB or daemon access. |
| `tools/db.rs` | Session-scoped KV database tools (`db_set`, `db_get`, `db_delete`, `db_delete_range`, `db_get_range`, `db_list`, `db_count`). |
| `tools/vm.rs` | RISC-V sandbox: compiles Rust → ELF via rustc, executes in `ckb-vm` with custom syscall handler (`TaiSyscall`) for tool dispatch. |

### Provider Architecture

The provider system has three layers:

**1. `ProviderClient` trait (`providers/traits.rs`):**
```rust
/// Holds the common parameters for a chat completion turn.
pub struct ChatTurnRequest<'a> {
    pub model: &'a str,
    pub messages: &'a [ChatRequestMessage],
    pub tools: &'a [ChatToolDefinition],
    pub thinking_effort: ThinkingEffort,
    pub on_retry: &'a mut Option<RetryCallback>,
    pub cancel_rx: Option<&'a mpsc::Receiver<()>>,
}

pub trait ProviderClient: Debug + Send + Sync {
    fn provider_slug(&self) -> &'static str;
    fn chat_completion_turn(&self, params: ChatTurnRequest<'_>) -> Result<ChatTurnResult, InferenceError>;
    fn chat_completion_turn_streaming(&self, params: ChatTurnRequest<'_>, on_chunk: &mut dyn FnMut(...)) -> Result<ChatTurnResult, InferenceError>;
    fn list_models(&self) -> Result<Vec<String>, InferenceError>;
}
```

`ChatTurnRequest` consolidates the six per-turn parameters into a single struct
to eliminate repetitive argument passing across all provider implementations.
Uses `&mut dyn FnMut` for the streaming callback to keep the trait object-safe.
Each client implementation maps `ThinkingEffort` to its wire format:
- **OpenAI**: `reasoning_effort` field (`"low"`, `"medium"`, `"high"`)
- **Anthropic**: `thinking` block with `budget_tokens` (clamped to `max_tokens - 1024`)
- **Google**: `thinkingConfig` with `includeThoughts: true`
- **Mistral**: `reasoning_effort` field (`"low"`, `"medium"`, `"high"`)

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

A static `PROVIDER_CATALOG: &[ProviderEntry]` maps each provider slug to:
- `display_name` — human-readable name for UIs
- `protocol` — which wire protocol to use
- `default_base_url` — well-known API endpoint
- `default_model` — sensible default model name
- `reasoning` — `ReasoningSupport` variant declaring which reasoning parameter protocol the provider speaks

`ReasoningSupport` enum: `None`, `ReasoningEffort` (OpenAI-style), `AnthropicThinking` (thinking budget block), `GoogleThinkingConfig` (thinkingConfig field). Model-level gating is handled by `effective_reasoning_support()`, which uses name heuristics since the static catalog cannot enumerate every model variant dynamically.

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
├── reads ClientMessages from socket via tai-proto read_message_sync
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
        └► tool-call loop (max configurable iterations, default 25):
           0.5. re-check context fingerprint, rebuild volatile context if changed
           1. send messages + tools → model
           2. receive response
           3. if tool_call → execute Tool → append subdirectory hints to ToolResult → goto 0.5
           4. else → emit final text, Done
     └► if responses or chat_completions (no tools):
        └► stream chunks via SSE → emit OutputChunk per token → Done
```

### Token Usage Tracking

Token usage flows from providers through the daemon to all clients:

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
      run_agent_loop (tai-daemon/src/requests.rs)
        ├─ embeds per-turn TokenUsage into SessionMessage::AssistantText / AssistantToolUse
        └─ accumulates into SessionState.config.accumulated_usage (TokenUsage)
        │
        ▼
      SessionState (tai-daemon/src/sessions.rs)
        ├─ persisted via SessionRecord.accumulated_usage (through SessionConfig)
       ├─ sent to subscribers via DaemonMessage::SessionState.token_usage
       ├─ sent to clients via DaemonMessage::Done.token_usage
       └─ included in SessionSummary.token_usage (listing / get-session)
       │
       ▼
     Clients (tai-tui, tai-dioxus, tai-im)
       └─ tai-tui: displays in session detail view (render.rs:render_session_detail_view)
```

**Key type** — `TokenUsage` (tai-proto/src/types.rs):
```rust
pub struct TokenUsage {
    pub input_tokens: u32,
    pub output_tokens: u32,
    pub total_tokens: u32,
}
```

All new fields use `#[serde(default)]` so old persisted sessions remain compatible (deserialize to zero usage).

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


### `tai-im` — IM platform bridge

Entry point: `src/main.rs`

Single binary (`tai-im`) that bridges IM platforms to the daemon.
The binary accepts a platform subcommand: `tai-im telegram`.

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
`type Args` (must implement `DeserializeOwned`) and `type Return` (must implement `Serialize`).

```rust
pub trait Tool: Send + Sync {
    type Args: DeserializeOwned + 'static;
    type Return: Serialize + 'static;

    fn name(&self) -> &'static str;
    fn group(&self) -> &'static str { "core" }
    fn description(&self) -> &'static str;
    fn schema(&self) -> serde_json::Value;

    fn execute(
        &self,
        args: Self::Args,
        x_credentials: Option<&ServiceCredential>,
        cwd: Option<&Path>,
        ctx: Option<&ToolContext>,
    ) -> Result<Self::Return, ToolError>;

    fn execute_streaming(
        &self,
        args: Self::Args,
        x_credentials: Option<&ServiceCredential>,
        cwd: Option<&Path>,
        output_tx: mpsc::Sender<Vec<u8>>,
        ctx: Option<&ToolContext>,
    ) -> Result<Self::Return, ToolError> {
        let ret = self.execute(args, x_credentials, cwd, ctx)?;
        let bytes = postcard::to_allocvec(&ret).map_err(ToolError::Postcard)?;
        let _ = output_tx.send(bytes);
        Ok(ret)
    }

    fn extract_image(&self, _ret: &Self::Return) -> Option<PreparedImage> { None }
}

Tool implementations can declare which parameters they need via flags on the
`define_tool!` invocation (see below). The `ctx` parameter provides `ToolContext`
with session ID, database handle, and a daemon command channel — used by admin
tools (`list_sessions`, `get_session`).
```

Tools that produce images (e.g. `display_image`) override `extract_image()` to return a
`PreparedImage` from the typed return value. The conversion layer (see `ToolDyn` below)
sends the image through an out-of-band `image_tx: Option<mpsc::Sender<PreparedImage>>`
channel rather than embedding it in the response struct. The agent loop drains this
channel after execution to persist and broadcast the image.

### `ToolDyn` — type-erased dispatch trait

The `ToolDyn` trait erases the generic parameters so tools can be stored in a `HashMap`:

```rust
pub trait ToolDyn: Send + Sync {
    fn name(&self) -> &'static str;
    fn group(&self) -> &'static str;
    fn description(&self) -> &'static str;
    fn schema(&self) -> serde_json::Value;

    fn execute_json(&self, args_json: &str, ..., image_tx: Option<mpsc::Sender<PreparedImage>>) -> ToolExecutionOutput;
    fn execute_binary(&self, args_bytes: &[u8], ...) -> Vec<u8>;
    fn execute_streaming_json(&self, args_json: &str, ..., image_tx: Option<mpsc::Sender<PreparedImage>>) -> ToolExecutionOutput;
    fn execute_streaming_binary(&self, args_bytes: &[u8], ...) -> Vec<u8>;
}
```

A blanket impl `impl<T: Tool> ToolDyn for T` provides all four dispatch paths:

| Path | Input | Output | Used by |
|---|---|---|---|
| `execute_json` | `&str` (JSON) | `ToolExecutionOutput` | LLM tool calls (OpenAI/Anthropic etc.) |
| `execute_binary` | `&[u8]` (postcard) | `Vec<u8>` (postcard) | RISC-V VM tool calls |
| `execute_streaming_json` | `&str` (JSON) | `ToolExecutionOutput` | Streaming shell/VM tools via LLM |
| `execute_streaming_binary` | `&[u8]` (postcard) | `Vec<u8>` (postcard) | Streaming VM tool calls |

The JSON path deserializes arguments with `serde_json`, calls `Tool::execute()`, then
serializes the return value back to JSON. The binary path uses `postcard` for both
deserialization and serialization, enabling compact cross-VM communication.

### `define_tool!` macro

The macro reduces boilerplate for tools that don't need session context. Four variants
control which parameters the `execute()` implementation receives:

**Default (no flags):** The implementation function receives `(&Args, Option<&Path>)`:
```rust
define_tool!(MyTool, "my_tool", "Description...", MyToolArgs, String,
    execute_my_tool, schema!(), "core");
```

**`use_context`:** The implementation receives `(&Args, Option<&Path>, Option<&ToolContext>)`:
```rust
define_tool!(MyTool, ..., "core", use_context);
```

**`use_credentials`:** The implementation receives `(&Args, Option<&ServiceCredential>, Option<&Path>)`:
```rust
define_tool!(MyTool, ..., "core", use_credentials);
```

**`use_credentials, use_context`:** The implementation receives all four parameters:
```rust
define_tool!(MyTool, ..., "core", use_credentials, use_context);
```

The macro implements `Tool` for the struct, wiring `execute()` to call the implementation
function with the deserialized args and automatically ignoring unused parameters via `_`.

### Registry

Tools are registered in a `ToolRegistry` stored as `Box<dyn ToolDyn>`. The registry is
owned by `DaemonStateInner`, constructed once at daemon startup. The agent loop extracts
an `Arc<ToolRegistry>` from the daemon state to list available tool definitions and
dispatch tool execution.

The registry provides `execute_dyn()` for the binary dispatch path (used by the VM):

```rust
pub fn execute_dyn(&self, name: &str, args_bytes: &[u8], ...) -> Vec<u8> {
    // finds the tool by name, calls ToolDyn::execute_binary()
}
```

Each tool receives an optional `cwd: Option<&Path>` parameter that represents the session's
working directory. Filesystem and Git tools resolve relative paths against this CWD.

### Postcard binary encoding

Tools communicate with the RISC-V sandbox via a `postcard`-encoded binary protocol:

- **Arguments:** Encoded as `postcard::to_allocvec(&args)` where `args: Self::Args`
- **Return value:** Encoded as `postcard::to_allocvec(&Result::<Self::Return, String>::Ok(ret))`
  — a postcard `Result` where `Ok(bytes)` carries the serialized return value and
  `Err(String)` carries the error message
- **Tool call frame (VM → host):** `[tool_name: postcard String][args: postcard-encoded Args]`

### Available tools (up to 35 total, some dependent on installed binaries)

| Group | Tools |
|---|---|
| **Core** | `list_sessions`, `get_session`, `load_skill`, `read_file`, `read_file_range`, `write_file`, `edit_file`, `list_files`, `line_count`, `random` (integers, floats, booleans, bytes, UUID v4 — with optional seed), `get_current_time` (Unix millisecond timestamp) |
| **HTTP** | `http_request` (GET/POST/HEAD with headers, body, timeout) |
| **Image** | `display_image` (from path, URL, base64, or SVG text) |
| **Git** | `git_status`, `git_diff`, `git_log`, `git_add`, `git_commit`, `git_push` |
| **EVM** | `evm_chain`, `evm_balance`, `evm_token_balance`, `evm_block`, `evm_transaction`, `evm_call`, `evm_gas`, `evm_logs`, `evm_nonce`, `evm_resolve` |
| **File search** | `fff` (file finding) |
| **RISC-V VM** | `run_riscv` (compile & run Rust code in a sandboxed RISC-V VM with access to all registered tools) |
| **Shell** | `exec` (direct program execution), `sh` (bash/dash/zsh — detected at startup), `nushell` (if `nu` is installed), `fish` (if `fish` is installed) |
| **X/Twitter** | `x_post`, `x_search_recent`, `x_user_lookup` |
| **DB** | `db_set`, `db_get`, `db_delete`, `db_delete_range`, `db_get_range`, `db_list`, `db_count` |
| **Sub-session** | `spawn_subsession` (spawns an autonomous child session with its own tool-calling loop) |

### Tool groups

Tools are organized into groups to reduce context overhead. Each tool declares its group
via `fn group() -> &'static str` on the `Tool` trait. Groups are:

| Group | Default | Description |
|---|---|---|
| `core` | always on | File system, HTTP, images, file search, random values, and time queries |
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
- `ToolRegistry::available_definitions(active)` returns definitions for registry tools in the
  active set, plus always-available meta-tools (`load_tools`, `unload_tools`, `spawn_subsession`)
  that require mutable session state or deep coupling with the agent loop
- `load_tools`/`unload_tools` are intercepted in `execute_tool_with_timeout()` (same pattern as
  `spawn_subsession`) — they are not in the registry because they modify `session.config.active_tool_groups`
- `list_sessions`, `get_session`, and `load_skill` were formerly intercepted like `spawn_subsession`
  but are now proper `Tool` trait implementations registered in the default registry via `ToolRegistry::build()`,
  using `ToolContext.daemon_tx` to communicate with the daemon command loop
- Session state stores `active_tool_groups: HashSet<String>` (default: `{core, git, shell}`)
- `ToolGroup` struct and `GROUPS` constant live in `tai-daemon/src/tools/mod.rs`
- Handler functions live in `tai-daemon/src/tools/groups.rs`
- Group metadata is appended to the system prompt in `context::build_base_prompt()`

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
`~/.local/share/tai-daemon/state.redb`. Five tables:

| Table | Key | Value |
|---|---|---|
| `sessions` | `u64` session ID | postcard(`SessionRecord`) |
| `session_messages` | `(u64, u32)` (session ID, index) | postcard(`SessionMessage`) |
| `credentials` | `&str` service name | encrypted blob |
| `session_kv` | `(u64, String)` (session ID, key) | `Vec<u8>` |
| `meta` | `&str` | `u64` counter |

`SessionRecord` fields: `title`, `selected_model`, `parent_session_id`, `cwd`,
`message_count`, `created_at`, `context_config`, `account_name`.

### Session state (in-memory)

Each active session has a `SessionState` owned by its control thread. Persistent
configuration fields are extracted into `SessionConfig` to avoid duplication
across snapshot/restore, metadata conversion, and record persistence:

**`SessionConfig` (persisted):**

- `title: Option<String>` — display name
- `selected_model: Option<String>` — AI model for this session
- `reasoning_effort: Option<ThinkingEffort>` — per-session reasoning effort
- `parent_session_id: Option<u64>` — parent session for sub-sessions
- `cwd: Option<PathBuf>` — working directory for filesystem tools
- `max_turns: Option<u32>` — per-session tool loop iteration cap (inherits from parent)
- `created_at: i64` — Unix timestamp of creation
- `context_fingerprint: Option<u64>` — fingerprint for context file refresh
- `context_file_paths: Vec<PathBuf>` — tracked context file paths
- `context_message_index: Option<usize>` — index of the context system message
- `status: SessionStatus` — current status (Inactive, Inference, Retrying, Sleeping, …)
- `active_tool_groups: HashSet<String>` — tool groups active for this session
- `context_config: ContextConfig` — file discovery settings (context file names, max bytes)
- `account_name: Option<String>` — inference account assigned to this session
- `accumulated_usage: TokenUsage` — session-level token counter

**Runtime fields (not persisted directly):**

- `messages: Vec<SessionMessage>` — conversation history (persisted to DB separately)
- `subscribers: HashMap<u64, mpsc::Sender<DaemonMessage>>` — attached clients
- `active_requests: HashMap<u32, ActiveRequest>` — running request cancel flags
- `provider: Option<InferenceProvider>` — resolved inference provider for the account

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
- **Message append**: Each `SessionMessage` (including `DisplayedImage` records for
  persisted images) is written to the DB alongside the in-memory push via
  `append_message_and_persist()`.
- **Shutdown**: The daemon sends `SessionCommand::Shutdown` to each active session, waits for request workers to drain, then exits cleanly.

### Multiple concurrent sessions

Multiple sessions can be active at the same time. Each session control thread stays
responsive while at most one request worker runs for that session. Request workers own a
snapshot of the session state and use cooperative cancellation via an `AtomicBool`.

---


**Service config:** `~/.config/tai-daemon/config.toml`

```toml
max_turns = 25                                   # default tool loop iteration cap

[context]
context_file_names = ["AGENTS.md", "CLAUDE.md"]
context_file_max_bytes = 32768
```

> **Note:** Provider-level settings (`base_url`, `streaming`, `retry_*`, timeouts, endpoint paths, request format) have moved to per-account overrides in `accounts.toml`. See `README.md` for the full list.

**Credential storage:** Credentials are encrypted per-credential in the `redb` database (`state.redb`). Identity keys reside in `~/.config/tai-daemon/identity.pk` (private), `~/.config/tai-daemon/public.pk` (public), and optionally `~/.config/tai-daemon/identity.pk.enc` (passphrase-encrypted private key).

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

## Metrics / OpenMetrics monitoring

The daemon can expose a `/metrics` HTTP endpoint in the OpenMetrics format
(suitable for Prometheus scraping).

**CLI flag:** `--metrics-addr <ADDR>` (e.g. `127.0.0.1:9464`). When the flag is
absent no metrics server is started — the daemon runs exactly as before.

**Endpoint:** `GET /metrics` returns `Content-Type: text/plain; version=0.0.4; charset=utf-8`.

### Exposed metrics

| Metric | Type | Labels | Description |
|---|---|---|---|
| `tai_sessions_active` | Gauge | — | Number of active sessions |
| `tai_connections_active` | Gauge | — | Number of active client connections |
| `tai_requests_total` | Counter | `status` (`done`, `failed`, `cancelled`) | Total requests processed |
| `tai_tool_executions_total` | Counter | `tool`, `status` (`ok`, `error`) | Tool call count |
| `tai_api_calls_total` | Counter | `model`, `endpoint` | API call count |
| `tai_api_errors_total` | Counter | `model`, `error_type` | API error breakdown |
| `tai_connections_total` | Counter | — | Total connections accepted |
| `tai_turns_total` | Counter | `model` | Agent loop turns |
| `tai_request_duration_seconds` | Histogram | `status` | Request latency |
| `tai_tool_execution_duration_seconds` | Histogram | `tool` | Per-tool execution time |
| `tai_api_call_duration_seconds` | Histogram | `model`, `endpoint` | API round-trip time |

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
| `daemon.rs` — `CreateSession` handler | `record_session_created` | `tai_sessions_active +1` |
| `daemon.rs` — `SessionExited` handler | `record_session_exited` | `tai_sessions_active -1` |
| `server/connection.rs` — `client_thread` start | `record_client_connected` | `tai_connections_active +1` |
| `server/connection.rs` — `client_thread` end | `record_client_disconnected` | `tai_connections_active -1` |
| `server/lifecycle.rs` — accept loop | `record_connection_accepted` | `tai_connections_total +1` |
| `sessions.rs` — `run_request_worker` | `record_request_total`, `record_request_duration` | request status + latency |
| `requests.rs` — `run_agent_loop` turn | `record_turn` | turn count per model |
| `requests.rs` — `execute_tool_with_timeout` | `record_tool_execution` | tool duration + status |
| `providers/shared.rs` — `timed_result` | `record_api_call`, `record_api_error` | API latency + errors (all providers) |

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

Images are delivered out-of-band via a one-shot `mpsc` channel rather than embedded in
`ToolExecutionOutput`:

```
Model calls display_image tool
  → daemon creates (image_tx, image_rx) channel
  → passes image_tx to ToolDyn::execute_json → execute_streaming_json
  → tool extracts PreparedImage from typed return → sends via image_tx
  → agent loop drains image_rx after tool completion
  ├── Persistence path:
  │     → SessionMessage::DisplayedImage(DisplayedImageRecord)
  │     → push to session messages + write to DB via write_message_retry
  │     → replayed on session re-attach via DaemonMessage::SessionState
  └── Live broadcast path:
        → DaemonMessage::ImageStart { request_id, metadata }
        → DaemonMessage::ImageChunk { request_id, data (≤64 KiB) } × N
        → DaemonMessage::ImageEnd { request_id }
        → client ImageAssembler reconstructs full image
        → client renders image in terminal (ratatui-image) or desktop (data: URL)
```

### Session switch flow (updated)

```
User presses Enter on a session in the session manager
  → history cleared (client.history, render_cache, scroll, in_progress, pending_images)
  → AttachSession sent to daemon
  → daemon responds with SessionState { messages: Vec<SessionMessage>, … }
    where messages may include SessionMessage::DisplayedImage for persisted images
  → for each message:
    DisplayedImage → build_rendered_image(picker, metadata, data) → HistoryItem::Image
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
   session has its own model, CWD, and messages. Clients subscribe/unsubscribe from sessions
   via the broadcast system. Sessions persist in a redb database and survive daemon restarts.
   Session lifecycle operations (create, attach, list, delete) succeed even when the daemon
   is locked — credentials are only needed to run models. Sessions carry an optional
   `account_name` field that determines which provider credential to use at request time;
   there is no global "default account" fallback. The provider is resolved lazily from the
   daemon's provider registry when the first RunInput is issued.

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

9. **SSE streaming** — a custom `SseReader` (not a library) handles `data:` lines and `[DONE]`
   for OpenAI SSE, giving full control over parsing and buffering behavior. The Anthropic
   module has its own `AnthropicSseReader` that handles both `event:` and `data:` lines
   (required by the Anthropic Messages streaming format) and yields `(event_type, data)` pairs.

10. **Markdown as the intermediate format** — all text (tool output, assistant text, error
    messages) is treated as markdown and rendered as HTML (desktop) or shaped to terminal output
    (tai-tui), providing a consistent rendering layer.

11. **Flexible API format** — the daemon supports both OpenAI Chat Completions and Responses
    endpoints, selectable per-model. This lets users route different models to their best-supported
    endpoint.

12. **Pluggable providers via `InferenceProvider`** — the provider system supports OpenAI-compatible,
    Anthropic Messages, Google Gemini, and Mistral APIs. Each provider implements the same interface
    (`chat_completion_turn`, `chat_completion_turn_streaming`, `list_models`) and is constructed
    from an `AccountConfig` + credential. Accounts are defined in `accounts.toml` with a
    `provider` field (`"openai"`, `"opencode"`, `"anthropic"`, etc.). The TUI new-account form
    offers all registered provider options via a shared `PROVIDER_OPTIONS` array.

13. **OS threads with sidecar async runtime** — the daemon avoids async Rust everywhere except
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
2. If `source` is provided, it is first formatted via `rustfmt` (silently skipped
   if `rustfmt` is unavailable).  The formatted source is then prepended with a
   `#![no_std]` boilerplate (panic handler, entry point, `tai` module with
   `tool_call`, `write`, `exit` syscall wrappers, optional 128 KB bump allocator
   enabled via the `allocator` parameter) and compiled via a single
   `rustc +nightly --target riscv64imac-unknown-none-elf` invocation in a temp
   directory.
3. Creates a `DefaultCoreMachine<u64, FlatMemory<u64>>` with 4 MB of flat memory.
4. Registers a `TaiSyscall` handler that intercepts three guest syscalls:
   - **Syscall #0 (TOOL_CALL)** — reads a postcard-encoded frame `[tool_name: String][args: bytes]`
     from guest memory, dispatches it via the `ToolRegistry::execute_dyn()`, and writes the
     postcard-encoded `Result<Return, String>` result to the guest's output buffer.
   - **Syscall #1 (WRITE)** — copies guest data into an accumulator buffer that becomes the tool's
     output upon VM exit.
   - **Syscall #2 (EXIT)** — stops the VM.
5. Loads the ELF via `TraceMachine::load_program` and runs via `TraceMachine::run()`.
6. After execution, the machine is dropped and the output channel is drained with
   a blocking `recv()` loop (deterministic — no buffered-item race).
7. Returns the formatted source wrapped in a `rust` markdown fenced code block,
   followed by the accumulated WRITE output, then a `[VM: exited with code N in M cycles]`
   summary line.  The TUI renders the code block with syntect syntax highlighting.

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
| Daemon Anthropic | Content block deserialisation, response→turn result conversion, message payload building, config overrides | `tai-daemon/src/anthropic/tests.rs` |
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

# Run IM bridge (Telegram)
cargo run -p tai-im -- telegram
```


---

## External dependencies (key crates)

| Crate | Used by | Purpose |
|---|---|---|
| `tokio` | tui, dioxus, im | Async runtime |
| `serde` + `postcard` | proto, clients, daemon | Wire protocol framing and internal storage |
| `snow` | daemon, client-core, transport | Noise IK handshake and transport encryption |
| `reqwest` (rustls) | daemon | HTTP client |
| `pulldown-cmark` + `ammonia` | client-core | Markdown parsing, HTML sanitization |
| `ratatui` + `crossterm` | tai-tui | Terminal UI |
| `dioxus` | tai-dioxus | Desktop UI |
| `image` + `resvg` | daemon, tai-tui | Image decoding, SVG rasterization |
| `syntect` | tai-tui | Syntax highlighting for code blocks (uses Sublime Text grammar files) |
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
| `tai-proto` | `ProtoError` | `Postcard`, `FrameTooLarge`, `TrailingBytes`, `UnsupportedVersion`, `Io` |
| `tai-keystore` | `KeystoreError` | `Io`, `TooShort`, `DecryptionFailed`, `InvalidKeyLength`, `EncryptionFailed`, `ConfigDirNotFound` |
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
errors, network failures, postcard encoding errors, etc. Tools return `Result<Self::Return, ToolError>`
from their `execute()` method. The `ToolDyn::execute_json()` conversion layer transforms errors
into `ToolExecutionOutput { is_error: true }` for the LLM path, and the binary path encodes
them as `Result::<Return, String>::Err(e.to_string())` via postcard.

The legacy `ToolResult { content, is_error }` and `ToolExecutionOutput` types remain for
backward compatibility in the VM's internal `run_riscv_impl` and the `ToolDyn` conversion layer.
| `gix` | daemon | Git operations |
| `teloxide` | tai-im | Telegram Bot API client |
| `prometheus` | daemon | OpenMetrics instrumentation, process metrics |
| `tiny_http` | daemon | Metrics HTTP server for `/metrics` endpoint |
| `tracing` | daemon | Structured logging |
