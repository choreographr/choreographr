# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- Channel-selection convention in [AGENTS.md](./AGENTS.md) (Thread
  Communication section): thread-to-thread messaging in the crates that
  already depend on it (`choreo-daemon`, `choreo-tui`, `choreo-client-core`,
  `choreo-ai-protocols`) must use `crossbeam_channel` rather than
  `std::sync::mpsc` — cloneable receivers, `select!`/`select_biased!`
  (including send arms and timer channels), and a consistent error taxonomy
  keep future evolution a one-line change. Existing std `mpsc` converts
  opportunistically (only when the change would strain single-consumer
  semantics or needs select/cloned-receiver support); one-shot reply/flag
  channels and test-site constructions are left alone; leaf crates
  (`choreo-acp`, `choreo-gui`, `choreo-im`, `choreo-mcp`,
  `choreo-transport`) stay on std `mpsc` for their trivial single-consumer
  fan-in/out; async code keeps the runtime's own channels and never calls a
  blocking `recv()` inside an async task. Also records the crossfire
  evaluation outcome: crossbeam stays; crossfire is rejected codebase-wide
  (its select layer has no send arms or timer channels, it targets saturated
  throughput rather than our human-rate control-plane traffic, and its own
  README flags memory-ordering bugs on weaker-ordering platforms such as the
  aarch64 Termux/Android targets `choreo-tui` supports).

- iOS GUI runs an embedded in-process daemon (step 5 of the embedded-daemon
  refactor, final step): on `target_os = "ios"` and with no `--tcp-addr`
  override, `choreo-gui` now opens `DaemonState` via `DaemonState::open` under
  `ToolPolicy::Mobile` (no shell/exec/RISC-V tools, no MCP subprocess
  spawning — sandbox-safe), spawns it with `choreo_daemon::spawn_embedded`, and
  connects to mint an `EmbeddedLink` whose channel ends become
  `ConnectionMode::InProcess` — messages travel as values, no codec, no
  socket. The whole construction is `#[cfg(target_os = "ios")]` and the
  choreo-daemon dependency is target-gated in choreo-gui's Cargo.toml, so
  desktop and Android builds never compile or link any of it and desktop
  behavior is byte-for-byte unchanged (UnixSocket default). Every
  construction failure is logged and degrades to the previous `TcpPinned`
  remote-daemon fallback so the app still launches. The `EmbeddedDaemon`
  handle is kept in a static `OnceLock<Mutex<…>>`; the Dioxus Native
  lifecycle has no daemon-shutdown hook, so shutdown happens at process
  teardown (the `Drop` warn in `embedded.rs` is expected there) — no polling,
  no background threads.

