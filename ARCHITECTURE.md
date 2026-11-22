# Choreographr Architecture

## Overview

`Choreographr` is a client/server AI assistant built as a Rust workspace. A **daemon** process
communicates with multiple LLM providers through a pluggable trait-based provider
system, while **clients** (terminal, desktop, and IM platforms) connect to the daemon
over a Unix domain socket (or Noise IK encrypted TCP for remote connections) using a custom length-prefixed binary protocol.

```
┌──────────────┐    Unix socket     ┌──────────────┐    HTTP/SSE     ┌──────────────────────┐
│   choreo-tui     │◄──────────────────►│              │◄──────────────►│  OpenAI API          │
│  (terminal)  │                    │              │                ├──────────────────────┤
├──────────────┤                    │  choreographr  │◄──────────────►│  Anthropic Messages   │
│ choreo-gui       │◄──────────────────►│              │                ├──────────────────────┤
│ (desktop/mobile) │  Unix socket or    │              │◄──────────────►│  Google Gemini API    │
│                  │  TCP/Noise-IK      │              │                │                      │
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

Twenty crates in a single Cargo workspace (resolver = "3") — the root
package plus nineteen members:

```
Choreographr (workspace)
├── choreographr          Workspace root — declares ONLY the daemon binary
│                       (default-run = "choreographr"); workspace
│                       default-members = [".", "choreo-tui"] keeps a bare
│                       `cargo build` producing daemon + TUI exactly as before
│                       the binary split (the GUI is a separate crate,
│                       choreo-gui, deliberately excluded from default-members)
├── choreo-proto           Wire protocol (shared types + framing)
├── choreo-shared         Leaf crate — shared binary-facing helpers:
│                       release-name metadata, clap styling, the `-v`/`-q`
│                       verbosity + log-level policy every CLI binary uses,
│                       and the filesystem-layout resolver (platform dirs +
│                       the `--base-dir` instance-root override); no protocol
│                       or transport logic
├── choreo-sanitize        Leaf crate — shared Unicode "spoofing" predicates,
│                       the tool-output byte budget + truncation marker, and
│                       the child-process code-injection env set
├── choreo-image          Leaf crate — shared image decode (EXIF-orientation
│                       baking, ICC→sRGB colour management via `moxcms`,
│                       header-only dimension probes, HEIC/HEIF decode +
│                       colour conversion through `heifer` under an
│                       in-decoder pixel budget)
├── choreo-keystore        X25519 + ECDH keypair crypto, encrypted storage primitives
├── choreo-transport       Noise IK encrypted TCP transport abstraction
├── choreo-client-core     Shared client logic (parsing, images, history, credentials, first-run setup)
├── choreo-markdown        Markdown parser and HTML renderer (pulldown-cmark + ammonia),
│                       plus a LaTeX math → Unicode pretty-printer (`render_math_pretty`)
├── choreo-mcp             MCP (Model Context Protocol) client built on the official `rmcp`
│                       SDK — connects over a subprocess's stdio or a remote Streamable
│                       HTTP endpoint, negotiates the protocol era (`server/discover`
│                       with an `initialize` fallback), lists paginated tools, and
│                       dispatches cancellable, concurrency-capped calls behind a
│                       blocking facade (linked via the daemon's `mcp` feature,
│                       which is on by default)
├── choreo-ai-protocols    Provider protocols — OpenAI-compatible, Anthropic Messages, and
│                       Google Gemini clients, the ProviderClient trait, and the provider
│                       catalog (models.dev base + bundled overlay, embedded postcard)
├── choreo-sockreg         Live provider-socket registry + TCP keepalive tuning —
│                       force-close (`shutdown_all`) and liveness-prune for sockets
│                       registered by provider clients, so cancels/suspends can un-block
│                       workers wedged in a provider read (Unix via nix; Windows via
│                       Winsock shutdown/prune + keepalive tuning)
├── choreo-power-events    Platform suspend/wake notifications as crossbeam events —
│                       logind PrepareForSleep (Linux), IOKit power notifications (macOS),
│                       user32 suspend/resume callbacks (Windows), inert fallback elsewhere;
│                       best-effort convenience layer over
│                       sockreg's kernel keepalives, never a correctness layer
├── choreo-daemon          Unix socket server — the core engine (library; the
│                       daemon binary `choreographr` is declared by the root
│                       package)
├── choreo-blockchain      Blockchain tools — EVM (alloy) and Substrate/Polkadot
│                       (subxt) read-only queries + the tokio sidecar runtime they
│                       need (linked only via the daemon's `blockchain` feature,
│                       off by default)
├── choreo-acp             ACP bridge — translates the Agent Communication Protocol
│                       (JSON-RPC over stdin/stdout) into choreo-proto messages over the
│                       daemon's Unix socket, enabling ACP-compatible editors (Claude
│                       Code, Cline, etc.) to interact with Choreographr sessions —
│                       owns its binary (src/main.rs); not in default-members, build
│                       with -p choreo-acp
├── choreo-content           Choreographr Coordination Platform client — Substrate
│                       chain writes (subxt via a tokio sidecar), indexer reads,
│                       IPFS add/cat, content protobuf encode/decode, and the
│                       publish-time image mipmap pipeline behind the `content` tools
│                       (feature-gated `content` cargo feature, off by default)
├── choreo-tui             Terminal UI client (ratatui + crossterm; owns its
│                       binary — src/main.rs; in default-members)
├── choreo-gui             Desktop/Android/iOS GUI client (Dioxus Native / Blitz
│                       renderer — no webview; lib+cdylib for dx/gradle APK
│                       packaging; iOS via scripts/build-ios.sh + the ios/
│                       Xcode scaffold; on iOS it hosts an embedded in-process
│                       daemon via choreo-daemon::embedded under the Mobile
│                       tool policy — see the choreo-gui section)
└── choreo-im              IM platform bridge (Telegram; owns its binary
                        (src/main.rs); not in default-members, build with -p
                        choreo-im)
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

Two leaf crates exist so that a policy with many consumers has exactly one home:
`choreo-sanitize` (consumed by `choreo-daemon`, `choreo-mcp`, `choreo-tui`,
`choreo-blockchain`, and `choreo-client-core`) holds the Unicode "spoofing"
predicates, the tool-output byte budget with its `...[truncated]` marker, and the
code-injection environment set stripped from every spawned child, so every
sanitizer, streaming cap and spawned child agrees; `choreo-image` (consumed by
`choreo-daemon` and `choreo-tui`) holds the single decode path for rasters,
HEIC/HEIF and SVG under one decompression-bomb budget, so the model path and the
UI path cannot drift apart on a colour fix or a bomb guard. Their rustdoc owns
the per-format detail (`cargo doc -p choreo-sanitize` / `-p choreo-image`,
`just doc`), held complete by the `#![warn(missing_docs)]` + `doc-check` gate.

---

## Release & packaging

Releases are cut **locally** with the orchestrator, or on GitHub Actions by
pushing a `vX.Y.Z` tag —
[`.github/workflows/release.yml`](../.github/workflows/release.yml) is the CI
counterpart of [`scripts/release.sh`](../scripts/release.sh): the Linux, macOS,
and FreeBSD jobs run that script verbatim and the Termux job runs its android
sibling, so build flags, `--locked`, feature selection, service-file staging,
and tarball naming stay in one place. A `workflow_dispatch` run builds
everything but creates no release — the pipeline-test path, so the first real
tag is never the first test of the pipeline. The version is read from
`[workspace.package] version` in the root `Cargo.toml`, the single source of
truth the Homebrew formula, AUR PKGBUILD, and installer mirror, and every
shipped build selects the workspace's dedicated `[profile.dist]` profile (root
`Cargo.toml`) rather than the local `[profile.release]`.

The phase-by-phase runbook is [RELEASE.md](./RELEASE.md). `scripts/release.sh`
is a dry run by default and only uploads under `--upload`; `just release` /
`release-upload` / `release-tap` / `smoke-test` / `package-deb` / `package-rpm`
/ `install` are the front door.

### Shipped artifacts

Exactly **two binaries** ship in every artifact (tarball, `.deb`, `.rpm`,
Windows `.zip`, Homebrew, AUR):

- `choreographr` — the daemon
- `choreo-tui` — terminal UI client

Nothing else is shipped. The `choreo-im` and `choreo-acp` bridges are
source-build extras (`cargo build -p choreo-im` / `-p choreo-acp`), not release
targets. `choreo-mcp` is **library-only**: its sole `[[bin]]` is the integration
suite's `mcp-fixture-server`, which release builds never produce. `choreo-gui`
is built separately (desktop via `cargo run -p choreo-gui`, Android via
`dx build --platform android`, iOS via `scripts/build-ios.sh` + the `ios/`
Xcode scaffold) and is not shipped either.

The POSIX target set is exactly five; `release.sh` and `install.sh` hardcode it
and refuse any other platform, and each native host builds its own arch's
tarball (a Darwin-arm64 host builds both darwin tarballs, the x86_64 one
cross-compiled against the single Xcode SDK):

| Target | Platform | Asset |
|---|---|---|
| `x86_64-unknown-linux-musl` | Linux x86_64 | `choreographr-<version>-x86_64-unknown-linux-musl.tar.gz` |
| `aarch64-unknown-linux-musl` | Linux arm64 | `choreographr-<version>-aarch64-unknown-linux-musl.tar.gz` |
| `aarch64-apple-darwin` | macOS arm64 (native) | `choreographr-<version>-aarch64-apple-darwin.tar.gz` |
| `x86_64-apple-darwin` | macOS Intel (cross-built on the arm64 host) | `choreographr-<version>-x86_64-apple-darwin.tar.gz` |
| `x86_64-unknown-freebsd` | FreeBSD x86_64 (native) | `choreographr-<version>-x86_64-unknown-freebsd.tar.gz` |

The CI release adds a sixth artifact, a **Windows `x86_64-pc-windows-msvc`
`.zip`** of the same two `.exe` binaries, built only by the workflow's
`windows-msvc` job and attached to the release beside the tarballs, the
`.deb`/`.rpm`, and the Termux `.deb`.

Build details — the fully static musl tarballs with `mimalloc`, the native
glibc `.deb`/`.rpm`, the FreeBSD base-system build, archive layout, and
host-arch detection — are documented at their build sites in
[`scripts/`](../scripts) and in [`packaging/README.md`](../packaging/README.md).

Shipped binaries build at an explicit **CPU floor per target**, passed as
`RUSTFLAGS="-C target-cpu=…"` by the release scripts — never
`target-cpu=native`, and never profile rustflags (those ignore `--target` and
would poison a cross build, they are nightly-only, and `scripts/build-stable.sh`
strips them before every stable release build; env rustflags additionally
override a developer's `~/.cargo/config.toml`, so local and CI artifacts are
comparable). Each target's chosen level — x86-64-v2 on x86_64 Linux and
Windows, the target default on aarch64, x86-64-v3 on Intel macOS, and baseline
for the `.deb`/`.rpm` — and the fleet reasoning behind it are documented where
the flag is set, in [`scripts/release.sh`](../scripts/release.sh) and the
release workflow. C dependencies (mimalloc, compiled by `cc`/zig) are NOT
affected by `RUSTFLAGS` — only rustc codegen is — and mimalloc does its own
runtime CPU-feature detection.

**CI linker policy: default linkers everywhere.** No mold/wild/lld additions in
the release pipeline: fat-LTO links spend their time in LLVM, not in the
linker's data structures, so a faster linker barely moves link time, and CI link
time is a rounding error next to the fat-LTO builds. A developer's local `wild`
(via `~/.cargo/config.toml`) is deliberately overridden by the release scripts'
env `RUSTFLAGS`. That link's one expected warning is documented at the build
site.

### `packaging/` assets

`packaging/` holds the shipped service files — `choreographr.service` (systemd
**user** unit), `com.choreographr.daemon.plist` (launchd agent for
non-Homebrew macOS installs), `choreographr.rc` (FreeBSD `rc.d` script) — plus
the package recipes: `homebrew/choreographr.rb`, `aur/PKGBUILD` + `.SRCINFO`,
and `rpm/choreographr.spec`.
[`packaging/README.md`](../packaging/README.md) owns the per-asset inventory
(what each file is, and where it is published).

**Policy: installed, never auto-enabled.** The daemon is a *user* service that
needs accounts and API keys before it is useful, so no package script,
installer, or release tool enables it — opting in is always the user's action
(`systemctl --user enable --now choreographr`, `launchctl load …`). The
`packaging/README.md` policy section carries the per-channel consequences.

### `scripts/` tooling

Each script's header comment is its authoritative description; this table is the
index.

