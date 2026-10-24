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
│                       baking, HEIC/HEIF with a pre-decode allocation guard)
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

`choreo-sanitize` is a leaf crate (no workspace deps) consumed by
`choreo-daemon`, `choreo-tui`, `choreo-client-core`, `choreo-blockchain`, and
`choreo-mcp` —
it owns the Unicode "spoofing" predicates, the shared tool-output byte
budget / `...[truncated]` marker, and the canonical code-injection environment
set stripped from every spawned child, so every sanitizer and streaming cap in
the workspace agrees on the same policy and budget and the shell tool and MCP
stdio child strip the same variables.

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
`cargo build --profile dist -p choreographr -p choreo-tui` with
package-scoped feature syntax (the daemon and the TUI are SEPARATE packages
since the binary-split refactor; the musl build additionally enables both
packages' `mimalloc` features) — the two shipped binaries only
(choreo-gui's Dioxus Native (Blitz) renderer stack is excluded from
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

Exactly **two binaries** ship in every artifact (tarball, `.deb`, `.rpm`,
Homebrew, AUR):

- `choreographr` — the daemon
- `choreo-tui` — terminal UI client

The `choreo-im` and `choreo-acp` bridges are NOT built for release —
release.sh / the CI workflow build only the two shipped binaries; the
bridges are source-build extras (`cargo build -p choreo-im` /
`cargo build -p choreo-acp`). Each bridge/TUI binary lives in its own
workspace crate (choreo-im, choreo-acp, choreo-tui), which owns its
`src/main.rs` wrapper and its own `mimalloc` feature.

`choreo-mcp` is a **library-only crate** (the MCP client the daemon's
feature-gated `mcp` module uses to reach tool servers over stdio subprocesses or
the Streamable HTTP transport) — it is never shipped as a
binary. Its only `[[bin]]` target is a test fixture
(`mcp-fixture-server`, a scripted stdio server the integration suite spawns),
which release builds never produce. `choreo-gui` is built separately (desktop via `cargo run -p choreo-gui`,
Android via `dx build --platform android`, iOS via `scripts/build-ios.sh` +
the `ios/` Xcode scaffold) and is not shipped either.

The shipped target set is exactly four; `release.sh` and `install.sh`
hardcode this set and refuse any other platform ("ships Linux x86_64 + arm64,
macOS arm64, and macOS x86_64"), and `release.sh` builds each Linux host's
own arch — the x86_64 tarball on a Linux-x86_64 host, the aarch64 tarball on a
Linux-aarch64 host — and BOTH darwin tarballs on a Darwin-arm64 host (the
x86_64 one
cross-compiled — Apple's arm64-hosted toolchain targets x86_64-apple-darwin
natively, sharing the single Xcode SDK):

| Target | Platform | Asset |
|---|---|---|
| `x86_64-unknown-linux-musl` | Linux x86_64 | `choreographr-<version>-x86_64-unknown-linux-musl.tar.gz` |
| `aarch64-unknown-linux-musl` | Linux arm64 | `choreographr-<version>-aarch64-unknown-linux-musl.tar.gz` |
| `aarch64-apple-darwin` | macOS arm64 (native) | `choreographr-<version>-aarch64-apple-darwin.tar.gz` |
| `x86_64-apple-darwin` | macOS Intel (cross-built on the arm64 host) | `choreographr-<version>-x86_64-apple-darwin.tar.gz` |

The Linux tarballs are **fully static musl builds** — `release.sh`
cross-builds each to `<arch>-unknown-linux-musl` with `--features mimalloc`, so
each artifact
runs on any Linux kernel of its arch regardless of the host's glibc version
(this also
replaces the old "build inside an old-glibc container" compatibility dance).
The `.deb`/`.rpm` remain native glibc host-target builds without the
`mimalloc` feature — see `scripts/release.sh`; `build-deb.sh`/`build-rpm.sh`
tag them from the host arch (Debian's `amd64`/`arm64` in the control field,
the tarball spelling `x86_64`/`aarch64` in the filename). The tarball holds the two
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
request-worker panics with `catch_unwind` (sessions/worker.rs), which abort would
defeat. The RPM spec keeps `__os_install_post %{nil}` so rpm's brp scripts
never re-process the already-stripped binaries.

Shipped binaries build at an explicit **CPU floor per target**, set via
`RUSTFLAGS="-C target-cpu=…"` in the release scripts/workflow — never
`target-cpu=native`, and never profile rustflags (those ignore `--target`, are
nightly-only, and are stripped by `scripts/build-stable.sh` before every
stable release build; env rustflags additionally override any developer's
`~/.cargo/config.toml`, so local and CI artifacts are comparable):

- **x86_64 musl tarball + Windows zip: x86-64-v2** (SSE3/SSSE3/SSE4.1/SSE4.2/POPCNT/
  CMPXCHG16B — Intel Nehalem 2008+, AMD Bulldozer 2011+). The level enterprise
  distros have moved to (RHEL 10 baseline = v3, SLES 16 = v2) while the
  community distros (Debian/Arch/Fedora, Ubuntu mainline) stay v1 — v2 is the
  pragmatic floor between "runs on anything since 2003" and modern
  vectorization. Future per-CPU-level artifacts (e.g. a v3 tarball) reuse the
  same `RUSTFLAGS` mechanism with a different value.
- **arm64 musl tarball: the generic aarch64 baseline (no flag)** — SIMD (NEON)
  is mandatory in AArch64, so there is no x86-style v1/v2/v3 tier split to aim
  at; the target default is the fleet-safe choice.
- **macOS arm64 tarball: the target default** — `aarch64-apple-darwin` already
  defaults to `apple-a14` (Apple-Silicon-tuned), and the fleet is homogeneous
  by definition; no flag needed.
- **macOS x86_64 tarball: x86-64-v3** (AVX2/FMA). Unlike the aarch64 target,
  `x86_64-apple-darwin` defaults to generic baseline x86-64 (2003 SSE2),
  which undershoots the actual fleet: the last Intel-capable macOS (26 Tahoe)
  supports only the 2019–2020 Intel Macs (Coffee/Ice/Comet Lake, mac Pro
  2019), every one AVX2-class — so v3 DESCRIBES the fleet rather than betting
  on it. The fleet can only shrink (macOS 27 is Apple-Silicon-only), so v3
  can never become too aggressive, and Rosetta 2 emulates AVX2/FMA, so the
  binary also stays valid if run translated on Apple Silicon.
- **Android/Termux: generic `armv8-a`** — the device fleet spans a decade of
  cores with nothing newer in common; `build-android.sh` enforces
  `RUSTFLAGS="-C target-cpu=generic"`.
- **.deb/.rpm: baseline (v1)** — the glibc-distro range they serve is split
  (Debian/Arch = v1, RHEL 10 = v3), so baseline is the only level covering
  all of them (2003 SSE2 on x86-64; the generic aarch64 baseline on arm64).

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
| `install.sh` | curl\|sh installer — downloads the pinned-version tarball, verifies its SHA-256 against the `SHA256SUMS` fetched over the same TLS channel (no trust-on-first-use, no eval), extracts only the shipped binaries via an explicit member list, installs the platform service file, and never auto-enables. `--uninstall` removes everything; `CHOREOGRAPHR_BASE_URL` overrides the download base for testing/mirrors only. |
| `build-deb.sh` / `build-rpm.sh` | Build the single fat `.deb` / `.rpm` containing the shipped binaries plus the systemd user unit, from existing `target/dist/` artifacts. Both detect the host arch (`uname -m`): x86_64/aarch64 only, failing loudly otherwise. The `.deb` control field uses Debian's tag (`amd64`/`arm64`) while the filename uses the tarball tag (`x86_64`/`aarch64`); the `.rpm` gets its `Version` and `BuildArch` from the `pkg_version`/`pkg_arch` macros passed to the spec (the version read from the workspace manifest, so a shipped package can never carry a stale version). The `.deb` forces xz archive members (`-Zxz`) — dpkg ≥ 1.22 defaults to zstd, which older-dpkg distros the package targets (e.g. Ubuntu 22.04) cannot read |
| `build-deb-termux.sh` | Build the **Termux-native** `.deb` from the already-cross-built `target/android/arm64-v8a/` binaries (NO rebuild) — package `choreographr`, `Architecture: aarch64` (Termux's tag, not Debian's `arm64`), files at `./data/data/com.termux/files/usr/bin/<name>` — Termux's dpkg installs against `/` with no chroot, so the package must carry the real on-device path of the fixed `$PREFIX` (as upstream Termux packages do; the earlier `./bin/` convention made dpkg try to write `/bin` at the read-only device root), no maintainer scripts / conffiles (Termux dpkg runs as the app uid, no root). Built with `dpkg-deb --build --root-owner-group -Zxz` — xz is forced because dpkg ≥ 1.22 on the ubuntu runners defaults to zstd and Termux's dpkg has no zstd support (it only finds `control.tar{xz,lzma,}` members; the on-device failure this caused is what the in-script member assertion guards against); validates control fields, archive members, contents, and exec bits in-script — structural only, since there is no Termux on the build host. Output: `dist/choreographr-termux_<ver>_aarch64.deb` (the `-termux-` infix disambiguates it from the desktop `.deb` on the release page). Wired into the release workflow's android job only — deliberately NOT into `release.sh` |
| `smoke-test.sh` | Dispatches on the artifact suffix: a release tarball is extracted, the shipped binaries' presence/exec bits/`--version`/`--help` are checked; a `.deb` (the Termux package) is validated structurally via `dpkg-deb` — control fields, no `Depends:`/maintainer scripts, the shipped binaries at Termux's `$PREFIX` path (`./data/data/com.termux/files/usr/bin/`) with 0755 modes — the structural ceiling since no Termux exists on the host |
| `release.sh` | The release orchestrator — local builds (its CI counterpart is `.github/workflows/release.yml`, which runs it on the Linux/macOS runners); dry-run by default, `--upload` runs `gh release create`, `--allow-dirty` skips the clean-tree guard (CI passes it: a checkout IS the pushed commit, so the uncommitted-edits threat model cannot apply) |
| `publish-stable.sh` | The crates.io publish wrapper (RELEASE.md Phase 2) — strips the nightly-only per-profile `rustflags` and the `[unstable]` config opt-in for the duration of `cargo release publish` (masking the two edited files from cargo-release's clean-tree gate via `git update-index --skip-worktree`, and passing an `--exclude` for every `publish = false` member derived from the manifests — cargo-release 1.1.5 won't drop them on its own via `publish = false`), so published manifests stay buildable by stable `cargo install`, then restores both files and clears the masks |
| `update-homebrew-tap.sh` | Bumps the `choreographr/homebrew-choreographr` tap formula to the workspace version — recomputes both macOS tarball `sha256` digests from `dist/` (no re-download), rewrites `Formula/choreographr.rb` with exact-count rewrite validation, prints the diff; `--push` commits + pushes to the tap. Keeps the tap bump on the release machine (the CI release workflow ships the tarballs but does not touch the tap) |
| `check-supply-chain.sh` | The dependency supply-chain gate — runs `cargo deny check advisories bans sources` against `deny.toml` (falling back to `cargo-audit` + a literal lockfile scan when cargo-deny is absent), after scanning the local `~/.cargo/registry` cache for the `.crate` files deleted during the 2026-08-20 `arrayref` attack (RUSTSEC-2026-0260). Wired into the release workflow (`.github/workflows/release.yml`, before publishing) and runnable locally via `just check-supply-chain`; see the **Dependency supply chain** subsection under **Security model** |
| `build-android.sh` | Cross-builds the shipped suite binaries for Android/Termux via cargo-ndk (`arm64-v8a` by default, `--emulator` adds `x86_64`; `--check` is a prerequisite-checking dry run) under `--profile dist` (the shipped-artifact profile — matches the desktop release pipeline), stages them in `target/android/<abi>/` (cargo's target/ tree — `dist/` is reserved for final publishable artifacts), and prints the `adb push` guidance for Termux `$PREFIX/bin`. Its output is the input for the Termux packaging step (`scripts/build-deb-termux.sh`, CI android job): packaging never rebuilds. Links both shipped binaries with a linker-script fragment that re-aligns the TLS output sections to 64 bytes — bionic's loader rejects arm64 executables whose `PT_TLS` has `p_align < 64` (rust/LLVM emit 8, and the emutls-for-Android rust PR was never merged), which aborted every Rust binary with thread-locals at startup on Android 10+; a post-build `readelf` gate fails the build rather than shipping a binary that dies on-device. Strips the per-profile `rustflags` from the manifest for the duration (persistent backups under `target/` + EXIT-trap restore, plus a next-run self-heal that recovers a tree left stripped by a hard-killed predecessor — the trap-reliant restore alone was not kill-safe; see `build-stable.sh`) — profile rustflags apply regardless of `--target`, so `-C target-cpu=native` would emit host-CPU code that traps on Android devices. Deliberately excludes `choreo-gui`, whose Android build is `dx build --platform android` (cdylib APK payload, `just gui-android`) |
| `build-ios.sh` | Compiles `choreo-gui` (the ONLY crate that ships to iOS) for `aarch64-apple-ios` (+ the `-sim` slice when run on a Mac) and stages the link inputs the `ios/` Xcode scaffold consumes. Runs on ANY host: with Xcode it is a full build; without one (the Linux check laptop) it installs shims under `target/ios-shims/` — a `RUSTC` wrapper stripping `-C target-cpu=native` (profile rustflags are not suppressible via `RUSTFLAGS` env, the same trap `build-android.sh` documents) and cc wrappers translating clang/rust-style target triples to `zig cc` form (Apple-iOS compiles AND the pdf-inspector-style build-script dylib links are rewritten to zig's macOS target, because zig ships darwin libc headers and stub dylibs only for macOS — compile-validation fidelity, not on-device code; the shim is also put on `PATH` so build scripts that spawn a bare `cc` hit it instead of the HOST compiler), with a fake `SDKROOT` so cc-rs never needs `xcrun` (the cc/cxx shim generator is SHARED with `check-ios.sh` in `scripts/lib/ios-cc-shims.sh` — one generator, so the two scripts cannot drift again; the generated shim files are stamped with the current generator's name so a stale shim shows where it came from). The final Apple dylib/app link happens on the Mac; the script builds the staticlib via `cargo rustc --crate-type staticlib` — a self-contained `.a` (std + every C dependency folded in by rustc's internal archiver, so NO per-dependency staging list exists to rot as deps change; the staged artifact now embeds the whole choreo-daemon tree for the iOS embedded daemon, minus the `pdf` feature which is iOS-opted-out because pdf-inspector's Apple dylib link is exactly what the shim cannot do). The `ios/` directory (main.m UIApplication bootstrap + xcodegen `project.yml` + Info.plist) is the Xcode-side counterpart; see the phase 0b caveat comments there about the winit/blitz-shell iOS event-loop wiring, the one piece no non-Mac host can verify |

### Distribution channels (0.1)

- **Homebrew tap** — `brew tap choreographr/choreographr && brew install choreographr` (prebuilt formula)
- **GitHub Releases** — the Linux x86_64 + arm64 musl tarballs, `SHA256SUMS`, the desktop `.deb`/`.rpm` in both Linux arches, and the Termux `.deb` at `https://github.com/choreographr/choreographr/releases`
- **choreographr.com** — `https://choreographr.com/download/<version>/` mirrors the tarball and `SHA256SUMS` (this is what `install.sh` fetches); `https://choreographr.com/install.sh` serves the installer, and per-version download redirects are added at release time
- **AUR** — `choreographr-bin` (prepared in `packaging/aur/`, **not yet published**: no maintainer account and AUR registration is closed)
- **crates.io** — `cargo install choreographr choreo-tui` (source build, needs Zig) and `cargo binstall choreographr choreo-tui` (prebuilt; asset naming resolved via `[package.metadata.binstall]` in each package, below)

### crates.io metadata

The workspace inherits crates.io-required fields from `[workspace.package]`
in the root `Cargo.toml` (`version`, `license`, `repository`, `homepage`,
`readme`, `description`), and members opt into publishing by *not* setting
`publish = false`. The **publish set** is therefore nineteen of the twenty
workspace packages (the root package plus all nineteen members except
`choreo-gui`) — everything except `choreo-gui`, the one private member (a leaf
client nothing depends on).
`choreo-sanitize`, `choreo-image`, `choreo-sockreg`, `choreo-power-events`, and
`choreo-content` are published too (all added since 0.1.0): the members that
depend on them must resolve them from crates.io after a release, and
cargo-release's publish verification rejects unpublished workspace deps (cargo
will not even package a crate whose dependency is unpublished — optional deps
included):

`choreographr` (root), `choreo-daemon`, `choreo-blockchain`, `choreo-tui`,
`choreo-im`, `choreo-acp`, `choreo-proto`, `choreo-shared`, `choreo-keystore`,
`choreo-transport`, `choreo-ai-protocols`, `choreo-mcp`, `choreo-client-core`,
`choreo-sanitize`, `choreo-markdown`, `choreo-image`, `choreo-sockreg`,
`choreo-power-events`, `choreo-content`

`choreo-gui` sets `publish = false`: it drags in the Dioxus Native (Blitz/wgpu)
renderer tree and is not part of the shipped suite, so
it is neither published to crates.io (`cargo install choreo-gui` does not
exist) nor included in the prebuilt release artifacts (tarball/.deb/.rpm/
Homebrew/AUR), which carry the daemon and TUI only. `choreo-gui` is kept
out of the publish selection explicitly — `scripts/publish-stable.sh` derives
an `--exclude` for every `publish = false` member from the manifests, because
cargo-release 1.1.5 does not honor
`publish = false` in `--workspace` selection (verified: its plan lists the
private crate, and a real publish would then fail on cargo's own
refusal). The
root `choreographr` package transitively depends on the other 18 publish-set
members, so releasing the suite publishes 19 crates in dependency order.

Releases are driven by **cargo-release** (`[workspace.metadata.release]` in
the root `Cargo.toml`): it bumps versions, tags, and publishes the 19 crates
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
musl tarball, plus an `aarch64-unknown-linux-gnu` override mapping arm64 glibc
hosts to the arm64 musl tarball (each arch's static musl tarball is the only
Linux asset shipped for that arch). The `choreo-tui` crate carries
an IDENTICAL `[package.metadata.binstall]` block: the release tarball is one
archive containing both binaries, and binstall installs only the binaries a
package declares — so `cargo binstall choreo-tui` resolves the SAME asset
URL (the version template renders the crate's version, which stays in
lockstep with the release version via the workspace version inheritance) and
extracts just `choreo-tui`. Fetching the same asset from two packages costs
a duplicate download but keeps every package self-describing — the
alternative (per-package asset split) would change the release artifact set
for zero benefit. The daemon crate is `choreo-daemon` (library
`choreo_daemon`, no `[[bin]]` target) — the `choreographr` binary it backs
is declared by the root package's `src/bin/choreographr.rs`.

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
no session, e.g. "no session attached" failures) — it can never be forgotten,
mismatched, or duplicated; the wire nests the event inside the envelope, so the
origin is present on the wire too. The daemon's payload enum is deliberately not
`#[non_exhaustive]`: the variant set IS the wire contract, so every consumer
match enumerates it fully.

**Wire format:**

```
┌──────────────────┬────────────────────────────────────────┐
│ 4 bytes (BE u32) │ msgpack((protocol_version: u8, msg))   │
│   payload len    │                                        │
└──────────────────┴────────────────────────────────────────┘
```

- Protocol version: `9` (v9 = live MCP config reload: the new `ClientMessage::McpReload` request and its `DaemonMessage::McpReloaded`/`McpReloadFailed` replies, so a running daemon can pick up `mcp.json` edits (added/removed/changed servers) without a restart; v8 = per-session `pinned`/`archived` flags: the new `ClientMessage::SetSessionPinned`/`SetSessionArchived` requests, the daemon-GENERATED broadcast `SessionEvent::SessionFlagsChanged` (rides `DaemonState::broadcast()`, so a client learns of a flag change via the broadcast rather than a targeted reply), and the `SessionSummary::pinned`/`archived_at` fields they surface. v7 = the create-session reply is split from the create-session broadcast: `SessionEvent::SessionCreatedForRequester` is the direct reply to the creating connection and is the ONLY create event a frontend may auto-attach to, while `SessionEvent::SessionCreated` is notification-only (broadcast to every subscriber) and must never move a client's view — fixing a client hijacking its own view when ANOTHER client created a session. Delete/status lifecycle messages are unaffected: `SessionDeleted` was already broadcast-only with the requester already knowing the id it deleted; v6 = displayed-image bytes are no longer shipped in session-scoped snapshots (`SessionState`/`TurnAppended`/`TurnsRedone`) — they carry only `ImageMetadata` — and are instead fetched on demand via the new `ClientMessage::GetImage` ⇄ `DaemonMessage::Image` pair, keyed by `(session_id, turn_id, image_index)` and served from the durable `session_attachments` store; v5 = the two-state `Locked`/`Unlocked` status *broadcasts* were replaced by `DaemonMessage::Keystore { state: KeystoreState }` (`Unbound`/`Locked`/`Unlocked`), pushed at subscribe time and on every transition so a first-run client learns it must BIND; `Locked`/`Unlocked` remain targeted operation replies. v4 = the 29 session-scoped events were moved into `SessionEvent` and now ride the `DaemonMessage::Session { session_id: Option<u64>, event }` envelope; v3 had removed `TurnFinalized` — the final-turn snapshot rides `TurnAppended` — and added `Evicted`, a best-effort lag-eviction advisory; mixed-version peers fail fast at the version gate). The v4 shape was amended in place before the first release — the `session_id: 0` sentinel became `Option<u64>` (`None` for connection-level replies) — so the wire version stayed `4` with no bump at that point. The MCP trust-query request/reply variants (`ClientMessage::McpTrust`/`McpUntrust`/`McpTrustList`, `DaemonMessage::McpTrustUpdated`/`McpTrustList`) were added to v9 the same way, before any release carried v9; the policy is that variants may be added to the **current, unreleased** wire version without a bump, because mixed-version peers do not exist until a release ships and the version gate fails fast on a mismatch either way. A bump is required only once a version has shipped in a release. Under the same pre-release policy, the v6 `GetImage`/`Image` pair was reshaped for v9 before any release carried v9: its positional `image_index: u32` became a tagged `key: ImageKey` (`Displayed { index }` or `ToolResult { call_id }`), so one fetch protocol now serves both displayed images and tool-result vision images from the shared `session_attachments` store. Under the same pre-release policy, the v9 frame was amended in place to the uniform correlation envelope: `ClientMessage` = `{ id: u64, inner: ClientMessageType }`, `DaemonMessage` = `{ id: Option<u64>, inner: DaemonMessageType }` (a broadcast is `id: None`; a reply stamps the request's id), the payload enums were renamed `ClientMessageType`/`DaemonMessageType`, the streaming `request_id` was renamed `stream_id` and widened to `u64`, and the `MessageKind` tag plus the `Accepted`/`Failed` acknowledgement replies were added. No bump — no release has shipped v9. Under the same pre-release policy, the v9 streaming axis was made daemon-owned: the session thread assigns a run's `stream_id` (a per-session, monotonic counter) when it accepts a `RunInput`/`ContinueGeneration` and reports it on the acceptance `Started` reply/broadcast, so those requests no longer carry a client-chosen `stream_id` (the client learns it from `Started`, which also fixes the cross-client collision where two clients fanned one session could each claim the same id); the `CANCEL_ALL` sentinel (`stream_id = 0`) remains for a pre-`Started` cancel. Under the same pre-release policy, the `SessionStatus` streaming phase was split in v9: the single `Inference` state became `Sending`/`Waiting`/`Reasoning`/`Responding`, so a status bar can tell the request dispatch from the provider's time-to-first-token wait from reasoning from answer streaming. The `Waiting` boundary is anchored on client-side request-upload completion (the last byte of the request body handed to the transport), not on the provider's response headers: a buffering gateway (e.g. opencode.ai behind Cloudflare) withholds the response head until the model begins generating, so anchoring on the headers would collapse `Waiting` into `Sending` for those providers. The upload hook fires once per attempt, so a retried turn reports `Waiting` for the failed attempt too, before `Retrying`. Under the same pre-release policy, `ClientMessage::AddCredential` was reshaped in v9 to carry a typed plaintext `ServiceCredential` (`{ service, credential }`) instead of a client-encrypted `encrypted_payload` plus an `unlock_key`: no key rides the wire, the daemon encrypts the credential to its own binding public key, and the add path requires an already-unlocked keystore (no implicit unlock). Also under the same pre-release policy, v9 gained the `DaemonMessage::ModelsUpdated { account, models }` broadcast, emitted when a background model-list fetch finishes so an already-open model picker refreshes in place; a cold picker now paints instantly from a locally cached or catalog-seeded list and this broadcast supersedes that seed when the live fetch lands, instead of blocking on a slow `/v1/models` round-trip.
- Max frame size: 64 MiB
- Lag-eviction byte gauge: `DaemonMessage::approx_wire_size` / `Turn::approx_size` (in `choreo-proto/src/size.rs`) — a deliberate over-estimate used by the daemon's lag accounting, pinned by `types::tests::approx_wire_size_never_underestimates_encoded_payload` (see the daemon's `broadcast` module docs for the lag accounting)

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

## Enrollment & transport trust

The TCP enrollment model is a deliberate ASYMMETRY — the SSH `known_hosts`
pattern, not symmetric TOFU. It exists so that the first-contact window (the
only moment a MITM could strike) can never observe the daemon's keystore
unlock key, which clients send inside `Unlock`. Every key arrives over a channel the
operator already trusts, or is confirmed by a human comparing fingerprints:

- **Client keys → daemon: out of band, always.** A client's transport public
  key is never learned from the wire for authorization purposes. It is
  provisioned into `authorized_clients.toml` by the operator via one of three
  equivalent paths (manual file edit, `/acl add` from a LOCAL connection,
  `choreographr acl-add` CLI) — all converging on the same file and the same
  advisory-file-lock discipline, with the hot-reload chain making every path
  effective without a daemon restart. The ACL parse-compare reload policy
  means a torn editor save never un-authorizes live clients. When the ACL
  lives in the config directory (the production layout) it is watched by the
  same unified config transport as the overlay and accounts; an ACL resolved
  elsewhere (the `--base-dir` layout keeps it at the base root) gets its own
  watcher, so ACL hot-reload never depends on where the overlay happens to
  live.
- **Server key → client: learned in-band on FIRST contact only, confirmed by
  a human, then pinned.** With no pin for the address, the client runs the
  Noise XX handshake (`PREAMBLE_XX`), which reveals the server's static key;
  the client renders its fingerprint (base64 clustered into 4-char groups —
  bijective with the 32-byte key) and requires confirmation against the value
  the daemon operator read out during the enrollment conversation (interactive
  y/N before the TUI starts, or `--trust-fingerprint` for headless clients).
  Only then is the key pinned to `known_servers.toml` and any protocol traffic
  — `Unlock` in particular — allowed to flow. Every later connection uses
  Noise IK (`PREAMBLE_IK`) against the pin; a changed server key fails LOUDLY
  with the pinned fingerprint and re-pair guidance, never a silent re-prompt.
- **Why not symmetric OOB, and why not blind TOFU:** blind TOFU would let a
  first-contact MITM harvest an `Unlock` (the daemon keystore's unlock key) —
  strictly worse than a phished SSH password. The XX + fingerprint flow keeps
  the single out-of-band artifact to one fingerprint readout while
  eliminating the client-side file handling of a symmetric exchange. The
  daemon-as-client future reuses exactly this shape: one keypair per daemon
  serving both roles, enrollment operator-driven, headless confirmation via
  the fingerprint flag.
- **Decision rule:** a new peer is always provisioned by an operator who can
  touch a trusted machine — never by the network. Any future feature that
  seems to require in-band trust bootstrapping is a sign of trying to automate
  operator trust; revisit the XX confirm UX before weakening the gate.

The Noise transport's shared state is the workspace's **second** sanctioned
shared-state exception (see AGENTS.md). `NoiseStream` clones one
`Arc<Mutex<TransportState>>` and one `Arc<AtomicBool>` single-writer guard
across its `try_clone` reader/writer halves, which must interleave
encrypt/decrypt against the same snow `TransportState` — so the state is shared,
not channeled. The lock is held only per chunk, never across blocking socket
I/O (that scope is what prevents the bidirectional large-message deadlock), and
the guard is a single-bit flag in the spirit of the cooperative-cancellation
flag; the full rationale is in `choreo-transport`'s `noise` module docs.

The plaintext side of the transport is zeroized. `choreo-proto`'s
`encode_payload`/`encode_frame` return a `zeroize::Zeroizing<Vec<u8>>`, and
`choreo-transport` holds that buffer until the Noise encryption call returns
(and clears its reused per-fragment plaintext staging buffer after each send),
so an encoded frame — which may carry a secret such as a credential riding
`AddCredential`, or the unlock key — is wiped from memory on drop instead of
lingering in freed or reused buffers.

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

Two contracts cross that boundary and are public surfaces in their own right:

- **Streaming.** A turn is delivered as a sequence of `StreamEvent`s
  (`Answer`/`Reasoning`, `src/types.rs`) through a `&mut dyn FnMut` callback
  (kept object-safe); a non-streaming turn is converted to the identical event
  sequence by `emit_non_streaming_events` (`src/shared.rs`), so one event-driven
  path serves both a streaming and a non-streaming configuration without
  per-provider duplication.
- **Metrics.** `InferenceProvider` records `record_api_call` /
  `record_api_error` around every turn — timing lives in the daemon so
  `choreo-ai-protocols` stays free of daemon concerns. The `endpoint` label is
  the **catalog slug**, not the protocol name (e.g. `"opencode"` rather than
  `"openai"`): more precise, but part of the public Prometheus contract, so
  renaming a slug changes that provider's series. Error labels come from
  `InferenceError::metric_label()` in `choreo-proto`, the single canonical
  mapping shared by all providers.

Each client maps the account's `&str` reasoning-effort slug onto its own wire
format (OpenAI-compatible `reasoning_effort`, Anthropic a `thinking` block with
`budget_tokens`, Google `thinkingConfig`); each adapter module documents its
exact mapping. Mistral speaks the OpenAI wire format
(`POST /v1/chat/completions`), so it is catalogued under `OpenAi`, not a
protocol of its own.

### models.dev + overlay

The catalog is a two-layer pipeline in `choreo-ai-protocols/src/catalog/`: a
**normalized models.dev base** supplies the facts (provider slugs/names, base
URLs, and per-model context/reasoning/cost data), and a **bundled policy
overlay** (`catalog/models-overlay.toml`) supplies everything models.dev cannot
express (wire-protocol selection, endpoint and default-model policy, per-model
passback exceptions, the prompt-cache TTL tiers, the request-body-compression
opt-in, and the wholesale overlay-only providers models.dev does not cover). The overlay is merged **at load time**,
never baked into the committed blob `catalog/catalog.bin`, so the runtime
refresh (S4) can re-merge the same base with a user overlay. The pipeline, the
per-model normalization, the exact overlay schema, and the merge precedence
(provider scalars field-wise replace; a named model overrides that entry's
fields onto the base; unknown keys warn and are skipped rather than failing the
daemon) are documented in-source (the `catalog` module and
`catalog/models-overlay.toml`), held complete by the `#![warn(missing_docs)]` +
`doc-check` gate.

### Provider auth: grant versus request material

Provider authentication is a **two-layer** model, split so the durable grant
never leaves the daemon and the wire crate never learns about refresh. The
daemon resolves an account to a `ProviderAuth` (`choreo-daemon/src/providers/auth.rs`):
either an API-key grant (`ApiKey { key }`) or an OAuth grant
(`OAuth { credential, access_token }`), where the OAuth arm carries the whole
`ServiceCredential::OAuth` (the refresh token, client registration, and identity
claims) plus any cached ephemeral access token. A client consumes a
`RequestAuth` (`choreo-ai-protocols/src/shared.rs`): the bearer material plus any
credential-derived extra headers, and nothing else. `ProviderAuth::into_request_auth`
is the projection between them, and it is the one place a not-yet-signed-in OAuth
account (a grant with no cached access token) becomes a clean error rather than a
request with an empty bearer. `choreo-ai-protocols` gains no `choreo-shared`
dependency: only the already-derived `RequestAuth` crosses into it.

`from_account_config` stays the single protocol dispatch. For the OpenAI protocol
it converts the grant to `RequestAuth` and builds the chat client from it, while
image generation — API-key-only — gets an image backend only for an API-key
grant (an OAuth account simply has none). The Anthropic and Google protocols have
no OAuth path in this stack, so they extract the API key and reject an OAuth grant
with the existing "no API key" guidance. `RequestAuth` is threaded through the
OpenAI request path to the retry layer (`openai/retry.rs`), which builds the
`Authorization: Bearer …` value from it and **merges** its credential-derived
extra headers with the opencode gateway routing headers (both apply to one
request).

The credential-derived headers are policy, kept in one function
(`auth.rs::derive_headers`) so a later provider is added as data: the
`ChatGPT`/Codex `chatgpt-account-id` header (the `account_id` claim extracted at
login, sent only when present) and a stable `originator` app id. An API-key grant
sends none of them.

Two facts drive the auth shape, and each lives in exactly one place. The
**catalog `oauth` profile** (`choreo-ai-protocols/src/catalog/`) is user-editable
**data** — an optional `[provider.<slug>.oauth]` overlay block carrying the
issuer, endpoint paths, scopes, redirect ports, per-endpoint grant encodings
(form versus JSON), a device-code flag, and a public client id. A provider's auth
kind is **derived** from it (`ProviderEntry::auth_kind`: an `oauth` profile means
OAuth, its absence API key), never a separate attribute, so models.dev stays the
one fact source and the overlay the one policy layer. The **client-identity
table** (`choreo-ai-protocols/src/identity.rs`) is compiled **policy** — which
posture (register our own client, borrow the vendor CLI's id, or delegate) applies
per provider slug — and is deliberately not overlay-overridable. Shipping or
using a borrowed vendor client id is gated behind an explicit per-install opt-in
(`borrowed_ids_enabled`, default off).

The **token cache** (`DaemonState::access_tokens`, keyed by account) holds the
ephemeral access token plus its expiry and receipt time, in daemon memory only —
never persisted, since the refresh token in the credential blob is the durable
root. It is owned by the command loop (single writer) and wiped on `/lock`
alongside the credentials map.

#### Provider OAuth login (frontend)

The interactive sign-in runs in the **frontend**, not the daemon. The shared
login library in `choreo-client-core` (`src/oauth/`) performs PKCE **S256** plus
a random `state`, a loopback callback server bound to the loopback name the
`redirect_uri` advertises (`localhost` for a provider, `127.0.0.1` for an MCP
server; ports 1455 then 1457, falling back on `AddrInUse`), or an RFC 8628
device-code poll for headless hosts, then builds the durable
`ServiceCredential::OAuth` and returns the `ClientMessageType::AddCredential`
frame that carries it — no render crate constructs an OAuth credential. The flow
runs on a **worker thread** and streams progress (`AuthUrl`, `DeviceCode`,
`Prompt`, `Progress`, `Done`, `Error`) over a crossbeam channel; a single-bit
cancel flag (polled by the loopback accept loop via `tiny_http`'s
`recv_timeout`, since a blocking accept cannot be interrupted by a channel
message) and a stop channel end it. The returned `LoginSession` handle owns the
worker's lifetime: **dropping it requests cancellation** (setting the flag and
sending on the stop channel, never joining), so a caller that discards a flow
mid-sign-in never leaves the login thread — or its bound loopback callback
server — running for the rest of the process. The `id_token`'s identity claim
(`chatgpt_account_id`) is read **without signature verification** — the token
arrived over TLS from the token endpoint — with the access token as a fallback;
the access token is then dropped and only the refresh token, client
registration, and extracted claims persist. The claim decode is shared with the
runtime refresh path (`choreo-shared`'s `oauth_claims`), so login and refresh
read the same namespace one way; the endpoint-security rule (`https`, or a
loopback `http` address) is likewise one shared predicate
(`choreo-shared`'s `oauth_endpoint`) that both the login and the refresh path
apply, so the two cannot drift; and the token-endpoint response body is parsed
by one shared reader (`choreo-shared`'s `oauth_token`), so a token reply —
success or error, and with a `expires_in` a lenient provider emits as a string —
is interpreted identically by both paths, and the owned token set it projects to
(`choreo_shared::oauth_token::TokenSet`) is that one shared type, so the
wipe-on-drop of the access/refresh/id tokens and their redacting `Debug` are
decided in one place rather than per path. The login path applies the endpoint
rule to every endpoint it
sends a credential to — the token and device calls, and the authorization
endpoint before the URL is opened in the browser — refusing a URL whose
authority carries userinfo outright, since the real host sits after the `@`.

The daemon serves each provider's sign-in parameters over a
`ClientMessage::ListOAuthProviders` ⇄
`DaemonMessage::OAuthProviders(Vec<OAuthProviderDescriptor>)` pair, so the
frontend embeds no copy of the issuer, scopes, redirect ports, encodings, or
client identity. The daemon assembles the descriptors
(`choreo-ai-protocols::oauth_provider_descriptors`) from the catalog `oauth`
profile (data) paired with the compiled client-identity table (policy), and
**withholds** a borrowed vendor client id unless the per-install opt-in is
enabled. `/login <account>` (alias `/add-oauth`, `--device` for the headless
flow) targets an **existing** account whose provider selects the flow; the TUI
renders the streamed progress in a cancelable modal. The two wire variants amend
the unreleased v9 changelog in `choreo-proto/src/frame.rs` (no version bump).

#### Provider OAuth refresh (runtime)

The access token is refreshed on demand, in the **session thread** — the
blocking exchange stays in `choreo-ai-protocols`
(`oauth::refresh_oauth_token`, a synchronous `ureq` POST that is the provider
crate's only OAuth responsibility), and it refuses to send the durable refresh
token to any endpoint that is not `https` (a loopback `http` address is the sole
exception, for the in-repo test issuer), so a credential whose stored issuer is
a cleartext URL cannot leak the token at refresh time — the login library
applies the same rule through the one shared policy (`choreo-shared`'s
`oauth_endpoint`), so login and refresh share one implementation rather than two
copies that could drift. The daemon remains the sole owner of the token cache and
the sole writer of the credential blob. The seam between
them is `choreo-daemon/src/oauth_refresh/`: an `OAuthTokenSource` the session
implements over its command channel (`DaemonTokenSource` sends
`DaemonCommand::LoadOAuthToken` / `SaveOAuthToken` / `MarkOAuthDead`) and a
single `refresh_single_flight` driver. A source hands back the durable material
(the token endpoint, encoding, client id/secret, refresh token, scopes) and the
cached token, and takes back a `RefreshedTokens`; the refresh token and client
secret travel wipe-on-drop (`RefreshMaterial` wraps them in `Zeroizing`, like
the credential itself), since the session thread holds them only for the one
exchange. The token endpoint and the refresh body encoding are resolved once, at
login, and stored on the credential (`ServiceCredential::OAuth`'s
`token_endpoint` / `refresh_encoding`), so a runtime refresh reuses the exact
endpoint and encoding the grant was issued against — an overlay edit or a
catalog change cannot repoint or re-encode it. The daemon's handlers update
`access_tokens` and, **only when a
returned refresh token or identity claim differs from the stored one** (an
identical value echoed back is not a rotation), re-encrypt the durable subset to
the binding public key (the same `set_credential_blob` path as provisioning —
no private key, no re-login); a plain expiry refresh writes no blob. A save whose
credential was replaced mid-exchange (a re-login or logout) is dropped
wholesale, so an access token minted from the superseded grant never reaches the
cache either.

Two rules make it correct under rotation. **Single-flight**: the whole
`load → re-check → exchange → persist` runs under the account's lock in
`OAuthRefreshLocks`, so the loser of a race re-reads the winner's persisted
token (the load happens *under* the lock) and reuses it instead of spending a
single-use refresh token twice. The registry holds strong `Arc`s and is pruned
opportunistically (`OAuthRefreshLocks::prune`) once a refresh releases its guard
and no other thread holds the entry — the provider path prunes after
`refresh_single_flight`, the MCP path after a credential save/clear — so it does
not grow one lock per credential ever refreshed; a held guard pins its entry so
a prune can never evict a lock that is in use. `OAuthRefreshLocks` is the
**fourteenth** sanctioned shared-state exception (see AGENTS.md).
**Rotate-if-present**: the engine
adopts a returned `refresh_token` when one is present and keeps the stored one
when it is omitted — one code path for rotating and non-rotating providers — and
replaces the `account_id` claim only when a new `id_token` carries it. That claim
is derived at the one place that persists it (`refresh_under_lock`), from the
returned `id_token` through the shared namespace decode, rather than carried on
the token set: login and refresh read the claim the same way, and the refresh
parse stays a pure status → token-set projection.

A request refreshes proactively when the cached token is absent or within a
hardcoded **60 s** buffer of expiry (`oauth_refresh::REFRESH_BUFFER_SECS`), and
force-refreshes once on a `401` (ignoring the buffer), rebuilding the client
from the daemon's fresh token and retrying the request **once** — never a loop.
The **failure taxonomy** classifies the token-endpoint response in
`choreo-ai-protocols`: `invalid_grant` / `invalid_token` /
`refresh_token_reused` (or any other non-retryable rejection) is **terminal** —
the daemon quarantines the account (`DaemonState::dead_oauth`) so no further
refresh is attempted, and broadcasts a single `OAuthReloginRequired` notice; a
transport error, `429`, or `5xx` is **transient** — a transient proactive
failure does not fail the request when a cached access token is present: the
session proceeds on that cached bearer (the first request `401`s and drives the
force-refresh path if it has actually expired) and only surfaces the refresh
error when no cached token is available. The terminal notice is emitted exactly
once per event because the daemon broadcasts only on the quarantine *transition*,
deduplicating across concurrent sessions and retries. The new
`InferenceError::OAuthReloginRequired`
variant and the `DaemonMessage::OAuthReloginRequired` broadcast amend the
unreleased v9 changelog in `choreo-proto/src/frame.rs` (no version bump); the
TUI surfaces the notice as a status line, GUI/IM as no-ops.

### Request-body compression

Every LLM turn re-sends the whole conversation plus the tool schemas as JSON,
so the request body is the cheapest thing to shrink. A provider whose endpoint
accepts a compressed request body can be opted in with
`request_compression = "gzip"` or `"zstd"` on its `[provider.<slug>]` overlay
entry (default is `identity`): models.dev carries no encoding fact and the
server must decompress, so compression is **opt-in policy**, and a provider the
overlay does not pin is sent an uncompressed body. Because the coding is baked
into the provider client at construction, an overlay edit reaches live sessions
only through the catalog-swap client invalidation (see Runtime catalog refresh).
The bundled overlay pins exactly one provider, `deepinfra` (`gzip`), the only
in-catalog endpoint whose documentation states it decodes a compressed request
body; models.dev carries no such fact, so the list is hand-curated in this
overlay file.

The compression is applied once, in `choreo-ai-protocols`'s
`retry::HookedJsonBody` (after the compact-JSON serialize, alongside the
retained plain bytes), so every retry attempt reuses the compressed
`Arc<[u8]>` and the upload-complete hook — the v9 `Waiting` boundary, see the
wire-protocol section — still fires on the last compressed byte: both the
dispatch/upload boundary and the length-delimited send are unchanged. The
coding and its codecs live in `retry/compression.rs`; two codings are
supported, each chosen per provider by which its endpoint decodes: `gzip` (the
broadly-supported one, via the same pure-Rust `flate2` backend ureq enables) and
`zstd` (targeting endpoints known to decode it, via the same pure-Rust
`structured-zstd` codec the DB uses). A provider that
answers a compressed request with `415 Unsupported Media Type` is downgraded to
identity in place and retried once uncompressed
(`retry::retry_loop_with_provider_policies`); the downgrade is then remembered per
provider for the process, so later requests skip compression entirely. The
memory is the process-wide `retry::compression::COMPRESSION_DOWNGRADED` set —
the **thirteenth** sanctioned shared-state exception (see AGENTS.md), read under
a shared lock on each request of a compression-opted-in provider (the identity
default skips the lookup) and written once per provider on a 415, so it carries
no protocol data and readers never contend.

The downgrade triggers on `415` only, the spec-correct signal for an
unsupported request coding: `400` is deliberately not treated as an encoding
rejection, because it is also the status for a genuinely malformed request
(bad JSON, unknown model, schema violation) that re-sending uncompressed cannot
fix, and the retry layer never treats `400` as an encoding rejection (the one
body-inspecting policy is the separate gateway-400 retry, below — it never
applies to the compression decision). A
provider — or a gateway in front of one — that answers `400` for an unsupported
`Content-Encoding` therefore keeps failing until its `request_compression` is
removed by hand.

#### Transient gateway 400 (message content)

The opencode.ai zen/go gateway validates message *content* on only some of its
routes, so the same request bytes are rejected on one attempt and accepted on
the next (upstream measured a byte-faithful replay of `200/400/200`). This is
the one place the retry layer consults the response body, and it is fenced to
exactly that case: `retry::retry_loop_with_provider_policies` resends the
**identical** body up to two more times when, and only when, the provider slug
is a known gateway slug (`shared::is_opencode_gateway_slug` — the same
exact-match set that gates the gateway routing headers, so the two can never
drift) **and** the terminal `400`'s detail matches one of the gateway's own
content-rejection wordings — `Message content must contain text` or
`Message content supports text content only` — or is the opaque model-only stub
(`{"model":"…"}`, no `error`/`message` field). Every other `400` from every
provider stays terminal, so the status-only "a 4xx is a deterministic rejection"
contract holds everywhere else. The policy lives in `retry/gateway.rs`: its
signature gate matches the gateway's human-readable content wordings (plus the
opaque model-only stub), which is the more specific signal — the `[code]` label
the gateway preserves in its rewritten message is generic (`unsupported_request`)
and would also cover deterministic rejections. That recovered label is used
only to label the retry's log line (`retry::gateway_error_code`), so the failure
class is diagnosable without dumping the prose. The extra attempts are counted
separately from the per-send budget, and a `no_retry` request
(`max_attempts == 1`, e.g. the cache-warming ping) skips the policy entirely.

### Runtime catalog refresh (S4)

The compiled-in catalog is only the *fallback*: at runtime the daemon layers a
local cache and a user overlay on top and keeps the base fresh from models.dev.
One maintenance thread (`choreo-daemon/src/catalog.rs`) owns the whole pipeline
but never mutates the catalog itself — every change is sent to the daemon
command loop, the single writer of the `ArcSwap`, over a channel.

The cross-cutting invariants: the normalized base is cached in the data dir in
the same postcard format as the embedded blob, written atomically, with the
models.dev etag persisted in the DB *after* the bin is on disk so a crash
between the two self-heals (a new etag over stale content would 304 forever).
A models.dev fetch is attempted at most once per 25 h, whatever the last
outcome, anchored on a wall-clock attempt timestamp written *before* the fetch
so the cadence survives restarts — the 25 h period (not 24 h) spreads load
across the daemon population; `/refresh-models` bypasses the cooldown but still
records the attempt. The user overlay (`$XDG_CONFIG_HOME/choreographr/
models-overlay.toml`, the same schema as the bundled layer, merged last at
highest precedence) is watched by the unified config transport, which surfaces
edits to the maintenance thread through a fingerprint gate so editor save-event
storms collapse; deleting the file falls back to bundled-only. Two wire surfaces
ride the refresh: `/refresh-models` (TUI slash command →
`ClientMessage::RefreshModels` → the maintenance thread → `DaemonMessage::
ModelsRefreshed` with `RefreshStatus`, or `ModelsRefreshFailed`; a burst of
requests is coalesced into one fetch) and `CatalogUpdated`, broadcast on every
catalog swap to all activity subscribers so the TUI's provider picker tracks the
live catalog. A swap is not a bare table replacement: because a provider client
bakes its catalog entry's protocol, base URL, and `request_compression` in at
construction (and caches itself on the session), the command loop compares the
pre- and post-swap entries and sends `DropProvider` to every session whose
account's provider had one of those change — or vanish — so an overlay edit
reaches a live session's next request instead of being masked by a stale
client. Only those client-relevant fields are compared: a models.dev refresh
that merely rewrites model lists leaves warm clients (and their connection
pools) untouched. The cache/etag/cooldown mechanics, the conditional GET, and the
fingerprint gate are documented in-source (`choreo-daemon`'s `catalog` module
and `choreo-ai-protocols`'s `catalog::refresh`).

- **`/mcp`.** The TUI's `/mcp` slash command (and the `choreographr mcp` CLI)
  surfaces the state of the configured MCP servers. `ClientMessage::McpStatusRequest`
  is translated by the connection thread into `DaemonCommand::McpStatus` (the
  command loop is the sole owner of the `McpManager`) and the reply is
  converted to the wire `McpServerStatus` records and sent as
  `DaemonMessage::McpStatus`; the TUI renders one server per line (slug,
  transport, target, connected state, tool count, or the recorded last error).
  `ClientMessage::McpReconnect { slug }` maps to `DaemonCommand::McpReconnect`,
  which rebuilds the server's connection and swaps the tool catalogue; on
  success the connection replies with a refreshed `DaemonMessage::McpStatus`,
  on failure with `DaemonMessage::McpReconnectFailed { slug, error }`.
  `ClientMessage::McpReload` maps to `DaemonCommand::McpReload`, which re-reads
  the config files and reconciles the running server set with them
  (add/remove/restart) before swapping the tool catalogue; it replies with
  `DaemonMessage::McpReloaded { summary, servers }` (a change summary plus the
  refreshed list) on success, or `DaemonMessage::McpReloadFailed { error }` when
  the config cannot be read or parsed. The
  offline CLI subcommands (`mcp list`/`add`/`remove`) read and edit the same
  `mcp.json` shape directly, so they work without a running daemon
  (and without the `mcp` cargo feature); `choreographr mcp reconnect`/
  `reload` are minimal one-shot Unix-socket clients for a running daemon.

- **MCP remote-server OAuth.** A remote server may declare an `oauth` block
  (`{ clientId?, callbackPort?, scopes?, authorizationServer?, clientSecretEnv? }`;
  never a literal `clientSecret` — `clientSecretEnv` names the environment
  variable holding a confidential client's secret) in `mcp.json`
  or a trusted project `.mcp.json`. Sign-in is a standard OAuth 2.1 PKCE flow
  owned by the frontend login library (`choreo-client-core`'s `oauth::mcp`,
  split into `mcp::{mod,discovery,register,authorize}`): an unauthenticated
  `initialize` elicits the server's `WWW-Authenticate` challenge (RFC 9728
  §5.1), whose `resource_metadata` URL is preferred as the
  protected-resource metadata location; failing that it falls back to probing
  the well-known path, discovering the
  authorization server (RFC 9728 protected-resource metadata → RFC 8414 or
  OpenID Connect Discovery), building each well-known URL with the segment
  inserted *before* a path-bearing issuer/resource and also appending it
  *after* (the `{issuer}/.well-known/openid-configuration` form a path-issuer
  provider such as Entra/Keycloak/Okta serves — the root document remains the
  fallback); an explicit `authorizationServer` names the issuer directly,
  skipping protected-resource probing. It rejects a
  metadata document whose `issuer` does not match the authorization-server URL
  it was fetched from (RFC 8414 §3.3), tolerating a single trailing-slash
  difference (one shared `issuers_match`); each discovery GET follows a small,
  bounded number of redirects, re-vetting every hop so a redirect cannot steer
  discovery onto a cleartext or internal host. It
  dynamically registers a client
  (SEP-837) when no `clientId` is configured, runs PKCE over a loopback callback
  (paste-URL fallback; no device code), and stores the durable grant as
  `ServiceCredential::OAuth { kind: Mcp }`. A server that issues no refresh
  token — GitHub's remote MCP through an OAuth App is the motivating case, and
  it does so even with `offline_access` requested — has no grant root to store,
  so the long-lived access token it does return is persisted in the credential
  as the deliberate exception to the usual "access token never persisted" rule:
  with nothing to refresh, it is the only credential that can authenticate the
  connect and the only thing that survives a restart. A confidential client's
  secret is read from the `clientSecretEnv` variable, sent at the token endpoint
  (`client_secret_basic` or `client_secret_post`, chosen from the server's
  advertised methods), and stored in the grant so connect-time refresh
  re-authenticates the token endpoint; a `clientSecretEnv` with no `clientId` is
  rejected, because a client with no id is dynamically registered and would
  receive its own credentials, so a configured secret could never match. The
  authorization response's `iss` is
  validated against the discovered issuer when the server advertises RFC 9207
  support (or whenever the parameter is present), rejecting a mix-up before the
  code is exchanged. The check uses the same shared `issuers_match` as
  discovery, so a trailing-slash difference in how the server spells its issuer
  does not force a needless re-login. The scopes requested follow the spec's
  precedence — the configured `scopes`, else the challenge's `scope`, else the
  protected resource's `scopes_supported`, else the authorization server's
  advertised scopes — and the `scope` parameter is omitted when nothing
  applies. The authorization endpoint taken from the discovered metadata is
  vetted with the same policy before the browser is opened, so a server
  advertising a cleartext remote endpoint is refused.

  The policy is **earned by the resource**: the flow computes a `HopPolicy` once
  from the resource URL and permits a loopback `http` hop only when the resource
  itself is loopback (the in-repo mock server), and a non-public IP-literal hop
  (private, link-local, or another internal-purpose range) only when the
  resource is itself loopback or such an IP literal
  (a LAN self-hosted server). Every hop a fetched document chooses — a discovery
  GET or its redirect, the dynamic-registration endpoint, the discovered
  authorization and token endpoints — is vetted under that policy, so a hostile
  protected-resource or authorization-server document cannot steer a hop onto
  `http://127.0.0.1:<port>` or an internal address — `10/8`/`192.168/16`, link-local,
  the `100.64/10` shared (carrier-grade NAT) block, `198.18/15` benchmarking, the
  unspecified address, or `0.0.0.0/8` — and probe the host's own network. The
  IP-literal range check is the
  one shared classifier in `choreo-shared`'s `net` module (the same one the
  provider media download uses). The provider login and the runtime refresh keep
  the permissive default, since their endpoints come from the trusted
  descriptor/credential rather than a fetched document.
  `mcp login <slug>` reads the server's resolved identity, `oauth` block, and
  credential key from the extended `McpStatus` and sends the grant over the
  existing `AddCredential`; storing an `mcp:`-keyed credential makes the
  command loop reconnect the server the key names (the key encodes its slug)
  and refresh the catalogue, so a sign-in is self-sufficient; `mcp logout
  <slug>` removes it via `RemoveCredential` and reconnects. The unlock tail
  (run by both `Unlock` and `BindKeystore`) likewise reconnects every
  `mcp:`-keyed server on the locked→unlocked transition: the daemon starts
  locked and connects its servers before any grant is decryptable, so a restart
  brings signed-in servers up without a manual reconnect, while a redundant
  re-unlock (the TUI auto-unlocks on every connect) leaves the live connections
  alone. Refresh at connect
  is owned by `rmcp`'s `AuthorizationManager`, driven through a
  `CredentialStore` adapter in `choreo-mcp`'s `auth` module that normally strips
  the access token on `save` (serving
  it from an in-process cache; a cold start returns no usable token so `rmcp`
  refreshes) and maps to and from `ServiceCredential::OAuth`. Because `rmcp`
  loads the credential on every request, that cache is a fast path: a cached
  token with more than a 60-second margin of life (deliberately above `rmcp`'s
  own 30-second refresh buffer) is served with no backend read and no durable
  credential clone, while anything nearer expiry — and any cache the remembered
  durable fields no longer describe — falls through to the full load, which is
  what re-reads a rotated single-use refresh token under the refresh guard. The
  exception is a
  grant with no refresh token: there is nothing to refresh, so the adapter
  persists the (possibly rotated) access token in the durable grant on `save`
  and serves it directly as the bearer on a cold `load` (no `expires_in` and no
  receipt time, so `rmcp` skips its expiry check), keeping a server that issues
  only an access token usable across a restart. A configured
  `authorizationServer` overrides that connect-time discovery too, exactly as it
  does at sign-in: `rmcp` has no explicit-issuer setter, so the override URL's
  metadata is resolved by `rmcp`'s own discovery and seeded onto the manager
  (whose base stays the resource, so the RFC 8707 `resource` parameter remains
  the MCP endpoint). The module splits
  the durable↔`StoredCredentials` mapping (`auth::adapter`) from the connect-time
  `rmcp` `AuthClient`/`AuthorizationManager` wiring (`auth::connect`), so each
  `rmcp`-facing concern owns one module.

  The credential row is **target-scoped**: `mcp:<slug>|<target-hash>`, where the
  hash is the repo's stable FNV-1a over the canonicalized server URL joined to
  the slug. The daemon computes the key (one authority for provisioning and
  connect-time load) and exposes it, the resolved `oauth` block, and an auth
  state (`NotApplicable`/`BearerToken`/`NotLoggedIn`/`OAuth`/`Reauthentication`)
  per server in `McpServerStatus`. Load is **fail-closed**: a
  stored credential whose `resource` no longer matches the configured URL is
  treated as absent, and — once the authorization server has been re-discovered
  at connect — so is one whose `issuer` differs from that server (compared with
  the shared `issuers_match`, which tolerates a trailing slash so a spelling
  difference does not force a needless re-login). A re-pointed
  server therefore never receives a token minted by the previous authorization
  server; the connect surfaces a re-login instead. Trust gates *use*, not
  existence — an untrusted project's
  `oauth` block is never expanded or offered, and its stored credential is never
  loaded or sent while untrusted, but survives un-trust and is usable again on
  re-trust with no new login.

  The `CredentialBackend` trait (`load`/`save`/`clear`/`acquire_refresh_guard`/
  `mark_reauthentication`) is implemented by the daemon (`choreo-daemon`'s
  `mcp::oauth`): no `rmcp` type crosses the crate boundary. Because a server can
  connect while the command loop is blocked resolving a session overlay, the
  backend never round-trips the loop for a read — the command loop publishes a
  credential snapshot the backend reads under a short-lived lock (the
  **fifteenth** sanctioned shared-state exception, see AGENTS.md). The slot is
  re-installable: a restarted embedded daemon installs a fresh runtime (empty
  snapshot, its own command sender) so it never keeps a dead command sender or
  the previous daemon's credentials. The snapshot **withholds a quarantined
  grant** (`dead_oauth`): a terminally rejected refresh token is not served
  again, so the connect proceeds unauthenticated and surfaces a re-login rather
  than re-spending a spent token. The `/mcp` status still reports the
  quarantine: the daemon refines each server's `McpAuthState` from the store (a
  quarantined key refines to `Reauthentication`) and the server summary line
  appends that state's label, so a server needing a re-login is visible to the
  operator. The backend's writes are
  fire-and-forget `DaemonCommand`s (`OAuthSave`/`OAuthClear`/`McpAuthRejected`)
  that persist to the binding public key when the loop is next free, but before
  enqueuing each durable command the backend first advances the read-view
  snapshot synchronously (publishing a rotation, or removing the entry on a
  clear/rejection), so a concurrent refresh of the same grant re-reads the
  rotated credential without a command-loop round-trip rather than spending an
  already-rotated single-use refresh token. A save the command loop drops — the
  credential was removed (logout/clear), or a re-login replaced it mid-refresh —
  republishes that key's entry from the durable store, so the synchronous publish
  cannot leave the read view serving a grant the store rejected. Every single-key
  credential mutation publishes **only that key** (`publish_mcp_oauth_key`); the
  whole-map `publish()` is reserved for the two places the entire credential set
  changes wholesale (lock clears it, unlock decrypts it afresh). That per-key
  granularity is what prevents another key's mutation from clobbering a rotation
  that is published ahead of its not-yet-durable `OAuthSave` — a whole-map
  republish built from the durable store would revert such a key to its
  already-spent refresh token, and a concurrent refresh would then spend it. The
  single-flight refresh guard reuses `OAuthRefreshLocks`, keyed by the MCP
  credential key: the registry's one semaphore-backed `OwnedRefreshLock` serves
  both engines, and the async adapter takes its owned `Send` guard from a
  `spawn_blocking` task so it can be held across the refresh `await`.

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
`catalog/loader.rs`, with the entry types themselves in `catalog/entry.rs`, the
fact lookups in `catalog/lookup.rs`, and the OAuth login descriptors assembled
from an entry's profile in `catalog/oauth.rs`): the first access deserializes the embedded base and
merges the bundled overlay once; every later access goes straight to the
`ArcSwap` (an atomic load, then lock-free reads / atomic `store` on swap). The
`ArcSwap` makes the catalog runtime-swappable: readers are lock-free and
`replace_catalog()` atomically swaps the whole catalog (single writer: the
daemon command loop), so lookups return *owned* values cloned out of the
atomic guard rather than `&'static` references.

The same single-writer `ArcSwap` shape is the workspace's **fourth** sanctioned
shared-state exception (see AGENTS.md): the daemon's tool registry
(`DaemonState::tool_registry`, an `Arc<ArcSwap<ToolRegistry>>` shared with every
session and request worker) is read lock-free and swapped only by the command
loop, on `DaemonCommand::McpListChanged` — a wholesale registry swap is what
lets a live MCP list change reach in-flight sessions without a restart.

### Reasoning round-trip (capture → carry → re-emit)

For Anthropic (thinking blocks + signatures), DeepSeek/Kimi
(`reasoning_content`), Gemini (thought signatures), and OpenAI/xAI Responses
(opaque reasoning items + `previous_response_id`), reasoning must be sent back
on the next request or the tool-call loop fails with a 400. The round-trip
payload is an **opaque, provider-owned artifact** handled in four layers: the
catalog decides *how* to send (`ReasoningPassback`, per-model override else
protocol default via `model_reasoning_passback`, plus the chat echo policy in
`chat_reasoning_echo`); each adapter captures the artifact verbatim at its parse
boundary and re-emits it verbatim in its own wire format; the daemon's request
builder (`choreo-daemon/src/reasoning.rs`) decides *whether* to send and never
interprets the bytes; and the chat adapter's lowering (`ChatWireMessage`) decides
*which wire key* carries it. *Whether* is derived, never configured: same-model
provenance (`Turn.reasoning_producer` versus the current provider + model, so a
mid-session model switch drops every old artifact) plus the passback policy —
with the empty-message fallback (a reasoning-only turn is never shipped as a
wholly empty assistant message), the data-driven `reasoning_echo` /
`reasoning_echo_required` facts (ingested from the snapshot's `interleaved` field
name, with no name-based heuristic, and overridable per model by the overlay),
and the `previous_response_id` continuity
chain.

**Which key wins.** The catalog's declared echo field is authoritative and
outranks the field a payload was *observed* under: `ReasoningArtifact::
ChatReasoning` records the observation (a hint, used only when the route declares
nothing) while `ModelEntry::reasoning_echo` states the destination — and, when
the separate `ModelEntry::reasoning_echo_required` fact says so, that the field
must be present. There are **two destinations**: a declared scalar field
(`reasoning_content`, `reasoning`, or `reasoning_text`) or the declared
structured `reasoning_details` array. Destination and presence are **two
independent catalog facts**, each pinnable on its own — a presence requirement
with no declared destination resolves to the chat default key
(`field: None, require_present: true`), which the lowering emits empty — and the
lowering (`choreo-ai-protocols`'
`openai::chat_projection`, which owns the projection) applies both in one place so they
cannot drift apart. Writing a scalar destination on *every* assistant message —
the artifact's text when there is something to replay, an empty string when
there is not — is exactly the presence case (`require_present`), not a
consequence of declaring a field. Ingestion keeps the two aligned with what the
upstream actually enforces: `interleaved` naming a scalar field sets BOTH facts
(a scalar name has always meant the upstream demands the key echoed), while
`interleaved` naming the structured array sets only the destination. This is
why a gateway that streams
DeepSeek thinking under `reasoning` while
rejecting any history without `reasoning_content` cannot starve the required
field. The artifact never leaves the daemon process — client-bound
`SessionEvent`s carry it as `None`. The variant semantics and the resolver
contracts are documented on `ReasoningPassback`, `ChatReasoningEcho`, and the
resolver functions.

**Structured details.** Some chat routes carry reasoning as a structured
`reasoning_details` array instead of a scalar string (models.dev's `interleaved`
names it for the OpenRouter resellers and a few gateways). The artifact then
stores the array as JSON under `ChatReasoningField::ReasoningDetails` rather than
text: display text is derived from its text/summary entries when no scalar field
streamed, and an encrypted-only array still counts as model output (never an
empty response). The live display comes from exactly one channel — whichever
channel produced a fragment first claims it for the rest of the turn, and the
other channel then contributes nothing to the display — because a route that
populates both sends the same reasoning twice and emitting both renders it twice
in the client. The rule is symmetric: a route may stream the scalar field first
and the structured array later just as easily as the reverse, so neither channel
is privileged. Processing order inside a delta stays details-first, then the
scalar fields, so a chunk carrying both lets the structured entries claim first.
Both channels are still captured regardless of who owns the display — the scalar
text is accumulated all the same, since it stays the display and artifact
fallback for a route that streams only the scalar field, and the array is
captured even while the scalar field is on screen. All three capture paths —
both streaming request paths and the non-streaming response conversion —
delegate those rules to one shared state machine
(`openai/chat_completions/reasoning_stream.rs`) rather than each reimplementing
them, which is what keeps the plain prompt path, the tool path, and the
non-streaming turn result interpreting reasoning identically (including the
first-non-empty scalar precedence that stops an empty field masking a populated
one). Capture normalizes the array under a
documented per-turn byte cap, beyond which the *replay payload* is dropped and
logged rather than failing the request. Three answers are kept apart here, which
is what stops a storage bound from costing anything but the echo: the turn's
display text is a fold over the fragments (unbounded, like the scalar channel), the
payload is what the cap bounds, and "did the model produce output" is a fact
reported by the fold — never inferred from whether a payload survived. An
over-cap turn therefore still shows its reasoning, and an encrypted-only turn whose
payload was over the cap is a `FinalText` with no visible text rather than an
`EmptyResponse` error. That bound and the per-request replay budget below are
**one number** (`reasoning_details::MAX_REASONING_BYTES`): an artifact larger
than the request budget could only ever ride as its "newest always
replays" overshoot, so a larger per-turn cap would have no replay path. Two
bounds still work together, because the payload is persisted per
turn and replayed on *every* later request of the tool loop: the per-turn cap
limits what one turn may persist, and the request build that lowers the history
applies a separate per-request budget to the total artifact bytes it replays
across all assistant messages. The request walks the history **newest-first**:
the most recent artifact-carrying assistant turn always replays, even when that
single artifact alone exceeds the budget, and each older turn replays only while
the bytes already accepted stay within the budget. A turn the budget skips is
lowered with no artifact but under the same echo policy, so a route that
requires the key still emits its presence marker (an empty string, or the empty
details array) and only the payload is lost. Without that second bound a long
tool loop would build a request body that grows roughly with `turns × cap` until
the provider rejects it outright: losing an old turn's echo degrades a route
that requires it, while an oversized body fails the whole turn. The scalar
artifacts are not capped at capture at all — real thinking can exceed the
per-turn cap and the DeepSeek/GLM echo requirement needs it replayed — so they
are bounded by that same request budget rather than by capture. The fragments of
one stream slot merge into one block: a slot is the entry's `index` (equal
`index` on both entries) or, where the provider names none, the newest
index-less entry of that kind, and each slot remembers its own accumulating
entry — so a route that interleaves two slots across deltas still merges both
rather than starting a fresh entry per fragment. Entries carried by one delta are
distinct blocks and never merge on absent-index evidence, and a mixed presence
is ambiguous and stays unmerged, because fusing on absent evidence could rewrite
a sequence the upstream requires to match. A merge is also refused when the two
entries state a conflicting identity — a different `id`, `format`, or
`signature` — since that is two distinct blocks sharing a slot number. OpenRouter
states the rule this protects: the entire sequence of reasoning blocks must match
the output of the original request and may not be rearranged or modified
([docs](https://openrouter.ai/docs/guides/best-practices/reasoning-tokens#preserving-reasoning)),
and GLM says the same of its preserved-thinking blocks
([docs](https://docs.z.ai/guides/capabilities/thinking-mode)). Encrypted entries
stay discrete (they are individually opaque), and an entry whose `type` is not
modelled is kept and replayed **verbatim** — it contributes nothing to display
text, but dropping it would again alter the sequence the model produced. The
lowering replays OpenRouter's namespace-prefixed `reasoning.*` entries verbatim
(index, format, and signature intact, as its documented array shape specifies —
[spec](https://openrouter.ai/docs/guides/best-practices/reasoning-tokens#reasoning_details-array-structure)).
A **bare** entry (an undocumented single-word `type`) is the one exception to
verbatim replay: it loses only the stream-only `index` and keeps everything else,
the payload key included, because no upstream documents that shape — Kimi's own
documentation uses solely `reasoning_content`
([docs](https://platform.kimi.ai/docs/guide/use-thinking-models)) and models.dev's
snapshot vocabulary is `reasoning_content` or `reasoning_details`
([schema](https://github.com/anomalyco/models.dev/blob/dev/packages/core/src/schema.ts)) —
so a rebuilt entry could drop an opaque block under a guessed key. The array is
the **only** reasoning field such a message carries — a scalar mirror beside it
would be a shape no upstream documents, and OpenRouter's guidance is one form
*or* the other, so the companion is not emitted. The array is a **destination
fact only**: a declared details route with nothing to echo emits *no* reasoning
key,
because no upstream is known to reject a history that omits the array and an
invented `reasoning_details: []` would send an unverified array to a gateway
that is strict about its contents. An **undeclared** route is the same shape for
a different reason: it only reaches the array through the observation, so when
the artifact lowers to nothing (corrupt or over the cap) no key is emitted at
all. A route whose separate presence fact (`reasoning_echo_required`) demands
the array still carries it empty on every assistant message — an empty array is
a presence marker, not a payload, so it never keeps an otherwise-empty assistant
message alive. Capture tolerates a value that is not an array or object only as
far as one JSON decode: a proxy that double-encodes the array as a JSON string is
decoded once and folded like a real array, and anything still not an array or
object (a bare number, a string of non-JSON) is ignored and logged by kind alone
— never logged by content, never invented into an entry. A scalar payload
reaching a structured route (a catalog or overlay flip changes a model's
declared field) is replayed as one `reasoning.text` entry rather than discarded,
the counterpart of deriving display text from a structured payload on a scalar
route.

**Empty turns never reach the wire.** An assistant turn that produced no visible
content (blank text counts as none), no tool calls, and no reasoning payload is
dropped from the provider-facing projection by every adapter — a message with
neither content nor tool calls is rejected outright by chat servers, and an
empty `content`/`parts` array is rejected by Anthropic and Gemini. The turn stays
in the session (the transcript, diagnostics, and `session_inspect` still see it);
only the request projection omits it. A turn whose *only* payload is a replayed
reasoning artifact is not empty and always survives — dropping it would starve
the very echo this section is about — while a required-field empty string is a
presence marker, not a payload, so it never keeps an otherwise-empty message
alive.

### Supported providers

Currently supports 209 providers (184 from the models.dev base + 25 overlay-only). Adding or refreshing a provider is a data change in `catalog/`: update the snapshot (or add an overlay entry) and re-run `cargo run --bin catalog-gen` — zero client code.

**Supported providers by protocol:**

| Protocol | Providers (overlay-only providers in *italics*) |
|---|---|
| OpenAI-compatible | OpenAI, DeepSeek, Mistral, xAI, Groq, Together AI, OpenRouter, Hugging Face, GitHub Copilot, NVIDIA NIM, Cerebras, Fireworks AI, Xiaomi MiMo, Alibaba (Qwen), Moonshot AI, Perplexity, Z.AI, Qwen Token Plan, Venice AI, Novita AI, LM Studio, Ollama Cloud, OpenCode Zen/Go, DeepInfra, Upstage, StepFun, Inception, Meta, NEAR AI, OrcaRouter, Routstr, Sakana, SaladCloud, Scaleway, OVHcloud, FuturMix, EmpirioLabs, Friendli, Atomic Chat, custom OpenAI-compatible — plus the overlay-only providers that keep their pre-models.dev slugs: *aimlapi, GitLawb OpenGateway, Kilo Gateway, ChatGPT (Codex subscription), iFlytek, Nous, Arcee, GMI, Zhipu, Bankr, Atlas Cloud, Ant Ling, oMLX, Qwen Token Plan (CN), Tensorix, Tanzu, Llama Swap, kimi-code, ollama, openai-compatible, …* |
| Anthropic Messages | Anthropic Claude, MiniMax, Vercel AI Gateway, Kimi Code, Fireworks (Anthropic mode), OpenCode Go (Anthropic-compatible), custom Anthropic-compatible |
| Google Generative AI | Google Gemini |

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

**Per-client architecture (OS threads):**

```
client_thread(socket)
├── reads ClientMessages from socket via choreo-proto read_message_sync
├── sends DaemonCommands via daemon_tx crossbeam channel
└── receives DaemonMessages via per-client crossbeam receiver → writes to socket
```

**Thread topology:**

```
main()
├── listener thread — UnixListener accept loop (non-blocking poll)
│   └── per client: spawns client_thread (std::thread::spawn)
├── metrics HTTP thread — (optional) serves /metrics at `--metrics-addr`
├── command thread — DaemonCommand receiver loop (daemon_tx crossbeam)
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

## Token Usage & Context Window Tracking

Token usage is the protocol-agnostic `TokenUsage` struct
(`choreo-proto/src/types/common.rs`) — `input_tokens` / `output_tokens` /
`total_tokens` plus the prompt-cache `cached_tokens` / `cache_write_tokens`
counts — whose per-field-max `merge_max` is the merge policy shared by the
daemon's mid-turn sync and the TUI's attach-snapshot merge. For every provider
`input_tokens` is the **full** prompt count: the prompt-cache `cached_tokens` /
`cache_write_tokens` counts are a **subset** of it (priced separately, so
tracked on their own), and a provider whose raw input counter excludes cache
(Anthropic's post-breakpoint `input_tokens`) folds the cache counts in at the
provider boundary — so `last_prompt_tokens` is the live turn's full prompt size.
`TokenUsage::accumulate` is the per-turn **summation** counterpart to the
per-field-max `merge_max`: it rebuilds the lifetime total from the turn history
(on load) and is the same fold the worker uses for its incremental
accumulation, so the two cannot diverge on which fields they add.

Two cross-cutting accounting contracts:

- **The billing counter vs. the context payload.** Each session tracks an
  `accumulated_usage` billing total (persisted on the session record and
  re-derived from per-turn usage on load) *and* a separate
  `last_prompt_tokens` (the latest live turn's `input_tokens`) — the
  actual context size the model last saw, used for the context-window progress
  display (`last_prompt_tokens / context_window`), never a cumulative sum.
  The two diverge under undo: `accumulated_usage` sums **every** turn (undo does
  not refund billed tokens), while `last_prompt_tokens` is derived from the
  **live** turn set — the last non-undone turn's `input_tokens` — so `undo`/`redo`
  recompute it and re-broadcast a `TokenUsageUpdate` (a `None` when every turn is
  undone, clearing the readout). A turn with no reported usage contributes
  nothing; there is no local estimate fallback.
- **Mid-turn freshness.** The request worker accumulates on a **private clone**
  of the session state and merges it back at `RequestFinished`; to keep every
  consumer fresh *mid-turn*, the worker's cumulative total is routed through
  `SessionCommand::SyncAccumulatedUsage` to the session's main thread, which
  applies it as a per-field **max** (`TokenUsage::merge_max`) — so an
  out-of-order or overlapping sync can never regress a total a client already
  saw — re-broadcasts `TokenUsageUpdate` from the authoritative state, and
  refreshes the daemon's metadata index without a `last_modified` bump.
  `last_prompt_tokens` is not cumulative, so clients gap-fill it from snapshots
  (never overwriting a fresher value) rather than max-merging it, while the
  authoritative `TokenUsageUpdate` path (mid-turn sync and undo/redo) overwrites
  it unconditionally — including a `None` that clears a stale readout.

The per-turn extraction in each adapter, the `SessionState` →
`SessionEvent`/`SessionSummary` field plumbing, the context-window resolution
chain (`resolve_context_window` → per-model config → catalog fallback,
re-resolved on load when absent), and the client displays are documented
in-source (`choreo-daemon`'s `requests`, `sessions`, and `providers` modules and
`choreo-proto`; `cargo doc -p choreo-daemon` / `just doc`), held complete by the
`#![warn(missing_docs)]` + `doc-check` gate.

All session fields use `#[serde(default)]` so old persisted sessions remain
compatible (deserialize to zero usage).

---

## Security model

### Lock/Unlock flow

Each daemon's credential keystore is governed by a keypair whose private half (the
**unlock key**) is held client-side, one per daemon, in the client's
`known_servers.toml`. The daemon stores only the public half as a **binding**, and
it starts **locked** with no credentials in memory. The binding is created by
exactly ONE wire path, `ClientMessage::BindKeystore` (TOFU): on an unbound
keystore it ADOPTS the presented key (loud `KEYSTORE BOUND` log), runs the shared
unlock tail, and replies the targeted `DaemonMessage::Bound` — bind implies
unlock; on an already-bound keystore it verifies only (a mismatch is the usual
wrong-key rejection, never an overwrite). `Unlock` is strictly VERIFY-ONLY:
against an unbound keystore it answers the targeted
`DaemonMessage::KeystoreUnbound` (deliberately distinct from `LockedError`,
which means "bound but wrong key") and never adopts anything. Binding keys are
ALWAYS freshly generated by the client's CSPRNG at bind time (auto-bind — there
is no `/bind-key` command); pre-held keys (stored known_servers key or the
legacy raw `identity.pk` file) verify existing bindings and never create new
ones. Binding is TOFU-once — rotation is not implemented.

A client sends `ClientMessage::Unlock { private_key }` (resolved verify-only as
its stored per-daemon unlock key, else the legacy raw `identity.pk` file —
copied into the store on first use; a caller-supplied base64 key from
`/unlock <key>` is decoded WRITE-FREE — recording happens only on the daemon's
targeted confirmation). `AddCredential` carries the credential as a TYPED
plaintext value and no key at all (`AddCredential { service, credential }`):
the client neither resolves nor transmits an unlock key, and the daemon
ENCRYPTS the credential to its own binding public key before persisting it. The
add path requires an already-UNLOCKED keystore — a locked daemon rejects it with
`CredentialAddFailed` ("unlock the keystore first") and does NOT change lock
state (there is no implicit unlock). The
client records an unlock key per-daemon only after the daemon's targeted
confirmation (`Unlocked` / `Bound`) — except the auto-bind key, which is
recorded PRE-SEND (mandatory: an unbound daemon adopts whatever key arrives
first, so even a lost confirmation leaves the recorded key matching the
binding). Nothing is ever auto-deleted; `KnownServers::remove(addr)` re-pair is
the only removal path.

**ORDERING INVARIANT:** the targeted reply to Unlock/BindKeystore is enqueued by
the daemon command loop into the acting client's writer sink BEFORE any
lock-state transition broadcast — both travel the same per-client FIFO writer
queue, so the broadcast can never overtake the reply. (AddCredential also
enqueues its targeted `CredentialAdded`/`CredentialAddFailed` reply on this
thread, though the handler never produces a lock-state broadcast.) Client
key-recording correctness keys on this (implemented via `DaemonCommand`
carrying the client's `SubscriberSink`; pinned by the integration test
`unix_targeted_reply_precedes_lock_state_broadcast`).

```
startup              connect→unbound?        /unlock [key]            AddCredential{service, credential}
   │                       │                       │                          │
   │  locked               │  AUTO-BIND (once per  │ verify-only resolve      │ requires an UNLOCKED
   │  (no credentials)     │  connection): mint    │ unlock key; unbound →    │ keystore (locked →
   │                       │  fresh key, record    │ KeystoreUnbound →        │ CredentialAddFailed, no
   │                       │  PRE-SEND, BindKeystore│ auto-bind; match →      │ state change); encrypt
   │                       │  → Bound (implies     │ decrypt all credential   │ the credential to the
   │                       │  unlock)              │ blobs, load accounts     │ binding pubkey, persist
   ▼                       ▼                       ▼                          ▼ (no lock-state change)
```

- Credentials are encrypted per-credential with ECDH (X25519) + HKDF + AES-256-GCM, keyed
  to the daemon's binding public key; the daemon ENCRYPTS each incoming credential to its
  own stored binding before persisting it (one key per keystore).
- The unlock key is sent over the channel (by `Unlock`/`BindKeystore`); zeroized after use
  by the daemon
- A credential is one of `ApiKey`, `X`, `Substrate`, or `OAuth`. The `OAuth` variant
  carries the durable grant (the refresh token, the client registration, the
  identity claim extracted from the id_token, and — for a provider grant — the
  token endpoint and refresh body encoding resolved at login, so a runtime refresh
  does not depend on the catalog profile; never the raw JWT), discriminated by
  `OAuthKind` (`Provider` or `Mcp`). The ephemeral access token is not part of
  that durable shape, with one deliberate exception: an MCP grant whose
  authorization server issued **no refresh token** persists its long-lived access
  token, because with nothing to refresh it is the only usable credential — absent
  it the variant carries no access token. Every variant
  participates in `#[zeroize(drop)]` and redacts every secret field in both `Display`
  and `Debug`
- `/lock` destroys all in-memory credentials, returning to locked state
- **The locked state is authoritative and broadcast.** `DaemonState` keeps a single
  `locked: bool` (starts `true`; `false` after a successful Unlock or BindKeystore
  — the shared `unlock_tail` is the one place it is cleared; `true`
  again on `/lock`, which clears in-memory credentials/providers). On a REAL
  lock-state transition the daemon broadcasts the current state to ALL activity
  subscribers (`DaemonMessage::Unlocked` / `DaemonMessage::Locked` — the existing
  flat variants, no wire change) via `broadcast_lock_state`, and a
  freshly-connecting activity subscriber is sent the CURRENT state immediately
  (alongside the send-on-subscribe `CatalogUpdated`), so a client that connects to
  an already-locked daemon learns so without waiting for a transition. The acting
  client additionally keeps its targeted per-action reply (an
  `Unlocked`/`Locked`/`LockedError`) — enqueued through the request's
  `ReplyTarget` (carried on `Unlock`/`BindKeystore`/`SaveCredential`) so it is
  id-stamped like every other reply; receiving the broadcast too is idempotent.
  This is what makes client B's unlock re-latch client A's banner.
- `LockedError` is sent if any client attempts a request that requires credentials while locked
- **Multi-client provisioning:** the first client to connect to a fresh (unbound) daemon auto-binds it with a minted key; other clients get `LockedError` until the key is shared out-of-band (add `unlock_key` to their `known_servers.toml`, or `/unlock <base64-key>`). A daemon DB reset invalidates its binding; the next connect auto-binds a fresh key (the stale stored key is replaced by the new bind's pre-send record).
- Session lifecycle operations (CreateSession, AttachSession, ListSessions, etc.) succeed even
  when locked — credentials are only needed at RunInput time. Provider resolution is lazy:
  when RunInput is called, the session thread resolves the InferenceProvider from the daemon's
  provider registry. If no credential is available for the session's account, a clear error is
  returned telling the user to add a key.
- **Model lists are warmed in the background, never on unlock.** Unlock (and credential/account
  mutations) resolve providers in-memory only — no network I/O touches the daemon command loop.
  Instead, the daemon spawns a detached prefetch thread whenever a session joins an account:
  on CreateSession, AttachSession (both branches — already-active and loaded-from-db), and when
  `UpdateMetadata` changes a session's account. The thread calls `list_models()` and reports
  back via the `DaemonCommand::ModelPrefetchResult` command so the command loop — the single
  writer of `model_cache` — records the result; a per-account `model_prefetch_in_flight` guard
  deduplicates bursts of joins and is released even on fetch failure. The shared
  `MODEL_CACHE_TTL` (5 min) governs both prefetch freshness and the on-demand path in
  `handle_list_models`, which NEVER fetches on the command loop: a fresh, non-empty cache
  answers immediately; a stale/missing cache triggers the same background prefetch (no-op
  while one is in flight, so an open picker cannot double-fetch) and answers with the best
  list it already has — the stale entry, else the local catalog's curated ids for the
  account's provider (an instant first paint, so the picker never blocks on a slow
  `/v1/models`), and only then a retryable "warming" error. Both stand-ins are flagged
  `provisional` on the wire, so a picker can say the list is not (yet) the account's verified
  one, and both the prefetch gate (`should_prefetch_models`) and the on-demand path treat an
  EMPTY cache entry as absent (`fresh_model_list`), so an empty entry never suppresses a
  fetch. Every list the daemon
  serves is ordered case-insensitively by id at the point it enters the daemon (a fetched
  result and the catalog seed, `sort_model_ids`), so the cache, the `Models` reply, the
  `/model` listing and the broadcast below cannot disagree. When a fetch lands the loop caches
  the result unconditionally (the insert is what refreshes the TTL) and broadcasts
  `DaemonMessage::ModelsUpdated { account, models }` on the ACTIVITY bus UNLESS the entry was
  already a fresh, non-empty list identical to the result; a stale or absent entry is always
  re-announced even when the ids match, so an open picker's `provisional` label clears once
  the live fetch confirms it. The broadcast lets an already-open picker replace its list in
  place without moving the row under the cursor. A panic inside the fetch
  thread is caught (`catch_unwind`) and reported as a normal failure so the in-flight guard
  can never leak, and a result arriving for an account whose provider was removed or rebuilt
  mid-flight is discarded instead of cached.
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

The MCP path's heaviest third-party dependency is **`rmcp` 3.5** (the official Model
Context Protocol Rust SDK, Apache-2.0), linked by default now that the `mcp` feature
ships on. It is pinned (a deliberate upgrade, not an auto-follow of `3.x` churn),
declared with `default-features = false` so only the needed transports compile, and
resolves from crates.io like every other package — so the four layers above cover it
exactly as they cover the rest of the tree. Its HTTP transport shares one compiled
`reqwest` 0.13 build with `alloy` and `choreo-mcp` (the lockfile's other `reqwest`
major, 0.12, predates MCP and comes from `blitz-net` → `dioxus-native` → `choreo-gui`;
`deny.toml` keeps `multiple-versions = "warn"`).

### MCP client trust boundary

The MCP client treats every server as untrusted and every server-supplied string as
data, never as instructions or configuration:

- **Untrusted inputs.** Tool/resource descriptions, `title`, `icons`, `instructions`,
  and resource text are rendered to the model as data only (the wrapper prefixes each
  tool description with `[MCP <slug>] `) and are never executed or used to drive a
  security decision.
- **Schema safety.** Tool schemas are bounded before use: a schema over 256 KiB, deeper
  than 32 nesting levels, or not an object drops that tool while the rest of the
  catalogue is kept (per the spec's "exclude the offending tool" rule). A server's
  catalogue is capped at 1024 tools. No network `$ref` dereferencing is performed.
- **Header discipline.** Only `Mcp-Method` / `Mcp-Name` / `Mcp-Param-*` are generated
  (by `rmcp`, from the request body); user-configured `headers` are validated up front,
  reserved transport headers are rejected, and config values never become header names.
  Config-file secrets are never logged unredacted.
- **Process safety.** A stdio server is spawned as an explicit executable plus args
  (never a shell string) in its own process group, with a byte-capped read path (an
  over-8-MiB frame drops the connection) and kill escalation on shutdown: a graceful
  close awaits `kill()`, and a dropped transport sends a synchronous process-group
  `SIGKILL` (`start_kill`, no runtime required), so an abandoned connection — a
  detached connect thread, a teardown with the sidecar runtime already gone — never
  leaks its process tree. A per-server
  log file is size-capped and created owner-only (0600, Unix), so its captured server
  stderr is not group- or world-readable.
- **Environment hygiene.** The code-injection environment variables (`LD_*`, `DYLD_*`,
  `PYTHONPATH`, `PERL5LIB`, `RUBYLIB`) are stripped from every spawned child — the shell
  tool and the MCP stdio server share one canonical list — but the *remaining* daemon
  environment is inherited by the child unchanged, because a blanket `env_clear()` would
  break servers that rely on inherited `PATH`/`HOME`/`SSH_AUTH_SOCK`. Operators must
  therefore **not** rely on the daemon's environment to carry secrets to an MCP server;
  set only what a server needs via that server's `env` in the MCP config (optionally a
  `${VAR}` reference expanded from the environment), where the value is scoped to that
  one server rather than leaked to every spawned child.
- **Capability honesty.** The client never advertises elicitation/sampling/roots it
  cannot honor, so a server demanding an undeclared capability fails with the missing
  capability surfaced rather than a silent hang.
- **Least privilege.** An MCP server runs with the daemon user's authority; the mobile
  tool policy excludes MCP entirely, and this adds no auto-approval — tool invocation
  keeps the same broadcast/visibility semantics as every other tool.
- **Egress.** A remote server is contacted only when explicitly configured; there is no
  discovery of servers from arbitrary content. Credentials are a static token in
  `headers` or a keystore-stored OAuth grant (see the MCP remote-server OAuth
  section), never shared across issuers.

### MCP connection lifecycle (off the command loop)

Connecting an MCP server is I/O (spawn, handshake, `tools/list`, OAuth refresh), so it
never runs on the daemon command loop and never before a client's acknowledgement. The
command loop's only MCP job is to install a finished result and swap the tool catalogue
(the sanctioned single-writer `ArcSwap`, exception #4); the handshake itself runs on a
worker thread that posts a `DaemonCommand` back.

- **Startup.** `DaemonState::open` only loads `mcp.json` and queues the daemon-tier
  `shared` servers; the socket is bound and the first client served immediately. The
  daemon core drains the queue and starts a background connect per server once the
  command channel and OAuth runtime exist; each connect posts a
  `DaemonCommand::McpConnected` that the loop installs and rebuilds the cached
  catalogue from. A request already on a turn keeps its previous registry snapshot; the
  next turn sees the server's tools. The per-server connect budget is generous (60 s)
  precisely because it is off the critical path.
- **Unlock.** The daemon starts LOCKED and runs those startup connects while an OAuth
  grant is still encrypted, so such a server 401s; the unlock tail schedules a resync
  of every `mcp:`-keyed server, but only AFTER the acting client's `Unlocked` reply and
  the lock-state broadcast are enqueued, and on background workers — so an unlock never
  blocks the reply or the loop. A resync requested while a connect is already in flight
  is coalesced: a failed in-flight connect retries once with the now-live credentials.
- **Session overlays.** A session's project / per-session servers are planned cheaply
  (pool-only), pushed as a partial overlay, then connected on a worker that posts one
  `DaemonCommand::McpSessionConnected` **per entry as it settles** — so a fast server's
  tools land without waiting for a slow sibling's handshake. The result is installed
  only when the session's resolve generation still matches (which advances only when
  the resolve RELEASES the session's connections, i.e. a project-root or trust change)
  and the entry is still wanted; a re-resolve that reuses the connections keeps the
  generation and skips entries a worker is already connecting, so one entry never gets
  two simultaneous handshakes. `/mcp reload` likewise drops changed servers and defers
  their reconnects, and a manual `/mcp reconnect` of a daemon-tier *shared* server parks
  its reply on the connect rather than blocking the loop. A project or per-session
  server's manual reconnect still rebuilds in place (bounded, user-initiated), because
  its connections are per session, not pool-wide: routing it through the daemon-tier
  connect would pool a second, shared connection whose tools duplicate the session's
  private ones. The same rule governs the post-unlock and post-sign-in bring-up: a
  shared server reconnects on a worker, while a `shared = false` server has its stale
  per-session slots dropped so the holding sessions re-resolve and reconnect them.

Two values inside the `rmcp` engine are sanctioned shared-state exceptions (see
AGENTS.md). The live `RunningService` handle sits in a
`tokio::sync::Mutex<Option<..>>` (the **eighth** exception) — a
connection-lifecycle handle closed exactly once and never touched per message,
holding the lock only across the bounded `close_with_timeout` on shutdown. The
`ClientHandler`'s notification rate limiter carries a shared
`Arc<std::sync::Mutex<LimiterState>>` (the **ninth** exception) because `rmcp`
delivers notifications on more than one task; the lock guards a handful of
integers and is never held across an `await`.


---

## Tool system

The daemon's tool machinery — the typed `Tool` trait, its type-erased `ToolDyn`
counterpart, the `ToolRegistry` that owns and dispatches every tool, and the
`define_tool!` macro — is documented in-source (see `choreo-daemon`'s `tools`
module, `cargo doc -p choreo-daemon` / `just doc`), held complete by the
`#![warn(missing_docs)]` + `doc-check` gate.

The system has three dispatch paths with distinct callers:

| Path | Input | Output | Caller |
|---|---|---|---|
| `ToolDyn::execute_json` | `&str` (JSON) | `Result<ToolOutput, ToolError>` | LLM tool calls (OpenAI/Anthropic …) |
| `ToolDyn::execute_streaming_json` | `&str` (JSON) | `Result<ToolOutput, ToolError>` | streaming shell/VM tools |
| `ToolDyn::execute_postcard` | `&[u8]` (postcard) | `Vec<u8>` | RISC-V VM guest ecalls (see "Postcard binary encoding") |

The two JSON paths take a `ToolOutputFormat` so the caller chooses `Text`
(human/LLM rendering via `Tool::return_string`) or `Json` (Programmatic Tool
Calling — the typed return serialized with `serde_json`). A tool's `Error`
associated type is per-tool: simple tools use the string-wrapper `ToolExecError`;
tools whose structured errors are consumed by VM guests (e.g. `DbError`,
`HttpError`) define a `thiserror` enum that is serde-serializable, so the guest
can pattern-match on specific variants. Tools gate which callers may invoke them
via `allowed_callers()` (direct model call, programmatic PTC call, or both), and
tools that need session context receive it through `ToolContext`. A tool that
produces an image carries it on its typed return value, drained by the request
worker over an out-of-band multi-message channel (an MCP result may carry several
image blocks) rather than embedded in the response struct.

**Invocation descriptions.** The human-readable sentence `Tool::describe_invocation`
produces before execution is stored in `ToolOutput.invocation_description`, carried
onto `ToolResultRecord.invocation_description`, and rendered by clients as the
first line of the tool-result block. It is seeded onto every placeholder result
when the model's tool calls are recorded, so clients render the tool's context
(e.g. "Running command: `…`.") the moment the seeded turn is broadcast — before
any output streams. It is deliberately excluded from LLM message construction:
the model never sees it.

Each tool call executes with an optional `working_dir` — the session's working
directory — that filesystem and Git tools resolve relative paths against. A
leading `~` or `~/` in any path argument is expanded to the user's home directory;
the `~user` form is passed through unchanged.

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
  log fields and invocation descriptions; the file tools on their path labels and
  invocation descriptions — `read_file`'s header, `line_count`'s result, `edit_file`'s
  result, and every file tool's `describe_*_invocation` path). The same policy now
  covers the raw-content readers: `render_streamed_line` sanitizes every `read_file` line,
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
- **Sink.** `choreo-tui`'s terminal-safety filter (`src/text_filter.rs`, the one home
  of the policy, shared by the markdown renderer, the plain widgets, the stored
  status text, and the OSC field strippers) exposes `normalize_ansi_text`, which
  runs over every tool-result body before rendering: it keeps complete SGR color
  sequences (`ESC [ … m`, so ANSI
  coloring still works) and escapes everything else — OSC/CSI/DCS sequences, C0/C1
  controls (including lone CR: a carriage return not followed by a line feed
  would let hostile content overwrite its own rendered line; a CRLF pair is folded to
  a single `\n`, matching the daemon's line sanitizers), U+2028/U+2029, and the Cf spoofing class (via the same shared
  `choreo_sanitize::is_unsafe_unicode`). The same pass expands tabs to 4-column
  spaces, so the text measured is the text drawn. This is what makes the raw
  shell/VM streams safe to draw; it defends against terminal-escape injection (OSC-52
  clipboard writes, clear-screen, title changes, …) for every tool at once, including
  the tools that never sanitize at the source. The module's third entry point,
  `sanitize_pasted_text`, is the *editing* rule described under the two-tier policy
  below: it rewrites rather than escapes, because its output is stored and submitted
  rather than drawn.
- **Model output is filtered at the same sink.** The markdown renderer (the
  response, the reasoning body, user text, and tool descriptions) runs
  `normalize_display_text` over its source before parsing it: the same fused pass with
  SGR passthrough **off** (nothing there parses escape
  sequences, so a kept one would only draw as literal `[31m`), tabs expanded in the
  same scan. Besides making hostile model output inert, this closes a
  measurement/drawing desync: `unicode-width` counts a control character as one
  column while ratatui filters it out of the drawn grapheme stream, so a row
  carrying one measures wider than it renders — which rags a code box's right
  border on tab-indented lines. The same normalizer is applied by every
  plain widget that *displays* rather than authors its text — the status bar's session
  identity fields (working directory, account, model, reasoning effort, tool
  groups), the session-status label (whose tool name is MCP-supplied), tool-result
  labels, the session table and detail pane (title, model, working dir, account,
  tool groups), image block titles (including the model-chosen tool-call id in a
  placeholder), the provider/model/credential modals and their delete
  confirmations, and the OAuth login popup (whose status, authorize URL, and
  device code come from the authorization server) — via `render::display_text`.
  The rule the renderer holds to is that **every row measures exactly what
  ratatui draws**; any producer that hands a widget text it did not author must
  normalize it first (clean text borrows, so the common case costs one scan).

  **The OSC fields classify the same characters.** The OSC 2 window title and the
  OSC 7501 `title`/`msg` values are free text shown outside the grid, and their
  strippers use the identical Unicode policy — C0/DEL/C1 plus the shared
  `choreo_sanitize::is_unsafe_unicode` class (every non-joiner format character and
  U+2028/U+2029). Only the action differs: the grid **escapes** (the text stays
  readable and keeps measuring exactly what it draws), while an OSC payload
  **drops** (a payload body cannot carry an escape sequence at all, and OSC 7501
  forbids control bytes — a decoded one makes the terminal discard the whole
  report).

  The policy is tiered, and which tier a value belongs to is decided by whether
  anything other than a widget reads it:

  1. **Ingest** — daemon-authored text that is *only* displayed is normalized
     once, where it is stored: the chat status/error line and the quit message
     `run_app` prints with `println!` once the alternate screen is gone
     (`App::set_status`/`App::set_error`/`App::set_quit_message` writing a
     private `DisplayLine` field, so no call site can store raw bytes and the
     height math measures exactly the drawn text without re-scanning), the
     AI-providers sub-modal error lines, the session-manager and model-selector
     error rows, and the login modal's status text, authorize URL and device
     code (both halves). Because the stored value is already inert, it stays
     inert at every place it is later read — including a non-ratatui consumer
     with no widget sink of its own to route through (a `println!` output path,
     a log line, a clipboard copy).
  2. **Sink** — identity-bearing or keyed values stay byte-exact in state and
     are normalized at the draw sink instead: a model id, a reasoning effort,
     an account slug, a session title (which also feeds the OSC 2 window title,
     where the policy is applied by *dropping* rather than escaping — see
     above), a tool name compared against
     `MARKDOWN_TOOLS`, a tool-call id — and, in the login modal, `provider` and
     `account`, because the account name is the credential key the modal hands
     back and the provider selects the auth descriptor. Escaping those in place
     would change what is sent back to the daemon, what is compared, or what
     key it is looked up by, so the rewriting happens at the last moment.

  The two tiers compose safely: the normalizer is idempotent and borrows text
  that is already normalized, so a sink pass over an ingest-normalized string
  costs one scan and allocates nothing. The sink therefore stays as
  belt-and-braces for whatever a future widget forgets to place on the ingest
  tier.

  The rule covers text the TUI only *displays*, not text the user is *editing*.
  The prompt buffer and the picker filter fields (command palette, model
  selector, provider wizard) draw their raw bytes deliberately: those bytes are
  exactly what reaches the daemon, and the buffer's cursor is a byte offset into
  them, so normalizing on the way to the screen would desync the cursor from the
  row it is drawn on and alter the text that gets submitted.  That exemption
  costs no measurement honesty, because a paste is sanitized at the paste choke
  point before it reaches any of those buffers: `sanitize_pasted_text` drops
  every character the terminal filter rejects (C0/C1 controls and DEL, the
  line/paragraph separators, the non-joiner format class), expands tabs to
  4-column stops, and folds CR to `\n` — rewriting rather than escaping, since an
  injected `\u{1b}` would be text nobody typed. The buffer's bytes therefore stay
  exactly what the daemon receives *and* measure exactly what draws.  Everything
  the user merely reads — including their own submitted turn text, once it is out
  of the buffer — is normalized like any other displayed content.

**Streaming is byte-bounded end to end.** The bounded streaming channel only bounds
*in-flight* chunks (backpressure); the *total* is capped too, so the live view can never
diverge unboundedly from the recorded result. (Note the two independent bounds: this
section is about `ByteBudget` capping a single tool's *content*; the lossless delivery
design separately bounds *delivery* — the per-client in-flight bytes that trigger
lag-eviction — see the `choreo-daemon` `broadcast` module docs. They don't collide: content is capped
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

`pdf_classify` and `pdf_to_markdown` give the agent native PDF ingestion by
wrapping `pdf-inspector` (Firecrawl) — a pure-Rust, extraction-only PDF parser
built on `lopdf`. The parser has no JavaScript engine, never renders pages, and
never executes embedded files, `/Launch` actions, or external references, so the
classic PDF malware *execution* vectors are excluded by construction. The tool
layout and per-tool contracts are documented in-source (see `tools/pdf/`).

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
> `tests/it/pdf_tool_integration.rs` still covers the vuln: with `lopdf >= 0.42` the parser
> caps nesting depth, so the PoC yields a clean parse error or a graceful `SCANNED`
> classification (exit 0) instead of SIGABRT.

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

### Available tools

Each tool's contract — its arguments, return shape, and output behaviour, and its
feature/binary gating — is documented in-source (see `choreo-daemon`'s tool modules,
`cargo doc -p choreo-daemon` / `just doc`), held complete by the `#![warn(missing_docs)]` +
`doc-check` gate. The registry lists the shipped tools (see the `tools` module and its `ToolRegistry` docs).

### Tool groups

Tools are organized into groups to reduce context overhead. Each tool declares its group
via `fn group() -> &'static str` on the `Tool` trait. The group catalog — each group's name,
its model-facing description, and its cargo-feature gating — is documented in-source (see
`choreo-daemon`'s `tools` module, `cargo doc -p choreo-daemon` / `just doc`), held complete by
the `#![warn(missing_docs)]` + `doc-check` gate; the default active set (`core`, `git`, and
`shell`) travels with the session-config default in `choreo-daemon`'s `sessions` module.

The system prompt lists all groups and their descriptions. The model uses `load_tools` to
activate additional groups and `unload_tools` to deactivate them. **core** cannot be unloaded.

Groups affect only tool **availability** in the API `tools` array — they are a discovery
mechanism, not access control. The RISC-V VM (`run_riscv`) always has access to all registered
tools regardless of group state.

### Concurrent tool dispatch

`run_agent_loop` partitions each response's tool calls into a **serial
(session-config)** phase — `load_tools`, `unload_tools`, `set_working_dir`, and
the other session-config mutators — and a **concurrent** phase holding every
remaining tool (shell, filesystem, VM, HTTP, Git, `spawn_subsession`, …) whose
execution is independent of session state. The partition, the typed
`PendingConfigChange` capture, and the Phase-3 worker-copy mirror that carries
successful session-config mutations onto the request worker's config copy are
documented in-source (`choreo-daemon`'s `requests` module).

The dispatch and execution machinery (channel wiring, per-tool timeout
resolution, the wait loops, the streaming forwarder, and per-tool result
recording) lives in `requests/tool_execution.rs`, and the system-prompt /
tool-result-collection helpers live in `requests/system_content.rs`; both are
re-exported from `requests.rs`. Each concurrent tool runs on a dedicated
execution thread, a forwarding thread, and a wait-loop thread — see the
`requests/tool_execution.rs` module docs for the thread topology and the
per-tool timeout budgets.

Cancellation is fully event-driven: every wait blocks on a
`crossbeam_channel::select_biased!` between its result channel, the request's
cancel channel, and (where a timeout applies) an exact `after(remaining)` timer
— there are no `recv_timeout` poll loops — and every wait biases the cancel arm
first, so a queued cancel is selected deterministically and a cancel that lands
mid-block is more likely to beat a simultaneously-ready result. When a cancel
wins a race, an already-completed result is still drained (non-blocking) rather
than discarded, so the tool's real output is recorded while the request still
stops (sticky `cancelled` flag). A cancel observed by the concurrent collector
sends every still-running wait-loop a per-tool kill and keeps draining until all
batch handles have arrived, so each unfinished call records a deterministic
`"tool '<name>' cancelled"` outcome; the placeholder sweep
(`mark_unexecuted_tool_results`) remains a safety net for wait-loops that die
before delivering. Tool results are always rendered in the model's original call
order — placeholders are seeded in call order and filled **in place by
`call_id`** as each tool streams or finishes, and the accumulator fed to the
provider is re-sorted back to call order (`sort_by_call_order`) after the batch.
A tool thread that panics is caught and reported as a `ToolOutput` with
`is_error: true` instead of crashing the daemon.

Both phases share a `ToolContext.cancelled` flag with the running tool so a tool
that consults it can stop early — the one sanctioned shared-state exception to
the repo's channel-only thread-communication rule (see AGENTS.md).

### spawn_subsession

`spawn_subsession` is a core-group `Tool` implementation that runs in the
concurrent dispatch path. Invoked, it creates a child session
(`DaemonCommand::CreateSession`, parent as `parent_session_id`, inheriting the
parent's working directory and tool groups), pushes the prompt as a `SystemText`
message, drives the child's own `run_agent_loop()` (subject to the daemon-wide
`max_turns` cap), and returns the child's assistant text to the parent; the child
persists as a normal session. The tool's contract is documented in-source
(`choreo-daemon`'s `tools/subsession.rs`).

The daemon tracks the parent→child relationship (`DaemonState::children`) so a
parent's cancellation, exit, or deletion also stops its children — see the
daemon's session-lifecycle docs for the cancellation-propagation and
cascade-deletion rules.

---

## Session architecture

Sessions are persisted to a `redb` (v4) embedded key-value store at
`~/.local/share/choreographr/state.redb`, and each active session's in-memory state
is a `SessionState` owned by its control thread with the persisted fields extracted
into `SessionConfig`. The table schema — every table's key and value codec, the
zstd turn-value compression, the turn-attachment byte split, the deletion
tombstone, and the runtime catalog-refresh state — and the
`SessionState`/`SessionConfig` split, with every field's rationale, are documented
in-source (see `choreo-daemon`'s `db` and `sessions` modules, `cargo doc -p
choreo-daemon` / `just doc`), held complete by the `#![warn(missing_docs)]` +
`doc-check` gate.

### Turn durability

Each `Turn` is written to disk when the **agent-loop iteration that produced it**
completes, not when the request does. The loop builds its turns on the request
worker's private `SessionState` clone, which the session thread only merges at
`RequestFinished`; that merge remains the authoritative final write, but it is no
longer the only one. The iteration boundary is the first moment a turn is
complete (assistant message plus one recorded result per call), and it precedes
the next provider round-trip, so a checkpoint there bounds the crash window to
the single turn in flight. This matters because one request is not one turn: a
tool loop may iterate without limit (unbounded when `max_turns` is 0) for tens of
minutes, and deferring the write to the end made the whole request the unit of
durability.

Three properties hold around that boundary:

- **A turn is written whole or not at all.** One `write_turn` transaction covers
the zstd-compressed blob and every attachment slot, so a reader can never see a
turn with some of its images. Image bytes are additionally written per emit
(see the image flow) because a client fetches them by key the moment the
turn's `TurnAppended` arrives.
- **A committed turn is durable.** `redb` write transactions default to
`Durability::Immediate` and the daemon never lowers it, so a committed turn has
been fsynced — a crash loses only what was never committed.
- **Every request ending persists.** Completion, inference failure, and
cancellation all finalize the turn and route the worker's snapshot through
`RequestFinished`, so a graceful shutdown (which cancels in-flight requests)
loses nothing; only process death, which cannot cancel, skips the paths that
would have written.

A checkpoint failure is logged and swallowed rather than failing the request:
the turn stays in memory, the loop continues, and the `RequestFinished` merge
retries the same idempotent write. Because a checkpointed turn is never mutated
after its boundary, the worker reports the ids it wrote in the snapshot's
`persisted_turns` set, and the merge inserts those turns into memory without
re-writing the identical row — so a turn is written once, not twice. A turn the
worker did *not* checkpoint (for example the turn open when a provider-side
cancel returned) is absent from that set and is still written by the merge.

**Crash-recovery skew.** Durability covers the turn rows, not every field of the
worker's `SessionState`. Config the worker accumulates over a request — mainly
`last_response_id` (the provider's continuation-chain head) and
`accumulated_usage` (the billing total) — still merges only at `RequestFinished`,
so after a mid-request crash the on-disk turns can be newer than those fields.
Config changes a tool makes through the session's own commands (model, title,
working directory, tool groups) are persisted when the command is applied, so
only the worker-owned continuation/billing fields lag. On restart the session is
rebuilt from the turn rows (with `next_turn_id` derived from the maximum turn id,
not the record's advisory `turn_count`), and the stale `last_response_id` is
restored only if its producing provider+model still matches; a `ResponseId`
provider then continues from that older head with the checkpointed turns re-sent
as new input. This is the accepted cost of turn-level durability — the
conversation itself is never lost.

### Schema versioning & migrations

The `meta` table holds the persisted schema version, and the daemon runs the
migration chain right after opening the database on every startup. The versioning
policy (an additive change needs no migration; a breaking change gets a numbered
migration and a version bump), the migration contract (a contiguous chain,
idempotent, transactional, and fixture-tested), the pre-migration backup, and the
downgrade / redb `UpgradeRequired` refusals are documented in-source (see
`choreo-daemon`'s `db` module, `cargo doc -p choreo-daemon` / `just doc`), held
complete by the `#![warn(missing_docs)]` + `doc-check` gate.

### Undo/Redo

Sessions support undo/redo via an `undone` boolean flag on each `Turn`. The `Turn`
struct — its `turn_id`/`undone`/`user_text` fields, the assistant-response bundle,
and the opaque `reasoning_artifact`/`reasoning_producer` round-trip payload — is
documented in-source in `choreo-proto` (alongside its `ReasoningArtifact` and
`ReasoningProducer` companions; `cargo doc -p choreo-proto` / `just doc`), held
complete by the `#![warn(missing_docs)]` + `doc-check` gate. Two cross-cutting
facts about that payload: the daemon strips it from every client-bound
`DaemonMessage` (only its own request builder reads it), and the builder's
same-model provenance check drops the artifact after a mid-session model switch.

A single `/undo` hides the most recent user turn together with every still-live
turn after it (a tool-loop iteration creates several turns per user turn), and
records only the turns it actually flipped as one **redo batch** on a stack.
`/redo` pops the newest batch and restores exactly it, so undo and redo are
LIFO-symmetric — one redo re-applies one undo, never collapsing several pending
undos at once. A new user turn clears the stack (redo is void once the
conversation has moved on), and undo also clears the persisted
`last_response_id` so a chained request cannot leak the hidden turns back into
the model. Both commands are gated to an **idle** session: an in-flight request
worker holds the turn history on a private clone and merges it back at
`RequestFinished`, so a history mutation underneath it would race that merge
(and could resurrect the cleared chain). The daemon refuses a busy session with
`Failed { error: "session is not idle" }`; the TUI applies the same idle check
client-side to skip the doomed round-trip.

---


**Service config:** `~/.config/choreographr/config.toml`

```toml
max_turns = 0      # daemon-wide tool-loop budget; 0 = unlimited (default)

[context]
context_file_names = ["AGENTS.md", "CLAUDE.md"]
context_file_max_bytes = 32768

[cache_warming]          # prompt-cache warming — off by default; see the cache_warm.rs module
mode = "off"             # off | streaming (warm the prompt cache while a tool call blocks)
min_prefix_tokens = 32000    # `tokens`-metered gate: minimum cacheable prefix
min_expected_savings = 0.05  # `payg`-metered gate: minimum USD saved per ping
```

> **Note:** Provider-level settings (`base_url`, `streaming`, `retry_*`, timeouts, endpoint paths, request format) have moved to per-account overrides in `accounts.toml`. See `README.md` for the full list.

**Cache warming is off by default.** A warm ping re-sends the last
request with a 1-token output cap to refresh the provider's in-memory prompt
cache before its TTL expires. Whether that pays off depends on how the account
is billed, so a per-account `meter` gates it: `payg` warms when the expected
dollar saving clears `min_expected_savings`, `tokens` warms when the cacheable
prefix is at least `min_prefix_tokens`, and `requests`/`flat`/`unknown` never
warm (a ping burns request budget on a request-metered plan and saves nothing
on an unmetered one). The dollar gate is only meaningful under pay-as-you-go,
which is why the meter is explicit and defaults to `unknown` (never warm).
When `mode = "streaming"` (and the account's meter gate clears) the agent loop
spawns a per-request warmer thread that fires while a tool call blocks; the
ping is a single non-streaming, non-retrying 1-token request that never
touches session or turn state. Status is observable through the
`choreo_cache_warm_attempts_total` / `choreo_cache_warm_skips_total{reason}`
metrics and `tracing` logs only — no `SessionEvent` is emitted (a status
event is a deliberate follow-up).

**Credential storage:** Credentials are encrypted per-credential in the `redb` database (`state.redb`). Each daemon's keystore is bound to one client-held unlock key (the daemon stores only the derived public binding, created once via the `BindKeystore` wire path — TOFU-once, no rotation); the legacy `identity.pk` / `public.pk` pair is removed, and the legacy raw `identity.pk` file is an unlock-verification fallback that is copied into `known_servers.toml` on first use (never deleted, never binds). `AddCredential` carries the credential as typed plaintext with no key: the DAEMON encrypts it to its own stored binding public key before persisting, so the client never holds or transmits the encryption key on the add path (which requires an already-unlocked keystore).

**Database:** `~/.local/share/choreographr/state.redb` (override via `CHOREOGRAPHR_DB_PATH` env var)

**Socket path:** `$XDG_RUNTIME_DIR/choreographr.sock` (falls back to `/tmp/choreographr.sock` when no runtime dir; override via `CHOREOGRAPHR_SOCKET_PATH` env var)

**Tool loop limit:** `CHOREOGRAPHR_MAX_TURNS` env var overrides `config.toml` `max_turns`. Resolution
chain: `CHOREOGRAPHR_MAX_TURNS` env var → `config.toml` → default 0 (unlimited).
A value of `0` means *unlimited* — the agent loop runs until the model
produces a final answer, is cancelled, or hits an error. This is a daemon-wide
cap; individual sessions no longer carry their own `max_turns`.

**Logging:** every binary uses `tracing` with `tracing-subscriber`. Default level is `info`.
CLI flags `-v` (debug), `-vv` (trace), or `-q` (warn) override the level, and
**explicit flags take precedence over `RUST_LOG`** (the Unix convention; `RUST_LOG`
is a per-target directive language, applied verbatim when no flag is given). The
flag parsing (`Verbosity`), the level resolution (`LoggingConfig::resolve`), and
the one subscriber initializer (`logging::init`) live in `choreo-shared::logging`,
so all five binaries — daemon, TUI, GUI, IM, and ACP — share the exact same
policy and the exact same sink layout: diagnostics always go to a **hardened,
pid-keyed file** (`<binary>-<pid>.log` in `{base}/log`, else
`$XDG_STATE_HOME/choreographr`, else the platform temp dir; create-new `O_EXCL` +
`O_NOFOLLOW` + 0600) **and**, where a console
exists, are **mirrored to stderr** from the same filter. Which binaries mirror,
and whether the mirror is unconditional or terminal-gated, is chosen per binary:
the daemon and IM bridge always mirror (their stderr is the console *or*
journald/launchd); the GUI and ACP adapter mirror only when stderr is a terminal
(a desktop-icon / editor launch has none); the TUI never mirrors (it owns the
alternate screen). A file open that fails is never fatal for any binary: it
degrades to the console sink (with a warning), so an unwritable log directory
cannot take a binary down. `--log-file <path>` on any binary chooses the
file's path only (used verbatim, with no pid key) — it never affects the level
and never mutes the console; with no base dir and no XDG state dir the file falls
back to the platform temp dir (Termux/Android included).

Every binary also **prunes** the shared log directory on startup: any log the
suite names older than **one week** is removed — its own `<binary>-<pid>.log`
and the captured MCP server `mcp-<…>.log` files alike. Retention is time-based
(not a fixed file count), so a quiet instance keeps a full week of history while
a busy one never accumulates unbounded files. A user's `--log-file` under any
other name is never touched.

**Logs never carry message payloads.** A log line (and any `bail!`/`anyhow!`
message, which is printed and may be logged) holds only an *id*, *kind tag*,
*length*, or *count* — never a user prompt, an assistant response, tool
arguments/output, a request body, or a secret. The protocol and event enums
derive `Debug` and several variants carry exactly this content, so they are
never `?`-formatted into a line; a payload-free projection (`MessageKind`,
`DaemonMessageType::kind` / `SessionEvent::kind`, `BridgeEvent::kind`) or a
length is used instead (see AGENTS.md → Logging).

**Session persistence:** On daemon start, sessions are loaded from the database into
`session_metadata` (in-memory). Model selection (`/model <name>`) updates both the
in-memory metadata and the database via `UpdateMetadata → db::write_session`. The
`AttachSession` handler also populates `session_metadata` when re-loading a session
from the database, ensuring `ListModels` and metadata queries see the correct
`selected_model`.

Two sanctioned shared-state exceptions (see AGENTS.md) underpin session
cancellation and the daemon's connection accounting. Each session owns a
`choreo-sockreg` registry (the **seventh** exception) — plus one daemon-level
registry for non-session-scoped work (prefetch, catalog fetch) — so the command
loop can force-close a wedged session's provider sockets from the thread that
decided the cancel: `shutdown(fd, SHUT_RDWR)` is the only way to un-block a
worker blocked in a provider `read()`, and the registry's lock is held only for
a handful of fd syscalls. The daemon's live-connection counter (the **third**
exception, `server/lifecycle.rs`'s `Arc<AtomicUsize>` released by the RAII
`ConnectionSlot`) enforces `MAX_CONCURRENT_CONNECTIONS` atomically across the
two accept paths — a bound no channel can express without a dedicated
accounting thread.

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

### Implementation

The metrics module keeps the real implementation in a feature-gated `backend`
module (a no-op stub backend when disabled, re-exported behind the same public
API), holding one static `Metrics` (a `std::sync::OnceLock`) of atomic Prometheus
counters/gauges/histograms. A dedicated thread serves `/metrics` via `tiny_http`;
because a blocking accept cannot be interrupted by a channel, that serve loop is
the one place that consults the exception-#1 shutdown flag, on a bounded (1 s)
`recv_timeout`.

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

`edit_image` inputs may also be **session references** (`@last`,
`@display:<turn>:<index>`, `@result:<turn>:<call_id>`) naming an image already
stored in the session; they are resolved through the daemon command loop — a
single batched `GetSessionImages` command reads the session's persisted
turns/attachments once for the whole call — so the tool never reads session
state directly and the command loop stays its sole reader.

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
(`reasoning.rs`'s `build_chat_request_messages` / `tool_result_image_messages`)
are documented in-source (see `choreo-daemon`'s `image_prep` and `reasoning`
modules and `choreo-image`, `cargo doc -p choreo-daemon` / `just doc`), held
complete by the `#![warn(missing_docs)]` + `doc-check` gate.

### Session switch flow

Because the TUI subscribes to *all* session activity at startup, every
session's streaming events are routed into a per-session display even for
sessions the user is not viewing, so switching sessions preserves that
accumulated state and merges it with the daemon's attach snapshot (finished
turns come from the snapshot; the in-flight turn keeps the accumulated version,
which holds the live content the snapshot's placeholder lacks). The TUI-side
mechanics (`reset_for_session_switch`, the absolute scroll anchor, the display
merge, and the per-session status/token routing) are documented in-source (see
`choreo-tui`'s `state` and `connection` modules, `cargo doc -p choreo-tui` /
`just doc`), held complete by the `#![warn(missing_docs)]` + `doc-check` gate.

### Streaming response auto-scroll pin

While the assistant response streams, the TUI's history pane follows the newest
content from the bottom until the response block's top row reaches the top of
the viewport; at that point it pins that row at the top and the rest of the
response grows below the fold. The anchor is the response block's top padding
row — its first visible row, so the first text line sits one row below the
viewport top — never the reasoning trace or tool output, and the pin fires at
most once per response. The decision is made in the single pass every frame's
height model runs through before rendering (`compute_total_height_and_markers`),
so it holds identically across the streaming fast path, a full rebuild after an
interleaved `Done`/`TurnAppended`, a resize, or a session switch-in. The pin
state lives on the per-session display and is deliberately **not** reset by
`reset_for_session_switch`: a response that streamed in a background session
pins on the first frame after the user switches to it, and one that already
pinned keeps its position on every re-entry (it never re-pins).

A single agent run is a *tool loop*: the daemon appends several turns — and
their tool calls and output — against one stream id before it is done. Every
response pins while it streams, but the first thing that streams *after* it (a
tool call, tool output, or the next turn's `Started`) releases the pin: a pinned
response is dropped and the viewport returned to the bottom, an armed one is
just dropped (so its trigger can never fire later on a finished response), and
so the following content is followed from the bottom. The run's *final* response
has nothing after it and is the one left pinned. A manual scroll drops the pin
entirely (armed or already pinned), so no release ever moves the viewport over a
user who chose to read elsewhere; the same response can then never re-pin. The
mechanics are documented in-source (see `choreo-tui`'s `state::pin`,
`state::history`, and `state::streaming`; `cargo doc -p choreo-tui` / `just doc`).

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
(`EvictLargestLagging`).  `handle_evict_client` drops the client's single `clients` entry (which holds
its writer sink, its summary/activity flags, and its session memberships),
tells its sessions to drop it (`RemoveSubscriber`), and enqueues a best-effort
`Evicted` advisory before dropping the sink.

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
a socket write timeout (`DaemonState::writer_write_timeout`, default 5 s `WRITER_WRITE_TIMEOUT`), so a wedged client
(zero receive window) cannot stall its writer forever — the write fails,
the writer shuts the socket down itself (notify-before-EOF on the graceful
path), and the reader's blocking read unblocks into the normal
`cleanup_client` teardown (see the `choreo-daemon` `server` module docs).

Evictions are not lost silently: every one increments the
`choreo_evictions_total` Prometheus counter served on `/metrics`, so a
permanently wedged subscriber — one whose backlog keeps crossing the cap —
remains observable.  The old `choreo_broadcast_dropped_total` counter (and
the drop-on-full policy it measured) is gone: the daemon no longer drops
broadcast messages.

Each connection is identified by a process-unique `ClientId`, minted from a
process-wide monotonic `AtomicU32` counter (`broadcast::ClientId::next`) — the
tenth sanctioned shared-state exception (see AGENTS.md): a lock-free tick that
carries no protocol data and never crosses the wire, chosen over a random id
so connection ids read as small sequential numbers in logs.  The daemon's
consolidated `DaemonState::clients` map is keyed by it — one entry per client
holding its writer sink, its subscription flags, and its session memberships.

The one process-global override shared with tests is `choreo-shared`'s
`paths::TEST_LOG_DIR` (a `static RwLock<Option<PathBuf>>`) — the eleventh
sanctioned shared-state exception (see AGENTS.md).  Integration tests must keep
the suite's log directory out of the developer's real `$XDG_STATE_HOME`, but
that directory is resolved from many threads (the daemon command loop and each
per-server MCP thread), so a thread-local override would let a worker leak its
log there; a process-global override is shared instead.  It carries no protocol
data and is a no-op in production (nothing sets it outside tests), and
nextest's process-per-test keeps one override per process isolated.

The tools' per-file mutation locks (the daemon's
`tools::file_locks::FILE_LOCKS`, a `LazyLock<FileLocks>` wrapping a
`Mutex<HashMap<PathBuf, Arc<Mutex<()>>>>`) are the twelfth sanctioned
shared-state exception (see AGENTS.md).  Each turn's non-config tool calls run
on their own threads (`requests.rs`, "Phase 2: All remaining tools
(concurrent)"), so two mutations of the same file in one batch — the
read-modify-write in `edit_file`, a `write_file` racing an `edit_file`, a
`delete_files` racing either — would otherwise both read the original and
silently lose one update; the per-path lock serializes them while mutations of
different files stay parallel.
The key is the target's canonical path — canonicalizing the parent directory
and re-appending the file name when the target does not exist yet, so a create
racing an edit of the now-existing file agrees on one key even through a
symlinked or `..`-bearing parent, and two symlinks to one target share a lock.
The registry reserves each path's handle under the map lock (brief, never
across the mutation), holds each per-path mutex only for the mutation itself,
and prunes unreferenced entries from a RAII guard's `Drop` — so a mutation that
panics still releases its entry.  It carries no protocol data.  Only the three
`fs` mutation tools take it, and the key is a *single* path: deleting a
directory does not serialize against a write of a file inside it, and a tool
that writes an arbitrary path outside `tools::fs` (e.g. `retrieve_webpage`'s
`output_path`) is not covered.

---

## Design decisions

1. **Unix sockets, not HTTP for client↔daemon** — keeps everything local, avoids port conflicts,
   leverages OS-level access control.

2. **Binary protocol (MessagePack, named mode), not JSON** — self-describing, compact, typed,
   versioned. Length-prefixed framing avoids parsing ambiguities. Version field allows protocol
   evolution.

3. **Lock/Unlock security** — the daemon starts without credentials in memory. The unlock key travels
   only over authenticated transport and is zeroized after use. Credentials are encrypted per-credential so
   they can be stored in the database without a global passphrase. The keystore binding is established once (TOFU) via the `BindKeystore` wire path — the only message that can create it; `Unlock` is verify-only. `AddCredential` carries the credential as typed plaintext with no key — the daemon encrypts it to its own binding and requires an already-unlocked keystore.

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
   Because the peer is a provider, a gateway, or a proxy, an event is only delimited by a blank
   line — so the reader caps one event's accumulated bytes (`MAX_SSE_EVENT_BYTES`) rather than
   letting a peer that never terminates an event grow the buffers without bound, and it resumes
   its newline scan from the last unscanned offset instead of rescanning the whole buffer on
   every read (which would make one large event quadratic in its length).
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
   value in aggregate. A separate body-read **idle timeout** (`request_timeout_secs`) is
   enforced at the socket level: the connector caps the socket read timeout at that bound,
   and because a socket timeout resets on every received byte it fires only when the provider
   sends nothing for that long — so a steadily streaming response is never cut short, however
   long, while a silent one fails promptly. ureq's own `timeout_recv_body` is NOT used: it is
   a whole-body total ("the budget is not restarted for each read"), so mapping the idle bound
   to it would abort any stream longer than the bound even while bytes flow. A read timeout is
   surfaced as a fatal stream error and logged (`warn!`) with the request's provider slug +
   model (`SseContext`) so a silent provider is attributable; the wall-clock deadline above
   remains the backstop for a stream that trickles keep-alive bytes without ever forming an
   event.

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
    `input` array as `{role: "system"}` items (rather than the `instructions` field), an
    assistant turn's tool requests become `function_call` items and their answers
    `function_call_output` items (the answer names its call by `call_id`, so a history
    rebuilt from messages — no chained `previous_response_id`, e.g. after a model switch
    or a restored session — has to replay the calls themselves, not only the outputs),
    and the `input` is an array of typed items rather than a
    flat messages list. Multi-turn chaining uses `previous_response_id` to link turns together,
    while Chat Completions relies on the full message history.

    The streaming terminal events wrap the whole `Response` object under `response`:
    `response.completed` and `response.incomplete` carry the id/usage there (`sse::response_object`
    also reads a flattened shape, which some gateways emit), and `response.failed` carries the
    error object on `response.error`. `incomplete` is a terminal **success**, not a transport
    failure: `incomplete_details.reason` decides — `content_filter` is the terminal
    `ContentFiltered` error, while `max_output_tokens` (and any reason this client does not
    classify) leaves a partial answer reported as a truncated `FinalText`, the same reading chat
    gives `finish_reason: length`. Reasoning text is streamed as
    `response.reasoning_summary_text.delta` fragments (the summary is the only reasoning text a
    Responses model exposes); part boundaries carry no text and are not modelled.

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
    where third-party libraries (alloy, subxt, rmcp) require it. These live in the
    `choreo-blockchain`, `choreo-content`, and `choreo-mcp` crates, each of which holds a global
    `OnceLock<tokio::runtime::Runtime>` as a sidecar and runs its clients via `block_on()`.
    `choreo-mcp` additionally runs one blocking **dispatcher thread per MCP server** that spawns
    its in-flight calls onto that sidecar, so calls to one server run concurrently and can be
    cancelled while the dispatcher keeps serving commands. The daemon links those crates only
    behind the `blockchain`, `content`, and `mcp` cargo features (the first two off by default;
    `mcp` is on by default) and calls
    their synchronous entry points, so tokio is
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
    `Signature` / `ResponseId`, per-model override else protocol default). `ToolLoop` echoes the
    tool-involving turns, and on a route whose catalog entry requires the echoed field it echoes
    every turn instead: DeepSeek asks for all earlier turns' `reasoning_content` once the request
    carries `tools`, tool-less turns included
    ([docs](https://api-docs.deepseek.com/guides/thinking_mode)), and GLM treats a missing block as
    degraded preserved thinking ([docs](https://docs.z.ai/guides/capabilities/thinking-mode)).
    Display text stays in
    `Turn.assistant_reasoning`; the artifact bytes are never interpreted by the daemon.
    *Where* the payload lands on the chat wire is the adapter's decision, from two independent
    catalog facts — the desired field (`ModelEntry::reasoning_echo`, ingested from the snapshot's
    `interleaved`) and whether that field must be PRESENT on every assistant message even when
    empty (`ModelEntry::reasoning_echo_required`). A declared field outranks the field the payload
    was observed under; a scalar
    `interleaved` name sets both facts, while the structured `reasoning_details` array is a
    destination only — a details route with nothing to echo omits the key instead of carrying an
    unverified empty array.
    The field may be the structured `reasoning_details` array: its artifact payload is JSON
    (normalized at capture under the reasoning byte budget), replayed verbatim for OpenRouter's
    `reasoning.*` entries and rebuilt minimally for bare Kimi-shaped ones. The array is the
    **only** reasoning field such a message carries — no scalar mirror is invented beside it,
    because no upstream documents a message carrying both. Replay itself is bounded, per request,
    over the artifacts of the whole history (newest always kept, older ones while the total stays
    within the budget), and that budget is the *same number* as the per-turn cap: a larger
    per-turn cap would have no replay path, so the two differ in behaviour (capture drops the one
    turn's array, replay drops the oldest) rather than in size — which is why they are one
    constant, `reasoning_details::MAX_REASONING_BYTES`. The Responses adapter's opaque reasoning
    items ride the same persisted `ReasoningArtifact` and the same compounding replay path, so
    they are bounded by that one constant too: capture drops an over-budget item array, and a
    replay skips an artifact that somehow exceeds it. A corrupt persisted payload is likewise a
    dropped artifact on both adapters, never a failed request — a hard error there would make
    every later turn of the session unsendable.

    **A reasoning-only turn is output.** The turn result carries the fact that the provider
    produced reasoning, independent of whether any of it could be displayed or replayed: a
    response whose only output is a reasoning item (or an entry over the replay cap) becomes a
    `FinalText` with empty content that still carries the round-trip artifact, never an
    `EmptyResponse` error that discards a payload the provider billed for. Both wire formats
    reach that verdict through one shared assembler (`openai::turn::assemble_turn`), fed by
    adapter-specific parts, so the tool-call / truncated-call / empty-response decision tree
    exists once rather than once per path.




---

## Context file discovery

The daemon discovers project-specific context files (`AGENTS.md`, `CLAUDE.md`)
and Agent Skills at session start and re-checks them before every model call
(every turn of the tool-call loop), feeding what it finds into the system
prompt. The discovery walk, the skill scopes and precedence, the fingerprint
cache, and the `load_skill` body plumbing are documented in-source (see
`choreo-daemon`'s `context` and `requests` modules, `cargo doc -p choreo-daemon`
/ `just doc`), held complete by the `#![warn(missing_docs)]` + `doc-check` gate.
The `[context]` config table (`context_file_names`, `context_file_max_bytes`,
`disable_claude_code_prompt`) is documented in the README and the `ContextConfig`
rustdoc (`choreo-proto`).

### System prompt construction

`build_system_content()` runs once per loop iteration and **always** returns a
full system prompt, so a session with no working directory still receives the
base prompt (identity, tool groups, skill metadata, loaded skill bodies, session
title); the context files, pending subdirectory hints, and their fingerprint
cache are the only working-dir-gated parts. Documented in-source
(`choreo-daemon`'s `requests/system_content.rs`).

### Discovery algorithm

Global files load first in a fixed order — the config-dir `AGENTS.md`, then
`~/.claude/CLAUDE.md` (unless `disable_claude_code_prompt` is set), then
`~/.agents/AGENTS.md`. The project tree is then walked from the session working
directory up to the git repository root (or the filesystem root when there is no
`.git`), taking at most one file per directory (the first `context_file_names`
match that exists) and rendering the outermost ancestor first and the working
directory last, so the nearest instructions win. Documented in-source
(`choreo-daemon`'s `context.rs`).

### Subdirectory hints

When a filesystem tool (`read_file`, `list_files`, `grep`, `find`, …) touches a
path below the working directory, the daemon walks up from that path toward the
working directory and appends any newly found `AGENTS.md`/`CLAUDE.md` to the
**tool result** rather than the system prompt, so nested instructions surface
without disturbing prompt-cache stability. Documented in-source
(`choreo-daemon`'s `context.rs::subdirectory_hints`).

### Skills (Agent Skills standard)

Skills follow the Agent Skills standard: a `<name>/SKILL.md` under
`~/.agents/skills` (global — always scanned) or `.agents/skills` (project —
scanned only when the session has a working directory), each carrying YAML
frontmatter with `name` and `description`. The project scope is scanned first
and results dedup by frontmatter `name`, so a project-local skill shadows a
same-named global one (and a more-local skill shadows one higher up the tree);
within a scope the lexicographically-first path wins deterministically rather
than depending on the unspecified `read_dir` order. **Progressive disclosure:**
only the name + description metadata rides the stable prompt (`messages[0]`); the
full body is injected when the model calls `load_skill`. Documented in-source
(`choreo-daemon`'s `context.rs`).

### Fingerprint-based refresh

Before each turn the daemon fingerprints the known context-file paths and their
mtimes (not their contents); an unchanged fingerprint reuses the assembled text
cached on `SessionState::context_cache`, while a change (a file added, removed,
or modified) rebuilds it. Documented in-source
(`choreo-daemon`'s `context.rs::compute_fingerprint`).

### User system prompt override

The stable base prompt (`messages[0]`) is `~/.config/choreographr/system.md`
when present, otherwise the built-in default (`choreo-daemon/system.md`,
embedded at compile time via `include_str!`). Documented in-source
(`choreo-daemon`'s `context.rs`).

### Tool: `load_skill`

`load_skill(name)` (core group) resolves the name against the session's
`discovered_skills` snapshot — the same list the prompt listing was built from,
falling back to a fresh ambient walk only when no snapshot is available — strips
the YAML frontmatter, and returns `Loaded skill: <name>` plus the body. The agent
loop then persists the body into `SessionState::loaded_skill_bodies`, re-injected
under `<skill name="...">` on every later turn so its instructions stay visible
after the tool result scrolls out of the context window. Documented in-source
(`choreo-daemon`'s `tools/admin/load_skill.rs` and `requests/system_content.rs`).

### `run_riscv` — RISC-V sandboxed code execution

The guest runs in an isolated VM: it cannot reach host memory, syscalls, or files
except through the registered tools, and every guest tool call goes through the
same `ToolRegistry` the host agent uses, so it honours the same `x_credentials`
and `working_dir` context. The tool's own contract — the compile-and-run flow,
the guest ABI and syscall set, and the ckb-vm memory/cycle bounds — is documented
in-source (`choreo-daemon`'s `tools/vm.rs`).

### `sh` — POSIX shell command execution

The shell/exec tools (`sh`, `nushell`, `fish`, `powershell`, `exec`) share one
sandbox contract, implemented in `tools/shell_util.rs` and documented in-source:
a watchdog kills the child's process tree on timeout; the child's environment is
stripped of the canonical injection variables; stdin is not connected, so a
command that reads from it hangs until the timeout; and output is bounded by the
shared 128 KiB cap. Confinement is OS-level — the session working directory is the
boundary, enforced by [Landlock](https://landlock.io/) on Linux and
[Seatbelt](https://theapplewiki.com/wiki/Dev:Seatbelt) on macOS (see README) — so
no in-process path confinement or resource limits are applied. The shell the `sh`
tool runs under is resolved once at daemon startup (see `tools/shell_resolver.rs`).

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

## Testing

| Layer | What's tested | Location |
|---|---|---|
| Protocol | Framing, version handling, round-trip encode/decode | `choreo-proto/src/tests.rs` |
| Client core | Shell parsing, markdown→HTML, image assembly, history | `choreo-client-core/src/tests.rs` |
| Daemon | Request lifecycle, session CRUD, cancellation, tool calls, model listing | `choreo-daemon/src/tests.rs`, `choreo-daemon/tests/it/session_integration.rs`, `choreo-daemon/tests/it/lifecycle_integration.rs` |
| MCP (choreo-mcp) | Server spawn / Streamable HTTP connect, tool discovery, echo/error/structured/image calls, both protocol eras (`server/discover` + `Auto` fallback to `initialize`), schema normalization, cancellation (including the fixture asserting the SERVER observed `notifications/cancelled`), progress notifications turned into streamed chunks, the MRTR decline-and-retry loop for `input_required`, resource list/read, the `subscriptions/listen` list-change forwarding (a `modern-list-changed` fixture emits a tools list-changed that must reach the caller's sink, and a server without the capability must NOT have a stream opened), crash/no-init/garbage/oversized scenarios (the oversized case now failing on the client's own 8 MiB frame cap, plus `BoundedLineReader` unit tests); over HTTP: JSON and SSE responses, generated-header validation, `Auto` fallback, a retryable `503` connect, and rejection of the removed HTTP+SSE transport; the dispatcher protocol (including the concurrency cap: `CallGate` unit tests and a deterministic dispatcher test proving an excess call is queued until a slot frees), error mapping, retry/backoff math, and config parsing are unit-tested against a mock engine (no sleeps); the P6 bounds are covered too — the tool-count cap, the schema depth/size bounds (`json_depth`, `normalize_input_schema`/`normalize_output_schema`), the configurable restart cap (`RestartPolicy::new(0)` disables reconnect), the notification rate limiter, and the bounded SSE retry policy each have wait-free unit tests, and a `stubborn` fixture (which ignores stdin EOF) backs both a `choreo-mcp` and a daemon test proving shutdown returns within its bounded wait, while a `slow-list` fixture test proves the wedged-discovery server exits on stdin EOF rather than orphaning its child, and a crate-internal stdio test proves a dropped transport kills its child synchronously with no live runtime; the official client conformance suite runs both protocol eras against the `mcp-conformance-client` harness with a committed expected-failures baseline, and the OAuth half — target-scoped credential keying, connect-time refresh / revocation / scope handling, the `authorizationServer` override at connect, and the issuer fail-closed quarantine — against an in-repo mock authorization server with a temp-file credential backend | `choreo-mcp/tests/it/mcp_integration.rs` (stdio, against the in-tree `tests/fixtures/fixture_server.rs`), `choreo-mcp/tests/it/mcp_http_integration.rs` (a local `TcpListener` HTTP fixture), and `choreo-mcp/tests/it/mcp_oauth.rs` (a local mock authorization server); dispatcher/config/engine/retry/stdio unit tests in `src/`; the conformance harness in `tests/fixtures/conformance_client.rs` via `scripts/mcp-conformance.sh` |
| MCP (daemon) | McpManager + ToolRegistry integration, dynamic group registration (tool wrappers plus the `resources`-capability catalogue tools), tool execution, progress streaming through the wrapper, image attachment via the sink, the `subscriptions/listen` → shared-channel forwarding and the `register_all` / `refresh_server` + `register_cached` catalogue re-registration (the latter pinned by `mcp_list_change_only_relists_the_changed_server`, which proves the unchanged server is not re-listed), the list-change handler's atomic registry swap (unit-tested with no servers), `McpManager::status` for the `/mcp` surface and `session_inspect`, `McpManager::reload` (an empty reload and a malformed-config hard failure, unit-tested) and the `/mcp reload` connection handler (success summary + refreshed list, and the config error reply), and `mcp.json` parsing (transport inference, `${VAR}` expansion, `cwd`/`~` expansion, `disabledTools`, `maxRestarts`, `shared`, and the two-tier daemon/project load with the trust-gated project expansion, plus a fuzz-style malformed-input corpus), the project-root walk, the trust store (exact-canonical matching, no ancestor inheritance, fail-closed on garbage), same-root re-resolve reuse (connection count) and the reload/reconnect/trust-reload reconciliation paths (a changed `shared = false` slot is dropped, a per-session slot reconnects, a server FLIPPED from `shared = true` to `false` drops its stale daemon shared slot, an UNTRUSTED project does not suppress a daemon per-session server, a failed project connect does not shadow the daemon group, and an unchanged trust set is a no-op), and MCP OAuth at the daemon tier — the project-root trust gate on a server's `oauth` block, that untrusting a project never deletes a stored credential row, and the interactive sign-in (RFC 9728/8414 discovery, dynamic client registration, PKCE, and the confidential-client `client_secret_basic`/`client_secret_post` selection) against a mock authorization server | `choreo-daemon/tests/it/mcp_integration.rs`; `choreo-daemon/tests/it/mcp_project_trust.rs`; `choreo-daemon/tests/it/mcp_oauth.rs`; the registry-swap unit test in `src/daemon/tests.rs`; config unit tests in `src/mcp/config.rs`, `src/mcp/trust.rs`, `src/mcp/mod.rs` |
| Providers (`choreo-ai-protocols`) | SSE parsing, HTTP request construction, chat completions + responses serialization, content-block deserialisation, config overrides, catalog lookups, the one-shot completion paths' terminal-stop classification | `choreo-ai-protocols/src/openai/tests.rs`, `choreo-ai-protocols/src/openai/chat_completions.rs`, `choreo-ai-protocols/src/openai/config.rs`, `choreo-ai-protocols/src/anthropic/tests.rs`, `choreo-ai-protocols/src/google/tests.rs`, `choreo-ai-protocols/src/catalog/mod.rs`, `choreo-ai-protocols/tests/it/simple_completion.rs` |
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
>
> **Shared daemon-test harness.** `choreo-daemon/tests/common/mod.rs` provides
> the scaffolding for end-to-end daemon tests that run the real
> `run_server` (Unix socket + TCP/Noise) and drive it with the real client
> library: `test_db()`, `test_daemon_state()`, and `SpawnedDaemon` (spawns
> the server on a temp socket/ACL/free port, waits for both listeners, and
> SIGINT-shuts it down on drop). Test files opt in with `mod common;`.
> The daemon<->client tests (`tests/it/daemon_client_unix.rs`,
> `tests/it/daemon_client_noise.rs`) build on it.

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

**One integration-test binary per crate.** A crate's integration suite lives in
a single `tests/it/main.rs` target (one module per former `tests/foo.rs`).
Cargo builds each `tests/*.rs` file into its own test binary that statically
links the whole library, so `choreo-daemon`'s 33-file suite meant 33 relinks and
33 clippy re-checks on every change to the library or any of its dependencies;
folding them into one target collapses that fan-out to a single compile + link.
This is invisible at runtime: cargo-nextest spawns the test binary once per test
(`--exact <name>`), so process-per-test isolation is unchanged. The one caveat
is the libtest fallback (`cargo test -- --ignored`), which now runs a crate's
whole integration suite in one process — the sanctioned runner is nextest (the
integration suite is `#[ignore]` precisely so `test-libtest` never touches it),
so this only narrows an already-discouraged path.

**Test runner:** The recommended runner is cargo-nextest, configured in
`.config/nextest.toml` (`fail-fast = false`; 120s `slow-timeout` that kills hung
tests). Cargo aliases `test-fast` (unit tests), `test-integration` (the
`#[ignore]` suite), and `test-all` (both) invoke it with `--workspace`
(`test-all` additionally passes `--exclude choreo-gui` — the GUI's Blitz/wgpu
stack is too heavy for the gate, matching its `default-members` exclusion; its
tests run via `just test-gui`); plain
`cargo test` / `cargo test -- --ignored` still work via libtest. Nextest runs
every test in its own process — a large wall-time win for this 20-crate
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

**Coverage:** the `coverage` cargo alias runs exactly the `test-all` selection
under cargo-llvm-cov (`just coverage` for a per-file table, `just coverage-lcov`
for an lcov file plus a per-crate summary from `scripts/coverage-summary.sh`).
The `.github/workflows/coverage.yml` job runs `coverage-lcov` on every push and
pull request that touches Rust sources, manifests, or the test configuration;
it publishes the per-crate table on the run's summary page and the `lcov.info`
artifact, and enforces no floor. Two tooling constraints shape it. First,
cargo-llvm-cov must be ≥ 0.9.1: on the pinned nightly, test binaries land in
the build-dir layout (`target/…/debug/build/<crate>/<hash>/out/`), which older
releases do not scan — they report only the final binaries' share of the
workspace (about a third) with no error, so `_require-llvm-cov` refuses them.
Second, the report must come from the same invocation that ran the tests: a
separate `cargo llvm-cov report` step has the same blind spot even on 0.9.1,
which is why the per-crate summary is derived from the lcov file rather than
from a second report pass. Coverage counts inline `#[cfg(test)]` modules
inside source files (only crate-level `tests/` directories are excluded), so
the figure slightly overstates production-code coverage.

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
lag the resolved tree during development; that is fine. The floor is also
raised deliberately when the workspace adopts a newly stabilized std API (for
example `String::from_utf8_lossy_owned` or `Option::map_or_default`): the API becomes
the true build floor even when no dependency demands it, so the declared
`rust-version` then reflects the workspace's own sources rather than the
dependency tree alone. Before publishing,
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
| `rmcp` + `process-wrap` + `reqwest` | choreo-mcp | Official Model Context Protocol Rust SDK, its child-process wrappers, and the HTTP client — the MCP client protocol engine (behind the `mcp` feature; `process-wrap` places the stdio server child in its own process group, and `reqwest` (rustls) drives the Streamable HTTP transport and its deprecated-transport probe; the stdio read side uses a crate-local capped transport over `rmcp`'s `AsyncRwTransport`) |
| `serde` + `rmp-serde` | proto, daemon | Wire protocol framing and DB value encoding (MessagePack, named mode) |
| `structured-zstd` | daemon, choreo-ai-protocols | Pure-Rust zstd: the daemon's `session_turns` DB values (a standard zstd frame around the MessagePack blob, level 6 — the tuned level maps onto C zstd numbering; see `db/codec.rs` `COMPRESSION_LEVEL`) and opted-in LLM request bodies (level 3; see `choreo-ai-protocols/src/retry/compression.rs`). Apache-2.0; no libzstd C build. |
| `flate2` | choreo-ai-protocols | Pure-Rust gzip for opted-in LLM request bodies (the `rust_backend`/miniz_oxide backend `ureq` already enables; see `choreo-ai-protocols/src/retry/compression.rs`). |
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
