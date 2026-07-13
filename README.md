# tai

A local AI terminal interface in Rust. Supports multiple LLM providers (OpenAI-compatible, Anthropic, Google Gemini, and 30+ others via a pluggable catalog), runs tools, and streams responses over a Unix socket.

## Crates

| Crate | Description |
|---|---|
| `tai-daemon` | Unix socket server that validates credentials, manages persistent sessions (with sub-sessions and working directory), runs requests with tool-call loop, and streams responses |
| `tai-tui` | Full-screen terminal UI client (ratatui + crossterm) |
| `tai-dioxus` | Minimal desktop GUI client |
| `tai-im` | Instant messaging bridge (Telegram) that connects to the daemon |
| `tai-client-core` | Shared parsing, markdown, and image assembly for UI clients |
| `tai-proto` | Framed binary protocol used between client and daemon |
| `tai-keystore` | X25519 keypair crypto library for credential encryption |

## Concepts

**Agent loop (harness):** The daemon drives a server-side loop that repeatedly sends
conversation history and available tools to the LLM, executes any tool calls the model
requests, appends results back into the conversation, and loops until the model produces a
final answer or the per-session iteration cap is reached. Each session keeps a responsive
control thread and runs request work in a separate worker thread. The client only sees
`ToolCallStarted`/`ToolCallFinished` lifecycle events, keeping it simple.

**Session / subsession:** A *session* is a persisted conversation with its own message
history, AI model, working directory, and tool-iteration cap. Sessions form a parent-child
tree, support multiple concurrent client attachments, and survive daemon restarts via an
embedded `redb` database. A *subsession* is a child session spawned by the
`spawn_subsession` tool — it inherits the parent's working directory, runs its own full
agent loop independently, and returns its output as the parent's tool result. Subsessions
persist permanently and can be inspected like any other session.

**Diff rendering:** Unified diff output from tool results (`edit_file`, `git_diff full`) is
automatically detected by the TUI client and rendered side-by-side with red/green coloring
for deletions and additions, rather than as raw monospaced text.

**Tool:** A function the LLM can call to interact with the outside world (read files, make
HTTP requests, run git commands, query blockchains, post to X, etc.). Tools implement the
`Tool` trait (name, group, description, JSON Schema, `fn execute`) and are registered in a
`ToolRegistry` at daemon startup. The daemon passes tool definitions for active groups to
the model with each request and executes them on the model's behalf, feeding results back
into the conversation.

**Tool group:** Tools are organized into groups (`core`, `git`, `shell`, `x`, `vm`) declared on the `Tool` trait. Only `core`, `git`, and `shell` are active by default. The model can activate additional groups with the `load_tools` tool and deactivate them with `unload_tools`. Groups are a discovery mechanism, not access control — the RISC-V VM always has access to all tools.

**Skill:** A filesystem-based extension following the Agent Skills standard — a `SKILL.md`
file with YAML frontmatter (`name`, `description`) placed under `.agents/skills/<name>/`.
At session creation, skill names and descriptions are listed in the system prompt. When
the model calls the `load_skill` tool, the full instruction body is injected into the
conversation (progressive disclosure), giving the model specialized knowledge on demand
without bloating every request.

## Build

```bash
cargo build
```

## Run

Start the daemon:

```bash
cargo run -p tai-daemon         # default log level: info
cargo run -p tai-daemon -- -v   # debug
cargo run -p tai-daemon -- -vv  # trace
cargo run -p tai-daemon -- -q   # warnings only
```

Alternatively, set `RUST_LOG` (takes precedence over CLI flags):

```bash
RUST_LOG=debug cargo run -p tai-daemon
```

Then a client:

```bash
cargo run -p tai-tui     # terminal UI
cargo run -p tai-dioxus  # desktop app
cargo run -p tai-im      # IM bridge
```

Available provider options in the TUI's "AI Provider Accounts" page include OpenAI, Anthropic, Google Gemini, DeepSeek, xAI Grok, Groq, Together AI, Mistral, Ollama, OpenRouter, Hugging Face, GitHub Models, NVIDIA NIM, Cerebras, Fireworks AI, Xiaomi MiMo, DashScope, Moonshot AI, Perplexity, Z.ai, Venice AI, Novita AI, LM Studio, OpenRouter, MiniMax, and DM Enterprise. Use `n` to add a new account, select provider in the form, enter an API key, and press Enter to save.

