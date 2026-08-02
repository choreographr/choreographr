# Choreographr

<p align="center">
  <a href="LICENSE"><img src="https://img.shields.io/badge/License-Apache_2.0-blue" alt="Apache 2.0"></a>
  <a href="https://www.rust-lang.org/"><img src="https://img.shields.io/badge/Rust-2024_edition-orange" alt="Rust 2024"></a>
  <img src="https://img.shields.io/badge/providers-30+-brightgreen" alt="30+ providers">
  <img src="https://img.shields.io/badge/wire_protocols-3-lightgrey" alt="3 wire protocols">
</p>

## What is Choreographr?

Choreographr is a local-first AI agent daemon written entirely in Rust — a server-side engine that runs agent sessions, with 30+ LLM providers across three wire protocols, real tools, encrypted credentials, and a sandboxed RISC-V VM, fronted by clients that can connect and disconnect at any time.

### General Purpose

Choreographr is a general purpose agent. It can be used for software development, a personal / business agent, or as a research tool. It can run on your desktop or in the cloud.

### Client / Server Architecture

Choreographr was designed from the beginning to have a separation of concerns between the server software that actually runs the sessions and the clients that can connect and disconnect any time.

The client can either run on the same computer as the server agent (via local socket), or the server can be anywhere else on your local network or the Internet. When not connecting locally the client connects to the server via [Noise-IK](https://en.wikipedia.org/wiki/Noise_Protocol_Framework) encrypted TCP connection. Because the server can live anywhere and is reachable over encrypted TCP, it can also be accessed from mobile devices — for example, chatting with your agent on the go via the `choreo-im` Telegram bridge.

Client/server communication is currently encoded via [Postcard](https://postcard.jamesmunns.com/), but this will probably be changed in future to a self-describing binary format with broader language support.

Currently the primary client is **`choreo-tui`** - a fullscreen terminal UI.

Other clients being developed: 

- **`choreo-gui`** — GUI built with [Dioxus](https://dioxuslabs.com/) - just a placeholder for now, it will support Linux, macOS, Windows, Android and iOS.
- **`choreo-im`** — instant-messaging bridge (Telegram, more platforms coming) - chat with your agent on the go!
- **`choreo-acp`** — ACP bridge so ACP-compatible editors (Claude Code, Cline, …) can drive Choreographr sessions over JSON-RPC.
- **`choreo-daemon`** — Choreographr servers will be able to connect to other servers to deploy work elsewhere.

### RISC-V Virtual Machine

The LLM can invoke the RISC-V VM (powered by [CKB VM](https://github.com/nervosnetwork/ckb-vm)) by either providing a Rust snippet, or pre-compiled bytecode. Other languages will be supported in future.

The VM has full access to the agent's tool calls. The LLM can quickly write a Rust script for a complex task and hand it off to the VM for high-performance execution.

The sandbox VM is single-hart with the RISC-V A (atomic) extension disabled, so guest code must not use `core::sync::atomic` read-modify-write operations (they fail at compile time).


### Multiple live sessions

Each server can run multiple sessions simultaneously (only limited by system resources). Sessions are stored in the database and only "woken-up" when a client connects to them.

Rather than having a multi-session terminal multiplexor, you can manage all your sessions directly from a client program.

Sessions have undo/redo functionality. If an LLM is mis-prompted it is often better to remove the prompt than to prompt more to try to "fix it".

### Hierarchical Sessions

Many agents support the concept of "subagents". Choreographr has "subsessions". This enables work to be broken up into manageable chunks and potentially worked on in parallel.

In Choreographr, the LLM or VM can start new sessions that will report back once they are finished. Subsessions are real sessions that can be interacted with like any other session. The user can pause them and provide additional prompting. Subsessions can invoke their own subsessions as necessary.

### Session database

Each session has a persistent key/value store. The LLM / VM can store data and retrieve it at a later time.

### High performance Multithreaded Architecture

Currently the codebase doesn't use any async code - this reduces the complexity of the codebase significantly. It uses real kernel threads with event loops and message passing. Mutable state is not shared between threads (except for the message passing). Everything is event driven without polling.

Extensions may require tokio to use certain crates.

choreo-tui is entirely event driven, and runs in immediate mode. The terminal is updated immediately upon receiving a keystroke or networking event. There is no maximum framerate. Additionally, it has O(1) scrolling and O(1) streaming. It is ultra-smooth!

### Encrypted Keystore

Credentials are encrypted per-credential with ECDH (X25519) + HKDF +
AES-256-GCM before being stored in the `redb` database, so only the holder
of the daemon's private key can decrypt them. Identity keys live in
`~/.config/choreographr/` — `identity.pk` (private), `public.pk` (public),
and optionally `identity.pk.enc` (passphrase-encrypted). The daemon starts
locked and only decrypts credentials into memory after `/unlock`.

### Maximum model compatibility

Currently Choreographr supports the following model APIs:

- **OpenAI**
    1. Chat Completions - used by almost all model providers
    2. Responses - including [programmatic tool calling](https://developers.openai.com/api/docs/guides/tools-programmatic-tool-calling) (gpt-5.6+ models)
- **Anthropic**
- **Gemini**

Other major APIs will be supported in future:

- AWS Bedrock Runtime
- Google Vertex AI
- Azure OpenAI (classic)
- AWS SageMaker
- gRPC-based inference servers (Triton, ONNX Runtime, TensorRT-LLM)
- Cohere native API
- AI21 native API
- Ollama native /api/chat

## Future Functionality

### Extensions

Extensions communicate with the choreographr server via a local socket. They will be able to hook into the operation of the server, for example to expose new tool calls. Similar to MCP (also supported). There will be blockchain extensions that enable reading and writing to EVM / Solana / Polkadot blockchains.


### Stored VM programs

Once the tool call ABI has stabilized, it will be possible for compiled Rust programs to be stored and executed when necessary.

### Cron

Programs will be able to run automatically at designated times.


### Sandboxing

With the agent able to run anything via the shell, there needs to be a robust sandboxing solution.

On Linux, [Landlock](https://landlock.io/) will be used. On macOS, [Seatbelt](https://theapplewiki.com/wiki/Dev:Seatbelt).
Windows does not have a good solution for this yet.


### Advanced Context Management

Currently, as with most AI agents, if the LLM wants to see a file it issues the `read_file` tool. This adds it permanently into the session context. A better solution is to enable the LLM to add and remove files from the context at any time, enabling it manage context bloat itself.

### Git Worktree Support

To get the most out of subsessions, they need to run in parallel on the same codebase. The problem is that they will interfere with each other's work. The solution is for each subsession to work on its own branch in its own directory. This is where [Git Worktrees](https://git-scm.com/docs/git-worktree) come in. Once a subsession has finished committing in its own branch, the parent session can merge it into its own branch. Any merge conflicts can be resolved by the LLM.

The problem with worktrees is that programming languages such as Rust can have many gigabytes of build artifacts. If each worktree has to regenerate these it consumes CPU bandwidth, I/O bandwidth, storage space and is generally very slow. Copying the artifacts from the parent's tree reduces the CPU bandwidth, but is still a big problem.

The solution is to use CoW filesystems such as [BTRFS](https://en.wikipedia.org/wiki/Btrfs) so the file is only copied if it is re-generated by the subsession.

### Looping

A common scenario in agentic coding is to manually "loop" over the codebase changes until a certain goal is met. For example, after a new feature has been implemented a new session can be prompted to check the changes for bugs, potential refactorings, optimizations, security issues. The LLM will then make some recommendations. It will then be prompted to implement these. Once this is complete a new session is created to do it again. This process repeats until the LLM says it is ready, or only complains about very minor issues.

Choreographr will have an option to automate this process, so it can be left alone to complete the whole process without interaction.

## Comparison to other agents

| Feature | Choreo | zero | goose | pi | opencode | hermes | turnstone | openclaw | buzz | OpenMinis | langgraph | tau | mercury | openwork | t3code | herdr |
|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|
| Daemon + multi-client | ✅ | — | server | — | — | — | server | ✅ | ✅ | — | — | — | — | ✅ | ✅ | — |
| Providers | ~30/3 proto | 36 | 35+ | 38/9 proto | 15 | 29+ | 5 | 40+ | agnostic | 8 | agnostic | 9 | 11 | agnostic | 5 | n/a |
| OAuth | — | ✅ | — | ✅ | ✅ | ✅ 6× | ✅ MCP | ✅ | — | ✅ | — | ✅ | ✅ | ✅ | — | — |
| Credential rotation/fallback | retry only | — | — | — | — | ✅ pool | ✅ | ✅ failover | — | ✅ fallback | — | — | ✅ | — | — | — |
| Tool permission gating | — | ✅ | ✅ | ✅ | ✅ | env-only | ✅ judge | ✅ | — | ✅ | — | — | — | — | — | — |
| Compaction | — | ✅ | ✅ | ✅ | ✅ | ✅ | ✅ | ✅ | ✅ | ✅ | — | — | — | — | — | — |
| Sandbox | RISC-V VM | seccomp eng. | — | — | — | Docker/SSH | OpenShell | Docker/SSH | — | iSH/PRoot | — | — | — | worker | — | — |
| Subagents | subsessions | specialists | — | — | — | delegation | workstreams | swarm | agent pool | — | subgraphs | — | — | — | multi-agent | — |
| Skills (SKILL.md) | ✅ | ✅ | — | ✅ | ✅ | ✅ | ✅ | ✅ | — | ✅ | — | — | — | ✅ | — | — |
| MCP client | ✅ | ✅ | ✅ | — | ✅ | — | ✅ (OAuth) | ✅ | ✅ | ✅ | — | — | — | ✅ meta | — | — |
| ACP | bridge | — | server | — | — | — | — | bridge | harness | — | — | — | — | — | — | — |
| IM surfaces | Telegram | — | — | — | — | 20+ | 2 | 25+ | chat natively | — | — | — | — | — | — | — |
| Web search | — | ✅ | — | — | ✅ | ✅ | ✅ | ✅ | — | — | — | — | — | — | — | — |
| Hooks/lifecycle | — | ✅ | ✅ | ✅ | ✅ | ✅ | — | ✅ | ✅ | — | — | — | — | — | — | — |
| Plugins | — | ✅ | — | ✅ | ✅ | ✅ | — | ✅ | — | — | — | — | — | ✅ | — | — |
| Encrypted creds | ✅ unique | ✅ | ✅ keyring | — | — | — | ✅ Fernet | ✅ | NIP auth | ✅ keychain | — | 0600 | — | ✅ | ✅ | — |
| Storage | redb | fs JSONL | SQLite | JSONL | event src | SQLite | SQL/Postgres | SQLite | Postgres | SQLite | SQLite/Postgres | — | — | fs+MySQL | — | — |
| Metrics | ✅ | — | telemetry | — | — | — | ✅ | ✅ OTel | — | — | — | — | — | — | — | — |
| Undo/redo | ✅ | rewind | — | branch | — | — | replay | — | — | fork | time-travel | — | — | — | — | — |
| Context fingerprints | ✅ | partial | — | — | — | — | ✅ | — | — | — | — | — | — | — | — | — |


## Quick start

Requires a [Rust toolchain](https://rustup.rs/) — minimum supported Rust version (MSRV) is **1.91**.

```bash
rustup toolchain install 1.91.0
cargo build --release
```

Start the daemon:

```bash
cargo run --release -p choreo-daemon         # default log level: info
cargo run --release -p choreo-daemon -- -v   # debug
cargo run --release -p choreo-daemon -- -vv  # trace
cargo run --release -p choreo-daemon -- -q   # warnings only
```

`RUST_LOG` takes precedence over the CLI flags:

```bash
RUST_LOG=debug cargo run --release -p choreo-daemon
```

Then a client:

```bash
cargo run --release -p choreo-tui     # terminal UI
cargo run --release -p choreo-gui     # desktop app
cargo run --release -p choreo-im      # IM bridge
cargo run --release -p choreo-acp     # ACP bridge for editors
```

### First conversation

1. **Configure an account** in `~/.config/choreographr/accounts.toml` (see
   [Configuration](#configuration)) and add an API key with `/add-key <service> <api_key>`.
2. **Unlock the daemon** with `/unlock` (reads `identity.pk`, or decrypts
   `identity.pk.enc` with a passphrase) so it can decrypt your credentials.
3. Select the account with `/account <name>` and start prompting.

```
┌──────────────┐   Unix socket /     ┌──────────────┐   HTTP/SSE     ┌────────────────────┐
│  choreo-tui  │◄───────────────────►│              │◄──────────────►│  OpenAI-compatible │
│  (terminal)  │                     │              │                ├────────────────────┤
├──────────────┤                     │              │◄──────────────►│  Anthropic Messages│
│  choreo-gui  │◄───────────────────►│ choreographr │                ├────────────────────┤
│  (desktop)   │   Noise-IK TCP      │  (daemon)    │◄──────────────►│  Google Gemini     │
├──────────────┤                     │              │                └────────────────────┘
│  choreo-im   │◄───────────────────►│              │
│  (IM bridge) │                     │              │
├──────────────┤                     │              │
│ choreo-acp   │◄───────────────────►│              │   MCP subprocess servers
│  (ACP bridge)│                     └──────────────┘   RISC-V VM sandbox
└──────────────┘                                        redb database
```

Mistral and the other OpenAI-compatible providers (Ollama, Groq, DeepSeek,
OpenRouter, …) speak the OpenAI wire protocol; Anthropic Messages and Google
Gemini each have their own format.

## Crates

A Rust workspace of eleven crates (resolver = "3"):

See [ARCHITECTURE.md](ARCHITECTURE.md) for a deep dive into the daemon's
internals — threading model, provider architecture, tool system, and session
data model.

| Crate | Description |
|---|---|
| `choreo-daemon` | The core engine — binary `choreographr`. Unix socket server that validates credentials, manages persistent sessions (with sub-sessions and working directories), runs requests with a tool-call loop, and streams responses |
| `choreo-proto` | Framed binary protocol (postcard + length prefix) shared between clients and daemon |
| `choreo-keystore` | X25519 keypair + ECDH/AES-256-GCM crypto library for encrypted credentials |
| `choreo-transport` | Noise-IK encrypted transport over TCP |
| `choreo-mcp` | MCP (Model Context Protocol) client — spawns subprocess servers, discovers tools, dispatches calls over JSON-RPC stdio |
| `choreo-acp` | ACP (Agent Communication Protocol) bridge — translates JSON-RPC 2.0 over stdin/stdout into `choreo-proto` messages so ACP-compatible editors can drive sessions |
| `choreo-tui` | Full-screen terminal UI client (ratatui + crossterm) |
| `choreo-gui` | Desktop GUI client (Dioxus) |
| `choreo-im` | Instant messaging bridge (Telegram) |
| `choreo-client-core` | Shared parsing, markdown, image assembly, and daemon-message dispatch for UI clients |
| `choreo-markdown` | Markdown parser and HTML renderer (pulldown-cmark + ammonia) |

## Concepts

**Agent loop (harness).** The daemon drives a server-side loop that repeatedly
sends conversation history and available tools to the LLM, executes any tool
calls the model requests, appends the results, and loops until the model
produces a final answer, is cancelled, or hits an error (subject to the
daemon-wide iteration cap; 0 = unlimited). Each session keeps a responsive control thread and runs request work
in a separate worker thread. The client only sees `ToolCallStarted` /
`ToolCallFinished` lifecycle events, keeping it simple.

**Session / subsession.** A *session* is a persisted conversation with its own
message history, model, and working directory. Sessions form
a parent-child tree, support multiple concurrent client attachments, and survive
daemon restarts via an embedded `redb` database. A *subsession* is a child
session spawned by the `spawn_subsession` tool — it inherits the parent's
working directory, runs its own full agent loop independently, and returns its
output as the parent's tool result. Subsessions persist permanently.

**Tool.** A function the LLM can call to interact with the outside world (read
files, make HTTP requests, run git commands, query blockchains, post to X,
etc.). Tools implement the `Tool` trait (name, group, description, JSON Schema,
`fn execute`) and are registered in a `ToolRegistry` at daemon startup.

**Tool group.** Tools are organized into groups (`core`, `git`, `shell`, `x`,
`vm`, `db`, `mcp`). Only `core`, `git`, and `shell` are active by default. The
model can activate additional groups with `load_tools` and deactivate them with
`unload_tools`. Groups are a discovery mechanism, not access control — the
RISC-V VM always has access to all tools.

**Skill.** A filesystem-based extension following the Agent Skills standard — a
`SKILL.md` file with YAML frontmatter (`name`, `description`) placed under
`.agents/skills/<name>/`. At session creation, skill names and descriptions are
listed in the system prompt. When the model calls `load_skill`, the full
instruction body is injected into the conversation (progressive disclosure).

## Configuration

The daemon reads config from `~/.config/choreographr/config.toml` (all fields
optional):

```toml
max_turns = 0      # daemon-wide tool-loop budget; 0 = unlimited (default)

[context]
context_file_names = ["AGENTS.md", "CLAUDE.md"]
context_file_max_bytes = 32768
disable_claude_code_prompt = false
```

> **Note:** Provider-level settings (`base_url`, `streaming`, `retry_*`,
> timeouts, endpoint paths, request format, etc.) have moved to per-account
> overrides in `accounts.toml`. They are no longer read from `config.toml`.

Credentials are encrypted per-credential with the daemon's X25519 public key
and stored in the `redb` database. Identity keys reside in
`~/.config/choreographr/identity.pk` (private),
`~/.config/choreographr/public.pk` (public), and optionally
`~/.config/choreographr/identity.pk.enc` (passphrase-encrypted private key).

The socket path defaults to `/tmp/Choreographr.sock` (override with
`CHOREOGRAPHR_SOCKET_PATH`). The database path defaults to
`~/.local/share/choreographr/state.redb` (override with `CHOREOGRAPHR_DB_PATH`).

`CHOREOGRAPHR_MAX_TURNS` overrides the `max_turns` setting from `config.toml`
(resolution chain: `CHOREOGRAPHR_MAX_TURNS` → `config.toml` → default 0; `0` =
unlimited — the agent loop runs until the model produces a final answer, is
cancelled, or hits an error). This is a daemon-wide cap; individual sessions no
longer carry their own `max_turns`.

### Accounts

Accounts are configured via `~/.config/choreographr/accounts.toml`. Account
names must be lowercase alphanumeric with hyphens or underscores (`[a-z0-9_-]`).
Each session may have its own account, set via `/account <name>`; there is no
global default account.

```toml
[[account]]
name = "main"
provider = "openai"

[[account]]
name = "claude"
provider = "anthropic"

[[account]]
name = "gemini"
provider = "google"

[[account]]
name = "local"
provider = "ollama"
base_url = "http://localhost:11434/v1"
streaming = false
retry_max_attempts = 3
```

Supported providers: all entries in the provider catalog — 70+ across three
wire protocols (OpenAI-compatible, Anthropic Messages, Google Generative AI).
Each provider has its own data file under
`choreo-daemon/src/providers/catalog/<slug>.toml` (one file per provider,
TOML data, not code) with a curated model list, context windows, reasoning
levels, and the API format each model uses. Highlights: OpenAI, Anthropic,
Google Gemini, Mistral, DeepSeek, xAI Grok, Groq, Together AI, OpenRouter,
Hugging Face, GitHub Models, NVIDIA NIM, Cerebras, Fireworks AI, Alibaba
(Qwen), Moonshot AI (Kimi), Perplexity, Z.ai, Xiaomi MiMo, Qwen Token Plan, Vercel AI
Gateway, OpenCode Zen/Go, GitHub Copilot, Kimi Code, Ollama (local/cloud), LM
Studio, and many regional/niche gateways. See the `catalog/` directory for the
full list. Each provider ships sensible defaults (base URL, default model) —
override any field per-account:

| Field | Description |
|---|---|
| `base_url` | API base URL |
| `streaming` | Enable/disable streaming responses |
| `stream_options` | Include usage in stream |
| `retry_max_attempts` | Max retry count on transient errors |
| `retry_initial_backoff_ms` | Initial backoff between retries (ms) |
| `retry_max_backoff_ms` | Max backoff between retries (ms) |
| `connect_timeout_secs` | TCP connect timeout |
| `request_timeout_secs` | HTTP request timeout |
| `model_list_path` | Custom models list endpoint path |
| `responses_path` | Custom responses endpoint path |
| `chat_completions_path` | Custom chat completions endpoint path |
| `default_request_format` | Request format: `"chat_completions"` or `"responses"` |
| `chat_completions_max_tokens` | Default max tokens for chat completions |
| `model_max_tokens` | Per-model max token caps |
| `chat_completions_max_tokens_field` | Token field: `"max_tokens"` or `"max_completion_tokens"` |
| `model_max_tokens_fields` | Per-model token field overrides |
| `responses_max_output_tokens` | Default max output tokens for Responses API |
| `model_responses_max_output_tokens` | Per-model max output tokens for Responses API |
| `programmatic_tool_calling` | Enable programmatic tool calling (Responses API, gpt-5.6+) |
| `context_window` | Default context window for all models (overrides catalog defaults) |
| `model_context_windows` | Per-model context window overrides (e.g. `{"gpt-4.1-nano": 1048576}`) |

The Responses API is fully supported — including tool use, streaming, reasoning
effort slugs (mapped to the `reasoning_effort` wire field), multi-turn chaining
via `previous_response_id`, and **programmatic tool calling** (gpt-5.6+
models). With `default_request_format = "responses"`, system messages go into
the `input` array and tool results into `function_call_output` input items.
Programmatic tool calling auto-enables for gpt-5.6 models using the Responses
API; set `programmatic_tool_calling = true` to override.

Sessions can be created and browsed while the daemon is locked — credentials
are only required when running prompts.

## Slash commands

In `choreo-tui`:

- `/ping` — health check
- `/models` — list and select models
- `/model` — alias for `/models`
- `/session` — show current session info
- `/session list` — list all sessions
- `/session new [title]` — create a new session
- `/session switch <id>` — switch to a different session
- `/session info <id>` — show info for a specific session
- `/cancel <request-id>` — cancel a running request
- `/unlock [passphrase]` — unlock the daemon (reads `identity.pk` or decrypts `identity.pk.enc`)
- `/lock` — lock the daemon, clearing credentials from memory
- `/add-key <service> <api_key> [unlock]` — add an API key credential (service name must be `[a-z0-9_-]`)
- `/add-x <service> <api_key> <api_key_secret> <access_token> <access_token_secret> <bearer_or_->_ [unlock]` — add an X credential (service name must be `[a-z0-9_-]`)
- `/remove-key <service>` — remove a credential
- `/account list` — list configured AI provider accounts
- `/account remove <name>` — remove an AI provider account
- `/account <name>` — set the session's AI provider account
- `/reasoning` — show current reasoning effort slug
- `/reasoning <slug>` — set reasoning effort (e.g. `off`, `low`, `medium`, `high`, `on`, `xhigh`, `max`; available values depend on the model)
- `Ctrl+R` — cycle reasoning effort through available slugs for the attached session's model
- `Ctrl+M` — open the model selector: list models available on the attached session's account, type to filter, Enter to select, Esc to dismiss (requires a terminal that implements the kitty keyboard protocol — e.g. kitty, foot, wezterm, ghostty, alacritty; on other terminals Ctrl+M arrives as Enter)
- `/continue` — continue a stopped/idle session by sending a "Please continue." prompt
- `/stop` — cancel whatever request is currently active on the attached session (same as `/cancel 0`)
- `/undo` — undo the most recent user turn and its entire assistant response subtree
- `/redo` — redo the most recently undone turn (cleared if new input is sent)
- any other input — sent as a prompt

In `choreo-tui`, `Ctrl+C` exits the local client and disconnects from the
daemon without requesting daemon shutdown.

## Security model

The daemon starts **locked**. Clients resolve the private key (reading
`identity.pk` directly, or decrypting `identity.pk.enc` with a passphrase) and
send it to the daemon via `ClientMessage::Unlock`. The daemon then decrypts all
stored credential blobs into memory.

- Credentials are encrypted per-credential with ECDH (X25519) + HKDF +
  AES-256-GCM; only the holder of the private key can decrypt them.
- `/lock` destroys all in-memory credentials and returns the daemon to the
  locked state.
- The private key is zeroized after use; lock/unlock does not interrupt session
  browsing — credentials are only needed at prompt time.
- Remote connections (over TCP) use the Noise IK handshake with X25519 key
  agreement, giving an authenticated, encrypted transport for clients like
  `choreo-gui` (via `--tcp-addr` / `--server-pk`).

## Monitoring

The daemon can expose an OpenMetrics (Prometheus) endpoint:

```bash
cargo run --release -p choreo-daemon -- --metrics-addr 127.0.0.1:9464
```

When `--metrics-addr` is provided, a dedicated HTTP thread serves `GET /metrics`
at the given address. Without the flag, no metrics server is started. Metrics
include session counts, connection counts, request latency, API call latency,
tool execution time, error breakdowns, and process-level metrics (RSS, CPU,
file descriptors).

## Testing & development

```bash
cargo test                  # unit tests
cargo test -- --ignored     # integration tests
cargo clippy --workspace    # lints
cargo fmt --all             # formatting
```

## Troubleshooting

- `choreo-tui` writes its diagnostics to `/tmp/choreo-tui.log` — check there
  for client-side issues.
- The daemon logs to stderr; use `-v`/`-vv` for more detail, or set
  `RUST_LOG`.

## License

[Apache License 2.0](LICENSE)