| Script | Role |
|---|---|
| `install.sh` | curl\|sh installer — pinned version, per-file SHA-256 fetched over the same TLS channel, explicit tarball member list (no eval, no trust-on-first-use), installs the service file, never auto-enables; `--uninstall` removes everything |
| `release.sh` | The release orchestrator (dry-run default; `--upload`, `--allow-dirty`); its CI counterpart is `.github/workflows/release.yml` |
| `build-deb.sh` / `build-rpm.sh` | The fat `.deb` / `.rpm` from existing `target/dist/` artifacts; host arch detected (`x86_64`/`aarch64` only, failing loudly otherwise), package naming per `packaging/README.md` |
| `build-deb-termux.sh` | The Termux-native `.deb`, packaged from the already-cross-built `target/android/arm64-v8a/` binaries — no rebuild, CI android job only |
| `smoke-test.sh` | Artifact smoke test — tarball: the shipped binaries present, executable, and answering `--version`/`--help`; Termux `.deb`: structural `dpkg-deb` validation |
| `daemon-smoke.sh` | Boots the shipped daemon hermetically (scratch socket + config dir) and proves the listener comes up |
| `publish-stable.sh` | The crates.io publish wrapper (RELEASE.md Phase 2) — strips the nightly-only profile rustflags and the `[unstable]` opt-in, masks the two edited files from cargo-release's clean-tree check, and excludes every `publish = false` member |
| `update-homebrew-tap.sh` | Bumps the tap formula to the workspace version, recomputing both macOS digests from `dist/`; `--push` commits and pushes to the tap |
| `check-supply-chain.sh` | The dependency supply-chain gate — the local registry-cache scan for the 2026-08-20 `arrayref` attack plus `cargo deny check advisories bans sources` with a `cargo-audit` fallback |
| `build-android.sh` | cargo-ndk cross-build for Termux under `--profile dist`, including the bionic `PT_TLS` 64-byte alignment gate that fails the build rather than shipping a binary that dies on-device |
| `build-ios.sh` / `check-ios.sh` / `lib/ios-cc-shims.sh` | `choreo-gui` iOS staticlib staging for the `ios/` Xcode scaffold, and the zig-`cc`/SDK shims that let a host without Xcode validate it |

### Distribution channels (0.1)

- **Homebrew tap** — `brew tap choreographr/choreographr && brew install choreographr` (prebuilt formula)
- **GitHub Releases** — the Linux x86_64 + arm64 musl tarballs, `SHA256SUMS`, the desktop `.deb`/`.rpm` in both Linux arches, and the Termux `.deb` at `https://github.com/choreographr/choreographr/releases`
- **choreographr.com** — `https://choreographr.com/download/<version>/` mirrors the tarball and `SHA256SUMS` (this is what `install.sh` fetches); `https://choreographr.com/install.sh` serves the installer, and per-version download redirects are added at release time
- **AUR** — `choreographr-bin` (prepared in `packaging/aur/`, **not yet published**: no maintainer account and AUR registration is closed)
- **crates.io** — `cargo install choreographr choreo-tui` (source build, needs Zig) and `cargo binstall choreographr choreo-tui` (prebuilt; asset naming resolved via `[package.metadata.binstall]` in each package, below)

### crates.io metadata

The workspace inherits crates.io-required fields from `[workspace.package]` in
the root `Cargo.toml` (`version`, `license`, `repository`, `homepage`, `readme`,
`description`), and members opt into publishing by *not* setting
`publish = false`. The **publish set** is therefore nineteen of the twenty
workspace packages (the root package plus eighteen of the nineteen members) —
everything except `choreo-gui`, the one private member (a leaf client nothing
depends on, whose Dioxus Native renderer tree is not part of the shipped suite,
so it is neither published nor in the prebuilt artifacts). The leaf crates —
`choreo-sanitize`, `choreo-image`, `choreo-sockreg`, `choreo-power-events` — and
`choreo-content` are in the set because a published member depends on each of
them, and cargo will not package a crate whose dependency is unpublished
(optional deps included):

`choreographr` (root), `choreo-daemon`, `choreo-blockchain`, `choreo-tui`,
`choreo-im`, `choreo-acp`, `choreo-proto`, `choreo-shared`, `choreo-keystore`,
`choreo-transport`, `choreo-ai-protocols`, `choreo-mcp`, `choreo-client-core`,
`choreo-sanitize`, `choreo-markdown`, `choreo-image`, `choreo-sockreg`,
`choreo-power-events`, `choreo-content`

Releases are driven by **cargo-release** (`[workspace.metadata.release]` in the
root `Cargo.toml`): it bumps versions, tags, and publishes the 19 crates
topologically, and `dependent-version = "fix"` keeps published manifest
requirements in lockstep across minor/major bumps. The crates.io publish runs
through `scripts/publish-stable.sh` (which keeps the published manifests
buildable by stable `cargo install`) and happens before `scripts/release.sh`
builds the prebuilt artifacts.

Both shipped packages declare `[package.metadata.binstall]` — identical blocks,
so `cargo binstall choreographr` and `cargo binstall choreo-tui` resolve the
same release asset from the package itself, with no `--pkg-url`; binstall
installs only the binaries a package declares, so each extracts just its own.
The block's template mechanics and the glibc→musl host overrides are documented
in the manifests.

---

## Wire protocol

Defines all shared message types and framing.

The envelope design is cross-cutting. Both directions use one shaped frame — a
`ClientMessage` is `{ id: u64, inner: ClientMessageType }`, a `DaemonMessage` is
`{ id: Option<u64>, inner: DaemonMessageType }`. The `id` is the **reply axis**:
per-connection and one-shot — the daemon MUST answer every request with exactly
one `DaemonMessage { id: Some(the same id), .. }` (the terminal reply, success or
failure), and a broadcast (`id: None`) never resolves a request. It is
deliberately kept SEPARATE from the **stream axis** (`stream_id`): the stream
axis is per-session and many-event (fanned to every subscriber, including
mid-stream joiners), so its key space is per-session while the reply axis's is
per-connection — they cannot merge, and no code carries both. Reply-ness is a
property of the SEND, not the payload type: the same `inner` (e.g.
`SessionState`, `ReasoningEffortSet`) may ride either way, and a variant is split
into a dedicated type only for requester-relative intent
(`SessionCreatedForRequester`). Session-scoped `SessionEvent`s ride the
`DaemonMessage::Session { session_id: Option<u64>, event }` **envelope**, which
hoists the origin session off the event: events do NOT carry a `session_id`, so
every event has an origin by construction (`Some(id)` for session-scoped
broadcasts; `None` for the connection-level replies the daemon synthesizes with
no session, e.g. "no session attached" failures). The daemon's payload enum is
deliberately not `#[non_exhaustive]`: the variant set IS the wire contract, so
every consumer match enumerates it fully.

**Wire format:**

```
┌──────────────────┬────────────────────────────────────────┐
│ 4 bytes (BE u32) │ msgpack((protocol_version: u8, msg))   │
│   payload len    │                                        │
└──────────────────┴────────────────────────────────────────┘
```

- Protocol version: `9` (v9 = live MCP config reload: the new `ClientMessage::McpReload` request and its `DaemonMessage::McpReloaded`/`McpReloadFailed` replies, so a running daemon can pick up `mcp.json` edits (added/removed/changed servers) without a restart; v8 = per-session `pinned`/`archived` flags: the new `ClientMessage::SetSessionPinned`/`SetSessionArchived` requests, the daemon-GENERATED broadcast `SessionEvent::SessionFlagsChanged` (rides `DaemonState::broadcast()`, so a client learns of a flag change via the broadcast rather than a targeted reply), and the `SessionSummary::pinned`/`archived_at` fields they surface. v7 = the create-session reply is split from the create-session broadcast: `SessionEvent::SessionCreatedForRequester` is the direct reply to the creating connection and is the ONLY create event a frontend may auto-attach to, while `SessionEvent::SessionCreated` is notification-only (broadcast to every subscriber) and must never move a client's view — fixing a client hijacking its own view when ANOTHER client created a session. Delete/status lifecycle messages are unaffected: `SessionDeleted` was already broadcast-only with the requester already knowing the id it deleted; v6 = displayed-image bytes are no longer shipped in session-scoped snapshots (`SessionState`/`TurnAppended`/`TurnsRedone`) — they carry only `ImageMetadata` — and are instead fetched on demand via the new `ClientMessage::GetImage` ⇄ `DaemonMessage::Image` pair, keyed by `(session_id, turn_id, image_index)` and served from the durable `session_attachments` store; v5 = the two-state `Locked`/`Unlocked` status *broadcasts* were replaced by `DaemonMessage::Keystore { state: KeystoreState }` (`Unbound`/`Locked`/`Unlocked`), pushed at subscribe time and on every transition so a first-run client learns it must BIND; `Locked`/`Unlocked` remain targeted operation replies. v4 = the 29 session-scoped events were moved into `SessionEvent` and now ride the `DaemonMessage::Session { session_id: Option<u64>, event }` envelope; v3 had removed `TurnFinalized` — the final-turn snapshot rides `TurnAppended` — and added `Evicted`, a best-effort lag-eviction advisory; mixed-version peers fail fast at the version gate). The v4 shape was amended in place before the first release — the `session_id: 0` sentinel became `Option<u64>` (`None` for connection-level replies) — so the wire version stayed `4` with no bump at that point. The MCP trust-query request/reply variants (`ClientMessage::McpTrust`/`McpUntrust`/`McpTrustList`, `DaemonMessage::McpTrustUpdated`/`McpTrustList`) were added to v9 the same way, before any release carried v9; the policy is that variants may be added to the **current, unreleased** wire version without a bump, because mixed-version peers do not exist until a release ships and the version gate fails fast on a mismatch either way. A bump is required only once a version has shipped in a release. Under the same pre-release policy, the v6 `GetImage`/`Image` pair was reshaped for v9 before any release carried v9: its positional `image_index: u32` became a tagged `key: ImageKey` (`Displayed { index }` or `ToolResult { call_id }`), so one fetch protocol now serves both displayed images and tool-result vision images from the shared `session_attachments` store. Under the same pre-release policy, the v9 frame was amended in place to the uniform correlation envelope: `ClientMessage` = `{ id: u64, inner: ClientMessageType }`, `DaemonMessage` = `{ id: Option<u64>, inner: DaemonMessageType }` (a broadcast is `id: None`; a reply stamps the request's id), the payload enums were renamed `ClientMessageType`/`DaemonMessageType`, the streaming `request_id` was renamed `stream_id` and widened to `u64`, and the `MessageKind` tag plus the `Accepted`/`Failed` acknowledgement replies were added. No bump — no release has shipped v9. Under the same pre-release policy, the v9 streaming axis was made daemon-owned: the session thread assigns a run's `stream_id` (a per-session, monotonic counter) when it accepts a `RunInput`/`ContinueGeneration` and reports it on the acceptance `Started` reply/broadcast, so those requests no longer carry a client-chosen `stream_id` (the client learns it from `Started`, which also fixes the cross-client collision where two clients fanned one session could each claim the same id); the `CANCEL_ALL` sentinel (`stream_id = 0`) remains for a pre-`Started` cancel. Under the same pre-release policy, the `SessionStatus` streaming phase was split in v9: the single `Inference` state became `Sending`/`Waiting`/`Reasoning`/`Responding`, so a status bar can tell the request dispatch from the provider's time-to-first-token wait from reasoning from answer streaming. The `Waiting` boundary is anchored on client-side request-upload completion (the last byte of the request body handed to the transport), not on the provider's response headers: a buffering gateway (e.g. opencode.ai behind Cloudflare) withholds the response head until the model begins generating, so anchoring on the headers would collapse `Waiting` into `Sending` for those providers. The upload hook fires once per attempt, so a retried turn reports `Waiting` for the failed attempt too, before `Retrying`. Under the same pre-release policy, `ClientMessage::AddCredential` was reshaped in v9 to carry a typed plaintext `ServiceCredential` (`{ service, credential }`) instead of a client-encrypted `encrypted_payload` plus an `unlock_key`: no key rides the wire, the daemon encrypts the credential to its own binding public key, and the add path requires an already-unlocked keystore (no implicit unlock). Also under the same pre-release policy, v9 gained the `DaemonMessage::ModelsUpdated { account, models }` broadcast, emitted when a background model-list fetch finishes so an already-open model picker refreshes in place; a cold picker now paints instantly from a locally cached or catalog-seeded list and this broadcast supersedes that seed when the live fetch lands, instead of blocking on a slow `/v1/models` round-trip. Under the same pre-release policy, v9 gained the `DaemonMessage::ModelsUnavailable { account, error }` broadcast — the counterpart of `ModelsUpdated`, emitted when that background fetch FAILS, so a picker waiting on it shows the provider's error instead of waiting forever — and `DaemonMessage::Models` with `provisional: true` and an empty `models` list became the way the daemon reports "nothing is cached or seedable for this account and a fetch is running". Those two pushes are what end a client's wait: a client keeps no retry timer for an answer the daemon promises to send. Under the same pre-release policy, v9 gained the atomic account/model/effort choice: `ClientMessage::SetSessionChoice { account, model, effort }` commits all three to the attached session in ONE write, so an account is never observable bound to a model its provider does not serve. The two queries that let a client *suggest* a choice first ride the same version: `ListModels` gained `account: Option<String>` (`Some(name)` asks about a candidate account the session is not bound to yet), `DaemonMessage::Models`/`ModelsUpdated` now carry `Vec<ModelInfo>` (each id with its supported reasoning levels) plus, on `Models`, a `suggested_model` to pre-highlight while the choice is pending, and the new `ClientMessage::ModelReasoning { account, model }` ⇄ `DaemonMessage::ModelReasoning { capability, suggested_effort }` pair answers a candidate model's levels and the effort to pre-select for it.
- Max frame size: 64 MiB

Payloads are MessagePack in **named mode** (`rmp_serde::to_vec_named`): structs
serialize as maps with field names and enum variants by variant name, so the
format is self-describing, compact, and broadly supported across languages
(future mobile/web/third-party clients). The `(protocol_version, message)` tuple
still encodes as a MessagePack array of 2 even in named mode.