In `tai-tui`, `Ctrl+C` exits the local client and disconnects from the daemon without
requesting daemon shutdown.

## Configuration

The daemon reads config from `~/.config/tai-daemon/config.toml` (all fields optional):

```toml
max_turns = 25

[context]
context_file_names = ["AGENTS.md", "CLAUDE.md"]
context_file_max_bytes = 32768
disable_claude_code_prompt = false
```

> **Note:** Provider-level settings (`base_url`, `streaming`, `retry_*`, timeouts, endpoint paths, request format, etc.) have moved to per-account overrides in `accounts.toml`. They are no longer read from `config.toml`.

Credentials are encrypted per-credential with the daemon's X25519 public key and stored in the `redb` database. Identity keys reside in `~/.config/tai-daemon/identity.pk` (private), `~/.config/tai-daemon/public.pk` (public), and optionally `~/.config/tai-daemon/identity.pk.enc` (passphrase-encrypted private key).

The socket path defaults to `/tmp/tai.sock` and can be overridden via `TAI_SOCKET_PATH`. The database path defaults to `~/.local/share/tai-daemon/state.redb` and can be overridden via `TAI_DB_PATH`.

## Shell commands

In `tai-tui`:

- `/ping` — health check
- `/models` — list and select models
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
- `/reasoning` — show current reasoning effort
- `/reasoning off|low|medium|high` — set reasoning/thinking effort for the session
- `/account <name>` — set the session's AI provider account
- any other input — sent as a prompt

Account names must be lowercase alphanumeric with hyphens or underscores (`[a-z0-9_-]`).
Supported providers: all entries in the provider catalog — ~30 providers across OpenAI-compatible, Anthropic-compatible, and Google Gemini protocols. See `tai-daemon/src/providers/catalog.rs` for the full list.
Each session may have its own account, set via `/account <name>`. There is no global default
account. Sessions can be created and browsed while the daemon is locked — credentials are
only required when running prompts.

Accounts are configured via `~/.config/tai-daemon/accounts.toml`:
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

[[account]]
name = "mistral"
provider = "mistral"
```

Each provider from the catalog is pre-configured with sensible defaults (base_url, default model). Override any field per-account. Available per-account overrides:

| Field | Description |
|-------|-------------|
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
| `model_request_formats` | Per-model request format overrides |
| `chat_completions_max_tokens` | Default max tokens for chat completions |
| `model_max_tokens` | Per-model max token caps |
| `chat_completions_max_tokens_field` | Token field: `"max_tokens"` or `"max_completion_tokens"` |
| `model_max_tokens_fields` | Per-model token field overrides |
| `responses_max_output_tokens` | Default max output tokens for Responses API |
| `responses_store` | Persist Responses API conversations (default: `true`) |
| `model_responses_max_output_tokens` | Per-model max output tokens for Responses API |
| `programmatic_tool_calling` | Enable programmatic tool calling (Responses API, gpt-5.6+) |
| `context_window` | Default context window for all models (overrides catalog defaults) |
| `model_context_windows` | Per-model context window overrides (e.g. `{"gpt-4.1-nano": 1048576}`) |

The Responses API is now fully supported — including tool use, streaming, reasoning effort, multi-turn chaining via `previous_response_id`, and **programmatic tool calling** (gpt-5.6+ models). When `default_request_format` is set to `"responses"`, the daemon uses the Responses endpoint for all requests (both simple completions and tool-assisted turns), with system messages mapped to the `instructions` field and tool results to `function_call_output` input items. Programmatic tool calling auto-enables for gpt-5.6 models using the Responses API; set `programmatic_tool_calling = true` in `accounts.toml` to override.

## Monitoring

The daemon can expose an OpenMetrics (Prometheus) endpoint:

```bash
cargo run -p tai-daemon -- --metrics-addr 127.0.0.1:9464
```

When `--metrics-addr` is provided, a dedicated HTTP thread serves `GET /metrics`
at the given address. Without the flag, no metrics server is started.

Metrics include session counts, connection counts, request latency, API call
latency, tool execution time, and error breakdowns. Process-level metrics (RSS,
CPU, file descriptors) are also included.

## Testing

```bash
cargo test                  # unit tests
cargo test -- --ignored     # integration tests
```

## License

No license file is present in this repository yet.
