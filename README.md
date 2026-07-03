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

## Build

```bash
cargo build
```

## Run

Start the daemon:

```bash
cargo run -p tai-daemon
```

Then a client:

```bash
cargo run -p tai-tui     # terminal UI
cargo run -p tai-dioxus  # desktop app
cargo run -p tai-im      # IM bridge
```

## Configuration

The daemon reads auth config from `~/.config/tai-daemon/auth.toml`:

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

[model_max_tokens]
big-pickle = 4096
```

Only `api_key` is required. The socket path defaults to `/tmp/tai.sock` and can be overridden via `TAI_SOCKET_PATH`.

## Shell commands

In `tai-tui`:

- `/ping` — health check
- `/models` — list and select models
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