**Codec rule.** MessagePack (named) carries anything that crosses a language
boundary: the client↔daemon wire and the values in `sessions`/`session_turns`.
Since schema 2, `session_turns` values are additionally zstd-compressed (see
the DB section) — the on-disk codec is zstd frame + MessagePack named(`Turn`),
while the wire turn stays plain MessagePack. Postcard is retained only on
Rust-only, language-isolated internal channels —
the RISC-V VM↔host protocol and the encrypted credential pipeline — where no
foreign reader will ever touch the bytes.

## Enrollment & transport trust

The TCP enrollment model is a deliberate ASYMMETRY — the SSH `known_hosts`
pattern, not symmetric TOFU: a client's transport key is provisioned into the
daemon's ACL by the operator out of band, while the server's key is learned
in-band on first contact, confirmed by a human, then pinned. The asymmetry
exists so the first-contact window (the only moment a MITM could strike) can
never observe the daemon's keystore unlock key, which clients send inside
`Unlock`.

The trust gate that makes that hold: with no pin for an address the client runs
Noise XX, and the revealed server key is confirmed against the fingerprint the
daemon operator read out during enrollment BEFORE a single protocol message
flows — a refusal closes the connection with no `Unlock` sent. Only a confirmed
key is pinned, and every later connection uses Noise IK against the pin, where a
changed server key fails loudly with re-pair guidance rather than silently
re-prompting. Blind TOFU is rejected because it would let a first-contact MITM
harvest an `Unlock` (strictly worse than a phished SSH password), and a
symmetric out-of-band exchange would add client-side file handling for no
security gain. The daemon-as-client future reuses this shape: one keypair per
daemon serving both roles, enrollment operator-driven, headless confirmation via
the fingerprint flag.

**Decision rule:** a new peer is always provisioned by an operator who can
touch a trusted machine — never by the network. Any future feature that
seems to require in-band trust bootstrapping is a sign of trying to automate
operator trust; revisit the XX confirm UX before weakening the gate.

