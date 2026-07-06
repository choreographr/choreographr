# tai

A local AI terminal interface in Rust. Connects to an OpenAI-compatible API, runs tools, and streams responses over a Unix socket.

## Crates

| Crate | Description |
|---|---|
| `tai-daemon` | Unix socket server that validates credentials, manages persistent sessions (with sub-sessions and CWD), runs requests with tool-call loop, and streams responses |
| `tai-tui` | Full-screen terminal UI client (ratatui + crossterm) |
| `tai-dioxus` | Minimal desktop GUI client |
| `tai-im` | Instant messaging bridge (Telegram) that connects to the daemon |
| `tai-client-core` | Shared parsing, markdown, and image assembly for UI clients |
| `tai-proto` | Framed binary protocol used between client and daemon |
| `tai-keystore` | Encrypted credential keystore used by the daemon |

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

In `tai-tui`, `Ctrl+C` exits the local client and disconnects from the daemon without
requesting daemon shutdown.

## Configuration

The daemon reads config from `~/.config/tai-daemon/config.toml` (all fields optional):

```toml
base_url = "https://api.openai.com/v1"
model_list_path = "/models"
responses_path = "/responses"
chat_completions_path = "/chat/completions"
default_request_format = "chat_completions"   # or "responses"
chat_completions_max_tokens = 4096
streaming = true
max_turns = 25
retry_max_attempts = 5
retry_initial_backoff_ms = 1000
retry_max_backoff_ms = 30000
connect_timeout_secs = 30
request_timeout_secs = 120

[model_request_formats]
gpt-5 = "responses"

[model_max_tokens]
big-pickle = 4096

[context]
context_file_names = ["AGENTS.md", "CLAUDE.md"]
context_file_max_bytes = 32768
disable_claude_code_prompt = false
```

API keys are stored in the encrypted keystore (`~/.config/tai-daemon/credentials.enc`) and managed via `tai-keystore` CLI or the `/unlock` command at runtime.

The socket path defaults to `/tmp/tai.sock` and can be overridden via `TAI_SOCKET_PATH`. The database path defaults to `~/.local/share/tai-daemon/state.redb` and can be overridden via `TAI_DB_PATH`. The keystore path can be overridden via `TAI_KEYSTORE_PATH`.

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
- `/unlock <passphrase>` — unlock the encrypted keystore
- `/lock` — lock the daemon, clearing credentials from memory
- `/add-key <service> <api_key> <passphrase>` — add an API key to the keystore
- `/remove-key <service> <passphrase>` — remove a credential from the keystore
- any other input — sent as a prompt

## Testing

```bash
cargo test                  # unit tests
cargo test -- --ignored     # integration tests
```

## License

No license file is present in this repository yet.
