# Choreographr Architecture

## Overview

`Choreographr` is a local-first AI assistant built as a Rust workspace. A **daemon** process
communicates with multiple LLM providers through a pluggable trait-based provider
system, while **clients** (terminal, desktop, and IM platforms) connect to the daemon
over a Unix domain socket (or Noise IK encrypted TCP for remote connections) using a custom length-prefixed binary protocol.

```
┌──────────────┐    Unix socket     ┌──────────────┐    HTTP/SSE     ┌──────────────────────┐
│   choreo-tui     │◄──────────────────►│              │◄──────────────►│  OpenAI API          │
│  (terminal)  │                    │              │                ├──────────────────────┤
├──────────────┤                    │  choreographr  │◄──────────────►│  Anthropic Messages   │
│ choreo-gui       │◄──────────────────►│              │                ├──────────────────────┤
│ (desktop/Android)│    Unix socket     │              │◄──────────────►│  Google Gemini API    │
├──────────────┤                    │              │                ├──────────────────────┤
│   choreo-im     │◄──────────────────►│              │◄──────────────►│  Mistral API          │
│ (IM bridge)  │    Unix socket     │              │                ├──────────────────────┤
└──────────────┘                    └──────────────┘                └──────────────────────┘
                                                                    │  200+ OpenAI-compat   │
                                                                    │  providers via catalog│
                                                                    └──────────────────────┘
```

---

## Workspace topology

Sixteen crates in a single Cargo workspace (resolver = "3") — the root
package plus fifteen members:

```
Choreographr (workspace)
├── choreographr          Workspace root — the suite installer: declares the four
│                       binaries (choreographr choreo-tui choreo-im choreo-acp;
│                       the desktop GUI is a separate crate, choreo-gui)
├── choreo-proto           Wire protocol (shared types + framing)
├── choreo-sanitize        Leaf crate — shared Unicode "spoofing" predicates
│                       and the tool-output byte budget + truncation marker
├── choreo-image          Leaf crate — shared image decode (EXIF-orientation
│                       baking, HEIC/HEIF with a pre-decode allocation guard)
├── choreo-keystore        X25519 + ECDH keypair crypto, encrypted storage primitives
├── choreo-transport       Noise IK encrypted TCP transport abstraction
├── choreo-client-core     Shared client logic (parsing, images, history, credentials)
├── choreo-markdown        Markdown parser and HTML renderer (pulldown-cmark + ammonia),
│                       plus a LaTeX math → Unicode pretty-printer (`render_math_pretty`)
├── choreo-mcp             MCP (Model Context Protocol) client — spawns subprocess servers,
│                       discovers tools, and dispatches tool calls over JSON-RPC stdio
├── choreo-ai-protocols    Provider protocols — OpenAI-compatible, Anthropic Messages, and
│                       Google Gemini clients, the ProviderClient trait, and the provider
│                       catalog (models.dev base + bundled overlay, embedded postcard)
├── choreo-daemon          Unix socket server — the core engine (library; the daemon
│                       binary `choreographr` lives in the root package)
├── choreo-acp             ACP bridge — translates the Agent Communication Protocol
│                       (JSON-RPC over stdin/stdout) into choreo-proto messages over the
│                       daemon's Unix socket, enabling ACP-compatible editors (Claude
│                       Code, Cline, etc.) to interact with Choreographr sessions
├── choreo-content           Choreographr Coordination Platform client — Substrate
│                       chain writes (subxt via a tokio sidecar), indexer reads,
│                       IPFS add/cat, content protobuf encode/decode, and the
│                       publish-time image mipmap pipeline behind the `content` tools
│                       (feature-gated `content` cargo feature, off by default)
├── choreo-tui             Terminal UI client (ratatui + crossterm)
├── choreo-gui             Desktop/Android GUI client (Dioxus Native / Blitz
│                       renderer — no webview; built as lib+cdylib for dx/gradle
│                       APK packaging)
└── choreo-im              IM platform bridge (Telegram)
```

### Dependency graph

```
                    ┌──────────────────┐
                    │   choreo-proto   │ (no workspace deps)
                    └────────┬─────────┘
                             │
                    ┌────────▼──────────┐
                    │ choreo-keystore   │ (no workspace deps)
                    └────────┬──────────┘
                             │
              ┌──────────────┼──────────────────────────────┐
              │              │                              │
      ┌───────▼────────┐    │                ┌──────────────▼──────────────┐
      │ choreo-client-core │    │                │        choreo-daemon        │
      └───────┬───┬────┘    │                └───────┬──────────────┬───────┘
              │   │         │                        │              │
              │  ┌▼─────────▼────────┐  ┌────────────▼─────────┐  ┌──▼────────┐
              │  │ choreo-transport    │◄─│ choreo-ai-protocols  │  │ choreo-acp │
              │  └─────────┬──────────┘  └────────────┬─────────┘  └───────────┘
              │            │                          │
         ┌────▼───┐  ┌─────▼─────┐  ┌──────▼────┐  ┌───▼─────────────┐
         │choreo-tui  │  │ choreo-gui  │  │ choreo-im  │  │  choreo-mcp    │
         └────────┘  └───────────┘  └──────────┘  └─────────────────┘
```

(`choreo-markdown` is consumed by `choreo-client-core` and `choreo-tui`; it is omitted from the graph for brevity.)

`choreo-sanitize` is a leaf crate (no workspace deps) consumed by
`choreo-daemon`, `choreo-tui`, `choreo-client-core`, and `choreo-blockchain` —
it owns the Unicode "spoofing" predicates and the shared tool-output byte
budget / `...[truncated]` marker, so every sanitizer and streaming cap in the
workspace agrees on the same policy and budget.

`choreo-image` is a leaf crate (only `image` + `heif-oxide` + `tracing`)
consumed by
`choreo-daemon` and `choreo-tui` — it owns the single decode path for raster
formats (with EXIF orientation baked in) and for HEIC/HEIF (with a pre-decode
allocation guard), so the model path and the UI path cannot drift apart.

---

## Release & packaging

Releases are cut **locally** with the orchestrator below, or on GitHub
Actions by pushing a `vX.Y.Z` tag —
[`.github/workflows/release.yml`](../.github/workflows/release.yml) is the CI
counterpart of `scripts/release.sh` (the Linux and macOS jobs literally run
that script, so build flags, `--locked`, feature selection, service-file
staging, and tarball naming stay in one place; the Windows and Termux jobs
pass the same build line inline). A `workflow_dispatch` run builds everything
but creates no release — that is the pipeline-test path so the first real tag
is never the first test of the pipeline. The orchestrator,
[`scripts/release.sh`](../scripts/release.sh), is a dry-run by default: it
reads the version from the root `Cargo.toml` (the single source of truth that
the Homebrew formula, AUR PKGBUILD, and installer mirror), runs
`cargo build --profile dist -p choreographr` (the four shipped binaries only —
choreo-gui's Dioxus Native (Blitz) renderer stack is excluded from
the release build) under the workspace's dedicated `[profile.dist]` profile
(root `Cargo.toml`),
packs the release tarball, writes
`SHA256SUMS`, builds the `.deb`/`.rpm` when the tools are present, and prints
the exact upload + checklist commands. Only `--upload` runs
`gh release create`; `--allow-dirty` skips the clean-tree guard. The
`just` front door wraps all of it: `just release`, `just release-upload`,
`just release-allow-dirty`, `just release-tap`, `just smoke-test`,
`just package-deb`, `just package-rpm`, and `just install` (see the
README).

### Shipped artifacts

Exactly **four binaries** ship in every artifact (tarball, `.deb`, `.rpm`,
Homebrew, AUR):

- `choreographr` — the daemon
- `choreo-tui` — terminal UI client
- `choreo-im` — IM bridge
- `choreo-acp` — ACP bridge

`choreo-mcp` is a **library-only crate** (the MCP client the daemon spawns
for tool servers) — it has no `[[bin]]` target and is never shipped as a
binary. `choreo-gui` is built separately (desktop via `cargo run -p choreo-gui`,
Android via `dx build --platform android`) and is not shipped either.

The shipped target set is exactly two; `release.sh` and `install.sh` hardcode
this pair and refuse any other platform ("ships Linux x86_64 and macOS arm64
only"), and `release.sh` builds whichever of the two it is run on:

| Target | Platform | Asset |
|---|---|---|
| `x86_64-unknown-linux-musl` | Linux x86_64 | `choreographr-<version>-x86_64-unknown-linux-musl.tar.gz` |
| `aarch64-apple-darwin` | macOS arm64 | `choreographr-<version>-aarch64-apple-darwin.tar.gz` |

The Linux tarball is a **fully static musl build** — `release.sh` cross-builds
it to `x86_64-unknown-linux-musl` with `--features mimalloc`, so one artifact
runs on any Linux kernel regardless of the host's glibc version (this also
replaces the old "build inside an old-glibc container" compatibility dance).
The `.deb`/`.rpm` remain native glibc host-target builds without the
`mimalloc` feature — see `scripts/release.sh`. The tarball holds the four
binaries at the **top level** (no `bin/` prefix)
plus both service files, exec bits preserved — `install.sh` and the Homebrew
formula reference them directly.

**All shipped binaries are stripped.** The workspace `[profile.dist]` profile
(root `Cargo.toml` — the shipped-artifact profile every release-pipeline build
selects with `--profile dist`; every shipped-relevant key is pinned
explicitly there so `[profile.release]` tuning for local builds can't leak
into artifacts) sets `strip = "symbols"`, so every
artifact — tarball, `.deb`,
`.rpm`, Homebrew, AUR — ships binaries without a symbol table (~22% smaller;
~10% smaller tarball). Shipped binaries get **fat LTO + `codegen-units = 1`**
— set in `[profile.dist]` only, so the default local `cargo build --release`
keeps its fast, LTO-free links (thin LTO was removed from `[profile.release]`
in e6b5a47 because it made every default release link slow and memory-hungry;
the dist profile is the shipped-only tuning home, and the fat-LTO link cost
lands only in the release pipeline). Panic messages keep their `file:line`
locations (compiled-in string constants via `#[track_caller]`); only
`RUST_BACKTRACE=1` symbolization is lost, and the daemon emits no backtraces.
`panic = "abort"` is deliberately NOT set: the daemon isolates
request-worker panics with `catch_unwind` (sessions.rs), which abort would
defeat. The RPM spec keeps `__os_install_post %{nil}` so rpm's brp scripts
never re-process the already-stripped binaries.

Shipped binaries build at an explicit **CPU floor per target**, set via
`RUSTFLAGS="-C target-cpu=…"` in the release scripts/workflow — never
`target-cpu=native`, and never profile rustflags (those ignore `--target`, are
nightly-only, and are stripped by `scripts/build-stable.sh` before every
stable release build; env rustflags additionally override any developer's
`~/.cargo/config.toml`, so local and CI artifacts are comparable):

- **musl tarball + Windows zip: x86-64-v2** (SSE3/SSSE3/SSE4.1/SSE4.2/POPCNT/
  CMPXCHG16B — Intel Nehalem 2008+, AMD Bulldozer 2011+). The level enterprise
  distros have moved to (RHEL 10 baseline = v3, SLES 16 = v2) while the
  community distros (Debian/Arch/Fedora, Ubuntu mainline) stay v1 — v2 is the
  pragmatic floor between "runs on anything since 2003" and modern
  vectorization. Future per-CPU-level artifacts (e.g. a v3 tarball) reuse the
  same `RUSTFLAGS` mechanism with a different value.
- **macOS tarball: the target default** — `aarch64-apple-darwin` already
  defaults to `apple-a14` (Apple-Silicon-tuned), and the fleet is homogeneous
  by definition; no flag needed.
- **Android/Termux: generic `armv8-a`** — the device fleet spans a decade of
  cores with nothing newer in common; `build-android.sh` enforces
  `RUSTFLAGS="-C target-cpu=generic"`.
- **.deb/.rpm: baseline (v1)** — the glibc-distro range they serve is split
  (Debian/Arch = v1, RHEL 10 = v3), so baseline is the only level covering
  all of them.

C dependencies (mimalloc, compiled by `cc`/zig) are NOT affected by
`RUSTFLAGS` — only rustc codegen is — but those libraries do their own runtime
feature detection.

The desktop-notify tool (`notify_send`, backed by notify-rust) was removed
from the daemon, so the shipped binaries link no C libraries on the glibc
targets — and the only C component on the musl tarball is the mimalloc
allocator, enabled via `--features mimalloc` (the only shipped artifact with a
non-system allocator).

**CI linker policy: default linkers everywhere.** No mold/wild/lld-fast-linker
additions in the release pipeline. Fat-LTO links spend their time in LLVM, not
the linker's data structures, so a faster linker barely moves link time; macOS
links Mach-O via `ld64` and Windows links PE via `link.exe` regardless, and CI
link time is a rounding error next to the 20–30 minute fat-LTO builds. A
developer's local `wild` (via `~/.cargo/config.toml`) is deliberately
overridden by the release scripts' env `RUSTFLAGS`.