Every mechanism behind that model — the three ACL provisioning paths and their
shared advisory-lock write discipline, the parse-compare reload policy that
keeps a torn editor save from un-authorizing live clients, the watcher
placement, the fingerprint rendering, and the pin/unlock-key store's own
hardening — is documented in-source (`choreo-daemon`'s `server/acl`,
`choreo-transport`'s `key`, `choreo-client-core`'s `known_servers` and
`connection`, `choreo-tui`'s `lib`).

The plaintext side of the transport is zeroized end to end: an encoded frame —
which may carry a secret such as a credential riding `AddCredential`, or the
unlock key — is a `Zeroizing` buffer in `choreo-proto` that `choreo-transport`
holds until the encryption call returns, and the transport's reused per-fragment
staging buffer is cleared after every send. Both halves are documented at their
sites (`frame` and `noise`).

## Provider Architecture

Provider support is layered across two crates, and the seam between them is the
point of the split. `choreo-ai-protocols` owns every wire protocol — the
`ProviderClient` trait (`src/traits.rs`), the `ChatTurnRequest` /
`ChatTurnResult` turn types it is expressed in (`src/types.rs`), the three chat
adapters (`openai/`, `anthropic/`, `google/`), and the static catalog
(`src/catalog/`). `choreo-daemon` owns `InferenceProvider`
(`src/providers/mod.rs`), the only daemon type that dispatches by protocol: it
wraps a `dyn ProviderClient` together with the **catalog slug** (e.g.
`"opencode"` even when the client is an `OpenAiClient`) because catalog lookups
need the provider identity the client itself may report generically, and
`from_account_config` resolves the account's slug through the catalog and builds
the matching client. All wire-format knowledge stays in `choreo-ai-protocols`;
the daemon supplies the account-config, metrics, and session concerns at the
boundary.

One contract crosses that boundary as a public surface in its own right:
**metrics**. `InferenceProvider` records `record_api_call` / `record_api_error`
around every turn — timing lives in the daemon so `choreo-ai-protocols` stays
free of daemon concerns. The `endpoint` label is the **catalog slug**, not the
protocol name (e.g. `"opencode"` rather than `"openai"`): more precise, but part
of the public Prometheus contract, so renaming a slug changes that provider's
series. Error labels come from `InferenceError::metric_label()` in `choreo-proto`,
the single canonical mapping shared by all providers.

### models.dev + overlay

The catalog separates **facts** from **policy**: a normalized models.dev base
supplies slugs, base URLs, and per-model context/reasoning/cost data, and a
bundled overlay supplies everything models.dev cannot express (wire protocol,
endpoint, reasoning passback exceptions, the prompt-cache TTL and
retention/affinity dialects, request-body compression, and the overlay-only
providers). The overlay is merged **at load time**, never baked into the
committed base blob, so the runtime refresh (S4) can re-merge the same base with
a user overlay. The overlay schema and merge precedence are
`catalog/models-overlay.toml`'s header; the pipeline and the model-entry types
are the `catalog` module's rustdoc.

### Provider auth: grant versus request material

Provider authentication is a **two-layer** model: the daemon resolves an account
to a `ProviderAuth` (an API-key grant, or the whole durable OAuth grant plus any
cached access token) and projects it into the `RequestAuth` a client consumes —
the bearer material and the credential-derived headers, nothing else. The grant
therefore never leaves the daemon, and `choreo-ai-protocols` gains no
`choreo-shared` dependency: only the already-derived request material crosses
that seam. Both layers, the projection that turns a not-yet-signed-in account
into a clean error, and the single header-derivation function are documented in
`choreo-daemon/src/providers/auth.rs`. A provider's auth kind is **derived** from
whether the catalog carries an `oauth` profile, and how the flow identifies
itself is a compiled per-slug policy that the overlay deliberately cannot
override (`choreo-ai-protocols/src/identity.rs`).

#### Provider OAuth login (frontend)

The interactive sign-in runs in the **frontend**, never the daemon: the shared
login library in `choreo-client-core` (`src/oauth/`) runs PKCE S256 with a random
`state` plus a loopback callback server (or an RFC 8628 device-code poll on
headless hosts) on a **worker thread**, streams progress as events over a
channel, and returns the `AddCredential` frame carrying the durable credential —
so no render crate constructs an OAuth credential and the credential's shape is
assembled in one place. Dropping the returned session handle cancels the flow.
The daemon serves each provider's sign-in parameters over a `ListOAuthProviders`
⇄ `OAuthProviders` pair assembled from the catalog `oauth` profile plus the
identity policy, so a frontend embeds no copy of the issuer, scopes, redirect
ports, encodings, or client id, and a borrowed vendor id is withheld unless the
per-install opt-in is on. The `id_token` claim decode, the
endpoint-security predicate, and the token-reply reader are shared with the
refresh path in `choreo-shared`, so login and refresh cannot drift.

#### Provider OAuth refresh (runtime)

The access token is refreshed on demand in the **session thread**: the blocking
exchange stays in `choreo-ai-protocols` (`oauth::refresh_oauth_token`, which
refuses to send the durable refresh token to anything but `https` or a loopback
address), while the daemon's command loop owns the token cache and is the sole
writer of the credential blob. `choreo-daemon/src/oauth_refresh/` is the seam —
an `OAuthTokenSource` the session implements over its command channel, plus one
single-flight driver. The wipe-on-drop refresh material, the proactive expiry
buffer, the one forced refresh on a `401`, the rotate-if-present rule, and the
terminal-versus-transient classification are documented there and in
`choreo-ai-protocols/src/oauth`; the per-credential lock that makes a concurrent
refresh re-read the winner's token is the **fourteenth** sanctioned shared-state
exception (see AGENTS.md). A terminal rejection quarantines the account
(`DaemonState::dead_oauth`) and broadcasts `OAuthReloginRequired` exactly once,
on the quarantine transition.

### Request-body compression

Every turn re-sends the conversation plus the tool schemas as JSON, so the body
is the cheapest thing to shrink — but models.dev carries no encoding fact and the
server must decompress, so compression is **opt-in overlay policy**
(`request_compression = "gzip"`/`"zstd"` on a `[provider.<slug>]` entry; the
default is `identity`). The coding is baked into the provider client at
construction, so an overlay edit reaches live sessions only through the
catalog-swap client invalidation (see Runtime catalog refresh). The one downgrade
is a process-wide registry written **only** on an HTTP `415` — the
**thirteenth** sanctioned shared-state exception (see AGENTS.md). `400` is
deliberately not an encoding rejection: it is also the status for a genuinely
malformed request that re-sending uncompressed cannot fix, so a provider that
answers `400` for an unsupported coding keeps failing until its
`request_compression` is removed by hand. The coding, the codecs, the upgrade
hook, and the downgrade all live in `choreo-ai-protocols`'s `retry` module.

### Per-session cache affinity

Some OpenAI-compatible providers accept a **session identifier** in the request
body as a routing hint, so requests sharing it are more likely to reach the same
backend, whose prompt cache is then warm for their shared prefix. The hint's wire
shape is per provider, so it is modelled as a capability selected by the
`cache_affinity` scalar on that provider's `[provider.<slug>]` overlay entry
(`off`, `prompt-cache-key`, or `openrouter-session`). The value is always the
daemon's **real per-session id**, never a constant: a fixed key would pin every
session to one routing bucket and, on the models that bucket cache *accounting*
by the key, merely partition the accounting rather than improve routing. One
provider class is withheld outright — the **stateless Responses dialect**
documents no affinity field, and an unverified body field either 400s the turn or
is ignored — and the withholding is folded into the one provider-level
resolution both the cache-warming gate and the body builders read, so they cannot
disagree. The dialects, their model-generation semantics and their validation are
`choreo-ai-protocols/src/session_routing.rs`; the withholding and the layered
resolution are `openai/config.rs`.

#### Extended cache retention

A warm ping only helps within the provider's cache tier, and some providers
expose a longer one a request can ask for. Its wire shape varies across a
provider's model generations, so it is a **per-model** overlay scalar
(`cache_retention`) gated on the model's curated long TTL and never inferred from
a model-id string; the bundled overlay seeds no model. Values, gates, and each
dialect's restriction are `choreo-ai-protocols/src/cache_retention.rs`, with the
schema in `models-overlay.toml`.

#### Downgrade on a rejected field

The hint fails safe: a `(provider, model)` that answers a prompt-cache request
field with an HTTP **400 naming that very field** receives none of them for the
rest of the process, while a provider that merely ignores an unknown field never
downgrades. One registry (`cache_field_rejection::CACHE_FIELD_REJECTED`, the
**sixteenth** sanctioned shared-state exception) covers the **whole** prompt-cache
request surface — affinity and retention alike — and is keyed by the exact pair,
so a rejection on one model never withholds the hint from a sibling the provider
still honours. The detector's narrowness and the three-layer resolution
precedence (a per-account override outranks the catalog dialect, which outranks
the registry) are documented in that module and in `openai/config.rs`.

#### Cache warming reads both facts

Cache warming assumes its ping reaches a backend that **still holds the prefix**,
so with the hint withheld it could pay for a cache write the next real turn never
reads. The arming step therefore resolves the dialect the request bodies actually
send and declines to arm when the registry withheld it
(`SkipReason::AffinityUnavailable`, `choreo_cache_warm_skips_total{reason="affinity_unavailable"}`);
the same step resolves the retention tier so the ping schedule is computed
against the lifetime the cache really has. Both resolutions defer to the
resolvers the body builders use, so the ping can never disagree with what a
request sends (`choreo-daemon`'s `cache_warm::arming`).

### Provider-specific 400 handling

The retry layer consults the response body in exactly two provider-specific
cases, each gated tightly enough that the status-only "a 4xx is a deterministic
rejection" contract holds everywhere else. The opencode.ai zen/go gateway
validates message *content* on only some routes, so the identical bytes flip
`200/400/200` there; `retry_loop_with_provider_policies` resends the identical
body up to two more times when the slug is a known gateway slug — the same
exact-match set that gates the gateway routing headers, so the two cannot drift —
**and** the terminal `400`'s detail matches one of the gateway's own
content-rejection wordings or its opaque model-only stub (`retry/gateway.rs`).
Separately, a bare `400` naming no field that refuses a request over a tool call
would reach the operator with no cause attached, so the one funnel that turns a
provider error into an `InferenceError` appends a short clause (and tags one log
line with the matched family) for a known rejection wording
(`shared::TOOL_CALL_REJECTION_CLASSES`); an unrecognized body passes through
byte-identical, and nothing here feeds retries or any downgrade.

### Runtime catalog refresh (S4)

The merged catalog is parsed lazily into `PROVIDER_CATALOG`, a process-global
`LazyLock<ArcSwap<Vec<ProviderEntry>>>`: readers are lock-free, `replace_catalog()`
swaps the whole catalog atomically, and lookups therefore return *owned* values
cloned out of the atomic guard rather than `&'static` references. That
single-writer `ArcSwap` shape is the workspace's **fourth** sanctioned
shared-state exception (see AGENTS.md).

One maintenance thread (`choreo-daemon/src/catalog/`) owns the runtime pipeline —
the conditional GET against models.dev, the user-overlay reload, the attempt
cooldown — but never mutates the catalog: every change travels to the daemon
command loop, the sole writer of the `ArcSwap`, over a channel. The thread holds
**no copy of the base** between events, which leaves `catalog.bin`'s single
reader its single writer and makes the re-merge race-free. Because a provider
client bakes its catalog entry's protocol, base URL, compression coding, and
affinity dialect in at construction, a swap also invalidates — `DropProvider` —
only the sessions whose provider's *client-relevant* config changed or vanished,
so a routine model-list refresh leaves warm clients and their connection pools
alone. The pacing arithmetic, the cache/etag write ordering, the burst
coalescing, the fingerprint gate, and the `/refresh-models` and `CatalogUpdated`
wire variants are documented in-source (`choreo-daemon`'s `catalog` module and
`choreo-ai-protocols`'s `catalog::refresh` and `choreo-proto`).

### Client-facing MCP surfaces

`/mcp` shows the state of every MCP server visible to the session:
`ClientMessage::McpStatusRequest` is answered with one `McpServerStatus` per
server, and that record is what every frontend reads. It carries the slug, tier,
transport, target, connected state, tool count, the recorded last error, the
target-scoped keystore key (`mcp:<slug>|<target-hash>`), and an `McpAuthState`,
so a client derives its sign-in/sign-out action from the status alone instead of
holding a copy of the server's config. The daemon is the sole authority for that
key (one derivation feeds both provisioning and connect-time load, so the two
cannot drift), and the wire `oauth` block carries no literal secret: a
confidential client's `clientSecretEnv` names the environment variable that does.
A terminally rejected grant is quarantined, and the two surfaces treat it
differently on purpose — the connect-path snapshot withholds it, so a connect
surfaces a re-login rather than re-spending a spent token, while the status reads
the quarantine from the store and reports `Reauthentication`, so the server that
needs that re-login stays visible to the operator.

The requests beyond the status record are `McpReconnect` (rebuild one server — a
refreshed list on success, `McpReconnectFailed { slug, error }` otherwise),
`McpReload` (re-read the config and reconcile the running set with it —
`McpReloaded { summary, servers }` or `McpReloadFailed { error }`), and the
`McpTrust`/`McpUntrust`/`McpTrustList` trio that admits, revokes, or lists a
trusted project root. The connection thread translates each into a
`DaemonCommand`, because the command loop is the sole owner of the manager and
the tool catalogue. The variants and their replies are documented on
`choreo-proto`'s `ClientMessage`/`DaemonMessage`, the handling in
`choreo-daemon`'s `daemon::mcp_commands` and `mcp::trust`, and the user-facing
commands (`/mcp`, `/mcp reconnect`, `/mcp reload`, `/mcp trust`, and the
`choreographr mcp` CLI, whose offline `list`/`add`/`remove` edit `mcp.json` with
neither a daemon nor the `mcp` cargo feature) in the README.

**Remote-server OAuth** is the part that spans crates. The interactive PKCE
sign-in — discovery, dynamic client registration, loopback callback — belongs to
the frontend (`choreo-client-core`'s `oauth::mcp`), the durable
`ServiceCredential::OAuth { kind: Mcp }` is stored under the key above, and
connect-time refresh is `rmcp`'s own `AuthorizationManager`, driven through
`choreo-mcp`'s `auth` module, which keeps every `rmcp` type on its side of the
crate boundary and exposes only its own `CredentialBackend`. Two policies are
contractual rather than incidental: a load is **fail-closed** on the canonical
target and on the issuer, so a re-pointed server never receives a token minted
for another; and trust gates *use*, not existence — an untrusted project's
stored grant is neither loaded nor sent, yet survives un-trust and works again on
re-trust with no new login.

The daemon's backend reads the command-loop-published credential snapshot rather
than round-tripping the loop, which would deadlock a connect that runs while the
loop is blocked resolving a session overlay. Its writes are fire-and-forget
`DaemonCommand`s, and each one first advances that snapshot synchronously, so a
concurrent refresh of the same grant re-reads the rotation instead of spending an
already-rotated single-use token. That two-writer read view is the **fifteenth**
sanctioned shared-state exception (AGENTS.md): a single-key mutation publishes
only its own key, a whole-map republish is reserved for the two wholesale changes
(lock clears the set, unlock decrypts it afresh), and the slot is re-installable
so a restarted embedded daemon gets a fresh snapshot and its own command sender.
It carries no per-message data — only credential values the command loop already
owns.

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

### Reasoning round-trip (capture → carry → re-emit)

For Anthropic (thinking blocks + signatures), DeepSeek/Kimi (`reasoning_content`),
Gemini (thought signatures), and OpenAI/xAI Responses (opaque reasoning items +
`previous_response_id`), reasoning must be sent back on the next request or the
tool-call loop fails with a 400. The round-trip payload is an **opaque,
provider-owned artifact** whose custody is split four ways: the catalog decides
*how* to send (the per-model passback policy, else the protocol default, plus the
chat echo policy); each adapter captures the artifact verbatim at its parse
boundary and re-emits it verbatim in its own wire format; the daemon's request
builder (`choreo-daemon/src/reasoning.rs`) decides *whether* to send — derived,
never configured, from same-model provenance plus the passback policy plus the
`previous_response_id` continuity chain — and never interprets the bytes; and the
chat adapter's lowering decides *which wire key* carries it. The artifact never
leaves the daemon process: client-bound `SessionEvent`s carry it as `None`.

Whether a Responses route may chain is a **provider-level catalog fact**
(`responses_stateless`), read through the catalog rather than an adapter-side slug
list, and the catalog test `every_stateless_provider_replays_rather_than_chains`
fails the build if a model on such a provider resolves to
`ReasoningPassback::ResponseId` — the stateless dialect cannot chain, so it must
replay its captured items instead. The remaining rules (which key wins, the
structured `reasoning_details` array, the single reasoning byte budget shared by
capture and replay, and the empty-turn predicate) are the owning modules'
rustdoc: `catalog/entry.rs`, `openai/chat_projection`, and
`openai/reasoning_details`.

### Supported providers

The full provider list, its count, and the protocol each entry speaks are the
README's user-facing catalog overview. What stays here is a scope decision that
sits between the catalog data and the daemon's credential model — which known
providers the catalog deliberately does not offer:

> **Note — providers present in agent catalogs but deferred from the daemon catalog**
> (present in models.dev, catalogued for their model lists but with no API
> endpoint / no API-key path in the current catalog):
>
> | Provider(s) | Reason deferred |
> |---|---|
> | `amazon-bedrock`, `google-vertex`, `azure-foundry`, `azure-openai-responses` | Multi-field credentials (AWS keys/region, GCP service account, Azure resource + key). The daemon's single-API-key credential model cannot represent them yet. |
> | `copilot`/`copilot-acp`, `qwen-oauth`, `kimi-code` OAuth, `github-copilot` OAuth, `radius` | OAuth-only / dynamic-catalog auth — no static API-key path. The catalog carries an `oauth` profile mechanism and a per-slug client-identity table (see Provider auth above), and the provider OAuth **sign-in and refresh** flow ships for the `chatgpt` (Codex) and token-sharing `openai` routes; these remaining providers are added through the catalog `oauth` profile and the client-identity table as data. |
> | `cursor` | t3code subprocess driver (spawns the Cursor CLI), not a direct HTTP provider. |
>
> Adding the multi-field-credential providers requires a credential generalization (a `Fields` credential variant); the OAuth providers are added through the catalog `oauth` profile and the client-identity table, not a code change.

### Daemon client & request topology

One OS thread per client connection — reading `ClientMessage`s off the socket,
sending `DaemonCommand`s over one crossbeam channel, and writing that client's
`DaemonMessage` receiver back to the socket — plus one command thread that owns
`DaemonState` exclusively, so no thread ever shares daemon state and all
cross-thread flow is messages. Per-session threads own their `SessionState`
exclusively, are spawned on `CreateSession`, and are signalled on `Shutdown`.
The assembly (`core.rs` builds the transport-independent core, `lifecycle.rs` the
accept/signal/metrics adapters and the shutdown drain) is documented in
`choreo-daemon/src/server/`, and the tool-call loop the command and session
threads drive in `choreo-daemon/src/requests.rs`.

## Token Usage & Context Window Tracking

Token usage is the protocol-agnostic `TokenUsage` struct
(`choreo-proto/src/types/common.rs`), whose two operators divide the work:
`merge_max` keeps a monotonic per-field maximum across repeated snapshots of the
same cumulative figure, while `accumulate` sums one turn's usage into a total
rebuilt from the turn history. For every provider `input_tokens` is the **full**
prompt count and the prompt-cache counts are a **subset** of it — priced
separately, so tracked on their own — folded in at the adapter boundary by a
provider whose raw input counter excludes cache.

Two accounting contracts cross crates. **Billing versus context:** each session
tracks an `accumulated_usage` total that sums **every** turn (undo does not refund
billed tokens) and a separate `last_prompt_tokens` derived from the **live** turn
set for the context-window readout, so `undo`/`redo` recompute it and re-broadcast
a `TokenUsageUpdate` (a `None` clearing the readout when every turn is undone).
**Mid-turn freshness:** the request worker accumulates on a private clone and
routes its total through `SessionCommand::SyncAccumulatedUsage` to the session
thread, which applies it as a per-field max — so an out-of-order sync can never
regress a total a client already saw — re-broadcasts from the authoritative
state, and refreshes the daemon's metadata index without a `last_modified` bump;
because `last_prompt_tokens` is not cumulative, clients gap-fill it from
snapshots and never overwrite a fresher value. The per-adapter extraction and the
context-window resolution chain are documented in-source (`choreo-daemon`'s
`requests`, `sessions`, and `providers` modules and `choreo-proto`).

---

## Security model

### Local trust domain (the Unix socket)

The Unix-domain socket is the daemon's **local trust domain**: it carries no
Noise handshake, so any peer that reaches it is trusted exactly as the daemon
owner is — it can read session transcripts, retrieve plaintext credentials
while the keystore is unlocked (`GetCredential`), run tools, and enroll other
clients (`/acl add`, which is refused on the TCP/Noise listener). Transport
authentication cannot be what protects it; the operating system's file
protections are, in three layers: an owner-only (`0700`) parent directory, a
`0600` socket file narrowed immediately after `bind(2)`, and a Linux/Android
`SO_PEERCRED` peer-uid check that rejects a foreign uid (and a socket whose
credentials cannot be read) before a single message is read. A directory owned
by another user is refused outright — never merely warned about — because that
user could enumerate or swap the socket path; a *pre-existing shared* parent
(`$XDG_RUNTIME_DIR`, the platform temp dir) is never re-moded, since narrowing
it would be destructive.

Each layer's rationale, the bind-then-chmod window, the per-uid temp-dir
fallback (and why the path has one definition in
`choreo_shared::paths::default_socket_path`, delegated to by `choreo-proto`),
and the shared `choreo_shared::perm` policy behind the modes are documented
in-source (`choreo_shared`'s `perm`/`paths`, `choreo-daemon`'s
`server/lifecycle` and `server/connection`).

### On-disk state database

The `redb` store (`state.redb`, plus every `state.redb.bak-v*` pre-migration
snapshot) holds plaintext conversation history, tool output — which routinely
carries secrets — and image attachments, so it is protected by the filesystem
**independently of the keystore's AES-GCM credential blobs**, which are the only
encrypted-at-rest records in the suite. The same shared policy applies: the
*immediate* parent through `ensure_private_dir` (`0700`, foreign owner refused
fatally), the file itself forced to `0600` on every open path and after every
backup copy (a typical `redb` create honours the umask and lands `0644`). A
shared ancestor is never narrowed, and any tightening failure other than a
foreign-owned directory only warns — losing the daemon over a permission quirk
is worse than a visible log line.

The per-path decision (which directories are shared roots, the open/create/
recreate call sites, the backup copies) is documented in-source
(`choreo-daemon`'s `db` module).

### Lock/Unlock flow

Each daemon's credential keystore is governed by a keypair whose private half (the
**unlock key**) is held client-side, one per daemon, in the client's
`known_servers.toml`. The daemon stores only the public half as a **binding**, and
it starts **locked** with no credentials in memory. The contract between the two
halves is deliberately narrow: `ClientMessage::BindKeystore` is the ONLY path
that can create a binding — it ADOPTS the presented key on an unbound keystore
(TOFU, loud `KEYSTORE BOUND` log) and merely verifies it on a bound one — while
`Unlock` is strictly VERIFY-ONLY, answering `KeystoreUnbound` (deliberately
distinct from the bound-but-wrong-key `LockedError`) rather than ever adopting.
Binding is TOFU-once: no rotation is implemented, and re-binding a daemon whose
DB was reset is what the next connect's auto-bind does.

`AddCredential` carries the credential as a TYPED plaintext value and no key at
all, and requires an already-unlocked keystore (a locked daemon refuses it rather
than unlocking implicitly); the daemon encrypts each credential to its own
binding before persisting, so provisioning and refresh rotation share one
blob-write path that never needs the private key. Because the client records a
per-daemon unlock key only after a targeted confirmation (except the auto-bind
key, which must be recorded pre-send), one ordering guarantee crosses both
halves: **the targeted reply to `Unlock`/`BindKeystore` reaches the acting
client's writer queue before the lock-state transition broadcast**, so the
broadcast can never overtake it.

**Multi-client provisioning** follows from those rules: the first client to
reach a fresh (unbound) daemon auto-binds it with a freshly minted key, and any
other client stays locked until that key is shared out of band (added to its
`known_servers.toml`, or supplied with `/unlock <base64-key>`).

The per-side mechanics — the `unlock_tail`/`finish_unlock` ordering that leaves a
failed unlock's plaintext unpublished, the broadcast-to-all-subscribers and
send-on-subscribe `Keystore { state }` pushes, the write-free `/unlock` key path
and pre-send bind record, the `known_servers.toml` hardening, and the lazily
rebuilt per-session providers — are documented in-source (`choreo-daemon`'s
`daemon/keystore` and `daemon/subscriber_handlers`, `choreo-client-core`'s
`credentials` and `known_servers`, `choreo-keystore`'s `crypto`).

### Dependency supply chain

The workspace ships Rust that must trust its third-party dependencies, so the
dependency graph is part of the security surface: the 2026-08-20 `arrayref`
republishing made that concrete. Four layered controls make a repeat fail loudly
instead of landing silently, and three repository files carry them:

- **`Cargo.lock` + `--locked`** — a silent re-resolution against the live
  registry fails the command instead of succeeding quietly.
- **`deny.toml`** (`cargo-deny`) — the attacker's versions banned by name, a
  crates.io-only source restriction, and advisory scoping (the one accepted
  vulnerability is a documented, reachability-justified `ignore`).
- **`scripts/check-supply-chain.sh`** — the cache scan for the DELETED `.crate`
  files that no graph tool can see anymore, then the policy check, with a
  `cargo-audit` + literal lockfile-scan fallback.
- **The shared RustSec advisory database** — both tools read it, so
  re-introducing an attacker crate fails the gate even without an explicit ban.

This file states no version numbers of its own: `deny.toml` and the script are
the authoritative ban list, and their headers carry the incident rationale. The
strongest remaining option — bit-for-bit reproducible builds from a checked-in
dependency snapshot via `cargo vendor` + a `[source]` replacement — is
intentionally not enabled (repository size).

The MCP path's heaviest third-party dependency, `rmcp`, is pinned and limited to
the transports it needs (`default-features = false`), and the whole tree
resolves a single `reqwest` major — both declared in the workspace `Cargo.toml`,
with `deny.toml` keeping `multiple-versions = "warn"` so a future divergence is
reported rather than silently duplicated.

### MCP client trust boundary

The MCP client treats every server as untrusted and every server-supplied string as
data, never as instructions or configuration. The boundary is enforced at four
sites, each documented where it lives: schema bounding and catalogue caps in
`choreo-mcp`'s `protocol`, header/transport discipline and capability honesty in
its `engine`, process safety and environment hygiene in its `stdio` (with the
injection-variable list shared with the shell tool through `choreo-sanitize`'s
`child_env`), and least privilege in the daemon, where `ToolPolicy::Mobile`
registers no shell, sandbox, or MCP group at all.

Two consequences belong to the system view rather than to any one module. An MCP
server runs with the daemon user's authority, so the daemon's environment is NOT
a secret channel: the code-injection variables are stripped but everything else is
inherited, and anything a server needs must be set through that server's own
`env`. And credentials are per-server: a static token in `headers`, or a
keystore-stored OAuth grant keyed to the canonical target (`choreo-mcp`'s
`auth::keying`), so a re-pointed project `.mcp.json` cannot be handed an existing
grant.

### MCP connection lifecycle (off the command loop)

Connecting an MCP server is I/O (spawn, handshake, `tools/list`, OAuth refresh), so it
never runs on the daemon command loop and never before a client's acknowledgement. The
command loop's only MCP job is to install a finished result and swap the tool catalogue
(the sanctioned single-writer `ArcSwap`, exception #4); the handshake itself runs on a
worker thread that posts a `DaemonCommand` back. That holds for all three phases —
startup (which only queues the daemon-tier `shared` servers so the socket is bound
and the first client served immediately), the post-unlock resync that retries the
OAuth servers whose first connect ran against a still-encrypted grant, and the
per-session overlay connects — and each phase's mechanics (the install/generation
check, reply parking, connect coalescing, per-entry result posting, and the
in-place rebuild reserved for project/per-session servers) are documented in-source
(`choreo-daemon`'s `daemon::mcp_commands` and `mcp` modules, `choreo-mcp`'s
`session`), held complete by the `#![warn(missing_docs)]` + `doc-check` gate.

---

## Tool system

The daemon's tool machinery — the typed `Tool` trait, its type-erased `ToolDyn`
counterpart, the `ToolRegistry` that owns and dispatches every tool, and the
`define_tool!` macro — is documented in-source (see `choreo-daemon`'s `tools`
module, `cargo doc -p choreo-daemon` / `just doc`), held complete by the
`#![warn(missing_docs)]` + `doc-check` gate.

Three dispatch paths cross the tool boundary, and the split is a contract, not
an implementation detail: two JSON paths (`ToolDyn::execute_json` and
`execute_streaming_json`) serve LLM tool calls and return a `ToolOutput` rendered
as `Text` (human/LLM) or `Json` (Programmatic Tool Calling), while
`ToolDyn::execute_postcard` serves RISC-V VM guest ecalls and answers in postcard
bytes — the guest↔host wire format described under "Postcard binary encoding"
below. A tool's `Error` type stays structured per tool so a guest can match on its
own variants (`DbError::NotFound`, `HttpError::InvalidUrl`, …), and a tool that
produces an image carries it on its typed return value, drained by the request
worker over an out-of-band multi-message channel (an MCP result may carry several
image blocks) rather than embedded in the response struct.

### Tool output sanitization and bounding

Tool output is defended in three layers at once — at the **source** (the tool
that produces the bytes), at the **transcript** (what the model sees on the next
call), and at the **sink** (the terminal that renders it) — and all three sit on
one Unicode policy and one byte budget so they cannot drift. `choreo-sanitize`
owns those shared primitives; each layer's per-tool application, including the
ordering rule that keeps a critical tail alive through the transcript's
post-sanitize re-cap, is documented where it lives (`choreo-daemon`'s
`tools/sanitize.rs` and its transcript choke point, `choreo-tui`'s
`text_filter`).

**Content is capped; delivery is bounded separately.** The byte budget caps one
tool's content at the source and in every client's live accumulation, while the
lossless delivery design separately bounds the per-client in-flight bytes that
trigger lag-eviction (see the `choreo-daemon` `broadcast` module docs). The two
do not collide — content is capped at the source, delivery per client queue.

### PDF tools

`pdf_classify` and `pdf_to_markdown` wrap `pdf-inspector` (Firecrawl), a
pure-Rust, extraction-only parser built on `lopdf`: with no JavaScript engine
and no page rendering, the classic PDF malware *execution* vectors are excluded
by construction. Input gating, the untrusted content framing, and the
containment story are documented in-source (`tools/pdf/`). The dependency's
RUSTSEC-2026-0187 fix (`lopdf >= 0.42`'s nesting cap, with the regression guard
`nested_array_poc_does_not_abort_process` still in
`tests/it/pdf_tool_integration.rs`), the `pdf` feature's placement in `default`,
and the iOS embedded build's opt-out are explained in
`choreo-daemon/Cargo.toml`.

### Postcard binary encoding

Tools reach the RISC-V sandbox over a `postcard` binary protocol. The frame
shapes, the args encoding, and the nested `Result<Result<R, E>, ToolError>`
return encoding — the outer layer for infrastructure failures (arg
deserialisation), the inner for the tool's own error type, so a guest can
pattern-match its variants — are documented in-source (`choreo-daemon`'s `tools`
module and `tools/vm.rs`).

### Tool groups

Tools are grouped to reduce context overhead, and each declares its group via
`Tool::group`. The group catalog — each group's name, its model-facing
description, and its cargo-feature gating — is in-source (`choreo-daemon`'s
`tools` module), as is the default active set (`core`, `git`, `shell`) with the
session-config default in `sessions`. The system prompt lists every group and
its description; the model activates and deactivates them with
`load_tools`/`unload_tools`, and **core** cannot be unloaded.

Groups affect only tool **availability** in the API `tools` array — they are a
discovery mechanism, not access control. The RISC-V VM (`run_riscv`) reaches
every registered tool regardless of group state.

### Concurrent tool dispatch

Each response's tool calls are partitioned into a **serial (session-config)**
phase — `load_tools`, `unload_tools`, and `set_working_dir` — and a
**concurrent** phase holding every remaining tool (shell, filesystem, VM, HTTP,
Git, `spawn_subsession`, …) whose execution is independent of session state. The
typed `PendingConfigChange` capture, the `is_session_config_tool` predicate, and
the Phase-3 worker-copy mirror that carries successful mutations onto the
request worker's config copy live in `requests/config_tools.rs`; the dispatch
and execution machinery — thread topology, per-tool timeout resolution, the wait
loops, the streaming forwarder, and per-tool result recording — lives in
`requests/tool_execution.rs`.

Three properties hold across both phases however the races fall. Cancellation is
fully event-driven (every wait is a `select_biased!` that lists the cancel arm
first, over the result channel, the cancel channel, and an exact
`after(remaining)` timer — never a poll loop), and a cancel that wins a race
still **drains** an already-completed result rather than discarding it, so the
transcript keeps the tool's real output while the request still stops on the
sticky flag. A cancel observed by the concurrent collector sends every
still-running wait-loop a per-tool kill and keeps draining until all batch
handles have arrived, so each unfinished call records a deterministic outcome
and the placeholder sweep (`mark_unexecuted_tool_results`) stays only a safety
net for a wait-loop that dies before delivering. Tool results are always
rendered in the model's original call order — placeholders are seeded in call
order and filled **in place by `call_id`** — and a tool thread that panics is
caught and reported as an error `ToolOutput` instead of crashing the daemon.

Both phases share a `ToolContext.cancelled` flag with the running tool so a tool
that consults it can stop early — the one sanctioned shared-state exception to
the repo's channel-only thread-communication rule (see AGENTS.md).

### spawn_subsession

`spawn_subsession` is a core-group tool on the concurrent dispatch path: it asks
the daemon command loop for a child session (`parent_session_id` recorded;
working directory, model, and tool groups inherited), drives the child's own
agent loop to completion under the daemon-wide `max_turns` cap, and returns the
child's assistant text. Its contract is in-source (`choreo-daemon`'s
`tools/subsession.rs`). The daemon tracks the parent→child relationship
(`DaemonState::children`) so a parent's cancellation, exit, or deletion also
stops its children — see `cancel_children_of` and the cascade-deletion docs —
and the TUI's navigation round trip is documented in `choreo-tui`'s
`state/subsession.rs`.

### `run_riscv` — RISC-V sandboxed code execution

The guest runs in a `ckb-vm` RISC-V machine with no host syscall passthrough: it
cannot reach host memory, files, or the environment except through the choreographr
ABI's tool calls, and every one of those dispatches through the same `ToolRegistry`
the host agent uses — with the caller's `x_credentials`, `working_dir`, and
`ToolContext` — so a guest inherits exactly the authority of the session that ran
it. Dispatch is by registry name, so group state is discovery, not access control
(see "Tool groups"). The compile-and-run flow, the guest ABI and syscall set, and
the ckb-vm memory/cycle bounds are documented in-source (`choreo-daemon`'s
`tools/vm.rs`).

### `sh` — POSIX shell command execution

The shell/exec tools (`sh`, `nushell`, `fish`, `powershell`, `exec`) share one
sandbox contract, implemented in `tools/shell_util.rs` and documented in-source:
a watchdog kills the child's process tree on timeout; the child's environment is
stripped of the canonical injection variables (one shared list, the same one the
MCP stdio transport strips); stdin is not connected, so a command that reads from
it hangs until the timeout; and output is bounded by the shared 128 KiB cap.
**No confinement is applied today** — neither in-process path checks nor resource
limits — so a spawned child can reach whatever the daemon's own user can. The
intended boundary is OS-level (Landlock on Linux, Seatbelt on macOS), still
planned per README → Future Functionality → Sandboxing. The shell the `sh` tool
runs under is resolved once at daemon startup (see `tools/shell_resolver.rs`).

On Windows the same helpers swap in the platform analogues: `setup_child` has
no process group to create, so the child is assigned to a Job Object
(`ChildJob`, created right after spawn — `Command` has no pre-exec hook — with
`JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE` making the handle a whole-tree kill
switch), and the watchdog's timeout kill is gated on a `ProcessIsAlive` probe
(a zero-timeout `WaitForSingleObject` on a copy of the child's process handle)
so a child that already exited is not misreported as killed — the Windows
analogue of the Unix pidfd/ESRCH check. The `Arc<ChildJob>` and `ProcessIsAlive`
handle copies are the fifth sanctioned shared-state exception (see AGENTS.md
#5): a `HANDLE` is a kernel-table index, not an address-space pointer, and every
Job Object operation is thread-safe kernel-side, so the watchdog and drain
threads share one immutable value whose handle is closed exactly once by `Drop`
on the last owner. The kernel-handle rationale is in `choreo-daemon`'s
`tools/shell_util.rs`.

---

## Session architecture

Sessions are persisted to a `redb` embedded key-value store at
`~/.local/share/choreographr/state.redb`, and each active session's in-memory state
is a `SessionState` owned by its control thread with the persisted fields extracted
into `SessionConfig`. The table schema (every key and value codec), the
`SessionState`/`SessionConfig` split with every field's rationale, and the store's
owner-only filesystem permissions are documented in-source (see `choreo-daemon`'s
`db` and `sessions` modules, `cargo doc -p choreo-daemon` / `just doc`); the
permissions themselves are summarized under **On-disk state database** in the
Security model.

### Turn durability

Each `Turn` is written when the **agent-loop iteration that produced it**
completes, not when the request does: a request is not one turn, and its tool
loop can iterate unbounded for tens of minutes, so the request is far too coarse
a durability unit. The iteration boundary is the first moment a turn is complete
(assistant message plus one recorded result per call) and it precedes the next
provider round-trip, so a checkpoint there bounds the crash window to the single
turn in flight. The worker builds its turns on a private `SessionState` clone
and the session thread's merge at `RequestFinished` stays the authoritative
final write; the checkpoint itself, that merge's `persisted_turns` re-write
skip, and the swallow-and-retry handling of a failed checkpoint are documented
in-source (`choreo-daemon`'s `requests/tool_execution.rs`, `sessions/state.rs`,
and `sessions/handlers/inference.rs`).

Three invariants hold around that boundary:

- **A turn is written whole or not at all.** One `write_turn` transaction covers
  the zstd-compressed blob and every attachment slot, so a reader can never see a
  turn with some of its images. Image bytes are additionally written per emit
  (see the image flow) because a client fetches them by key the moment the
  turn's `TurnAppended` arrives.
- **A committed turn is durable.** `redb` write transactions default to
  `Durability::Immediate` and the daemon never lowers it, so a committed turn has
  been fsynced (see the `db` module docs).
- **Every request ending persists.** Completion, inference failure, and
  cancellation all finalize the turn and route the worker's snapshot through
  `RequestFinished`, so a graceful shutdown (which cancels in-flight requests)
  loses nothing; only process death, which cannot cancel, skips the paths that
  would have written.

**Crash-recovery skew.** Durability covers the turn rows, not every field of the
worker's `SessionState`: the worker-owned fields (`last_response_id`, the
continuation-chain head, and `accumulated_usage`, the billing total) change on
that private clone and merge only at `RequestFinished`, so after a mid-request
crash the on-disk turns can be newer than they are — config a tool changes
through the session's own commands is persisted when the command is applied, so
only the worker-owned fields lag. On restart the session is rebuilt from the
turn rows: `next_turn_id` comes from the maximum turn id (never the record's
advisory `turn_count`), the billing total and context-size readout are
re-derived from the per-turn usage those rows carry, and the stale
`last_response_id` is restored only when its producing provider+model still
matches — a `ResponseId` provider then continues from that older head with the
checkpointed turns re-sent as new input. That is the accepted cost of turn-level
durability: the conversation itself is never lost.

### Schema versioning & migrations

The `meta` table holds the persisted schema version, and the daemon runs the
migration chain immediately after opening the database on every startup. The
policy — an **additive** change needs no migration, a **breaking** one gets a
numbered migration and a `SCHEMA_VERSION` bump — and the contract every entry
must satisfy (a chain contiguous over `1..SCHEMA_VERSION`, one write transaction
per step, idempotent under re-run, old record shapes decoded through frozen
local copies), with the pre-migration `bak-v{from}` backup and the downgrade
refusal, are documented in-source (`choreo-daemon`'s `db` module).

### Undo/Redo

`/undo` and `/redo` mutate turn history through the `undone` flag on each `Turn`
(the struct itself is documented in-source in `choreo-proto`; the daemon strips the
opaque reasoning payload from every client-bound `DaemonMessage`, and the builder's
same-model provenance check drops it after a mid-session model switch — see
**Reasoning round-trip**). Two contracts govern the pair:

- **Redo is LIFO-symmetric with undo.** A single `/undo` hides the most recent user
  turn together with every still-live turn after it (a tool-loop iteration creates
  several turns per user turn) and records only the turns it actually flipped as
  one **redo batch**; `/redo` pops the newest batch and restores exactly it, so one
  redo re-applies one undo and never collapses several pending undos at once. A new
  user turn clears the stack, and undo also clears the persisted `last_response_id`
  so a chained request cannot leak the hidden turns back into the model.
- **Both commands require an idle session.** An in-flight request worker holds the
  turn history on a private clone and merges it back at `RequestFinished`, so a
  history mutation underneath it would race that merge (and could resurrect the
  cleared chain). The daemon refuses a busy session with
  `Failed { error: "session is not idle" }`; the TUI applies the same idle check
  client-side to skip the doomed round-trip.

### Cancellation and connection accounting

Two sanctioned shared-state exceptions (see AGENTS.md) underpin session
cancellation and the daemon's connection accounting. Each session owns a
`choreo-sockreg` registry (the **seventh** exception) — plus one daemon-level
registry for non-session-scoped work (prefetch, catalog fetch) — so the command
loop can force-close a wedged session's provider sockets from the thread that
decided the cancel: `shutdown(fd, SHUT_RDWR)` is the only way to un-block a
worker blocked in a provider `read()`, and the registry's lock is held only for
a handful of fd syscalls. The daemon's live-connection counter (the **third**
exception, released by the RAII `ConnectionSlot`) enforces
`MAX_CONCURRENT_CONNECTIONS` atomically across every accept path — a bound no
channel can express without a dedicated accounting thread. Both rationales live
on the types (`DaemonState::session_registries`, `server/lifecycle.rs`).

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
> requested endpoint. The integration test (`tests/it/metrics_integration.rs`)
> is gated with `#![cfg(feature = "metrics")]`, and `cargo test-lean` — the
> feature-off unit run — is what keeps the no-op stubs and the `--metrics-addr`
> refusal path compiled: the `--all-features` test aliases never build that
> configuration.

**CLI flag:** `--metrics-addr <ADDR>` (e.g. `127.0.0.1:9464`). When the flag is
absent no metrics server is started — the daemon runs exactly as before.

**Endpoint:** `GET /metrics` returns `Content-Type: text/plain; version=0.0.4; charset=utf-8`.

The full metric set — names, types, labels, and the exported help text — is documented
in-source in the daemon's `metrics` module, where every `record_*` function names the metric
it updates (`cargo doc -p choreo-daemon --features metrics` / `just doc`), held complete by
the `#![warn(missing_docs)]` + `doc-check` gate.

---

## Data flow: a prompt from input to response

```
1. User types "hello" in choreo-tui
        │
2. choreo-client-core::shell::parse_input_line("hello")
   → ClientMessage::request(id, ClientMessageType::RunInput { input: "hello" })
        │
3. choreo-tui's `PendingReplies::send` allocates the per-connection `id` and frames the message; the connection writer task forwards it → Unix socket → choreographr
        │
4. choreographr server.rs handles RunInput:
   - validates session exists and is attached
   - hands the request to the session thread, which assigns the run's `stream_id`
     (a per-session, monotonic counter — the client never chooses one)
   - sends BOTH the targeted acceptance reply
     DaemonMessage::reply(id, DaemonMessageType::Session { session_id: Some(1), event: SessionEvent::Started { stream_id, turn_id, .. } })
     and the identical broadcast Started (`id: None`) to every session subscriber
   - appends SessionMessageKind::UserText("hello") to session
   - calls requests.rs to execute
        │
5. requests.rs builds message array from session history
   → calls openai::chat_completions or openai::responses (based on request_format_for_model)
        │
6. openai::chat_completions / openai::responses streams SSE chunks
   → per chunk: DaemonMessage::broadcast(DaemonMessageType::Session { session_id: Some(1), event: SessionEvent::OutputChunk { stream_id, stream: true, data: "Hello" } })
        │
7. DaemonMessage is serialized + framed → socket → choreo-tui
        │
8. choreo-tui reader task receives OutputChunk
   → pushes to UI event stream (routed by the `stream_id` learned from Started)
        │
9. UI loop consumes event → updates ClientHistory → re-renders
        │
10. Final chunk arrives → DaemonMessage::broadcast(DaemonMessageType::Session { session_id: Some(1), event: SessionEvent::Done { stream_id, .. } })
    choreo-tui marks request complete, adds session message
```

### Image flow (tool-triggered)

An image a tool produces (a `display_image`/`generate_image`/`edit_image` call, a
`retrieve_webpage` screenshot, an MCP image block) is delivered to subscribers
out-of-band — a tool can emit several, so an image cannot ride the `ToolOutput`
value — and is persisted to `session_attachments` at emit time, so a client's
on-demand `GetImage` never races the turn's final write. The tool→session→client
plumbing (the `Tool::extract_image` hook, `emit_image`, and the mid-turn
`TurnAppended` broadcast) is documented in-source (see `choreo-daemon`'s `tools`
and `requests` modules, `cargo doc -p choreo-daemon` / `just doc`), held
complete by the `#![warn(missing_docs)]` + `doc-check` gate.

### Vision input flow (`read_image` → model)

Vision input is the mirror pipeline — images sent *to* the model rather than
*from* it. `read_image` decodes and normalizes a source image to
provider-allowlisted PNG/JPEG (every `image`-crate raster format, SVG via
`resvg`, HEIC/HEIF, and feature-gated AVIF), reports a text handle, and carries
an `ImageReference` whose normalized bytes the framework stores on the turn's
tool result and persists in `session_attachments`. At request-build time the
bytes are re-attached to a **synthetic user message** appended after the turn's
tool messages (preserving `tool_use → tool_result` adjacency), under two gates:
**decay** (only turns belonging to the request currently in flight attach pixels;
older turns replay a path-naming placeholder, never converting back) and
**vision** (`catalog::model_supports_vision` — no bytes on a text-only model).
The normalization (`choreo-daemon`'s `image_prep`) and the decay/vision gates
(`reasoning.rs`'s `assemble_request_messages` / `tool_result_image_messages`)
are documented in-source (see `choreo-daemon`'s `image_prep` and `reasoning`
modules and `choreo-image`, `cargo doc -p choreo-daemon` / `just doc`), held
complete by the `#![warn(missing_docs)]` + `doc-check` gate.

### Attaching to a run in flight

A client that attaches to a session with a request in flight is replayed a
synthetic `Started` for that run, so it can route the chunks still to come into
the right turn (without it those chunks are dropped — the client has no
`stream_id → turn_id` mapping for a run it never saw begin). One agent run is a
*tool loop* that appends a turn per iteration — the user's prompt turn first,
then one turn per model/tool round — so the replay must carry the turn the run
is streaming into **at that moment**, never the run's first turn. The session
thread keeps `ActiveRequest`'s turn id and prompt-token estimate current from the
worker's per-iteration `Started` (so a joiner's context-size readout resumes at
the live value instead of a zeroed one), and both the acceptance broadcast and
the replay are built from that one record through `SessionEvent::started`, so a
field added to the event cannot be announced to subscribers and silently
defaulted on the replay path. A client additionally refuses to route a stream
*backwards*: turn ids are per-session monotonic and never reused (a wire
contract, documented on `SessionEvent::Started`), so a `Started` naming an
earlier turn than the one a `stream_id` is already routed to can only be a stale
replay, and honouring it would append everything still to come to a turn the run
has already left — in practice the user's own prompt turn, several turns above
the one on screen. The daemon is authoritative and never sends a rewind, so that
client guard is defence in depth; it is also why every place that mints a turn
id derives it from the highest id already allocated (never from a turn count).

### Broadcast fan-out (lossless delivery, lag eviction)

All three subscriber fan-outs — the all-activity broadcast, the summary
broadcast, and the per-session broadcast — share ONE policy through
`crate::broadcast::SubscriberSink`, so they cannot drift: every subscriber's
writer channel is unbounded (an enqueue can never be full), so the daemon never
drops a broadcast message and a slow subscriber can never stall the
single-threaded command loop or a session thread; delivery is guaranteed,
in-order, and exactly-once for every connected non-evicted client. Memory is
bounded by LAG-BASED EVICTION instead of drops — the crossing message is still
enqueued, and the outcome only tells the caller to evict the lagging client. The
policy, `LagLimits`, the soft-threshold rationale, the counter-balance invariant
with its one bounded exit-window exception, and the solicited-`Image` carve-out
are `choreo-daemon`'s `broadcast` module docs.

Two facts cross layer lines. **Eviction needs no daemon-held socket handle**:
each connection's writer gets a socket write timeout, so a wedged client cannot
stall its writer forever — the write fails, the writer shuts the socket down
itself, and the reader's blocking read unblocks into the normal teardown (see
the `choreo-daemon` `server` module docs). And **evictions are not silent**:
each one increments the `choreo_evictions_total` counter served on `/metrics`,
so a permanently wedged subscriber stays observable.

Four sanctioned shared-state exceptions (see AGENTS.md) sit behind this design;
each is a single-purpose lock-free counter or registry whose rationale lives on
its own type docs: the delivery-lag byte counters
(`SubscriberSink.bytes_in_flight` plus the daemon-wide `global_lag`, the
**sixth**), the process-unique `ClientId` minted from a monotonic `AtomicU32`
(the **tenth** — written there in `broadcast`'s `ClientId` docs),
`choreo-shared`'s `paths::TEST_LOG_DIR` test override (the **eleventh** — on its
own docs), and the per-file mutation locks (`tools::file_locks::FILE_LOCKS`, the
**twelfth** — whose module docs also state the single-path key's two deliberate
gaps).

---

## Terminal integration (`choreo-tui`)

The client talks to whatever terminal it was launched in, and the terminals
users actually run differ in which escape-sequence families they honour. The
in-source rustdoc of `choreo-tui/src/terminal/` is the reference for each
sequence, and for the single launch-time decision that governs which of them the
client emits: a `TerminalProfile` identified **once** per process
(`terminal/profile` — a pure `detect` over an injected environment, the compile
platform and the image protocol `ratatui_image`'s picker negotiated), stored
immutably on `App`, and consumed by every later phase rather than re-detected —
the background image encoder, the OSC 7501/9;4/OSC 22 emission gates, the
welcome notice, the Terminal page, and the hosted-pane reporter. Each consumer
documents its own rules beside it (`state/terminal_notice`,
`state/terminal_advice`, `state/terminal_status`, `render/terminal_help`,
`render/help`), held complete by the `#![warn(missing_docs)]` + `doc-check`
gate.

---

## Design decisions

1. **Unix sockets, not HTTP for client↔daemon** — keeps everything local, avoids
   port conflicts, leverages OS-level access control.

2. **Binary protocol (MessagePack, named mode), not JSON** — self-describing,
   compact, and typed, over a length-prefixed frame (so framing is unambiguous)
   whose version gate rejects a mixed-version peer at decode rather than letting
   it misread a payload.

3. **Lock/Unlock security** — the daemon starts with no credentials in memory,
   and each credential is encrypted to the daemon's own binding before it is
   stored, so no global passphrase exists to lose; the unlock key rides only
   authenticated transport and is zeroized after use. Two wire contracts hold
   that together: `BindKeystore` is the ONLY path that can create a binding
   (TOFU, once), so `Unlock` must stay verify-only, and `AddCredential` carries
   typed plaintext with no key, so the daemon encrypts it itself and requires an
   already-unlocked keystore. The flow, its reply-before-broadcast ordering
   invariant, and the multi-client provisioning consequences are under
   **Lock/Unlock flow** above.

4. **Sessions, not per-client state** — a session owns its model, working
   directory, and messages, and is independent of the connections watching it;
   clients subscribe/unsubscribe from sessions via the broadcast system.
   Sessions persist in redb and survive a restart, and every lifecycle operation
   (create, attach, list, delete) works while the daemon is locked — credentials
   are only needed to run a model. A session carries an optional `account_name`
   that decides which credential a request uses, so there is no global "default
   account" fallback, and the provider is resolved lazily from the daemon's
   provider registry rather than pinned at creation.

5. **Session hierarchy** — sessions can have parent sessions
   (`parent_session_id`), forming a tree. Child sessions inherit their parent's
   working directory unless explicitly overridden. The `spawn_subsession` tool
   creates autonomous child sessions that run their own tool-calling loop and
   report results back to the parent.

6. **Tool-call loop in the daemon** — the daemon drives multi-turn tool
   interactions, up to a configurable daemon-wide `max_turns` cap (default 0 =
   unlimited), rather than pushing that complexity to the client or model. The
   client just sees `ToolCallStarted`/`ToolCallFinished` events.

7. **Session subscription model** — multiple clients can subscribe to the same
   session, and every session event fans out to all of them as a broadcast
   (`id: None`) rather than back to the requester alone; that is what makes
   shared session viewing work. Only a request's terminal reply is targeted at
   one client.

8. **SSE streaming is hand-written, not a library** — a reader per wire dialect
   (OpenAI `data:` lines plus `[DONE]`, Anthropic's `event:`/`data:` pairs)
   keeps parsing and buffering ours. Three properties are contractual rather
   than incidental: the blocking socket read is **never on the consumer's
   thread**, so events, cancellation, and the deadline are all observable at
   once with no polling; a stream is bounded in **three** independent ways — one
   event's accumulated bytes, a per-attempt wall-clock deadline that fires even
   on a keep-alive trickle, and a socket-level idle timeout that resets per byte
   and so never cuts a steadily streaming response short; and deadline expiry is
   its own non-retryable error, distinct from a socket `Io` failure. Losing any
   of the three is what lets a gateway, a proxy, or a silent provider wedge an
   inference worker. The readers, the reader thread, the constants, and ureq's
   per-timeout quirks are `choreo-ai-protocols` rustdoc (`openai/sse`, `stream`,
   `retry`, `shared`).

9. **Markdown as the intermediate format** — all text (tool output, assistant
   text, error messages) is treated as markdown and rendered as HTML (desktop)
   or shaped to terminal output (choreo-tui), providing a consistent rendering
   layer.

10. **Flexible API format** — both OpenAI Chat Completions and the Responses API
    are first-class, resolved per model (`RequestFormat`, from the catalog's
    `openai_responses` flag with `default_request_format` as the fallback). The
    cross-cutting invariant is that there is **one canonical `messages`
    history** that each adapter lowers into its own wire shape, so a history
    stays sendable across a model switch or a restored session instead of being
    bound to the format that produced it. Each adapter's event vocabulary,
    streamed tool-call assembly, stateless dialect, and terminal-event
    classification are that adapter's own concern (the `choreo-ai-protocols`
    `openai` module docs own them).

    One fact crosses all of them: a tool call's **caller attribution** is
    carried, never interpreted — captured by the adapter, persisted on the turn,
    copied back **unchanged** onto the call and the output answering it by the
    rebuilt request, and stripped by the chat projection, whose wire has no such
    field. The value is typed in `choreo-proto`, which sits below
    `choreo-ai-protocols` and so cannot name the adapter's caller type.

11. **Pluggable providers** — every protocol client (OpenAI-compatible,
    Anthropic Messages, Google Gemini, Mistral) implements one `ProviderClient`
    trait and is built from an account's `provider` slug plus a credential. The
    trait, the clients, the shared types, and the provider catalog all live in
    `choreo-ai-protocols`, which is what keeps the daemon free of per-protocol
    knowledge: its `InferenceProvider` is a thin dispatch/metrics facade.

12. **OS threads with sidecar async runtime** — the daemon avoids async Rust
    everywhere except where third-party libraries (alloy, subxt, rmcp) require
    it, and those live in their own crates (`choreo-blockchain`,
    `choreo-content`, `choreo-mcp`), each holding a global
    `OnceLock<tokio::runtime::Runtime>` as a sidecar and running its clients via
    `block_on()`. The daemon links those crates only behind the `blockchain`,
    `content`, and `mcp` cargo features and calls their synchronous entry
    points, so tokio is never a direct dependency of the daemon. What each crate
    must therefore own is the bound the daemon cannot supply: a synchronous
    `block_on` cannot be interrupted, so the daemon's tool timeout can only
    *abandon* the execution thread and it is the crate-level cap that turns a
    black-holed endpoint into a clean error instead of a leaked thread (see
    `choreo-blockchain`'s `RPC_TIMEOUT` docs). Keeping the daemon thread-only
    simplifies the mental model (each thread owns its data, no `Send` bounds on
    shared state, no `Pin<Box<dyn Future>>`), improves stack traces, and avoids
    the complexity of async cancellation.

13. **Reasoning round-trip as an opaque artifact** — reasoning is not only
    display text: on Anthropic, DeepSeek/Kimi, Gemini, and the OpenAI/xAI
    Responses routes it must be sent back on the next request or the tool-call
    loop fails with a 400, so it is carried as a payload the daemon **never
    interprets**. Display text is a separate field (`Turn.assistant_reasoning`),
    so losing the artifact never blanks the transcript, and *whether* to send is
    derived (same-model provenance plus the catalog's `reasoning_passback`
    policy), never configured. **Reasoning round-trip** above holds the four-way
    custody split; the remaining rules are the owning modules' rustdoc.

    Two consequences are contractual because they cross crate lines. Capture and
    replay share **one** byte budget (`reasoning_details::MAX_REASONING_BYTES`):
    a larger per-turn cap would have no replay path, so the two differ in which
    artifact they drop, never in the number. And replay is bounded **per request
    over the whole history**, so a long session cannot compound artifacts
    without limit.

    **A reasoning-only turn is output**, and every turn reports **how the
    provider said it ended** (`FinishOutcome`, whose `Absent` is a first-class
    verdict rather than a fallback): both wire formats reach those verdicts
    through one shared assembler (`openai::turn::assemble_turn`), so the
    tool-call / truncated-call / empty-response decision tree exists once and a
    truncated call is reported rather than dropped — dropping it silently would
    make an unfinished turn indistinguishable from a finished one. **A tool call
    is reported, not adjudicated**: each adapter is responsible only for its own
    wire's shape, and whether a reply is runnable is decided once, in the
    daemon's agent loop (`requests::recovery`), with replay repair of an
    already-recorded call in the daemon's `wire` module, on the request copy
    only.

---

## Context file discovery

The daemon discovers project-specific context files (`AGENTS.md`, `CLAUDE.md`)
and Agent Skills at session start and re-checks them before every model call (every
turn of the tool-call loop), feeding what it finds into the system prompt. Three
properties are contractual and the discovery mechanism behind them is documented
in-source (`choreo-daemon`'s `context` and `requests` modules, `cargo doc -p
choreo-daemon` / `just doc`):

- **The prompt is always full.** `build_system_content()` runs once per loop
  iteration and always assembles a complete system prompt, so a session with no
  working directory still gets the base prompt (identity, tool groups, skill
  metadata, loaded skill bodies, session title); only the project context files
  and pending subdirectory hints are working-dir-gated.
- **Precedence runs most-local-wins.** Global files load first in a fixed order,
  then the project tree from the outermost ancestor down to the working directory;
  skills are scanned project-first (a project-local skill shadows a same-named
  global one, a more-local one shadows a same-named ancestor), so the nearest
  instructions and definitions always win.
- **Skills are disclosed progressively.** Only a skill's `name` + `description`
  ride the prompt; its body is injected when the model calls `load_skill`, and
  re-injected on every later turn so it survives the tool result scrolling out of
  the context window.

The `[context]` config table (`context_file_names`, `context_file_max_bytes`,
`disable_claude_code_prompt`) is documented in the README and the `ContextConfig`
rustdoc (`choreo-proto`).

## Testing

| Layer | What's tested | Location |
|---|---|---|
| Protocol | Framing, version handling, round-trip encode/decode | `choreo-proto/src/tests.rs` |
| Client core | Shell parsing, markdown→HTML, image assembly, history | `choreo-client-core/src/tests.rs` |
| Daemon | Request lifecycle, session CRUD, cancellation, tool calls, model listing | `choreo-daemon/src/tests.rs`, `choreo-daemon/tests/it/session_integration.rs`, `choreo-daemon/tests/it/lifecycle_integration.rs` |
| MCP (choreo-mcp) | Server spawn / Streamable HTTP connect, tool discovery, echo/error/structured/image calls, both protocol eras (`server/discover` + `Auto` fallback to `initialize`), schema normalization, cancellation (including the fixture asserting the SERVER observed `notifications/cancelled`), progress notifications turned into streamed chunks, the MRTR decline-and-retry loop for `input_required`, resource list/read, the `subscriptions/listen` list-change forwarding (a `modern-list-changed` fixture emits a tools list-changed that must reach the caller's sink, and a server without the capability must NOT have a stream opened), crash/no-init/garbage/oversized scenarios (the oversized case now failing on the client's own 8 MiB frame cap, plus `BoundedLineReader` unit tests); over HTTP: JSON and SSE responses, generated-header validation, `Auto` fallback, a retryable `503` connect, and rejection of the removed HTTP+SSE transport; the dispatcher protocol (including the concurrency cap: `CallGate` unit tests and a deterministic dispatcher test proving an excess call is queued until a slot frees), error mapping, retry/backoff math, and config parsing are unit-tested against a mock engine (no sleeps); the P6 bounds are covered too — the tool-count cap, the schema depth/size bounds (`json_depth`, `normalize_input_schema`/`normalize_output_schema`), the configurable restart cap (`RestartPolicy::new(0)` disables reconnect), the notification rate limiter, and the bounded SSE retry policy each have wait-free unit tests, and a `stubborn` fixture (which ignores stdin EOF) backs both a `choreo-mcp` and a daemon test proving shutdown returns within its bounded wait, while a `slow-list` fixture test proves the wedged-discovery server exits on stdin EOF rather than orphaning its child, and a crate-internal stdio test proves a dropped transport kills its child synchronously with no live runtime; the official client conformance suite runs both protocol eras against the `mcp-conformance-client` harness with a committed expected-failures baseline, and the OAuth half — target-scoped credential keying, connect-time refresh / revocation / scope handling, the `authorizationServer` override at connect, and the issuer fail-closed quarantine — against an in-repo mock authorization server with a temp-file credential backend | `choreo-mcp/tests/it/mcp_integration.rs` (stdio, against the in-tree `tests/fixtures/fixture_server.rs`), `choreo-mcp/tests/it/mcp_http_integration.rs` (a local `TcpListener` HTTP fixture), and `choreo-mcp/tests/it/mcp_oauth.rs` (a local mock authorization server); dispatcher/config/engine/retry/stdio unit tests in `src/`; the conformance harness in `tests/fixtures/conformance_client.rs` via `scripts/mcp-conformance.sh` |
| MCP (daemon) | McpManager + ToolRegistry integration, dynamic group registration (tool wrappers plus the `resources`-capability catalogue tools), tool execution, progress streaming through the wrapper, image attachment via the sink, the `subscriptions/listen` → shared-channel forwarding and the `register_all` / `refresh_server` + `register_cached` catalogue re-registration (the latter pinned by `mcp_list_change_only_relists_the_changed_server`, which proves the unchanged server is not re-listed), the list-change handler's atomic registry swap (unit-tested with no servers), `McpManager::status` for the `/mcp` surface and `session_inspect`, `McpManager::reload` (an empty reload and a malformed-config hard failure, unit-tested) and the `/mcp reload` connection handler (success summary + refreshed list, and the config error reply), and `mcp.json` parsing (transport inference, `${VAR}` expansion, `cwd`/`~` expansion, `disabledTools`, `maxRestarts`, `shared`, and the two-tier daemon/project load with the trust-gated project expansion, plus a fuzz-style malformed-input corpus), the project-root walk, the trust store (exact-canonical matching, no ancestor inheritance, fail-closed on garbage), same-root re-resolve reuse (connection count) and the reload/reconnect/trust-reload reconciliation paths (a changed `shared = false` slot is dropped, a per-session slot reconnects, a server FLIPPED from `shared = true` to `false` drops its stale daemon shared slot, an UNTRUSTED project does not suppress a daemon per-session server, a failed project connect does not shadow the daemon group, and an unchanged trust set is a no-op), and MCP OAuth at the daemon tier — the project-root trust gate on a server's `oauth` block, that untrusting a project never deletes a stored credential row, and the interactive sign-in (RFC 9728/8414 discovery, dynamic client registration, PKCE, and the confidential-client `client_secret_basic`/`client_secret_post` selection) against a mock authorization server | `choreo-daemon/tests/it/mcp_integration.rs`; `choreo-daemon/tests/it/mcp_project_trust.rs`; `choreo-daemon/tests/it/mcp_oauth.rs`; the registry-swap unit test in `src/daemon/tests.rs`; config unit tests in `src/mcp/config.rs`, `src/mcp/trust.rs`, `src/mcp/mod.rs` |
| Providers (`choreo-ai-protocols`) | SSE parsing, HTTP request construction, chat completions + responses serialization, content-block deserialisation, config overrides, catalog lookups, the one-shot completion paths' terminal-stop classification | `choreo-ai-protocols/src/openai/tests/mod.rs`, `choreo-ai-protocols/src/openai/tests/sse.rs`, `choreo-ai-protocols/src/openai/chat_completions/tests/mod.rs`, `choreo-ai-protocols/src/openai/config/tests.rs`, `choreo-ai-protocols/src/anthropic/tests.rs`, `choreo-ai-protocols/src/google/tests.rs`, `choreo-ai-protocols/src/catalog/mod.rs`, `choreo-ai-protocols/tests/it/simple_completion.rs` |
| choreo-tui | SVG rasterization, Unicode width, app state | The crate suite in `choreo-tui/src/tests/` — every fixture in one module (`tests/support.rs`: `test_app`, `add_user_text`, `make_session`, `make_session_full`, `insert_turn`), one module per subject (`lib_api.rs`, `cli.rs`, …), and a subject too large for one file as a directory module with one child per subject (`tests/state/`, whose 21 children cover the display-state plumbing, plus `tests/app/`, `tests/input/`, `tests/session/`, `tests/daemon/`, `tests/render/`), alongside the white-box suites that need a module's private items and therefore live in their own module (`render/tests.rs`, `selection/tests.rs`, `markdown_render/tests/`, and the inline `mod tests` blocks in `state/*.rs`, `terminal/*.rs`, `scrollbar.rs`, …) |
| choreo-gui | App state, render helpers | `choreo-gui/src/app_tests.rs` |
| Transport (`choreo-transport`) | Noise data plane — typed message round trips, single-fragment boundary + multi-fragment reassembly, oversized/tampered length-prefix rejection, malformed-handshake rejection, tampered-ciphertext rejection, silent-peer + dribbling-peer handshake timeout | `choreo-transport/tests/it/noise_integration.rs`, `choreo-transport/src/noise.rs`, `choreo-transport/src/handshake.rs` |
| Daemon↔client (Unix socket) | Ping/Pong, session CRUD round trips, attach ordering, `ShuttingDown`, concurrent clients + disconnect cleanup | `choreo-daemon/tests/it/daemon_client_unix.rs` |
| Daemon↔client (TCP/Noise) | Ping/Pong, session CRUD, cross-transport shared state, ACL rejection, wrong server key, encrypted `ShuttingDown`, explicit summary subscription (subscribed client gets broadcasts, unsubscribed gets none), >64 KiB fragmented message round trip, and a daemon→client >64 KiB reply round trip (`noise_large_message_daemon_to_client`) | `choreo-daemon/tests/it/daemon_client_noise.rs` |

> **Binary-spawning integration tests.** Integration tests live in their
> crates and test the libs. Any future binary-spawning integration test (via
> `env!("CARGO_BIN_EXE_...")`) must live in the crate that owns the binary
> (choreo-tui / choreo-im / choreo-acp each own theirs; the root package owns
> only the daemon binary).

**End-to-end harness.** `choreo-daemon/tests/it/common/mod.rs` is the shared
scaffolding for tests that run the real `run_server` (Unix socket + TCP/Noise)
and drive it with the real client library; its module docs own the details
(`test_db`, `test_daemon_state`, `SpawnedDaemon`'s temp socket/ACL/port and
SIGINT shutdown). Test modules opt in with `use crate::common;`.

**The conventions live elsewhere.** The unit/integration split, the
no-time-based-waits rule, and the one-integration-binary-per-crate layout are
the test-authoring contract (AGENTS.md → Test Discipline); the nextest profile
and the cargo aliases carry their own rationales (`.config/nextest.toml`,
`.cargo/config.toml`), the commands are listed in the README, and each CI job's
scope is documented in its workflow file. What is left here is the layout of
this workspace's suites, above.

---

## Build and run

The manifests declare a `rust-version` (MSRV) in every crate, inherited from
`[workspace.package]` in the root `Cargo.toml`. It is a **release-time claim,
not a build constraint**: development runs on nightly and dist/publish builds on
the current stable, and `.cargo/config.toml` sets
`resolver.incompatible-rust-versions = "allow"`, so resolution always takes the
newest available dependency versions even when their own `rust-version` exceeds
the floor. The floor is the higher of the resolved tree's high-water mark and
any newly stabilized std API the workspace adopts — an adopted API is the true
build floor even when no dependency demands it — so the number may lag while
development resolves ahead of it. It is verified rather than trusted: `just msrv`
compiles the workspace on exactly the declared toolchain and the CI `msrv` job
runs the same check, so a dependency that outgrows the floor (or an adopted API
newer than it) surfaces as a red check instead of a stale claim. Sync it before
publishing, per RELEASE.md Phase 2. The per-API detail, and the exact
recompute command, live in the `[workspace.package]` comment and that phase.

**Building defaults to nightly.** `rust-toolchain.toml` pins the workspace to
the `nightly` channel (rustup installs it on the first `cargo` run), so every
adhoc `cargo` command — including per-crate `cargo build -p choreo-x` /
`cargo check -p x` / `cargo nextest run -p x` — automatically gets the fast
per-profile `-Z` compiler flags that the nightly-only `profile-rustflags`
feature enables (opted in under `[unstable]` in `.cargo/config.toml`, and
configured — with the reason for each flag — in the root `Cargo.toml`'s profile
comments).

**Stable builds are a supported opt-out.** The sources use no nightly-only
features, so the code builds on current stable (the root `Cargo.toml`'s
`rust-version` documents the tested floor), but the nightly-only
`profile-rustflags` wiring hard-blocks stable *Cargo*, so a stable build runs
through `scripts/build-stable.sh` (`just build-stable` / `check-stable` /
`test-stable`): it strips the nightly-only keys and the `[unstable]` block for
the duration, runs `cargo +stable`, and restores them on exit — kill-safely,
with backups persisted under `target/` and a self-heal on the next run.
`scripts/build-android.sh` shares that mechanism, and the publish step meets the
same constraint from the consumer side: crates.io publishing runs through
`scripts/publish-stable.sh` (`just publish-stable`), which strips the same keys
before `cargo release publish`, because a published manifest carrying them
hard-breaks stable `cargo install`. See RELEASE.md Phase 2.

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

# Run terminal client (its own crate — owns its binary)
cargo run -p choreo-tui

# Run desktop client (its own crate — owns its binary)
cargo run -p choreo-gui

# Run IM bridge (Telegram)
cargo run -p choreo-im -- telegram
```

---

## External dependencies (key crates)

| Crate | Used by | Purpose |
|---|---|---|
| `tokio` | choreo-blockchain, choreo-content, choreo-mcp | Async runtime — the sidecar the blockchain, Coordination Platform, and MCP clients run on (linked via the daemon's `blockchain`, `content`, and `mcp` features respectively) |
| `alloy` | choreo-blockchain | EVM blockchain tools (behind the `blockchain` feature) |
| `subxt` | choreo-blockchain | Substrate/Polkadot blockchain tools (behind the `blockchain` feature) |
| `rmcp` + `process-wrap` + `reqwest` | choreo-mcp | The official Model Context Protocol Rust SDK, its child-process wrappers, and the HTTP client for the Streamable HTTP transport (behind the `mcp` feature); `choreo-mcp/Cargo.toml` records why each is needed |
| `serde` + `rmp-serde` | proto, daemon | Wire protocol framing and DB value encoding (MessagePack, named mode) |
| `structured-zstd` | daemon, choreo-ai-protocols | Pure-Rust zstd (no libzstd C build) — the daemon's `session_turns` DB values and opted-in LLM request bodies; level choices and rationale live in `db/codec.rs` and `retry/compression.rs` |
| `flate2` | choreo-ai-protocols | Pure-Rust gzip for opted-in LLM request bodies (the miniz_oxide backend `ureq` already enables) |
| `snow` | daemon, client-core, transport | Noise IK handshake and transport encryption |
| `ureq` | daemon | HTTP client |
| `pulldown-cmark` + `ammonia` | client-core | Markdown parsing, HTML sanitization |
| `ratatui` + `crossterm` | choreo-tui | Terminal UI |
| `dioxus` | choreo-gui | Desktop/Android UI (Native/Blitz renderer) |
| `image` + `resvg` + `heifer` + `moxcms` (via the `choreo-image` leaf crate) | daemon, choreo-tui | Image decoding (all `image`-crate raster formats incl. feature-gated AVIF), SVG rasterization, HEIC/HEIF decode, and sRGB colour management — one decode path shared by the model and UI paths; the per-format detail, the decompression-bomb budget, and the colour policy are the crate's rustdoc |
| `syntect` | choreo-tui | Syntax highlighting for code blocks (uses Sublime Text grammar files) |
| `aes-gcm` + `argon2` | keystore | Encryption, key derivation |
| `x25519-dalek` + `hkdf` + `sha2` | keystore | X25519 ECDH key agreement, HKDF key derivation |
| `ckb-vm` | daemon | RISC-V VM interpreter for sandboxed code execution |
| `postcard` | daemon, client-core | Compact binary serialization for Rust-only internal channels (VM↔host tool communication, encrypted credential pipeline) |
| `thiserror` | proto, keystore, client-core, daemon | Structured library error types |
| `anyhow` | daemon, tui, dioxus, im, keystore | Application error context & propagation |
| `gix` | daemon | Git operations |
| `teloxide` | choreo-im | Telegram Bot API client |
| `prometheus` | daemon | OpenMetrics instrumentation, process metrics (optional — behind the `metrics` feature, off by default) |
| `tiny_http` | daemon | Metrics HTTP server for `/metrics` endpoint (optional — behind the `metrics` feature, off by default) |
| `tracing` | daemon | Structured logging |

---

## Error handling strategy

### Library crates — `thiserror`

Each library crate defines one structured error enum with `thiserror`
(`ProtoError`, `KeystoreError`, `ClientError`, …), documented at its
declaration: the variant set, the `Display` wording, and each `#[from]`
conversion are the type's own rustdoc, so that is the reference. The conversion
that crosses a crate boundary is `impl From<ErrorType> for io::Error` on the
error types whose callers sit on io-shaped plumbing (`ProtoError`,
`ClientError`), which folds non-I/O variants into
`io::ErrorKind::InvalidData`.

### Binary crates — `anyhow`

All binary `main()` functions return `anyhow::Result<()>`. Key boundaries (socket
bind, keystore load, config parse) use `.context()` to attach meaningful messages;
internal functions keep domain error types (`io::Result`, `ProtoError`, …) and `?`
auto-converts through the blanket `From<E: Error> for anyhow::Error` impl.

### Tool errors

Each tool defines its own error type via the `Tool` trait's `type Error`.
Simple tools use `ToolExecError` (a string-wrapper newtype); tools whose errors
a VM guest consumes (`DbError`, `HttpError`) define a `Serialize` +
`Deserialize` `thiserror` enum so the guest can pattern-match specific variants.

`ToolError` covers *infrastructure* failures around tool execution — argument
deserialisation, I/O, postcard encoding — and is never returned by a tool's
`execute()` directly, only by the `ToolDyn` conversion layer. `execute_json`
and `execute_streaming_json` return `Result<ToolOutput, ToolError>` so a caller
can tell an infrastructure failure (JSON deserialisation) from a tool failure,
folding the latter into `ToolOutput { is_error: true }` for the LLM while keeping
the structured error for programmatic consumers.

The postcard binary path encodes all three outcomes in one nested
`Result<Result<R, E>, ToolError>` (`encode_outer()` in
`choreo-daemon/src/tools/mod.rs`), so a VM guest gets the whole picture from a
single byte buffer. The type docs in `choreo-daemon/src/tools/` own the field-by-field
detail of `ToolOutput` and `ToolOutputFormat`.
