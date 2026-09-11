# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- New `choreo-power-events` leaf crate: platform suspend/resume notifications
  as crossbeam channel events for the daemon's command loop. `PowerMonitor`
  spawns a dedicated, daemon-like monitor thread and exposes
  `SuspendEvent::{Sleep, Wake}` on `events()` (Sleep sent BEFORE the machine
  sleeps, Wake after resume). Linux subscribes to systemd-logind's
  `PrepareForSleep` via zbus 5.19's blocking façade (`blocking-api` +
  `async-io` features — no tokio; the async→sync bridge lives entirely on
  the monitor thread); macOS uses `IORegisterForSystemPower` + CFRunLoop and
  always acknowledges sleep with `IOAllowPowerChange` (declining would block
  system sleep); Windows/other platforms and best-effort fallbacks get an
  inert monitor (`is_active() == false`, never-firing receiver). Events are
  explicitly best-effort — kernel-level dead-link detection (sockreg TCP
  keepalives) remains the correctness fallback. `PowerMonitor::new` is the
  strict constructor, `best_effort()` logs once and falls back, `inert()`
  builds the no-op monitor directly.
- New `choreo-sockreg` leaf crate: a cloneable `SocketRegistry` that tracks
  live sockets (by duplicate fd) so a control thread can `shutdown_all()` them
  out from under blocked readers, `prune_dead()` liveness probing (the same
  non-blocking peek technique ureq's socket transport uses), `SocketTuning`
  TCP keepalive options (idle 45s / interval 10s / 5 probes / Linux
  `TCP_USER_TIMEOUT` 30s) applied after connect, and — behind the optional
  `ureq` feature — a `RegisteringTcpConnector` for `Agent::with_parts` that
  dials TCP, tunes keepalives, and registers every connection it opens. Real
  functionality is Unix-only via `nix` (socket/net/fs features); Windows
  compiles with logged no-ops (Winsock shutdown is a planned follow-up).
- Daemon power-event handling: `DaemonState` handles a new
  `DaemonCommand::PowerEvent(SuspendEvent)` — `start_daemon_core` builds a
  `PowerMonitor::best_effort()` and a forwarder thread translates each
  suspend/wake event into that command; on `Sleep` the command loop
  force-closes every session's socket registry plus the daemon's own
  ("machine sleeping: force-closing N provider sockets"), on `Wake` it
  prunes dead entries via `SocketRegistry::prune_dead` (defense-in-depth
  for a missed `Sleep` — normally a no-op since a well-delivered Sleep
  already emptied every registry; live connections are never disturbed in
  either arm).
- Forced socket close on cancel: user cancellation
  (`handle_cancel_request` in the daemon command loop) force-closes the
  TARGET SESSION's socket registry — and, via `cancel_children_of`, every
  child's — with an info log ("request cancelled: force-closing provider
  sockets to unblock any wedged reader"); other sessions' connections are
  untouched. Organic provider IO errors deliberately do NOT trigger a
  shutdown. Pinned by new unit tests (cancel isolation between sessions,
  parent-cancel cascade, sleep force-closes, wake untouched) and the
  `#[ignore]` integration test `tests/cancel_force_close.rs` (a stalling
  SSE provider + real daemon: mid-stream cancel finishes promptly via the
  registry force-close).

### Fixed

- Request cancellation no longer force-closes provider sockets belonging to
  unrelated concurrent sessions: each session owns a private
  `SocketRegistry` (plus one daemon-level registry for prefetch/catalog
  fetches), so `shutdown_all` from a cancel is scoped to exactly the
  cancelled session and its children.

- **Windows `SocketRegistry` handle leak**: the Windows `close_logged`
  variant called `into_raw_socket()`, transferring the `SOCKET` OUT of the
  `OwnedSocket` without ever calling `closesocket` — every RAII unregister /
  `shutdown_all` / prune on Windows leaked a socket handle. It now logs the
  handle from a borrow (`as_raw_socket`) and lets `OwnedSocket`'s `Drop`
  close it exactly once (verified with a `cargo check
  --target x86_64-pc-windows-msvc`).
- **`AccountsReload` over-invalidation**: the external-edit handler sent
  `SessionCommand::DropProvider` to EVERY live session, forcing sessions
  bound to untouched accounts to tear down their cached clients and HTTP
  connection pools and rebuild on the next request. It now invalidates only
  sessions bound to REMOVED or CHANGED accounts (the diff it already
  computes), leaving untouched accounts' sessions warm — pinned by a
  test asserting the untouched session's command channel stays empty.
- The lazy provider-resolution reply (`ResolveAccountCmd`) crosses threads
  via a `crossbeam_channel::Sender` (per the AGENTS.md channel rule for all
  new code) and carries the decrypted API key wrapped in `Zeroizing<String>`,
  so an unconsumed reply (session dropped mid-request) is wiped from the
  channel queue on drop instead of lingering as an ordinary `String`.
- Comment-only/test-only polish: de-duplicated the Wake-arm comment in
  `handle_suspend_event`; the `make_daemon_state` test helper leaks its
  config dir via the explicit `Box::leak` idiom instead of `mem::forget`.

### Changed

- **AGENTS.md channel rule is now workspace-wide**: thread-to-thread
  messaging must use `crossbeam_channel` in ALL crates, not only those that
  already depend on it — a crate gains the dependency in the same change
  that introduces its first cross-thread channel (leaf-crate std-`mpsc`
  tolerance removed).
- `SocketRegistry::register` now returns a `SocketId` and
  `RegisteredTcpTransport` unregisters + closes its registry fd on `Drop`,
  so the registry tracks only live connections (no more growth bounded
  only by the 256-entry prune). Unregistering an entry that `shutdown_all`/`prune`
  already removed is a documented no-op: entry removal is the single
  close-ownership-transfer signal, so the RAII guard cannot double-close an
  fd the registry closed first.
- Provider HTTP clients (`choreo-ai-protocols`) now build their ureq agents
  through a registry-registered connector chain: `build_agent` replaces
  ureq's plain TCP stage with choreo-sockreg's `RegisteringTcpConnector`
  (chain: `ConnectProxyConnector` → `RegisteringTcpConnector` →
  `RustlsConnector`, mirroring ureq 3.4's `DefaultConnector` for our feature
  set), so every provider HTTP connection is keepalive-tuned and tracked in a
  `SocketRegistry` for force-closing hung connections. All client
  constructors (`OpenAiClient`, `AnthropicClient`, `GoogleClient`,
  `OpenAiImageClient`, `ZaiImageClient`) now take the registry explicitly.
- **Per-session socket registries and lazy clients**: neither the daemon nor
  a session shares a provider client. Each session owns a private
  `SocketRegistry` plus a lazily-built provider client (sessions can be
  created while the keystore is locked, so no client can exist at creation
  time — it is built on the session thread at the first request, against
  that session's registry). The daemon command loop holds a clone of each
  session's registry so `handle_cancel_request` can force-close a wedged
  session's sockets from the one thread that DECIDED the cancel; a
  registry-clone map lives in `DaemonState`, entered at session spawn and
  dropped at session exit. Cancellation granularity is exactly the session
  (sub-sessions own their own registries; a parent cancel closes the whole
  subtree). Future async tool calls will reuse the session's
  agent+pool+registry triple. The old per-account provider cache
  (`DaemonState.providers`) is gone: `/lock`, `RemoveCredential`, and
  `AccountsReload` invalidate affected sessions' clients via a new
  `SessionCommand::DropProvider`, and each session rebuilds lazily on its
  next request. Only non-session-scoped work (model prefetch, catalog
  maintenance fetch — never individually cancelled) uses a
  command-loop-owned `daemon_registry`.
- Repinned the `zai` provider to z.ai's documented standard PaaS gateway
  (`https://api.z.ai/api/paas/v4`, per docs.z.ai) instead of the Coding-Plan
  gateway (`/api/coding/paas/v4`); z.ai's single API key type works on both,
  and Coding-Plan subscribers can still reach the coding gateway via the
  per-account `base_url` override. The image adapter's coding-gateway
  rewrite is unchanged but is now a no-op passthrough for the default base;
  doc comments/doc rows updated to say so.

### Added

- **Chat-completions `finish_reason` parsing and truncation notice (`choreo-ai-protocols`, `choreo-proto`, `choreo-daemon`):**
  the OpenAI-compatible chat-completions adapter now deserializes the per-choice `finish_reason`
  (non-streaming `Choice` and the streaming `StreamChoice`, threaded through the SSE accumulator)
  into a lenient, provider-portable `FinishReason` enum: z.ai's set (`stop`, `tool_calls`,
  `length`, `sensitive`, `model_context_window_exceeded`, `network_error`) plus OpenAI's aliases
  (`content_filter` → `Sensitive`, `function_call` → `ToolCalls`). Unknown values map to
  `Other(raw)` (logged, raw string preserved) and never fail the response parse. A `length`
  finish on a FINAL-TEXT turn sets a new `truncated: bool` on `FinalTextResult` (`false` for
  other providers and for tool-call turns, where `length` is normal tool-loop flow); the daemon
  appends a visible "⚠ response truncated (length limit)" line to such answers and logs a
  warning, so an output-limited cut-off no longer looks like a complete reply.

### Fixed

- **Silent failure modes for content-filter refusals and context-window overflow:** z.ai's
  `sensitive` finish reason (and OpenAI's `content_filter`) previously surfaced as an empty or
  blank response indistinguishable from a glitch — and potentially retryable. It now maps to the
  terminal, non-retryable `ContentFiltered` error (following the images-adapter precedent: no
  fabricated HTTP status, the retry layer never retries it). A
  `model_context_window_exceeded` finish now maps to a new distinct terminal
  `ContextWindowExceeded` error variant on both `ProviderError` and `InferenceError` (metrics
  label `context_window_exceeded`) with a clear "prompt exceeded the model's context window"
  message, turning a previously invisible compaction-bug signal into a diagnosable failure.
  `stop` / `tool_calls` / `network_error` / unknown values leave behavior unchanged.

- **Cached prompt-token reporting (`choreo-proto`, `choreo-ai-protocols`, `choreo-tui`):**
  `TokenUsage` gains a `cached_tokens: u32` field (`#[serde(default)]`, 0 when unreported, so
  old wire payloads and providers that omit the details object keep deserializing) and
  `merge_max` now folds it per-field. The OpenAI-compatible client parses z.ai's
  `usage.prompt_tokens_details.cached_tokens` (an optional nested `prompt_tokens_details`
  struct on `Usage`) in both the non-streaming and streaming chat-completions paths, maps it
  into `TokenUsage` with a `debug!` trace on nonzero counts, and the TUI session-detail
  "Tokens:" line annotates `(<N> cached)` when the provider reported a cached count. DeepSeek's
  differently-shaped flat `prompt_cache_hit_tokens` and the Responses API's
  `input_tokens_details.cached_tokens` are noted in-code as possible follow-ups.

- **`max_tokens` field pinned for the Zhipu slugs (`choreo-ai-protocols` catalog/models-overlay.toml):**
  z.ai's chat-completions API documents only `max_tokens` (no `max_completion_tokens`), but the
  models.dev derivation defaults every OpenAI-protocol provider to `max_completion_tokens` —
  z.ai would ignore or reject the output cap sent under that name. The bundled overlay now pins
  `max_tokens_field = "max_tokens"` on both `zai` and `zhipuai` (same GLM PaaS-v4 contract shape);
  pinned end-to-end by the new bundled-catalog regression test
  `bundled_overlay_pins_max_tokens_field_for_the_zhipu_slugs`.

- **`ZaiImageClient` — the z.ai (Zhipu GLM) Images adapter (`choreo-ai-protocols` src/images/zai.rs):**
  a second adapter behind `ImageGenerationClient` for the `/paas/v4/images/generations` endpoint
  (glm-image): request body is `{model, prompt}` + optional `size` (OpenAI wire strings verbatim;
  Auto omitted — z.ai documents no auto sentinel) and `quality` (`Low`/`Medium` → `standard`,
  `High` → `hd`, `Auto` omitted); `n`/`response_format`/`background`/`output_format` are never
  sent (z.ai documents none of them; an explicitly-set background is silently ignored — a
  documented best-effort-knobs decision, not an error). Responses are URL-returning (a temporary
  CDN link that expires after 30 days), so the adapter downloads the URL with the shared agent
  (no Authorization header — the pre-signed URL must not leak the API key to the CDN), an 8 MiB
  streaming cap, a loose `image/*` content-type guard, and the same 180 s per-attempt deadline;
  a `b64_json` field is tolerated (parse-level) and preferred when present. `content_filter`
  entries at level 0..=2 (0 = most severe, 3 = least) surface a clear "provider content filter
  blocked the generation (level N)" ClientError instead of EmptyResponse — blocked means no
  retry. Flat `{code, message}` error bodies surface their message via the existing retry-layer
  envelope extraction. Dispatch: the daemon's `from_account_config` routes the `zai` and
  `zhipuai` provider slugs to this adapter (all other OpenAI-protocol providers keep the default
  `OpenAiImageClient`); the catalog overlay pins `glm-image` as `supports_image_output` under
  both slugs (picked as the sole priority-fallback image candidate — no `pick_image_model`
  change needed).
- **Image-generation capability on the provider facade + `GetImageGenerationProvider` command:**
  `InferenceProvider` now carries an optional `image_client`
  (`Option<Arc<dyn ImageGenerationClient>>`, populated with an
  `OpenAiImageClient` built from the same `AccountConfig`/key as the chat
  client for OpenAI-protocol accounts; `None` for Anthropic/Gemini, whose
  image backends are deferred in v1), exposed via an `image_client()`
  accessor. The daemon gains a new `GetImageGenerationProvider` command that
  hands a tool thread an opaque `ImageProviderHandle { slug, client }` over a
  crossbeam reply channel — explicit account name, else the deterministic
  first image-capable provider in sorted-key order — with precise errors for
  a locked keystore, a named account that does not support image generation,
  and no matching account. `/lock` revocation falls out of the existing
  `providers.clear()`: no new handle can be resolved once locked.
- **`ImageGenerationClient` trait + OpenAI Images adapter (`choreo-ai-protocols
  src/images/`):** a provider-agnostic image-generation trait with typed
  wire enums (`ImageSize`/`ImageQuality`/`OutputFormat`/`Background`,
  `JsonSchema`+`Deserialize` so the daemon's tool args reuse them directly)
  and the sole `OpenAiImageClient` adapter — 180 s per-attempt wall-clock
  deadline, a deliberately frugal 2-attempt retry budget (a generation costs
  the provider money, so only clearly-transient 429/5xx get a second shot),
  and no `response_format` field (modern gpt-image models ignore it and
  older proxies 400 on it; the requested `output_format` carries the intent
  and the returned bytes are decoded with that MIME). Errors reuse
  `InferenceError`, so chat and image share one error taxonomy and metrics
  label mapping.
- **Image-output modality in the provider catalog:** `ModelEntry` gained
  `supports_image_output`, ingested from models.dev's `modalities.output`
  array and overridable via the same per-model overlay key used for the
  other model facts; new lookup helpers `model_supports_image_output` and
  `image_models_for_provider` (the provider's image-capable model ids) back
  the tool's model selection; `catalog/catalog.bin` regenerated.
- **`GenerateImage` tool (new `image` tool group):** the model generates an
  image from a text prompt; provider resolution runs through
  `GetImageGenerationProvider` in the daemon command loop (the credential
  never reaches a tool thread), model selection is catalog-driven (explicit
  `model` arg wins; otherwise a priority pick gpt-image > imagen >
  gemini-image > flux > dall-e over ONLY catalog-verified candidates, never
  a guess), and the returned bytes re-enter the exact `prepare_image_from_bytes`
  + `DisplayImageReturn` pipeline `display_image` uses — zero proto/client
  changes — so display, durable persistence, and the vision-feedback loop
  (the model sees its own generation and can refine it) are free. Covered by
  the `#[ignore]` integration test `choreo-daemon/tests/image_gen_integration.rs`.

### Changed

- **GLM `reasoning_effort` mapping in the OpenAI chat adapter**: z.ai's chat-completions API does not accept the full OpenAI effort set, and the accepted values differ by GLM generation. For the `zai`/`zhipuai` slugs, chat requests now map our effort slugs through the documented z.ai values — GLM-5.3/-flash accept only `low`/`high`/`max` (`minimal`→`low`, `medium`/`high`→`high`, `xhigh`→`max`; `off` omits the field with a warning since 5.3 cannot disable thinking and falls back to its `max` default), while GLM-5.2-and-below follow the documented family mappings (`minimal` passes through as skip-thinking, `low`/`medium`→`high`, `xhigh`→`max`). Other providers keep the previous pass-through behavior. The bundled overlay also pins `reasoning_levels = ["off", "low", "high", "max"]` on the `opencode-go` `glm-5.3-flash` wholesale entry so the UI never advertises slugs the API rejects.

- **Image-generation provider resolution extracted into `daemon/image_provider.rs` (`choreo-daemon`)**: the `GetImageGenerationProvider` handler and its pure `resolve_image_generation_provider` logic moved out of `daemon.rs` into a dedicated `pub(super)` child module (same pattern as `daemon/subscriber_handlers.rs`); its tests moved from `daemon/tests.rs` into the module's `#[cfg(test)]` block. The reply channel no longer carries `Result<_, String>`: a structured `ImageProviderError` (thiserror; `Locked` / `AccountNotConfigured` / `NoImageBackend` / `NoImageCapableAccount`, re-exported next to `DaemonCommand`) replaces it, preserving the precise guidance wording — the `generate_image` tool maps the error's `Display` text into its `ToolExecError` so the model still sees "keystore is locked — unlock first", the named-account guidance, and the "does not support image generation" slug. `ImageProviderHandle` stays in `providers/mod.rs`, next to the `InferenceProvider` facade it is protocol-erased alongside. No behavior change.

- Image-generation wire-body minimization: `ImageGenerationRequest` now `skip_serializing_if`-omits knobs left at their defaults
  (`auto` size/quality/background, `png` format), so an all-defaults request serializes to just `{model, prompt, n}` —
  image models reached through OpenAI-compatible proxies (imagen, flux, gemini-image) often reject parameters they do not
  implement even as explicit defaults. `Display` impls on the knob enums mirror the serde wire strings exactly, so the
  `generate_image` invocation line shows what the API receives (`1024x1024`, `high`, …) instead of Rust variant names.
- The OpenAI image adapter's wire tests moved from `src/images/tests.rs` (unit) to `tests/images_wire.rs` (integration,
  `#[ignore]`) per the Test Discipline rule — socket-based tests no longer run under `cargo test-fast`.
- `default_image_model` removed from `ImageGenerationClient` (and the hardcoded `gpt-image-1` default from `OpenAiImageClient`):
  with no catalog image-output candidates the tool now fails with guidance (pass `model` explicitly, or add a
  `supports_image_output` overlay entry + /refresh-models) instead of silently sending a guessed model the provider likely
  does not route (e.g. `gpt-image-1` against an opencode gateway); `args.prompt` is moved into the request instead of cloned.
- `prepare_image_from_bytes` (normalization + alt-text return shape) was
  extracted from `tools/image.rs`'s `display_image` so the new
  `generate_image` tool can share the same pipeline; behavior-neutral
  refactor, `display_image` output unchanged.

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

- `generate_image` tool timeout raised from the generic 60 s default to a dedicated floor derived from the image adapters' shared
  retry/deadline constants (`IMAGE_MAX_ATTEMPTS`/`IMAGE_DOWNLOAD_ATTEMPTS`/`IMAGE_TOTAL_TIMEOUT_SECS`, now `pub` in
  choreo-ai-protocols) plus a 60 s inter-attempt backoff headroom — currently 960 s: the previous default fired
  *while a paid generation was still rendering* (glm-image `hd` is documented at ~20 s but the adapters' bounded worst case —
  2 POST attempts × the 180 s per-attempt deadline plus the z.ai URL download's 3-fetch retry budget — exceeds 60 s), causing
  the outer wait-loop to kill a generation that was working correctly. Because the floor is computed from the adapter constants,
  an adapter retry-policy change automatically keeps the outer deadline in sync; the adapters' internal deadlines keep the
  ceiling bounded.

- z.ai image download resilience: z.ai's object storage advertises the generated image URL *before* the object is published
  (observed in production — the identical URL served a non-image error page on the first GET and a clean PNG seconds later, with
  the CDN's `X-Ufile-Create-Time` confirming lazy materialization), which turned the single-shot URL fetch into a hard
  `generate_image` failure right after a successful paid generation. The z.ai adapter's URL download now has its own bounded
  3-attempt retry budget over the account's short initial backoff (scheme violations, cap overflows, transport errors, and the
  exhausted budget stay terminal), with the cancel flag honored during the retry wait.

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

### Fixed

- `generate_image` post-generation cancellation: `ToolContext.cancelled` is now re-checked the moment the provider
  round-trip returns, so a cancel issued while a (up to 180 s) generation was in flight discards the result before any
  decode/validation/persistence/display work instead of surfacing an image the user already cancelled.
- `GetImageGenerationProvider` error accuracy: a named-but-unknown account now reports
  "account '<name>' is not configured or has no resolved provider" instead of the misleading generic
  "no OpenAI-compatible account is configured", and the no-image-backend error picks its provider slug deterministically
  (sorted-key order) instead of HashMap iteration order.

- **z.ai image error honesty + SSRF hardening (`choreo-ai-protocols` images/, `choreo-proto`):**
  (1) the CDN download retry loop no longer overloads `EmptyResponse` as "object storage hasn't
  published the file yet" — a dedicated `NotReady { detail }` variant was added to `ProviderError`
  and `InferenceError` (metrics label `not_ready`) for the propagation race, and `EmptyResponse`
  once again means strictly "empty body" (terminal); the retry loop was reshaped to an
  attempt-counter `loop` so the exhausted-budget fallthrough (and its `unwrap_or` workaround) is
  gone entirely. (2) A blocked generation no longer fabricates `ClientError { status: 200 }` — a
  new honest `ContentFiltered { detail }` variant (metrics label `content_filtered`) carries the
  policy denial with no invented HTTP status. (3) SSRF guard on the CDN download:
  `is_downloadable_url` (`url`-crate parse, new `choreo-ai-protocols` dependency) rejects
  IP-literal hosts in loopback/private/link-local/unique-local ranges (incl. IPv4-mapped v6);
  non-IP provider CDN hostnames are allowed — residual risk documented in code (provider-
  controlled hostname over an authenticated TLS channel, bytes fully validated downstream).
  Also hoisted the Zhipu slug allowlist out of the daemon into
  `images::is_zhipu_image_provider_slug` (the client crate owns provider-family knowledge), and
  added pure unit tests for the SSRF guard and the `/coding/paas` → `/paas` base rewrite.

## [0.1.0]

Initial release: daemon, TUI, protocol, Noise transport, provider catalog,
markdown rendering, PDF tooling, and the 14-crate crates.io suite.

[Unreleased]: https://github.com/choreographr/choreographr/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/choreographr/choreographr/releases/tag/v0.1.0
