# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Changed

- **Bin-to-crate relocation (part 1):** the thin binary wrappers moved out of
  the root `choreographr` package into their own crates — `choreo-tui`,
  `choreo-im`, and `choreo-acp` now each declare their `[[bin]]`
  (`src/main.rs`, a thin wrapper calling the library's `main()`) plus their
  own `mimalloc` feature/optional dependency (mimalloc cannot live in
  `[workspace.dependencies]` because cargo rejects `optional` there — each
  crate declares it inline). The root package declares ONLY the daemon
  binary: the `im`/`acp` features, the optional bridge dependencies, and the
  root `avif` → `choreo-tui/avif` forwarding were removed (the TUI's `avif`
  feature stays on the `choreo-tui` package). Source builds of the bridges
  are now `cargo build -p choreo-im` / `cargo build -p choreo-acp`, and the
  full crates.io source install is `cargo install choreographr choreo-tui
  choreo-im choreo-acp`. `just tui/im/acp` recipes updated accordingly
  (`just im`/`just acp` also dropped `_require-zig` — those crates never
  touch zlob).
- Workspace `default-members = [".", "choreo-tui"]` added: a bare `cargo
  build` at the root keeps producing the daemon + TUI exactly as before the
  split, while choreo-gui (Blitz/wgpu-heavy) stays out of default builds.
- **Release pipeline for the split binaries (part 2):** the release build
  now selects BOTH owning packages (`-p choreographr -p choreo-tui`) with
  package-scoped feature syntax (`--features
  choreographr/metrics,choreographr/blockchain[,choreographr/mimalloc,choreo-tui/mimalloc]`),
  in `scripts/release.sh`, the CI `windows-msvc` job, and
  `scripts/build-android.sh` (whose `--features` list is prefixed per-item
  with `choreographr/` for two-package unambiguity). Both bins still land in
  the shared `target/<triple>/dist` profile dir, so tarball/.deb/.rpm/Termux
  staging, smoke tests, and `install.sh` are unchanged. `choreo-tui` gained
  an identical `[package.metadata.binstall]` block so `cargo binstall
  choreo-tui` resolves the same single tarball asset and extracts just its
  own binary.

### Added

- iOS-native-tool C-ABI bridge skeleton (Subsession 1 of the iOS tools
  plan; Subsession 2 added tool registration): `choreo_daemon::tools::ios_bridge` defines
  the unconditionally-compiled `IosToolBridge` trait, the
  `IosToolRequest`/`ToolBridgeReply` envelope, the serializable
  `ToolBridgeError` (BridgeUnavailable/Canceled/Timeout/Platform), the
  `IosToolPending` handle (deadline-bounded `wait` with cancellation polling
  + cancel-precedence, best-effort `cancel()`), named per-tool timeout
  constants (clipboard 1500ms / open_url 3000ms / notify 5000ms), and a
  scripted `MockBridge`; the concrete `SwiftIosToolBridge` lives in
  `choreo-gui/src/ios_bridge.rs` behind `#[cfg(target_os = "ios")]` (extern
  "C" declarations for the Swift host plus the exported
  `choreo_ios_tool_reply` callback that reconstructs the boxed one-shot reply
  sender — reply-slot ownership contract documented verbatim in the module
  header: Rust never frees the box; Swift guarantees exactly-once reply on
  the main queue; an abandoned request's late reply sends into a
  disconnected channel and drops the slot), and `ios/IosToolHost.swift` is
  the main-queue-serialized Swift host (UIPasteboard clipboard
  write/read, https:/mailto:-restricted `open_url`, UNUserNotificationCenter
  `notify` with lazily-requested provisional authorization; the Swift file
  is not compiled in CI — its errors surface on a Mac; the zig path of
  `scripts/build-ios.sh` validates the Rust cfg(ios) code).
- iOS-native tools themselves (Subsession 2 of the iOS tools plan): the four
  `Tool` wrappers — `clipboard_write`, `clipboard_read`, `open_url`, `notify`
  — live in `choreo-daemon/src/tools/ios/` (compiled unconditionally; no cfg
  anywhere; the bridge's presence is the only gate). All are Direct-only
  (exfiltration-chain mitigation), entry-check `ToolContext.cancelled` before
  dispatching (a cancelled call never touches the bridge), wait with the
  cancel-precedence `IosToolPending::wait` and per-tool timeout constants,
  and best-effort `pending.cancel()` on the late-cancel path. `open_url`
  advertises an `https|mailto`-pattern schema and re-validates in `execute`
  (scheme allow-list + control-character ban; schema is advisory, the
  executor is the boundary); `notify` enforces 200/2000-character
  title/body caps in `execute`. `ToolRegistry::register_platform_tools`
  registers them under a new PROTECTED `"ios"` group; `OpenOptions`
  gained `platform_tool_bridge: Option<Arc<dyn IosToolBridge>>` and the
  iOS GUI passes its Swift bridge there (desktop passes `None` and the
  group never exists). Protected groups: always unioned into
  `available_definitions` (so pre-existing persisted sessions get the ios
  tools too), excluded from `group_names()` (never offered to
  load_tools/unload_tools schemas), and honored by `apply_unload_tools`,
  which now takes the registry's protected set instead of hardcoding
  "core". The default group lists (`daemon.rs` CreateSession,
  `default_active_tool_groups`) push "ios" behind `cfg(target_os = "ios")`
  as belt-and-suspenders for display honesty. The iOS GUI's bridge hand-off
  is gated by a new user setting, **on-device tools** (default ON — all four
  tools are permission-free): choreo-gui gains its first settings store
  (`src/settings.rs`, `gui-settings.toml` in the shared config dir via
  `choreo_keystore::paths::config_dir()` — not the daemon DB, since the
  bridge decision happens at `DaemonState::open` before any daemon exists;
  tolerant load — missing/corrupt file falls back to defaults — and
  whole-file persist), an iOS-only toolbar toggle that persists the flag
  and states that a change takes effect on the next app start (the
  protected group cannot be re-registered live), and host unit tests for
  the persistence round-trip. Desktop/Android behavior is unchanged — the
  setting surface compiles but is consumed only under
  `cfg(target_os = "ios")`, and the desktop GUI toolbar renders an empty
  placeholder component.
- `powershell` shell tool for Windows: executes commands via Windows
  PowerShell 5.1 (`powershell.exe`, always present) or PowerShell 7+ (`pwsh`)
  with the same timeout-watchdog/streaming plumbing as the other shell tools.
  Invocations run `-NoProfile -NonInteractive -EncodedCommand` (Base64
  UTF-16LE, so LLM-generated quoting never needs escaping), with a preamble
  forcing `[Console]::OutputEncoding` to UTF-8 so redirected output is UTF-8
  instead of the console code page (and `$ProgressPreference` silenced).
  Registered only on Windows, and only when a PowerShell binary is on PATH.

### Fixed

- ConfigWatcher resends directory state after an inotify queue overflow, so
  subscribers no longer miss changes dropped by the kernel under load (fixes
  the flaky config_watch integration tests): `notify` surfaces `IN_Q_OVERFLOW`
  as an event carrying `Flag::Rescan`, and on that signal (and on watch
  read-errors, which can equally mean missed events) the transport thread now
  rescans the watched directory and replays divergences from a per-basename
  last-known-content view as synthesized Create/Modify/Remove through the
  same subscriber routing as real events.

- `binary_exists` (the registration-time PATH probe behind conditional tool
  registration) now resolves Windows executables through PATHEXT: a bare
  `nu`/`pwsh` is really `nu.exe`/`pwsh.exe`, so the old exact-name probe
  missed every installed binary on Windows. Unix behavior is unchanged.
- `IosToolPending::wait`'s `Duration`-overflow fallback no longer converts an
  intended-indefinite deadline into an instant `Timeout` (the old
  `unwrap_or_else(Instant::now)` produced an already-elapsed deadline); the
  overflowed case now means "wait until the reply or a cancel", which matches
  the intent of an unbounded wait.
- The `cancel_wins_over_arrived_reply` bridge test genuinely exercises the
  POST-reply cancel re-check now: the old version flipped the flag BEFORE
  `wait` with a `Duration::ZERO` deadline, so it only pinned the pre-wait
  check and never reached the reply arm it claimed to cover. The predicate
  now flips the flag on its second invocation (the post-reply re-check), and
  the per-tool cancel-race tests share one deterministic
  `cancel_race_fixture`.
- `toggle_on_device_tools` (choreo-gui) now LOAD-modifies-persists the
  settings file instead of rewriting it from a fresh struct — a whole-file
  rewrite from scratch would have silently reset every OTHER preference to
  its default the moment a second field exists.

### Changed

- iOS platform tools hardening/streamlining: `open_url`'s executor-side
  validation now parses the URL with the `url` crate (scheme allow-list over
  a real parse — a bare prefix check accepted degenerate strings like
  `https:not-a-url`) before the control-character ban; `notify`'s schema
  advertises `maxLength` (200/2000) mirroring the executor caps; `open_url`
  and `notify` surface the host's verdict (`{"opened": Bool}` /
  `{"scheduled": Bool}`) instead of always reporting success; and
  `run_bridge_tool` is the single serialization point (`&impl Serialize`),
  removing the per-tool pre-encode + re-encode dance. iOS behavior for the
  success paths is unchanged; declined opens/schedules now report as such.

- `itertools` adopted (declared in `[workspace.dependencies]`, consumed by
  `choreo-daemon` and `choreo-ai-protocols`) alongside a new AGENTS.md
  dependency-management rule: use it where it genuinely improves code
  quality (`.format()` joins, `.sorted()`, `process_results`), never for its
  own sake. Applied at the concrete sites that warranted it: the grep tool's
  `describe_invocation` (a chain of conditional clauses instead of a manual
  `parts` vec), the three output renderers (`flat_map` + `.format("\n")`
  replacing buffered collect-join, `sorted()` folding the sort into the
  chain), `handle_evict_largest_lagging` (hand-rolled max accumulator →
  `max_by_key`), the Anthropic/Google system-message joining (multi-message
  newline joining via `.format("\n")` instead of push-with-separator
  bookkeeping), and Google system texts kept borrowed instead of cloned.
  Behavior-preserving; existing tests pin every output shape.
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

### Added

- Review follow-ups for the embedded-daemon series: (1) the CI `ios aarch64`
  release job now installs zig like the other jobs (zlob, via choreo-daemon,
  compiles its Zig source with `zig cc` in build.rs on every host — the job
  previously only claimed to, commit 40fada9 changed a comment); (2) the
  cc/cxx shim generators duplicated byte-identically in `build-ios.sh` and
  `check-ios.sh` are extracted to the shared `scripts/lib/ios-cc-shims.sh`
  (one generator so they cannot drift; generated files are stamped with the
  current generator's name), with the CLI executor line emitted via `printf`
  so the runtime arg-forwarding expression cannot be corrupted by heredoc
  escaping (the generated shims are byte-identical to the previous output);
  (3) a consumer notice on the workspace `choreo-daemon` dependency:
  `default-features = false` lives there, so a NEW consumer must re-enable
  `features = ["pdf"]` or it silently gets a PDF-less daemon.

### Fixed

- Embedded daemon teardown leaks on the embedder side: the GUI's
  `EMBEDDED_DAEMON` static is now `Mutex<Option<EmbeddedDaemon>>` instead of
  a `OnceLock<Mutex<…>>`, so a (never-expected but reachable) double-startup
  drains the stale daemon through its ordered `shutdown()` instead of
  leaking both daemons detached; and a `connect()` failure after
  `spawn_embedded` succeeded now runs the ordered drain instead of dropping
  the running core detached (no `ShuttingDown` broadcast, no bounded joins).
- Channel-selection convention applied to the new code of the same series:
  `EmbeddedDaemon`'s JoinHandle ferry and the in-process pump's internal
  writer-shutdown channel now use `crossbeam_channel` (the
  `DaemonState::daemon_tx` field and the pre-existing `from_ui` public
  signature stay std `mpsc`, converted opportunistically only).
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
