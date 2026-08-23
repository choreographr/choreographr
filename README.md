# Choreographr

<p align="center">
  <a href="LICENSE"><img src="https://img.shields.io/badge/License-Apache_2.0-blue" alt="Apache 2.0"></a>
  <a href="https://www.rust-lang.org/"><img src="https://img.shields.io/badge/Rust-2024_edition-orange" alt="Rust 2024"></a>
  <img src="https://img.shields.io/badge/providers-208+-brightgreen" alt="208+ providers">
  <img src="https://img.shields.io/badge/wire_protocols-3-lightgrey" alt="3 wire protocols">
  <a href="https://t.me/choreographr"><img src="https://img.shields.io/badge/Telegram-Choreographr_Community-2CA5E0?logo=telegram&logoColor=white" alt="Telegram community"></a>
</p>

## What is Choreographr?

Choreographr is an all purpose extensible AI agent system written entirely in Rust. It has a client/server architecture and can run many sessions simulataneously. It can be run locally or in the cloud. LLM generated code can be run in a sandboxed RISC-V VM for complete security and observability.

### All Purpose

Choreographr is a all-purpose agent. It can be used for software development, a personal / business agent, or as a research tool. It can run on your desktop or in the cloud.

## Community

Join the [Choreographr Community](https://t.me/choreographr) on
Telegram for announcements, questions, show-and-tell, and development
chatter.

[![Telegram](https://img.shields.io/badge/Telegram-Join_the_community-2CA5E0?logo=telegram&logoColor=white)](https://t.me/choreographr)

### Client / Server Architecture

Choreographr was designed from the beginning to have a separation of concerns between the server software that actually runs the sessions and the clients that can connect and disconnect any time.

The client can either run on the same computer as the server agent (via local socket), or the server can be anywhere else on your local network or the Internet. When not connecting locally the client connects to the server via [Noise-IK](https://en.wikipedia.org/wiki/Noise_Protocol_Framework) encrypted TCP connection. Because the server can live anywhere and is reachable over encrypted TCP, it can also be accessed from mobile devices — for example, chatting with your agent on the go via the `choreo-im` Telegram bridge.

Client/server communication is encoded in [MessagePack](https://msgpack.org/) in *named* mode — a self-describing, compact binary format with broad language support (struct field names and enum variant names travel on the wire, so the format is evolution-safe for future mobile/web/third-party clients). [Postcard](https://postcard.jamesmunns.com/) remains only on internal Rust-only channels: the RISC-V VM↔host protocol and encrypted credential storage.

Delivery is **lossless**: the daemon never drops a broadcast message. Each connected client gets an unbounded queue drained by its own writer thread, so a slow client can never stall a session or the daemon loop — memory is bounded instead by **lag-based eviction** (per-client 64 MiB cap, 512 MiB daemon-wide). A client that falls too far behind receives a best-effort `Evicted` advisory and is disconnected; it reconciles on reconnect via the attach/snapshot path. Final turns ride a single `TurnAppended` delivery — a `SessionEvent` wrapped in the `DaemonMessage::Session { session_id: Option<u64>, event }` envelope (protocol v4) — so the live stream and the recorded turn can never diverge.

Currently the primary client is **`choreo-tui`** - a fullscreen terminal UI.

Other clients being developed: 

- **`choreo-gui`** — GUI built with [Dioxus](https://dioxuslabs.com/) - just a placeholder for now, it will support Linux, macOS, Windows, Android and iOS.
- **`choreo-im`** — instant-messaging bridge (Telegram, more platforms coming) - chat with your agent on the go!
- **`choreo-acp`** — ACP bridge so ACP-compatible editors (Claude Code, Cline, …) can drive Choreographr sessions over JSON-RPC.
- **`choreographr`** — Choreographr servers will be able to connect to other servers to deploy work elsewhere.

### RISC-V Virtual Machine

The LLM can invoke the RISC-V VM (powered by [CKB VM](https://github.com/nervosnetwork/ckb-vm)) by either providing a Rust snippet, or pre-compiled bytecode. Other languages will be supported in future.

This has 2 main purposes:
- a tool call scripting language - the LLM can quickly write a little script to call tools with custom logic
- a complete replacement for the shell tool. Giving the LLM direct access to the shell is potentially very dangerous. Disabling the shell tool and doing everything via the VM provides complete control and observability.

### Browsing the web

The `retrieve_webpage` tool renders a URL in a **local** headless Chromium/Chrome (preferring chromium; a binary must already be installed — there is no auto-download) and returns the page's HTML, plain text, a PNG screenshot (inline or to an `output_path`), or a PDF (to `output_path`). URLs may use the `http`, `https`, or `file` scheme — `file://` renders a local file directly in the browser. It runs locally and offline; for cloud-hosted headless rendering, see the Cloudflare Browser Run / Kitesurf research in this project's history.

### Multiple live sessions

Each server can run multiple sessions simultaneously (only limited by system resources). Sessions are stored in the database and only "woken-up" when a client connects to them.

Rather than having a multi-session terminal multiplexor, you can manage all your sessions directly from a client program.

Sessions have undo/redo functionality. If an LLM is mis-prompted it is often better to remove the prompt than to prompt more to try to "fix it".

### Hierarchical Sessions

Many agents support the concept of "subagents". Choreographr has "subsessions". This enables work to be broken up into manageable chunks and potentially worked on in parallel.

In Choreographr, the LLM or VM can start new sessions that will report back once they are finished. Subsessions are real sessions that can be interacted with like any other session. The user can pause them and provide additional prompting. Subsessions can invoke their own subsessions as necessary.

### Agent databases

LLMs can create persistent key/value databases. The LLM / VM can store data and retrieve it at a later time.

### High performance Multithreaded Architecture

Currently the codebase doesn't use any async code - this reduces the complexity of the codebase significantly. It uses real kernel threads with event loops and message passing. Mutable state is not shared between threads (except for the message passing). Everything is event driven without polling.

The one exception is the optional `blockchain` feature: the `choreo-blockchain` crate (linked only then) holds a tokio sidecar runtime for the async alloy/subxt clients, and the daemon calls its synchronous `execute_*` entry points.

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

While the VM itself is a perfect sandbox, tools are executed outside of this sandbox for example, if the shell tool is enabled. An OS-level sandbox will be required.

On Linux, [Landlock](https://landlock.io/) will be used. On macOS, [Seatbelt](https://theapplewiki.com/wiki/Dev:Seatbelt).
Windows does not have a good solution for this yet.


### Advanced Context Management

The session context needs to be divided between permanent and temporary context. Permanent context should be append-only (except when undoing) this ensures maximum cache hit rate. 

Currently, as with most AI agents, if the LLM wants to see a file it issues the `read_file` tool. This adds it permanently into the session context. A better solution is to have an `add_to_context` tool with the option to add it to the permanent or temporary context. If it is added the to temporary context it can be removed later by a `remove_from_context` tool.

### Git Worktree Support

To get the most out of subsessions, they need to run in parallel on the same codebase. The problem is that they will interfere with each other's work. The solution is for each subsession to work on its own branch in its own directory. This is where [Git Worktrees](https://git-scm.com/docs/git-worktree) come in. Once a subsession has finished committing in its own branch, the parent session can merge it into its own branch. Any merge conflicts can be resolved by the LLM.

The problem with worktrees is that programming languages such as Rust can have many gigabytes of build artifacts. If each worktree has to regenerate these it consumes CPU bandwidth, I/O bandwidth, storage space and is generally very slow. Copying the artifacts from the parent's tree reduces the CPU bandwidth, but is still a big problem.

The solution is to use CoW filesystems such as [BTRFS](https://en.wikipedia.org/wiki/Btrfs) so the file is only copied if it is re-generated by the subsession.

### Looping

A common scenario in agentic coding is to manually "loop" over the codebase changes until a certain goal is met. For example, after a new feature has been implemented a new session can be prompted to check the changes for bugs, potential refactorings, optimizations, security issues. The LLM will then make some recommendations. It will then be prompted to implement these. Once this is complete a new session is created to do it again. This process repeats until the LLM says it is ready, or only complains about very minor issues.

Choreographr will have an option to automate this process, so it can be left alone to complete the whole process without interaction.

## Comparison to other agents

Feature matrix against other AI agent projects, ordered by GitHub ⭐ descending
after Choreographr.

| Feature | Choreo | openclaw | hermes | opencode | codex | pi | goose | langgraph | buzz | openwork | t3code | OpenMinis | mercury | tau | maka-agent | zero | turnstone |
|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|
| Language | Rust | TypeScript | Python | TypeScript | Rust | TypeScript | Rust | Python | Rust | TypeScript | TypeScript | Swift/Kotlin | TypeScript | Python | TypeScript | Go | Python |
| Daemon + multi-client | ✅ | ✅ | — | server | — | — | server | — | ✅ | ✅ | ✅ | — | ✅ daemon | — | ✅ | — | server |
| Concurrent sessions | ✅ daemon | ✅ gateway | ✅ capped | ✅ server | ✅ threads | — | ✅ server | ✅ framework | ✅ | — | ✅ | — | ✅ | — | ✅ | ✅ pool | ✅ |
| Providers | 208/3 proto | 40+ | 34 | 15 | 1 (OpenAI) | 42/9 proto | 39 | agnostic | agnostic | agnostic | 5 (drives) | 8 | 6 | 28 | multi | 36 | 5 |
| OAuth | coming | ✅ | ✅ 6× | ✅ | ✅ ChatGPT | ✅ | — | — | — | ✅ | — | ✅ | ✅ device | ✅ | ✅ subs | ✅ | ✅ MCP |
| Credential rotation/fallback | retry only | ✅ failover | ✅ pool | — | — | — | — | — | — | — | — | ✅ fallback | ✅ | — | — | — | ✅ |
| Tool permission gating | coming | ✅ | env-only | ✅ | ✅ | — | ✅ | — | — | — | ✅ | ✅ | ✅ | — | ✅ | ✅ | ✅ judge |
| Compaction | coming | ✅ | ✅ | ✅ | ✅ | ✅ | ✅ | — | ✅ | — | — | ✅ | — | ✅ | ✅ | ✅ | ✅ |
| Sandbox | RISC-V VM · Landlock/Seatbelt coming soon | Docker/SSH | Docker/SSH | — | ✅ sandbox | — | — | — | — | — | — | iSH/PRoot | — | — | — | seccomp eng. | OpenShell |
| Subagents | subsessions | swarm | delegation | ✅ | ✅ | — | — | subgraphs | agent pool | — | — | — | ✅ | — | ✅ graph | specialists | workstreams |
| Skills (SKILL.md) | ✅ | ✅ | ✅ | ✅ | ✅ | ✅ | — | — | — | ✅ | — | ✅ | ✅ | ✅ | ✅ | ✅ | ✅ |
| MCP client | ✅ | ✅ | ✅ | ✅ | ✅ | — | ✅ | — | ✅ | ✅ meta | — | ✅ | — | — | ✅ | ✅ | ✅ (OAuth) |
| ACP | bridge | bridge | ✅ | ✅ | — | — | server | — | harness | — | — | — | — | — | — | ✅ | — |
| IM surfaces | Telegram | 25+ | 20+ | — | — | — | — | — | chat natively | — | — | — | CLI/Web/Telegram | — | — | — | 2 |
| Web search | coming | ✅ | ✅ | ✅ | ✅ | — | — | — | — | — | — | — | ✅ | — | ✅ | ✅ | ✅ |
| Hooks/lifecycle | coming | ✅ | ✅ | ✅ | ✅ | ✅ | ✅ | — | ✅ | — | — | — | — | — | — | ✅ | — |
| Plugins | coming | ✅ | ✅ | ✅ | ✅ | ✅ | — | — | — | ✅ | — | — | — | — | — | ✅ | — |
| Cron/scheduling | coming | — | ✅ | — | — | — | ✅ | — | ✅ | — | — | — | ✅ | — | ✅ | ✅ | — |
| Long-term memory | coming | ✅ | ✅ | — | ✅ | — | — | ✅ | — | — | — | ✅ | ✅ | — | — | — | ✅ |
| Encrypted creds | ✅ unique | ✅ | — | — | ✅ keyring | — | ✅ keyring | — | NIP auth | ✅ | ✅ | ✅ keychain | — | 0600 | — | ✅ | ✅ Fernet |
| Storage | redb | SQLite | SQLite | event src | SQLite | JSONL | SQLite | SQLite/Postgres | Postgres | fs | — | SQLite | SQLite+JSONL | JSONL | SQLite | fs JSONL | SQL/Postgres |
| Metrics | ✅ | ✅ OTel | — | — | ✅ OTel | telemetry | telemetry | — | — | — | — | — | — | — | — | — | ✅ |
| Undo/redo | ✅ | — | ✅ | — | — | branch | — | time-travel | — | — | — | — | — | — | — | rewind | replay |
| Context fingerprints | ✅ | — | — | — | — | — | — | — | — | — | — | — | — | — | — | partial | ✅ |

### Concurrent sessions

Choreographr's headline concurrency: one daemon runs many sessions at once — each
session is an independent control thread with at most one request worker, sessions
persist to `redb` and only wake when a client attaches, any number of clients can
subscribe to the same session, and subsessions (children) run their own loops in
parallel and can be interacted with independently. How the other agents compare:

- **openclaw** — Gateway hosts many concurrent chat sessions; per-session actor
  queues serialize ACP operations while the swarm tool fans out parallel subagents
  (default `maxConcurrent: 8`).
- **hermes** — Gateway processes messages concurrently via asyncio; a
  `max_concurrent_sessions` cap (default unset = unlimited) limits simultaneous
  active chat sessions, enforced via a cross-process lease file, with concurrent
  turns on different sessions kept isolated.
- **opencode** — Server mode (`opencode serve`) exposes sessions over HTTP; each
  session runs one prompt at a time (a `SessionBusyError` rejects overlapping
  runs) but many sessions run concurrently, and the TUI / web / desktop all attach
  to the same server.
- **codex** — App-server `ThreadManager` tracks a tree of threads; each thread has
  its own serialized listener, subagents spawn as child threads (`spawn_subagent`),
  and concurrent requests are tracked with unique in-flight IDs.
- **pi** — Single-process CLI: sessions are JSONL files you resume or fork; within a
  session, tool calls default to parallel execution (`toolExecution: "parallel"`)
  but only one session runs per process.
- **goose** — SessionManager over SQLite; the desktop app lists and switches many
  sessions, and the ACP server multiplexes them, but each session handles one
  prompt at a time.
- **langgraph** — A framework rather than a daemon: durable execution keyed by
  `thread_id`, subgraphs, and parallel graph branches give the building blocks;
  concurrency is up to the hosting app.
- **buzz** — Relay/ACP harness supports unlimited concurrent sessions
  (`BUZZ_AGENT_MAX_SESSIONS`; one prompt per session at a time) with up to 8
  parallel tool calls per turn, and agents are first-class members of shared
  channels.
- **openwork** — Desktop app with per-workspace session groups; it exposes
  capabilities over MCP rather than running many sessions in parallel itself.
- **t3code** — A control surface: one app drives Codex, Claude Code, Cursor, Grok
  Build and OpenCode concurrently, each with its own sessions/panes.
- **OpenMinis** — On-device agent with separate workspaces; tool calls run
  concurrently (up to 10 via `TaskGroup`) and background sessions are supported,
  but it is a mobile app rather than a multi-session server.
- **mercury** — Background daemon with a pool of sub-agent workers (auto-scaled by
  CPU cores, overridable); the main agent queues messages while busy, and board
  batches run concurrently per batch.
- **tau** — Single-session teaching harness: append-only JSONL sessions, resume and
  branch, parallel tool calls within a turn, but one session at a time.
- **maka-agent** — Runtime serves several concurrent runs; `ChildAgentRunLimiter`
  (FIFO permits) caps real child-agent executions, and the Agent Graph runs a
  supervisor that wakes on checkpoints.
- **zero** — Daemon mode supervises a bounded pool of headless `zero exec` worker
  processes (default pool size 4) routing multiple sessions over a local socket,
  with read-only tool calls executed concurrently in a turn and specialist
  subagents as separate sessions.
- **turnstone** — Server runs many workstreams concurrently; each workstream gets
  its own worker thread (queue-or-spawn decided under a lock), children spawn via
  a coordinator, and parallel tool batches are judge-approved before execution.


## Install

Prebuilt releases ship exactly four binaries — `choreographr choreo-tui
choreo-im choreo-acp` (`choreo-mcp` is a library-only crate and ships no
binary) — for **x86_64 Linux** and **macOS (Apple Silicon)**. All installs
below use prebuilt binaries; no Rust or Zig toolchain is required.

### macOS

**Homebrew (recommended).** The `choreographr/choreographr` tap provides a
prebuilt formula — no toolchain needed:

```bash
brew tap choreographr/choreographr
brew install choreographr
brew services start choreographr
```

`brew services` registers a **launchd agent**, so the daemon starts at login
and is kept alive — but only because you asked; nothing is ever auto-enabled.

Alternatives:

- **GitHub Releases tarball** — download
  `choreographr-0.1.0-aarch64-apple-darwin.tar.gz` from the
  [releases page](https://github.com/choreographr/choreographr/releases) and
  put the four binaries on your `PATH`. The binaries are unsigned, so
  Gatekeeper quarantines them: clear the attribute with
  `xattr -dr com.apple.quarantine /path/to/choreographr`, or right-click →
  Open once.
- **curl installer** — pinned version, SHA-256 verified:
  `curl -fsSL https://choreographr.com/install.sh | sh`
- **cargo binstall** — installs the prebuilt tarball, no toolchain:
  `cargo binstall choreographr`
- **cargo install** — builds from source; needs Zig at build time. Installs the
  whole suite (daemon + TUI + IM + ACP — the root package owns all four
  `[[bin]]` targets; `default-run` only affects `cargo run`):
  `cargo install choreographr`

### Linux

- **Debian / Ubuntu** — install the `.deb` from the release:
  `sudo apt install ./choreographr-0.1.0-x86_64.deb`
- **Fedora / RHEL / openSUSE** — install the `.rpm` from the release:
  `sudo dnf install ./choreographr-0.1.0-x86_64.rpm`
- **Arch Linux (AUR)** — the prebuilt `choreographr-bin` package:
  `paru -S choreographr-bin` (or `yay -S choreographr-bin`)
- **Any distro** — tarball + installer, or cargo:
  `curl -fsSL https://choreographr.com/install.sh | sh` ·
  `cargo binstall choreographr` (prebuilt, no toolchain — fetches the static
  musl tarball from GitHub Releases; the binstall manifest maps glibc x86_64
  hosts to the musl asset, so no `--target` is needed) ·
  `cargo install choreographr` (source build, needs Zig — installs the whole
  suite: `choreographr`, `choreo-tui`, `choreo-im`, `choreo-acp`)

### Running the daemon

In 0.1 the daemon is a user service that **you** start — installers place
the service file but never enable it. One of:

```bash
systemctl --user enable --now choreographr       # Linux: unit ships with the .deb/.rpm and the tarball
brew services start choreographr                 # macOS: Homebrew launchd agent
launchctl load ~/Library/LaunchAgents/com.choreographr.daemon.plist   # macOS: non-Homebrew (curl installer)
choreographr                                     # ...or just run it in a terminal
```

The non-Homebrew launchd plist expects `/opt/homebrew/bin/choreographr` —
edit its `ProgramArguments` if your binaries live elsewhere. Once the daemon
is up, attach a client (`choreo-tui`, `choreo-im`, `choreo-acp`) and follow
[First conversation](#first-conversation) below. The daemon listens on the
Unix socket `/tmp/Choreographr.sock` and stores its data under
`~/.local/share/choreographr/` (see [Configuration](#configuration)).

> **Zig?** Only source builds need it. Homebrew, the `.deb`/`.rpm`, the AUR
> `-bin` package, the tarball, and `cargo binstall` all use prebuilt
> binaries — `cargo install` and the [Build from source](#build-from-source)
> path need the Zig toolchain.

## Build from source

Requires a [Rust toolchain](https://rustup.rs/) — minimum supported Rust version (MSRV) is **1.94.1** — and a [Zig toolchain](https://ziglang.org/) (`brew install zig`), which `choreographr` needs to compile the `zlob` glob/walker dependency.

The repo builds on **nightly** Rust by default (`rust-toolchain.toml` pins `nightly`), which lets every `cargo` command — including per-crate ones like `cargo check -p choreo-proto` or `cargo test -p choreo-sanitize` — automatically apply the fast per-profile `-Z` compiler flags (`-Zshare-generics` in dev, parallel rustc frontend in dev and release). No opting in or remembering of flags is needed: a bare `cargo` command just builds fast. `rustup` auto-installs the pinned channel the first time you run `cargo` in the checkout:

```bash
brew install zig
cargo build --release
```

The **source uses no nightly-only features**, so the code also builds on any stable ≥ 1.94.1 (the MSRV floor, enforced by CI). Stable builds are a supported but explicit opt-out, because the nightly-only flags we wire in hard-block stable *Cargo* — use the `just` recipes, which temporarily strip those flags for one command and restore them:

```bash
just build-stable   # build on stable (release by default)
just check-stable   # type-check on stable
just test-stable    # unit tests on stable
```

Start the daemon:

```bash
cargo run --release -p choreographr         # default log level: info
cargo run --release -p choreographr -- -v   # debug
cargo run --release -p choreographr -- -vv  # trace
cargo run --release -p choreographr -- -q   # warnings only
```

`RUST_LOG` takes precedence over the CLI flags:

```bash
RUST_LOG=debug cargo run --release -p choreographr
```

Then a client — the suite binaries live in the root package, selected with `--bin`;
the desktop GUI is a separate crate (`choreo-gui`):

```bash
cargo run --release -p choreographr --bin choreo-tui                 # terminal UI
cargo run --release -p choreo-gui                                    # desktop app
cargo run --release -p choreographr --bin choreo-im                  # IM bridge
cargo run --release -p choreographr --bin choreo-acp                 # ACP bridge for editors
```

### First conversation

1. **Configure an account** in `~/.config/choreographr/accounts.toml` (see
   [Configuration](#configuration)) and add an API key with `/add-key <service> <api_key>`.
2. Select the account with `/account <name>` and start prompting.

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

## Crates

A Rust workspace of fifteen crates (resolver = "3"):

See [ARCHITECTURE.md](ARCHITECTURE.md) for a deep dive into the daemon's
internals — threading model, provider architecture, tool system, and session
data model.

| Crate | Description |
|---|---|
| `choreographr` | Workspace root — the suite installer. Declares the four binaries (`choreographr choreo-tui choreo-im choreo-acp`); `cargo run -p choreographr` / `cargo install choreographr` default to the daemon binary via `default-run` |
| `choreo-daemon` | The core engine — binary `choreographr`. Unix socket server that validates credentials, manages persistent sessions (with sub-sessions and working directories), runs requests with a tool-call loop, and streams responses |
| `choreo-ai-protocols` | Provider protocols — OpenAI-compatible, Anthropic Messages, and Google Gemini clients, the `ProviderClient` trait, and the provider catalog (208 providers) |
| `choreo-blockchain` | Blockchain tools — EVM (alloy) and Substrate/Polkadot (subxt) read-only queries plus the tokio sidecar runtime they run on; pulled in by the daemon's `blockchain` feature (off by default) |
| `choreo-proto` | Framed binary protocol (MessagePack named + length prefix) shared between clients and daemon |
| `choreo-sanitize` | Internal leaf crate — the single source of truth for the Unicode "spoofing" predicates (bidi/ZWSP escaping) and the shared tool-output byte budget + `...[truncated]` marker, used by the daemon, TUI, blockchain tools, and client |
| `choreo-keystore` | X25519 keypair + ECDH/AES-256-GCM crypto library for encrypted credentials |
| `choreo-transport` | Noise-IK encrypted transport over TCP |
| `choreo-mcp` | MCP (Model Context Protocol) client — spawns subprocess servers, discovers tools, dispatches calls over JSON-RPC stdio |
| `choreo-acp` | ACP (Agent Communication Protocol) bridge — translates JSON-RPC 2.0 over stdin/stdout into `choreo-proto` messages so ACP-compatible editors can drive sessions |
| `choreo-tui` | Full-screen terminal UI client (ratatui + crossterm) |
| `choreo-gui` | Desktop GUI client (Dioxus) |
| `choreo-im` | Instant messaging bridge (Telegram) |
| `choreo-client-core` | Shared parsing, markdown, image assembly, and daemon-message dispatch for UI clients |
| `choreo-markdown` | Markdown parser, HTML renderer (pulldown-cmark + ammonia), and a LaTeX-math → Unicode pretty-printer (`render_math_pretty`) |

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
files, make HTTP requests, run git commands, classify PDFs and convert them to
Markdown, query blockchains, post to X, etc.). Tools implement the `Tool` trait
(name, group, description, JSON Schema, `fn execute`) and are registered in a
`ToolRegistry` at daemon startup.

**Tool group.** Tools are organized into groups (`core`, `git`, `shell`, `x`,
`vm`, `db`, `mcp`, `blockchain`). Only `core`, `git`, and `shell` are active by default. The
model can activate additional groups with `load_tools` and deactivate them with
`unload_tools`. Groups are a discovery mechanism, not access control — the
RISC-V VM always has access to all tools.

**Skill.** A filesystem-based extension following the Agent Skills standard — a
`SKILL.md` file with YAML frontmatter (`name`, `description`) placed under
`.agents/skills/<name>/`. At session creation, skill names and descriptions are
listed in the system prompt. When the model calls `load_skill`, the full
instruction body is injected into the conversation (progressive disclosure).

**Reasoning round-trip.** Reasoning text is both *displayed* in the TUI
(collapsible per-turn "Reasoning" section) and, for several providers, *sent
back* to the model on the next request — the tool-call loop otherwise fails
with a 400. The daemon captures the provider's reasoning payload verbatim at
the parse boundary (an opaque, provider-owned artifact), stores it on the turn,
and re-emits it per provider rules on the next request:

- **Anthropic** — thinking blocks (with encrypted `signature`) and
  `redacted_thinking` blocks are echoed back, complete and unmodified,
  alongside `tool_use` blocks (a missing or altered block is a 400).
- **DeepSeek / Kimi (OpenAI-compatible chat)** — `reasoning_content` is passed
  back on every assistant tool-call message when the request carries `tools`.
  On any echo-capable chat provider, a turn recorded as reasoning-only (empty
  content, no tool calls) still echoes its same-model reasoning text so the
  history never ships a wholly empty assistant message (the "must not be
  empty" 400 that previously broke a mid-session model switch, e.g. deepseek
  → kimi); models pinned to "never replay" (e.g. Cerebras gpt-oss) and the
  Responses API are left to the flag-only diagnostic instead.
- **Gemini** — the encrypted thought-step `thoughtSignature` values are sent
  back (the summary text stays display-only).
- **OpenAI / xAI Responses** — reasoning continuity is chained across user
  turns via `previous_response_id` (the server retains the reasoning items in
  the chain; a fresh chained turn sends only the new user message, and opaque
  reasoning items are re-emitted into `input` on non-chained conversions).

Display-only reasoning (providers/fields that expose no reusable payload) is
never replayed. Artifacts are model-bound: after a mid-session model switch
(`/model`), old turns' reasoning is **not** replayed — a turn produced under
the previous model never has its payload sent to the new one.

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
Turn history (conversation text, tool output, reasoning) is stored
zstd-compressed in the `session_turns` table (schema 2, pure-Rust codec); on
the first startup after upgrade the daemon re-encodes existing turns and keeps
a `state.redb.bak-v1` backup.

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

Supported providers: all entries in the provider catalog — 208 across three
wire protocols (OpenAI-compatible, Anthropic Messages, Google Generative AI).
The catalog is a two-layer pipeline in `choreo-ai-protocols/catalog/`: a
**models.dev snapshot** (`models.dev.json`, a *local, gitignored* file that
`catalog-gen` fetches from models.dev when absent, normalized into the
embedded postcard blob `catalog.bin` — the only committed catalog data file)
supplies provider/model *facts* (context windows, reasoning support and
levels, the Responses-API flag), and a bundled **`models-overlay.toml`**
policy layer
supplies everything models.dev can't express — wire-protocol selection,
endpoint policy, per-model passback exceptions, and the local/niche providers
models.dev doesn't cover (ollama, kimi-code, custom-*, …). Highlights: OpenAI,
Anthropic, Google Gemini, Mistral, DeepSeek, xAI Grok, Groq, Together AI,
OpenRouter, Hugging Face, GitHub Copilot, NVIDIA NIM, Cerebras, Fireworks AI,
Alibaba (Qwen), Moonshot AI (Kimi), Perplexity, Z.AI, Xiaomi MiMo, Qwen Token
Plan, Vercel AI Gateway, OpenCode Zen/Go, Kimi Code, Ollama (local/cloud), LM
Studio, and many regional/niche gateways. See the `catalog/` directory for the
full list. Each provider ships sensible defaults (base URL, default model) —
override any field per-account:

**Runtime refresh & user overlay.** At startup the daemon loads the base from
the cache at `$XDG_DATA_HOME/choreographr/catalog.bin` (falling back to the
embedded blob) and revalidates it against models.dev with an etag conditional
GET on a background thread (304 → keep, 200 → normalize, swap, and persist the
cache). **Refresh pacing is a 25 h attempt cooldown**: the daemon attempts a
fetch at most once per 25 h regardless of the last outcome (200/304/failure),
anchored on a wall-clock attempt timestamp persisted in the daemon DB
(`catalog_state`), recorded BEFORE each fetch — so the cadence survives
restarts, a crash mid-fetch cannot re-trigger an immediate re-fetch, and each
daemon's fetch time drifts +1 h/day to spread load across the daily cycle.
The startup fetch is **gated**: it runs immediately iff there is no valid
cache, no recorded attempt, or the attempt is stale; otherwise the daemon
skips the network hit and arms the revalidation timer for the remaining time
(`/refresh-models` bypasses the cooldown anytime). The models.dev **etag also
lives in the DB**, written only after the cache bin is on disk (crash-safe
ordering; a missing cache never sends `If-None-Match`). A **user overlay** at
`$XDG_CONFIG_HOME/choreographr/models-overlay.toml` is merged on top of the
bundled overlay with the same schema — provider scalars (`protocol`,
`base_url`, `max_tokens_field`, `default_model`, `display_name`) and per-model
entries (`[provider.<slug>.models."<model>"]` with `context_window`,
`reasoning_supported`, `reasoning_levels`, `responses`, `reasoning_passback`),
plus wholesale provider definitions for anything models.dev doesn't list. The
file is watched and reloads automatically on change (deleting it falls back to
the bundled overlay; the daemon creates the config dir at startup so the watch
installs even on a fresh system);
`/refresh-models [--force]` re-fetches the upstream catalog on demand and also
re-reads the user overlay (a burst of requests is coalesced into one fetch;
each requester's status reflects its own `--force` flag).

| Field | Description |
|---|---|
| `base_url` | API base URL |
| `streaming` | Enable/disable streaming responses |
| `stream_options` | Include usage in stream |
| `retry_max_attempts` | Max retry count on transient errors |
| `retry_initial_backoff_ms` | Initial backoff between retries (ms); must not exceed `retry_max_backoff_ms`. Capped at 3,600,000 ms (1 h) like the max — an over-ceiling value is rejected when the accounts file loads and at `accounts add`, and the library clamps it with a warning for programmatic configs |
| `retry_max_backoff_ms` | Max backoff between retries (ms). This is the Retry-After budget: a 429/503 whose `Retry-After` exceeds it fails immediately instead of retrying (see below). Capped at 3,600,000 ms (1 h) — an over-ceiling value is rejected when the accounts file loads and at `accounts add`, and the library clamps it with a one-time warning (per distinct value) for programmatic configs |
| `connect_timeout_secs` | TCP connect timeout |
| `request_timeout_secs` | HTTP request timeout |
| `total_timeout_secs` | Wall-clock deadline for a single request attempt including the streaming body (default 3600s; 0 disables). Complements `request_timeout_secs` (idle/no-progress): armed before the request is sent and re-armed on each retry, so one attempt's budget spans DNS → connect → headers → body (ureq's `timeout_global` bounds it from DNS through the first body byte, and the SSE consumer enforces the same deadline with an exact timer, so it fires even when keep-alive bytes trickle in). Expiry surfaces as a dedicated `deadline_exceeded` error. Each retry restarts the deadline, so retries + backoff can exceed it in aggregate. |
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
- `/refresh-models [--force]` — re-fetch the models.dev catalog (conditional GET against the cached etag; 304 → "models up to date"); `--force` bypasses the etag so the server must return a fresh catalog. Also re-reads the user overlay. The daemon fetches on a background thread and replies with provider/model counts; a burst of `/refresh-models` requests is coalesced into a single fetch (each requester's status reflects its own `--force` flag, and a 304 reply is ordered after any queued overlay reload so the counts are current).
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
- `Ctrl+A` — open the AI provider accounts page (list accounts; `Enter` sets the highlighted account on the active session and returns to chat, `r` removes, `c` opens the API-key modal, `n` starts the new-account wizard)
- New-account wizard (`n` on the accounts page) — centered modal windows: a **searchable provider picker** (type to filter by provider name, `↑`/`↓`/`PgUp`/`PgDn` or the mouse wheel navigate, `Enter` or a click on a row picks — the list is alphabetical), then a separate **slug modal** (enter the account's unique name, e.g. `/account <slug>`); `Enter` creates the account and jumps straight to the **API-key modal**
- `/reasoning` — show current reasoning effort slug
- `/reasoning <slug>` — set reasoning effort (e.g. `off`, `low`, `medium`, `high`, `on`, `xhigh`, `max`; available values depend on the model)
- `Ctrl+R` — cycle reasoning effort through available slugs for the attached session's model (status message states: model reports no effort levels → "model does not support reasoning"; no model selected → "no model selected — pick one with Ctrl+M"; model selected but capability not yet reported → "reasoning capability not yet available")
- `Ctrl+M` — open the model selector: list models available on the attached session's account, type to filter, `↑`/`↓`/`PgUp`/`PgDn` or the mouse wheel to navigate, click a row or press Enter to select, Esc to dismiss (requires a terminal that implements the kitty keyboard protocol — e.g. kitty, foot, wezterm, ghostty, alacritty; on other terminals Ctrl+M arrives as Enter)
- `Ctrl+Backspace` — clear the draft prompt in the input box (empties the whole draft wherever the cursor sits; `Ctrl+W` deletes the previous word and `Ctrl+U` clears only up to the cursor)
- Mouse select-to-copy — drag to select text in the history pane; on release it is copied to the system clipboard automatically (via the OSC 52 escape sequence, so it works in kitty/wezterm/ghostty/alacritty ≥0.13/Windows Terminal and over SSH/tmux, and is a silent no-op in terminals without OSC 52 support such as macOS Terminal.app), and the status line reports "Selection copied to clipboard.". Only the message text is selected — the box chrome around turns (the `┃` gutter, padding, and trailing fill) is excluded from both the highlight and the copy. Wrapped text is copied *unwrapped*: rows the renderer folded onto separate lines because the pane is narrow are re-joined into the original text (paragraph wraps regain their single space, verbatim tool output is reproduced byte-for-byte), while real line/paragraph breaks — and the blank spacer rows the renderer leaves between blocks — stay newlines, so copying a heading and its paragraph keeps the blank line between them. Selections over 1 MiB are refused with a "too large to copy" status (OSC 52 payloads are base64 and terminals cap oversized pastes). Selection starts only on plain text — clicking a reasoning/tool-result header still toggles it, a plain click without a drag copies nothing, and scrolling mid-selection — by wheel or because new content streams in — keeps the starting point pinned to the text while the selection tracks the cursor
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
cargo run --release -p choreographr -- --metrics-addr 127.0.0.1:9464
```

When `--metrics-addr` is provided, a dedicated HTTP thread serves `GET /metrics`
at the given address. Without the flag, no metrics server is started. Metrics
include session counts, connection counts, request latency, API call latency,
tool execution time, error breakdowns, and process-level metrics (RSS, CPU,
file descriptors).

Metrics are compiled in via the `metrics` cargo feature, which is
**off by default**. A plain build omits the Prometheus machinery entirely. To
enable the endpoint:

```bash
cargo run --release -p choreographr --features metrics -- --metrics-addr 127.0.0.1:9464
```

Release binaries enable `metrics` explicitly via
`scripts/release.sh`, so installed binaries keep the `/metrics` endpoint. When
a build was made without the feature, the `--metrics-addr` flag is still
accepted but the daemon refuses to start with a clear error telling you to
rebuild with `--features metrics`.

## Blockchain tools

Read-only EVM and Substrate/Polkadot queries (`evm_chain`, `evm_balance`,
`evm_token_balance`, `evm_block`, `evm_transaction`, `evm_call`, `evm_gas`,
`evm_logs`, `evm_nonce`, `evm_resolve`, `subxt_chain`, `subxt_balance`,
`subxt_query`, `subxt_block`) live in the `choreo-blockchain` crate (alloy +
subxt + the tokio sidecar runtime they need). They are compiled in via the
`blockchain` cargo feature, which is **off by default** — a plain build omits
alloy/subxt/tokio entirely. To enable:

```bash
cargo run --release -p choreographr --features blockchain
```

Once enabled, the tools are registered under the `blockchain` tool group, which
the model activates per-session with `load_tools blockchain`. In a session, they
need no credentials — they query public RPC endpoints.

## Testing & development

The workspace uses [cargo-nextest](https://nexte.st) as its primary test runner:
it executes every test in its own process, in parallel across all cores, and
gives per-test timeouts and retries. Install it once with
`cargo install cargo-nextest` (or `brew install nextest` on macOS); the aliases
below fail with "no such command" until it is on `PATH`. The unit-vs-integration
split is the same as libtest's — integration tests live in crate-level `tests/`
and are marked `#[ignore]` (see AGENTS.md):

```bash
cargo test-fast          # unit tests (nextest, parallel)
cargo test-lean          # unit tests with every optional feature off (nextest)
cargo test-integration   # integration tests — the #[ignore] suite (nextest)
cargo test-all           # everything in one pass (nextest)

cargo test                  # unit tests (libtest, serialized across binaries)
cargo test -- --ignored     # integration tests (libtest)
cargo clippy --workspace    # lints
cargo fmt --all             # formatting
```

`cargo test-lean` is the feature-off run: it compiles the workspace with every
optional feature disabled (metrics, blockchain, mimalloc), which is the only
way the metrics no-op stub backend and the feature-off `--metrics-addr` startup
refusal in `server/lifecycle.rs` get built — the `--all-features` aliases never
compile that configuration, so `test-lean` guards against the stubs drifting
out of sync with the real backend.

The nextest profile lives in `.config/nextest.toml`: `fail-fast = false` (run
the whole suite even after a failure) and a 120s `slow-timeout` that aborts any
hung test. On a 16-core machine `cargo test-all` runs the entire suite (~2,050
tests, unit + integration) in ~6s wall, versus ~22s for the two equivalent
libtest commands (`cargo test` + `cargo test -- --ignored`) on a warm build.
Nextest wins on two fronts: it parallelizes across test binaries (libtest runs
them one at a time) and runs every test in its own process. Useful raw nextest
invocations:

```bash
cargo nextest run --workspace -E 'test(ignored)'   # filterset: integration only
cargo nextest run --workspace --retries 2          # retry flaky tests
cargo nextest run --workspace --partition count:1/2   # shard for CI
```

Note that the `test-*` aliases bake in `--workspace`, so passing `-p <crate>`
to them is rejected by cargo (conflicting flags) — run
`cargo nextest run -p <crate>` directly to scope a run to a single crate.

### justfile

A [`justfile`](./justfile) wraps the common workflows above (and the daemon run
commands) in one place — `just` lists every recipe, and `just help` explains the
prerequisites. Install `just` with `cargo install just` (or `brew install just`
on macOS); the recipes require the same toolchain as the
[Build from source](#build-from-source) section (cargo ≥ 1.94.1 + zig), and
nextest only where noted:

```bash
just preflight            # verify cargo + zig (+ optional nextest) are present
just build                # cargo build --workspace (release by default)
just check                # cargo check --workspace --all-targets (fastest CI signal)
just check-macos          # macOS cross-compile gate: type-check every lib for aarch64-apple-darwin
just check-windows        # Windows cross-compile gate: type-check every lib for x86_64-pc-windows-gnu

just test                 # full suite via nextest (alias of just test-all)
just test-fast            # unit tests via nextest
just test-lean            # unit tests, every optional feature off (nextest)
just test-integration     # integration tests (the #[ignore] suite) via nextest
just test-libtest         # unit tests via libtest (no nextest required)
just test-crate choreo-proto   # a single crate via nextest

just fmt                  # cargo fmt --all
just clippy               # cargo clippy --workspace --all-targets
just check-supply-chain   # dependency gate: deny.toml bans + RustSec advisories + cache scan
just install-cargo-deny   # install the policy tool (cargo-deny) that check-supply-chain prefers
just pre-commit           # AGENTS.md gate: fmt-check + clippy + test-all + check-supply-chain
just ci                   # CI gate: fmt-check + clippy-strict + test-all + check-supply-chain

just daemon -v            # run the daemon with debug logging
just tui / gui / im / acp # run the other clients (im takes e.g. `just im telegram`)
```

`just --set profile debug build` switches the build profile (default `release`);
`CARGO_FLAGS` (env) appends flags to every cargo invocation. The nextest-backed
recipes (`test`, `test-fast`, `test-lean`, `test-integration`, `test-all`,
`test-crate`, `shard`, `retry`) fail with an install hint until `cargo-nextest`
is on `PATH`.

### Supply-chain security

The workspace depends on `arrayref` (pinned at `0.3.9`) transitively — through
`tiny-skia` → `usvg`/`resvg` for SVG rendering in the daemon and TUI, and
`blake2b_simd` → `subxt` (blockchain feature). On 2026-08-20 the `arrayref`
maintainer's crates.io account was compromised and `arrayref@0.3.10` was
republished with a dependency on payload-downloading build scripts (RUSTSEC-2026-0260,
[Rust blog](https://blog.rust-lang.org/2026/08/20/supply-chain-attack-on-arrayref/));
it was live for ~86 minutes before deletion. The defenses below make that class
of attack fail loudly instead of landing silently:

- **Committed `Cargo.lock`** with package checksums is the first line: builds
  resolve exactly what the lockfile pins. `just test-all`, `just clippy`, and
  `scripts/release.sh` all pass `--locked`, so the committed lockfile is
  authoritative and a silent regeneration fails instead of re-resolving against
  the live registry.
- **`deny.toml`** (enforced by `cargo-deny` via `just check-supply-chain`)
  hard-bans every version from the 2026-08-20 attack (`arrayref@0.3.10`,
  `internment@0.8.7`, `append-only-vec@0.1.9`, and the six deleted payload
  crates `proc-macro1`/`proc-macro-en`/`aovine`/`arone`/`aronenao`/`tinymember`
  by name), fails on any RustSec vulnerability or "malicious" advisory, and
  restricts all sources to crates.io.
- **`scripts/check-supply-chain.sh`** runs three layers: a scan of the local
  `~/.cargo/registry` cache for the deleted malicious `.crate` files (the Rust
  blog's own remediation `find` — neither cargo-deny nor cargo-audit can see
  idle cache files), then `cargo-deny` (preferred) or, as a fallback,
  `cargo-audit` plus a literal lockfile scan. It is part of `just pre-commit`
  and `just ci`.

RustSec advisories for the attack (`RUSTSEC-2026-0260` and friends) are in the
advisory database cargo-deny/cargo-audit fetch automatically, so any future
introduction of a flagged crate also fails the gate. For the strongest — and
heaviest — hardening, `cargo vendor` + a `[source]` replacement in
`.cargo/config.toml` would make builds bit-for-bit reproducible from a checked-in
dependency snapshot; it is intentionally not enabled (repo size).

## Packaging & releases

Release tooling lives in [`scripts/`](./scripts) and the packaging assets it
consumes in [`packaging/`](./packaging) — see `packaging/README.md` for the
per-asset breakdown. The end-to-end runbook for cutting a release (crates.io
publish, both build machines, GitHub release, Homebrew/AUR/choreographr.com
updates) is [`RELEASE.md`](./RELEASE.md). The one-command flow is:

```bash
just release                # dry-run: build, tarball, SHA256SUMS, .deb/.rpm (never uploads)
just release-upload         # also run `gh release create`
just release-allow-dirty    # dry-run from a dirty tree (staged-but-uncommitted changes)
just release-tap            # dry-run: bump the Homebrew tap formula from dist/ (never pushes)
just release-tap -- --push  # commit + push the tap bump to choreographr/homebrew-choreographr
just smoke-test             # validate the tarball `just release` just built
just package-deb / package-rpm   # rebuild only the .deb / .rpm from existing artifacts
just install                # run the pinned-version installer locally (not via curl|sh)
```

Equivalently, invoke the scripts directly:

```bash
scripts/release.sh                 # dry-run: build, tarball, SHA256SUMS, .deb/.rpm
scripts/release.sh --upload        # also run `gh release create` (never uploads by default)
scripts/update-homebrew-tap.sh     # dry-run: bump the tap formula (never pushes)
scripts/update-homebrew-tap.sh --push   # commit + push to choreographr/homebrew-choreographr
scripts/smoke-test.sh dist/choreographr-0.1.0-x86_64-unknown-linux-musl.tar.gz
```

Prebuilt installs (no Rust toolchain needed) use `scripts/install.sh`, which
pins the version and verifies a SHA-256 checksum before extracting the four
binaries (`choreographr choreo-tui choreo-im choreo-acp` — `choreo-mcp` is a
library-only crate and ships no binary). The systemd unit / launchd agent is
installed but **never auto-enabled** — the daemon is a user service and
starting it is an explicit choice (`systemctl --user enable --now
choreographr`).

## Troubleshooting

- `choreo-tui` writes its diagnostics to `/tmp/choreo-tui.log` — check there
  for client-side issues.
- The daemon logs to stderr; use `-v`/`-vv` for more detail, or set
  `RUST_LOG`.

## License

[Apache License 2.0](LICENSE)
