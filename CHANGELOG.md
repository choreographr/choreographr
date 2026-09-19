# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- **Command-entry mode + inline command palette in `choreo-tui`.** Pressing
  `/` on an EMPTY prompt enters a dedicated command-entry mode: the `/` is only
  a trigger and is never shown, the input holds the bare command line (e.g.
  `model`, or `model gpt-4o`), and a keyboard-only overlay lists the matching
  commands. `↑`/`↓` move the highlight, `Tab` completes the highlighted
  command's name into the input (a trailing space is added) and stays in the
  mode, **`Enter` runs the command**, and `Esc` returns to the prompt. Each row
  shows the command's summary and its keyboard shortcut, resolved for the
  terminal in use. A `/command` line typed directly and submitted via the
  normal prompt still works.

- **New `choreo-shared` leaf crate.** The suite's small binary-facing helpers
  now live in one place: the release-name metadata (`release-name.txt`, moved
  out of `choreo-proto` and `include_str!`-baked exactly as before, so
  `--version` and the CI release title are unchanged), the shared clap `Styles`
  (previously copy-pasted into five CLI crates), and the `-v`/`-q` verbosity
  flags plus log-level resolution (`Verbosity`, `LoggingConfig::resolve`). Every
  binary — `choreographr`, `choreo-tui`, `choreo-gui`, `choreo-im`,
  `choreo-acp` — depends on it.

- **`retrieve_webpage` can render WebGL.** A new opt-in `webgl` argument
  launches Chromium in new-headless mode with ANGLE/SwiftShader software GL —
  including `--enable-unsafe-swiftshader`, which modern Chrome requires before
  it will grant a SwiftShader-backed WebGL context — and suppresses the
  crate's GPU-disabling defaults that would otherwise leave canvas/3D pages
  blank. The default stays legacy headless with GPU disabled, so existing calls
  are unchanged. Because a flag being present is not proof WebGL works, the
  tool probes the page for a real context and reports the WebGL version, or
  that no context could be created, in its result — appended as a clearly
  delimited `[webgl] …` line so it can't be mistaken for captured page content.