- `ConnectionMode::InProcess` (step 4 of the embedded-daemon refactor):
  `choreo-client-core` gains an in-process connection mode carrying the raw
  crossbeam channel ends of an embedded daemon's `EmbeddedLink` (as values —
  client-core takes NO dependency on choreo-daemon). The pump mirrors the
  Noise/TCP structure exactly: the calling thread drains `daemon_rx` into
  `handle_daemon_message` (channel close = clean EOF, so the GUI's
  `UiEvent::ReaderClosed` behaves identically), and a dedicated writer thread
  forwards `from_ui` into `daemon_tx` (same `recv_timeout` + shutdown-flag
  structure as the socket modes; closing `from_ui` delivers EOF to the
  daemon's embedded connection). In-process shutdown is cooperative: the
  external shutdown signal stops the writer only — the reader ends when the
  embedded daemon closes its channel. `ConnectionMode` switched to a manual
  `Debug` impl (channel ends are not `Debug`) that reproduces the derived
  output for the socket variants and renders `InProcess(<embedded link>)`.
- Embedded daemon transport (step 3 of the embedded-daemon refactor): the new
  `choreo_daemon::embedded` module spawns the daemon core in-process
  (`spawn_embedded`) and connects clients over plain channels — `ClientMessage`
  and `DaemonMessage` values travel GUI→daemon and daemon→GUI with NO
  serialization, no crypto, and no polling; channel close is the EOF. The same
  `ClientConn` state machine, lag accounting, eviction, and the
  notify-before-close shutdown broadcast are reused unchanged (`EmbeddedDaemon`
  delivers `ShuttingDown` as a value, then closes the channel). Includes
  `DaemonState::open(OpenOptions)` (explicit DB/accounts/catalog paths + tool
  policy, replacing the CLI's inline construction) and a registration-time
  `ToolPolicy` (`Full`/`Mobile`) where `Mobile` never registers shell/exec,
  RISC-V, or MCP tool groups.
- Vision input: `read_image` tool feeding per-provider image parts; image
  bytes persisted durably via a `session_attachments` table.
- Image format surface: HEIC + SVG support, EXIF orientation baked in,
  AVIF behind a feature gate; image decoding consolidated into the new
  `choreo-image` crate.
- `retrieve_webpage` tool rendering pages with a local headless browser,
  with true full-page capture, correct element-scoped screenshots (below-the-fold
  elements now render instead of blank background), and `file://` URL support.
- `session_inspect` read-only diagnostic tool (debug tool group).
- Web-fetch fallback order documented in the default system prompt;
  brotli response decompression in `ureq`.
- LaTeX math rendered as pretty Unicode in markdown and the TUI.
- New `choreo-sanitize` crate hardening tool output across all tools.
- Shell tool: live stderr streaming; child-process waits moved from polling
  to channels with a bounded drain completion grace; shell tools can raise
  the outer deadline above the 300 s floor.
- models.dev provider catalog with overlay support and runtime refresh,
  persisted in the DB with a 25-hour attempt cooldown.
- Data-driven model facts in the catalog: `reasoning_content`,
  `max_output_tokens`, `supports_temperature`, `deprecated`.
- Background model prefetch: model lists warmed on session join instead of
  at unlock, never blocking ListModels.
- Per-session opencode gateway routing headers and a `choreographr`
  User-Agent on inference requests.
- Retry handling driven strictly by HTTP status and `Retry-After`, with a
  three-layer bounded retry budget.
- Live config watching: `accounts.toml` edits hot-rebuild providers.
- New `choreo-blockchain` crate with EVM/Polkadot tools behind the
  `blockchain` feature; real ENS resolution, `evm_call` block-tag support,
  RPC timeouts, single WebSocket connection.
- Choreographr Coordination Platform: new `choreo-content` crate (initially
  `choreo-coord`) behind the default-disabled `content` feature, with an
  orchestration layer composing the chain/indexer/IPFS pipelines; Substrate
  (Polkadot account) credential type and Polkadot-JS keyring import;
  `coord` tool group wired into the daemon; `coord_image` tool;
  `coord_status` reporting live chain health (best + finalized blocks).
- Keystore: per-daemon keystore unlock keys with hardened per-daemon state;
  unlock UX reworked (`/unlock` uses the stored key, `/unlock <key>` records
  it); `BindKeystore` as the sole binding path with frontend auto-bind
  (TUI/GUI/IM bind fresh unbound daemons and confirm on Bound).
- Client trust model: Noise XX first-contact mode with TCP wire v5 handshake
  preamble, fingerprint rendering and a `known_servers.toml` pin store,
  pinned-mode confirmation flow, hot-reloaded client ACL from
  `authorized_clients.toml`, `/acl add` enrollment, and the
  `choreographr acl-add` / `fingerprint` subcommands. The TUI refuses to
  start against an untrusted daemon and never crashes on connection errors.
- zstd compression of `session_turns` values via a schema 1→2 migration.
- Lossless streaming delivery: unbounded per-client queues with lag-based
  eviction (protocol v3: `TurnFinalized` removed, `Evicted` added).
- Noise transport hardening: message fragmentation with a validated length
  prefix and reassembly cap, single-writer guard, absolute handshake
  deadline, accept-time writer registration, and a concurrent-connection cap
  with `ConnectionSlot` RAII accounting.
- Default Unix socket moved under the platform temp dir, with the path named
  in bind errors; ACP adapter log file likewise, uncreatable log never fatal.
- Ctrl+C daemon shutdown path instrumented stage-by-stage with SIGINT
  regression tests.
- TUI: modal account wizard with searchable provider picker; mouse
  select-to-copy in the history pane (OSC 52); picker click-to-select with
  mouse wheel and pin-at-middle navigation; account/session rows
  click-to-enter; per-session unsent input drafts; Ctrl+Backspace clears the
  draft; model selector rebound to Ctrl+O for legacy terminals; opt-in
  side-by-side diff rendering via `diff` fences (extended to `git_add`
  output); `write_file` results rendered as markdown; request failures
  reported in the UI with a wrapped error block; exit-on-eviction/shutdown
  handling; TUI log written to the platform temp dir.
- Choreographr Coordination Platform TUI: Polkadot-account import wizard
  (`p` key).
- Build & release: Android build target (Dioxus Native GUI + Termux suite
  binaries) with automated environment discovery and setup docs; Windows
  support (`x86_64-pc-windows-gnu`); iOS support groundwork for `choreo-gui`
  (per-SDK device/simulator staging, self-contained staticlib link); a
  Termux-native `.deb` as the fifth release artifact; GitHub Actions release
  pipeline building all platforms with per-target binary-execution smoke
  tests (qemu Termux rootfs, hermetic daemon smoke on desktop artifacts)
  and an iOS build/link gate; crates.io publish path; nightly-by-default
  builds with stable via `build-stable.sh`; dedicated fat-LTO
  `[profile.dist]` shipped-artifact profile with per-target CPU floors;
  `choreo-im`, `choreo-acp`, and `choreo-mcp` feature-gated off by default.

### Changed

- Connection keying for the in-process mode: `choreo-gui`'s
  `connection_addr()` keys an embedded daemon's keystore binding under the
  distinct stable string `"embedded"` instead of the unix socket path — a
  real unix daemon's binding lives under `socket_path()`, and the embedded
  daemon must never collide with it. The UI's display path for
  `ConnectionMode::InProcess` shows the label "embedded daemon".
- `choreo-daemon`'s `pdf` tool group (pdf_classify / pdf_to_markdown) moved
  behind a new `pdf` cargo feature that is in `default`, so desktop builds
  are unchanged; the iOS GUI build opts out (`default-features = false`)
  because pdf-inspector's build script links a C dylib for the Apple target,
  which the Linux compile-validation shim path cannot perform, and a mobile
  daemon has no use for a desktop PDF parser.
- `choreo-daemon` promoted to `[workspace.dependencies]` (now consumed by the
  root crate and — target-gated — choreo-gui).
- `scripts/build-ios.sh` / `scripts/check-ios.sh`: the zig `cc` shims now
  translate clang/rust-style target triples to zig's form (instead of
  stripping them) and are also put on `PATH`, because the choreo-daemon
  dependency tree brings build scripts that compile C for the HOST (ring via
  headless_chrome's build deps) and link Apple dylibs (pdf-inspector) during
  an iOS build — the host triple `--target=x86_64-unknown-linux-gnu` is
  unparseable to zig and a bare `cc` from PATH bypassed the shim entirely.
  Apple-iOS compiles AND build-script dylib links are rewritten to zig's
  macOS target (compile-validation fidelity; the final Apple link happens on
  the Mac). Staging remains the single self-contained staticlib — new C
  dependencies are folded in by rustc automatically, so no per-library
  staging list exists.

- Refactor(daemon): split `run_server` (step 2 of the embedded-daemon
  refactor) into a transport-independent `start_daemon_core` in the new
  `server/core.rs` (command channel, ACL install + watcher, config watchers,
  catalog-maintenance thread, shutdown flag, live-connection counter,
  command-loop thread, returned as a `DaemonCore` bundle) plus the transport
  adapters (bind, signal threads, metrics, accept loops, shutdown drain)
  which stay in `server/lifecycle.rs` over `&DaemonCore`. `CoreOptions`
  plumbs `acl: Option<_>` and `config_watchers: bool` for the future
  embedded daemon; the shipped binary passes `Some(acl)` / `true`, so
  behavior is unchanged.
- Refactor(daemon): extracted the per-connection protocol state machine into
  a transport-agnostic `ClientConn` (owns sink, lag counter, attachment
  state, and writer-thread join handle with `dispatch`/`finish`); the Unix
  and TCP/Noise read loops now differ only in transport read and error
  classification. Behavior-preserving.
- TUI status bar shows the attached session's account slug instead of the
  inference provider slug.
- Protocol rework: `DaemonMessage` split into a `SessionEvent` bus behind a
  `session_id` envelope (`Option<u64>`, replacing the `session_id: 0`
  sentinel) with explicit broadcast-origin provenance.
- Provider catalog facts (model list, reasoning support, etc.) are now
  data-driven; stale glm-5.3-flash context window corrected (200k → 1M).
- `session_turns` storage moved to the compressed schema 2 (zstd, pure-Rust
  `structured-zstd` crate replacing the C zstd binding).
- Default sockets and log files now live under the platform temp dir instead
  of hard-coded `/tmp` paths (TUI log, daemon socket, ACP adapter log).
- Dependency/MSRV: workspace crates refreshed, MSRV raised to 1.94.1 and
  decoupled from dependency resolution; release workflow actions bumped
  past the Node.js 20 deprecation; Android Termux `.deb` xz-compressed for
  Termux's dpkg and installed at the real `$PREFIX`; desktop `.deb` forced
  to xz as well.

### Removed

- `identity.pk.enc` file — the unlock key is stored in the keystore
  (`/unlock` uses it; `/unlock <key>` records it); rejected-unlock-key
  revert semantics replaced by survivor semantics.

### Fixed

- Release workflow: the manifest rejected `choreo-gui`'s `choreo-daemon = { workspace = true, default-features = false }` on stable cargo (a workspace member cannot override `default-features` of an inherited dependency; nightly cargo tolerates it, which is why local builds passed). `default-features = false` now lives on the workspace dependency definition itself, and the root package explicitly re-enables the default `pdf` feature (`choreo-daemon = { workspace = true, features = ["pdf"] }`); choreo-gui inherits the feature-less default, keeping the iOS build C-dylib-free.
- iOS bootstrap launch ordering (resolves the PHASE 0B event-loop-handshake
  caveat): the Xcode host bootstrap no longer calls `UIApplicationMain` from
  a custom `UIApplicationDelegate` — winit 0.30's `EventLoop::run_app` calls
  `UIApplicationMain` itself and asserts `sharedApplication` is still nil, so
  the old ordering (Rust event loop started from
  `application:didFinishLaunchingWithOptions:`) would have died at launch.
  `ios/main.m` now hands control straight to the `choreo_gui_ios_main`
  trampoline; contract documented in `main.m` and `choreo-gui/src/lib.rs`.
- Retry: hand-built configs hardened, validation gap closed, retry budget
  bounded against pathological configurations.
- Empty assistant messages are never shipped after a model switch
  (fallback generalized beyond DeepSeek/Kimi); empty `reasoning_content` is
  injected on DeepSeek/Kimi chat assistant turns.
- Per-provider error decoding: Responses API `response.failed` object
  decoded, provider JSON error envelopes unwrapped, duplicate request
  errors deduplicated; rate-limit status carried in `RateLimited` detail.
- Mid-turn token-usage sync keeps streaming results and the scrollbar in
  lockstep; token-usage merge policy hardened with a bounded turn-version map.
- Transport: broadcast-origin tripwire gap closed, lifecycle broadcasts
  delivered to all-activity subscribers, lag-byte accounting balanced on
  every path, `approx_wire_size` a true over-estimate, tool-stream
  abort-disconnect truncation fixed, peer close classified as
  `ConnectionClosed`.
- Shell streaming: byte-identical truncated records, bounded line memory,
  escape-before-budget, char-boundary flushes, footer-safe finish path,
  one-shot VM truncation, bounded HTTP bodies, CRLF folding.
- TUI: copy selection preserves blank lines and copies unwrapped text;
  selection stays anchored to the text and tracks the cursor while
  scrolling; highlight visible on shaded turns; selection clamped to
  per-line content ranges; side-by-side diff panes pinned to exact width;
  diff fences hardened against early-close; tabs expanded to 4-column
  stops; plain-text tool output wrapped at content width; `git_show`
  commit/tag messages emitted unindented and fenced, directory entries
  skipped in commit diffs; "Running command:" line shown reliably during
  streaming; list-click hit-test clamped to drawn rows; pre-migration DB
  backup taken before redb locks the file.
- Vision: decode limits aligned across providers; HEIC grid canvas bounds
  verified via iloc/grid parsing; raster dimension probe guarded.
- Coordination platform: revision resolution filtered to the item's own
  events; indexer key wire format and camelCase event decoding corrected.
- Windows: MSVC link failure fixed (windows-sys `WaitForSingleObject`);
  bionic TLS-alignment abort on Android fixed (.tdata/.tbss aligned to 64
  at link); BSD sed compatibility in `build-stable.sh`.

### Security

- Dependency supply chain hardened against the arrayref attack.
- Tool-output safety: six output-sanitization gaps closed across the tool
  suite; shell-tool spawning hardened; streaming bounded across all tools.
- Keystore: secrets zeroized; credential modal, keystore auto-bind, and
  daemon lock-state handling hardened.
- Transport: fragment reassembly capped and continuation authenticated;
  handshake hardened to an absolute deadline; concurrent connections
  capped; writer-loop joins bounded.
- Trust: client fingerprint comparison tightened with pinned-mode failure
  UX; enrollment & transport trust model documented in ARCHITECTURE.md.

## [0.1.0]

Initial release: daemon, TUI, protocol, Noise transport, provider catalog,
markdown rendering, PDF tooling, and the 14-crate crates.io suite.

[Unreleased]: https://github.com/choreographr/choreographr/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/choreographr/choreographr/releases/tag/v0.1.0
