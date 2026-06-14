# tai

A small Rust workspace for a local AI terminal interface.

`tai` is split into three crates:

- `tai-daemon` — a Unix socket server that validates OpenAI-compatible credentials, lists models, accepts requests, and returns model output
- `tai-sh` — a terminal UI client built with `ratatui` and `crossterm`
- `tai-dioxus` — a minimal desktop app client built with `Dioxus`
- `tai-proto` — the shared framed binary protocol used between the client and daemon

The current implementation is intentionally small and local-first:

- communication happens over a Unix domain socket
- the daemon speaks to an OpenAI-compatible HTTP API
- the shell lets you list/select models and submit prompts interactively
- the daemon streams text responses incrementally when the provider supports SSE/token streaming
- the protocol and UI already have image support primitives, though the daemon currently only returns text

## Workspace layout

```text
.
├── tai-daemon/
├── tai-dioxus/
├── tai-proto/
└── tai-sh/
```

## Features

### `tai-daemon`

- Loads auth/config from `auth.toml`
- Validates provider credentials on startup by listing models
- Serves multiple concurrent client requests
- Tracks request IDs and rejects duplicate active IDs
- Supports cancellation for active requests
- Exposes model discovery and model selection
- Uses structured logging via `tracing`
- Reuses a shared HTTP client for model listing and completions
- Streams text tokens/chunks to clients when enabled

### `tai-sh`

- Full-screen terminal UI
- Scrollable history pane
- Interactive command input
- Displays daemon status, request lifecycle events, and model information
- Includes image rendering support through `ratatui-image` for protocol messages that carry images

### `tai-dioxus`

- Minimal desktop app UI
- Uses the same Unix socket transport and `tai-proto` message types as `tai-sh`
- Supports prompt input, streaming output, model listing/selection, and cancellation
- Image messages are not rendered yet

### `tai-proto`

- Length-prefixed framed protocol
- Versioned message format
- Shared client/daemon message enums
- Limits frame size to avoid unbounded payloads
- Includes text and image message variants

## Requirements

- Rust toolchain with Cargo
- Unix-like OS with Unix domain socket support
- An OpenAI-compatible API endpoint

## Configuration

`tai-daemon` reads its auth config from:

- Linux: `~/.config/tai-daemon/auth.toml`
- more generally: `dirs::config_dir()/tai-daemon/auth.toml`

Example:

```toml
api_key = "sk-..."
base_url = "https://api.openai.com/v1"
model_list_path = "/models"
responses_path = "/responses"
chat_completions_path = "/chat/completions"
default_request_format = "chat_completions"
chat_completions_max_tokens = 4096
streaming = true

[model_request_formats]
gpt-5 = "responses"
gpt-5-mini = "responses"
legacy-model = "chat_completions"

[model_max_tokens]
big-pickle = 4096
```

Only `api_key` is required if you want the default OpenAI endpoints.

### Migration note

If you already have a `~/.config/tai-daemon/auth.toml`, you can now configure both request formats, choose a default, and control streaming:

```toml
responses_path = "/responses"
chat_completions_path = "/chat/completions"
default_request_format = "chat_completions"
chat_completions_max_tokens = 4096
streaming = true

[model_request_formats]
gpt-5 = "responses"

[model_max_tokens]
big-pickle = 4096
```

Supported request format values are `"chat_completions"` and `"responses"`.

### Config fields

- `api_key` — bearer token sent to the provider
- `base_url` — base URL for the OpenAI-compatible API
- `model_list_path` — path used for model listing
- `responses_path` — path used for Responses API requests
- `chat_completions_path` — path used for chat completions requests
- `default_request_format` — default request format for models not explicitly overridden
- `model_request_formats` — per-model request format overrides
- `chat_completions_max_tokens` — default max token cap sent on chat completions requests
- `model_max_tokens` — per-model max token caps for chat completions requests
- `streaming` — enable streaming completions via SSE when the provider supports it; falls back to one-shot requests when false

## Socket path

By default the client and daemon use:

```text
/tmp/tai.sock
```

You can override it with:

```bash
export TAI_SOCKET_PATH=/tmp/custom-tai.sock
```

## Build

```bash
cargo build
```

## Run

Start the daemon in one terminal:

```bash
cargo run -p tai-daemon
```

Start the shell in another:

```bash
cargo run -p tai-sh
```

Or start the desktop app:

```bash
cargo run -p tai-dioxus
```

If startup succeeds, the daemon will validate your config against the provider before listening for requests.

## Shell commands

Inside `tai-sh`:

- `:ping` — ask the daemon for a health-style pong response
- `/models` — list available models and show the selected one
- `/models <model-id>` — select a model
- `:cancel <request-id>` — cancel an active request
- any other non-empty line — send that line as a prompt

## Example session

```text
/models
/models gpt-5.4-nano
Write a haiku about terminals
:cancel 3
```

## Protocol overview

The protocol is a length-prefixed binary frame:

- 4-byte big-endian payload length
- bincode-encoded `(protocol_version, message)` tuple

Current protocol version:

```text
1
```

### Client messages

- `RunInput { request_id, input }`
- `Cancel { request_id }`
- `Ping`
- `ListModels`
- `SetModel { model }`

### Daemon messages

- `Started { request_id }`
- `OutputChunk { request_id, stream, data }` — emitted incrementally for streamed text and once for non-streaming text
- `ImageStart { ... }`
- `ImageChunk { ... }`
- `ImageEnd { ... }`
- `Done { request_id }`
- `Failed { request_id, error }`
- `Cancelled { request_id }`
- `Pong`
- `Models { models, selected_model }`
- `ModelsFailed { error }`
- `ModelSelected { model }`
- `ModelSelectionFailed { model, error }`

## Testing

Run all tests with:

```bash
cargo test
```

This workspace includes unit and integration tests for:

- protocol framing and version handling
- shell command parsing
- image assembly logic
- daemon request lifecycle behavior
- model-listing and selection failure cases
- cancellation and duplicate request handling

## Notes and current limitations

- The shell and protocol support image messages, but the current daemon implementation only emits text completions.
- The daemon supports both chat completions and Responses API requests.
- Request format is selected by `default_request_format`, with optional per-model overrides in `model_request_formats`.
- Model selection is per client connection, not global.
- `tai-sh` currently targets a local daemon over Unix sockets; there is no Windows named-pipe transport.
- The protocol caps frame sizes at 1 MiB.

## Crate summary

- `tai-proto`: shared wire format and socket helpers
- `tai-daemon`: provider-backed request executor
- `tai-sh`: terminal client and image-capable history viewer
- `tai-dioxus`: minimal Dioxus desktop client

## License

No license file is present in this repository yet.