- **Bounded auto-recovery from truncated tool calls in the agent loop
  (`choreo-daemon`).** When a provider cuts a response off at its output-token
  limit mid-tool-call (the arguments JSON arrives truncated and is discarded as
  unsafe to execute), the loop no longer dead-ends the request: it records a
  short explanatory turn, seeds an actionable recovery instruction as the next
  user turn ("your call was truncated; split large writes into smaller tool
  calls"), and retries — up to `MAX_TRUNCATION_RECOVERIES` times before ending
  the request cleanly. The cap is essential because a session may run with
  `max_turns == 0` (unlimited), which cannot bound the loop itself. For
  `ResponseId`-policy (Responses-API) providers the retry also drops the
  `previous_response_id` chain — the truncated turn's id was never captured, so
  chaining onto the pre-truncation id would replay a `function_call` whose
  matching output was dropped with the discarded call; the retry resends the
  full, self-consistent history instead.

- **New `just pre-release` — a read-only release gate.** Where `just pre-commit`
  is the commit gate (and, by design, *mutates* the tree via `clippy-fix` and
  `fmt`), `pre-release` is its read-only twin for release time: it runs
  `preflight`, the release-state check (on `master`, clean, not behind
  `origin/master`), `fmt-check`, `clippy-strict`, `test-all`, and
  the release-only guards `pre-commit` omits (`check-supply-chain`,
  `check-changelog`, `check-release-name`) plus the new crates.io credential
  check — and never edits a single file. `RELEASE.md`'s Preflight
  section (now just `just pre-release`) and the gates now call it in place of
  the long-removed `just ci`.

### Changed

- **`MAX_FRAME_SIZE` raised from 32 MiB to 64 MiB (`choreo-proto`).** The
  codec's shared single-frame cap — enforced on send and receive by the
  Unix-socket and TCP/Noise transports, and used as the Noise
  fragment-reassembly bound — is doubled so a large full-session
  `SessionState` snapshot still encodes. A long session's turns are
  re-serialized *uncompressed* for the wire (the on-disk copy is
  zstd-compressed), so an attach snapshot carrying many turns of tool output
  and displayed images can exceed 32 MiB even when the database is well under
  it. Past the cap `encode_inner` returned `FrameTooLarge`, which the daemon's
  writer treated as a fatal transport error and aborted the connection —
  escalating, on a single-client auto-started daemon, to a full graceful
  shutdown. This is a stopgap: the oversized-frame path is still treated as
  fatal, it just takes 64 MiB to reach. The in-process/embedded transport is
  unaffected either way — it forwards `DaemonMessage` values and never
  serializes.

- **The TUI command palette now runs the highlighted command on `Enter` — no preceding `Tab`.** Previously a `/`-query had to be completed with `Tab` before `Enter` would submit it, so selecting a row with `↑`/`↓` and pressing `Enter` did nothing on an empty or partial line. `Enter` now resolves the line through `command_palette_enter_line`: a first token that already names a command exactly runs verbatim (arguments preserved — `model gpt-4o`, `session new foo`), while an empty or still-partial token adopts the highlighted row (keeping any argument tail, so `mo gpt-4o` runs `model gpt-4o`). `Tab` still completes the name without submitting for users who want to keep editing the line.

- **`just pre-commit` is now the commit gate, run automatically after every implementation run.** The gate runs in mutation-aware order — `clippy-fix` → `clippy-strict` → `test-all` → `fmt` → `check-changelog` — and the agent loops it (fix by hand, re-run) until green before committing, without asking the user first. Clippy and `test-all` now cover **all targets and all features**, and any clippy warning fails the gate (the verification pass denies warnings). Formatting runs *last*, not first, precisely because `clippy-fix` mutates the tree while `fmt` is semantics-preserving — the tested bytes stay behaviourally identical to the committed bytes. Commit messages now follow **Conventional Commits**, scoped by crate. The supply-chain and release-name guards moved out of the commit path to the release workflow (they are release guards, not pre-commit guards), and the redundant local-only `just ci` recipe was removed. When work is delegated, each subsession now runs the same gate and commits its unit before returning its report; a subsession that aborts leaves its changes uncommitted for the parent to inspect. The clippy/fmt gate flags live in `.cargo/config.toml` aliases (`clippy-all`, `clippy-all-fix`, `fmt-all`) so the `just` recipes stay flag-free.

- **Markdown tables in the TUI now use a nushell-style rounded frame.** The
  outer corners are `╭ ╮ ╰ ╯` (the `┬`/`┴`/`├`/`┤`/`┼` junctions and `│`/`─`
  strokes are unchanged, matching nushell's rounded preset), the header row is
  drawn bold, and the header rule is a uniform `├───┼───┤`. Column alignment is
  still applied via cell padding, but the GFM delimiter row's alignment colons
  (`:---`, `:---:`, `---:`) are no longer echoed into the rendered rule — they
  read as stray punctuation against the rounded frame.

- **Unified command model.** A single catalog in `choreo-client-core`
  (`command_catalog`) is now the source of truth for command discovery, and
  parsing and the TUI's keyboard shortcuts both route through the same
  `Command` path — so a key and its typed spelling behave identically. Bare
  forms are now the "most useful" form: `/model` opens the picker, `/session`
  opens the session manager, `/account` opens the accounts page, and
  `/reasoning` cycles effort (`/reasoning list` lists the levels). `Ctrl+O`
  opens the model selector only on legacy terminals; on kitty-protocol
  terminals the binding is `Ctrl+M`.

- **Command dispatch is table-driven end to end in `choreo-tui`.** The Chat
  page now resolves *every* command shortcut through the single logical
  shortcut table before page key handling — including the non-Ctrl `Alt+Enter`,
  which previously needed its own hard-coded match arm — so a key and its typed
  `/command` spelling share one dispatcher. The dispatcher itself moved out of
  the (already large) `connection/chat.rs` into `connection/command.rs`, and
  `parse_input_line` dropped its now-unused `attached_session_id` argument.

- **Uniform logging across every binary.** `choreo-tui`, `choreo-gui`,
  `choreo-im`, and `choreo-acp` now accept the same `-v`/`-q` flags and apply
  the same level policy as the daemon, resolved once in `choreo-shared::logging`.
  Precedence follows the Unix convention — **explicit flags win over `RUST_LOG`**
  (the daemon previously let `RUST_LOG` win and ignored the flags). `--version`
  on `choreo-gui` now also appends the release name, matching the other binaries.

- **The shared logging policy now owns the startup diagnostics and the
  log-file open.** `LoggingConfig::emit_startup_logs` emits the
  "flags take precedence over `RUST_LOG`" warning plus the effective-level
  banner (previously five hand-copied blocks), and the file-only binaries —
  `choreo-tui`, `choreo-gui`, and `choreo-acp` — open their pid-keyed
  temp-dir log through one `logging::create_log_file` helper that is
  owner-only (0600) and refuses a symlink planted at the predictable path.
  `choreo-im` keeps its target-less log format (the shared wiring had dropped
  `.with_target(false)`), and `choreo-acp`'s default level is now the shared
  `info` (its own module still at `debug`) instead of the ad-hoc env-only
  filter it used before.

- **Documentation reconciled with the new workspace size.** `ARCHITECTURE.md`
  and `README.md` now call the workspace twenty crates (root + nineteen
  members) and the publish set nineteen crates, list `choreo-shared` in the
  workspace topology and the publish set, and say all five binaries report the
  release name from `--version`.

### Removed

- **The `/models` alias is gone — use `/model`.**

### Fixed

- **The RELEASE.md crates.io sign-in check now actually works.** The documented
  `curl … /api/v1/me` probe could never return `200`: crates.io declares that
  endpoint cookie-only and rejects API tokens outright
  (rust-lang/crates.io#3518, 2021), so it answers `403` for a *valid* token — and
  crates.io's edge 403s curl's default User-Agent with an HTML page on top. The
  new `scripts/check-crates-io-token.sh` (wired into `just check-crates-io-token`
  and `just pre-release`) resolves the token the way cargo does
  (`$CARGO_REGISTRY_TOKEN`, else `~/.cargo/credentials.toml`), sends a custom
  User-Agent, and treats the endpoint's website-only 403 as *success* (the token
  authenticated) while failing on `authentication failed` / `401`. The stale
  `just ci` references in `RELEASE.md` and the doubly-wrong supply-chain wiring
  note in `deny.toml` (that guard never ran in `pre-commit`) are corrected.

- **The TUI command palette now lists commands in alphabetical order.** The
  palette presents the shared command catalog verbatim, but the catalog was
  ordered by group (Session → Account → Security → System) in natural usage
  order, so the picker read out of order. The catalog is now sorted A→Z by
  command name (each entry keeps its group tag as metadata), and a
  `catalog_is_alphabetical` unit test pins the order so it cannot silently
  drift again.

- **OpenAI Responses requests now use the correct `reasoning` shape, fixing
  every model on the official OpenAI provider.** Reasoning was sent wrong
  twice over: the effort went out as the Chat Completions top-level
  `reasoning_effort` (the Responses API rejects it with "this parameter has
  moved to `reasoning.effort`"), and the reasoning summary was requested via
  `include: ["reasoning.summary"]` — not a member of the fixed `include` enum,
  so a hard `400`. Both now live in the nested `reasoning` object
  (`reasoning.effort` and `reasoning.summary: "auto"`), and the invalid
  `include` is gone. The `reasoning` object is emitted ONLY for
  reasoning-capable models (`ServiceConfig::model_supports_reasoning`), so
  non-reasoning models such as `gpt-4o` and `gpt-4.1` send no reasoning config
  at all. Previously no official-OpenAI request could succeed: non-reasoning
  models failed on `reasoning.summary`, reasoning models on `reasoning_effort`.

- **Selecting a model choreographr can't drive now yields an actionable
  error.** The live model picker deliberately lists the provider's whole
  catalogue (so just-released and custom models stay selectable), which
  includes entries choreographr cannot use — legacy completions models such as
  `gpt-3.5-turbo-instruct`, embeddings, audio, and image models. Sending a turn
  with one used to surface the provider's terse rejection verbatim (OpenAI's
  `404 This is not a chat model …`); it is now shown as a short, plain line —
  e.g. `'gpt-3.5-turbo-instruct' is not a chat model — please try a different
  one.` — with the provider's jargon kept in the log rather than the UI.

- **Model-usage detection narrowed so genuine errors are no longer masked, and
  the Responses `reasoning` object gated wholly on model capability.** The
  check that recognises a not-a-chat-model or model-not-found rejection no
  longer matches the over-broad "not supported in the v1/…" wording, so an
  unrelated "Unsupported parameter … is not supported in the v1/…" error now
  surfaces its own message instead of being hidden behind the generic rewrite.
  The Responses `reasoning` object is likewise emitted only when the selected
  model actually supports reasoning — a non-reasoning model never receives any
  reasoning configuration at all, even a stray effort.

- **`Shift+Enter` no longer inserts a newline in command-entry mode.** The
  command line is single-line, so a stray `\n` only made the parser reject an
  otherwise-valid command; `Shift+Enter` now runs the command exactly like
  plain `Enter` (both are intercepted before the prompt's newline binding).

- **The inline command palette no longer recomputes the Chat-page layout.**
  `render_chat` returns the command input box rect it already laid out and the
  palette reuses it, so the layout solver runs once per frame instead of twice.

- **`choreo-tui` no longer writes an unfiltered TRACE log.** Its file
  subscriber installed no level filter, so the subscriber's max level defaulted
  to TRACE and every `debug!`/`trace!` event from the TUI *and its dependencies*
  was appended to `$TMPDIR/choreo-tui-<pid>.log` — the source of
  multi-hundred-MB log files. It now honors the shared `-v`/`-q`/`RUST_LOG`
  policy (default `info`).

- **The daemon's `RUST_LOG is set; -v/-q CLI flags are ignored` warning is no
  longer lost.** It was emitted *before* the tracing subscriber was installed,
  so it was silently dropped; it now logs after init (and reports that the
  flags take precedence).

- **A length-truncated tool call no longer masquerades as an empty answer
  (`choreo-ai-protocols`, `choreo-proto`).** The chat-completions adapter (the
  path the opencode zen/go gateway uses) returned a `FinalText` with empty
  content and `truncated: true` whenever every tool call's arguments were cut
  off mid-JSON — which the daemon rendered as a bare "⚠ response truncated
  (length limit)" with no explanation and no recovery. It now returns
  `TruncatedToolCall` instead, matching the Responses adapter, so the daemon's
  dedicated handler takes over. `DiscardedToolCall`'s `Display` is also bounded
  (tool name, a short preview, and the total size) so a cropped ~20 KB
  `write_file` payload can no longer bloat the log line or the transcript.

- **`glm-5.3-flash` now carries its real output-token ceiling
  (`choreo-ai-protocols`).** The model overlay comment claimed "1M context /
  128K output" but only pinned `context_window`, leaving `max_output_tokens`
  unknown (`0`) — so the catalog's clamp-down had no ceiling to apply and an
  account/model `max_tokens` override could request more than the model can
  produce. Pinned to `128000` per the official GLM-5.3-Flash model card
  (verified against docs.z.ai), so that override is clamped to the real
  ceiling. (The pin bounds the *requested* limit only when one is configured;
  it does not by itself stop a response from reaching the ceiling — the
  agent-loop recovery above handles that.)

## [0.2.1] - 2026-09-17 (Lindy)

### Added

- **A `check-changelog` guard now enforces the CHANGELOG category rule.**
  Every `## [...]` section may carry each Keep a Changelog category heading
  (`### Added`, `### Changed`, `### Deprecated`, `### Removed`, `### Fixed`,
  `### Security`) at most once, must not invent an unknown heading, and must
  not leave a category empty. It runs in `just pre-commit`/`just ci`
  (`just check-changelog`, via `scripts/check-changelog.sh`) and in the release
  workflow *before* the release body is extracted, so a malformed section
  fails the release instead of shipping a release page with two Fixed
  sections.

- **macOS x86_64 (Intel) release target:** a `choreographr-<version>-x86_64-apple-darwin.tar.gz`
  now ships alongside the arm64 tarball. It is cross-built in the same
  `scripts/release.sh` pass on the arm64 runner/host (Apple's toolchain
  targeting x86_64 from an arm64 Mac is first-class; no second build machine,
  and no reliance on the paid x64 CI runners) with `-C target-cpu=x86-64-v3`
  — every Intel Mac supported by the last Intel-capable macOS (26 Tahoe) is
  AVX2-class — while the arm64 tarball keeps its fleet-tuned target default.
  The Homebrew tap formula's x86_64 branch is now real (brew picks the archive
  by CPU at install time, so Intel Macs get the Intel tarball automatically);
  the curl installer gained the `Darwin-x86_64` mapping; the release first
  hard-fails if either darwin tarball is missing before generating
  `SHA256SUMS`, and `scripts/update-homebrew-tap.sh` requires both darwin
  tarballs and rewrites both digests in one pass, so a stale/placeholder Intel
  digest can never reach the tap. The Intel slice is not executed natively in
  CI (no free x64 macOS runner) — it is smoke-tested under Rosetta 2 on the
  arm64 job and verified by construction (digests + tap metadata) downstream;
  see the release.yml comments.

- **Server-authoritative keystore status (`choreo-proto`, `choreo-daemon`):**
  the daemon now models its keystore as three states — `Unbound` (no binding
  yet), `Locked` (bound, no cleartext in memory), and `Unlocked` — and exposes
  them on the wire as `DaemonMessage::Keystore { state: KeystoreState }`. The
  authoritative status is pushed to a client the moment it registers for
  notifications (activity subscribers) and broadcast to every activity
  subscriber on each transition, replacing the two-state `Locked`/`Unlocked`
  status *broadcasts*. `Unlocked`/`Locked`/`Bound`/`LockedError`/`KeystoreUnbound`
  remain targeted operation replies. This is what lets a first-run client LEARN
  the keystore is unbound and auto-bind, instead of inferring it from an
  operation reply. Wire protocol bumped v4 → v5.

- A `homebrew-verify` GitHub Actions workflow runs the SOP's manual Homebrew
  check (`brew install` + `choreographr --version`) on a macOS arm64 runner,
  so the Homebrew channel can be verified in CI without a physical Mac. It is
  dispatched by hand after the tap is bumped and asserts the tap formula's
  version, the installed `--version`, the formula test, and that the service is
  installed but not auto-started.

- **`choreo-sockreg` now implements its Windows (Winsock) path instead of
  logging no-ops.** `SocketRegistry::shutdown_all` issues a Winsock
  `shutdown(SD_BOTH)` on every registered socket before closing it, so a cancel
  or a suspend can now un-block an inference worker wedged in a provider read
  on Windows — the same service `shutdown(SHUT_RDWR)` provides on Unix.
  `prune_dead` probes each socket for liveness with the same verdicts the
  Unix `recv(MSG_PEEK)` probe uses (EOF / unsolicited data / reset ⇒ dead),
  but through a purely observational zero-timeout `WSAPoll`: the probe never
  flips `FIONBIO`, so it cannot misclassify live sockets while a worker is
  blocked in a provider read (`ioctlsocket` is not permitted then) and cannot
  touch the blocking mode shared with the caller's twin handle; a failed poll
  keeps conservatively instead of closing a possibly-healthy connection,
  replacing the old "close the oldest entry once the 256-entry cap is hit"
  stand-in; and `SocketTuning::apply` applies `SO_KEEPALIVE` plus the idle /
  interval timings through `WSAIoctl(SIO_KEEPALIVE_VALS)` (Windows has no
  per-socket probe-count knob). All three previously emitted a `tracing::warn!`
  no-op and were marked `WINDOWS-FOLLOW-UP`. The crate's Windows unit and
  integration tests are cfg-gated, so a Windows `cargo test` compiles too (the
  Unix test module used `nix`/`std::os::fd`, which broke it).

- **`choreo-power-events` gains a Windows suspend/resume backend.** It
  registers user32's `RegisterSuspendResumeNotification` with a
  `DEVICE_NOTIFY_CALLBACK` (Windows 8+), so the daemon now receives
  `SuspendEvent::Sleep` before the machine sleeps and `SuspendEvent::Wake` on
  resume instead of the previous inert, never-firing monitor. Windows invokes
  the callback on a system thread, so this backend spawns no dedicated monitor
  thread (unlike the Linux/macOS backends) and deliberately leaks its
  registration context; a registration failure logs and degrades to the inert
  monitor, matching the crate's best-effort contract.

### Changed

- **Tool-result images now "decay" out of older requests.** When a tool produces
  an image, the model sees its pixels for the duration of that request; from the
  next message onward the conversation carries only the existing text placeholder
  naming the tool and the source path, so a large history of images no longer
  bloats every request (a previously-attached image no longer re-rides the wire multi-megabyte at a time). Nothing is lost: the
  model can re-read the image from its path with a file tool, images still render
  normally in the UI, and a session loaded from disk starts with all historical
  images decayed.
- Internal cleanup of the daemon's keystore handlers (`daemon/keystore.rs`):
  one shared `unlock_error_reply` mapping for the Unlock/Bind add-credential
  reply construction, one `send_targeted_ack` for the targeted-reply-plus-ACK
  pair, and one `send_credential_add_failed` helper for every `AddCredential`
  rejection path. The `BindKeystore` reply no longer spells out the
  unreachable `Unbound` arm (the bind path TOFU-adopts, so it can never
  report unbound). `AddCredential`'s implicit-unlock tail no longer
  re-decrypts the just-test-decrypted blob from the DB — the decoded
  credential is seeded into the tail instead. No behavior change.

- `AutoBindAttempt::Failed` (the shared auto-bind trigger in
  `choreo-client-core`) now carries the structured `ClientError` rather than
  a pre-flattened string, so a frontend can distinguish a refused pre-send
  persist from a store-load failure. The enum is consequently no longer
  `Clone` (key material in the `Bind` variant should not be silently
  duplicated). UI output is unchanged.

- **The daemon's keystore handlers live in their own module
  (`choreo-daemon/src/daemon/keystore.rs`):** the `Unlock` / `BindKeystore` /
  `Lock` / `AddCredential` command handlers, the TOFU binding helpers
  (`bind_keystore` / `verify_keystore_binding`), the key-wipe helpers, and the
  shared `unlock_tail` were extracted out of the already-large
  `daemon.rs` into a child module, alongside the existing
  `daemon/subscriber_handlers.rs` / `daemon/image_provider.rs` split. Pure
  code motion — no behavior change.

- The once-per-connection auto-bind trigger is now SHARED (`choreo-client-core`):
  the mint-fresh-key / pre-send record / bind-loop latch policy previously
  duplicated across `choreo-tui` and `choreo-gui` (two nearly identical
  `trigger_keystore_auto_bind` copies) has been hoisted into
  `choreo_client_core::attempt_keystore_auto_bind`, returning a structured
  `AutoBindAttempt` (`Bind` / `Suppressed` / `Failed`); the frontends keep only
  the UI mapping and the send. No behavior change.

- A `/lock` against an UNBOUND daemon no longer re-broadcasts the (unchanged)
  `Unbound` status to all activity subscribers: the transition broadcast is
  now gated on the keystore actually being bound. Latching clients already
  held that state, so this only removes redundant chatter.

- Routine dependency refresh: every manifest requirement was bumped to its
  latest stable release and `Cargo.lock` re-resolved to current upstream.
  Manifest requirement bumps: `dirs` 6 → 7, `schemars` 1.2.1 → 1.2.2, and
  `structured-zstd` 0.0.49 → 0.0.53 (workspace); `alloy` 2.3 → 2.4
  (`choreo-blockchain`); and `ckb-vm` 0.24.14 → 0.24.15 (`choreo-daemon`). The
  lock additionally advances `clap` 4.6.6 → 4.6.7, `pdf-inspector` 1.19 →
  1.20, `lopdf` 0.44 → 0.45, `redb` 4.2 → 4.3, plus `const-hex`, `ruint`,
  `derive-where`, `zlib-rs`, and the `yoke-derive`/`zerofrom-derive` pair. No
  source changes.

- **External account edits now refresh a session's recorded provider slug.**
  The accounts-reload path already invalidated the cached client of every
  session bound to a removed/changed account; it now also pushes the new
  catalog key (`SessionCommand::SetProviderSlug`) — or clears it on removal —
  so slug-keyed static facts (context window, reasoning capability) stay exact
  immediately instead of going stale until the next request rebuilds the
  client. Internally, the client-invalidation and slug-refresh fan-outs share
  one `for_each_session_bound_to` helper and the account→slug lookup is a
  single `account_provider_slug` helper; the session's effective-slug accessor
  is renamed `effective_provider_slug` so it no longer shadows the
  `provider_slug` field.

### Removed

- **Removed the orphaned `dua.jpg` demo asset and its dead `REQUEST_IMAGE_*`
  constants.** The image was left behind after the demo `/image` command was
  removed: `choreo-daemon` still embedded it via `include_bytes!` and re-exported
  `REQUEST_IMAGE_BYTES` / `REQUEST_IMAGE_WIDTH` / `REQUEST_IMAGE_HEIGHT` /
  `REQUEST_IMAGE_MIME_TYPE` from its public API, but nothing in the workspace
  consumed them — the constants survived only as compiled-in dead weight
  (~57 KiB per build) with no remaining user. The asset file, the four constants,
  and the re-export are gone; the image-generation path is unaffected.

### Fixed

- **`[Unreleased]` had two `### Fixed` blocks, which would have shipped a
  release page with two Fixed sections.** The duplicate arose because a fresh
  `### Fixed` heading was inserted at the top of the section (before the
  existing `### Added`/`### Changed`/`### Fixed` blocks) rather than appending
  the bullet under the already-present `### Fixed` further down, and every
  later commit added to the new top block. The entries are merged into a single
  `### Fixed` and the section now follows the canonical
  Added/Changed/Removed/Fixed order; the new `check-changelog` guard prevents a
  recurrence.

- **`read_file_range` no longer rejects calls that omit the range fields** —
  `start_line` defaults to `1` and `max_lines` to the 500-line cap, so calling
  it with only `path` reads the whole file (with its usual range metadata)
  instead of failing with "missing field `start_line`".
- **`http_request` no longer rejects calls that omit `method`** — the field
  now defaults to `GET` (the schema marks it optional), and method names are
  case-normalized (`"get"` behaves like `"GET"`), so the model-facing
  failures "missing field `method`" and unsupported lowercase variants are
  gone.
- **`http_request` sends the same structured `User-Agent`
  (`choreographr/<daemon version>`) as inference requests** instead of a
  stale hardcoded `choreographr/0.1`; the product string is now shared via
  a single helper. An explicit caller-supplied `User-Agent` header still
  overrides it (matched case-insensitively) — the default is now applied only
  when the caller omits one, because ureq *appends* request headers, so the
  old "set ours, then the caller's" order put two `user-agent` lines on the
  wire instead of replacing ours.
- **`http_request` reports the timeout and method it actually uses.** The
  `timeout_secs` argument's schema text claimed a 30-second default while the
  tool defaulted to 10s and clamped to 1–30s, and the invocation summary
  echoed the raw (unclamped) value while the request used the clamped one;
  the default, bounds, and summary now derive from one helper. The summary
  also upper-cases the method (`"get"` is shown as `GET`) to match the
  canonical form the request dispatch uses.
- **Provider connections no longer stall or give up when the first resolved
  IP is unreachable instead of declining the connection.** The daemon's HTTP
  connector previously only moved on to the next resolved address after an
  explicit-refusal failure; on a network where the first address is
  blackholed (e.g. a stale IPv6 answer on a broken v6 route), the dial hung
  until the connect timeout expired without ever trying the v4 address that
  would have connected. The dialing loop now matches ureq's own TCP
  connector: the overall connect budget is split across resolved addresses,
  address-specific failures (refused, host/network unreachable, address
  unavailable) and dial timeouts fall through to the next address while
  budget remains, and the total dial is bounded by the connect timeout. A
  fast address-specific failure (a refusal or an unreachable route) leaves the
  next address's time slice intact — it consumed no budget — whereas a dial
  timeout halves it, exactly as ureq's own geometric scheduling does.

- **The context window and reasoning capability no longer blink out on a
  locked keystore.** A session's static catalog facts — the model's context
  window and its reasoning effort levels — are pure catalog lookups keyed by
  the account's provider slug (e.g. `opencode-go`), but the daemon resolved
  them through the lazily-built, credential-bound `InferenceProvider`
  client. Until the client existed (keystore unlocked + first request, or a
  SetAccount/SetModel with a stored key), the attach snapshot reported no
  capability (Ctrl+R answered "reasoning capability not yet available") and
  the context window stayed unresolved — so e.g. glm-5.3-flash over
  opencode-go displayed 1M one moment and nothing the next. The session now
  records the provider slug the moment the account config resolves (at
  spawn time from `AccountManager` — a non-secret fact — in lazy provider
  resolution, and on `SetAccount` even before unlock) and resolves both
  facts from the slug: the context window prefers the client-config
  override when a client exists and otherwise falls back to the catalog,
  and the reasoning capability always resolves from the slug. Wire behavior
  and lookups are unchanged for sessions with a live client.
- **Switching a session's account could keep serving the previous account's
  provider client.** `SetAccount` rebuilt the client only on success and never
  dropped the old one, while `resolve_provider` returns any cached client
  unconditionally — so setting a new account whose config/key was not yet
  resolvable (e.g. the keystore was locked) left the session dialing the OLD
  provider (wrong endpoint, wrong key) under the new account name, and the
  attach snapshot kept reporting the old account's slug-keyed facts. A real
  account switch now drops the previous client up front (a failed re-resolve
  leaves the session clientless, rebuilt lazily on the next request) and
  records—or clears—the recorded provider slug to match the new account, even
  when no client can be built.
- **A failed unlock could leak freshly decrypted credentials into daemon
  state with `locked` still `true`.** The unlock tail populated
  `state.credentials` (the plaintext `ServiceCredential` map) BEFORE loading the
  accounts TOML, so an account-load failure returned an error from an unlock
  that had already published decrypted secrets into reachable state — and a
  later `/lock` only clears what the state maps contain at that moment, so
  the partial state was never committed by the usual lock path. The tail now
  performs every fallible step (bulk decrypt, accounts load, default-account
  resolution) on locals and only then commits credentials, accounts, and
  `locked = false` in one shot, so a failing unlock leaves the daemon exactly
  as it was.

- **A first-run client could never bind a fresh daemon (the "keystore is
  locked" dead end).** A fresh daemon starts both locked AND unbound, but the
  clients only auto-bound on a `KeystoreUnbound` reply to an `Unlock` — which
  `choreo-tui` never sent when it had no key (a genuine first run has neither a
  stored per-daemon key nor a legacy `identity.pk`). The subscribe-time push
  was a bare `Locked`, carrying no "unbound" signal, so the user was stuck at
  the `🔒 keystore locked` banner with the (false) guidance that "a fresh daemon
  binds automatically" — while `/unlock` failed with `NoUnlockKey`. `choreo-tui`
  now auto-binds on the authoritative `Keystore { Unbound }` status push, and
  the startup guidance no longer promises a bind it cannot perform. `choreo-gui`
  — which does not subscribe to the all-activity bus and so never receives the
  push — gains a connect-time keystore bootstrap mirroring `choreo-im`'s
  `establish_keystore`: unlock with a resolved key, else PROBE with a freshly
  minted `BindKeystore` (verify-only against an already-bound daemon, so it
  never overwrites a binding).

- The Homebrew tap formula installed the 0.1.0 binary set (`choreo-im`,
  `choreo-acp`), which the 0.2.0 release build no longer ships, so
  `brew install choreographr` failed with `ENOENT` on `choreo-im`. The formula
  now lists only the shipped binaries (`choreographr`, `choreo-tui`), and
  `scripts/update-homebrew-tap.sh` reconciles the `bin.install` line against the
  in-repo mirrored formula so the two cannot drift apart again.
- The release workflow no longer attaches the not-yet-shippable Windows
  `.zip` to GitHub releases. The `release` job downloaded every artifact in
  the run (`pattern: "*"`), so the zip landed on a release whenever the
  `windows-msvc` job finished first — and leaked into `SHA256SUMS`. The
  download now names the three shipping platforms, and a guard fails the
  release on any stray Windows artifact.

## [0.2.0] - 2026-09-15 (Lindy)

### Added

- The TUI now rejects a prompt submitted while the attached session is not
  idle with the status message "Session is not idle, please wait before
  prompting.", instead of sending a `RunInput` the daemon cannot start. The
  guard is client-side only (the daemon remains authoritative) and fails open
  when no session status is known yet. Rejected text stays in the input bar and
  the per-session draft is preserved. Slash-commands bypass the guard so they
  stay usable mid-turn (e.g. `/cancel`), with the one deliberate exception of
  `/continue`, itself a new-turn trigger and hence guarded too. A future change
  will replace this with prompt queueing for async tool calls.
- Autostart feedback in the TUI: while the autostart hook spawns the daemon
  and waits for its socket, the status line shows "no daemon running —
  starting choreographr…" (then "daemon started") via a new
  `UiEvent::Status` event from the connection task — no more silent screen
  during the wait, and still nothing printed directly to the terminal (that
  would garble the alternate screen).

- **Daemon autostart from the TUI (`choreo-tui`, new `autostart` module):**
  in Unix-socket mode the TUI connects DIRECTLY to the daemon socket — no
  pre-flight probe. `choreo_client_core`'s new
  `run_daemon_connection_with_autostart` keeps the stream of a successful
  first dial and invokes the caller's autostart hook only when the dial
  itself fails with `NotFound`/`ConnectionRefused`; the TUI's hook spawns the
  sibling `choreographr` binary (same directory as its own executable,
  resolved via `current_exe`) as a detached child with
  `--auto-exit --log-file $TMPDIR/choreo-daemon-<tui-pid>.log`, waits for the
  spawned daemon's socket (100 ms interval, 5 s budget), and the connection
  is retried. Spawn failure kills the child and the TUI exits with an error
  naming the daemon's log path. TCP mode (`--tcp-addr`) never spawns — a
  remote daemon is not launchable from the client machine. This also fixes
  the probe-as-client problem: a probe connection to a live `--auto-exit`
  daemon would have looked like a connect-and-disconnect client and killed
  the daemon.
- `choreographr --log-file <path>`: write daemon logs to a file instead of
  stderr (ANSI disabled for file output; level control unchanged via
  `-v`/`-q`/`RUST_LOG`). The daemon refuses to start when the file cannot be
  created or opened — a TUI-spawned daemon with a bad log path must fail
  loudly with the path, not silently lose diagnostics.
- `choreographr --auto-exit`: graceful shutdown when the last client
  disconnects (used by the TUI's spawned daemon). Connection threads report
  `DaemonCommand::LastClientDisconnected` on exit (after releasing their
  `ConnectionSlot`, so the command loop's zero-check of the shared
  live-connection counter is accurate); the command loop — the single
  thread that owns every shutdown decision — sets the existing shutdown flag
  and wakes the accept loop with a self-connect, running the exact SIGINT
  drain (notify-before-EOF, bounded joins). No idle timeout: a daemon that
  has never had a client runs forever. The embedded daemon passes `None`
  (auto-exit is a socket-listening-daemon feature only).
- Workspace-wide strict clippy lints, modeled on the "strict lints"
  configuration popularized by No Boilerplate (namtao.com/rust) and adapted
  to this workspace's conventions: `[workspace.lints.clippy]` in the root
  Cargo.toml denies the panic family (`unwrap_used`, `expect_used`, `panic`,
  `panic_in_result_fn`, `unreachable`, `unimplemented`, `todo`, `exit`), the
  slicing family (`indexing_slicing`, `string_slice`), and
  `unchecked_time_subtraction`, inherited by every member via
  `[lints] workspace = true`; a root `clippy.toml` re-allows
  unwrap/expect/panic/slicing in tests, matching AGENTS.md's error-handling
  exception. ~350 production indexing/slicing sites were converted to
  bounds-checked `.get()` access (binary parsers, wire framing, TUI layout
  chunks, credential parsing), the daemon's one `unreachable!` became a
  structured error, and the GUI's startup `process::exit` carries a targeted
  waiver. Deliberately deferred: `pedantic` stays advisory (~800 findings,
  dominated by `cast_possible_truncation`) until the backlog is worked off;
  `nursery` is never denied (unstable lints would hard-break builds on
  toolchain updates); `arithmetic_side_effects` (~150 sites) and
  `as_conversions` are per-crate follow-ups.
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

### Changed

- **Project-local skills now shadow global skills of the same name, and skill
  discovery is shared between the `load_skill` tool and skill persistence.**
  Discovery deduplicates by frontmatter `name` and scans the project walk before
  the global `~/.agents/skills` scope, so a project skill wins over a same-named
  global one (previously both were listed and `load_skill` resolved whichever
  came first); within a single scope the candidate directories are visited in
  sorted order, so a tie is deterministic. The two scopes are passed as a named
  `SkillScopes { global_home, working_dir }` (injectable, so global discovery is
  unit-testable). The agent loop computes the session's skill set once and shares
  it with tools via `ToolContext::discovered_skills`, so `load_skill` resolves
  against the SAME snapshot the system prompt lists — the body returned to the
  model can never diverge from the body persisted into the prompt, and no second
  filesystem walk is needed.

- **Releases may carry a dance-style name** (major/minor only): `RELEASE.md`
  documents that the conductor picks a name at release time — a dance style
  such as *Lindy*, with no pre-assigned list — and records it in the CHANGELOG
  section heading as `## [X.Y.Z] - YYYY-MM-DD (Lindy)`; patch releases stay
  nameless. The name is release metadata only (not in the tag or any install
  identifier). The CI release job lifts it into the GitHub release title
  (`choreographr 0.2.0 — Lindy`); AGENTS.md notes the heading form.

- **Release names are baked into the binaries.** `choreo-proto/release-name.txt`
  is now the single source of truth for a series' dance-style name: a new
  `choreo_proto::release_name` module compiles it in with `include_str!` (no
  `build.rs`; an empty file means unnamed), so all four clap binaries
  (`choreographr`, `choreo-tui`, `choreo-im`, `choreo-acp`) print
  `0.2.0 (Lindy)` from `--version`, and the daemon and TUI also log the version
  string at startup. The name is a per-minor-series attribute — major/minor sets
  a new name, a patch keeps the current one. The CI release job reads the same
  file for the GitHub release title (parentheses, matching `--version`), and a
  new `scripts/check-release-name.sh` drift guard — wired into `just pre-commit`,
  `just ci`, and the release workflow — keeps the file and the CHANGELOG heading
  in sync.

- **CHANGELOG.md reformatted to satisfy Keep a Changelog**: the `[Unreleased]`
  section, which had accumulated **seven** `### Fixed`, **seven** `### Changed`,
  and five `### Added` blocks, is consolidated to **one heading per category**
  (Added / Changed / Removed / Fixed / Security) with every entry preserved
  verbatim. AGENTS.md gains a "CHANGELOG.md" subsection spelling out the
  convention (one heading per category, at most; write it for the release page;
  promoted to a dated `## [X.Y.Z] - YYYY-MM-DD` at tag time). The release-notes
  extraction in `.github/workflows/release.yml` (and RELEASE.md's manual
  appendix) now matches the version heading with an optional date
  (`index($0, "## [X.Y.Z]") == 1` instead of an exact-line `==`), so the dated
  Keep-a-Changelog heading no longer fails the release job. Also corrected the
  0.1.0 entry's crate count (14 → 12: the initial release published 12 crates).

- **Release documentation reconciled with the current workspace**
  (`RELEASE.md`, `ARCHITECTURE.md`, `README.md`): the crates.io publish set is
  **18** crates (every member except `choreo-gui`, the one private member); the
  next release adds **six** new crates (`choreo-blockchain`, `choreo-sanitize`,
  `choreo-image`, `choreo-sockreg`, `choreo-power-events`, `choreo-content`) —
  which EXCEEDS the new-crate burst of 5, so RELEASE.md Phase 2 now documents
  the concrete two-batch staging plan (4 new + 2 new, ≥10 min apart) as well as
  the burst-override option; the Windows `.zip` is documented as built-but-not-released (the CI
  release job's `needs` omits `windows-msvc`); the crates.io /`binstall` install
  routes name `choreo-tui` alongside `choreographr` (the TUI binary moved to its
  own package); the workspace is nineteen crates (root + eighteen members); and
  the batch-staging example now describes 0.1.0's actual 12-crate set.

- The TUI's client-side submit guard is now a single `App::new_turn_rejection`
  helper covering both the idle check and the keystore-locked check, and it is
  applied to **every** action that begins a new turn — a plain prompt, Alt+Enter,
  and the `/continue` command (all of which end up as `RunInput` /
  `ContinueGeneration`) — so the `/continue` path that was previously unguarded
  is now covered too. A new
  `SessionStatus::is_idle` (exactly `Inactive`; `Sleeping` is not idle) makes
  the idle test explicit and shared. Behaviour change: a locked-keystore
  rejection now runs before the input buffer is cleared, so the rejected text
  is preserved (matching the idle-guard behaviour) instead of being dropped.
- The two `ContinueGeneration` senders (Alt+Enter and `/continue`) collapsed
  into a single `connection::chat::send_continue_generation` helper that owns
  the guard, request-id allocation, in-flight tracking and send, so the two
  triggers can no longer drift. The only intended difference is the shell echo:
  `/continue` shows `> continue`, Alt+Enter does not.
- The "nothing is listening" dial classification (`NotFound` /
  `ConnectionRefused`) moved into a shared predicate,
  `choreo_proto::dial_error_means_no_listener`, used by the TUI connection
  path (`choreo-client-core`'s `run_daemon_connection_with_autostart`) and
  documented as the mirror of the daemon-side stale-socket probe, so the
  client and daemon classifications can never drift. The predicate takes the
  whole `&io::Error` (matching on `kind()` internally) rather than a bare
  `io::ErrorKind`, so a caller cannot accidentally classify an unrelated
  error.
- The cross-platform unix-socket DIAL is now a single primitive in
  `choreo-proto` (`connect_unix` for the stream-keeping dial, plus the
  `socket_listening` boolean wrapper and the `UnixStream` re-export that
  resolves std-vs-`uds_windows` per platform). Every dial site — the TUI
  autostart wait, `choreo-client-core`'s connection path, the daemon's
  stale-socket probe and its signal/auto-exit accept-loop wake-ups — now uses
  it instead of four inlined `#[cfg]` copies, and the now-redundant direct
  `uds_windows` dependencies were dropped from `choreo-client-core` and
  `choreo-tui`.
- Autostart/poll tests that wait on real time or bind real sockets moved out
  of the `src/` unit-test modules into the crates' `tests/` integration
  suites (`choreo-tui/tests/autostart_poll.rs`,
  `choreo-client-core/tests/connection_autostart.rs`), per the test
  discipline: unit tests are now timing-free.

- Refactored the post-strict-lints bounds-checked slicing boilerplate: a
  shared `read_slice` helper in `choreo-ai-protocols` replaces the four
  duplicated `buf.get(..n).unwrap_or(&[])` read-contract sites (SSE readers,
  image CDN download); `zai.rs` hoists its duplicated `as_object_mut` guard;
  `choreo-daemon`'s `text_stream.rs` no longer uses silent-widening
  `unwrap_or(full-buffer)` slicing fallbacks — out-of-bounds windows and a
  violated `Utf8Error::valid_up_to()` invariant now produce loud errors
  instead of quietly defeating the display cap.

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
- Routine dependency refresh (`cargo update`): the tree re-resolved to current
  upstream releases (83 packages), including `ureq` 3.4.2 / `ureq-proto` 0.6.3,
  `alloy` 2.4.2, `reqwest` 0.13.5, `serde_with` 3.23, `crossbeam` 0.8.5, and the
  PDF stack (`pdf-inspector` 1.19, `lopdf` 0.44, `md-5` 0.11). `Cargo.lock` only;
  no source changes.

### Removed

- `identity.pk.enc` file — the unlock key is stored in the keystore
  (`/unlock` uses it; `/unlock <key>` records it); rejected-unlock-key
  revert semantics replaced by survivor semantics.

### Fixed

- **`choreo-sockreg`, `choreo-power-events`, and `choreo-content` are now
  published** (release-blocking): each was `publish = false`, but each is a
  dependency of a PUBLISHED crate (`choreo-ai-protocols` → `choreo-sockreg`;
  `choreo-daemon` → `choreo-power-events`; and `choreo-daemon`'s optional
  `content` feature → `choreo-content`), and cargo refuses to package a crate
  whose dependency is not on crates.io — even an OPTIONAL one (verified: `cargo
  package -p choreo-ai-protocols` fails with "no matching package named
  `choreo-sockreg`", and `cargo package -p choreo-daemon` fails on the optional
  `choreo-blockchain` dep). `choreo-gui` stays private (a leaf client nothing
  depends on). Without this the next release could not be published at all.
  `scripts/publish-stable.sh` now derives an `--exclude` for every remaining
  `publish = false` member (currently just `choreo-gui`) from the manifests,
  since cargo-release 1.1.5 ignores the flag in `--workspace` selection.

- Deleting the attached TUI session now clears the cached attach-state
  (`attached_status` / `attached_tool_groups`) along with the session id. These
  describe the attachment that just went away; leaving them set rendered a
  stale status bar and (with the new idle-guard) made a later plain prompt be
  rejected as "session not idle" even though no session was attached at all.
- The unified config watcher now arms its `notify` watch **synchronously** in
  `ConfigWatcher::spawn()` (on the caller's thread) instead of on the spawned
  transport thread: previously a config-file write landing between `spawn()`
  returning and the thread's first `watch()` call was silently lost, which
  made `config_watcher_delivers_only_registered_basenames` (and its sibling)
  flaky under full-suite parallel load. `spawn()` now also creates the config
  dir itself, so both startup steps complete before the thread exists; the
  thread receives the already-created, already-armed watcher plus the raw-event
  receiver and initial `armed` flag, and the re-arm cadence still covers a
  dir deleted at runtime or a spawn-time arm failure.

- The daemon's `--log-file` is hardened for the shared temp dir the TUI
  autostart writes into: on unix it is created 0600 AND opened `O_NOFOLLOW`
  (a symlink planted at the predictable pid-keyed path fails the open
  instead of redirecting the daemon's diagnostics), the opened file is
  verified to be a regular file owned by the daemon's own euid (a
  pre-created file owned by another user, or a FIFO/device, is refused), and
  its mode is explicitly tightened to 0600 (the create mode only applies on
  creation, so a file left by an earlier run could be group/world-readable).
  Windows keeps inherited ACLs.
- The TUI's autostart status message no longer lingers: a `UiEvent::Status`
  is flagged transient (`App::status_is_transient`) and cleared by the first
  real daemon message, so "daemon started" does not sit on the status line
  once the connection is live and the first turn is quiet. Statuses written
  by daemon handlers are never cleared by this rule.
- `poll_until_listening` now clamps each inter-probe sleep to the time
  remaining before its budget, so the total wait is genuinely bounded by the
  budget instead of overshooting by up to one interval after the last failed
  probe (matching the doc contract).

- `run_server` no longer steals a live daemon's socket: before removing an
  existing socket file it now probes it with a connect — a successful
  connect means another daemon is still listening, so startup fails with
  "another daemon is already listening at …" instead of orphaning the
  working daemon (the two-daemon race that TUI autostart widens); any
  failed connect (ENOENT, ECONNREFUSED, a regular file at the path) means
  stale, and the leftover is removed as before — with the socket path now
  carried in the removal error so a bare "Permission denied" (the Termux
  /tmp failure mode) is diagnosable.
- A daemon protocol-version mismatch surfaces an actionable TUI quit
  message — "the daemon's protocol version is incompatible — restart the
  daemon (it may be an older build)" — instead of a raw codec error the
  user cannot act on; all other connection errors keep the historical
  wording.
- Eliminated the last clippy warnings across the workspace so
  `cargo clippy --workspace --all-targets` is warning-free (the only
  remaining notice is the `proc-macro-error2` dependency advisory): removed
  a needless `Ok(.. ?)` in `choreo-transport`'s preamble reader, collapsed a
  nested `if` in the OpenAI tool-call accumulator, factored the
  `noise_integration` test helper's complex tuple return into a
  `NoiseTestPair` alias, and silenced the unused-import/dead-code warnings in
  the temporarily-disabled `choreo-content` platform round-trip test via
  `#[cfg(any())]` gating (items preserved verbatim for restoration) instead
  of leaving a doc comment dangling before its block comment.

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

- A session with no working directory now still receives a full system
  prompt: `build_system_content` always builds the base identity prompt,
  tool-groups listing, skills metadata (global `~/.agents/skills` plus any
  optional project scope), loaded-skill bodies, and session title. Only the
  project context files (`AGENTS.md`/`CLAUDE.md`) and subdirectory hints
  genuinely depend on a working directory, so they are now gated on
  `Option<&Path>` and the function returns `String` instead of `Option<String>`.
  This also demotes the per-agent-loop-iteration `warn!` for dir-less
  sessions to a `debug!`.

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
- `rustls` bumped 0.23.43 → 0.23.45 to fix RUSTSEC-2026-0285
  (GHSA-2mjx-qc3c-rqvc, CVE-2025-61730): rustls accepted TLS 1.3 handshake
  messages sent at the wrong encryption level. rustls reaches the tree only as
  a transitive dependency (via `ureq`), so this is a lockfile-only update with
  no source change. Verified with `cargo test-all` (3273 passing) and the
  supply-chain gate (`cargo deny` advisories).

## [0.1.0]

Initial release: daemon, TUI, protocol, Noise transport, provider catalog,
markdown rendering, PDF tooling, and the 12-crate crates.io suite.

[Unreleased]: https://github.com/choreographr/choreographr/compare/v0.2.1...HEAD
[0.2.1]: https://github.com/choreographr/choreographr/releases/tag/v0.2.1
[0.2.0]: https://github.com/choreographr/choreographr/releases/tag/v0.2.0
[0.1.0]: https://github.com/choreographr/choreographr/releases/tag/v0.1.0