**Known-benign linker warning on the musl zigbuild link**: `warning: linker
stderr: ignoring deprecated linker optimization setting '1'`
(`#[warn(linker_messages)]`, once per binary). Root cause verified against the
rustc source and reproduced locally: rustc itself emits `-Wl,-O1` for every
optimized (opt-level ≥ 2) GNU-flavor link (`GccLinker::optimize()` in
`rustc_codegen_ssa/src/back/linker.rs`) — a GNU-ld output-optimization hint
that zig's bundled lld recognizes as deprecated and ignores. It never appears
on `cargo -v`'s rustc command line because rustc constructs linker args
internally at link time (same reason the relro args don't show there), which
is why the source of the flag took a spy-linker investigation to pin down.
Binaries are correct; treat it as CI noise, not something to suppress with
`-A linker_messages` (that would hide real linker warnings too).

### `packaging/` assets

| Asset | Purpose |
|---|---|
| `choreographr.service` | systemd **user** unit (`ExecStart=%h/.local/bin/choreographr`, `Restart=on-failure`, `WantedBy=default.target`) — shipped in the tarball, `.deb`, and `.rpm`; installed to `~/.config/systemd/user/` by `install.sh` |
| `com.choreographr.daemon.plist` | launchd agent for **non-Homebrew** macOS installs (`RunAtLoad`/`KeepAlive` true, logs to `/tmp/choreographr.log`, hardcoded `/opt/homebrew/bin/choreographr`) — shipped in the tarball; Homebrew installs use the formula's `service do` block instead |
| `homebrew/choreographr.rb` | Homebrew formula for the `choreographr/choreographr` tap — prebuilt-tarball variant (no build toolchain); its `service do` block backs `brew services` |
| `aur/PKGBUILD` + `aur/.SRCINFO` | Arch `choreographr-bin` (prebuilt; empty `depends=` — static binaries) |
| `rpm/choreographr.spec` | RPM spec for the fat package — compiles nothing: `build-rpm.sh` stages the prebuilt binaries into the build root and disables `__os_install_post` so they are not stripped/rewritten |

**Policy: installed, never auto-enabled.** The daemon is a *user* service
that needs accounts and API keys before it is useful, so no package script,
installer, or release tool ever enables it: no `%post`/`%preun` in the RPM
spec, no `postinst` in the `.deb`, no `systemctl enable` in `install.sh`, and
the launchd agent is loaded only on explicit user action. The user opts in
with `systemctl --user enable --now choreographr` (Linux) or
`launchctl load ~/Library/LaunchAgents/com.choreographr.daemon.plist`
(macOS).

### `scripts/` tooling

| Script | Role |
|---|---|
| `install.sh` | curl\|sh installer — downloads the pinned-version tarball, verifies its SHA-256 against the `SHA256SUMS` fetched over the same TLS channel (no trust-on-first-use, no eval), extracts only the four binaries via an explicit member list, installs the platform service file, and never auto-enables. `--uninstall` removes everything; `CHOREOGRAPHR_BASE_URL` overrides the download base for testing/mirrors only. |
| `build-deb.sh` / `build-rpm.sh` | Build the single fat `.deb` / `.rpm` containing all four binaries plus the systemd user unit, from existing `target/dist/` artifacts |
| `smoke-test.sh` | Extracts a release tarball, checks the four binaries exist and are executable, asserts each binary's `--version` reports the release version, and runs `--help` on all four clap clients |
| `release.sh` | The release orchestrator — local builds (its CI counterpart is `.github/workflows/release.yml`, which runs it on the Linux/macOS runners); dry-run by default, `--upload` runs `gh release create`, `--allow-dirty` skips the clean-tree guard (CI passes it: a checkout IS the pushed commit, so the uncommitted-edits threat model cannot apply) |
| `publish-stable.sh` | The crates.io publish wrapper (RELEASE.md Phase 2) — strips the nightly-only per-profile `rustflags` and the `[unstable]` config opt-in for the duration of `cargo release publish` (masking the two edited files from cargo-release's clean-tree gate via `git update-index --skip-worktree`, and always passing `--exclude choreo-gui`, which cargo-release 1.1.5 won't drop on its own via `publish = false`), so published manifests stay buildable by stable `cargo install`, then restores both files and clears the masks |
| `update-homebrew-tap.sh` | Bumps the `choreographr/homebrew-choreographr` tap formula to the workspace version — recomputes both macOS tarball `sha256` digests from `dist/` (no re-download), rewrites `Formula/choreographr.rb` with exact-count rewrite validation, prints the diff; `--push` commits + pushes to the tap. Keeps the tap bump on the release machine (the CI release workflow ships the tarballs but does not touch the tap) |
| `check-supply-chain.sh` | The dependency supply-chain gate — runs `cargo deny check advisories bans sources` against `deny.toml` (falling back to `cargo-audit` + a literal lockfile scan when cargo-deny is absent), after scanning the local `~/.cargo/registry` cache for the `.crate` files deleted during the 2026-08-20 `arrayref` attack (RUSTSEC-2026-0260). Wired into `just pre-commit` / `just ci`; see the **Dependency supply chain** subsection under **Security model** |
| `build-android.sh` | Cross-builds the four suite binaries for Android/Termux via cargo-ndk (`arm64-v8a` by default, `--emulator` adds `x86_64`; `--check` is a prerequisite-checking dry run) under `--profile dist` (the shipped-artifact profile — matches the desktop release pipeline), stages them in `dist/android/<abi>/`, and prints the `adb push` guidance for Termux `$PREFIX/bin`. Strips the per-profile `rustflags` from the manifest for the duration (persistent backups under `target/` + EXIT-trap restore, plus a next-run self-heal that recovers a tree left stripped by a hard-killed predecessor — the trap-reliant restore alone was not kill-safe; see `build-stable.sh`) — profile rustflags apply regardless of `--target`, so `-C target-cpu=native` would emit host-CPU code that traps on Android devices. Deliberately excludes `choreo-gui`, whose Android build is `dx build --platform android` (cdylib APK payload, `just gui-android`) |

### Distribution channels (0.1)

- **Homebrew tap** — `brew tap choreographr/choreographr && brew install choreographr` (prebuilt formula)
- **GitHub Releases** — the tarball, `SHA256SUMS`, and the `.deb`/`.rpm` at `https://github.com/choreographr/choreographr/releases`
- **choreographr.com** — `https://choreographr.com/download/<version>/` mirrors the tarball and `SHA256SUMS` (this is what `install.sh` fetches); `https://choreographr.com/install.sh` serves the installer, and per-version download redirects are added at release time
- **AUR** — `choreographr-bin`
- **crates.io** — `cargo install choreographr` (source build, needs Zig) and `cargo binstall choreographr` (prebuilt; asset naming resolved via `[package.metadata.binstall]` below)

### crates.io metadata

The workspace inherits crates.io-required fields from `[workspace.package]`
in the root `Cargo.toml` (`version`, `license`, `repository`, `homepage`,
`readme`, `description`), and members opt into publishing by *not* setting
`publish = false`. The **publish set** is therefore fifteen of the sixteen
workspace members — everything except `choreo-gui` (not shipped).
`choreo-sanitize` is published too (previously private): the members that
depend on it must resolve it from crates.io after a release, and
cargo-release's publish verification rejects unpublished workspace deps:

`choreographr` (root), `choreo-daemon`, `choreo-blockchain`, `choreo-tui`,
`choreo-im`, `choreo-acp`, `choreo-proto`, `choreo-keystore`, `choreo-transport`,
`choreo-ai-protocols`, `choreo-mcp`, `choreo-client-core`, `choreo-sanitize`,
`choreo-markdown`, `choreo-image`

`choreo-gui` sets `publish = false`: it drags in the Dioxus Native (Blitz/wgpu)
renderer tree and is not part of the shipped suite, so
it is neither published to crates.io (`cargo install choreo-gui` does not
exist) nor included in the prebuilt release artifacts (tarball/.deb/.rpm/
Homebrew/AUR), which carry the daemon, TUI, and bridges only. The GUI is kept
out of the publish selection explicitly — `scripts/publish-stable.sh` always
passes `--exclude choreo-gui` because cargo-release 1.1.5 does not honor
`publish = false` in `--workspace` selection (verified: its plan lists the
GUI crate, and a real publish would then fail on cargo's own refusal). The
root `choreographr` package transitively depends on the other 14 publish-set
members, so releasing the suite publishes 15 crates in dependency order.

Releases are driven by **cargo-release** (`[workspace.metadata.release]` in
the root `Cargo.toml`): it bumps versions, tags, and publishes the 15 crates
to crates.io topologically. With `dependent-version = "fix"`, published
manifest requirements (e.g. `choreo-tui = "0.1"`) stay in lockstep across
minor/major bumps. The crates.io publish runs through `scripts/publish-stable.sh`
(which strips the nightly-only per-profile rustflags so the shipped manifests
stay buildable by stable `cargo install` — profile rustflags in a published
manifest hard-break stable cargo) and it runs before `scripts/release.sh`
builds the prebuilt artifacts.

The root package declares `[package.metadata.binstall]`, so
`cargo binstall choreographr` resolves the GitHub release asset naming
(`choreographr-<version>-<target>.<ext>`) from the package itself instead of
requiring a manual `--pkg-url`; `bin-dir = "{ bin }{ binary-ext }"` maps the
tarball's archive-root binaries (an empty `bin-dir` is rejected by binstall),
and an `x86_64-unknown-linux-gnu` override maps glibc hosts to the static
musl tarball (the only Linux asset shipped). The daemon crate is `choreo-daemon` (library
`choreo_daemon`, no `[[bin]]` target) — the `choreographr` binary it backs
lives in the root package's `src/bin/`.

---

## Crate details

### `choreo-proto` — Wire protocol

Defines all shared message types and framing. No dependencies on other workspace crates.

**Key types:**

| Type | Purpose |
|---|---|
| `ClientMessage` | Enum of all messages a client can send |
| `DaemonMessage` | Enum of all messages the daemon can send. Split into a `Session { session_id: Option<u64>, event }` **envelope** carrying the 29 session-scoped [`SessionEvent`]s (next row) plus 23 flat connection/reply/global variants (`Sessions`, `Pong`, `Models`, keystore + account replies, `ModelsRefreshed`/`ModelsRefreshFailed`, `CatalogUpdated`, `ShuttingDown`, `Evicted`, …). No `#[non_exhaustive]` — the variant set IS the wire contract, so every consumer match enumerates it fully. |
| `SessionEvent` | Enum of the 29 session-scoped events (`SessionCreated`, `SessionAttached`, `SessionState`, `SessionStatusChanged`, `TurnAppended`, `TurnsUndone`/`TurnsRedone`, `Started`, `OutputChunk`, `ToolCallStarted`/`Finished`/`Failed`, `ToolResultChunk`, `Done`, `Failed`, `Cancelled`, `TokenUsageUpdate`, `LiveOutputTokenCount`, `ModelSelected`/`ModelSelectionFailed`, `SessionAccountSet`, `ContextWindowResolved`, `SessionWorkingDirSet`, `SessionTitleSet`, `ReasoningEffortSet`/`Failed`, …). Events do NOT carry a `session_id` — it is hoisted onto the [`DaemonMessage::Session`] envelope's `Option<u64>` field, so every event has an origin session **by construction** (`Some(id)` for session-scoped broadcasts; `None` for the connection-level replies the daemon synthesizes with no session, e.g. "no session attached" failures) — it can never be forgotten, mismatched, or duplicated; the wire nests the event inside the envelope, so the origin is present on the wire too. |
| `SessionMessage` | A single turn in a conversation with `message_id: u32` (monotonically increasing per-session), `parent_id: Option<u32>` (links to the triggering user/ATU message for undo subtree traversal), `deleted: bool` (soft-delete for undo), a `created_at: TimestampMs` field and a `kind: SessionMessageKind` enum. Variants (`SessionMessageKind`): `SystemText`, `UserText`, `AssistantText`, `AssistantToolUse`, `ToolResult`, `DisplayedImage` (persisted image replay) |
| `ImageMetadata` | Mime type, dimensions, byte length for streamed images |
| `DisplayedImageRecord` | Binary image data + `ImageMetadata` for persisted image replay (carried inside `SessionMessageKind::DisplayedImage`) |
| `ReasoningCapability` | Struct with `available_effort_levels: Vec<String>` — the reasoning effort slugs a model supports (e.g. `"off"`, `"low"`, `"medium"`, `"high"`, `"max"`). Empty means reasoning is not supported. Cycle helper validates/rotates through slugs. |
| `ReasoningArtifact` | **Opaque reasoning round-trip payload**, captured verbatim by a provider adapter at the parse boundary and re-emitted verbatim on the next request. Variants: `ChatReasoning { field: ChatReasoningField, bytes: Vec<u8> }` (OpenAI-compatible chat — `field` tags which wire field the text came from, `reasoning_content` / `reasoning` / `reasoning_text`, so re-emission targets the same field; DeepSeek/Kimi capture `reasoning_content`), `AnthropicThinking(Vec<u8>)` (ordered thinking / redacted_thinking block JSON, signatures + redacted data intact), `GoogleSignatures(Vec<u8>)` (Gemini encrypted thought signatures), `ResponsesItems(Vec<u8>)` (OpenAI/xAI Responses opaque reasoning items). Stored as raw bytes so `choreo-proto` stays dependency-light — only the producing adapter may interpret a payload (each adapter (de)serializes its own wire representation). Carried on `Turn.reasoning_artifact`. |
| `ReasoningProducer` | `{ provider_slug: String, model: String }` — identity of the model that produced a turn's reasoning artifact, stored on `Turn.reasoning_producer`. The request builder compares it against the current provider+model (same-model provenance): artifacts are model-bound, so a turn produced by a different model must not have its (possibly encrypted) payload replayed after a mid-session model switch. |
| `TokenUsage` | Tracks LLM token consumption (`input_tokens`, `output_tokens`, `total_tokens`). Embedded in `SessionMessageKind::AssistantText` and `SessionMessageKind::AssistantToolUse` for per-turn accounting, in `SessionSummary` and `SessionEvent::SessionState` (inside `DaemonMessage::Session`) for session-level totals, and in `SessionEvent::Done` (same envelope) for per-request usage. |
| `last_prompt_tokens` | `Option<u32>` field on session metadata and protocol messages tracking the `input_tokens` from the most recent API response — the actual context size being sent to the model, used for context-window progress displays. |
| `last_modified` | `i64` Unix-epoch-**milliseconds** on `SessionSummary` / `SessionEvent::SessionStatusChanged` (proto) and `SessionMetadata` / `SessionConfig` / `SessionRecord` (daemon). Bumped on **completed requests**, session creation, and explicit metadata edits (title/model/account/reasoning) — NOT on transient status transitions (Inference/ToolCall/Retrying), which would re-sort the sessions list mid-request. The sessions list is ordered by it (newest first) and it survives restarts via `SessionRecord`. All session-level timestamps (`created_at`, `last_modified`) are milliseconds to match `Turn.created_at` (`TimestampMs`). |
| `SessionStatus` | Enum representing the current session state: `Inactive`, `Inference`, `ToolCall(String)`, `Retrying {…}`, `Sleeping`. Included in `SessionSummary` and `SessionEvent::SessionState` for live status display in client toolbars. |
| `ToolResultRecord` | Persisted tool result with fields `call_id`, `name`, `content`, `is_error`, `invocation_description`, and an additive `image: Option<ImageReference>` (a **reference** to a vision image this tool produced — the source path + MIME + dimensions **plus the normalized bytes** in `ImageReference::data`; `#[serde(default)]` so old persisted turns deserialize with `None`). The bytes are daemon/model-only: they feed the request builder directly and are moved to the `session_attachments` table at persistence time, and they are stripped from client-facing turns (see `turn_for_client`). |

`ClientMessage` variants:
`CreateSession`, `ListSessions`, `AttachSession`, `GetSessionState`, `RunInput`,
`TestImage`, `Cancel`, `Ping`, `GetCredential`, `ListModels`, `SetModel`, `Unlock`,
`Lock`, `AddCredential`, `RemoveCredential`, `AddAccount`, `RemoveAccount`,
`ListAccounts`, `SetSessionAccount`, `SetReasoningEffort`, `GetReasoningEffort`,
`Undo`, `Redo`, `ContinueGeneration`
- `CreateSession` now carries optional `context_config`, `account_name`, `selected_model`, and `reasoning_effort` (slug string) fields

`DaemonMessage` variants — split into two families:
- **Session-scoped events** ride the `Session { session_id: Option<u64>, event: SessionEvent }` envelope: `Some(id)` for every event broadcast by a session task (the origin by construction), `None` for the connection-level replies the daemon synthesizes without a session task ("no session attached" failures, create/attach/set-account errors). Inner [`SessionEvent`]s: `SessionCreated`, `SessionAttached`, `SessionState`, `SessionStatusChanged`, `SessionFailed`, `SessionDeleted`, `SessionDeleteFailed`, `TurnAppended`, `TurnsUndone`, `TurnsRedone`, `Started`, `OutputChunk`, `ToolCallStarted`, `ToolCallFinished`, `ToolCallFailed`, `ToolResultChunk`, `Done`, `Failed`, `Cancelled`, `TokenUsageUpdate`, `LiveOutputTokenCount`, `ModelSelected`, `ModelSelectionFailed`, `SessionAccountSet`, `ContextWindowResolved`, `SessionWorkingDirSet`, `SessionTitleSet`, `ReasoningEffortSet`, `ReasoningEffortSetFailed`
- **Flat variants** (connection/reply/global — no session scope): `Sessions`, `Pong`, `Models`, `ModelsFailed`, `Unlocked`, `Locked`, `LockedError`, `CredentialAdded`, `CredentialAddFailed`, `CredentialRemoved`, `CredentialRemoveFailed`, `Credential`, `AccountAdded`, `AccountAddFailed`, `AccountRemoved`, `AccountRemoveFailed`, `Accounts`, `AccountListFailed`, `ModelsRefreshed`/`ModelsRefreshFailed` (with `RefreshStatus`: `UpToDate`/`Updated`/`Forced`), `CatalogUpdated`, `ShuttingDown`, `Evicted` (best-effort advisory sent just before a lag-eviction disconnect; clients use it to distinguish eviction from a crash)

**Wire format:**

```
┌──────────────────┬────────────────────────────────────────┐
│ 4 bytes (BE u32) │ msgpack((protocol_version: u8, msg))   │
│   payload len    │                                        │
└──────────────────┴────────────────────────────────────────┘
```

- Protocol version: `4` (v4 = the 29 session-scoped events were moved into `SessionEvent` and now ride the `DaemonMessage::Session { session_id: Option<u64>, event }` envelope; v3 had removed `TurnFinalized` — the final-turn snapshot rides `TurnAppended` — and added `Evicted`, a best-effort lag-eviction advisory; mixed-version peers fail fast at the version gate). The v4 shape was amended in place before the first release — the `session_id: 0` sentinel became `Option<u64>` (`None` for connection-level replies) — so the wire version stays `4`, no bump.
- Max frame size: 32 MiB
- Framing functions: `encode_frame`, `decode_frame`, `read_message`, `write_message`
- **Error type**: `ProtoError` (thiserror enum) — `Codec`, `FrameTooLarge`, `TrailingBytes`, `UnsupportedVersion`, `Io`
- Lag-eviction byte gauge: `DaemonMessage::approx_wire_size` / `Turn::approx_size` (in `choreo-proto/src/size.rs`) — a deliberate over-estimate used by the daemon's lag accounting, pinned by `types::tests::approx_wire_size_never_underestimates_encoded_payload` (see the daemon broadcast section)

Payloads are MessagePack in **named mode** (`rmp_serde::to_vec_named`): structs
serialize as maps with field names and enum variants by variant name, so the
format is self-describing, compact, and broadly supported across languages
(future mobile/web/third-party clients). The `(protocol_version, message)` tuple
still encodes as a MessagePack array of 2 even in named mode. Decoding runs
through an explicit `Deserializer` over a `Cursor` so the trailing-bytes check
can use the cursor position as a remainder probe (rmp-serde 1.3.1 has no
`from_slice_ref`).

**Codec rule.** MessagePack (named) carries anything that crosses a language
boundary: the client↔daemon wire and the values in `sessions`/`session_turns`.
Since schema 2, `session_turns` values are additionally zstd-compressed (see
the DB section) — the on-disk codec is zstd frame + MessagePack named(`Turn`),
while the wire turn stays plain MessagePack. Postcard is retained only on
Rust-only, language-isolated internal channels —
the RISC-V VM↔host protocol and the encrypted credential pipeline — where no
foreign reader will ever touch the bytes.


### `choreo-sanitize` — Shared string-safety primitives

An internal leaf crate (`publish = true`, no workspace deps beyond
`unicode-general-category`) that is the single source of truth for two things
every consumer of tool output must agree on:

- **The Unicode "spoofing" predicates.** [`is_unsafe_unicode`] (line/paragraph
  separators U+2028/U+2029 plus every Unicode *format* character — general
  category Cf — except the joiners U+200C/U+200D) is used by the daemon's
  line-oriented sanitizers (`sanitize_keeps`), the TUI's terminal sink filter
  (`terminal_keeps` in `markdown_render.rs`), and the blockchain tools'
  node-output sanitizer. [`is_non_joiner_format_char`] is the Cf-only subset
  the LLM-transcript sanitizer (`sanitize_transcript`) escapes. The Cf set
  comes from the Unicode data tables, so newly-assigned format characters are
  escaped automatically on a crate bump; a code-space sweep test next to the
  predicates guards them against the tables.
- **The tool-output byte budget.** [`MAX_TOOL_OUTPUT_BYTES`] (128 KiB) and the
  shared `...[truncated]` marker (`TRUNCATION_MARKER` / `TRUNCATION_SUFFIX`),
  with [`truncate_tool_output`] / [`finish_tool_output`] applying the cap and
  [`ByteBudget`] tracking it incrementally on streaming paths. The daemon
  (`tools/mod.rs` re-exports them), the blockchain crate, and the client's
  live streaming cap (`history.rs`) all use these, so the final record, the
  streamed live view, and the client's live accumulation read identically.

Previously this logic was duplicated across `choreo-daemon`'s `tools/mod.rs`,
`choreo-blockchain`'s `lib.rs`, `choreo-tui`'s `markdown_render.rs`, and
`choreo-client-core`'s `history.rs`; consolidating it into one leaf crate means
a policy or budget change (or a Unicode table bump) is applied everywhere at
once, and the guard tests live next to the code they protect.


### `choreo-image` — Shared image decode helpers

A leaf crate (`publish = true`, depends only on `image` + `heif-oxide` +
`tracing`) that
owns the two decode paths shared by `choreo-daemon` (vision normalization for
`read_image`, and `display_image`) and `choreo-tui` (client display decode), so
the model path and the UI path can never drift apart. Its only log emission is
a `warn!`-level `tracing` event when the HEIC guard rejects a container, so a
rejected hostile input is observable without the crate owning any state:

| Function | Purpose |
|---|---|
| `decode_raster_oriented` | `image`-crate raster decode with EXIF orientation baked in (JPEG/WebP/PNG-`eXIf`), in one pass, under a decompression-bomb `image::Limits` guard (`MAX_SOURCE_DIMENSION` x-side, `MAX_DECODE_ALLOC`). |
| `decode_heic` | Pure-Rust `heif-oxide` HEIC/HEIF decode. Applies the container's orientation, delivers display-ready sRGB, and runs a **pre-decode allocation guard** (see below) — a rejection is logged via `tracing`. |

**HEIC decompression-bomb guard.** `heif-oxide` exposes no decoder limit and
allocates its YUV/RGB/RGBA buffers from file-declared geometry, so an
untrusted HEIC could otherwise drive a huge allocation before resize. The
crate pre-parses the container (`heif::heif_geometry`, in
`choreo-image/src/heif.rs`) for the geometry it allocates, without decoding
any pixels: every `ispe` (ImageSpatialExtentsProperty) extent — the per-item
frame size a single coded image or grid tile is decoded from — and every
`grid` derived item's canvas, read from the grid item payload located via
`iinf`/`iloc` (`rows`/`cols` × tile extent), which is the amplification
vector a per-item cap alone does not close. Any container whose declared
extent or canvas exceeds [`MAX_SOURCE_DIMENSION`] (or whose geometry cannot be
proved — no `ispe`, an unlocatable/unsupported grid payload) is rejected
before `heif-oxide` runs, the safe default. The box walk descends only into
the `meta`/`iinf`/`iprp`/`ipco` containers and is careful about **full boxes**
(`meta`/`iinf` carry a version/flags prefix + count), never descending into
`mdat` raw media data, so arbitrary payload bytes cannot cause a false
rejection.

### `choreo-keystore` — Identity keypair & credential crypto

Provides the cryptographic primitives for credential management. No longer a standalone
CLI binary — it is a library used by `choreo-client-core` and `choreo-daemon`.

**Identity keypair (X25519):**
The daemon's identity is an X25519 keypair stored as two files:
- `~/.config/choreographr/identity.pk` — raw 32-byte private key
- `~/.config/choreographr/public.pk` — raw 32-byte public key

The private key can be stored encrypted at rest:
- `~/.config/choreographr/identity.pk.enc` — Argon2 + AES-256-GCM encrypted private key

**Credential encryption pipeline (client-side):**
```
credential ──► postcard serialize ──► ECDH (ephemeral + recipient pubkey)
  ──► HKDF ──► AES-256-GCM encrypt ──► encrypted payload
```

Output format for each credential:
```
eph_public(32) || salt(32) || nonce(12) || ciphertext(rest)
```

Credentials are encrypted per-credential, using ECDH key agreement so only the
daemon (holder of the private key) can decrypt them. The encrypted blobs are stored
in the `redb` database alongside sessions.

**Modules:**

| Module | Purpose |
|---|---|
| `crypto.rs` | X25519 keypair generation, ECDH + HKDF + AES-256-GCM encrypt/decrypt, passphrase-based private key encryption; shared AES-256-GCM helpers |
| `paths.rs` | Resolves filesystem paths for identity key files |
| `error.rs` | `KeystoreError` enum |

**Credential types:** `ApiKey` (OpenAI), `X` (Twitter OAuth 1.0a credentials)
**Error type:** `KeystoreError` (thiserror enum) — `Io`, `TooShort`, `InvalidKeyLength`, `EncryptionFailed`, `DecryptionFailed`, `ConfigDirNotFound`


### `choreo-transport` — Noise IK encrypted transport

A small crate providing Noise IK handshake and encrypted message I/O over
TCP.  Used by both `choreo-client-core` (client side) and `choreo-daemon` (server side).

| Module | Purpose |
|---|---|
| `noise.rs` | `NoiseStream` — wraps `TcpStream` + `snow::TransportState` with length-prefixed AES-256-GCM framing. Payloads above snow's 65535-byte single-message ciphertext cap are split into fragments and reassembled transparently, so the effective per-message cap is now the proto codec's 32 MiB `MAX_FRAME_SIZE`. The reassembly decision is made from an AUTHENTICATED continuation byte embedded as the first byte of each fragment's plaintext (covered by the AES-GCM tag) — the 4-byte wire length prefix carries no continuation flag, so a wire-level tamper can never silently truncate or extend a message: any prefix flip either trips the size cap or fails the GCM authentication. The unauthenticated prefix is validated before any allocation (snow's 65535-byte ciphertext cap) and reassembly is capped at the codec's 32 MiB `MAX_FRAME_SIZE` (enforced on both send and receive), so a hostile or corrupted peer cannot force a huge buffer allocation. The shared `TransportState` lock is held only per-chunk during encryption, never during the blocking socket writes — together with the single-writer-per-connection discipline on the daemon, this prevents a bidirectional large-message deadlock (see `noise_concurrent_bidirectional_large_messages`). A runtime single-writer guard on `send_message` rejects a concurrent second sender instead of interleaving fragments. The data plane reuses per-stream buffers (`send_buf`/`send_frag`/`recv_ct_buf`/`recv_pt_buf`) so no buffer is allocated per message or fragment, and each frame is written as ONE coalesced `write_all` (4-byte prefix + ciphertext); an empty payload still emits a single real frame (a cleared continuation header), so `recv_message` never blocks forever on a missing length prefix. EOF-class read failures (the peer closing its end mid-read) surface as `TransportError::ConnectionClosed` rather than a raw `Io(UnexpectedEof)`, so the daemon's read loop logs a graceful disconnect instead of an error. The `Arc<Mutex<TransportState>>` and the `Arc<AtomicBool>` single-writer guard are shared across `try_clone` reader/writer clones — a deliberate, documented exception to the workspace's message-passing rule (the transport state must be shared for the clones to interleave encrypt/decrypt on one connection; the guard is a single-bit flag in the spirit of the sanctioned cooperative-cancellation-flag exception). |
| `handshake.rs` | Noise IK handshake (split out of `noise.rs`): `handshake_initiator()` (client) and `handshake_responder()` (server) implement Noise IK with X25519 key agreement over 2-byte-BE-length-prefixed handshake messages. The handshake is bounded by an ABSOLUTE deadline (an `Instant` budget enforced across every handshake read AND write — `read_handshake_exact` / `write_handshake_all` re-arm the socket timeout to the time *remaining* until the deadline, so a per-read timeout alone (resettable by a peer dribbling bytes) cannot stretch the total, and a peer that stops reading mid-handshake cannot hold the writer past the deadline; both timeouts are cleared before the data plane, which has no timeout by design): the ACL check happens mid-handshake, so a peer that connects and stalls — or dribbles to keep per-read timers from firing — must not be able to hold a connection thread + FD forever. Deadline expiry surfaces as `TransportError::HandshakeTimeout` (read/write `WouldBlock`/`TimedOut` map to it, since the socket is blocking and the timeout is armed to the remaining budget). Pinned by `noise_handshake_times_out_when_peer_silent` and `noise_handshake_times_out_against_dribbling_peer`. The budget is injectable: `handshake_initiator_with_timeout` / `handshake_responder_with_timeout` take their own `Duration` (the plain functions delegate to them with the 10 s default), so tests exercise the timeout path in milliseconds. |
| `error.rs` | `TransportError` enum — `Io`, `Noise`, `Protocol`, `InvalidFragment`, `HandshakeTimeout` (absolute-deadline expiry), `AuthFailed`, `ConnectionClosed` (peer closed the connection mid-read; classified from EOF/reset kinds by `noise::recv_message`). |
| `key.rs` | Transport keypair handling — `TransportSecretKey` (type-safe X25519 secret), `ensure_transport_keypair()` (generate-or-load with advisory file locking), `read_server_pk()`. `set_test_config_root()` is the keypair-directory test override, now `pub` (and `#[doc(hidden)]` — a test seam, not part of the public contract) so integration tests can redirect keypair generation to a temp dir — matching the `choreo_keystore::paths` / `choreo_daemon::mcp::config` precedent. |

The server-side TCP/Noise handler lives in `choreo-daemon/src/server/connection.rs`
(`tcp_client_thread`), where the Noise IK handshake is performed and the
encrypted stream enters the same dispatch loop as Unix socket clients.


### `choreo-client-core` — Shared client logic

Used by `choreo-tui`, `choreo-gui`, and `choreo-im`.

| Module | Purpose |
|---|---|---|
| `shell.rs` | Parses terminal input into `ShellCommand`: `/ping`, `/models`, `/model` (alias), `/cancel`, `/unlock`, `/lock`, `/image`, `/add-key`, `/add-x`, `/remove-key`, or `RunInput(prompt)`. All commands use `/` prefix exclusively; `parse_command()` is the single dispatch point. |
| `credentials.rs` | Shared helpers: `resolve_private_key()` (read or decrypt the identity key), `build_add_credential_message()` (encrypt and package a credential for the daemon), `read_public_key_bytes()`. Eliminates duplicated logic across `choreo-tui`, `choreo-gui`, and `choreo-im`. |
| `image.rs` | `ImageAssembler` — kept for legacy `choreo-im` use. No longer used by TUI/Dioxus (images delivered mid-turn as `DisplayedImage` via `SessionMessageAppended`). |
| `history.rs` | `ClientHistory` ring buffer of `HistoryItem` entries (text, images, session messages, streaming text, structured diffs) |
| `diff.rs` | Types for structured unified diff representation (`DiffLineKind`, `DiffLine`, `DiffHunk`, `FileDiff`) |
| `dispatch.rs` | `TurnEventHandler` trait + `dispatch_daemon_message()` — splits the v4 `DaemonMessage` into its two families before any per-arm work: `dispatch_session_event` (the inner `SessionEvent` of the `Session` envelope, with the origin resolved exactly once; the six None-capable events — `Failed`, `Cancelled`, `ModelSelectionFailed`, `ReasoningEffortSet`/`Failed`, `SessionFailed` — are pre-handled so a `None` origin (connection-level reply with no session) surfaces its status/error instead of being dropped, while every other event hard-requires `Some` via a guard that `warn!`s if a producer ever emits a session-scoped event without an origin) and `dispatch_flat_message` (all 23 flat connection/reply/global variants enumerated explicitly — no wildcard arm — so a new `DaemonMessage` variant must be triaged at compile time, matching the no-`#[non_exhaustive]` wire-contract rule). Used by all UI clients (TUI, GUI, IM bridge) to avoid duplicating the routing logic. |
| `connection.rs` | Daemon connection helpers: `run_daemon_connection()` (Unix socket), `run_daemon_tcp_connection()` (Noise IK), `run_daemon_connection_with_mode()` (dispatch), `run_daemon_reader()` (blocking reader). `ConnectionMode` enum (`UnixSocket` | `Tcp`) selects the transport. |

`TurnEventHandler` is the `choreo-client-core` dispatch sink; the `ClientError` type used by the connection layer is a thiserror enum — `Proto`, `Io`, `Utf8`, `ImageTooLarge`, `ImageExceedsSize`, `DuplicateImage`, `UnknownImage`, `ImageSizeMismatch`, `PrivateKeyRead`, `PrivateKeyInvalid`, `PrivateKeyEncRead`, `PrivateKeyDecrypt`, `PublicKeyRead`, `PublicKeyInvalid`, `CredentialParse`, `Postcard`, `Encryption`.


### `choreo-mcp` — MCP client (Model Context Protocol)

Communicates with MCP server subprocesses over JSON-RPC 2.0 stdio transport.
Used by `choreo-daemon` to spawn external MCP servers and register their tools.

| Module | Purpose |
|---|---|
| `client.rs` | `McpClient` — spawns a subprocess, performs MCP initialize handshake, discovers tools (`list_tools`), and dispatches tool calls (`call_tool`) |
| `protocol.rs` | JSON-RPC 2.0 wire types (`JsonRpcRequest`, `JsonRpcResponse`) and MCP protocol types (`McpTool`, `CallToolResult`, `McpContent`) |
| `transport.rs` | `StdioTransport` — manages a subprocess stdin/stdout, routes incoming JSON-RPC lines to response/notification channels |
| `error.rs` | `McpError` enum — `SpawnFailed`, `InitializeFailed`, `JsonRpcError`, `ProtocolError`, `Timeout`, `Io`, `ServerShutdown`, `ToolNotFound`, `InvalidParams` |


### `choreo-ai-protocols` — Provider protocols

The wire-protocol layer for LLM providers.  Owns the three client
implementations (`openai`, `anthropic`, `google`), the `ProviderClient` trait
and shared turn/error/message types they all use, the retry machinery, and
the static provider catalog.  It is free of daemon concerns — no metrics, no
account configuration, no sessions — so it can be consumed independently of
`choreo-daemon` (the daemon supplies those concerns at the boundary, e.g. via
[`ProviderOverrides`] and by timing calls itself).

| Module | Purpose |
|---|---|
| `openai/` | OpenAI-compatible client (`OpenAiClient`) covering both Chat Completions and Responses APIs (incl. programmatic tool calling), plus the canonical `ChatRequestMessage` / `ChatToolDefinition` types that the other clients translate into their own wire formats, `ServiceConfig`, SSE reader, and per-protocol retry helpers. Non-2xx HTTP bodies and Responses `response.failed` events both surface the provider's error `message` (the OpenAI error object `{code, message, param, type}` is decoded; a plain-string `error` or the serialized object is the fallback) so a mid-stream failure is never a blank error. For the opencode gateway providers (`provider_slug` exactly `"opencode"` or `"opencode-go"`), `ServiceConfig::opencode_request_headers()` attaches a fixed `x-opencode-session: choreographr` header to every request: the opencode.ai zen/go gateway selects a weighted upstream provider by hashing that header (or the API key's workspace id when absent), so a constant value pins routing to a working provider instead of deterministically hitting a broken one. |
| `anthropic/` | Anthropic Messages API client (`AnthropicClient`, `AnthropicConfig`). Implements `ProviderClient`. |
| `google/` | Google Gemini API client (`GoogleClient`, `GoogleConfig`). Implements `ProviderClient`. Uses its own SSE reader for streaming. |
| `traits.rs` | `ProviderClient` trait, `ChatTurnRequest`, `ToolResultItem` |
| `types.rs` | `ChatTurnResult`, `ChatToolCall`, `ChatAssistantToolUse`, `FinalTextResult`, `CallerInfo`, `StreamEvent` — the turn-result structs carry the `reasoning_artifact: Option<ReasoningArtifact>` captured at the parse boundary |
| `shared.rs` | `ProviderError` (unified error type re-exported as `OpenAiError`/`AnthropicError`/`GoogleError`), `MaxTokensField`, `MAX_TOOL_CALLS`, `build_agent()` (applies three timeouts: connect, idle `request_timeout_secs` per chunk, and a `total_timeout_secs` wall-clock deadline per attempt via ureq's `timeout_global` — each retry restarts it), `provider_error_to_inference()`, `emit_non_streaming_events()`, `list_models_with_fallback()` |
| `context_window.rs` | `ContextWindowConfig` — per-model/global context window resolution shared by all configs |
| `overrides.rs` | `ProviderOverrides` — protocol-agnostic account overrides carrier (the daemon converts its `AccountConfig` into this) |
| `retry.rs` | Shared HTTP retry logic. `ProviderHttpError` enum captures HTTP error codes generically; `retry_loop()` provides exponential backoff with jitter, retryable status detection, cancellation support, and a per-attempt wall-clock deadline (`AttemptDeadline`, re-armed at the start of every attempt). `AttemptContext` bundles the per-call retry inputs (`on_retry` callback, `cancel_rx`, `attempt_deadline`) so the retry entry points do not grow a parameter per knob. Non-2xx bodies are summarized by `extract_error_message` for display only — the envelope-unwrapping logic (`error.message` for OpenAI-compatible APIs, Anthropic, Gemini; plain-string `error` or top-level `message` for compat layers) falls back to the verbatim body when the body is not JSON or carries no recognizable message. **The retry decision is driven entirely by the HTTP contract — status code and `Retry-After` header — never the response body.** `is_retryable_status` curates the statuses the contract defines as potentially transient (429, 500, 502, 503, 504); every other 4xx (400/401/402/403/404/422/…) is terminal because the server rejected this exact request (bad input, missing auth, no balance, missing entitlement). 5xx and transport errors are retried; a 429/503 is retried only when its `Retry-After` fits within the backoff budget (`retry_decision` — the merged budget-gate + delay — returns `Some(delay)` to wait, `None` to fail now) — the two statuses RFC 7231 defines the header for. When the server's stated cooldown exceeds the ceiling, the wait outlives any delay the policy would ever make, so the response is treated as terminal instead of burning attempts and delaying the real error; the decline is logged (`warn!`) with the status, `Retry-After`, and ceiling so the fail-fast path is explainable. `parse_retry_after_secs` reads both RFC 7231 forms — delta-seconds verbatim and HTTP-date (IMF-fixdate, via chrono) as the remaining seconds from now. In-budget `Retry-After` values are waited verbatim (no jitter); statuses without a defined `Retry-After` (500/502/504) ignore the header and back off exponentially. The retry budget is enforced in three layers: `RetryConfig::new` (the constructor all providers use) clamps `max_backoff_ms` to `MAX_BACKOFF_MS` (1 h — the "no wait is ever worth more than an hour" invariant) and never lets it fall below `initial_backoff_ms`, emitting a `warn!` when a clamp fires — deduplicated to once per distinct bad value per process so a misconfigured programmatic caller cannot spam the log (`WARNED_CLAMPS`, a cold `LazyLock<Mutex<HashSet>>` holding only the raw pairs already warned about); `retry_decision` clamps the ceiling again at compare time, so even a hand-built `RetryConfig` (the fields are `pub` and bypass the constructor) cannot void the gate; `wait_before_retry` hard-caps any single wait at the same ceiling as the final backstop; and the daemon's `AccountConfig::validate()` rejects over-ceiling or inverted (`initial > max`) values at accounts-file load / add with a pinpointed message. The status lives on the error variant itself — every variant (incl. `RateLimited`, which carries `status` like the others) renders it in its Display — so the `detail` string carries only the provider's message and is never duplicated, and the shape is uniform whether or not the body parsed (e.g. `client error (402): Insufficient Balance` and `rate limited (429): Quota exceeded`). |
| `stream.rs` | Cancellable SSE reader plumbing: `spawn_sse_reader()` runs the blocking socket read on a dedicated thread and forwards parsed events through a bounded crossbeam channel (backpressure — the reader blocks on `send` instead of buffering unboundedly); an abort signal stops the thread at its next loop boundary on cancel/drop; `recv_sse_event()` waits event-driven with `select_biased!` on the event channel, the cancellation channel, and an exact timer for the remaining budget — no polling, so Escape and deadline expiry interrupt a stalled/trickling stream the moment they happen instead of blocking forever inside `read()`. The per-attempt wall-clock deadline is supplied by the caller (`retry::AttemptDeadline` — armed *before* the request is sent and re-armed on each retry, so it spans DNS → connect → headers → body; the real backstop — ureq's `timeout_global` is floored at ~1 s per socket read, so sub-second keep-alive trickles could otherwise evade it). Deadline expiry surfaces as a dedicated `ProviderError::DeadlineExceeded` (non-retryable, distinct from a socket `Io` error). |
| `catalog/` | Two-layer provider catalog pipeline — a **models.dev base** (a local, gitignored `catalog/models.dev.json` snapshot — fetched by `catalog-gen` when absent — normalized into an embedded postcard blob `catalog/catalog.bin`; deserialized by `load_bundled_base`) and a **bundled overlay** (`catalog/models-overlay.toml`, `include_str!` + merged at load time via `merge_overlay`; `normalize_modelsdev` runs the snapshot→base normalization; `bundled_overlay_src` exposes the overlay source so the daemon can re-merge it at runtime). `refresh.rs` owns the models.dev **conditional GET** (`fetch_modelsdev`, `If-None-Match` / `Cache-Control: no-cache` for `--force`, structured `RefreshError`, `RefreshOutcome::{NotModified, Fetched}`). `persist.rs` owns the shared atomic `write_file_atomic` (temp → fsync → rename + parent-dir fsync) used by `catalog-gen` and the daemon cache writer. `PROVIDER_CATALOG` is an `ArcSwap`-backed runtime-swappable global lazily initialized from `loader::load_catalog()`. Lookups: `lookup_provider` → owned `ProviderEntry` clone, `lookup_context_window`, `model_reasoning_capability`, `model_reasoning_passback`, `model_request_format`, `model_supports_vision` (the vision gate — whether a model accepts image input, derived from the models.dev `modalities.input` array at ingestion and overridable via the overlay), `all_slugs` → `Vec<String>`, `all_display_names` → `Vec<String>`. `ModelEntry` carries `reasoning_passback` (per-model override; `None` derives from the protocol) and `supports_vision` (per-model vision flag). `replace_catalog` atomically swaps the whole catalog (single writer: the daemon command loop) and `catalog_snapshot` returns an `Arc<Vec<ProviderEntry>>` pinning one version. |

**Root re-exports** give consumers a stable front door: the client types
(`OpenAiClient`, `AnthropicClient`, `GoogleClient`, …), the trait and shared
types (`ProviderClient`, `ChatTurnRequest`, `ChatTurnResult`, `StreamEvent`,
`ProviderError`, `ContextWindowConfig`, `ProviderOverrides`, …), and the
catalog (`ProviderProtocol`, `PROVIDER_CATALOG`, `lookup_*`, …).

**Threading — the catalog `ArcSwap` exception.** The `PROVIDER_CATALOG`
`ArcSwap` is a documented exception to the repo's channel-only
thread-communication rule (see AGENTS.md): readers are lock-free and the swap
is an atomic `store()`, but there is a strict **single-writer invariant** —
only the daemon command loop calls `replace_catalog` (after a catalog refresh,
overlay change, or `/refresh-models`). Every *change request* still travels by
channel (maintenance thread → daemon loop → store); no other thread mutates the
catalog. All other cross-thread communication in this crate (and the daemon)
uses mpsc channels.

**Threading — the catalog maintenance thread (S4).** One background thread
(`choreo-daemon/src/catalog.rs`, spawned by `run_server` before the accept
loop) owns the whole runtime pipeline but never mutates the catalog. It is
**channel-driven**: it multiplexes — via `crossbeam_channel::select!` — the
maintenance channel (the daemon command loop's `/refresh-models` requests,
sent as `MaintenanceEvent::RefreshNow`) with the config transport's
`models-overlay.toml` subscription, and the select's `after(timeout)` arm
doubles as the revalidation timer. After every refresh outcome — a successful
fetch, a 304, or a failure — the next conditional GET is armed
[`REFRESH_ATTEMPT_INTERVAL`] (25 h) out, so the cache keeps a steady
freshness cycle and a failure never spins. The 25 h period (rather than 24 h)
makes each daemon's fetch time drift +1 h/day, so across a population of
daemons the load wraps around the daily cycle instead of piling onto working
hours. **Refresh pacing is anchored on a wall-clock attempt timestamp
persisted in the DB** (`catalog_state` table): the maintenance thread records
it BEFORE every fetch (crash-safe — a daemon that dies mid-fetch and restarts
reads a fresh timestamp and honors the remaining cooldown), every outcome
counts (200/304/failure), and the cooldown survives restarts. At startup the
thread fetches immediately iff there is no valid cache, no recorded attempt,
or the attempt is stale; otherwise it skips the network hit and arms the
in-run timer for the remaining time (monotonic within a run — suspend pauses
the countdown; a restart re-derives from the DB anchor in strict wall time).
A burst of `/refresh-models` requests is **coalesced** into one fetch
(fold the force flags — the shared fetch is forced if ANY requester asked —
but each requester's reply status reflects its OWN flag, so a plain request
folded into a forced burst is reported `Updated`, not `Forced`), the whole
burst is ONE recorded attempt, and `/refresh-models` bypasses the cooldown
but still records the attempt (so the DB anchor reflects reality). The
`/refresh-models` path also **re-reads the user overlay** (fingerprint-gated,
shared with the watcher) so it is the documented reload fallback when the
config transport could not start. The **filesystem watching itself lives in
the unified config transport** ([`crate::config_watch`]), which owns the
config-dir creation (so the watch installs on a fresh system), the `notify`
watch, and the re-arm retry; the maintenance thread merely reacts to the
overlay events it fans out. All policy (basename filter, re-read, fingerprint
compare) lives on the maintenance thread. On startup the thread loads the base
(cache file → embedded `catalog.bin`), reads the user overlay, sends
`CatalogBaseChanged` to the command loop (which merges + swaps + broadcasts),
then runs the conditional GET against models.dev (gated as above). The thread
is detached: the process exits after `run_server` returns, and its sends to
the daemon channel fail harmlessly once the command loop is gone.


### `choreo-acp` — ACP bridge (Agent Communication Protocol)

Entry point: `src/main.rs` → initializes logging, connects to the daemon's Unix socket,
spawns I/O threads, runs the main event loop.

The ACP bridge translates the **Agent Communication Protocol** (JSON-RPC 2.0 over
stdin/stdout) into `choreo-proto` messages sent to the daemon over its Unix socket.
This allows ACP-compatible editors (Claude Code, Cline, etc.) to manage Choreographr sessions,
send prompts, and receive streaming responses as if they were native Choreographr clients.

**Thread topology:**

```
main()
├── acp-reader thread: reads newline-delimited JSON-RPC lines from stdin,
│   parses them, sends parsed RpcMessage into the shared event channel
├── daemon-reader thread: reads DaemonMessages from the daemon socket,
│   forwards them into the shared event channel
├── daemon-writer thread: receives ClientMessages via mpsc and writes
│   length-prefixed MessagePack frames to the daemon socket
└── main thread: event loop — receives from the shared event channel
    and dispatches to the appropriate handler
```

**Modules:**

| Module | Purpose |
|---|---|
| `main.rs` | CLI arg parsing (socket path, log file), thread spawning, logging setup |
| `acp_jsonrpc.rs` | JSON-RPC 2.0 wire types (`JsonRpcRequest`, `JsonRpcResponse`, `JsonRpcNotification`, `RpcMessage`) and ACP protocol payload types (`InitializeResult`, `ConfigOption`, `ContentBlock`, `SessionUpdateParams`, etc.) |
| `acp_reader.rs` | Blocking stdin reader thread with 1 MiB line-length limit |
| `acp_handler.rs` | Event loop dispatch — routes `RpcMessage` to ACP method handlers (`session/new`, `session/prompt`, `session/delete`, etc.), routes `DaemonMessage` to streaming or sync response handlers |
| `daemon_client.rs` | Unix socket connection, daemon reader/writer thread spawning, shared `Event` enum |
| `sessions.rs` | `SessionManager` — bidirectional map (ACP session ID ↔ daemon session ID), active-prompt guard, request ID counter |
| `pending.rs` | `PendingRequests` — tracks in-flight sync requests and streaming prompts |
| `config.rs` | Builds ACP `ConfigOption` objects from daemon state (model list, reasoning effort, tool groups) |
| `streaming.rs` | Translates daemon `OutputChunk`/`ToolCallStarted`/etc. into ACP `session/update` notifications |
| `client_capabilities.rs` | Stores editor-declared capabilities from the `initialize` handshake |
| `error.rs` | `AcpError` enum — JSON-RPC errors, daemon connection, session errors, I/O, proto, serde |

**Key behaviors:**

- **Concurrency:** Pure OS threads with `mpsc` message passing. No `Arc<Mutex>` shared state.
  Threads communicate exclusively through a single shared event channel (`mpsc::Receiver<Event>`).
- **Session lifecycle:** Sessions are created on the daemon via `CreateSession` and tracked locally
  in `SessionManager`. `session/close` cleans up local state only (the daemon keeps sessions alive
  until explicitly deleted). `session/delete` sends `DeleteSession` to the daemon and waits for
  confirmation before removing local state.
- **Streaming:** Prompt responses are streamed from the daemon as `OutputChunk` events, translated
  to ACP `session/update` notifications, and finalized with a JSON-RPC response on `Done`/`Failed`/`Cancelled`.


### `choreo-content` — Coordination Platform client

Implements the feature-gated `content` tool group (compiled only behind the
daemon's `content` cargo feature — off by default; a plain build registers no
content tools, and persisted sessions carrying the stale pre-rename `coord`
group name silently ignore it) against the Choreographr
Coordination Platform: a Substrate content registry (publish/retract items,
revisions, profiles, account pins) with content stored on IPFS and revisions
resolved through an event indexer. Reads are indexer-first with on-chain
authority for control state; writes encode content → pin to IPFS → submit the
extrinsic. Only the subxt submit path uses the tokio sidecar; IPFS (`ureq`)
and the indexer (`tungstenite`) are synchronous.

| Module | Purpose |
|---|---|
| `encode.rs` | Content protocol: protobuf `ItemMessage` mixin encode/decode (pinned to the reference `acuity-dioxus` mixin IDs), deterministic item-id derivation, CID ↔ sha2-256-digest helpers. `ContentInput` is the caller-facing shape — `image: ImageInput` accepts either a local file `path` or a complete `spec`; `encode_item` consumes the resolved `PreparedContent`. |
| `image.rs` | Publish-time image pipeline (ported from `acuity-dioxus`'s `build_image_mixin`): reads a local image, encodes a JPEG q82 mipmap pyramid (level 0 = full res, halved via Lanczos3 until a dimension ≤ 64), pins every level to IPFS as its own CID, and derives the full `ImageSpec` (dimensions, original-file sha2-256 digest, per-level sizes) so callers of `coord_publish_item`/`coord_publish_revision`/`coord_set_profile` only pass a `path` (the tools keep their pre-rename `coord_*` names; only the group is renamed to `content`). |
| `ipfs.rs` | Blocking IPFS client (`api/v0/add` with pin, `cat`, `id`) over `ureq` with bounded timeouts. |
| `indexer.rs` | Synchronous WebSocket JSON-RPC client for the event indexer (keyed event queries, index status). |
| `chain.rs` | subxt extrinsic submission + state reads, signed with the keystore's Substrate credential. |
| `orchestrate.rs` | The tool-facing read/write pipelines, including `resolve_content()` — the layer that turns a `path`-based `ImageInput` into a full `ImageSpec` via `image.rs` before encoding. |
| `runtime.rs` | The tokio sidecar runtime used only to drive subxt futures. |


### `choreo-daemon` — Core server (binary `choreographr`)

Entry point: `choreo_daemon::main` — invoked from the root package's
`src/bin/choreographr.rs` wrapper — initializes tracing, creates
`DaemonState`, runs socket server.

**Concurrency model:** Pure OS threads with message passing (actor model). No async code
in the daemon's own logic. All I/O uses blocking `std` APIs on dedicated threads. The one
exception is the optional `blockchain` and `content` features: `choreo-blockchain` (linked only with the former) and
`choreo-content` (linked only with the latter) each hold a tokio sidecar runtime for their async
alloy/subxt clients, and the daemon calls their synchronous `execute_*` entry points (which
`block_on` internally).

**Module breakdown:**

| Module | Purpose |
|---|---|---|
| `broadcast.rs` | The ONE shared subscriber-broadcast policy: **lossless delivery with lag-based eviction**. Each subscriber is a [`SubscriberSink`](choreo-daemon/src/broadcast.rs) — an UNBOUNDED crossbeam channel plus an `Arc<AtomicUsize>` in-flight byte counter (the 6th sanctioned shared-state exception, see AGENTS.md) — so the daemon never drops a broadcast and a slow subscriber can never stall a session thread or the command loop. Every enqueue (`enqueue`, `send_unchecked`, and the connection thread's `send_to_writer` via the shared `send_accounted` core) increments the per-client and daemon-wide (`global_lag`) counters before the send and self-corrects both when the send fails on a dead receiver; the connection's writer thread decrements both on every dequeue and drains-and-decrements whatever is still queued when it stops early, so the accounting stays balanced to within one bounded exit-window straggler (see the module docs). Crossing [`LagLimits`](choreo-daemon/src/broadcast.rs) (`per_client_cap` 64 MiB, `global_budget` 512 MiB, injectable for tests) reports `ClientOverLag`/`GlobalOverBudget` and the fan-out caller evicts the client(s) via `DaemonCommand::EvictClient`/`EvictLargestLagging`. The retain-and-evict collection itself is the single `fan_out_evicting` helper, shared by ALL THREE subscriber fan-outs (daemon summary, daemon all-activity, per-session) so the policy cannot drift between paths. `SessionStatusChanged` is the one message that would otherwise flow through all three for a single event — the session thread broadcasts it per-session AND forwards it to all-activity AND sends the summary command — so the summary fan-out (in `daemon/subscriber_handlers.rs::handle_broadcast_session_status`) skips clients that already received it via the per-session or activity fan-outs, making delivery exactly-once per client class (pinned by `handle_broadcast_session_status_dedups_against_session_and_activity_subscribers`). Daemon-generated LIFECYCLE events with NO session-thread path — `SessionCreated`, `SessionDeleted`, and the exit `Sleeping` status — are the one class that cannot dedup against a `BroadcastActivity` forward, so `DaemonState::broadcast` (in `daemon/subscriber_handlers.rs`) delivers them to BOTH the session-summary subscribers AND the all-activity subscribers: the summary fan-out skips all-activity clients that the activity fan-out already served, so an activity-only client now learns of sessions being created/deleted while a dual subscriber still gets exactly one copy (pinned by `broadcast_lifecycle_delivers_to_summary_and_activity_exactly_once_per_client`). The all-activity fan-out has its own duplicate-suppression in `DaemonCommand::BroadcastActivity { session_id: Option<u64>, msg }` / `handle_broadcast_activity`, keyed on **explicit provenance, not message shape**: the origin session id travels on the command — `Some` for session-originated broadcasts (the sending session thread knows its own id), `None` for global/control broadcasts — and `handle_broadcast_activity` passes a `should_skip` closure to `fan_out_evicting` that skips clients which are also direct subscribers of that origin session (they receive the event through the per-session path instead). Every session-scoped event still rides the `DaemonMessage::Session { session_id: Option<u64>, event }` envelope, so the origin is present on the wire too, but the filter reads ONLY the command field — a `Some(42)` origin suppresses delivery of any message, whatever its shape, to a session-42 subscriber (pinned by `handle_broadcast_activity_dedup_keyed_on_command_origin_not_message_shape`). Because the command provenance and the message origin must AGREE — a `Some` origin on a NON-session message would drop it for the origin session's direct subscribers on BOTH paths (deduped on the activity path and never carried on the per-session path), and a `Session` envelope whose own `session_id` differs from (or contradicts) the command's would dedup against the wrong session — `handle_broadcast_activity` guards the contract with a `warn!` tripwire: `violates_broadcast_origin_contract` in `subscriber_handlers.rs` (also flags a `None` command origin on a session-scoped envelope, which would double-deliver to the envelope origin's direct subscribers), pinned by `broadcast_origin_contract_requires_agreeing_provenance` — so a future producer that forwards a non-session message with `Some`, or an envelope whose origin disagrees with the command, fails loudly instead of silently mis-routing messages (no current producer does this). `approx_wire_size` (choreo-proto, `size.rs`) is the gauge — deliberately an over-estimate, pinned by `approx_wire_size_never_underestimates_encoded_payload`. |
| `server/lifecycle.rs` | Accept loop (blocking `UnixListener` + signal-wakeup), signal handling, shutdown orchestration. On graceful shutdown, `DaemonMessage::ShuttingDown` is routed through each connection's single writer thread (via a `client_writers` registry in the daemon command loop); the writer thread flushes it and closes its own socket, so a client observes the notification before the EOF. The accept-loop thread never writes to or closes client sockets — there are no retained stream clones and no backstop close pass, so the notification cannot be lost to a race with a socket close. With the lossless unbounded writer channels an enqueue can never be `Full` (the old bounded round-robin fan-out for full channels is dead code); a wedged writer is bounded by its 5 s socket write timeout plus the writer-join grace. The writer channel is created and REGISTERED with the daemon before the connection thread is spawned (see the `server/connection.rs` row's `register_client_writer`), so a connection accepted concurrently with shutdown is guaranteed to be in the registry when the broadcast is processed — the register is ordered before the broadcast on the FIFO command channel (Unix: same thread; TCP: the accept thread is joined before the broadcast). Before returning, `run_server` also waits — bounded, 5 s — for the TCP accept thread (woken by a probe connect to the listener's actual bound address, falling back to loopback for unspecified `0.0.0.0`/`::` binds, so a concrete non-loopback bind is woken too; the probe's own connect is bounded by a 1 s `connect_timeout` so a full accept backlog cannot stall shutdown on the kernel's SYN-retry timeout) and then for each connection thread (Unix handles tracked directly, TCP handles ferried back over a channel) and, through it, its writer thread, to flush the notification and close its own socket, so notify-before-EOF holds even when `run_server` is embedded in-process rather than exiting the process. Connection handles are pruned eagerly once the retained Vec grows past 64, so a long-running daemon does not accumulate one `JoinHandle` per connection ever accepted (pinned by a unit test). Live connections are also capped at 256 (`MAX_CONCURRENT_CONNECTIONS`, both transports combined, pinned by unit tests + an integration test): at the cap a newly-accepted connection is dropped immediately (bare EOF) so wedged-but-open clients cannot exhaust thread/FD resources. `cleanup_client`'s writer join is bounded (5 s `WRITER_JOIN_GRACE`) so a writer wedged in a blocking socket write cannot hang its connection thread's cleanup; a timed-out writer is detached and the shutdown drain remains the backstop. The cap is enforced with a daemon-wide `Arc<AtomicUsize>` live-connection counter (RAII `ConnectionSlot`) shared across both accept paths and every connection thread's exit — a single-purpose, lock-free resource-accounting exception to the workspace's message-passing rule (documented in AGENTS.md). Signal handling is channel-driven on both platforms: Unix blocks in `signal_hook`'s iterator (self-pipe); Windows has no sigwait/iterator, so `low_level::register` forwards SIGINT/SIGTERM as channel messages and the handler thread blocks in `recv()` — both wake the accept loop via a connect to the daemon's own socket, with no flag polling. |
| `server/connection.rs` | Per-client `client_thread` (Unix) and `tcp_client_thread` (TCP/Noise) — read `ClientMessages` from the socket, dispatch via `daemon_tx` mpsc channel. Single-writer discipline: each connection has exactly one writer thread draining its `writer_rx`; `ShuttingDown` AND `Evicted` are special-cased messages that make the writer flush, close the socket, and stop draining (notify-before-EOF / advisory-before-EOF), and no other thread ever writes to the socket. The writer loops of both transports share one implementation (a `ConnectionWriter` trait + `writer_thread`), so the sole-writer contract and the special-case closes live in exactly one place — pinned by unit tests with a mock `ConnectionWriter` (flush `ShuttingDown`, flush `Evicted`, stop on send error, end cleanly on disconnect). The writer channel is an UNBOUNDED crossbeam channel (the connection's `SubscriberSink`), created and registered with the daemon by `register_client_writer`, called by the acceptor BEFORE the connection thread spawns (see the `server/lifecycle.rs` row) — a failed TCP handshake unregisters via `ClientDisconnected` so the registry stays honest. The writer thread decrements the sink's in-flight byte counter (and the daemon-wide `global_lag`) on every dequeue, the exact counterpart of the producers' enqueue increment; on a send error (broken pipe, or the 5 s `WRITER_WRITE_TIMEOUT` on a wedged client whose receive window is zero) it shuts the socket down itself — unblocking the reader's blocking read so `cleanup_client` reaps the connection. Replies are written through `send_to_writer`, an unbounded `send_accounted` call that still increments (and self-corrects on a dead receiver) the byte counters (replies were never dropped — a blocking send just blocked; with unbounded channels they can no longer block either). Session-summary subscription is an explicit client decision on both transports: a client opts into `SessionCreated`/`SessionStatusChanged`/`SessionDeleted` push broadcasts with `SubscribeSessionsSummary` (previously `tcp_client_thread` auto-registered every Noise client on connect; the GUI now sends the subscribe message at connect to keep its session list live). `cleanup_client` joins the connection's writer thread with a 5 s bound (`WRITER_JOIN_GRACE`) so a writer wedged in a blocking socket write cannot hang the connection thread's cleanup — a timed-out writer is detached and exits on its own once the client goes away. |
| `daemon.rs` | `DaemonCommand` handler loop on a dedicated thread — session CRUD, attach/detach, listing, locking, account management (incl. `AccountsReload`, the external-edit watcher consumer: re-reads `accounts.toml`, parse-compares against the in-memory manager, applies a real change, drops cached providers for removed accounts and rebuilds them for modified ones, and broadcasts the fresh list), and the runtime catalog swap (`CatalogBaseChanged` → `replace_catalog`, the single writer of the `PROVIDER_CATALOG` ArcSwap), `CatalogUpdated` broadcasts, and `/refresh-models` plumbing (the fetch is delegated to the maintenance thread, never run here). `DaemonState` is owned by this thread only (no shared state). The per-client subscriber/eviction methods — registration, the lossless broadcast fan-outs, lag eviction, shutdown notification, and disconnect cleanup — live in the child module `daemon/subscriber_handlers.rs` (`pub(super)` methods on `DaemonState`; `handle_command` dispatches their `DaemonCommand` variants here) so this file stays focused on core command handling. |
| `accounts/` | `AccountManager` — loads/saves `accounts.toml`, manages named inference accounts with per-account config overrides. `save` is **deterministic** (accounts sorted by name) and **atomic** (temp + fsync + rename via `write_file_atomic`), so the config watcher can never observe a torn file and identical logical state always serializes to identical bytes. `spawn_accounts_watcher` is the thin consumer that forwards `accounts.toml` edits surfaced by the config transport to the daemon command loop as `DaemonCommand::AccountsReload` (it does no reading itself). `AccountConfig` applies OpenAI-specific overrides directly to `ServiceConfig` (including `total_timeout_secs`) and converts the shared fields into `ProviderOverrides` for the other protocols. `AccountConfig::validate()` is Layer 3 of the retry budget: it rejects `retry_max_backoff_ms` past the 1 h ceiling (`MAX_BACKOFF_MS`, re-exported from `choreo-ai-protocols`), `retry_initial_backoff_ms` past the same ceiling even when no `max` is set (otherwise the library clamp would silently widen the budget gate), and inverted `retry_initial_backoff_ms > retry_max_backoff_ms` at accounts-file load and at `add`, so a typo'd config is refused with a pinpointed message (see the `retry.rs` row for the other two layers). |
| `config.rs` | Daemon-level configuration: `DaemonConfig` (`max_turns`, `[context]`), `config_path()`, `load_daemon_config()`, and the deprecated `load_service_config()`. (Previously lived in `openai/config.rs`; it is daemon config, not provider config.) |
| `config_watch.rs` | The **unified config-file watching transport**: ONE `notify` watcher on the config directory (`$XDG_CONFIG_HOME/choreographr`) fanned out per-basename to consumers over their own crossbeam channels (`ConfigWatcher::subscribe`, `ConfigChange`/`ChangeKind`). It is **transport only, no policy** — it owns the config-dir creation (so the watch installs first-time on a fresh system), the directory watch (rename-safe), the re-arm retry while unarmed, and basename + coarse-kind filtering (`classify` strips `Access`/`Other` noise); it never reads a file or mutates any state. Consumers (the catalog overlay reload and the accounts watcher) subscribe to the basenames they care about and own their reload policy, forwarding reload requests to the daemon command loop — the single writer of whatever they govern. Spawned once by `run_server` before the consumers. |
| `catalog.rs` | Runtime catalog maintenance (S4): `CatalogPaths` (XDG data/config locations for the cache bin + user overlay), atomic cache persistence (temp → fsync → rename, `catalog.bin` postcard), the ONE background **maintenance thread** (ensures the cache data dir, loads the cache → embedded `catalog.bin`, reads the user overlay, runs the startup conditional GET — **gated**: fetch immediately iff no valid cache / no recorded attempt / stale, else skip and arm the timer for the remaining time — records the DB `catalog_state.last_attempt_ms` BEFORE every fetch (the crash-safe 25 h cooldown, single writer of the attempt timestamp), serves `/refresh-models` requests with **coalescing** of bursts into one fetch (per-requester status; one recorded attempt per burst), and reacts to overlay edits from the config transport — all channel-driven, multiplexed via `crossbeam_channel::select!` whose `after(timeout)` arm doubles as the revalidation timer, and never mutating the catalog itself), plus the **fingerprint-gated overlay reload** (pure compare collapses editor save-event storms; shared by the config transport and the `/refresh-models` path). Every change is delivered to the daemon command loop as `DaemonCommand::CatalogBaseChanged` (a swap) or `DaemonCommand::CatalogNotModified` (a 304 — a pure reply routed through the loop so a queued overlay reload is applied before the `UpToDate` counts are computed). |
| `providers/mod.rs` | `InferenceProvider` — protocol-erased facade wrapping `Arc<dyn ProviderClient>` plus the catalog slug. `from_account_config()` dispatches by `ProviderProtocol` and constructs the right client from `choreo-ai-protocols`. Records API metrics (`record_api_call`/`record_api_error`) around each turn — timing lives here, not in the provider crate. |
| `sessions.rs` | `SessionState` (split into `SessionConfig` for persisted fields + runtime state), `RequestContext` dependency bundle, `SessionCommand` enum and its handler functions. Each session has a control thread running `session_main()`; request work runs on separate worker threads via `run_request_worker()`. Sessions form a tree (parent → child sub-sessions), each with an optional working directory. |
| `context.rs` | Context file discovery, skills, fingerprint-based refresh. |
| `metrics.rs` | Prometheus/OpenMetrics gauges, counters, histograms; HTTP server for `/metrics` endpoint. Compiled only when the `metrics` cargo feature is enabled (off by default; release binaries opt in via `scripts/release.sh`); disabled builds get no-op stubs so the instrumentation call sites compile unchanged. |
| `tools/` | `Tool` trait (with `output_schema` for programmatic tool calling, `allowed_callers` for caller-level gating), `ToolRegistry` (with injectable `FffStateCache` replacing a global `OnceLock`), and 30+ registered tools (including `list_sessions`, `get_session`, `load_skill` via `admin/`). |
| `tools/context.rs` | `ToolContext` — session-scoped context (session ID, `Arc<Database>`, `mpsc::Sender<DaemonCommand>`, active tool groups, reasoning effort, selected model, working directory) passed to tools that need DB or daemon access or parent config for sub-sessions. |
| `tools/db/` | Session-scoped KV database tools (`db_set`, `db_get`, `db_delete`, `db_delete_range`, `db_get_range`, `db_list`, `db_count`), one file per tool (`set.rs`, `get.rs`, `delete.rs`, `delete_range.rs`, `get_range.rs`, `list.rs`, `count.rs`) with shared `DbError`/`DbValue` in `db/mod.rs`. |
| `tools/fs/` | Core filesystem tools (`list_files`, `line_count`, `write_file`, `edit_file`, `delete_files`), one file per tool with shared write helpers in `fs/mod.rs`. |
| `tools/x/` | X/Twitter API tools (`x_post`, `x_search_recent`, `x_user_lookup`), one file per tool with shared OAuth1/HTTP plumbing in `x/mod.rs`. |
| `tools/evm/` | Thin `Tool` wrappers over the EVM blockchain tools (`evm_chain`, `evm_balance`, …) — the implementations live in `choreo-blockchain`; compiled only with the `blockchain` feature. |
| `tools/subxt.rs` | Thin `Tool` wrappers over the Substrate/Polkadot blockchain tools (`subxt_chain`, `subxt_balance`, `subxt_query`, `subxt_block`) — implementations in `choreo-blockchain`; compiled only with the `blockchain` feature. |
| `tools/content/` | Thin `Tool` wrappers over the Choreographr Coordination Platform tools (`coord_item`, `coord_publish_item`, `coord_set_profile`, …) — the implementations live in `choreo-content`; compiled only with the `content` feature, registered under the `content` group name. |
| `tools/admin/` | Session-admin tools (`list_sessions`, `get_session`, `load_skill`), one file per tool. |
| `tools/pdf/` | Native PDF ingestion tools (`pdf_classify`, `pdf_to_markdown`), one file per tool (`classify.rs`, `markdown.rs`) with shared input-gating / output-hygiene helpers in `pdf/mod.rs` and the shared PDF fixture builders in `pdf/test_fixtures.rs`. |
| `tools/glob_util.rs` | `GlobFilter` — shared glob-matching utility used by `delete_files` and `grep` that follows gitignore conventions (patterns without `/` match basename, patterns with `/` match full path). |
| `tools/vm.rs` | RISC-V sandbox: compiles Rust → ELF via rustc, executes in `ckb-vm` with custom syscall handler (`ChoreographrSyscall`) for tool dispatch. |
| `tools/shell_util.rs` | Shared child-process spawning for the shell/exec tools (`spawn_with_watchdog` / `spawn_with_streaming`): env sanitization, output caps, the timeout watchdog, and process-tree isolation — process-group + pidfd kill on Unix, a Windows Job Object (`ChildJob`) with blocking reads bounded by job termination on Windows. All waits are channel-driven (`recv_timeout` on the watchdog and on every drain's completion channel — no polling), each bounded by a completion grace that detaches a wedged drain rather than hanging the tool; the `Arc<ChildJob>` shared by the watchdog and drain threads is the fifth sanctioned shared-state exception (AGENTS.md). |
| `mcp/` | `McpManager` — loads MCP server config from `mcp_servers.json`, spawns subprocesses via `McpClient`, wraps discovered tools as `McpToolWrapper` (implements `ToolDyn`) and registers them in the `ToolRegistry` under a `mcp/<slug>` group. |

### Provider Architecture

The provider system has three layers, now split across two crates:

**1. `ProviderClient` trait (`choreo-ai-protocols/src/traits.rs`):**
```rust
/// Holds the common parameters for a chat completion turn.
pub struct ChatTurnRequest<'a> {
    pub model: &'a str,
    pub messages: &'a [ChatRequestMessage],
    pub tools: &'a [ChatToolDefinition],
    pub thinking_effort: String,
    pub on_retry: &'a mut Option<RetryCallback>,
    pub cancel_rx: Option<&'a crossbeam_channel::Receiver<()>>,
    pub previous_response_id: Option<&'a str>,
    pub tool_results: &'a [ToolResultItem],
    pub programmatic_tool_calling: bool,
}

pub trait ProviderClient: Debug + Send + Sync {
    fn provider_slug(&self) -> &str;
    fn chat_completion_turn(&self, params: ChatTurnRequest<'_>) -> Result<ChatTurnResult, InferenceError>;
    fn chat_completion_turn_streaming(&self, params: ChatTurnRequest<'_>, on_event: &mut dyn FnMut(StreamEvent) -> io::Result<()>) -> Result<ChatTurnResult, InferenceError>;
    fn list_models(&self) -> Result<Vec<String>, InferenceError>;
    fn supports_programmatic_tool_calling(&self, model: &str) -> bool;
    fn context_window_for_model(&self, model: &str) -> Option<u32>;
}
```

`ChatTurnRequest` consolidates the per-turn parameters into a single struct
to eliminate repetitive argument passing across all provider implementations.
Uses `&mut dyn FnMut` for the streaming callback to keep the trait object-safe.
`context_window_for_model()` returns the model's context window size, using a
resolution chain: per-model config → global fallback → catalog fallback.
Each client implementation maps the `&str` effort slug to its wire format:
- **OpenAI**: `reasoning_effort` field (`None` for `"off"`, `"low"`/`"medium"`/`"high"` slug → API string)
- **Anthropic**: `thinking` block with `budget_tokens` (slug ≠ `"off"` enables thinking, clamping to `max_tokens - 1024`)
- **Google**: `thinkingConfig` with `includeThoughts: true` (slug ≠ `"off"` enables thinking)
- **Mistral**: `reasoning_effort` field (`"off"` omits the field, otherwise slug → `"low"`/`"medium"`/`"high"`)

**2. `InferenceProvider` struct (`choreo-daemon/src/providers/mod.rs`):**
```rust
pub struct InferenceProvider {
    client: Arc<dyn ProviderClient>,
    slug: String, // catalog slug, owned (e.g. "opencode" even for an OpenAiClient)
}
```
Created via `from_account_config()` which looks up the provider slug in the catalog (returning an owned clone) and dispatches to the appropriate client constructor by protocol type. `provider_slug()` borrows `&str` from the owned slug. All wire-protocol knowledge lives in `choreo-ai-protocols`; the daemon's `InferenceProvider` is the only daemon type that dispatches by protocol. It also records API metrics (`record_api_call` / `record_api_error`) around every turn — timing moved here from the provider crates so `choreo-ai-protocols` stays free of daemon concerns.

The metrics `endpoint` label is the **catalog slug** (e.g. `"opencode"` rather than the protocol name `"openai"`) — more precise than the protocol, but part of the public metrics contract: renaming it changes the Prometheus series for that provider. Error labels come from `InferenceError::metric_label()` in `choreo-proto`, the single canonical mapping shared by all providers.

**3. Provider Catalog (`choreo-ai-protocols/src/catalog/`):**
```rust
pub enum ProviderProtocol {
    OpenAi { max_tokens_field: MaxTokensField },
    AnthropicMessages,
    GoogleGenerativeAi,
}
```

**`StreamEvent`** (`choreo-ai-protocols/src/types.rs`) replaces the old `(CompletionChunkKind, String)` callback tuple:
```rust
pub enum StreamEvent {
    Answer(String),
    Reasoning(String),
}
```
Each variant carries its data inline so the streaming callback is self-describing and extensible.
`emit_non_streaming_events()` in `providers/shared.rs` converts a `ChatTurnResult` into the
equivalent sequence of `StreamEvent`s, allowing non-streaming configurations to reuse the
same event-driven path as streaming ones without duplication across providers.

**3. Provider Catalog (`choreo-ai-protocols/src/catalog/`):**
```rust
pub enum ProviderProtocol {
    OpenAi { max_tokens_field: MaxTokensField },
    AnthropicMessages,
    GoogleGenerativeAi,
}
```

(Note: Mistral speaks the OpenAI wire format — `POST /v1/chat/completions` —
so it is catalogued under `OpenAi`, not a protocol of its own.)

### models.dev + overlay

The catalog is a two-layer pipeline (`choreo-ai-protocols/src/catalog/`):

```text
catalog/models.dev.json  (local, gitignored)  ──catalog-gen──►  catalog/catalog.bin   (embedded postcard base)
                                                      │
                                             include_bytes!  ▼
                                    load_bundled_base() → ProviderEntry base
                                                      │
                  catalog/models-overlay.toml (include_str!)  ▼
                                              merge_overlay() → load_catalog()
```

- **Base — normalized models.dev facts.** `catalog/models.dev.json` is a
  **local, gitignored** snapshot of the models.dev API (fetched by
  `catalog-gen` when it is absent — the only committed catalog data file is
  `catalog.bin`).
  `normalize_modelsdev` turns it into base `ProviderEntry` values: slug/name
  from the provider key/`name`, `base_url` from `api` (empty when absent),
  `default_model` = the FIRST model id in the snapshot's JSON order, protocol
  derived from the `npm` package (`@ai-sdk/anthropic` → Anthropic,
  `@ai-sdk/google` → Google, everything else OpenAI-compatible), and per-model
  `context_window` / `reasoning_supported` / effort levels derived from
  `limit.context` / `reasoning` / `reasoning_options`. The `catalog-gen` binary
  (`cargo run --bin catalog-gen`) normalizes the snapshot, postcard-serializes
  the **normalized base only**, and writes `catalog/catalog.bin` **atomically**
  (temp → fsync → rename), which the library embeds via `include_bytes!`.
  Normalization is deterministic (JSON order preserved), so re-running the
  generator over the same snapshot yields a byte-identical blob (guarded by the
  `embedded_blob_matches_local_snapshot` unit test when the snapshot is present
  locally, and by `catalog-gen --check` for CI). `--check` is strictly
  read-only; `--snapshot <path>` lets CI point at a cached snapshot artifact so
  the check never needs the network.
- **Overlay — everything not derivable.** `catalog/models-overlay.toml` is
  `include_str!` and merged at load time by `merge_overlay` — never baked into
  the blob, so S4 can re-merge the same base with a user overlay at runtime.
  It carries: provider-level endpoint/protocol/default-model policy for
  models.dev-covered providers (base_url where models.dev has none or differs,
  `max_tokens_field` for `max_tokens` gateways, protocol overrides such as
  Fireworks/Vercel's Anthropic-mode endpoints), per-model exceptions
  (Anthropic `tool_loop` passback pins, the `responses = true` flags on
  opencode/github-copilot GPT-5.x entries, Cerebras' `gpt-oss-120b`
  `none` pin, and the two `claude-opus-4-1` models the snapshot dropped), and
  the **wholesale overlay-only providers** models.dev does not cover (ollama,
  kimi-code, custom-*, … — they keep their pre-models.dev slugs and carry
  their full model lists verbatim).
- **Merge semantics** (`merge_overlay`): provider scalars field-wise replace
  with omitted fields falling through; naming a model replaces that entry's
  fields onto the base (new keys add); unknown keys warn + skip, never fatal.

### Runtime catalog refresh (S4)

The compiled-in catalog is the *fallback*; at runtime the daemon layers a
**local cache + a user overlay** on top and keeps the base fresh from
models.dev:

- **Cache.** The normalized base is cached at
  `$XDG_DATA_HOME/choreographr/catalog.bin` (postcard, same format as the
  embedded blob — one load path), written **atomically** (temp file → fsync →
  rename). The models.dev **etag is persisted in the DB** (`catalog_state`
  table), written by the daemon command loop AFTER the bin is on disk — so a
  crash between the two leaves the OLD etag paired with the OLD bin
  (self-healing: the next conditional GET 200s and stores a fresh etag),
  never a NEW etag over OLD content (which would 304 forever against a stale
  cache). The etag is only *used* when the cache loaded — a missing/corrupt
  cache produces no `If-None-Match`, so the next fetch is a plain GET that
  rebuilds both. Load order at startup: valid cache file → embedded
  `catalog.bin` (a corrupt cache logs a warning and falls back). The
  effective catalog is `merge_overlay(base, [bundled_overlay, user_overlay])`.
- **Refresh pacing — the 25 h attempt cooldown.** A models.dev fetch is
  attempted at most once per 25 h, whatever the last outcome (200/304/
  failure). The cooldown is anchored on a **wall-clock attempt timestamp in
  the DB** (`catalog_state.last_attempt_ms`), written by the maintenance
  thread **BEFORE the fetch starts** — so a daemon that crashes mid-fetch and
  restarts immediately cannot re-fetch, and the cadence survives restarts (a
  daemon restarted daily fetches once per ~day of wall time, not once per
  start). The 25 h period (not 24 h) makes each daemon's fetch time drift
  +1 h/day, spreading load across the daily cycle. `/refresh-models` bypasses
  the cooldown but still records the attempt. A DB-write failure is logged
  and the fetch proceeds — the timestamp is advisory pacing.
- **Startup gate.** The maintenance thread fetches at startup immediately iff
  there is no valid cache, no recorded attempt (first run / upgrade from a
  build without the key), or the attempt is stale; otherwise it skips the
  startup fetch and arms the in-run timer for the remaining time, derived
  from the persisted timestamp. Within a single run the countdown is
  monotonic — suspend pauses it (a suspended laptop fetches after 25 h of
  *awake* time); restart behavior is strict wall time via the DB anchor.
- **Background refresh.** The same maintenance thread does the conditional
  GET against `https://models.dev/api.json` (`If-None-Match` with the DB
  etag; models.dev serves `ETag` + `must-revalidate`). 200 → normalize →
  validate non-empty → hand the new base to the daemon command loop, which
  merges overlays, atomically swaps the catalog (`replace_catalog`), persists
  the cache + etag, and broadcasts `CatalogUpdated`. Every outcome arms the
  next revalidation 25 h out (the thread's channel `recv_timeout` is the
  timer). The fetch helper (`choreo-ai-protocols`
  `catalog::refresh::fetch_modelsdev`) owns ureq + normalization; the daemon
  command loop never does HTTP.
- **User overlay.** `$XDG_CONFIG_HOME/choreographr/models-overlay.toml`, the
  same schema as the bundled layer, merged last (highest precedence). The
  unified config transport (`config_watch.rs`) watches the config
  *directory* (rename-safe; basename-filtered to `models-overlay.toml`) and
  surfaces edits to the maintenance thread, which reloads via a
  **fingerprint gate** — the file is re-read and compared against the
  last-applied contents, so editor save-event storms collapse naturally after
  the first reload. Deleting the file falls back to bundled-only (warn). The
  config transport **creates the config dir at startup** (before the watch is
  installed and the models.dev fetch runs) so the first watch install succeeds
  even on a fresh system — a failed watch is **retried** in the transport
  loop only as a last-resort fallback.
- **`/refresh-models`.** TUI slash command → `ClientMessage::RefreshModels`
  → the daemon hands the request to the maintenance thread over its channel
  (never blocking the command loop on the download) → reply routed back as
  `DaemonMessage::ModelsRefreshed` (with `RefreshStatus`:
  `UpToDate`/`Updated`/`Forced`) or `ModelsRefreshFailed`. `--force` sends
  `Cache-Control: no-cache` and skips the etag. The request also **re-reads
  the user overlay** (fingerprint-gated, shared with the watcher) so it is the
  documented reload fallback when the watch could not start, and a burst of
  queued requests is **coalesced** into a single fetch (force flags OR-ed;
  each requester's reply status reflects its own flag; the whole burst is ONE
  recorded attempt). `/refresh-models` **bypasses the 25 h cooldown** (explicit
  user intent) but still records the attempt timestamp, so the DB anchor
  reflects reality — otherwise the next startup would re-fetch immediately. A
  304 reply is
  **routed through the daemon command loop** (as `CatalogNotModified`, not
  sent directly by the maintenance thread) so an overlay reload queued just
  before the request is applied first and the `UpToDate` counts reflect the
  current catalog.
- **`CatalogUpdated` broadcast.** Every catalog swap (startup refresh,
  overlay reload, `/refresh-models`) broadcasts the full provider list
  (slug + display name) to all activity subscribers — and a freshly
  subscribed client is sent the current list immediately, so the TUI's
  new-account wizard provider picker tracks the live catalog instead of the
  static default.  The TUI sorts the incoming list alphabetically by display
  name (`sort_providers` in `choreo-tui/src/state/providers.rs`, applied at
  both the static-default and broadcast entry points) because the catalog is
  ordered by provenance, not name.

### Slug renames (one-time migration)

models.dev keys are canonical, so the old hand-curated slugs were renamed to
match — `fireworks→fireworks-ai`, `together→togetherai`, `github→
github-copilot`, `novita→novita-ai`, `saladcloud→salad-cloud`, `kilocode→
kilo`, `gmi→gmicloud`, `vercel-ai-gateway→vercel`, `zhipu→zhipuai`, and the
three Z.AI entry points `zai`/`zai-cn`/`zai-coding-cn` merged into `zai`. This
is a one-time data migration (accounts.toml / keystore service names / TUI
`PROVIDER_OPTIONS` updated to the new slugs); there is deliberately **no
runtime alias resolution**. The metrics `endpoint` label changes with the slug
(Prometheus series change accepted, pre-1.0).

The merged catalog is parsed lazily into `PROVIDER_CATALOG`, a process-global
`LazyLock<ArcSwap<Vec<ProviderEntry>>>` (`catalog/mod.rs` +
`catalog/loader.rs`): the first access deserializes the embedded base and
merges the bundled overlay once; every later access goes straight to the
`ArcSwap` (an atomic load, then lock-free reads / atomic `store` on swap). The
`ArcSwap` makes the catalog runtime-swappable: readers are lock-free and
`replace_catalog()` atomically swaps the whole catalog (single writer: the
daemon command loop), so lookups return *owned* values cloned out of the
atomic guard rather than `&'static` references.

A `ProviderEntry` maps each provider slug to:
- `display_name` — human-readable name for UIs
- `protocol` — which wire protocol to use
- `base_url` — well-known API endpoint
- `default_model` — sensible default model name
- `models` — curated `ModelEntry` list with `context_window`, `max_output_tokens` (from the snapshot's `limit.output`; `0` = unknown), and **wired as a clamp**: the outgoing `max_tokens` / `max_completion_tokens` / `max_output_tokens` request fields are clamped *down* to this fact when the lookup resolves and the request would exceed it (clamp-down only — a smaller request is never raised; see `ServiceConfig::clamp_output_to_catalog`), `reasoning_supported`, explicit `openai_reasoning_levels`, whether the model uses the Responses API (`openai_responses`), `reasoning_content_required` (ingested from the snapshot's `interleaved.field == "reasoning_content"`; see the resolver paragraph below), `supports_temperature` (from the snapshot's `temperature` flag; absent → permissive `true` — currently a **recorded-but-unwired** fact: no request builder sends a `temperature` parameter today, so there is nothing to gate; the fact and the `model_supports_temperature` resolver are kept so the gate exists the moment temperature sending is added), `deprecated` (from the snapshot's `status == "deprecated"`), and `supports_vision` (whether it accepts image input; derived from models.dev `modalities.input` and overridable in the overlay). All snapshot facts are overlay-overridable per model without regenerating the blob

Model-level reasoning is resolved at runtime by `model_reasoning_capability()`, which returns a `ReasoningCapability` with the model's available effort slugs. Providers without explicit entries fall back to protocol defaults (`off/low/medium/high` for OpenAI & Anthropic, `off/on` for Google).

#### Reasoning round-trip (capture → carry → re-emit)

Reasoning text is not only *displayed* — for several providers it must also be **sent back** on the next request, or the tool-call loop is rejected with a 400 (Anthropic requires the encrypted thinking blocks echoed unmodified; DeepSeek/Kimi require `reasoning_content` on every assistant tool-call message; Gemini requires the encrypted thought signatures back for reasoning continuity). The round-trip payload is an **opaque, provider-owned artifact** handled in three layers, each owning one concern:

| Layer | Owns |
|---|---|
| Catalog (`choreo-ai-protocols/src/catalog/`) | `reasoning_passback` format enum (`ReasoningPassback`), per-model + protocol-defaulted — *how* to send |
| Adapters (`openai/`, `anthropic/`, `google/`) | capture the artifact verbatim at the parse boundary; re-emit it verbatim in their own wire format on request build |
| Daemon (`build_chat_request_messages` in `choreo-daemon/src/reasoning.rs`) | derives *whether* to send (same-model provenance + passback policy); never interprets the payload |

**Capture** happens inside each adapter before the display field is consumed: OpenAI chat wraps the raw reasoning string — from whichever chat field the provider populated (`reasoning_content`, `reasoning`, or `reasoning_text`, with that precedence) — into `ChatReasoning { field, bytes }`, tagging the artifact with the field it came from; Anthropic serializes the ordered thinking / redacted_thinking blocks (signatures + redacted data intact, order preserved) into `AnthropicThinking`; Google collects the `thoughtSignature` values (the `thought: true` marker may carry a signature on **any** part type — the wire-format fix; there is no separate `thinking` key) into `GoogleSignatures`; Responses collects the raw reasoning output items verbatim — type tag, id, summary, `encrypted_content` in stateless mode, and any unknown fields (e.g. a newer `content` shape), preserved exactly as returned — into `ResponsesItems`. The artifact rides out of the provider crate on `ChatAssistantToolUse`/`FinalTextResult.reasoning_artifact` and is stored on the `Turn` by the agent loop via `SessionState::set_assistant_response` — which now takes an `AssistantResponse` struct bundling text, reasoning, tool calls, usage, and the artifact + producer pair — alongside `Turn.reasoning_producer` (provider slug + model).

**Carry** is a pure store-and-forward: the daemon never reads the payload bytes. It also strips the artifact (and its producer) from every client-bound `DaemonMessage` payload — the `SessionEvent::TurnAppended`, `SessionEvent::SessionState`, and `SessionEvent::TurnsRedone` events (on the `DaemonMessage::Session` envelope) carry client copies with `reasoning_artifact`/`reasoning_producer` set to `None` (see `turn_for_client` in `choreo-daemon/src/sessions.rs`), so the bytes never leave the daemon process; only the request builder consumes them, from the authoritative `Turn` in `SessionState` and the DB. The builder's only job is the *whether*: an artifact is attached to an assistant message only when (1) **same-model provenance** holds — `turn.reasoning_producer == {current provider_slug, current model}` — so a turn produced by a different model (mid-session `/model` switch) never replays its possibly-encrypted payload, and (2) the resolved `ReasoningPassback` policy says to (or the **empty-message fallback** kicks in — see the empty-message paragraph below):

| `reasoning_passback` | Meaning | Wire behavior |
|---|---|---|
| `None` | display-only providers/fields | never replay |
| `ToolLoop` | echo reasoning on assistant messages that had tool calls (DeepSeek/Kimi chat; the minimum for Anthropic tool loops) | attach artifact on tool-involving turns only |
| `AllTurns` | echo across all turns of the session (Anthropic keep-all models, GPT-5.6 `all_turns`) | attach artifact on every assistant message |
| `Signature` | send back encrypted thought signatures (Gemini) | attach artifact on every assistant message; the adapter attaches the final signature to the last part |
| `ResponseId` | chain via `previous_response_id` / opaque reasoning items (OpenAI/xAI Responses) | never via the message; continuity flows through the response id (see below) |

`model_reasoning_passback(slug, model)` mirrors `model_reasoning_capability`: an explicit per-model override from the overlay wins (including an explicit `none` — a model that must never replay can be pinned without inventing a provider), otherwise the protocol default applies — OpenAI-protocol with `responses = true` → `ResponseId`; OpenAI-protocol chat-completions → `ToolLoop`; Anthropic → `AllTurns` (last-turn-only models like `claude-haiku-4-5` carry an explicit `tool_loop` override in the overlay); Google → `Signature`; unknown providers → `None`. The overlay sets the field only where nuance matters (the Anthropic last-turn-only pins, Cerebras' `gpt-oss-120b` `none`; DeepSeek's `tool_loop` was already the derived default and is not carried).

**DeepSeek/Kimi `reasoning_content` must be *present*.** Beyond the echo policy, the chat-completions builder injects an explicit `reasoning_content: ""` (empty) on every assistant message that has nothing to echo for a model that requires the field (DeepSeek/GLM-5.x chat — the upstream 400s a history whose assistant tool-call message omits it, even when the model produced no reasoning on that call). A single `requires_reasoning_content(slug, model)` resolver drives this, and it is **purely data-driven**: the flag is a FACT ingested from the models.dev snapshot (the model's `interleaved` value names `"reasoning_content"` as the echo field — the snapshot encodes that value as either an object `{field: ...}` or a plain-string shorthand; a bare `true` is a capability flag with no field and is not a fact), stored on the `ModelEntry.reasoning_content_required` option at `catalog-gen` time; an explicit per-model overlay override (`reasoning_content_required = true|false` — the only path for models the snapshot does not cover, e.g. the wholesale-defined `opencode-go`/`glm-5.3-flash` entry) wins over the ingested fact. There is **no name-based fallback**: `None` (no fact) or an unknown model resolves to `false`, so a catalog gap surfaces as the upstream provider's own 400 about the missing field — auditable and fixable by adding the model with an explicit flag — instead of a substring guess (the former `is_deepseek_or_kimi` heuristic) that silently misses new family members (GLM 5.x carries the flag; GLM 4.5/4.6 do not, which a family-wide `"glm"` match would get wrong) and can never be overridden per model. The empty string is only injected when the artifact is absent — a real artifact still re-emits its text — and the field is never sent on Responses-API models (where `reasoning_content` is invalid). `session_inspect` mirrors the resolver so its ledger-vs-wire parity check stays exact.

**The empty-message fallback** closes the remaining hole in that injection: a turn recorded as *reasoning-only* (empty content, no tool calls — e.g. a response that streamed only `reasoning_content`) would serialize as a wholly empty assistant message that OpenAI-compatible chat providers reject with "the message ... with role 'assistant' must not be empty" — this is exactly the opencode-go deepseek→kimi shape (an empty assistant turn in history 400s the very next Continue). The single `include_reasoning_artifact()` helper (used by the builder, the precondition guard, and `session_inspect`) forces such a turn's **same-model** artifact in even though ToolLoop alone would skip it (no tool involvement): the artifact's real reasoning text is the only payload that keeps the wire message non-empty. The fallback is provider-agnostic — it fires on every passback that may legally echo (`ToolLoop`/`AllTurns`/`Signature`), not only the DeepSeek/Kimi `requires_rc` models — but deliberately does NOT fire under `None` (an explicit never-replay override: the gateway may itself reject replayed reasoning, e.g. Cerebras gpt-oss) or `ResponseId` (continuity flows through `previous_response_id`/input items, not the message reasoning field). A foreign-model artifact (mid-session switch) or a missing artifact leaves the message unfixable — the guard flags it as a "must not be empty" risk on any artifact-policed passback (not only `requires_rc` models) instead of letting the provider fail silently.

**Re-emit** is per-adapter, verbatim: OpenAI chat writes the `ChatReasoning` bytes back as the wire field recorded at capture (`reasoning_content` / `reasoning` / `reasoning_text` — DeepSeek/Kimi being `reasoning_content`), so a provider that streamed `reasoning_text` gets `reasoning_text` back, not `reasoning_content` (the artifact field itself never appears on the wire); Anthropic deserializes the block array and pushes the blocks verbatim (in order, ahead of text/tool_use — never rebuilt or reordered, and only when thinking is enabled for the request, `!thinking_disabled`); Google attaches the captured signatures to the assistant parts; Responses re-emits the opaque items into `input` ahead of the message and chains continuity through `previous_response_id`. A foreign artifact variant (e.g. a `ChatReasoning` payload on an Anthropic request) is dropped by the adapter — payloads stay opaque until their producer decodes them.

**ResponseId continuity:** the agent loop persists the last `response_id` on `SessionConfig.last_response_id` after every model call and restores it at the top of the next `run_agent_loop` invocation, so a new user turn continues the chain (`previous_response_id` + `reasoning.context: all_turns` guidance) instead of resetting it. Other policies reset to `None` so a stale id never leaks into a request that does not understand it.

When chaining a fresh user turn via `previous_response_id`, the request `input` carries only the messages that postdate the last assistant message (the new user message, plus the freshly rebuilt system prompt) — the server already holds everything up to the last response, and resending the full history would duplicate every prior turn on top of the chained context (billing + context-window inflation). Tool-loop turns keep sending only the new `function_call_output` items, as before. The adapter-level `messages_to_responses_input` still re-emits opaque reasoning items for non-chained (stateless-style) conversions. An `/undo` invalidates the chain: the persisted id points at a response whose conversation includes the undone turns, so `handle_undo` clears `last_response_id` (and its producer) and persists the cleared record — the next request falls back to a non-chained one carrying only the visible turns (redo does not restore the id; a stateless request is always safe).

A precondition guard (`warn_on_missing_reasoning_artifacts`) runs before any echo-policy request: a turn whose artifact is missing (e.g. pre-migration session state) or whose artifact was produced by a different model (a mid-session model switch — the builder never replays a foreign-model payload) is logged as a diagnosable warning instead of surfacing as a mysterious provider 400. `ToolLoop` policies check only tool-involving turns (that is where the provider demands the echo); `AllTurns`/`Signature` echo on every assistant message, so the guard checks every assistant turn there.

Replayed reasoning is billed as input on keep-all models, so `estimate_prompt_tokens` counts the artifact bytes (UTF-8 text when decodable, else a bytes/4 heuristic). The estimate counts the full conversation in `messages` as-is, which already covers the server-side chained context for `previous_response_id` requests: the adapter trims only the *wire* payload to the chain tail, but the provider bills the whole chain, and the full conversation in `messages` is that chain plus the new tail. There is deliberately no chained-context addend — adding the last request's `prompt_tokens` would count the conversation twice.

Currently supports 208 providers (184 from the models.dev base + 24 overlay-only). Adding or refreshing a provider is a data change in `catalog/`: update the snapshot (or add an overlay entry) and re-run `cargo run --bin catalog-gen` — zero client code.

**Supported providers by protocol:**

| Protocol | Providers (overlay-only providers in *italics*) |
|---|---|
| OpenAI-compatible | OpenAI, DeepSeek, Mistral, xAI, Groq, Together AI, OpenRouter, Hugging Face, GitHub Copilot, NVIDIA NIM, Cerebras, Fireworks AI, Xiaomi MiMo, Alibaba (Qwen), Moonshot AI, Perplexity, Z.AI, Qwen Token Plan, Venice AI, Novita AI, LM Studio, Ollama Cloud, OpenCode Zen/Go, DeepInfra, Upstage, StepFun, Inception, Meta, NEAR AI, OrcaRouter, Routstr, Sakana, SaladCloud, Scaleway, OVHcloud, FuturMix, EmpirioLabs, Friendli, Atomic Chat, custom OpenAI-compatible — plus the overlay-only providers that keep their pre-models.dev slugs: *aimlapi, GitLawb OpenGateway, Kilo Gateway, OpenAI Codex, iFlytek, Nous, Arcee, GMI, Zhipu, Bankr, Atlas Cloud, Ant Ling, oMLX, Qwen Token Plan (CN), Tensorix, Tanzu, Llama Swap, kimi-code, ollama, openai-compatible, …* |
| Anthropic Messages | Anthropic Claude, MiniMax, Vercel AI Gateway, Kimi Code, Fireworks (Anthropic mode), OpenCode Go (Anthropic-compatible), custom Anthropic-compatible |
| Google Generative AI | Google Gemini |

> **Note — providers present in agent catalogs but deferred from the daemon catalog**
> (present in models.dev, catalogued for their model lists but with no API
> endpoint / no API-key path in the current catalog):
>
> | Provider(s) | Reason deferred |
> |---|---|
> | `amazon-bedrock`, `google-vertex`, `azure-foundry`, `azure-openai-responses` | Multi-field credentials (AWS keys/region, GCP service account, Azure resource + key). The daemon's single-API-key credential model cannot represent them yet. |
> | `chatgpt` (Codex), `openai-codex` OAuth, `copilot`/`copilot-acp`, `qwen-oauth`, `kimi-code` OAuth, `github-copilot` OAuth, `radius` | OAuth-only / dynamic-catalog auth — no static API-key path. The slugs above that do have an API-key path (`openai-codex`, `kimi-code`, `github-copilot`) are catalogued; the pure-OAuth ones are not. |
> | `cursor` | t3code subprocess driver (spawns the Cursor CLI), not a direct HTTP provider. |
>
> Adding them requires daemon-side OAuth support and/or multi-field credentials, both out of scope for the current catalog model.

**Per-client architecture (OS threads):**

```
client_thread(socket)
├── reads ClientMessages from socket via choreo-proto read_message_sync
├── sends DaemonCommands via daemon_tx mpsc channel
└── receives DaemonMessages via per-client mpsc receiver → writes to socket
```

**Thread topology:**

```
main()
├── listener thread — UnixListener accept loop (non-blocking poll)
│   └── per client: spawns client_thread (std::thread::spawn)
├── metrics HTTP thread — (optional) serves /metrics at `--metrics-addr`
├── command thread — DaemonCommand receiver loop (daemon_tx mpsc)
│   └── owns DaemonState (exclusive access, no Arc<Mutex>)
├── per-session threads — spawned on CreateSession, reaped on Shutdown
│   └── owns SessionState (exclusive access)
└── main thread — polls shutdown flag every 200ms, orchestrates clean exit
```

**Request flow:**

```
RunInput received
  └► extract/validate session, check active requests
     └► if chat_completions + tools:
        └► tool-call loop (daemon-wide configurable cap, default 0 = unlimited):
           0.5. build system content (skills + context with fingerprint cache + loaded skills + subdirectory hints)
           1. send messages + tools → model
           2. receive response
           3. if tool_call → execute Tool → persist loaded skill bodies → collect subdirectory hints → goto 0.5
           4. else → emit final text, Done
     └► if responses or chat_completions (no tools):
        └► stream chunks via SSE → emit OutputChunk per token → Done
```

### Token Usage & Context Window Tracking

Token usage flows from providers through the daemon to all clients.
Each session also tracks the model's **context window size**, resolved when the model
is selected. The TUI displays context usage as a fraction
(`last_prompt_tokens / context_window`), showing the actual context size sent in the
most recent request rather than accumulated totals.

Additionally, each session tracks `last_prompt_tokens: Option<u32>` — the `input_tokens`
from the most recent API response. This is stored separately from `accumulated_usage`
(the billing counter) and reflects the actual context payload the model sees on the
latest turn. When an existing session is loaded from the database but has no stored
`context_window`, the daemon re-resolves it from the provider catalog.

```
LLM provider (API response)
  └► usage extracted per-turn in provider client
     ├─ OpenAI non-streaming:    ChatCompletionsResponse.usage
     ├─ OpenAI streaming:        final SSE chunk with usage (stream_options.include_usage=true)
     ├─ Anthropic non-streaming: MessagesResponse.usage (input + output tokens)
     ├─ Anthropic streaming:     message_start (input) + message_delta (output)
     ├─ Mistral:                 ChatCompletionResponse.usage
     └─ Google:                 Not yet supported (usage = None)
       │
       ▼
     ChatTurnResult (FinalText | ToolUse).usage: Option<TokenUsage>
       │
       ▼
      run_agent_loop (choreo-daemon/src/requests.rs)
        ├─ embeds per-turn TokenUsage into SessionMessageKind::AssistantText / SessionMessageKind::AssistantToolUse
        ├─ tracks last_prompt_tokens = Some(usage.input_tokens) for context-window display
        └─ accumulates into SessionState.config.accumulated_usage (TokenUsage)
        │    └─ on the worker's PRIVATE session clone; the main thread's config
        │       is synced mid-turn via SessionCommand::SyncAccumulatedUsage (see below)
        │
        ▼
      SessionState (choreo-daemon/src/sessions.rs)
        ├─ persisted via SessionRecord.accumulated_usage (through SessionConfig)
        ├─ sent to subscribers via SessionEvent::SessionState.token_usage
        ├─ sent to clients via SessionEvent::Done.token_usage
        ├─ included in SessionSummary.token_usage (listing / get-session)
        ├─ last_prompt_tokens flows through the same channels (SessionRecord,
        │  SessionState, SessionEvent::SessionState, SessionEvent::Done,
        │  SessionSummary)
        └─ status flows through SessionEvent::SessionState.status and
           SessionEvent::SessionStatusChanged for live toolbar display.
           Every status transition refreshes the daemon's session_metadata
           index in handle_broadcast_session_status (daemon.rs) so a later
           ListSessions never serves a stale status — but it does NOT bump
           last_modified (status transitions are pipeline churn, not
           modifications); the index is the source of truth for the sessions
           list.
        └─ the sessions list (ListSessions) is sorted newest-first by
           last_modified (id-desc tiebreak) — see handle_list_sessions.
        │
        ▼
     Clients (choreo-tui, choreo-gui, choreo-im)
       ├─ choreo-tui: displays in session detail view (render/session_manager.rs:render_session_detail_view)
       │  as "Context:  current / limit (pct%)"
       └─ choreo-tui: terminal progress bar uses last_prompt_tokens vs context_window
          for the OSC 9;4 percentage sequence
```

The request worker accumulates usage on a **private clone** of the session
state and only merges it back at `RequestFinished`.  To keep every consumer
fresh *mid-turn* (attach `SessionState` snapshots, session summaries, and
`TokenUsageUpdate` broadcasts), `broadcast_token_usage` (requests/tool_execution.rs) routes the
worker's cumulative total through `SessionCommand::SyncAccumulatedUsage` to the
session's main thread, which (1) applies it to the authoritative
`config.accumulated_usage` — as a per-field **max** (`TokenUsage::merge_max`,
shared with the TUI's attach-snapshot merge), so an out-of-order or overlapping
sync can never regress a total a client already saw — (2) re-broadcasts
`TokenUsageUpdate` from the updated state so a client can never be ahead of the
snapshot it receives on attach, and (3) refreshes the daemon's session-metadata
index without a `last_modified` bump.  `apply_worker_snapshot` at
`RequestFinished` applies the final (≥) value, so the two paths are idempotent.
On the client side, `last_prompt_tokens` is not cumulative, so the TUI
gap-fills it from snapshots (never overwriting a fresher value) instead of
max-merging it.

**Key type** — `TokenUsage` (choreo-proto/src/types.rs):
```rust
pub struct TokenUsage {
    pub input_tokens: u32,
    pub output_tokens: u32,
    pub total_tokens: u32,
}
```

**Context window resolution chain (per session):**

```
handle_set_model / handle_set_account
  └► InferenceProvider::resolve_context_window(model)
       ├─ ProviderClient::context_window_for_model(model)
       │    ├─ model_context_windows (exact model name match)
       │    └─ context_window (global fallback)
       └─ catalog::lookup_context_window(provider_slug, model)
             └─ model_context_windows (exact model slug match)
       │
       ▼
     SessionConfig.context_window: Option<u32>
       │
       ▼
     Re-resolved on session startup if None
       (session_main + handle_run_input both call
        SessionState::resolve_context_window_if_missing(),
        handling sessions created before the model was
        in the catalog or providers resolved after unlock)
       │
       ▼
     Client display (e.g. "Context: 45,000 / 128,000 (35%)")
```

The `ContextWindowConfig` struct (shared across all provider configs) holds the
per-model map and global fallback. Provider configs embed this struct; `AccountConfig`
applies its overrides through the shared `apply_overrides()` method.

All new fields use `#[serde(default)]` so old persisted sessions remain compatible (deserialize to zero usage).

### `choreo-tui` — Terminal client

Entry point: `src/main.rs`

**Thread topology:**

```
main()
├── reader task: read DaemonMessages from socket → push to UI event channel
├── writer task: receive ClientMessages from mpsc → write to socket
├── terminal-event thread: mio::Poll on three sources —
│   ├── stdin (fd 0) → crossterm events (keyboard, mouse, resize)
│   ├── notification pipe → clean shutdown signal
│   └── signal pipe (self-pipe trick) → SIGCONT/SIGTSTP (suspend/resume),
│       SIGWINCH (resize wakeup)
│       └── forwards signals as ResumeCommand via crossbeam channel
│           (SIGWINCH is not a ResumeCommand — it only wakes the poll so the
│           crossterm drain below reports `Event::Resize`)
└── UI loop: crossbeam select! on five event sources + ratatui rendering
```

**Signal handling (suspend/resume + resize wakeup):**

`SIGCONT`, `SIGTSTP`, and `SIGWINCH` are caught using the self-pipe trick for
POSIX portability (Linux and macOS). A pair of pipe fds (FD_CLOEXEC) is
created; the read end is registered with `mio::Poll` in the terminal-event
thread, and `signal_hook::low_level::pipe` installs signal handlers that
atomically write a byte to the write end. The terminal-event thread reads
from the pipe and forwards `ResumeCommand` messages through a crossbeam
channel to the UI loop. The UI loop handles `PrepareForSuspend`
(disable raw mode, leave alternate screen, `raise(SIGSTOP)`) and
`ReinitTerminal` (re-enable raw mode, re-enter alternate screen, clear).

`SIGWINCH` is deliberately *not* mapped to a `ResumeCommand`. Its only purpose
is to wake the terminal thread's `mio::Poll` (which otherwise sleeps on stdin
and would miss a resize entirely), so the thread's event drain calls
`crossterm::event::poll`/`read` and picks up the `Event::Resize` that
crossterm 0.29 generates from its own internal SIGWINCH handler. Without this,
a terminal resize — e.g. toggling fullscreen in Ghostty — would leave the
viewport at the stale size until the next keypress, breaking the layout.
`run_app` also primes crossterm's event reader once at startup (a
zero-timeout `event::poll`) so that lazy SIGWINCH handler is installed before
the first resize can arrive.

> **Note:** In raw mode, `termios` `ISIG` is disabled, so pressing Ctrl+Z in the
> terminal sends byte `0x1A` to stdin as a regular character — it does **not**
> generate a `SIGTSTP` signal. The self-pipe suspend only catches external
> `SIGTSTP` (e.g. from `kill`, shell job control, or another terminal).
> To support Ctrl+Z keyboard suspend from within the TUI, the page event
> handlers (`handle_chat_event`, etc.) would need an explicit
> `KeyCode::Char('z')` + `KeyModifiers::CONTROL` match that calls
> `handle_resume_command(PrepareForSuspend, …)`.

**Per-frame sequence (UI loop):**

```
while !app.should_quit:
  1. Block in crossbeam select! until any event arrives:
     - terminal events (keyboard, mouse, resize)
     - daemon messages from the reader task
     - image encoding results from the worker thread
     - resume commands from the terminal-event thread
  2. Drain all five event sources (non-blocking try_recv):
     - crossterm events
     - UI event channel (daemon messages)
     - image result channel
     - resume commands
  3. If nothing was dirty, skip render (continue)
  4. Consume scroll accumulator → apply batched delta
  5. Update history viewport dimensions from terminal size
  6. Clamp scroll state to valid range
  7. Render via ratatui terminal.draw()
  8. If `progress_dirty`, emit OSC 9;4 terminal progress bar
     (percentage of `last_prompt_tokens / context_window` for the
     attached session, or clear/indeterminate if no data)
```

**Module breakdown:**

| Module | Purpose |
|---|---|---|
| `connection/` | Socket setup, event loop, shutdown signal handling, input/keyboard/mouse dispatch, daemon message routing, terminal suspend/resume signal handling. The daemon→UI event channel is unbounded: with a bounded channel a burst from another session (all activity is subscribed, so background sessions stream their own chunks/updates through the same queue) could fill it and drop this session's streaming chunks — and a dropped chunk is a delta that only the final `TurnAppended` resyncs, freezing the live results until the tool completes. Mouse scroll events are accumulated per-frame rather than applied immediately — the delta is consumed in batch before each render (see `apply_scroll_delta`). Left-clicking a turn's reasoning header toggles that turn's collapsible reasoning section, and left-clicking a tool result's header row (triangle + description) toggles that result's collapsible body — both hit-tested against the per-turn visual-row ranges stored in `TurnLayout`, so clicks land exactly on the drawn rows. Left-clicking inside the command input box repositions the text cursor: the click is hit-tested against `App::input_box_rect` — derived from the shared `App::chat_page_layout`, the single source of truth for the Chat page's vertical layout (rendering, mouse hit-testing, and the history viewport all consume it, so they can never drift apart, even on terminals too small for the fixed chrome to fit) — and mapped to a byte offset via `InputBuffer::byte_offset_at_click`, so clicks land on wrapped/multi-line text exactly where they appear, border clicks are ignored, and padding clicks clamp to the line start/end. Click offsets are grapheme-cluster aware — the cursor can never land inside a ZWJ emoji or combining sequence, and clicking the right half of a 2-column-wide character places the cursor after it. The reserved scrollbar column is only treated as interactive when a scrollbar is actually rendered (`App::scrollbar_visible`), so a hidden scrollbar never arms drag state that would swallow subsequent history clicks. Text selection (select-to-copy) is a mouse arm checked *before* the scrollbar arms: while a selection is in progress every drag event extends it and the release finalizes it — `selection::finish_selection` → `clipboard::copy_to_clipboard` (OSC 52) + the "Selection copied to clipboard." status (or a "too large to copy" status when the text exceeds the OSC 52 size cap) — so a drag that crosses the scrollbar column keeps selecting, matching terminal-native selection. A plain click in the history box arms a selection only on non-interactive text rows (reasoning/tool header toggles and image clicks keep their existing Down behavior and arm nothing); a scroll wheel mid-gesture scrolls immediately AND keeps the selection — the anchor stays pinned to the text it was placed on (content coordinates) while the live drag head re-resolves to the content under the cursor, so the selection tracks the cursor as the viewport moves and the highlight updates on the wheel event itself, exactly like terminal-native drag-while-scroll (the same head-tracking happens at draw time when *content* scrolls the viewport, see `selection::follow_cursor`); the gesture state machine lives in `selection::handle_selection_mouse`, and any other mouse event (right-click, a second Down) cancels it. Opening the model selector also clears an armed selection, since the modal overlay routes mouse events away from the selection arms. `Ctrl+M` (Chat page) opens the model-selector popup: it sends `ListModels`, and while the popup is open the `Models`/`ModelsFailed` replies populate it instead of falling through to the generic chat-history print; Enter sends `SetModel`, Esc dismisses, PgUp/PgDn page the highlight, the mouse scroll wheel navigates like the arrows (one row per notch, pin-at-middle — see `step_focus`), a left-click on a visible list row selects it exactly like Enter (the row is resolved through the *rendered* window start — `window()`'s, not the stored `scroll`: a PgUp/PgDn jump moves `focused` without touching `scroll`, and `picker_window` then pushes the drawn window to keep the jumped focus visible, so the raw value would select a different row than the one drawn — and a row click while the popup is loading or showing a refresh error is a no-op, since no list is drawn then), a left-click on the filter row positions the input cursor (grapheme-aware, via `selector_position_filter_cursor`), clicks anywhere else are no-ops, and other keys feed the filter box. To make `Ctrl+M` distinguishable from Enter, the app requests the kitty keyboard-protocol `DISAMBIGUATE_ESCAPE_CODES` enhancement (pushed at startup and after resume, popped on suspend/exit); Ctrl+letter then arrives as an unambiguous CSI-u sequence while plain text stays legacy-encoded. `REPORT_ALL_KEYS_AS_ESCAPE_CODES` is deliberately NOT requested: with it enabled, kitty-protocol terminals deliver IME-composed text (e.g. Vietnamese via OpenKey) as a pure "text event" (`CSI 0;;<codepoints>u`) whose associated-text field crossterm 0.29 drops, mangling the event into `Char('\0')` — so IME input would type as nothing. Keeping text legacy-encoded lets composed text arrive as plain UTF-8 bytes. Incoming SHIFT-modified chars are normalised to the shifted glyph (US layout) so text entry matches legacy terminals. Terminals without kitty support ignore the push (there `Ctrl+M` arrives as Enter). The AI-provider new-account flow is a modal wizard driven from the accounts page (`n`): step 1 is a centered, searchable provider picker over the live catalog (`App.providers`, sorted alphabetically by display name — `sort_providers` in `state/providers.rs`; the canonical slug is deliberately hidden) — typing filters by case-insensitive substring, `↑`/`↓`/`PgUp`/`PgDn` move the highlight (j/k type into the filter, matching the model selector), the mouse scroll wheel navigates like the arrows, a left-click on a visible list row picks the provider (like Enter), a left-click on the filter row positions the cursor, Enter picks and advances to step 2, a slug modal that validates and submits `AddAccount`; Esc on the slug step backs up to the picker, Esc on the picker cancels the whole wizard. On success the wizard closes and the API-key modal auto-opens so the user can paste a key (`c` on an existing account opens the same modal). The key is masked while typing, wiped from the input buffer with `zeroize` when the modal closes, and encrypted with the daemon's identity key on save (`build_add_credential_message`); `AccountAddFailed` closes the key modal only when it still targets the failing account. `CredentialAdded`/`CredentialRemoved` replies re-issue `ListAccounts` so the accounts page (which renders each account's `has_credential` flag) refreshes immediately after a credential is stored or removed — the credential reply carries no account data of its own. The two full-page lists follow the picker pattern: on the accounts page a left-click on an account row selects it and sends `SetSessionAccount` then returns to Chat (mirroring the Enter handler), and on the session-manager list a left-click on a session row selects it and attaches via `attach_to_session` — both wheel-scroll the highlight, and both ignore clicks outside their content rows (and while a remove/delete confirmation is armed), hit-tested against the shared `page_list_content_rect` geometry (see `state/`); the two handlers share their confirm-guard/wheel/left-click skeleton via `handle_full_page_list_mouse`, each supplying only its own list-specific `select_*`/click-resolution/commit closures). |
| `state/` | `App` struct: input buffer, request tracking, `HashMap<u64, SessionDisplayState>` for per-session display state (session view, scroll state, height prefix-sum array, render cache, markers, streaming state, active requests, live token estimates, per-turn reasoning-collapse overrides, per-(turn, call_id) tool-result collapse overrides, and the unsent prompt draft — the input bar is a single shared buffer, so each session stashes its own unsubmitted prompt (text plus cursor position) here and it is restored on the next visit; `attach_to_session`/`handle_session_created` hand the buffer over via `persist_input_draft`, which drops active history navigation first so the real draft is saved (cursor included, and any edits made on top of a history entry are kept as the draft), while the startup auto-attach keeps pre-attach input rather than clobbering it, and submitting or deleting the session clears the draft, and Ctrl+Backspace clears the input bar outright — the whole draft, wherever the cursor sits (Ctrl+W deletes the previous word, Ctrl+U clears only up to the cursor)), `active_session_id` for the currently active session, the per-frame scroll accumulator (`scroll_accumulator`) consumed by `apply_scroll_delta()`, and the in-progress mouse text selection (`text_selection`, an `Option<TextSelection>` cleared on session switch, suspend, page switch, opening the model selector, and terminal resize — a resize re-wraps every line, so a stored (content line, viewport column) anchor would point at different text afterwards and is deliberately never re-resolved — so a stale rectangle can never highlight a different session's content). `chat_page_layout` is the single source of truth for the Chat page's vertical layout: `render_chat`, the mouse hit-test in `input_box_rect`, and `update_viewport_from_terminal_size` (which sizes the history viewport from the layout's history chunk, minus the scrollbar column) all run the identical `Layout::split`, so they agree even on tiny terminals where the solver shrinks the fixed-height chunks. The render cache key includes the effective reasoning visibility, the per-result tool collapse state, and a per-turn content version — bumped by every content-mutating handler (streaming chunks, turn replacement, snapshot merges, undo/redo) — so a rebuild can never reuse a stale cached rendering of a turn whose text changed behind the key's other fields; each `TurnLayout` stores the reasoning header's visual-row range plus the derived default-expanded flag and each tool result header's visual-row range (O(1) click hit-testing and per-frame effective-state lookup without re-scanning turn text). `status_error_height` sizes the status/error chunk by wrapping the message at `width−2` — the same inner width the inset status `Paragraph` (render/mod.rs `notify_area`) wraps at — so a long status reserves exactly the rows ratatui draws instead of clipping its tail. The streaming fast path (`apply_streaming_update`) re-renders the in-flight turn every chunk with the effective per-result visibility, so an expanded tool result streams live while a collapsed one stays flat; `compute_total_height_and_markers` applies streaming updates *first* — even when a separate event also marked `markers_dirty` — so a mid-stream `Done`/`TurnAppended`/`SessionState` (from this session or, via the all-activity subscription, a busy background session) can never force the per-chunk cost onto the O(n) full rebuild, and the rebuild that follows reuses the freshly-updated cache entry for the streaming turn. `find_turn_at_row` maps screen rows to content lines with a single bottom-anchored formula that covers both the scrolled case and the no-scrollbar case (where the blank band above bottom-anchored content must not resolve to a turn), so header/image clicks work regardless of scroll state or viewport fill. `ModelSelectorState` backs the `Ctrl+M` popup: the daemon's model list, the active-model marker, a case-insensitive substring filter (reusing `InputBuffer` so editing is grapheme-aware), and a focus/scroll window that keeps the highlighted row visible; `window()` is pure (`&self`) so the renderer can call it during `terminal.draw()` without mutating scroll/focus. `AccountWizardState` and `CredentialModalState` (both in `state/pages.rs`) back the new-account wizard and API-key modals: the provider picker's filter/focus/scroll plus a picked-provider slug+name snapshot (so a mid-wizard catalog refresh cannot shift the pick), the slug entry field, and the masked key input with its `target` account. Both pickers (wizard step 1 and the model selector) share `step_focus` — the pure pin-at-middle arrow/wheel navigation: the highlight walks to the middle row (`height / 2`) of a static window, then pins there while `scroll` increments/decrements so the list slides under it, un-pinning at the edges to walk the rest of the way; `viewport_height` (cached by `update_viewport_from_terminal_size` from the LIST-popup body, 0 until the first frame) drives the middle-row math and degrades to focus-only moves while unknown; `filtered_len()` counts matching rows without building the filtered list, so navigation/clamping skip the per-press Vec allocation. A stale `(focused, scroll)` pair is clamped locally before stepping — `scroll` to the true `max_scroll` (`len − viewport_height`), symmetrically for both directions — and `clamp_focus` (run after every filter mutation, catalog refresh, and model-list reply) clamps `scroll` to `len − viewport_height` rather than `len − 1`, so a narrowing can never leave the hint past the bottom of the window. `state/layout.rs` owns the shared picker geometry — `PopupSize`/`centered_popup` (moved here from `render/` so both renderers and the connection-layer mouse handlers share them without an import cycle), `SelectorLayout`/`selector_list_layout` (the LIST popup's filter-row/body/footer bands), `selector_local_row` (click → body row mapping), `selector_click_target` (maps a left-click onto a filtered-list row via the *rendered* window start — `window()`'s start, never the stored `scroll` — or onto the filter row for cursor placement), `apply_selector_left_click` (the shared click application: returns the filtered-list row to select or positions the filter cursor, so each connection-layer handler keeps only its own confirm action) and `selector_position_filter_cursor` (grapheme-aware filter-row click cursor placement) — the single geometry used by rendering, hit-testing, and the viewport cache, mirroring the `chat_page_layout` pattern. The same geometry-sharing pattern extends to the two full-page lists (accounts + sessions): `page_list_content_rect` derives the bordered list content rect (minus the scrollbar column), and `ai_providers_list_click_index`/`session_list_click_index` map a left-click onto the drawn row — the accounts list resolves against its stored `scroll` (it draws directly from it), the session list against `window()`'s rendered start (never the possibly-stale stored anchor); both share the bounds check (`list_content_local_row`, the single definition of "inside the content rows"), and the accounts index additionally clamps to the drawn set via `ai_providers_drawn_count` (which mirrors the renderer's item loop, so a click in the blank band below the rows actually drawn can never select an account that is not on screen) — so each row click is a no-op outside its content rows (block border, status bar, scrollbar column, the session table's header row, or past the drawn tail), exactly like the picker popups. `update_viewport_from_terminal_size` caches the LIST-popup body height for both pickers only while one is actually open (the value is consumed solely by open-picker navigation/click handling, so the per-frame `Block`+`Layout::split` is skipped when neither is up). |
| `render/` | Ratatui rendering: history pane (top) + command input + status bar (bottom), word wrap, Unicode width. Does **not** mutate scroll state or viewport dimensions — those are updated in the event loop before `terminal.draw()`. `render_chat` runs the height-prefix rebuild (`compute_total_height_and_markers`) and then `selection::follow_cursor`, which re-anchors an in-progress selection's live head to the content under the cursor once content-induced viewport movement has settled (so a selection tracks the pointer even when the mouse never moved). `follow_cursor` is **fingerprint-gated**: it stores the (total height, scroll, viewport height) mapping inputs alongside the head and skips the re-resolution entirely on frames where none of them changed, so the every-frame draw-path sync costs one tuple compare instead of a re-derivation. An in-progress text selection is highlighted at draw time in `render_history`: `selection::apply_selection_to_lines` restyles the covered display-column range of each visible line with the `SELECTION_BG` background, applied to the per-turn visible slice without mutating the render cache — the same cached lines drive the highlight and the copy, so what is highlighted is exactly what gets copied. The model-selector popup is drawn last as a centered bordered overlay (`Clear` + `Block`): a filter row with a live cursor, the model list (active model marked `●`, keyboard highlight marked `>`), and a footer hint; loading/error/empty states replace the list. The account modals overlay the AI-providers page the same way (`render/ai_providers.rs`): the credential modal wins when both are open, then the wizard's provider picker (the `PopupSize::LIST` footprint shared with the model selector via `centered_popup`, with the filter-row/body/footer bands taken from the shared `selector_list_layout` — the same geometry the connection-layer mouse handlers and the viewport cache use) and its slug modal. |
| `syntax.rs` | Shared syntect helpers (`syntax_set`, `highlight_theme`, `to_ratatui_color`). Used by `markdown_render.rs` for code-block syntax highlighting. |
| `markdown_render.rs` | Terminal markdown renderer. Parses markdown (via `choreo-client-core`'s `pulldown-cmark` wrapper), renders blocks (paragraphs, headings, code, lists, tables, block quotes) into styled `ratatui::text::Line` vectors. Math is pretty-printed via `choreo-markdown`'s `render_math_pretty` (a LaTeX → Unicode mapper: Greek letters, operator symbols, sub/superscripts, `\frac{..}{..}` → `a/b`, `cases`/matrix environments, `\mathbb`): inline math (`$...$`) renders as an unbreakable word coloured yellow, display math (`$$...$$`) renders as a centred block line in magenta (wrapped left-aligned when too wide for the content width, breaking the paragraph before and after); the printer is a total, depth-bounded, table-driven parser that falls back to raw source for anything it cannot map — so streaming partial input (a half-arrived `\frac{`) and hostile constructs degrade gracefully instead of losing data — and table cells run their plain-text extraction through the same printer so math in a table reads like the surrounding cell content. Code blocks are syntax-highlighted via `syntect` (shared setup from `syntax.rs`). Tool results are rendered as collapsible sections: each result's header row (triangle + first line of the invocation description, falling back to the `tool result:`/`tool error:` label while the description is empty) is always drawn, the full invocation description (including any wrapped continuation lines) stays visible beneath it, and the body (label row + content) appears below only when expanded — the description is wrapped two columns narrower than the content width so the triangle-prefixed header row never overflows the viewport. Tool results are dispatched to one of three renderers in fixed order: ANSI-escaped content takes the colored `ansi_lines` path, error results and every non-allowlisted tool render as **plain text** (verbatim) — so `**` in a grep match or shell line is data, not emphasis, and hostile results cannot weaponize markdown syntax to restyle output — and only tools that emit markdown by design reach the styled markdown parser (`MARKDOWN_TOOLS`: `pdf_to_markdown`; the daemon's `git_diff`/`git_show`/`git_add`/`edit_file`, whose diffs arrive ` ```diff `-fenced; and `write_file`, which returns the written file's contents in a `fence_content`-sized code block tagged by `ext_to_lang`, so the fence renders as a syntax-highlighted code block rather than literal fence markers). Verbatim plain text is still pre-wrapped at the tool content width (`plain_text_lines` → `wrap_plain_line`): lines that exceed the width are broken at whitespace boundaries when one fits, else hard-split by grapheme cluster via the shared `grapheme_chunks` (the same hard-splitter `split_word_to_width` uses for markdown word-splitting), preserving every character — indentation and aligned columns included — so no rendered line overflows the non-wrapping `Paragraph` that draws the history. Every tool-result body first passes through `sanitize_for_terminal`, which keeps complete SGR color sequences verbatim (so `ansi_lines` coloring survives) but escapes every other control/format char — lone CR included (a CRLF pair is folded to a single `\n`), and via the shared `choreo_sanitize::is_unsafe_unicode` predicate — the sink defense that makes raw shell/VM streams safe to draw (see "Tool output sanitization and bounding"); it iterates with a `Peekable<Chars>` (no intermediate `Vec<char>`, one reused ESC-sequence buffer), so the streaming fast path stays O(chunk) per chunk. Sanitized content then passes through `expand_tabs`, which replaces each `\t` with spaces advancing to the next 4-column stop (tracking the column per logical line; complete SGR sequences are copied through without advancing the column, so a tab after a color code still pads to the correct stop): `unicode-width` measures `\t` as 0 columns and ratatui drops control chars at draw time, so a literal tab would both mis-measure every width computation (wrap, height `div_ceil`, fill padding) and silently vanish from the rendered output — expansion makes the measured width exactly what ratatui draws and keeps tab-aligned columns aligned. Diff rendering is now fully opt-in rather than content-detected: within the markdown renderer a ` ```diff ` fenced code block — the daemon emits every diff fenced that way (git tools via `append_fenced_diff`/`git_diff_impl`, `edit_file` inline in `format_edit_result`) — has its *interior* handed to `diff_render::try_render_diff_content` (side-by-side when the block width is ≥ 40, unified otherwise), while the surrounding text (e.g. `git_show`'s commit preamble) renders normally; a ` ```diff ` block whose interior isn't a parseable diff falls back to a literal code block. Because only fence interiors ever reach the diff parser, the raw `--- ` / `diff --git` auto-detection sniffs — and the `DIFF_EXCLUDED_TOOLS` gate they forced — are gone. Collapse defaults come from `tool_result_default_collapsed`: quiet tools (`read_file`, `read_file_range`, `http_request`) default collapsed — the old hard suppression of their verbatim bodies is now just a default state, so expanding a quiet tool reveals the content — while error results and everything else default expanded. Tool call labels (`tool: name(args)`) have been removed from the assistant block — tool invocations are now visible through the `invocation_description` rendered in each result's header, and through streaming output. The assistant block renders the response first, followed by a collapsible reasoning section: a dimmed header line (arrow glyph + "Reasoning") is always shown when reasoning content exists, with the reasoning body below it only when expanded (▼), or hidden behind a right-pointing arrow (▶) once the response arrives. A request-level failure (`Turn.error`) renders as a red block below the user text: the message is pre-wrapped at the content width through the same `plain_text_lines` wrapper (an unwrapped single line would clip at the viewport edge — long provider JSON truncates mid-token) after passing the same terminal-safety gate as tool output (`sanitize_for_terminal` + `expand_tabs`, so hostile bytes in a provider error body render inert), so every line fits the non-wrapping history `Paragraph` and the height math stays exact. `render_turn_lines` returns a `RenderedTurnLines` struct carrying the reasoning header's semantic-line index, each tool result header's semantic-line index, a per-line *content column range* (`content_ranges` — where each row's real text starts/ends, `None` for pure-chrome rows like box separators/padding, and an *empty* `(lo, lo)` range for blank *content* rows so the selection copy can tell the renderer's blank spacers (heading/paragraph breaks, blank lines in tool output) apart from chrome and keep them as blank lines), and a per-line *copy-join* record (`joins`, a `LineJoin` vector aligned with the lines) so callers never re-scan the rendered output to locate them or the selectable cells. The copy-join metadata is what lets a selection copy the *original* text instead of the renderer's wrapped rows: every row records how it glues to the row before it — `Break` for a fresh paragraph/block (a list item, a code line, a table row, box chrome), `Space` for a wrapped continuation whose reflow consumed a word boundary (the copy re-inserts the one separating space), and `Join` for a hard mid-word grapheme split (`split_word_to_width`/`grapheme_chunks`) or a plain-text wrap that kept its whitespace run on the previous chunk (`wrap_plain_line` cuts at whitespace boundaries and keeps the run on the previous chunk, so `Join` concatenation reproduces the input byte-for-byte). `wrap_styled_line`/`inlines_to_lines` record `Space` at word-boundary flushes and `Join` after split-word flushes; code lines, list items, and table rows stay `Break`; blockquote rows are forced `Break` so the per-row `"> "` markers never merge mid-copy. |
| `lib.rs` | `RenderedImage` struct, `build_picker()` helper, public re-exports |
| `image_worker.rs` | Background worker thread for image decode + terminal protocol encoding. SVG is rasterized via `resvg` (`usvg` + `tiny_skia`); HEIC/HEIF and raster formats go through the shared `choreo-image` decoder (EXIF orientation baked in; HEIC gated by a pre-decode allocation guard) so phone/camera photos render upright. Communicates with the UI thread via `mpsc` channels; raw image data shared through `Arc<Vec<u8>>` to avoid copies. |
| `terminal_progress.rs` | Terminal-native progress bar via OSC 9;4 escape sequences. Cached capability detection, percentage/indeterminate/remove modes based on `last_prompt_tokens` vs `context_window`. |
| `selection.rs` | Mouse text selection over the chat history pane (select-to-copy, mirroring opencode). A drag gesture is tracked in *content* coordinates (a global content line plus a viewport column — `TextSelection` on `App`, which also records the last cursor screen position and the layout fingerprint the head was resolved against), so the anchor stays pinned to the text it was drawn over when the viewport moves, while the live drag head re-resolves to the content under the cursor (terminal-native drag-while-scroll) — on wheel events immediately, and at draw time via `follow_cursor` (called right after `compute_total_height_and_markers`, fingerprint-gated so an idle frame costs a tuple compare) whenever *content-induced* movement (streaming growth, appended turns, undo/redo) shifts the history under a stationary pointer; on mouse-up the covered rows are resolved — via the same height-prefix + render-cache machinery the click hit-testing uses, the exact inverse — to the plain text they display, which the caller hands to `clipboard`. Because the history renders pre-wrapped rows, the copy uses each row's per-line copy-join metadata (`RenderedTurn::joins`, a `LineJoin` vector produced by the renderer) to *un-wrap*: rows marked `Space` (a word-boundary reflow) are trimmed at the seam and re-joined with the single space the wrap consumed, rows marked `Join` (a hard mid-word split, or a plain-text wrap that kept its whitespace on the previous row) concatenate directly, and everything else (a fresh paragraph/block, a real newline, a turn boundary) stays a `\n` — so selecting a wrapped assistant response yields the original unwrapped paragraph, and selecting long verbatim tool output reproduces it byte-for-byte, instead of the old behavior of copying the display's wrap points as newlines. Rows that resolve to no content (pure-chrome rows, image blocks, lines past the end) are skipped, so what is highlighted is exactly what is copied; blank *content* rows — the renderer's spacer rows between blocks — resolve to an empty slot, so a blank line inside the selected text survives the copy as a blank line (the `Break` join of the row after it re-inserts the newline) instead of collapsing into its neighbour. The draw-time highlight re-evaluates the content-anchored selection against the current scroll every frame. Both the highlight and the copy are clamped to each line's *content range* — the renderer records where every line's real text starts and ends (`RenderedTurn::content_ranges`), excluding the box chrome (`┃` gutter, indents, trailing fill), so selecting an assistant response never drags the surrounding box into the selection. The gesture only becomes a real selection once the drag moves past the anchor; a plain click keeps its existing toggle/cursor behavior and copies nothing. Line slices snap to grapheme boundaries (never split a ZWJ emoji), use display columns for wide characters, and use terminal-native *anchor-fixed* columns on reverse (bottom-to-top) drags: the anchor row extends from the anchor column to end-of-line and the head row from start-of-line to the head column, so a reverse diagonal mirrors the columns instead of swapping them (the old lexicographic normalization swapped them). The draw-time highlight (`apply_selection_to_lines` → `style_line_selection`) restyles the covered column range with a solid selection background (`SELECTION_BG`, a mid-blue) — not `Modifier::REVERSED`, which is invisible on the shaded `BG_SHADE` turn boxes — without mutating the render cache. The status line reads "Selection copied to clipboard." Selection is cleared on session switch, suspend, page switch, opening the model selector, and terminal resize (a resize re-wraps every line, so the stored column anchor would point at different text afterwards) so a stale rectangle never highlights a different session's content. The extraction path and the draw-time highlight share one row→line→column mapping (`content_range_for_row`), so the two can never drift apart again — they already diverged twice (the screen-row offset and within-line column bugs, both pinned by regression tests); the gesture state machine lives in `selection.rs` (`handle_selection_mouse`), so the connection loop only performs the clipboard write and the status message. The production module is split from its tests: `selection.rs` holds the gesture/extraction/highlight code, `selection/tests.rs` (same `src/` tree, `mod tests;` under `#[cfg(test)]`) holds the unit tests. |
| `clipboard.rs` | OSC 52 clipboard writer: `copy_to_clipboard` encodes the text as base64 and writes `ESC ] 52 ; c ; <payload> ST` to stdout, mirroring `terminal_progress`'s OSC 9;4 usage. The terminal mediates the write, so it works over SSH/tmux (the *local* clipboard), is a silent no-op on terminals without OSC 52 support (e.g. macOS Terminal.app), and can be refused by the terminal without affecting the TUI. Selections larger than 1 MiB are refused up front (the caller shows a "too large to copy" status) rather than stalling the UI loop on a multi-megabyte escape sequence terminals may drop anyway. `build_osc52` is a pure function, so the byte layout is unit-tested without a terminal. A write is only ever triggered by a user-initiated mouse-up over their own selection — hostile LLM/tool output can never inject a clipboard write through this path. |


### `choreo-gui` — Desktop/Android client

Entry point: `src/bin/choreo-gui.rs` (thin wrapper calling `choreo_gui::main()`
in `src/lib.rs`) — the crate owns its binary, unlike the daemon/TUI/IM/ACP
which live in the root package.

Unix socket or Noise IK encrypted TCP transport (selected via `--tcp-addr` / `--server-pk` CLI flags),
rendered via Dioxus components on the Dioxus Native (Blitz/wgpu) renderer —
one renderer for both desktop and Android (no webview anywhere; the crate is
built as a lib+cdylib so dx/gradle can package it as an APK). Uses hooks to spawn async reader/writer tasks inside
the Dioxus runtime. Subscribes to the session summary at connect
(`SubscribeSessionsSummary`, alongside the initial `ListSessions`) so its session
list stays live via daemon push broadcasts — required since the daemon stopped
auto-registering TCP clients as summary subscribers.

**Module breakdown:**

| Module | Purpose |
|---|---|
| `client.rs` | `run_client()` — socket split, reader/writer, daemon message dispatch |
| `state.rs` | `AppState` with input, request tracking, `ClientHistory` |
| `render.rs` | RSX rendering of history items: markdown → sanitized HTML, images via `data:` URLs, structured diffs via `format_diff_file` |
| `lib.rs` | clap CLI, Dioxus `App` component, toolbar, history pane, textarea composer, CSS |


### `choreo-im` — IM platform bridge

Entry point: `src/lib.rs` (the root package's `src/bin/choreo-im.rs` is a thin wrapper calling `choreo_im::main()`)

Single binary (`choreo-im`) that bridges IM platforms to the daemon.
The binary takes a single required positional platform argument via clap:
`choreo-im telegram`. Like the rest of the suite it supports `--help` and
`--version`, with help output styled by a per-crate `clap_styles()` helper
(duplicated in each CLI crate — choreo-proto is the wire protocol and does
not host CLI styling) and `ColorChoice::Auto` (color only on a TTY).

**Credentials:** The daemon serves platform credentials via the `GetCredential` wire
message. The admin stores credentials via `/add-key` or `/add-x` at runtime, which
encrypts them with the daemon's public key. On unlock (`/unlock`) the daemon decrypts
all stored credentials into memory using its private key.

**Module breakdown:**

| Module | Purpose |
|---|---|
| `lib.rs` | CLI entry (clap), daemon handshake (Unlock, GetCredential), platform dispatch |
| `bridge.rs` | `DaemonBridge` — Unix socket read/write tasks, text buffering, image assembly, `BridgeEvent` enum |
| `telegram.rs` | Telegram bot: teloxide polling, admin-only filter, command dispatch, HTML formatting |

**Data flow:**

```
Telegram user → teloxide polling → handle_message()
  → parse_input_line() → ClientMessage → bridge.send()
  → daemon
  → DaemonMessage → bridge reader task → BridgeEvent
  → send_daemon_event() → Telegram HTML/photo
```


---

## Security model

### Lock/Unlock flow

The daemon starts in a **locked** state. The client resolves the private key (reading
`identity.pk` directly or decrypting `identity.pk.enc` with a passphrase) and sends
it to the daemon via `ClientMessage::Unlock { private_key }`.

```
startup                    /unlock [passphrase]
   │                              │
   │  locked                      │  read identity.pk (or decrypt identity.pk.enc)
   │  (no credentials)            │  decrypt all credential blobs from database
   │                              │  load accounts from accounts.toml
   │                              │  resolve InferenceProvider per account
   │                              │  → Unlocked (ready)
   ▼                              ▼
```

- Credentials are encrypted per-credential with ECDH (X25519) + HKDF + AES-256-GCM
- The private key is sent over the Unix socket; zeroized after use by the daemon
- `/lock` destroys all in-memory credentials, returning to locked state
- `LockedError` is sent if any client attempts a request that requires credentials while locked
- Session lifecycle operations (CreateSession, AttachSession, ListSessions, etc.) succeed even
  when locked — credentials are only needed at RunInput time. Provider resolution is lazy:
  when RunInput is called, the session thread resolves the InferenceProvider from the daemon's
  provider registry. If no credential is available for the session's account, a clear error is
  returned telling the user to add a key.
- There is no global "default account". Each session carries its own `account_name: Option<String>`
  field. When RunInput is issued, the daemon resolves the provider from the session's own account name.
  This avoids a global mutable fallback and lets different sessions use different accounts.

### Dependency supply chain

The workspace ships Rust that must trust its third-party dependencies, so the dependency
graph is treated as part of the security surface. On 2026-08-20 `arrayref` — a transitive
dependency here via `tiny-skia` → `usvg`/`resvg` (SVG rendering in the daemon and TUI) and
`blake2b_simd` → `subxt` (blockchain feature, off by default) — was republished as `0.3.10`
from a compromised maintainer account with a dependency on payload-downloading crates
(RUSTSEC-2026-0260); the malicious versions were deleted from crates.io ~1.5–2h later.
Four layered controls make a repeat fail loudly instead of landing silently:

1. **Committed `Cargo.lock`** — every locked package carries its checksum, so builds resolve
exactly what the lockfile pins. `just test-all`, `just clippy`, and `scripts/release.sh`
(the release build path, incl. the musl cross-build) pass `--locked`, making the committed
lockfile authoritative: a silent regeneration fails the command rather than silently
re-resolving against the live registry.
2. **`deny.toml`** (`cargo-deny`) — hard bans on every version from the 2026-08-20 attack
(`arrayref =0.3.10`, `internment =0.8.7`, `append-only-vec =0.1.9`, plus the six deleted
payload crates by name: `proc-macro1`, `proc-macro-en`, `aovine`, `arone`, `aronenao`,
`tinymember`), RustSec advisory checking (vulnerability and "malicious" advisories always
fail in cargo-deny 0.20; unmaintained only for direct deps), and a crates.io-only source
restriction (all 1261 locked packages currently resolve from the crates.io index).
3. **`scripts/check-supply-chain.sh`** — a first scan of the local `~/.cargo/registry` cache
for the DELETED malicious `.crate` files (neither cargo-deny nor cargo-audit inspects idle
cache files; this is the Rust Security Response Team's own remediation `find`, run fresh on
every gate), then the cargo-deny policy check, with a `cargo-audit` + literal lockfile-scan
fallback when cargo-deny isn't installed.
4. **RustSec advisory database** — the RUSTSEC-2026-0259..0266 series covering the attack is
in the DB both tools fetch, so re-introducing any attacker crate fails the gate even without
the explicit bans in `deny.toml`.

The strongest remaining option — bit-for-bit reproducible builds from a checked-in
dependency snapshot via `cargo vendor` + a `[source]` replacement in `.cargo/config.toml` —
is intentionally not enabled (repository size).


---

## Tool system

### Generic `Tool` trait

The tool trait is generic over argument and return types. Each tool declares its own
`type Args` (must implement `DeserializeOwned + JsonSchema`) and `type Return`
(must implement `Serialize + JsonSchema`). Both `schema()` and `output_schema()` are
auto-derived via `schemars` by default, eliminating the need for hand-written JSON schemas.
The generated schemas are then sanitized — `$schema`, `title`, and `$defs`/`$ref` are
stripped/resolved for compatibility with providers that do not support Draft 2020-12
meta-schema features, and `additionalProperties: false` is injected for parameter schemas.

```rust
pub trait Tool: Send + Sync {
    type Args: DeserializeOwned + JsonSchema + 'static;
    type Return: Serialize + JsonSchema + 'static;
    /// Error type — each tool defines its own. Simple tools use `ToolExecError`
    /// (a string-wrapper). Tools whose errors are consumed by VM guests (e.g.
    /// `DbError`, `HttpError`) define a `thiserror` enum that is serde-serializable,
    /// enabling the guest to pattern-match on specific variants.
    type Error: std::error::Error + Send + Sync + Serialize + DeserializeOwned + 'static;

    fn name(&self) -> &'static str;
    fn group(&self) -> &'static str { "core" }
    fn description(&self) -> &'static str;

    /// Auto-derived JSON Schema for the tool's input arguments.
    /// Sanitized via `sanitize_params_schema` (strips `$schema`/`title`/`$defs`,
    /// resolves `$ref`s inline, injects `additionalProperties: false`, converts
    /// unit-arg `{"type":"null"}` to empty object).
    fn schema(&self) -> serde_json::Value {
        sanitize_params_schema(
            serde_json::to_value(schemars::schema_for!(Self::Args)).unwrap_or_default(),
        )
    }

    /// JSON Schema for the tool's return value (for Programmatic Tool Calling).
    /// Auto-derived from the return type. Override for types schemars cannot represent.
    /// Sanitized via `sanitize_output_schema` (same as above but without
    /// `additionalProperties`).
    fn output_schema(&self) -> Option<serde_json::Value> {
        Some(sanitize_output_schema(
            serde_json::to_value(schemars::schema_for!(Self::Return)).unwrap_or_default(),
        ))
    }

    /// Controls which callers can invoke this tool
    /// (`Direct`, `Programmatic`, or both).
    fn allowed_callers(&self) -> Vec<AllowedCaller> {
        vec![AllowedCaller::Direct, AllowedCaller::Programmatic]
    }

    fn execute(
        &self,
        args: Self::Args,
        x_credentials: Option<&ServiceCredential>,
        working_dir: Option<&Path>,
        ctx: Option<&ToolContext>,
    ) -> Result<Self::Return, Self::Error>;

    fn execute_streaming(
        &self,
        args: Self::Args,
        x_credentials: Option<&ServiceCredential>,
        working_dir: Option<&Path>,
        _output_tx: mpsc::Sender<Vec<u8>>,
        ctx: Option<&ToolContext>,
    ) -> Result<Self::Return, Self::Error> {
        // Non-streaming tools deliver their result via TurnAppended —
        // no ToolResultChunk traffic needed.
        self.execute(args, x_credentials, working_dir, ctx)
    }

    fn extract_image(&self, _ret: &Self::Return) -> Option<PreparedImage> { None }

    /// Produce a human-readable description of what the tool is about to do,
    /// using every supplied argument for detail (e.g. "Reading file `main.rs`.",
    /// "Making POST HTTP request to `https://api.example.com/data`.").
    /// Returns a natural English sentence. There is no default — every tool
    /// must provide one. The value is stored in `ToolOutput.invocation_description`
    /// and flowes through to `ToolResultRecord.invocation_description` for the
    /// TUI to render as the first line of the tool result block.
    fn describe_invocation(&self, args: &Self::Args) -> String;

    /// Produce a human-readable string from the return value.
    /// The default implementation JSON-encodes the value.
    /// Tools whose `Return` is `String` override this to return
    /// the raw string directly (e.g. shell tools, macro-defined tools).
    fn return_string(ret: &Self::Return) -> String {
        serde_json::to_string(ret).unwrap_or_default()
    }
}
```

`ToolOutput` replaces the old `ToolExecutionOutput` + `ToolResult` pair:

```rust
pub enum ToolOutputFormat { Text, Json }
pub struct ToolOutput {
    pub content: String,
    pub is_error: bool,
    /// Human-readable sentence describing what the tool is about to do,
    /// produced by `Tool::describe_invocation()` before execution.
    /// Empty string when the description is unavailable (e.g. spawned
    /// thread error paths before the description could be generated).
    pub invocation_description: String,
    /// The tool's structured return value (`serde_json::to_value(ret)`),
    /// populated by the blanket `ToolDyn` impl after a successful execution.
    /// `None` for error/timeout outputs.  The request worker reads this to
    /// mirror session-config mutations (e.g. `set_working_dir`'s canonical
    /// path) onto its config copy without re-executing the tool.
    pub result_json: Option<serde_json::Value>,
}
```

`Text` format is used for LLM-facing tool results (human-readable, uses `return_string`).
`Json` format is used for Programmatic Tool Calling (PTC) — JSON-encodes the return via `serde_json::to_string`.
`invocation_description` is stored in `ToolResultRecord` and seeded onto every placeholder result when the
model's tool calls are recorded, so clients render the tool's context (e.g. "Running command: `…`.")
the moment the seeded turn is broadcast — before any output streams. It is explicitly excluded from LLM
message construction — the model never sees it.

Tools that need session context (`ToolContext` — used by `list_sessions`, `get_session`)
receive it in the `ctx` parameter. Tools that return structured data override
`output_schema()` to describe their return JSON shape, enabling the model to call
them programmatically (see [Programmatic Tool Calling](#114-programmatic-tool-calling-responses-api-gpt-56)).
Tools can restrict callers via `allowed_callers()`, gating whether the model calls
them directly, from generated JavaScript, or both.

Tools that produce images (e.g. `display_image`) override `extract_image()` to return a
`PreparedImage` from the typed return value. The conversion layer (see `ToolDyn` below)
sends the image through an out-of-band `image_tx: Option<mpsc::Sender<PreparedImage>>`
channel rather than embedding it in the response struct. The agent loop drains this
channel after execution to persist and broadcast the image.

### `ToolDyn` — type-erased dispatch trait

The `ToolDyn` trait erases the generic parameters so tools can be stored in a `HashMap`:

```rust
pub trait ToolDyn: Send + Sync {
    fn name(&self) -> &str;
    fn group(&self) -> &str;
    fn description(&self) -> &str;
    fn schema(&self) -> serde_json::Value;
    fn output_schema(&self) -> Option<serde_json::Value>;
    fn allowed_callers(&self) -> Vec<AllowedCaller>;

    /// Human-readable invocation description from JSON args.
    /// Delegates to `Tool::describe_invocation` via the blanket impl.
    /// Returns the static `description()` fallback when args fail to parse.
    fn describe_invocation_json(&self, args_json: &str) -> String;

    /// JSON path — takes JSON args, returns Result so callers can distinguish
    /// infrastructure errors (deserialisation failures) from tool errors.
    fn execute_json(&self, args_json: &str, format: ToolOutputFormat, ...) -> Result<ToolOutput, ToolError>;
    /// Streaming JSON path.
    fn execute_streaming_json(&self, args_json: &str, format: ToolOutputFormat, ...) -> Result<ToolOutput, ToolError>;
    /// Postcard binary path (VM ecall). Returns bytes encoding
    /// `Result<Result<T::Return, T::Error>, ToolError>` — all outcomes
    /// (infra error, tool error, tool success) are contained in the buffer.
    fn execute_postcard(&self, args_bytes: &[u8], ...) -> Vec<u8>;
}
```

A blanket impl `impl<T: Tool> ToolDyn for T` provides `describe_invocation_json` (deserializes
args and delegates to `Tool::describe_invocation`, falling back to `description()` on parse
failure) and all three dispatch paths:

| Path | Input | Output | Used by |
|---|---|---|---|---|
| `execute_json` | `&str` (JSON) + `ToolOutputFormat` | `Result<ToolOutput, ToolError>` | LLM tool calls (OpenAI/Anthropic etc.) |
| `execute_streaming_json` | `&str` (JSON) + `ToolOutputFormat` | `Result<ToolOutput, ToolError>` | Streaming shell/VM tools via LLM |
| `execute_postcard` | `&[u8]` (postcard) | `Vec<u8>` (postcard of `Result<Result<R, E>, ToolError>`) | RISC-V VM tool calls |

The JSON path deserializes arguments with `serde_json`, calls `Tool::execute()`, then
returns a `ToolOutput`. Both `execute_json` and `execute_streaming_json` first call
`T::describe_invocation(self, &args)` to produce the invocation description, then store
it on the returned `ToolOutput`. In the streaming path the description is deliberately
NOT sent as a chunk (a chunk can be dropped under load, and a chunk without a trailing
newline would be mashed against the first output line): it is delivered reliably via the
`ToolCallStarted` broadcast (queued before the tool starts) and on the seeded placeholder
result, so clients render the same header live and in the final record. When `format` is
`Text`, the content is produced via `T::return_string()` (human-readable). When `format`
is `Json`, the return value is JSON-encoded via `serde_json::to_string()` (for PTC
responses). The binary path uses `postcard` for both deserialization and serialization,
enabling compact cross-VM communication.

### `define_tool!` macro

The `define_tool!` macro reduces boilerplate for the common tool case
(`Return = String`, no credentials needed). It lives in `choreo-daemon/src/tools/mod.rs`.
The JSON schema is auto-derived from the args type via `schemars`, so no manual
schema parameter is needed. The macro now takes 7 arguments — the 7th is a
`fn(&Args) -> String` path that provides the invocation description:

```rust
define_tool!(MyTool, "my_tool", "Description...", MyToolArgs,
    execute_my_tool, "core", describe_my_tool_invocation);
```

The describe function is also used by the blanket `ToolDyn::describe_invocation_json`
implementation and by `ToolRegistry::describe_invocation`.

Tools that need custom `output_schema()`, `allowed_callers()`, non-`String` return types,
session context (`ToolContext`), or credentials (`ServiceCredential`) are written as
manual `impl Tool` blocks instead. Examples:
`DbGet`/`DbGetRange`/`DbList`/`DbCount` (custom `output_schema`),
`GetCurrentTime` (`Return = u64`), `DisplayImage` (overrides `extract_image`),
`ListSessions`/`GetSession` (need `ToolContext`).

### Registry

Tools are registered in a `ToolRegistry` stored as `Box<dyn ToolDyn>`. The registry is
owned by `DaemonStateInner`, constructed once at daemon startup. The agent loop extracts
an `Arc<ToolRegistry>` from the daemon state to list available tool definitions and
dispatch tool execution.

The registry provides `describe_invocation()`, `describe_invocation_for()`,
`execute_json()`, `execute_streaming_json()`, and `execute_postcard()` for dispatch:

```rust
pub fn describe_invocation(&self, tool_call: &ChatToolCall) -> String;
pub fn describe_invocation_for(&self, name: &str, args_json: &str) -> Option<String>;
pub fn execute_json(&self, tool_call: &ChatToolCall, format: ToolOutputFormat, ...) -> ToolOutput;
pub fn execute_streaming_json(&self, tool_call: &ChatToolCall, format: ToolOutputFormat, ...) -> ToolOutput;
pub fn execute_postcard(&self, name: &str, args_bytes: &[u8], ...) -> Vec<u8>;
```

`describe_invocation` returns the invocation description for a tool call by name + JSON args,
falling back to the tool name for unknown tools. `describe_invocation_for` returns `None`
for unknown tools. These are used by `run_agent_loop` to generate the description before
spawning tool threads, so error paths (timeout, panic) can include it in the `ToolOutput`.

`execute_postcard` replaces the old `execute_dyn` and calls `ToolDyn::execute_postcard()`.
`execute_json` and `execute_streaming_json` accept a `ToolOutputFormat` parameter so
callers can choose between `Text` (LLM) and `Json` (PTC) output formats.

Each tool receives an optional `working_dir: Option<&Path>` parameter that represents the session's
working directory. Filesystem and Git tools resolve relative paths against this working directory.
A leading `~` or `~/` in any path argument is expanded to the user's home directory via
`expand_tilde()` inside `resolve_path()`, so callers can write `~/project` instead of the
full absolute path. The `~user` form is *not* expanded and is passed through unchanged.

### File-read tool limits

`read_file` and `read_file_range` share a streaming, memory-bounded design. Each tool
lives in its own module (`tools/read_file.rs`, `tools/read_file_range.rs`); the shared
streaming and binary-sniff helpers (`open_text_reader`, `TextStream`, `render_streamed_line`,
`OutputBudget`, `read_line_capped`, `drain_rest_of_line`) live in `tools/text_stream.rs`;
the sanitization suite (`sanitize_name`, `sanitize_text`/`sanitize_content`,
`sanitize_transcript`, `sanitize_multiline`, `truncation_marker`, …) lives in
`tools/sanitize.rs`; and the shared byte budget,
`truncate_tool_output`, and `finish_tool_output` now live in the
`choreo-sanitize` leaf crate and are re-exported from `tools/mod.rs` (alongside the
split-out helpers, so every `crate::tools::X` reference keeps resolving unchanged).
The line-oriented output-formatting helpers `human_size` and `symlink_target_label`
stay in `tools/mod.rs`.
`finish_tool_output` caps a body at the shared
byte budget, reserving room *inside* the budget for the marker/footer it
appends — so the count signal survives even a byte-capped result, and stays
alive through the transcript re-cap in `record_tool_completion` (which
re-applies the cap after `sanitize_transcript`; a tail riding past the budget
would be cut off there). `TextStream` yields one capped line at a time
with byte accounting; `render_streamed_line` validates and renders a single line (NUL /
UTF-8 checks, CRLF normalization, control-character escaping, truncation marker);
`OutputBudget` enforces the shared
byte cap across appended lines.

- **Binary rejection:** both tools peek the first 8 KiB (`BINARY_SNIFF_BYTES`) and reject
  files containing a NUL byte with a friendly `"appears to be a binary file"` error,
  mirroring ripgrep's heuristic. Returned content is always valid UTF-8 — invalid UTF-8
  in the head or in a returned line yields an explicit `"not valid UTF-8"` error rather
  than a raw std I/O error. The head is *always* sniffed, regardless of the requested
  `read_file_range` window; beyond the head, only lines that are actually returned are
  validated, so invalid content outside the requested range is skipped, not rejected.
- **Control-character escaping:** every returned line is also run through the shared
  `sanitize_content` policy (tabs kept; ESC, backspace, U+2028/U+2029, and the Unicode
  format-char spoofing set escaped) — the same defense `grep` applies to matched lines,
  so a hostile file cannot inject terminal escape sequences or bidi-spoof the transcript
  through the file-read tools either (see "Tool output sanitization and bounding").
- **Line cap:** `read_file_range` returns at most 500 lines per call
  (`MAX_READ_FILE_RANGE_LINES`); larger requests fail with a validation error, while
  requests that run past EOF clamp to the last line.
- **Output budget:** tool output is capped at 128 KiB **bytes**
  (`MAX_TOOL_OUTPUT_BYTES`). Bytes are used rather than chars so the effective token cost
  is roughly uniform across scripts (ASCII and CJK are both ~3-4 bytes per token).
  Truncation always reports totals — `showing X of Y bytes; file has N line(s)` — so the
  agent knows what it is missing and can switch to `read_file_range`. For
  `read_file_range`, X counts the returned content up to the marker — body +
  prepended header + separator newline — so the reported figure matches the
  bytes actually returned (the marker text itself is appended past the budget).
- **Per-line cap:** a single line longer than 64 KiB (`MAX_LINE_DISPLAY_BYTES`) is shown
  as a truncated prefix with a `...[line truncated]` marker; the remainder is drained
  (counted for totals, never buffered).
- **Memory:** both tools stream via `BufReader` + `TextStream` (`read_line_capped`),
  holding at most one capped line plus the output budget in memory regardless of file
  size (previously the whole file was loaded via `read_to_string`).

### Tool output sanitization and bounding

Tool output is defended in three layers — at the **source** (the tool that produces the
bytes), at the **transcript** (what the model sees on the next call), and at the
**sink** (the terminal that renders it):

- **Source.** `sanitize_text` / `sanitize_name` / `sanitize_content` (in `tools/sanitize.rs`)
  escape C0/C1 controls, the line/paragraph separators U+2028/U+2029, and every Unicode
  *format* character (general category Cf) except the joiners U+200C/U+200D, via
  `char::escape_default`. The spoofing predicate itself is the shared
  `choreo_sanitize::is_unsafe_unicode` (the leaf crate that owns the policy — the
  blockchain tools and the TUI use the same one). The line-oriented tools use them so
  every listing stays one
  line per entry and a hostile name/line cannot inject terminal escapes (`grep` on match
  and context lines; `find` and `list_files` on paths and symlink targets; `pdf_*` on
  log fields and invocation descriptions). The same policy now covers the raw-content
  readers: `render_streamed_line` sanitizes every `read_file`/`read_file_range` line,
  and `http_request` runs response bodies through `sanitize_multiline` (a
  newline-preserving variant for content that legitimately spans lines) and header
  values through `sanitize_name` — so a hostile file or HTTP response cannot inject
  terminal escapes or bidi-spoof the model no matter which tool delivers it.
- **Transcript.** `sanitize_transcript` escapes only the Cf format chars (the spoofing
  class — bidi overrides, ZWSP, invisible operators, …) at the single point where every
  tool result is recorded (`record_tool_completion` in `requests/tool_execution.rs`), preserving
  ESC/ANSI, newlines, and tabs so shell/VM colors survive. Because escaping *expands*
  (a Cf char becomes `\u{202e}`), the choke point re-applies the byte cap **after**
  sanitizing (`truncate_tool_output(&sanitize_transcript(…))`), so content that was
  capped at the source as raw bytes (shell/VM/series) cannot exceed the budget once
  escaped. Tools that append a critical tail to *raw* output (the VM exit footer,
  `format_shell_output`'s `Exit code:` line, `pdf_to_markdown`'s closing
  untrusted-content delimiter) sanitize **before** the cap instead, via
  `finish_tool_output_sanitized` (in `tools/sanitize.rs`): `sanitize_transcript` is
  idempotent on its own ASCII escape output, so the choke point's re-sanitize is a
  no-op and its re-cap cannot cut the tail off — closing the residual gap where a
  Cf-heavy raw body near the cap would expand past the budget and lose its footer.
  This closes the remaining
  LLM-spoofing gap for the streaming tools whose raw output is deliberately not
  source-escaped.
- **Sink.** `choreo-tui`'s `sanitize_for_terminal` filter runs over every tool-result
  body before rendering: it keeps complete SGR color sequences (`ESC [ … m`, so ANSI
  coloring still works) and escapes everything else — OSC/CSI/DCS sequences, C0/C1
  controls (including lone CR: a carriage return not followed by a line feed
  would let hostile content overwrite its own rendered line; a CRLF pair is folded to
  a single `\n`, matching the daemon's line sanitizers), U+2028/U+2029, and the Cf spoofing class (via the same shared
  `choreo_sanitize::is_unsafe_unicode`). This is what makes the raw
  shell/VM streams safe to draw; it defends against terminal-escape injection (OSC-52
  clipboard writes, clear-screen, title changes, …) for every tool at once, including
  the tools that never sanitize at the source.

**Streaming is byte-bounded end to end.** The bounded streaming channel only bounds
*in-flight* chunks (backpressure); the *total* is capped too, so the live view can never
diverge unboundedly from the recorded result. (Note the two independent bounds: this
section is about `ByteBudget` capping a single tool's *content*; the lossless delivery
design separately bounds *delivery* — the per-client in-flight bytes that trigger
lag-eviction — see the broadcast section below. They don't collide: content is capped
at the source, delivery is capped per client queue.)

- `spawn_with_streaming` (sh/exec/fish/nu) streams **both** stdout and stderr:
  the two pipes are drained in background threads, split into lines (CRLF
  folded; oversized unterminated lines are flushed forward as partial chunks
  that never split a UTF-8 char and hold back a trailing `\r` so CRLF still
  folds), and merged onto one channel in arrival order; a single consumer
  escapes the Cf spoofing class (`sanitize_transcript`) *before* the bytes
  enter the shared `ByteBudget`, then forwards the escaped lines (the same
  "first N bytes + one marker" engine) and accumulates the same capped bytes
  into the returned body. Budgeting the *escaped* form is what makes the
  record byte-identical to the live view even for Cf-heavy output — escaping
  expands, and charging the budget for the expanded bytes keeps
  `finish_tool_output`'s cap a no-op (no re-cut, single marker). The stream
  budget reserves `format_shell_output`'s framing (the `$ {cmd}\n` header —
  measured at its escaped size — the exit-code footer, and the truncation
  marker) *inside* the cap via `RecordFraming`, so the final cap is a no-op
  and the exit-code footer always survives (including the transcript re-cap
  in `record_tool_completion`). A tool that writes progress to stderr (cargo,
  nextest, make, …) streams live instead of appearing all at once. On a
  timeout the watchdog kills the child and signals an abort channel; the
  merger selects on it while blocked on a full output channel, so a stalled
  subscriber can never wedge the tool past its timeout.
- `find`'s walk enforces the byte budget during collection (charging each rendered line
  plus its joining newline), stopping the walk and reporting the collected count in the
  marker — the streamed view and the final result now agree. The walk's budget reserves
  the finish tail *inside* it (the truncation marker plus the generic `...[truncated]`
  suffix `finish_tool_output` holds back), so the final cap never re-cuts the body.
- `run_riscv` caps guest `WRITE` output at the syscall via `ByteBudget` (both the
  accumulated and the streamed copies): a write that would cross the cap is kept as a
  fitting prefix, the one-shot truncation signal fires, and the streamed live view gets
  the shared marker. `finish_tool_output` then wraps the final content, reserving
  room inside the budget for the exit footer (and the truncation marker, when
  output was cut) so the signal always survives — including the transcript re-cap
  in `record_tool_completion`.
- `run_series` caps the aggregated step JSON with `finish_tool_output` (each step is
  capped, but N steps joined could exceed the budget).
- The clients cap their *live* accumulation too: `SessionView::tool_result_chunk` in
  `choreo-client-core` stops at the same shared `MAX_TOOL_OUTPUT_BYTES` (128 KiB)
  budget with the same one-time `...[truncated]` marker (a chunk landing exactly on the
  cap still marks the next chunk truncated, matching the daemon's `ByteBudget`), so a
  chatty tool cannot balloon client memory before the (authoritative, capped) final
  record replaces it.

### PDF tools

`pdf_classify` and `pdf_to_markdown` (both under `tools/pdf/` — one file per tool,
`classify.rs` and `markdown.rs`, with the shared helpers in `mod.rs` following the
workspace's one-tool-per-file convention) give the agent native PDF
ingestion by wrapping `pdf-inspector` (Firecrawl) — a pure-Rust, extraction-only PDF
parser built on `lopdf`. The parser has no JavaScript engine, never renders pages, and
never executes embedded files or `/Launch` actions, so the classic PDF malware
*execution* vectors are excluded by construction.

> **Dependency — security.** `pdf-inspector` is an **unconditional registry dependency**
> of `choreo-daemon` (`pdf-inspector = "1"`, version 1.x, no feature gate). The old
> arrangement — a crates.io 0.1 dep behind the optional `pdf` feature plus a
> workspace-root `[patch.crates-io]` redirect to a contributor fork
> (`omeileo/pdf-inspector@f86decf`, upstream PR firecrawl/pdf-inspector#198) — was removed
> in the 1.15.0 update: crates.io 0.1.x shipped `lopdf ^0.41.0`, vulnerable to
> RUSTSEC-2026-0187 (a ~21 KB crafted PDF with ~10,000-deep nested objects aborts the
> process via stack overflow — a SIGABRT that `catch_unwind` **cannot** intercept), and the
> fork bumped `lopdf` to 0.42.0 (MAX_NESTING_DEPTH). Upstream 1.15.0 now ships
> `lopdf >= 0.42` with `default = []`, so the pin, the `[patch.crates-io]` entry, and the
> `pdf` feature gates in the root and `choreo-daemon` `Cargo.toml` were all deleted. The
> regression guard `nested_array_poc_does_not_abort_process` in
> `tests/pdf_tool_integration.rs` still covers the vuln: with `lopdf >= 0.42` the parser
> caps nesting depth, so the PoC yields a clean parse error or a graceful `SCANNED`
> classification (exit 0) instead of SIGABRT.

- **`pdf_classify`** runs `pdf_inspector::detect_pdf_mem` (DetectOnly mode, ~10–50ms) and
  reports `pdf_type` (text_based / scanned / image_based / mixed), `confidence`,
  `page_count`, and `pages_needing_ocr` — the smart-routing signal for deciding between
  local extraction and OCR.
- **`pdf_to_markdown`** runs `process_pdf_mem_with_options` (Full mode) with optional
  1-indexed `pages` and an opt-in `compact` profile (`MarkdownProfile::Compact`, which
  collapses long dot leaders for token efficiency). Scanned/image-based PDFs return an
  OCR-routing notice instead of empty output.

Both tools funnel through `read_validated_pdf`, which resolves the path (working dir + `~`),
requires a regular file, caps input at 50 MiB, and rejects anything without the `%PDF-` magic
header *before* the parser sees it. The magic check tolerates a UTF-8 BOM and leading
whitespace exactly as `pdf-inspector`'s own validator does, and the size cap + magic check
run against the **same open file handle** as the (bounded) read, so a file swapped or grown
between check and read cannot slip past the gates (TOCTOU). Extracted markdown is treated as
**untrusted data end-to-end**: it is wrapped in an explicit `UNTRUSTED content extracted from
PDF…` delimiter (prompt-injection guard) whose closing line is appended *past* the shared
128 KiB `MAX_TOOL_OUTPUT_BYTES` budget, so a truncated extraction still closes its frame;
the framing literals are **redacted from extracted text**, so a hostile PDF that embeds
`--- end untrusted content ---` cannot close the frame early (frame-spoofing guard);
C0 control characters other than tab/newline/CR are escaped (terminal-escape guard).
Extracted markdown is additionally bounded by a 256 MiB **post-decompress budget**
(`MAX_PDF_DECOMPRESSED_BYTES`) — a decompression-bomb stopgap that refuses to ship a giant
string into the context and returns an actionable error instead (the hard `RLIMIT_AS`
backstop remains the sandbox phase). The hygiene passes (control-char escaping and frame
redaction) run only over the first `MAX_TOOL_OUTPUT_BYTES` of the extraction — the region
the output cap can ever show — so a just-under-budget, control-char-heavy string cannot be
amplified into multiple multi-hundred-MiB copies (a frame literal straddling the window
edge is only a partial match and cannot close the frame). Out-of-range `pages` requests are
rejected against the authoritative parsed page count (`result.page_count` — the *full*
document count regardless of the filter) *after* the same parse that produced the
markdown, so the pages path stays a single document parse; an entirely-out-of-range
request is still cheap because the parser skips markdown rendering for a filter that
matches nothing, and it is rejected before the scanned/OCR-routing branch can mislead the
agent. Input-gate rejections (non-regular file, size cap, missing `%PDF-` magic) are logged
via `tracing::warn!` with the control-char-sanitized path. `PdfError`
variants map to actionable one-line messages (e.g. encrypted → “pass a decrypted copy”).
Malformed-PDF panics from `lopdf` are contained by the request worker's `catch_unwind`
boundary (see the worker thread discussion above); OS-level sandboxing
(Landlock/seccomp/Seatbelt) for extension-process parsing is a planned follow-up.

### Postcard binary encoding

Tools communicate with the RISC-V sandbox via a `postcard`-encoded binary protocol:

- **Arguments:** Encoded as `postcard::to_allocvec(&args)` where `args: Self::Args`
- **Return value:** Encoded as `postcard::to_allocvec(&Result::<Result<R, E>, ToolError>::Ok(Ok(ret)))`
  — a nested postcard `Result` where:
  - `Ok(Ok(ret))` — tool succeeded, `ret: R` (serialized return value)
  - `Ok(Err(e))` — tool failed, `e: E` (structured, per-tool error type)
  - `Err(e)` — infrastructure failure, `e: ToolError`
  The outer layer captures infrastructure failures (arg deserialisation); the inner
  layer captures tool-defined errors.  This allows VM guests to pattern-match on
  specific error variants (e.g. `DbError::NotFound`, `HttpError::InvalidUrl`).
- **Tool call frame (VM → host):** `[tool_name: postcard String][args: postcard-encoded Args]`

### Available tools (up to 59 total, some dependent on installed binaries / the `blockchain` feature)

| Group | Tools |
|---|---|
| **Core** | `list_sessions`, `get_session`, `load_skill`, `set_session_title`, `set_working_dir`, `load_tools`, `unload_tools`, `read_file`, `read_file_range`, `write_file`, `edit_file`, `list_files`, `delete_files`, `line_count`, `random` (integers, floats, booleans, bytes, UUID v4 — with optional seed), `get_current_time` (Unix millisecond timestamp), `pdf_classify` (PDF type/confidence/OCR pages), `pdf_to_markdown` (PDF → Markdown, optional pages + compact), `retrieve_webpage` (render a URL in a local headless Chromium/Chrome — `http`/`https`/`file` — content / text / screenshot (PNG, inline or to `output_path`) / pdf (to `output_path`)) |
| **HTTP** | `http_request` (GET/POST/HEAD with headers, body, timeout) |
| **Image** | `display_image` (from path, URL, base64, or SVG text), `read_image` (read an image file from disk and feed it to a vision-capable model as image input) |
| **Git** | `git_status`, `git_diff`, `git_log`, `git_add`, `git_commit`, `git_push`, `git_show` |
> **`git_diff` output:** Always returns a line-by-line unified diff wrapped in a ````diff` fenced code block. The old `full` parameter (which previously toggled between summary-only and full diff modes) has been removed — the tool now always produces full diffs. The diff output for each file change is enclosed in ````diff` ... ```` fences for clear markdown formatting. Every diff fence the daemon emits (`append_fenced_diff` for git tools, `edit_file` in `format_edit_result`) routes through the shared `fence_content` helper in `tools/fs/mod.rs`, so a diff whose content carries a backtick run (e.g. a bare ``` context line while editing a Markdown file) cannot close the fence early in the TUI's markdown renderer; backtick-free diffs keep the canonical 3-backtick fence.

> **`git_show` output:** Commit, tag, and blob bodies are emitted verbatim inside a fenced code block (fence sized so content containing backticks cannot close it early, via the shared `fence_content` helper in `tools/fs/mod.rs`). Commit/tag messages are untrusted repo data, so they are never emitted as bare markdown — the TUI's markdown renderer would re-interpret headings/lists, mangle `--` with smart punctuation, and render a spoofed ```diff fence as a fake diff. The surrounding metadata (Author/Date/Tree/Head, etc.) renders normally; only the message/blob bodies are fenced.
| **Blockchain** | `evm_chain`, `evm_balance`, `evm_token_balance`, `evm_block`, `evm_transaction`, `evm_call`, `evm_gas`, `evm_logs`, `evm_nonce`, `evm_resolve`, `subxt_chain`, `subxt_balance`, `subxt_query`, `subxt_block` — **behind the `blockchain` cargo feature** (off by default; the tools live in the `choreo-blockchain` crate) |
| **File search** | `grep` (file content search), `find` (file name search) |
> **`find` output:** One match per line. Files render with a human-readable size (`blob.bin  4 KiB`), directories with a trailing `/`, symlinks as `name -> target`. Glob patterns containing `/` (e.g. `src/*.rs`) are matched natively by the walker against root-relative paths and prune traversal outside the pattern's literal prefix; bare patterns match file names (basename). A leading `./` is stripped and absolute patterns are converted to root-relative (erroring when outside the search root). `grep`'s `include` glob follows the same split — patterns with `/` match root-relative paths in directory mode (the file name for a directly-named file), bare patterns match basenames.
>
> **`grep` output:** Patterns are treated as regular expressions by default (`regex:false` switches to literal substring matching); `ignore_case:true` and `context: N` (surrounding lines, rendered `path-{line}-{content}` with `--` between non-contiguous groups) extend it. `output_mode` selects `content` (default), `files_with_matches` (one sorted path per hit file, rg `-l` semantics), or `count` (`path: N` matching lines per file, rg `-c` semantics). When `max_results` cuts the walk short, a `...[truncated at N results]` line is appended — note this signals *at least* N matches (the cap is hit before the walk proves nothing more exists); `grep` appends `...[truncated at N matches]` (`...[truncated at N files]` in the two non-content modes) the same way. The shared byte budget can stop collection before the cap (see below); the marker then reports the count actually collected, still an "at least N" figure. When `max_results` stops the walk mid-file, the capped match's trailing context lines are still delivered (rg `-m` + `-C` semantics); the searcher is capped at the same match limit (`SearcherBuilder::max_matches`), so it stops natively once the after-context window is exhausted instead of scanning the file's remaining tail — and with no context configured it stops at the cap line itself. If a line in the drain window exceeds the 64 KiB line cap (pathological input), it is delivered capped and then ends the drain: filling the rest of the window would otherwise force the searcher to scan the remainder of the file one giant line at a time. A directly-named single file in the two non-content modes never reports truncation — the result is provably complete once that one file is searched — while Content mode keeps the marker because the searcher may stop mid-file at the cap or byte budget. A search with no hits returns `No matches found.` rather than an empty string, so the model can distinguish "nothing matched" from a failed call; when the walk searched no file at all (an include glob filtered everything out, an empty directory), the regex-mode hint is suppressed the same way, because the empty result cannot be blamed on the pattern. A directly-named file that cannot be searched (e.g. permission denied) returns an error instead of a misleading no-match. Both tools escape control characters in file names and symlink targets so output stays one line per result; `grep` likewise escapes control characters in matched line *content* (ESC, backspace, … — tabs are kept literal) so a hostile file cannot inject terminal escape sequences. The escaping also covers Unicode line/paragraph separators (U+2028/U+2029, which terminals render as line breaks despite not being C0/C1 controls) and every Unicode *format* character (general category Cf) except the joiners U+200C/U+200D — the bidi marks/embeddings/overrides/isolates (U+061C, U+200E/U+200F, U+202A–U+202E, U+2066–U+206F), zero-width space (U+200B), word joiner and invisible operators (U+2060–U+2064), the BOM (U+FEFF), soft hyphen, the Mongolian vowel separator (U+180E), and the rarer format controls (tags, musical/phonetic, Egyptian hieroglyph, …) — all invisible and capable of reordering, hiding, or spoofing rendered text (only the joiners pass through; they are legitimate in Persian/Indic scripts and neither reorder nor hide). Matched and context lines are capped at 64 KiB with a `...[line truncated: exceeds 64 KiB]` marker (the same line cap and marker the file-read tools use), so a giant minified one-liner cannot balloon the result into memory; the aggregate buffered output is bounded to the same 128 KiB budget the renderer keeps — collection charges each item's exact rendered size (label + separators + line number + content + newline, precisely what `join` emits) and stops as soon as buffering more would exceed it, so a pathological tree of 64 KiB lines cannot balloon the result before rendering (if a single match's sanitized line alone exceeds the budget — e.g. a line dense with control characters, each escaping to ~6 bytes — the tool reports `...[truncated: matches exceed the 128 KiB output budget]` rather than a misleading no-match). Files whose head contains a NUL byte are treated as binary and skipped (ripgrep's default `BinaryDetection::quit`), so a binary blob cannot flood the result with garbage lines; output collected from a file before the NUL is discarded — a count-mode tally or a content-mode bucket of matches — so the file renders as skipped rather than leaking pre-NUL text. The one exception is `files_with_matches`: the searcher stops at the first hit (rg `-l` semantics) before it can observe a later NUL, so a file that matched before binary data is still listed — exactly what ripgrep does, which reports a file as soon as it matches.
| **RISC-V VM** | `run_riscv` (compile & run Rust code in a sandboxed RISC-V VM with access to all registered tools) |
| **Shell** | `exec` (direct program execution), `sh` (bash/dash/zsh — detected at startup), `nushell` (if `nu` is installed), `fish` (if `fish` is installed) |
| **X/Twitter** | `x_post`, `x_search_recent`, `x_user_lookup` |
| **DB** | `db_set`, `db_get`, `db_delete`, `db_delete_range`, `db_get_range`, `db_list`, `db_count` |
| **Sub-session** | `spawn_subsession` (spawns an autonomous child session with its own tool-calling loop) |

### Tool groups

Tools are organized into groups to reduce context overhead. Each tool declares its group
via `fn group() -> &'static str` on the `Tool` trait. Groups are:

| Group | Default | Description |
|---|---|---|
| `core` | always on | File system, HTTP, images, PDF classification/Markdown extraction, file search, random values, and time queries |
| `db` | off | Session-scoped key-value database |
| `git` | on | Local Git operations |
| `shell` | on | Shell and exec |
| `x` | off | X/Twitter API |
| `vm` | off | RISC-V sandboxed code execution |
| `content` | off | Choreographr Coordination Platform (publish/retract items, revisions, profiles, account pins; IPFS + indexer + Substrate) — only present when the `content` cargo feature is enabled (the tool group was previously named `coord`) |
| `blockchain` | off | EVM and Substrate/Polkadot blockchain queries (alloy/subxt) — only present when the `blockchain` cargo feature is enabled |
| `debug` | off | Read-only diagnostics and request dry-runs (`session_inspect`) — opt-in via `load_tools`, never on by default |

The system prompt lists all groups and their descriptions. The model uses `load_tools` to
activate additional groups and `unload_tools` to deactivate them. **core** cannot be unloaded.

Groups affect only tool **availability** in the API `tools` array — they are a discovery
mechanism, not access control. The RISC-V VM (`run_riscv`) always has access to all registered
tools regardless of group state.

Implementation details:
- `ToolRegistry::available_definitions(active)` returns definitions for all registry
  tools in the active set; every tool — including `load_tools`, `unload_tools`, and
  `set_working_dir` — is a proper `Tool` trait implementation registered in the
  default registry via `ToolRegistry::build()`
- The former meta-tools were converted from inline `&mut SessionState` handlers in
  `execute_tool_with_timeout()` to registry tools (see `tools/set_working_dir.rs`,
  `tools/load_tools.rs`, `tools/unload_tools.rs`).  They follow the
  `set_session_title` pattern: validate in the tool, then route the mutation
  through `DaemonCommand` → daemon → `SessionCommand` → the session's main loop,
  which applies it to the authoritative `SessionConfig` (broadcast + persist).
  This fixes a lost-update bug where the old inline handlers mutated the request
  worker's throwaway snapshot, which was discarded at request end
- `set_working_dir` supports tilde expansion in its `path` argument (inherited from
  `resolve_path`) and canonicalizes the target (rejecting non-existent paths and
  symlink escapes); `load_tools`/`unload_tools` carry a weak reference to the
  registry so their `groups` schema enum reflects the live group catalog
  (including dynamic MCP groups) at definition time
- `set_working_dir` performs a synchronous reply round-trip like
  `load_tools`/`unload_tools`: the daemon replies with an error immediately if
  the session is inactive, and the session main loop replies after applying the
  change — so a tool success means the authoritative state was actually updated
- `load_tools`/`unload_tools` validate their group names against the live
  registry catalog before sending (the schema enum is advisory): unknown groups
  are rejected with a clear error instead of being silently persisted into the
  session's active set.  The session handlers re-validate as defense-in-depth
  (see `unknown_group_names` / `ToolRegistry::known_group_names`)
- The three tools are restricted to `AllowedCaller::Direct` (model only) and are
  kept in the serial dispatch phase to preserve same-turn ordering of
  session-config mutations
- `list_sessions`, `get_session`, `load_skill`, and `spawn_subsession` are also
  proper `Tool` trait implementations registered in the default registry via
  `ToolRegistry::build()`, using `ToolContext.daemon_tx` to communicate with the
  daemon command loop
- `session_inspect` (group `debug`) is a **read-only** diagnostic (built with
  `Tool`): it opens the session record + turns via redb **read** transactions and
  dry-runs `build_chat_request_messages` + `warn_on_missing_reasoning_artifacts`
  with the manifest `model_reasoning_passback` policy, serializing each built
  `ChatRequestMessage` the way the adapter emits it — so its
  "would carry reasoning_content on the wire" count is exactly what the provider
  sees. It replays the reasoning-echo decision (ToolLoop/provenance/passback)
  per assistant turn to surface which turns are sent bare (the DeepSeek/Kimi
  `reasoning_content` must-be-passed-back 400 shape) and — via
  `include_reasoning_artifact`, the same helper the builder uses — flags wire
  EMPTY assistant messages (content-less, tool-less, no reasoning echo) as
  "must not be empty" 400 candidates, so a history that would fail at the
  upstream is visible before the request is sent. Privacy mirrors
  `turn_for_client`: artifact metadata + producer identity are shown for any
  session, but message-text previews and raw reasoning bytes are rendered only
  for the calling session, and raw reasoning additionally requires `include_raw`
  (thinking blocks / encrypted signatures never leave the daemon otherwise).
- Session state stores `active_tool_groups: HashSet<String>` (default: `{core, git, shell}`, plus `content` only when the `content` cargo feature is enabled; a persisted stale `coord` group name is silently ignored)
- `ToolGroup` struct and `GROUPS` constant live in `choreo-daemon/src/tools/mod.rs`
- Group metadata is appended to the system prompt in `context::build_base_prompt()`

### Concurrent tool dispatch

The tool-dispatch and execution machinery (channel wiring, the wait-loop,
streaming forwarder, timeout resolution, and per-tool result recording) lives in
`requests/tool_execution.rs`, and the system-prompt / tool-result-collection
helpers (`build_system_content`, `collect_tool_result`, `persist_loaded_skill`, …)
live in `requests/system_content.rs`; both are re-exported from `requests.rs`
via `pub(crate) use <mod>::*;` so every existing `crate::requests::X` reference
keeps resolving unchanged. `run_agent_loop` stays in `requests.rs`.

`run_agent_loop` in `requests.rs` partitions tool calls into two groups before execution:

- **Serial (session-config)** — `load_tools`, `unload_tools`, `set_working_dir`.
  These no longer require `&mut SessionState` (they route mutations to the
  session main loop via `DaemonCommand`), but they still execute serially so
  same-turn ordering of session-config mutations is preserved.
- **Worker-copy mirror (Phase 3)** — after every tool in the response has
  executed, `run_agent_loop` mirrors successful session-config mutations onto
  its own worker config copy so the next agent-loop iteration observes them
  (tool definitions, system content, working-dir-relative file ops).  The
  mutations are captured in Phase 1 as a typed `PendingConfigChange`
  (`LoadTools(Vec<String>)`, `UnloadTools(Vec<String>)`,
  `SetWorkingDir(Option<PathBuf>)`) and applied in call order in Phase 3:
  the shared `apply_load_tools`/`apply_unload_tools` for the group sets, and
  for `set_working_dir` the tool's **executed result** — the canonical path is
  carried on `ToolOutput.result_json` (populated by the blanket `ToolDyn` impl
  from the tool's typed return), so the mirror reproduces exactly what the
  main loop applied with no re-resolution and therefore no TOCTOU window.  A
  rarely-reachable fallback re-runs the shared `resolve_working_dir_path`
  helper (against the working directory in effect when the response was
  planned); if even that fails, the worker still invalidates its `discovered_skills`
  cache so a stale skill set can never leak across the request boundary.  The
  mirror is deferred until the end of the response because the model planned
  every tool call in the batch against the pre-change state (parallel
  semantics) — `set_working_dir` therefore takes effect on the next
  agent-loop turn, matching its advertised description.  The worker copy is
  discarded at request end, so it cannot drift from the main loop's
  authoritative state across requests.
- **Concurrent** — all remaining tools (shell, filesystem, VM, HTTP, Git,
  `spawn_subsession`, etc.) — tools whose execution is independent of session state.
  These are dispatched across multiple OS threads in parallel using `spawn_single_tool()`.

For concurrent tools, each call gets:
1. A dedicated **execution thread** that runs the tool via `ToolDyn::execute_streaming_json()`.
2. A **forwarding thread** that relays streaming output chunks to session subscribers in
   real time through the session command channel. It is fully event-driven — a
   `crossbeam_channel::select_biased!` on the streaming-output receiver (first arm) and
   the per-call kill receiver — so a kill signal is honored the instant it is sent and
   chunks already queued are still drained before the thread exits. The streaming-output
   channel between the execution thread and the forwarder is *bounded* (64 chunks), so a
   tool that out-produces the forwarder applies backpressure (blocks on `send`) instead of
   buffering an unbounded number of chunks in memory — the same bounded-channel design the
   SSE reader uses. It cannot deadlock: the forwarder drains continuously into the
   unbounded session command channel, and when it exits it drops the receiver, failing any
   blocked `send`.
3. A **wait-loop thread** that enforces the per-tool timeout (300s for shell tools, 60s for
   others, no limit for sub-sessions).
4. A dedicated **image channel** — the tool emits any produced image through this channel,
   which the wait-loop drains after execution completes.

**Thread count:** Because each concurrent tool spawns three threads (execution, forwarding,
wait-loop), dispatching N tools simultaneously creates up to 3N + 1 additional threads
(the +1 is the agent loop's main thread). The kernel scheduler handles these efficiently
for typical N (< 10), but callers should be aware of the resource footprint.

Tool results are always rendered in the model's original call order. When the model
returns a `ToolUse`, `run_agent_loop` seeds one placeholder `ToolResultRecord` per call
(empty content, in call order) into the turn and broadcasts it, so the transcript shows
every tool result slot in call order from the very start. Streaming chunks flow live via
per-tool forwarding threads (`ToolResultChunk`), and each tool's wait-loop thread delivers
its final `ToolHandle` through a shared batch channel the moment it finishes — both update
the matching placeholder **in place by `call_id`** (`update_tool_result`), then broadcast
`TurnAppended`. Because updates are in place, the rendered order never changes regardless
of completion order. Only the accumulator fed to the provider on the next call is
re-sorted back to call order (via `sort_by_call_order`) after the batch completes, so tool
messages mirror the assistant's `tool_calls` array. If a tool thread panics, the error is
caught and reported as a `ToolOutput` with `is_error: true` instead of crashing the daemon.
The `invocation_description` is generated before spawning (via
`ToolRegistry::describe_invocation`) and passed through `SpawnToolArgs`, so even timeout
and panic error paths carry a meaningful description in the `ToolOutput`. If the request
is cancelled before every tool's outcome was recorded, the unfilled placeholders are marked
`[cancelled — result not recorded]` (`SessionState::mark_unexecuted_tool_results`) before the
request stops, so the transcript shows what happened and the next provider request never
carries empty tool messages for calls whose outcome is unknown.

Cancellation during tool execution is fully event-driven: the concurrent collector and
`execute_tool_with_timeout` (serial phase) block on `crossbeam_channel::select_biased!`
between their result channels, the request's cancel channel, and (where a timeout applies)
an exact `after(remaining)` timer — there are no `recv_timeout` poll loops, and timeouts
fire precisely. Every wait that involves cancellation biases the cancel arm first, so a cancel already
queued when the wait begins is selected deterministically and a cancel that lands mid-block
is *more likely* to beat a simultaneously-ready result (bias for cancel — a preference,
not a guarantee, and both outcomes are handled correctly); when a cancel wins the race, an
already-completed result is still drained (non-blocking) rather than discarded, so
the tool's real output is recorded while the request still stops (sticky `cancelled` flag).
A cancel observed by the concurrent collector stops waiting for the slowest tool without
making the transcript nondeterministic: every still-running wait-loop receives a per-tool
kill (its forwarder stops streaming promptly and its `ToolContext.cancelled` flag is set
so the tool itself can stop early), pending handles are drained, and — because every
wait-loop selects on its kill channel — the collector keeps draining until all batch
handles have arrived. Each unfinished call therefore records a deterministic
`"tool '<name>' cancelled"` outcome instead of racing the placeholder sweep; the sweep
(`mark_unexecuted_tool_results`) remains as a safety net for wait-loop threads that die
before delivering (those are synthesized as panics) and for the serial phase. The drain
is bounded by thread scheduling, not by the slowest tool — its execution thread keeps
running in the background either way. The tool's *execution thread* cannot be
interrupted mid-call (Rust threads are not killable) and runs to completion in the
background, but every channel it would deliver through — exec result, streaming output,
image — has been dropped by the wait-loop's exit, so its late result is discarded and it
can no longer affect the transcript; external side effects (file writes, child processes)
still complete. Both the per-tool wait-loop threads and the
serial-phase wait drain the result channel before reporting a timeout, so a tool whose
result was already queued when the deadline fired is not reported as timed out (the
wait-loops bias their result arm ahead of the deadline timer; the serial wait drains once
the timer fires). The per-tool forwarding threads are event-driven too: they
`select_biased!` on the streaming-output channel and a dedicated kill channel (output arm
first, so the burst queued when a kill is observed is drained — bounded by the queue length
at that instant — before the kill is honored) and additionally re-check the
kill channel after every forwarded chunk, so a continuously-streaming tool cannot starve the
kill arm — a busy stream stops after one bounded final drain rather than streaming on
forever.
This removed the last poll loop from the tool execution path — the streaming channel itself
is a bounded crossbeam channel, so the forwarder blocks until a chunk or kill actually
arrives rather than waking on a 200 ms interval (and a tool that out-produces the
forwarder is throttled rather than buffered unboundedly). Both phases share a
`ToolContext.cancelled` flag with the running tool — the serial wait sets it when it
observes a cancel or the deadline expires, and the concurrent collector's per-tool kill
sets it on the wait-loop's behalf — so a tool that consults it can stop early. (This
lock-free flag is the one sanctioned shared-state exception to the repo's channel-only
thread-communication rule; see AGENTS.md.) The per-request cancel
channel is a crossbeam channel created in `sessions.rs`
(`ActiveRequest.cancel_tx`) and threaded through `run_agent_loop` → `ChatTurnRequest` →
retry/stream, so every wait (provider SSE, retry backoff, serial tool, concurrent
collector) can `select_biased!` on it directly (cancel arm first; `sleep_or_cancel` too).
The sender is held by `ActiveRequest` and dropped
only at `RequestFinished`, so a firing cancel arm always means a real cancel — never a
disconnect (the one deliberate exception is `sleep_or_cancel`, which proceeds on the
unreachable disconnect rather than aborting a retry loop). A cancel observed mid-batch
stops the request (sticky `cancelled` flag) after Phase 3 has mirrored the already-executed
config changes and the never-executed placeholders have been marked.

### spawn_subsession

`spawn_subsession` is a core-group `Tool` trait implementation registered in `ToolRegistry`.
It runs in the concurrent dispatch path alongside other tools. When invoked:

1. A child session is created via `DaemonCommand::CreateSession` with the parent as
   `parent_session_id` and inheriting the parent's working directory and tool groups.
2. The prompt argument is pushed as a `SystemText` message into the child session.
3. The child session runs its own `run_agent_loop()` (model → tools → model), subject to the daemon-wide `max_turns` cap.
4. The child's assistant text output is collected and returned to the parent as the tool result.
5. The child session persists in the database and is listable/attachable like any other session.

The daemon maintains a `children: HashMap<u64, Vec<u64>>` on `DaemonState` tracking the
parent→child relationship. This is used for **cancellation propagation** and **cascade
deletion**:

- **Cancellation:** When a client sends `Cancel`, the daemon routes it through
  `DaemonCommand::CancelRequest` rather than sending `SessionCommand::Cancel` directly to the
  session thread. After forwarding the cancel to the target session, the daemon also calls
  `cancel_children_of()` to propagate the cancel to all active child sessions, so they stop
  their work without polling.
- **Session exit:** When a parent session exits (sleeps), `handle_session_exited` calls
  `cancel_and_shutdown_child()` on each child to shut them down gracefully.
- **Session deletion:** `handle_delete_session` cascade-deletes children before the parent by
  calling `delete_session_inner()` on each child, logging but continuing if a child's DB
  delete fails. `delete_session_inner` never blocks the command loop: it removes the entry,
  records the id in `DaemonState::deleted_sessions` and writes a deletion tombstone
  (`deleted_sessions` DB table) **before** sending the thread `Cancel` + `Shutdown`, so a
  crash in the window after `Shutdown` but before the tombstone commits cannot leave a
  re-created record unmarked for the startup purge. The marker also means straggler
  `UpdateMetadata`/status messages from the still-shutting-down thread cannot re-insert the
  session into the in-memory index. The actual record delete is deferred to
  `handle_session_exited` — the thread's `persist_and_exit` runs *before* it sends
  `SessionExited`, so by the time the handler runs the record on disk is the thread's
  final state and can be removed without a re-create race. That delete runs on a
  **background thread** (`finalize_session_delete` — a pathologically large session, since
  `db::delete_session` walks every turn and kv entry, cannot block the command loop) and
  reports back via `DaemonCommand::SessionDeleteFinalized`; only a *successful* delete drops
  the `deleted_sessions` marker, on failure the marker and tombstone stay in place so the
  session cannot be attached or resurrected, and the startup purge
  (`db::purge_tombstoned_sessions`) retries. Two fast paths avoid the tombstone write and
  the deferred finalize entirely: deleting a session with **no live thread** (nothing can
  re-create the record) deletes immediately and sweeps any stale tombstone; deleting a
  session whose thread has **already terminated** (`JoinHandle::is_finished()` — its
  `persist_and_exit` ran and its `SessionExited` is queued) also deletes immediately, but
  *does* set the deleted marker — the thread's straggler messages are queued ahead of its
  `SessionExited`, so without the marker they would re-insert the session into the index,
  and the queued `SessionExited` then runs the standard finalize (an idempotent no-op
  delete, since the record is already gone) which clears the tombstone and drops the marker.
  The tombstone also covers the crash window: if the daemon dies while the thread is still
  shutting down, the next startup removes any record the zombie left behind.

The child session uses `ToolContext` (`active_tool_groups`, `reasoning_effort`, `working_dir`,
`daemon_tx`) to inherit parent config and communicate with the daemon command loop.


---

## Session architecture

### Data model

Sessions are persisted to a `redb` (v4) embedded key-value store at
`~/.local/share/choreographr/state.redb`. Seven tables:

| Table | Key | Value |
|---|---|---|
| `sessions` | `u64` session ID | MessagePack named(`SessionRecord`) |
| `session_turns` | `(u64, u32)` (session ID, turn ID) | zstd-compressed MessagePack named(`Turn`) — since schema 2 each value is a zstd frame around the MessagePack blob; turn text/tool-output/reasoning is the bulk of the DB and compresses 4–10×. Image/attachment bytes are **split out** of the blob into `session_attachments` (they are already incompressible PNG/JPEG) |
| `session_attachments` | `(u64, u32, String)` (session ID, turn ID, slot) | raw `Vec<u8>` — the general on-demand byte store for a turn: display + vision image bytes, persisted uncompressed and keyed by slot (`d{i}` for display index `i`, `r<call_id>` for a tool-result vision image), re-attached into the decoded turn by `read_turns`; written atomically with the turn blob in `write_turn` (which first clears the turn's stale slots so a rewrite with a shifted image layout never re-attaches old bytes to the wrong slot), removed by the session-wide delete helpers and both delete paths |
| `credentials` | `&str` service name | encrypted blob |
| `session_kv` | `(u64, String)` (session ID, key) | `Vec<u8>` |
| `deleted_sessions` | `u64` session ID | `()` tombstone — marks a deleted session whose still-shutting-down thread may re-create the record; written only when the delete is deferred (a live thread exists), cleared once the exit finalize re-deletes the record, purged at startup |
| `meta` | `&str` key (e.g. `schema_version`) | `u64` — persisted schema version (currently `2`) |
| `catalog_state` | `&str` key | `&[u8]` — runtime catalog-refresh state (S4): `last_attempt_ms` (Unix epoch millis, 8-byte LE — the 25 h cooldown anchor, written by the maintenance thread BEFORE every fetch) and `etag` (UTF-8 — the models.dev entity-tag, written by the daemon command loop after the cache bin is persisted). Created lazily on first write; purely additive, no schema bump |

`SessionRecord` fields: `title`, `selected_model`, `parent_session_id`, `working_dir`,
`turn_count`, `created_at`, `context_config`, `account_name`.

### Schema versioning & migrations

The `meta` table persists the schema version under the `schema_version` key
(`SCHEMA_VERSION`, currently `2`). A database file created by `open_db` (fresh
install, or a 0-byte interrupted-create corpse) is stamped immediately with
`INITIAL_SCHEMA_VERSION` (`1`) — the 0 → 1 transition is *initialization*
at creation, never a migration, so a fresh database is versioned from the
moment it exists. On every startup the daemon then runs `db::run_migrations`
right after `open_db` and before any session data is read; it is idempotent —
a database already at the current version exits immediately, and calling it
repeatedly is safe.

Schema 2 (the current version) is the first real migration: the
`session_turns` value codec changed from raw MessagePack to zstd-compressed
MessagePack. The 1 → 2 migration (`migrate_turn_values_to_zstd`) re-encodes
every existing turn row by wrapping its MessagePack bytes in a zstd frame
(compression is codec-orthogonal to serialization, so no deserialize/
re-serialize is needed); the stored rows are identified as already-compressed
by the zstd frame magic so the migration is safe to re-run after a crash.
The codec is implemented by `structured-zstd`, a pure-Rust library that emits
and reads standard zstd frames (numeric levels map onto C zstd numbering, so
`COMPRESSION_LEVEL=6` keeps its tuned meaning) and needs no libzstd C build.
Decompression on read is **bounded** to `MAX_TURN_DECODED_BYTES` (256 MiB per
row, far above any legitimate turn payload): `read_turns` stream-decodes
through a `Take` cap instead of trusting the frame header's declared content
size, so a corrupt/malicious row cannot pin the daemon's memory (a
"decompression bomb"). The decoder also requires the row to be exactly one
frame and consumed to EOF — trailing bytes or a second concatenated frame are
refused, not silently truncated. Reads also use a bounded `(session_id, …)` key-range
scan over only the target session's turns rather than decompressing the whole
table.

- **Versioning policy (additive vs breaking).** An additive change — a new
  struct field with `#[serde(default)]`, or a new enum variant appended — needs
  no migration and no version bump: named MessagePack tolerates it on decode.
  A breaking change — reordering/removing/mid-inserting a struct field,
  reordering or removing an enum variant, changing a type, key, or table
  (split/merge), or swapping the codec — requires a numbered
  `migrate_vX_to_vX+1` migration and a `SCHEMA_VERSION` bump. Future migrations
  that rewrite historical shapes must define frozen local copies of the old
  structs, and each migration ships with a fixture-based unit test (build a DB
  as the old version would have written it, run the runner, assert contents +
  version stamp + idempotency + backup artifact).
- **Migration chain.** `MIGRATIONS` holds one entry at release — version 1 → 2
  (the `session_turns` zstd codec change). Version 1 is the
  *initial* stamped version, reached by initialization at database creation
  (`open_db` stamps `INITIAL_SCHEMA_VERSION`), never by a migration. Each entry
  carries its source version explicitly (`from`, upgrading `from → from + 1`),
  so an entry's position in the array is irrelevant — the 0 → 1 transition is
  initialization, never a migration, so the first real migration is `from == 1`.
  Before applying anything, the runner validates that the entries' `from`
  values form the exact contiguous sequence `1..SCHEMA_VERSION`; a gap or
  misplaced entry is a hard error, never a silent stamp over data that was not
  migrated. Every migration must be idempotent under re-run (crash recovery
  re-applies from the last persisted version), transactional (one redb write
  transaction), and shipped with a fixture-based unit test.
- **Pre-release legacy data.** A database with no `meta` table reports version
  0. Since `open_db` stamps fresh files at creation, a database still reporting
  0 at startup is a *pre-existing* unversioned file: while the target is 1 it
  is initialized the same way (stamped to 1; nothing else happens), and any
  undecodable legacy blobs it holds are *not* migrated —
  `read_all_sessions` / `read_turns` skip undecodable entries with a warning,
  and single-record `read_session` treats an undecodable record as absent, so
  legacy sessions drop out loudly-but-non-fatally on first read. Once the chain
  grows past 1, a no-meta database is treated as pre-release leftovers and
  `run_migrations` refuses to start.
- **Backups.** A pre-migration snapshot (`state.redb` → `state.redb.bak-v{from}`,
  named after the version being migrated *from*, so a `bak-v2` file IS a v2
  database and restoring it rolls back to exactly the pre-migration state) is
  taken only *before a real migration writes* — never for the pure 0 → 1
  initialization stamp. The 1 → 2 zstd migration therefore writes a
  `state.redb.bak-v1` backup on the first startup after upgrade.
- **redb `UpgradeRequired` is a separate axis.** The redb file-format version
  (the library's on-disk format) is independent of the app's `schema_version`.
  If a newer redb wrote the file, `open_db` hard-errors with guidance to restore
  a backup (`state.redb.bak-v*`) or use the documented dump/restore path —
  it no longer silently recreates (and thereby destroys) a database it cannot
  open. A database whose `schema_version` is *newer* than the binary supports
  likewise errors at startup with "upgrade choreographr before continuing". A
  0-byte `state.redb` (the corpse of an interrupted create) is the one exception
  to the refuse-to-recreate rule — it holds no recoverable data, so `open_db`
  recreates it and stamps `INITIAL_SCHEMA_VERSION`, exactly like a fresh
  database.

### Session state (in-memory)

Each active session has a `SessionState` owned by its control thread. Persistent
configuration fields are extracted into `SessionConfig` to avoid duplication
across snapshot/restore, metadata conversion, and record persistence:

**`SessionConfig` (persisted):**

- `title: Option<String>` — display name
- `selected_model: Option<String>` — AI model for this session
- `reasoning_effort: Option<String>` — per-session reasoning effort slug (e.g. `"off"`, `"low"`, `"medium"`, `"high"`)
- `parent_session_id: Option<u64>` — parent session for sub-sessions
- `working_dir: Option<PathBuf>` — working directory for filesystem tools
- `created_at: i64` — Unix timestamp of creation
- `status: SessionStatus` — current status (Inactive, Inference, Retrying, Sleeping, …)
- `active_tool_groups: HashSet<String>` — tool groups active for this session
- `context_config: ContextConfig` — file discovery settings (context file names, max bytes)
- `account_name: Option<String>` — inference account assigned to this session
- `accumulated_usage: TokenUsage` — session-level token counter
- `context_window: Option<u32>` — model's context window size, resolved at model selection
- `last_prompt_tokens: Option<u32>` — `input_tokens` from the most recent API response;
  used for context-window progress displays (separate from the billing counter)
- `last_response_id: Option<String>` — the `response_id` of the most recent model call,
  persisted so ResponseId-policy providers (OpenAI/xAI Responses) can chain reasoning
  continuity across user turns via `previous_response_id` (restored at the top of each
  `run_agent_loop` invocation); every other policy keeps it `None`

**Runtime fields (not persisted directly):**

- `turns: BTreeMap<u32, Turn>` — conversation turns (persisted to DB separately)
- `next_turn_id: u32` — monotonically increasing counter for turn IDs
- `last_undo_turn_ids: Option<Vec<u32>>` — stores the turn IDs from the most recent undo, enabling `/redo` to restore exactly those turns; cleared when new user input is appended after an undo
- `subscribers: HashMap<u64, mpsc::Sender<DaemonMessage>>` — attached clients
- `active_requests: HashMap<u32, ActiveRequest>` — running request cancel flags
- `provider: Option<InferenceProvider>` — resolved inference provider for the account
- `loaded_skill_bodies: Vec<LoadedSkill>` — accumulated skill bodies from `load_skill` tool calls, injected into the system prompt on every turn
- `context_cache: Option<(u64, String)>` — cached context bundle fingerprint and assembled text, avoiding re-reading context files from disk when unchanged

### Hierarchy and working directory inheritance

Sessions form a tree: a session can have a `parent_session_id` pointing to another
session. When creating a child session, if no explicit `working_dir` is
provided, it inherits the parent's value. This allows sub-sessions (subagents)
to operate in the same directory as their parent.

### Persistence lifecycle

- **Startup**: `new_daemon_state()` reads all sessions and messages from the DB,
  reconstructing the in-memory `HashMap`. If the DB is empty, a default session #1
  is created.
- **Session creation**: Writes a `SessionRecord` to the DB immediately.
- **Message append**: Each `SessionMessage` (including `DisplayedImage` records for
  persisted images) is written to the DB alongside the in-memory push via
  `append_message_and_persist()`.
- **Shutdown**: The daemon sends `SessionCommand::Shutdown` to each active session, then joins each session thread bounded by `SESSION_SHUTDOWN_GRACE` (5s). The graceful path exits promptly once request workers drain; a worker stuck in an LLM provider read that a cancel cannot interrupt is abandoned rather than hanging the daemon — completed turns are already persisted as they finalize. Session joins happen concurrently (one join thread per session), so N stuck sessions cost ~one grace period, not N × grace. Deleted sessions are not joined here (their threads are reaped via the delete finalize on `SessionExited`); if the daemon exits before that finalize runs, the deletion tombstone ensures the next startup purges any record the zombie left behind.

### Multiple concurrent sessions

Multiple sessions can be active at the same time. Each session control thread stays
responsive while at most one request worker runs for that session. Request workers own a
snapshot of the session state and use cooperative cancellation via an `AtomicBool`.

### Undo/Redo

Sessions support undo/redo via an `undone` boolean flag on each `Turn`:

**Turn model:** Each `Turn` carries:
- `turn_id: u32` — monotonically increasing, assigned by `SessionState::start_turn()`.
- `undone: bool` — soft-delete flag; set to `true` on undo, back to `false` on redo.
- `user_text: Option<String>` — present for user-initiated turns, `None` for follow-up tool-loop turns.
- `assistant_text`, `assistant_reasoning`, `tool_calls`, `tool_results`, `displayed_images` — the assistant response.
- `reasoning_artifact: Option<ReasoningArtifact>` — the opaque reasoning round-trip payload captured by
  the provider adapter at parse time (see Provider Architecture); forwarded to the next request verbatim
  when the same model is still active and the passback policy asks for it. The daemon strips it from
  client-bound `DaemonMessage` payloads (clients receive `None`); only the daemon's request builder reads it.
- `reasoning_producer: Option<ReasoningProducer>` — the `{ provider_slug, model }` that produced the turn's
  artifact; the builder's same-model provenance check drops the artifact after a mid-session model switch.
  Also stripped from client-bound copies alongside the artifact.

**Undo flow (`/undo` → `ClientMessage::Undo` → `SessionCommand::Undo` → `handle_undo`):**
1. `SessionState::undo_turns()` finds the most recent non-undone turn with `user_text: Some(...)` via reverse scan.
2. Marks that turn and all higher-ID turns as `undone = true`.
3. Stores the undone turn IDs in `last_undo_turn_ids` for potential redo.
4. If a `last_response_id` is set, clears it (and its producer) and persists the session record — an undo invalidates the server-side response chain, which would otherwise leak the undone turns' context back into the model on the next chained request (the builder skips undone turns, but the chain does not). If an undo lands while a request worker is in flight, the worker's snapshot (taken from a child session that never saw the undo) cannot resurrect the cleared id: `handle_request_finished` compares undone-ness between the snapshot and live state and drops the stale id from the snapshot before applying it, and it refuses to overwrite the undone turns with the worker's pre-undo copies.
5. Persists each updated turn to the database.
6. Broadcasts `SessionEvent::TurnsUndone { turn_ids }` (on the `DaemonMessage::Session` envelope) to all subscribers.
7. The client removes the turns from its local history view.

**Redo flow** (`/redo` → `ClientMessage::Redo` → `SessionCommand::Redo` → `handle_redo`):
1. `SessionState::redo_turns()` restores the turn IDs stored in `last_undo_turn_ids` from the prior undo.
2. Sets `undone = false` on those turns.
3. Returns the restored turns as a `BTreeMap<u32, Turn>`.
4. Persists each restored turn.
5. Broadcasts `SessionEvent::TurnsRedone { turns }` (on the `DaemonMessage::Session` envelope) with full `Turn` objects so the client re-inserts them.

**Redo invalidation:** Starting a new turn with `user_text: Some(...)` after an undo clears
`last_undo_turn_ids`, making the redo unavailable — new user input starts a fresh editing session.

**Turn ordering on the client:** The `Started` and `ToolCallStarted` daemon messages
carry a `turn_id` that predicts the ID of the subsequent `Turn`. The client
uses `turn_id` to maintain a globally ordered history.

---


**Service config:** `~/.config/choreographr/config.toml`

```toml
max_turns = 0      # daemon-wide tool-loop budget; 0 = unlimited (default)

[context]
context_file_names = ["AGENTS.md", "CLAUDE.md"]
context_file_max_bytes = 32768
```

> **Note:** Provider-level settings (`base_url`, `streaming`, `retry_*`, timeouts, endpoint paths, request format) have moved to per-account overrides in `accounts.toml`. See `README.md` for the full list.

**Credential storage:** Credentials are encrypted per-credential in the `redb` database (`state.redb`). Identity keys reside in `~/.config/choreographr/identity.pk` (private), `~/.config/choreographr/public.pk` (public), and optionally `~/.config/choreographr/identity.pk.enc` (passphrase-encrypted private key).

**Database:** `~/.local/share/choreographr/state.redb` (override via `CHOREOGRAPHR_DB_PATH` env var)

**Socket path:** `/tmp/Choreographr.sock` (override via `CHOREOGRAPHR_SOCKET_PATH` env var)

**Tool loop limit:** `CHOREOGRAPHR_MAX_TURNS` env var overrides `config.toml` `max_turns`. Resolution
chain: `CHOREOGRAPHR_MAX_TURNS` env var → `config.toml` → default 0 (unlimited).
A value of `0` means *unlimited* — the agent loop runs until the model
produces a final answer, is cancelled, or hits an error. This is a daemon-wide
cap; individual sessions no longer carry their own `max_turns`.

**Logging:** `choreographr` uses `tracing` with `tracing-subscriber`. Default level is `info`.
CLI flags `-v` (debug), `-vv` (trace), or `-q` (warn) override the level.
`RUST_LOG` env var takes precedence over CLI flags.

**Session persistence:** On daemon start, sessions are loaded from the database into
`session_metadata` (in-memory). Model selection (`/models <name>`) updates both the
in-memory metadata and the database via `UpdateMetadata → db::write_session`. The
`AttachSession` handler also populates `session_metadata` when re-loading a session
from the database, ensuring `ListModels` and metadata queries see the correct
`selected_model`.

---

## Metrics / OpenMetrics monitoring

The daemon can expose a `/metrics` HTTP endpoint in the OpenMetrics format
(suitable for Prometheus scraping).

> **Feature gate.** The metrics machinery lives behind the `metrics` cargo
> feature, **off by default** at both the `choreo-daemon` crate and the root
> `choreographr` package — a plain build compiles it out entirely, with no
> `prometheus`/`tiny_http` dependencies and the module's public API degraded
> to inert no-op stubs, so the ~20 instrumentation call sites across the
> daemon compile unchanged. Opt in with `cargo build --features metrics`
> (release binaries enable it explicitly via `scripts/release.sh`). In a
> feature-off build the `--metrics-addr` flag is still accepted (so scripts
> that pass it get a clear, actionable error instead of clap's "unexpected
> argument") but the daemon refuses to start rather than silently ignoring the
> requested endpoint. The integration test (`tests/metrics_integration.rs`)
> is gated with `#![cfg(feature = "metrics")]`, and `cargo test-lean` — the
> feature-off unit run — is what keeps the no-op stubs and the `--metrics-addr`
> refusal path compiled: the `--all-features` test aliases never build that
> configuration.

**CLI flag:** `--metrics-addr <ADDR>` (e.g. `127.0.0.1:9464`). When the flag is
absent no metrics server is started — the daemon runs exactly as before.

**Endpoint:** `GET /metrics` returns `Content-Type: text/plain; version=0.0.4; charset=utf-8`.

### Exposed metrics

| Metric | Type | Labels | Description |
|---|---|---|---|
| `choreo_sessions_active` | Gauge | — | Number of active sessions |
| `choreo_connections_active` | Gauge | — | Number of active client connections |
| `choreo_requests_total` | Counter | `status` (`done`, `failed`, `cancelled`) | Total requests processed |
| `choreo_tool_executions_total` | Counter | `tool`, `status` (`ok`, `error`) | Tool call count |
| `choreo_api_calls_total` | Counter | `model`, `endpoint` | API call count |
| `choreo_api_errors_total` | Counter | `model`, `error_type` | API error breakdown |
| `choreo_connections_total` | Counter | — | Total connections accepted |
| `choreo_turns_total` | Counter | `model` | Agent loop turns |
| `choreo_request_duration_seconds` | Histogram | `status` | Request latency |
| `choreo_tool_execution_duration_seconds` | Histogram | `tool` | Per-tool execution time |
| `choreo_api_call_duration_seconds` | Histogram | `model`, `endpoint` | API round-trip time |

Process-level metrics (RSS, CPU, FD count) are also exposed via the `prometheus`
crate's `process` feature.

### Implementation

The metrics module (`src/metrics.rs`) keeps the real implementation in a
feature-gated `backend` module (selected by `#[cfg(feature = "metrics")]`,
with a no-op stub backend when disabled, re-exported behind the same public
API) using `std::sync::OnceLock` for a single static `Metrics` struct that
wraps Prometheus counters/gauges/histograms. All operations are atomic (no
`Arc<Mutex>` needed). A dedicated thread serves the `/metrics` endpoint via
`tiny_http`; it polls the shutdown flag every 1 second and exits cleanly when
the daemon shuts down.

### Instrumentation points

| Location | Function | Metrics recorded |
|---|---|---|
| `daemon.rs` — `CreateSession` handler | `record_session_created` | `choreo_sessions_active +1` |
| `daemon.rs` — `SessionExited` handler | `record_session_exited` | `choreo_sessions_active -1` |
| `server/connection.rs` — `client_thread` start | `record_client_connected` | `choreo_connections_active +1` |
| `server/connection.rs` — `client_thread` end | `record_client_disconnected` | `choreo_connections_active -1` |
| `server/lifecycle.rs` — accept loop | `record_connection_accepted` | `choreo_connections_total +1` |
| `sessions.rs` — `run_request_worker` | `record_request_total`, `record_request_duration` | request status + latency |
| `requests.rs` — `run_agent_loop` turn | `record_turn` | turn count per model |
| `requests/tool_execution.rs` — `execute_tool_with_timeout` | `record_tool_execution` | tool duration + status |
| `providers/shared.rs` — `timed_result` | `record_api_call`, `record_api_error` | API latency + errors (all providers) |

---

## Data flow: a prompt from input to response

```
1. User types "hello" in choreo-tui
        │
2. choreo-client-core::shell::parse_input_line("hello")
   → ClientMessage::RunInput { request_id: 1, input: "hello" }
        │
3. choreo-tui writer task serializes + frames → Unix socket → choreographr
        │
4. choreographr server.rs handles RunInput:
   - validates session exists and is attached
   - checks no duplicate active request_id
   - sends DaemonMessage::Session { session_id: Some(1), event: SessionEvent::Started { request_id: 1 } }
   - appends SessionMessageKind::UserText("hello") to session
   - calls requests.rs to execute
        │
5. requests.rs builds message array from session history
   → calls openai::chat_completions or openai::responses (based on request_format_for_model)
        │
6. openai::chat_completions / openai::responses streams SSE chunks
   → per chunk: DaemonMessage::Session { session_id: Some(1), event: SessionEvent::OutputChunk { request_id: 1, stream: true, data: "Hello" } }
        │
7. DaemonMessage is serialized + framed → socket → choreo-tui
        │
8. choreo-tui reader task receives OutputChunk
   → pushes to UI event stream
        │
9. UI loop consumes event → updates ClientHistory → re-renders
        │
10. Final chunk arrives → DaemonMessage::Session { session_id: Some(1), event: SessionEvent::Done { request_id: 1 } }
    choreo-tui marks request complete, adds session message
```

### Image flow (tool-triggered)

Images are delivered out-of-band via a one-shot `mpsc` channel rather than embedded in
`ToolOutput`:

```
Model calls display_image tool
  → daemon creates (image_tx, image_rx) channel
  → passes image_tx to ToolDyn::execute_json → execute_streaming_json
  → tool extracts PreparedImage from typed return → sends via image_tx
  → agent loop drains image_rx after tool completion
  → emit_and_persist_image creates SessionMessageKind::DisplayedImage
  → broadcasts it to live subscribers mid-turn via
    SessionCommand::Broadcast(SessionMessageAppended { DisplayedImage })
  → client push_session_message converts DisplayedImage → RenderedImage
  → also persists to DB + pushes to session messages for replay
  → after request completes, handle_request_finished skips DisplayedImage
    entries in its snapshot delta (already delivered mid-turn) and
    broadcasts only non-image messages (AssistantText, ToolResult, …)
```

### Vision input flow (`read_image` → model)

**Vision input** is the mirror-image pipeline: images are sent *to* the model rather than
*from* it. The `read_image` tool (`tools/read_image.rs`) and `crate::image_prep` decode
and normalize an image file: every raster format the `image` crate decodes (PNG, JPEG,
WebP, GIF, BMP, TIFF, TGA, DDS, ICO, PNM, HDR, OpenEXR, Farbfeld, QOI), **SVG**
(rasterized via `resvg`), **HEIC/HEIF** (via the pure-Rust `heif-oxide` decoder), and
**AVIF** — the last gated behind the `avif` feature (`image/avif-native`/dav1d, a C
library) so the default/release build stays C-free. Raster EXIF orientation is baked in
(`ImageReader::into_decoder` → `decoder.orientation` → `apply_orientation`) so
phone/camera photos reach the model upright; `heif-oxide` applies HEIC's own orientation.
The raster-decode and HEIC-decode paths (and the HEIC pre-decode allocation guard) live
in the shared [`choreo-image`](#choreo-image--shared-image-decode-helpers) leaf crate, so
the model path and the TUI display path use the same guarded decoder. The `display_image`
dimension probe (`tools/image.rs::inspect_image_dimensions`) also routes through the
shared guarded decoders for *every* source — raster (via `decode_raster_oriented`'s
`image::Limits` guard) and HEIC (via its pre-decode guard) — so a hostile image cannot
drive a huge allocation during the probe, not just during the display/model decode.
All sources are resized to ≤2000px, and re-encoded to PNG (alpha) or JPEG (opaque) under
a decompression-bomb guard. The tool reports a text handle (path, dimensions, MIME,
bytes), and returns an `ImageReference` that carries the **normalized bytes**
(`ImageReference::data`)
via the `Tool::extract_image_ref` hook. The framework moves that reference onto the
durable `ToolResultRecord.image` field, and the bytes are persisted in the raw
`session_attachments` table (split out of the zstd turn blob) so the model always has the
image on later turns — the source file is never re-read, so it can vanish without breaking
the session. At request-build time (`build_chat_request_messages` in `reasoning.rs`), each
image-bearing tool result attaches its stored bytes directly to a **synthetic user
message** appended *after* all of the turn's tool messages (preserving
`tool_use → tool_result` adjacency), and the provider adapters serialize it per protocol:

```
read_image tool → image_prep::load_and_normalize → ImageReference(data) → ToolResultRecord.image
  → persisted: bytes → session_attachments; blob carries byte-less turn
  → build_chat_request_messages: model_supports_vision gate
      vision model  → attach ImageReference.data → ChatRequestMessage.images (synthetic user msg)
      text-only     → placeholder text message (never pixels) — the vision gate
  → provider serializer: OpenAI chat image_url / Responses input_image /
                         Anthropic image / Google inline_data
```

The bytes never reach clients: `turn_for_client` strips `ToolResultRecord.image` from the
client-facing turn (display images, which clients render, travel separately via
`displayed_images` and persist in the same `session_attachments` table).

Replay across turns is byte-identical (the same normalized bytes from `session_attachments`
every request), which keeps provider prompt/image caches hitting.

The **vision gate** uses `catalog::model_supports_vision(provider_slug, model)` (from the
models.dev `modalities.input` flag, overridable via the overlay). On a text-only model the
image bytes are never sent — a placeholder text message names the source path so the model
can re-read it with a text tool. Images contribute a fixed `IMAGE_TOKEN_ESTIMATE` (1000)
per image to the prompt-token estimate for context-window accounting.


### Session switch flow (updated)

Because the TUI subscribes to *all* session activity (`SubscribeAllActivity`,
sent once at startup), every session's streaming events (`Started`,
`OutputChunk`, `TurnAppended`, `ToolCallStarted`, `ToolResultChunk`, `Done`,
…) are routed into per-session `SessionDisplayState` entries in
`session_displays` keyed by `session_id` — even for sessions the user is not
currently viewing.  Switching sessions therefore does **not** discard that
accumulated state:

```
User presses Enter on a session in the session manager
  → reset_for_session_switch(session_id)
      • preserves live state: view.turns, view.request_to_turn, active
        request set, live token estimates, reasoning overrides
      • resets only transient render state (scroll, markers, height caches)
        which is rebuilt on the next layout pass (markers_dirty)
  → AttachSession sent to daemon
  → daemon responds with SessionState { turns, … }
  → handle_session_state MERGES the snapshot with the accumulated turns:
      • finished turns come from the snapshot (daemon-canonical)
      • the in-flight turn keeps the accumulated version — the snapshot only
        holds the empty placeholder from start_turn, while the accumulated
        turn has the live streamed content (see turn_has_live_content)
  → rendered_images re-synced from the merged turn set
```

This makes switching into a streaming session seamless: the user "jumps in"
to the live content accumulated so far instead of seeing a blank turn until
the next chunk arrives.  (Cold-starting clients that were never subscribed
to the session still miss pre-attach content — the worker owns the live turn
and only syncs back on `RequestFinished`.)

The all-activity subscription is **sticky**: the daemon's activity broadcast
(`handle_broadcast_activity`) never drops a message for a subscriber —
only a disconnected receiver is removed.  This matters because the TUI
registers for all activity exactly once at startup and never re-subscribes:
evicting it would permanently blind it to every background session, so
switching into a streaming session would show a blank turn until the next
chunk arrived over the (just attached) per-session path instead of the
accumulated content.

All three subscriber fan-outs — the all-activity broadcast
(`handle_broadcast_activity`), the summary broadcast (`DaemonState::broadcast`),
and the per-session `broadcast()` in sessions.rs — share ONE lossless
policy via `crate::broadcast::SubscriberSink`.  Each subscriber's writer
channel is UNBOUNDED, so an enqueue can never be `Full`: the daemon never
drops a broadcast message, and a slow subscriber can never stall the
daemon's single-threaded command loop or a session thread (unbounded
`send` never blocks).  Delivery is guaranteed, in-order (FIFO), and
exactly-once for every connected non-evicted client.

Memory is bounded by LAG-BASED EVICTION instead of drops.  Each
`SubscriberSink` carries an in-flight byte counter (an `Arc<AtomicUsize>`
shared between the producers that increment it on enqueue and the
connection's writer thread that decrements it on dequeue — sanctioned
exception #6, see AGENTS.md); `SubscriberSink::enqueue` reports
`EnqueueOutcome::{Delivered, Disconnected, ClientOverLag, GlobalOverBudget}`
based on [`LagLimits`] (`per_client_cap` 64 MiB, daemon-wide `global_budget`
512 MiB — injectable in tests).  The crossing message is STILL enqueued
(lossless); the outcome only tells the caller to evict the lagging client
(`DaemonCommand::EvictClient`) or the largest-backlog client
(`EvictLargestLagging`).  `handle_evict_client` removes the client from
every subscriber map, tells its sessions to drop it (`RemoveSubscriber`),
and enqueues a best-effort `Evicted` advisory before dropping the sink.

The thresholds are SOFT bounds: a race can overshoot the cap by at most
one message's bytes before the eviction command lands — an exact hard
cutoff would require a blocking or dropping send, which is exactly what
this design eliminates.

The byte counters stay BALANCED on every path, to within one bounded race.
The writer thread decrements on each dequeue; when it stops early (send
error, or the `Evicted`/`ShuttingDown` stop) it drains whatever is still
queued and decrements that too, so an abandoned backlog can never stay
frozen in the daemon-wide total and permanently exhaust the global budget.
Every enqueue path (`enqueue`, `send_unchecked`, the connection's
`send_to_writer` — all through the shared `send_accounted` core)
self-corrects both counters when the send fails on a dead receiver (writer
thread already gone), and the `Evicted`/`ShuttingDown` advisories are
accounted like any other message. The one residual race is a straggler
enqueued in the microsecond window between the writer's exit drain and its
receiver being dropped: that `send` SUCCEEDS (the receiver is still alive),
the message is never dequeued, and its bytes stay in the daemon-wide
counter forever. The leak is bounded to at most one message's bytes per
teardown event (a producer that sends after the receiver is gone
self-corrects), so it is accepted and documented as the invariant's one
bounded exception.

Eviction needs no daemon-held socket handle: each connection's writer gets
a 5 s socket write timeout (`WRITER_WRITE_TIMEOUT`), so a wedged client
(zero receive window) cannot stall its writer forever — the write fails,
the writer shuts the socket down itself (notify-before-EOF on the graceful
path), and the reader's blocking read unblocks into the normal
`cleanup_client` teardown.  See the `server/connection.rs` row.

Evictions are not lost silently: every one increments the
`choreo_evictions_total` Prometheus counter served on `/metrics`, so a
permanently wedged subscriber — one whose backlog keeps crossing the cap —
remains observable.  The old `choreo_broadcast_dropped_total` counter (and
the drop-on-full policy it measured) is gone: the daemon no longer drops
broadcast messages.

Token bookkeeping follows the same per-session rule.  `LiveOutputTokenCount`
(during streaming) and `SessionState` snapshots (attach / `load_tools` /
`unload_tools` broadcasts) are routed to the display of the session they
belong to, never to the one the user happens to be viewing.  The status
bar's `↑/↓` token readout and context-fill come from the active session's
`SessionDisplayState` (`display_token_usage`), so a background session
streaming via the all-activity subscription cannot bleed its counts into
the session on screen — and its counts are already correct by the time the
user switches to it (`reset_for_session_switch` preserves live estimates).

The same rule extends to every other per-session status message the daemon
broadcasts over the all-activity subscription.  `ModelSelected`,
`ReasoningEffortSet`, `ReasoningEffortSetFailed`, `SessionAccountSet` and
`Failed` are routed to the display of the session they belong to (only
touching the status bar's identity fields when that session is the attached
one); `ModelSelectionFailed` — whose failure means there is no new model to
record — is gated the same way but updates no display.  The routing and the
gate are one operation: the shared `route_session_update` helper resolves
the reported id, applies the display update, and returns a
`SessionUpdateRouting` verdict (`FallThrough` for the attached session /
connection-level `None`, `Suppress` for background noise).  For a non-attached session the
`connection.rs` handler returns early so the global status/error line is not
rewritten either — a background session changing its model, reasoning effort
or account must not rewrite the fields of the session on screen, nor reflow
its viewport via a status-height change.  Request failures additionally
get recorded on the turn itself (`Turn.error`): the agent loop marks the
open turn and finalizes + broadcasts it before propagating the inference
error, so both clients render a red "Error:" block in the transcript and the
failure survives a daemon restart.  Because that transcript block is the
persistent home for a request-level failure, `handle_failed` deliberately
writes the global error line only for *connection-level* failures — the
daemon's `session_id: None` connection-level replies ("no session attached") — which have no
turn to render in; a request failure's block is never duplicated on the
status bar.

Two daemon conventions keep this gating correct:

- The daemon replies to `GetReasoningEffort` (bare `/reasoning`) with the
  attached session's real id, and sends some connection-level errors with
  `session_id: None` (no session exists).  `App::resolve_daemon_session` maps
  `None` to the attached id (when one exists) so the update lands in the right
  display (never a phantom session entry), and
  `App::is_background_session_message` — the single gate used
  by every arm above — treats `None`, like the attached session itself,
  as the user's own feedback rather than background noise.  The two are
  composed in the `route_session_update` helper so a message can never be
  resolved without also being gated (`ReasoningEffortSetFailed` runs its
  display reset through the same helper).  Without this, the
  background gating would swallow the confirmation of the user's own
  `/reasoning` and `/model` commands.

Two session-switch details keep the status bar honest after switching into
a streaming session:

- `handle_session_attached` fills *missing* display fields from the (possibly
  stale) session summary but never clobbers values already accumulated via
the all-activity subscription, so fresher per-turn token usage and live
counts survive the attach instead of regressing.
- The startup auto-attach in `handle_sessions` prefers the most recently
  modified *top-level* session (skipping agent-spawned sub-sessions, whose
  `last_modified` is bumped each time one of their requests completes and
  would otherwise hijack the view) and sets attachment state immediately so a
  second `Sessions` reply cannot re-fire the attach to a different session.
- `SessionCreated` for a sub-session (`parent_session_id = Some`)
  — e.g. from `spawn_subsession` — is likewise treated as background noise:
  neither `reset_for_session_switch` nor `AttachSession` runs, so a spawned
  sub-session cannot hijack the Chat view away from the session the user is
  reading.  The session list is refreshed only while the user is on the
  Session Manager page — an unsolicited `ListSessions` from the Chat page
  would make the daemon reply with `Sessions`, whose handler writes the
  global status line and reflows the viewed viewport.  User-created sessions
  (`parent_session_id = None`) keep the auto-attach behavior.
- The mirror-image rule: when the user *is* reading a sub-session on the Chat
  page and it finishes (its status transitions from active — inference / tool
  call / retrying — to idle), the TUI switches back to the parent session and
  shows `Subsession "…" finished. Switched back to parent "…".` on the
  status line.  Detection runs in the `SessionStatusChanged` dispatch *before*
  the status is applied (`App::attached_subsession_finished`), so the
  pre-transition active status is what distinguishes "just finished" from a
  duplicate idle→idle broadcast (summary refresh, re-attach of an already
  finished child), which never re-fires.  The switch (`App::switch_back_to_parent`)
  delegates to the shared `App::attach_to_session` sequence used by every
  attach path (Session Manager list/detail Enter included) —
  `UnsubscribeSessionsSummary` then `AttachSession`, sent before the local
  state is mutated — so a broken pipe leaves the view untouched.  The check
  also requires the parent to still exist in the summary list: a child whose
  parent was deleted while it ran stays put instead of attaching to a dead
  session id.


---

## Design decisions

1. **Unix sockets, not HTTP for client↔daemon** — keeps everything local, avoids port conflicts,
   leverages OS-level access control.

2. **Binary protocol (MessagePack, named mode), not JSON** — self-describing, compact, typed,
   versioned. Length-prefixed framing avoids parsing ambiguities. Version field allows protocol
   evolution.

3. **Lock/Unlock security** — the daemon starts without credentials in memory. The private key is
   sent over the Unix socket and zeroized after use. Credentials are encrypted per-credential so
   they can be stored in the database without a global passphrase.

4. **Sessions, not per-client state** — sessions are independent from client connections. A
    session has its own model, working directory, and messages. Clients subscribe/unsubscribe from sessions
   via the broadcast system. Sessions persist in a redb database and survive daemon restarts.
   Session lifecycle operations (create, attach, list, delete) succeed even when the daemon
   is locked — credentials are only needed to run models. Sessions carry an optional
   `account_name` field that determines which provider credential to use at request time;
   there is no global "default account" fallback. The provider is resolved lazily from the
   daemon's provider registry when the first RunInput is issued.

5. **Session hierarchy** — sessions can have parent sessions (`parent_session_id`), forming a
    tree. Child sessions inherit their parent's working directory unless explicitly overridden. The
   `spawn_subsession` tool creates autonomous child sessions that run their own tool-calling
   loop and report results back to the parent.

6. **Tool-call loop in the daemon** — the daemon drives multi-turn tool interactions (up to a
   configurable daemon-wide `max_turns` cap, default 0 (unlimited) rather than pushing that complexity to
   the client or model. The client just sees `ToolCallStarted`/`ToolCallFinished` events.

7. **Session subscription model** — multiple clients can subscribe to the same session. Events
   are broadcast to all subscribers except the originator, enabling shared session viewing.

8. **SSE streaming** — a custom `SseReader` (not a library) handles `data:` lines and `[DONE]`
   for OpenAI SSE, giving full control over parsing and buffering behavior. The Anthropic
   module has its own `AnthropicSseReader` that handles both `event:` and `data:` lines
   (required by the Anthropic Messages streaming format) and yields `(event_type, data)` pairs.
   The blocking socket read is decoupled onto a dedicated reader thread (`stream.rs`) that
   forwards parsed events through a bounded crossbeam channel, so the caller can `select!`
   on the event channel, the cancellation channel, and the deadline timer simultaneously —
   cancellation and deadline expiry are observed the moment they happen, with no polling; an
   abort signal stops the reader thread at its next loop boundary once the consumer cancels
   or drops the stream. A wall-clock deadline (`total_timeout_secs`) backstops each request
   attempt: the deadline is armed by `retry::AttemptDeadline` *before* the request is sent
   and re-armed at the start of every retry, so a single attempt's budget spans DNS →
   connect → headers → body (ureq's `timeout_global` bounds the attempt from DNS through
   the first body byte, and the SSE consumer enforces the same deadline with an exact
   timer — the real hard cap, since ureq floors its per-read timeout at ~1 s so sub-second
   keep-alive trickles could otherwise outlive the deadline). Expiry surfaces as a dedicated
   `ProviderError::DeadlineExceeded` — non-retryable and distinct from a socket `Io` error.
   Each retry restarts the deadline, so retries plus their backoff can exceed the configured
   value in aggregate.

9. **Markdown as the intermediate format** — all text (tool output, assistant text, error
    messages) is treated as markdown and rendered as HTML (desktop) or shaped to terminal output
    (choreo-tui), providing a consistent rendering layer.

10. **Flexible API format** — both OpenAI Chat Completions and Responses are first-class
    citizens, selectable per-model via a `RequestFormat` enum (`ChatCompletions` / `Responses`).
    The dispatch mechanism lives in `ServiceConfig::request_format_for_model()`: it checks
    the provider catalog's per-model `openai_responses` flag and falls back to `default_request_format`.
    Every entry point (`completion`, `completion_stream`, `chat_completion_turn`,
    `chat_completion_turn_streaming`) matches on the resolved format and calls the appropriate
    request builder. Input/output mapping differs between the two: system messages go into the
    `input` array as `{role: "system"}` items (rather than the `instructions` field), tool results
    become `function_call_output` items, and the `input` is an array of typed items rather than a
    flat messages list. Multi-turn chaining uses `previous_response_id` to link turns together,
    while Chat Completions relies on the full message history.

    **Programmatic tool calling (Responses API, gpt-5.6+):** When enabled, the Responses
    request body includes a `programmatic_tool_calling` tool with `type: "programmatic_tool_calling"`.
    The model responds with `response.program.code.delta` and `response.program.code.done` events
    carrying generated JavaScript, plus `response.program_output.done` with execution results.
    The daemon's `Tool` trait exposes `output_schema()` (JSON Schema describing each tool's
    return value) and `allowed_callers()` (whether a tool is callable directly by the model,
    programmatically, or both). These are plumbed through `ChatToolDefinition::function_with_options()`
    and `ResponsesTool` into the wire format. Per-model auto-enablement is controlled by
    `ServiceConfig::programmatic_tool_calling_for_model()`; account-level override via
    `accounts.toml`'s `programmatic_tool_calling` field.

11. **Pluggable providers via `InferenceProvider`** — the provider system supports OpenAI-compatible,
    Anthropic Messages, Google Gemini, and Mistral APIs. Each provider implements the same interface
    (`chat_completion_turn`, `chat_completion_turn_streaming`, `list_models`) and is constructed
    from an `AccountConfig` + credential. Accounts are defined in `accounts.toml` with a
    `provider` field (`"openai"`, `"opencode"`, `"anthropic"`, etc.). The TUI new-account form
    offers all registered provider options via a shared `PROVIDER_OPTIONS` array. The client
    implementations, trait, shared types, and catalog live in the `choreo-ai-protocols` crate;
    the daemon's `InferenceProvider` is the thin dispatch/metrics facade.

12. **OS threads with sidecar async runtime** — the daemon avoids async Rust everywhere except
    where third-party libraries (alloy, subxt) require it. Both live in the `choreo-blockchain`
    and `choreo-content` crates, which hold a global `OnceLock<tokio::runtime::Runtime>` as a
    sidecar and run their clients via `block_on()`. The daemon links those crates only behind
    the `blockchain` and `content` cargo features (both off by default) and calls their
    synchronous `execute_*` entry points, so tokio is
    never a direct dependency of the daemon. Every call is additionally bounded by a 30s
    wall-clock `RPC_TIMEOUT` inside the crate: the daemon's own ~60s tool timeout can only
    *abandon* the blocked execution thread (a synchronous `block_on` cannot be interrupted),
    so the crate-level cap is what turns a black-holed RPC endpoint into a clean error
    instead of a leaked thread. The tools accept an arbitrary `rpc_url`/`ws_url` from the
    model and open HTTP(S)/WebSocket connections to it — the same trust surface as the
    `http_request` tool, not a new capability — which is why the whole feature (and the
    network reach it adds) is off by default. Node-supplied strings (chain names, ENS
    records, decoded storage/block JSON) are run through a sanitizer that escapes control
    chars, line/paragraph separators, and Unicode format chars (the same Cf-set policy the
    daemon's line-oriented tools use) before they enter the tool transcript. This simplifies the mental model (each thread owns
    its data, no `Send` bounds on shared state, no `Pin<Box<dyn Future>>`), improves stack
    traces, and avoids the complexity of async cancellation.

13. **Reasoning round-trip as an opaque artifact** — reasoning is not only display text: for
    Anthropic (thinking blocks + signatures), DeepSeek/Kimi (`reasoning_content`), Gemini
    (thought signatures), and OpenAI/xAI Responses (opaque reasoning items + `previous_response_id`)
    it must be sent back on the next request or the tool-call loop fails with a 400. The adapter
    captures the payload verbatim at the parse boundary into an opaque `ReasoningArtifact`; the
    daemon stores it on the `Turn` (and persists it), but strips it from client-bound `DaemonMessage`
    payloads — only the daemon consumes it; the adapter re-emits it verbatim in
    its own wire format. *Whether* to send is derived, never configured: same-model provenance
    (`Turn.reasoning_producer` vs current provider+model, so a mid-session model switch drops every
    old artifact) plus the catalog's `reasoning_passback` policy (`None` / `ToolLoop` / `AllTurns` /
    `Signature` / `ResponseId`, per-model override else protocol default). Display text stays in
    `Turn.assistant_reasoning`; the artifact bytes are never interpreted by the daemon.




---

## Context file discovery

The daemon automatically discovers and injects project-specific context files
(`AGENTS.md`, `CLAUDE.md`) and skills at session creation, and refreshes them
before every model call (every turn of the tool-call loop).

### System prompt construction

Each turn in the agent loop calls `build_system_content()` which constructs the
system prompt from four sources:

1. **Base prompt** — identity, tool group listing, available skill metadata, and
   any loaded skill bodies (accumulated via `load_skill` calls).
2. **Project context files** (`AGENTS.md`, `CLAUDE.md`, etc.) — discovered by
   `discover_context()` and assembled by `assemble_context()`. Results are cached
   on `SessionState::context_cache` (fingerprint + assembled text) and reused
   when the fingerprint is unchanged.
3. **Subdirectory hints** — hints accumulated from filesystem tool calls in the
   previous turn, appended under "## New context from project subdirectories".
4. **Loaded skills** — `<skill name="...">...</skill>` blocks injected after the
   "Available skills" listing.

The system prompt is rebuilt every turn so that newly loaded skills and newly
discovered subdirectory hints are visible to the model immediately.

### Discovery algorithm

1. **Global files** (loaded first, prepended):
   - `~/.config/choreographr/AGENTS.md`
   - `~/.claude/CLAUDE.md` (unless `disable_claude_code_prompt` is set)
   - `~/.agents/AGENTS.md`
2. **Project files** (walking from session working directory up to the git repository root):
   - At each ancestor directory, checks `AGENTS.md` first, then `CLAUDE.md`.
   - Only one file per directory (first match in the configured `context_file_names` list).
   - Collected bottom-up (outermost first), then rendered in reverse order so
     closer-to-working-directory instructions appear last.

### Subdirectory hints

When filesystem tools (`read_file`, `list_files`, `grep`, `find`, etc.) access a file
in a subdirectory below the session working directory, the daemon walks up from that file's
parent toward the working directory and checks for `AGENTS.md`/`CLAUDE.md` files not already in
the main context. Any found hint content is appended to the tool result message
(not the system prompt), preserving prompt cache stability.

### Skills (Agent Skills standard)

Skills are discovered from:
- `~/.agents/skills/<name>/SKILL.md` (global)
- `.agents/skills/<name>/SKILL.md` (project, relative to session working directory)

Each `SKILL.md` must have YAML frontmatter with `name` and `description`.

**Progressive disclosure:** At session start, only metadata (name + description)
is included in the stable prompt (`messages[0]`). When the model calls the
`load_skill` tool with a skill name, the full `SKILL.md` body is loaded and
injected as a new `SystemText` message.

### Fingerprint-based refresh

Before each turn in the tool-call loop, the daemon computes a fingerprint via
`compute_fingerprint()` of all known context file paths and their mtimes. If the
fingerprint matches the cached value on `SessionState::context_cache`, the assembled
context string is reused without re-reading files from disk. If it differs (file
added, removed, or modified), the context is rebuilt and the cache is updated.

### Configuration

```toml
# ~/.config/choreographr/config.toml
[context]
context_file_names = ["AGENTS.md", "CLAUDE.md"]   # ordered list; first match per directory
context_file_max_bytes = 32768                     # max combined context size
disable_claude_code_prompt = false                 # skip ~/.claude/CLAUDE.md
```

### User system prompt override

The stable base prompt (`messages[0]`) is loaded from
`~/.config/choreographr/system.md` if it exists. Otherwise, a built-in default is
used. The default lives at `choreo-daemon/system.md` in the repository and is
embedded at compile time via `include_str!`.

### Module

Implementation lives in `choreo-daemon/src/context.rs`. Key entry points:

| Function | Purpose |
|---|---|
| `discover_context(working_dir, config)` | Walk filesystem, return `ContextBundle` with all discovered files |
| `discover_skills(working_dir)` | Scan Agent Skills directories, return `Vec<SkillMeta>` |
| `assemble_context(bundle)` | Render discovered files into an XML-like format for injection |
| `build_base_prompt(skills, groups, loaded_skills)` | Build the stable system prompt (identity + tool groups + skill metadata + loaded skill bodies) |
| `recheck_context(working_dir, config, old_fp)` | Re-discover and compare fingerprints |
| `subdirectory_hints(tool_name, args, working_dir, known)` | Return `Option<(String, Vec<PathBuf>)>` — subdirectory hint text and newly discovered paths |
| `load_skill_body(name, working_dir)` | Load the full body of a SKILL.md, stripping YAML frontmatter |

### Tool: `load_skill`

Registered alongside other tools in the tool loop (core group). When the model calls
`load_skill(name)`, the daemon:

1. Finds the matching `SKILL.md` from `~/.agents/skills/` or `.agents/skills/`
2. Strips the YAML frontmatter
3. Returns `"Loaded skill: <name>"` as the tool result

**Persistence:** After the tool result is collected, `run_agent_loop` detects the
`load_skill` call and pushes a `LoadedSkill { name, body }` into
`SessionState::loaded_skill_bodies`. On every subsequent turn, the
`build_system_content` helper includes all loaded skill bodies in the system prompt,
wrapped in `<skill name="...">` XML tags. This ensures skill instructions remain
visible to the model even as tool results scroll out of the context window.

### `run_riscv` — RISC-V sandboxed code execution

`run_riscv` is a tool that compiles Rust source code into a RISC-V ELF binary and executes it
inside a sandboxed virtual machine powered by `ckb-vm`. It is registered as a manual
`impl Tool` (not via `define_tool!`) to pass `x_credentials` and `working_dir` through
to the guest syscall handler.

**Execution flow:**

1. Accepts Rust `source`, pre-compiled base64 `program`, or `program_path` pointing at a
   pre-compiled ELF file on disk (read with a 4MB size cap — the same
   `ckb_vm::RISCV_MAX_MEMORY` bound as the VM's flat memory, see step 3).
2. If `source` is provided, it is first formatted via `rustfmt` (silently skipped
   if `rustfmt` is unavailable).  The formatted source is then prepended with a
    `#![no_std]` boilerplate (panic handler, entry point, `Choreographr` module with
     `tool_call`, `write`, `exit` syscall wrappers, dynamically-sized linked-list allocator)
    and compiled via a single
    `rustc +stable --target riscv64imac-unknown-none-elf -C opt-level=2 -C target-feature=+b,-a` invocation in a temp
   directory.  `opt-level=2` measurably reduces interpreter cycles
    versus the previous `-C opt-level=z` (≈8% in benchmarks), and `+b` lets LLVM emit
    RISC-V Bitmanip instructions (`cpop`, `clz`, `ctz`, `rev8`, …) that ckb-vm's `ISA_B`
    fully implements — harmless when unused, faster for bit-manip-heavy guests.
    The `-a` flag disables the RISC-V A (atomic) extension: the VM is single-hart (one
    instruction stream), so atomics have no real concurrency semantics, and removing them
    shrinks the untrusted instruction surface.  The machine is built with the same reduced
    ISA mask (`ISA_IMC | ISA_B | ISA_MOP`, no `ISA_A`), and guests that use
    `core::sync::atomic` read-modify-write operations (e.g. `AtomicU32::fetch_add`) are
    rejected at compile time — LLVM cannot select `amoadd.w` without the A extension.
3. Creates a `DefaultCoreMachine<u64, FlatMemory<u64>>` with 4 MB of flat memory
   (the default and the maximum — ckb-vm 0.24.14 hard-codes `RISCV_MAX_MEMORY = 4 << 20`
   in `ckb-vm-definitions`. `FlatMemory::new_with_memory` asserts on it and every memory
   access goes through `get_page_indices`, which rejects addresses beyond it, so 4MB is
   the largest VM this dependency can construct. The tool validates `memory_size` against
   `ckb_vm::RISCV_MAX_MEMORY` up front so an oversized request fails with a clean error
   instead of a panic inside the dependency. Raising the cap to 16MB requires a newer
   ckb-vm release — upstream `develop` has removed the cap, but nothing newer than
   0.24.14 is published; the `DEFAULT_VM_MEMORY` constant and schema text are derived
   from the upstream constant so they follow automatically on upgrade).  The default
    cycle budget is 10M (`DEFAULT_MAX_CYCLES`, configurable via `max_cycles`) — a ~10x
    bump over the original 1M, which real I/O-heavy guests (large tool outputs, line-heavy
    reports) routinely exhausted; a spinning `loop {}` still trips the cap in roughly a
    second of wall clock.
4. Registers a `ChoreographrSyscall` handler that intercepts three guest syscalls:
   - **Syscall #0 (TOOL_CALL)** — reads a postcard-encoded frame `[tool_name: String][args: bytes]`
     from guest memory, dispatches it via the `ToolRegistry::execute_dyn()`, and writes the
     postcard-encoded `Result<Return, String>` result to the guest's output buffer.
   - **Syscall #1 (WRITE)** — copies guest data into an accumulator buffer that becomes the tool's
     output upon VM exit.
   - **Syscall #93 (EXIT)** — stops the VM. Uses the Linux exit syscall number
     so that CKB-VM's `DefaultMachine::ecall()` handles it natively, properly
     propagating the exit code from register A0.
5. Loads the ELF via `TraceMachine::load_program` and runs via `TraceMachine::run()`.
6. After execution, the machine is dropped and the output channel is drained with
   a blocking `recv()` loop (deterministic — no buffered-item race).
7. Returns the formatted source wrapped in a `rust` markdown fenced code block,
   followed by the accumulated WRITE output, then a `[VM: exited with code N in M cycles]`
   summary line.  The TUI renders the code block with syntect syntax highlighting.

**Guest ABI** (auto-generated in the boilerplate):

```rust
pub mod Choreographr {
    pub unsafe fn tool_call(request: &[u8], output: &mut [u8]) -> usize;
    pub fn write(data: &[u8]);
    pub fn exit(code: i32) -> !;
}
```

A `#[global_allocator]` linked-list allocator is always included, enabling `alloc` crate
types (`Vec`, `String`, `format!`, `Box`, etc.), and `args()` is injected as a free function
returning `Vec<Vec<u8>>`:

```rust
pub fn args() -> Vec<Vec<u8>>;
```

**Safety:** The guest runs in an isolated VM with 4 MB of flat memory (ckb-vm's maximum). All tool access goes
through the same `ToolRegistry` as the host agent, respecting the same `x_credentials` and `working_dir`.
The guest cannot access host memory, syscalls, or files outside the VM without going through
registered tools.

### `exec` — direct program execution (no shell)

`exec` spawns a single program directly without shell interpretation. The command and each
argument are passed literally to `execvp` — no pipes, redirects, glob expansion, or
environment variable interpolation.

Two pre-flight guards steer the model away from the tool's two most common misuses; both
return actionable errors before anything is spawned:

1. **Shell-syntax guard** — a `|`, `>`, `<`, `&`, `;`, `$`, backtick, `*`, `?`, quote, or
   apostrophe in the command or any argument aborts with a message pointing the model to the
   `sh`/`nushell`/`fish` tools (pipes, redirects, globs, env vars, and chaining all require a
   shell).
2. **Program-existence check** — the command is resolved against PATH (or used directly when
   it contains a path separator); a miss returns the searched PATH and suggests `command -v
   <name>` via `sh` or an absolute path.

The tool description leads with the narrow use case (a concrete, existing program) and
explicitly defaults to `sh` when in doubt.

Sandboxing is identical to the shell tools: timeout, rlimits, env sanitization, output
truncation, and non-interactive stdin.

### `sh` — POSIX shell command execution

`sh` runs shell commands via a POSIX-compatible shell (`bash`, `dash`, or `zsh`). The `shell` parameter lists all three variants unconditionally (the manual `JsonSchema` impl emits a flat `"enum"` array instead of `oneOf`/`const` for wider provider compatibility). The `shell` parameter must be explicitly specified (no default), and `sh` itself is intentionally excluded — use `bash`, `dash`, or `zsh` directly.

Sandboxing (shared across all shell/exec tools via `shell_util.rs`):

1. **Timeout** — the command is killed after a configurable timeout (default 30s, max 300s). A watchdog thread enforces the inner timeout; the outer tool loop timeout is extended to 300s for this tool.

2. **Resource limits** — set via `setrlimit` in the child (pre-exec): `RLIMIT_AS` (4 GB) prevents runaway memory allocation, `RLIMIT_FSIZE` (100 MB) prevents disk-filling writes.

3. **Environment sanitization** — dangerous env vars (`LD_PRELOAD`, `LD_LIBRARY_PATH`, `LD_AUDIT`, `LD_DEBUG`, `PYTHONPATH`, `PERL5LIB`, `RUBYLIB`, `DYLD_INSERT_LIBRARIES`) are stripped in the child before exec.

4. **Output limits** — stdout/stderr are combined and truncated to 16 KB via `truncate_tool_output`, preventing context overflow.

5. **Non-interactive** — stdin is not connected. Commands that attempt to read from stdin will hang until the timeout.

In-process path confinement (`confine_path`) was removed in favour of OS-level sandboxing:
the session working directory is the boundary enforced by [Landlock](https://landlock.io/)
on Linux and [Seatbelt](https://theapplewiki.com/wiki/Dev:Seatbelt) on macOS (see README).
Tools still resolve relative paths against the working directory, but the boundary check
itself is the kernel's responsibility.

### `nushell` — nushell command execution with sandboxing

`nushell` runs commands in a child `nu -c` process with the same sandboxing as `sh`. Registered only when the `nu` binary is found in `PATH`.

### `fish` — fish shell command execution with sandboxing

`fish` runs commands in a child `fish -c` process with the same sandboxing as `sh`. Registered only when the `fish` binary is found in `PATH`.

Shell tools (`sh`, `fish`, `nu`, `exec`, and the streaming variants) put the
child in its own process group (`setup_child` in `tools/shell_util.rs`, applied
inside the shared `spawn_with_watchdog` / `spawn_with_streaming` helpers); on
timeout the watchdog kills the whole group via `killpg(2)`. On Linux the
child's identity is first pinned with a `pidfd` so a recycled PID can never
redirect the kill at an unrelated process; on platforms without `pidfd` (or
when `pidfd_send_signal` fails with a non-ESRCH error such as a seccomp
policy denying it) the kill is gated on the child being its group's leader
(`getpgid`), with a direct-kill fallback otherwise — rather than just killing
the direct child. This matters for shells that don't `exec` the final command
(fish): killing only the wrapper would orphan grandchildren like `sleep`, which
keep the output pipes open and turn a 500ms timeout into a ~10s hang.

The stdout/stderr pipes are drained in bounded background threads
(`drain_fd` / `poll_readable` in `tools/shell_util.rs`): each drain polls in
100ms slices and is stopped once the direct child is reaped. Without this, a
surviving grandchild that holds a pipe write end (a backgrounded
`sleep 10 &`, or a process that raced the killpg sweep) would keep the drain
thread blocked in `read(2)` past the timeout even though the direct child is
already gone. Every drain delivers its buffer (or a completion message) over
a channel, so the spawn helpers wait with `recv_timeout` — the same
channel-driven pattern as the watchdog — never by polling `is_finished`, and
every wait is bounded by a completion grace: a drain that misses it is
detached rather than hung (on Unix the handle is dropped and the thread exits
on its own once the survivor does — there is no way to force EOF without
killing the survivor, which we have no handle to).

On Windows the same helpers swap in the platform analogues: `setup_child`
has no process group to create, so the child is instead assigned to a Job
Object (`ChildJob`, created right after spawn — `Command` has no pre-exec
hook — with `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE` making the handle a
whole-tree kill switch). The pipes are drained with blocking reads
(`drain_reader`), which have no `poll(2)`/stop signal to interrupt; a drain
still silent at the 1s completion grace is wedged on a pipe a surviving
grandchild holds open, so the caller terminates the job to force EOF
(killing the survivor — the only way to close the write-end a blocking read
waits on) before waiting for delivery, bounded by a 5s grace after which the
thread is detached rather than hung. The watchdog's timeout kill is gated on
a `ProcessIsAlive` probe (a `WaitForSingleObject(0)` on a copy of the
child's process handle) so a child that finished on its own at the same
instant is not misreported as killed — the Windows analogue of the Unix
pidfd/ESRCH check. The `Arc<ChildJob>` and `ProcessIsAlive` handle copies
are the fifth sanctioned shared-state exception; the full rationale is in
AGENTS.md and the `tools/shell_util.rs` module row above.

On the streaming path (`spawn_with_streaming`), both drains split their
output into complete lines and forward them through a single merge channel
in arrival order — each stream keeps its relative order; the stdout/stderr
interleave itself is scheduling-dependent. The merger escapes Cf chars
before the bytes enter the stream budget and accumulates that same escaped
stream, so the recorded output contains exactly what was streamed,
truncation marker included (the stream budget reserves the record framing
via `RecordFraming` so the final cap never re-cuts the body). A watchdog
timeout additionally signals an abort channel that the merger selects on
while blocked on a full output channel, so a stalled subscriber cannot
wedge the tool past its timeout; the error path (a rare `wait()` failure)
tears every thread down before propagating.


| Layer | What's tested | Location |
|---|---|---|
| Protocol | Framing, version handling, round-trip encode/decode | `choreo-proto/src/tests.rs` |
| Client core | Shell parsing, markdown→HTML, image assembly, history | `choreo-client-core/src/tests.rs` |
| Daemon | Request lifecycle, session CRUD, cancellation, tool calls, model listing | `choreo-daemon/src/tests.rs`, `choreo-daemon/tests/session_integration.rs`, `choreo-daemon/tests/lifecycle_integration.rs` |
| MCP (choreo-mcp) | Server spawn, tool discovery, echo tool call/response | `choreo-mcp/tests/mcp_integration.rs` |
| MCP (daemon) | McpManager + ToolRegistry integration, dynamic group registration, tool execution | `choreo-daemon/tests/mcp_integration.rs` |
| Providers (`choreo-ai-protocols`) | SSE parsing, HTTP request construction, chat completions + responses serialization, content-block deserialisation, config overrides, catalog lookups | `choreo-ai-protocols/src/openai/tests.rs`, `choreo-ai-protocols/src/openai/chat_completions.rs`, `choreo-ai-protocols/src/openai/config.rs`, `choreo-ai-protocols/src/anthropic/tests.rs`, `choreo-ai-protocols/src/google/tests.rs`, `choreo-ai-protocols/src/catalog/mod.rs` |
| choreo-tui | SVG rasterization, Unicode width, app state | `choreo-tui/src/app_tests.rs`, `choreo-tui/src/lib_tests.rs` |
| choreo-gui | App state, render helpers | `choreo-gui/src/app_tests.rs` |
| Transport (`choreo-transport`) | Noise data plane — typed message round trips, single-fragment boundary + multi-fragment reassembly, oversized/tampered length-prefix rejection, malformed-handshake rejection, tampered-ciphertext rejection, silent-peer + dribbling-peer handshake timeout | `choreo-transport/tests/noise_integration.rs`, `choreo-transport/src/noise.rs`, `choreo-transport/src/handshake.rs` |
| Daemon↔client (Unix socket) | Ping/Pong, session CRUD round trips, attach ordering, `ShuttingDown`, concurrent clients + disconnect cleanup | `choreo-daemon/tests/daemon_client_unix.rs` |
| Daemon↔client (TCP/Noise) | Ping/Pong, session CRUD, cross-transport shared state, ACL rejection, wrong server key, encrypted `ShuttingDown`, explicit summary subscription (subscribed client gets broadcasts, unsubscribed gets none), >64 KiB fragmented message round trip, and a daemon→client >64 KiB reply round trip (`noise_large_message_daemon_to_client`) | `choreo-daemon/tests/daemon_client_noise.rs` |

> **Binary-spawning integration tests.** Integration tests live in their
> crates and test the libs. Any future binary-spawning integration test (via
> `env!("CARGO_BIN_EXE_...")`) must live in the ROOT package's `tests/`,
> because the root package owns the binaries.
>
> **Shared daemon-test harness.** `choreo-daemon/tests/common/mod.rs` provides
> the scaffolding for end-to-end daemon tests that run the real
> `run_server` (Unix socket + TCP/Noise) and drive it with the real client
> library: `test_db()`, `test_daemon_state()`, and `SpawnedDaemon` (spawns
> the server on a temp socket/ACL/free port, waits for both listeners, and
> SIGINT-shuts it down on drop). Test files opt in with `mod common;`.
> The daemon<->client tests (`tests/daemon_client_unix.rs`,
> `tests/daemon_client_noise.rs`) build on it.

These end-to-end tests close the previous coverage gap: the daemon's TCP/Noise
listener (`handshake_responder` + ACL + `tcp_client_thread`) and the real client
connection paths (`run_daemon_connection` / `run_daemon_tcp_connection`) had no
integration coverage before — only the transport primitives were exercised in
isolation. `daemon_client_unix.rs` covers Ping/Pong and ListSessions/CreateSession
round trips, CreateSession+Attach ordering (`SessionAttached` before
`SessionState`), the SIGINT `ShuttingDown` notification, and two concurrent
clients with client-disconnect cleanup; `daemon_client_noise.rs` covers the same
round trips over the encrypted channel plus cross-transport shared state (Noise +
Unix clients on one daemon), ACL rejection of an unknown client key,
wrong-server-public-key failure, and `ShuttingDown` through the encrypted channel
(previously Unix-only); summary broadcasts are an explicit opt-in on both
transports — `noise_subscribe_receives_session_broadcasts` pins that an
unsubscribed Noise client receives no broadcasts while a subscribed one does.
A 1 MiB `AddCredential` round trip (`noise_large_message_through_daemon`)
proves >64 KiB messages survive the full daemon path through the transport's
fragmentation. `noise_large_message_daemon_to_client` covers the reverse
direction: a ListSessions reply large enough to fragment travels intact from
the daemon's writer thread to the client. The extended `noise_integration.rs` data-plane tests push
the transport itself: typed `ClientMessage`/`DaemonMessage` round trips through
the Noise transport state, payloads at and beyond snow's 65535-byte ciphertext
cap — the 65518-byte single-fragment boundary plus multi-fragment reassembly
(65519 bytes = 2 fragments, 1 MiB = 17 fragments, and a post-fragment echo
proving nonces stay in sync) — malformed-handshake rejection, and a new
unit test (`transport_state_rejects_tampered_ciphertext`) proving GCM
authentication rejects a single flipped ciphertext byte, an
oversized-fragment-prefix rejection test (`noise_rejects_oversized_fragment_prefix`)
pins the length-prefix validation, and a tampered-prefix rejection test
(`noise_rejects_tampered_length_prefix`) proves a one-bit prefix flip on the
wire is rejected loudly — never silently truncated — because the reassembly
decision comes from the authenticated continuation byte, not the prefix. A regression test
(`noise_concurrent_bidirectional_large_messages`) pins the transport lock
scope: both endpoints send 1 MiB concurrently under tiny socket buffers, and
the sends must complete because `send_message` holds the `TransportState`
lock only per-chunk during encryption and never across the blocking socket
writes — the old lock-across-`write_all` code deadlocked this scenario
(neither side's reader could acquire the lock to drain the socket).

**Test infrastructure:** Most tests use `UnixStream::pair()` for socket-less daemon↔client
communication, and mock HTTP servers for API simulation; the end-to-end transport
tests above instead bind real sockets (a temp Unix-socket path and an ephemeral
TCP port via `SpawnedDaemon`).

**Test runner:** The recommended runner is cargo-nextest, configured in
`.config/nextest.toml` (`fail-fast = false`; 120s `slow-timeout` that kills hung
tests). Cargo aliases `test-fast` (unit tests), `test-integration` (the
`#[ignore]` suite), and `test-all` (both) invoke it with `--workspace`; plain
`cargo test` / `cargo test -- --ignored` still work via libtest. Nextest runs
every test in its own process — a large wall-time win for this 13-crate
workspace, since libtest serializes test binaries and threads their tests
within one process. Global *process-local* state needs no special handling
under nextest's process-per-test model — e.g. the keystore test-config-root
override in `choreo-transport` is thread-local and marked `#[serial]` only
because libtest runs tests as threads within one process; each nextest test
process gets its own copy. Fixed network ports are *not* isolated by
process-per-test, however: two test processes binding the same address conflict
just as two threads would, and `#[serial]` cannot serialize across processes —
prefer ephemeral ports (`TcpListener::bind("127.0.0.1:0")`) so tests never
contend for a fixed address.

Run all tests:
```bash
cargo test-all            # nextest: unit + integration, parallel
cargo test-fast           # nextest: unit tests only
cargo test-integration    # nextest: integration tests only
cargo test                # libtest unit tests
cargo test -- --ignored   # libtest integration tests
```


---

## Build and run

The manifests declare a `rust-version` (MSRV) in every crate, inherited from
`[workspace.package]` in the root `Cargo.toml`. It is a **release-time claim,
not a build constraint**: development runs on nightly and dist/publish builds
run on the current stable, and `.cargo/config.toml` sets
`resolver.incompatible-rust-versions = "allow"` so dependency resolution
always picks the newest available versions even when their declared
`rust-version` exceeds the workspace floor. Consequently the MSRV number may
lag the resolved tree during development; that is fine. Before publishing,
sync it: compute the resolved tree's floor with
`cargo metadata --format-version 1 | jq -r '[.packages[].rust_version |
select(. != null)] | sort_by(split(".") | map(tonumber)) | last'`, set the
result in `[workspace.package]` `rust-version`, and commit lockfile + bump
together, so
the crates.io metadata on the published crates is accurate (see RELEASE.md
Phase 2). The CI MSRV job validates the claim at release time rather than
constraining day-to-day development.

**Building defaults to nightly.** `rust-toolchain.toml` pins the workspace to
the `nightly` channel (rustup auto-installs it on first `cargo` run) so that
EVERY adhoc `cargo` command — including per-crate builds like `cargo build -p
choreo-x` / `cargo check -p x` / `cargo nextest run -p x` — automatically
applies the fast per-profile `-Z` compiler flags. Nightly enables the
per-profile `rustflags` in the root `Cargo.toml` via the unstable
`profile-rustflags` feature (opted in under `[unstable]` in `.cargo/config.toml`):
`-Zshare-generics=yes` in `[profile.dev]` only, `-Zunstable-options
--jobs-frontend=0` (parallel rustc frontend — `0` = one thread per logical
core via `available_parallelism`, the replacement for the deprecated
`-Zthreads`) in both dev and release, and `-C target-cpu=native`
(build for the local machine's CPU — AVX2/BMI2 on x86-64, the M-chip on
Apple Silicon) in both. An LLM/agent issuing raw `cargo` commands gets the
fast, native-tuned build with no extra ceremony. Profile rustflags replace
`[build]` rustflags but concatenate with `[target.'cfg(...)']` rustflags, so
per-machine linker flags (e.g. the wild linker in `~/.cargo/config.toml`)
still apply. `-C target-cpu=native` is a repo-wide local-build default that
NEVER reaches a shipped artifact: both `scripts/build-stable.sh` (dist
binaries) and `scripts/publish-stable.sh` (crates.io) strip the per-profile
rustflags keys, so dist builds get their CPU floor from the per-target
`RUSTFLAGS="-C target-cpu=…"` set in the release scripts (see the
"Release & packaging" section), published-source builds stay baseline, and
the RISC-V guest compiles (`tools/vm.rs`) are direct
`rustc +stable` calls that never see cargo rustflags at all.

**Stable builds are a supported opt-out.** The sources use no nightly-only
features, so the code builds on current stable (the MSRV claim in the root
`Cargo.toml` documents the tested floor; see above). The nightly-only
`profile-rustflags` wiring, however, hard-blocks stable *Cargo* (the keys it
enables require that unstable feature), so a stable build is run through
`scripts/build-stable.sh` (`just build-stable` / `check-stable` /
`test-stable`): it temporarily strips the nightly-only `rustflags` keys and the
`[unstable]` block, runs `cargo +stable ...`, and restores them on exit.
Kill-safety: the strip/restore is hardened against a hard-killed run (CI-style
timeout, SIGKILL — the EXIT trap cannot fire for those): backups are kept
persistently under `target/`, and the next run self-heals by restoring a
predecessor's surviving backups before taking its own (the failure this
closes — a killed run's mktemp backups lost, so the next run backed up and
"restored" the stripped files — was observed for real). `build-android.sh`
shares the mechanism.

The publish step has the identical constraint from the consumer side: per-
profile rustflags that ship inside a published `.crate` hard-break stable
`cargo install` (stable cargo errors on the `profile-rustflags` feature), so
crates.io publishing runs through the sibling `scripts/publish-stable.sh`
(`just publish-stable`), which strips the same keys before `cargo release
publish` and restores them on exit — see RELEASE.md Phase 2.

The `choreo-daemon` crate depends on `zlob` (a Zig-implemented glob and
gitignore-aware directory walker used by `grep`, `find`, `delete_files`, and
pathspec matching). Building it therefore requires the **Zig toolchain** on
`PATH` (or the `ZIG` environment variable pointing at the `zig` binary).
Install with Homebrew: `brew install zig`.

```bash
# Build everything
cargo build

# Build release
cargo build --release

# Run daemon (default-run selects the choreographr bin)
cargo run -p choreographr

# Run terminal client
cargo run -p choreographr --bin choreo-tui

# Run desktop client (its own crate — owns its binary)
cargo run -p choreo-gui

# Run IM bridge (Telegram)
cargo run -p choreographr --bin choreo-im -- telegram
```


---

## External dependencies (key crates)

| Crate | Used by | Purpose |
|---|---|---|
| `tokio` | choreo-blockchain, choreo-content | Async runtime — the sidecar the blockchain and Coordination Platform tools run on (linked via the daemon's `blockchain` and `content` features respectively) |
| `alloy` | choreo-blockchain | EVM blockchain tools (behind the `blockchain` feature) |
| `subxt` | choreo-blockchain | Substrate/Polkadot blockchain tools (behind the `blockchain` feature) |
| `serde` + `rmp-serde` | proto, daemon | Wire protocol framing and DB value encoding (MessagePack, named mode) |
| `structured-zstd` | daemon | Pure-Rust compression of `session_turns` DB values (a standard zstd frame around the MessagePack blob, level 6 — the tuned level maps onto C zstd numbering; see `db/codec.rs` `COMPRESSION_LEVEL`). Apache-2.0; no libzstd C build. |
| `snow` | daemon, client-core, transport | Noise IK handshake and transport encryption |
| `ureq` | daemon | HTTP client |
| `pulldown-cmark` + `ammonia` | client-core | Markdown parsing, HTML sanitization |
| `ratatui` + `crossterm` | choreo-tui | Terminal UI |
| `dioxus` | choreo-gui | Desktop/Android UI (Native/Blitz renderer) |
| `image` + `resvg` + `heif-oxide` (via the `choreo-image` leaf crate) | daemon, choreo-tui | Image decoding (all `image`-crate raster formats incl. feature-gated AVIF), SVG rasterization (resvg), HEIC/HEIF decode (heif-oxide) with a pre-decode allocation guard |
| `syntect` | choreo-tui | Syntax highlighting for code blocks (uses Sublime Text grammar files) |
| `aes-gcm` + `argon2` | keystore | Encryption, key derivation |
| `x25519-dalek` + `hkdf` + `sha2` | keystore | X25519 ECDH key agreement, HKDF key derivation |
| `ckb-vm` | daemon | RISC-V VM interpreter for sandboxed code execution |
| `postcard` | daemon, client-core | Compact binary serialization for Rust-only internal channels (VM↔host tool communication, encrypted credential pipeline) |
| `thiserror` | proto, keystore, client-core, daemon | Structured library error types |
| `anyhow` | daemon, tui, dioxus, im, keystore | Application error context & propagation |


---

## Error handling strategy

### Library crates — `thiserror`

Each library crate defines a structured error enum:

| Crate | Error type | Key variants |
|---|---|---|
| `choreo-proto` | `ProtoError` | `Codec`, `FrameTooLarge`, `TrailingBytes`, `UnsupportedVersion`, `Io` |
| `choreo-keystore` | `KeystoreError` | `Io`, `TooShort`, `DecryptionFailed`, `InvalidKeyLength`, `EncryptionFailed`, `ConfigDirNotFound` |
| `choreo-client-core` | `ClientError` | `Proto`, `Io`, `Utf8`, `ImageTooLarge`, `ImageExceedsSize`, `DuplicateImage`, `UnknownImage`, `ImageSizeMismatch` |

Every library error type implements `From<ErrorType> for io::Error` for backward compatibility
with code that still uses `io::Result`. Binary crates convert library errors to `anyhow::Error`
automatically via the blanket `From<E: Error> for anyhow::Error` impl.

### Binary crates — `anyhow`

All binary `main()` functions return `anyhow::Result<()>`. Key boundaries (socket bind,
keystore load, config parse) use `.context()` to attach meaningful messages. Internal
functions use domain error types (`io::Result`, `ProtoError`, etc.) and `?` auto-converts to
`anyhow::Error`.

The `send_or_warn` fire-and-forget broadcast helper uses `anyhow::Error` formatting for
warning logs when a subscriber is disconnected.

### Tool errors

Each tool defines its own error type via the `type Error` associated type on the `Tool` trait.
Simple tools use `ToolExecError` (a string-wrapper newtype). Tools whose errors are consumed by
VM guests (e.g. `DbError`, `HttpError`) define a `thiserror` enum that is `Serialize` +
`Deserialize`, enabling the guest to pattern-match on specific variants.

The `ToolError` enum (thiserror) covers *infrastructure* failures that happen around tool execution:
argument deserialisation, I/O, postcard encoding. It is never returned by a tool's `execute()`
directly — only by the `ToolDyn` conversion layer.

The `ToolDyn::execute_json()` and `execute_streaming_json()` methods return
`Result<ToolOutput, ToolError>` so callers can distinguish infrastructure errors
(JSON deserialisation) from tool execution errors. The caller converts `Err(e)` into a
`ToolOutput { is_error: true }` for the LLM path, preserving the structured error
for programmatic consumers.

The postcard binary path encodes all outcomes as a nested
`Result<Result<R, E>, ToolError>`: `Ok(Ok(ret))` for success, `Ok(Err(e))` for a
structured tool error, and `Err(e)` for an infrastructure failure. The `encode_outer()`
helper in `choreo-daemon/src/tools/mod.rs` handles this serialization.

`ToolOutput` replaces the old `ToolExecutionOutput` and `ToolResult` types. The `ToolOutputFormat`
enum lets callers choose between `Text` (human-readable via `return_string`) and `Json`
(JSON-encoded via `serde_json::to_string`) output formats.
| `gix` | daemon | Git operations |
| `teloxide` | choreo-im | Telegram Bot API client |
| `prometheus` | daemon | OpenMetrics instrumentation, process metrics (optional — behind the `metrics` feature, off by default) |
| `tiny_http` | daemon | Metrics HTTP server for `/metrics` endpoint (optional — behind the `metrics` feature, off by default) |
| `tracing` | daemon | Structured logging |
