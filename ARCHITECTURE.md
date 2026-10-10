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
├── choreo-client-core     Shared client logic (parsing, images, history, credentials)
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
request-worker panics with `catch_unwind` (sessions.rs), which abort would
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

## Crate details

This section keeps the **cross-cutting** story; the per-module **API reference**
for each crate lives in its in-source rustdoc (`cargo doc`, `just doc`). The two
are complementary by design: rustdoc owns a module's purpose, its public items'
contracts, and its local invariants/portability notes (everything that changes
when exactly one crate changes), while this file owns what spans two or more
crates — topology, data flow, the security model, and design rationale. A crate
is **migrated** once every public item carries docs and its rustdoc is
warning-free; it then carries `#![warn(missing_docs)]` at its crate root and
joins the `doc_crates` list that `just doc-check` (a `pre-commit` step) holds to
`-D warnings`.

### `choreo-shared` — Shared binary helpers

A deliberately tiny **leaf crate** (`clap`, `dirs`, `tracing`, and
`tracing-subscriber` only) holding the small, binary-facing helpers every CLI
crate in the suite would otherwise duplicate. It carries no protocol or
transport logic — `choreo-proto` stays the wire protocol. The public API —
modules, types, functions, and error variants — is documented in-source:
`cargo doc -p choreo-shared` (or `just doc`), held complete by the
`#![warn(missing_docs)]` + `doc-check` gate.

The crate owns the suite's dance-style **release name**: the raw name in
`choreo-shared/release-name.txt` is baked into every binary with `include_str!`
(compile-time inclusion, no `build.rs`; the file sits inside the crate directory
so it also ships in the published `.crate`) and read by the CI release job for
the GitHub release title, so every binary's `--version`/startup banner and the
release title share one source of truth (see [RELEASE.md](./RELEASE.md)). The
shared `-v`/`-q` verbosity flags and log-level policy, and the one
filesystem-layout resolver every crate resolves through, also live here; the
resolver's `--base-dir` override is described in **The base directory** below.

### The base directory (`--base-dir`)

By default each path resolves through its own XDG base directory (the spec
mandates no single base): `dirs::config_dir()/choreographr`,
`dirs::data_dir()/choreographr`, the socket in `$XDG_RUNTIME_DIR`, and logs in
`$XDG_STATE_HOME/choreographr` (the last two falling back to the platform temp
dir where no runtime/state dir exists — macOS/Windows). The
`--base-dir <PATH>` flag (all five binaries: `choreographr`, `choreo-tui`,
`choreo-gui`, `choreo-acp`, `choreo-im`) — or the `CHOREOGRAPHR_BASE_DIR`
environment variable — relocates the WHOLE instance under one root. Because the
base plays the role of the entire XDG home, its parents are already
app-private, so the `choreographr` segment is dropped (unlike the default,
where `~/.config` / `~/.local/share` are shared):

```text
{base}/config/   config.toml, accounts.toml, mcp.json, trust.toml,
                 models-overlay.toml, authorized_clients.toml,
                 identity.pk, transport.sec/.pub, known_servers.toml
{base}/data/     state.redb, catalog.bin
{base}/run/      choreographr.sock
{base}/log/      <binary>.log
```

Precedence: `--base-dir` / `CHOREOGRAPHR_BASE_DIR` over the platform dirs; the
file-specific `CHOREOGRAPHR_DB_PATH` / `CHOREOGRAPHR_SOCKET_PATH` still win over
both, so a single file can be pinned independently.

**Why an environment variable carries it.** The base must be visible to path
resolution in *separate processes* — the daemon is a child of the TUI, the ACP
adapter is launched by an editor, the IM bridge by an operator — and the socket
path is resolved in `choreo-proto`, which has no other reason to know the
config dir. `set_base_dir_from_cli` writes `CHOREOGRAPHR_BASE_DIR` once at
startup (single-threaded, before any thread spawns — the one contained
`set_var` in the suite), so every resolver and every child process agrees
without explicit forwarding.

**Migration.** The default layout's config and data roots are two separate
platform dirs, so they cannot both be expressed as one base. `choreographr
migrate --base-dir <PATH> [--move] [--dry-run] [--force]` relocates an existing
install (copy by default, leaving the old layout reversible); it reads the
*platform-default* locations as the source and copies the keystore files
verbatim, so the transport identity is preserved and a client's pinned server
key keeps verifying (no re-pair). With a base set but its dirs empty while the
default dirs exist, startup logs a loud warning (a silent fresh instance is the
one real footgun).

### `choreo-proto` — Wire protocol

Defines all shared message types and framing. It depends on `choreo-shared`
only for the filesystem-layout resolver (the base-dir-aware socket default).
It also owns the two unix-socket-path helpers every side of the wire must agree
on: `socket_path()` (the `CHOREOGRAPHR_SOCKET_PATH`-aware default, which falls
back to `default_socket_path()` — `{base}/run/choreographr.sock` under a base
dir, else `$XDG_RUNTIME_DIR/choreographr.sock`, else the platform temp dir) and
the
dial primitives `connect_unix` / `socket_listening` / `dial_error_means_no_listener`
(with the platform-resolved `UnixStream` re-export: std on unix, `uds_windows`
on Windows). Keeping the dial and its "nothing is listening" classification
here means the client (autostart) and the daemon (stale-socket probe) cannot
classify a socket path differently.

The public API — modules, types, functions, and error variants — is documented
in-source: `cargo doc -p choreo-proto` (or `just doc`), held complete by the
`#![warn(missing_docs)]` + `doc-check` gate.

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

- Protocol version: `9` (v9 = live MCP config reload: the new `ClientMessage::McpReload` request and its `DaemonMessage::McpReloaded`/`McpReloadFailed` replies, so a running daemon can pick up `mcp.json` edits (added/removed/changed servers) without a restart; v8 = per-session `pinned`/`archived` flags: the new `ClientMessage::SetSessionPinned`/`SetSessionArchived` requests, the daemon-GENERATED broadcast `SessionEvent::SessionFlagsChanged` (rides `DaemonState::broadcast()`, so a client learns of a flag change via the broadcast rather than a targeted reply), and the `SessionSummary::pinned`/`archived_at` fields they surface. v7 = the create-session reply is split from the create-session broadcast: `SessionEvent::SessionCreatedForRequester` is the direct reply to the creating connection and is the ONLY create event a frontend may auto-attach to, while `SessionEvent::SessionCreated` is notification-only (broadcast to every subscriber) and must never move a client's view — fixing a client hijacking its own view when ANOTHER client created a session. Delete/status lifecycle messages are unaffected: `SessionDeleted` was already broadcast-only with the requester already knowing the id it deleted; v6 = displayed-image bytes are no longer shipped in session-scoped snapshots (`SessionState`/`TurnAppended`/`TurnsRedone`) — they carry only `ImageMetadata` — and are instead fetched on demand via the new `ClientMessage::GetImage` ⇄ `DaemonMessage::Image` pair, keyed by `(session_id, turn_id, image_index)` and served from the durable `session_attachments` store; v5 = the two-state `Locked`/`Unlocked` status *broadcasts* were replaced by `DaemonMessage::Keystore { state: KeystoreState }` (`Unbound`/`Locked`/`Unlocked`), pushed at subscribe time and on every transition so a first-run client learns it must BIND; `Locked`/`Unlocked` remain targeted operation replies. v4 = the 29 session-scoped events were moved into `SessionEvent` and now ride the `DaemonMessage::Session { session_id: Option<u64>, event }` envelope; v3 had removed `TurnFinalized` — the final-turn snapshot rides `TurnAppended` — and added `Evicted`, a best-effort lag-eviction advisory; mixed-version peers fail fast at the version gate). The v4 shape was amended in place before the first release — the `session_id: 0` sentinel became `Option<u64>` (`None` for connection-level replies) — so the wire version stayed `4` with no bump at that point. The MCP trust-query request/reply variants (`ClientMessage::McpTrust`/`McpUntrust`/`McpTrustList`, `DaemonMessage::McpTrustUpdated`/`McpTrustList`) were added to v9 the same way, before any release carried v9; the policy is that variants may be added to the **current, unreleased** wire version without a bump, because mixed-version peers do not exist until a release ships and the version gate fails fast on a mismatch either way. A bump is required only once a version has shipped in a release. Under the same pre-release policy, the v6 `GetImage`/`Image` pair was reshaped for v9 before any release carried v9: its positional `image_index: u32` became a tagged `key: ImageKey` (`Displayed { index }` or `ToolResult { call_id }`), so one fetch protocol now serves both displayed images and tool-result vision images from the shared `session_attachments` store. Under the same pre-release policy, the v9 frame was amended in place to the uniform correlation envelope: `ClientMessage` = `{ id: u64, inner: ClientMessageType }`, `DaemonMessage` = `{ id: Option<u64>, inner: DaemonMessageType }` (a broadcast is `id: None`; a reply stamps the request's id), the payload enums were renamed `ClientMessageType`/`DaemonMessageType`, the streaming `request_id` was renamed `stream_id` and widened to `u64`, and the `MessageKind` tag plus the `Accepted`/`Failed` acknowledgement replies were added. No bump — no release has shipped v9. Under the same pre-release policy, the v9 streaming axis was made daemon-owned: the session thread assigns a run's `stream_id` (a per-session, monotonic counter) when it accepts a `RunInput`/`ContinueGeneration` and reports it on the acceptance `Started` reply/broadcast, so those requests no longer carry a client-chosen `stream_id` (the client learns it from `Started`, which also fixes the cross-client collision where two clients fanned one session could each claim the same id); the `CANCEL_ALL` sentinel (`stream_id = 0`) remains for a pre-`Started` cancel.
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


### `choreo-sanitize` — Shared string-safety primitives

An internal leaf crate (`publish = true`, no workspace deps beyond
`unicode-general-category`) that is the single source of truth for three things
every consumer of tool output must agree on — the Unicode "spoofing"
predicates, the tool-output byte budget, and the child-process code-injection
environment set. The public API — modules, types, functions, and error variants
— is documented in-source: `cargo doc -p choreo-sanitize` (or `just doc`), held
complete by the `#![warn(missing_docs)]` + `doc-check` gate.

The crate exists so this logic has exactly one home: it would otherwise be
duplicated across `choreo-daemon` (the line-oriented `sanitize_keeps`
predicates, the `tools/mod.rs` re-exports, and `tools/shell_util.rs`'s
`strip_injection_env`), `choreo-blockchain` (the node-output sanitizer),
`choreo-tui` (`markdown_render/text.rs`'s terminal sink filter),
`choreo-client-core` (`history.rs`'s live streaming cap), and `choreo-mcp`
(`stdio.rs`'s child-env stripping). Consolidating it into one leaf crate means a
policy or budget change (or a Unicode table bump, or an added injection
variable) is applied everywhere at once, and the guard tests live next to the
code they protect.


### `choreo-image` — Shared image decode helpers

A leaf crate (`publish = true`, depends only on `image` + `heif-oxide` +
`tracing`) that owns the two decode paths shared by `choreo-daemon` (vision
normalization for `read_image`, and `display_image`) and `choreo-tui` (client
display decode), so the model path and the UI path can never drift apart. The
public API — modules, types, functions, and error variants — is documented
in-source: `cargo doc -p choreo-image` (or `just doc`), held complete by the
`#![warn(missing_docs)]` + `doc-check` gate. Its only log emission is a
`warn!`-level `tracing` event when the HEIC guard rejects a container, so a
rejected hostile input is observable without the crate owning any state.

Both decode paths run under a decompression-bomb guard: the raster path checks
the total-pixel budget `MAX_DECODE_PIXELS` against the image's declared size
before any allocation (with `image::Limits` as defense-in-depth), and the HEIC
pre-decode path parses the container's declared geometry — every `ispe` extent
and every `grid` derived item's canvas — and rejects any container that exceeds
the budget or whose geometry cannot be proved, before `heif-oxide` runs.
`heif-oxide` exposes no decoder limit and allocates from file-declared geometry,
so this guard is what closes the pre-allocation amplification vector a per-item
cap alone does not; the box-walk detail lives in `choreo-image`'s `heif` module
docs.

### `choreo-keystore` — Per-daemon unlock-key keystore crypto

Provides the cryptographic primitives for credential management. No longer a
standalone CLI binary — it is a library used by `choreo-client-core` and
`choreo-daemon`. The public API — modules, types, functions, and error variants
— is documented in-source: `cargo doc -p choreo-keystore` (or `just doc`), held
complete by the `#![warn(missing_docs)]` + `doc-check` gate.

**Per-daemon unlock key (X25519):**
Each daemon's credential keystore is governed by ONE X25519 keypair belonging to
that daemon. The daemon stores only the *binding* — the public key derived from
that pair — and the private half (the "unlock key") is held CLIENT-side, one per
daemon, in the client's `known_servers.toml`. The binding is created by exactly
ONE wire path, `ClientMessage::BindKeystore` (TOFU): on an unbound keystore it
ADOPTS the presented key (loud `KEYSTORE BOUND` log) and implies unlock; on an
already-bound keystore it verifies only. `Unlock` and `AddCredential` are
strictly VERIFY-ONLY — they never create a binding (an unbound keystore answers
`KeystoreUnbound`, a bound one answers `LockedError` on a mismatch). Binding
keys are always freshly generated client-side at bind time; pre-held keys
(stored known_servers key or the legacy file) verify existing bindings and never
create new ones. Binding is TOFU-once — there is no rotation.

The legacy `identity.pk` / `public.pk` client keypair (and `ensure_keypair`)
have been **removed**. One legacy file remains as a fallback unlock-key source:
- `~/.config/choreographr/identity.pk` — raw 32-byte legacy private key. Used
  ONLY as an unlock-VERIFICATION fallback when no unlock key is stored for the
  addr — it NEVER creates a binding and is NEVER deleted; the file's key is
  COPIED into `known_servers.toml` on first use (the store is the single source
  of truth). Encrypted-key resolution (`identity.pk.enc` +
  `CHOREOGRAPHR_KEYSTORE_PASSPHRASE`) has been **removed** entirely.

Credentials are encrypted per-credential — a client-side pipeline of postcard
serialization, X25519 ECDH against the daemon's unlock-key public key, HKDF, and
AES-256-GCM — so only the authorized holder of the unlock key can test-decrypt
them; the envelope layout and flow live in the `crypto` module docs. The
encrypted blobs are stored in the `redb` database alongside sessions, and the
daemon refuses to persist a blob it cannot test-decrypt with its bound key
(enforcing that every credential in a keystore shares one key). The keystore's
on-disk path layout resolves through the shared `choreo_shared::paths::config_dir()`,
so the `--base-dir` override relocates the keystore with the rest of the instance.


### `choreo-transport` — Noise IK/XX encrypted transport

A small crate providing Noise IK and XX handshakes and encrypted message I/O over
TCP, used by both `choreo-client-core` (client side) and `choreo-daemon` (server
side). The public API — modules, types, functions, and error variants — is
documented in-source: `cargo doc -p choreo-transport` (or `just doc`), held
complete by the `#![warn(missing_docs)]` + `doc-check` gate.

**`noise` — the encrypted stream.** `NoiseStream` wraps `TcpStream` +
`snow::TransportState` with length-prefixed AES-256-GCM framing. Payloads above
snow's 65535-byte single-message ciphertext cap are split into fragments and
reassembled transparently, so the effective per-message cap is the proto codec's
64 MiB `MAX_FRAME_SIZE`. The reassembly decision is made from an AUTHENTICATED
continuation byte embedded as the first byte of each fragment's plaintext
(covered by the AES-GCM tag) — the 4-byte wire length prefix carries no
continuation flag, so a wire-level tamper can never silently truncate or extend a
message: any prefix flip either trips the size cap or fails the GCM
authentication. The unauthenticated prefix is validated before any allocation
(snow's 65535-byte ciphertext cap) and reassembly is capped at the codec's 64 MiB
`MAX_FRAME_SIZE` (enforced on both send and receive), so a hostile or corrupted
peer cannot force a huge buffer allocation. The shared `TransportState` lock is
held only per-chunk during encryption, never during the blocking socket writes —
together with the single-writer-per-connection discipline on the daemon, this
prevents a bidirectional large-message deadlock (see
`noise_concurrent_bidirectional_large_messages`). A runtime single-writer guard
on `send_message` rejects a concurrent second sender instead of interleaving
fragments. EOF-class read failures (the peer closing its end mid-read) surface as
`TransportError::ConnectionClosed` rather than a raw `Io(UnexpectedEof)`, so the
daemon's read loop logs a graceful disconnect instead of an error. The
`Arc<Mutex<TransportState>>` and the `Arc<AtomicBool>` single-writer guard are
shared across `try_clone` reader/writer clones — a deliberate, documented
exception to the workspace's message-passing rule (the transport state must be
shared for the clones to interleave encrypt/decrypt on one connection; the guard
is a single-bit flag in the spirit of the sanctioned cooperative-cancellation-flag
exception).

**`handshake` — IK/XX and the wire-v5 mode preamble.** IK is the normal
authenticated mode (the client knows the server's static in advance — the pinned
`transport.pub`); XX is first-contact mode (the client does NOT know the server's
static, learns it from handshake message 2, and returns it so the caller can
verify it out-of-band — fingerprint confirmation — BEFORE any protocol traffic
flows; the daemon starts the writer thread only after the caller's
`on_first_contact` callback approves, so an `Unlock` can never leak to an
unconfirmed server). **TCP wire v5: every TCP connection starts with a 1-byte
unauthenticated mode preamble** (`PREAMBLE_IK` = 0x01, `PREAMBLE_XX` = 0x02; 0x00
is deliberately never assigned so all-zero garbage can never select a mode),
which authorizes NOTHING — it only selects which equally-authenticated handshake
runs; a MITM cannot downgrade or impersonate via it because both handshakes
authenticate both static keys. XX's ACL check necessarily runs after message 3
(the client's static only arrives there), so a rejected XX client's handshake
succeeds client-side and the rejection surfaces as a clean `ConnectionClosed` on
the data plane — the rejected client never sends or receives a single data-plane
byte. All four handshakes are bounded by an ABSOLUTE deadline enforced across
every handshake read AND write, so a peer that connects and stalls — or dribbles
bytes to keep a per-read timeout from firing — cannot hold a connection thread +
FD forever; deadline expiry surfaces as `TransportError::HandshakeTimeout`.

**`key` — transport keypair.** The on-disk keypair resolves through the shared
`choreo_shared::paths::config_dir()`, so `--base-dir` relocates it with the rest
of the instance. The human-comparable `fingerprint` rendering is a cross-crate
contract: base64 clustered into 4-char groups — bijective with the 32-byte key,
no hashing — and cross-checkable against the ACL's plain-base64 form by stripping
separators; it enforces exactly 32 bytes so a truncated file errors rather than
rendering a plausible lie.

The server-side TCP/Noise handler lives in `choreo-daemon/src/server/connection.rs`
(`tcp_client_thread`, with the preamble read + handshake-mode dispatch in
`tcp_handshake_and_client_thread`), where the Noise IK or XX handshake is
performed and the encrypted stream enters the same dispatch loop as Unix
socket clients.


### `choreo-client-core` — Shared client logic

Used by `choreo-tui`, `choreo-gui`, and `choreo-im`. The public API — modules,
types, functions, and error variants — is documented in-source:
`cargo doc -p choreo-client-core` (or `just doc`), held complete by the
`#![warn(missing_docs)]` + `doc-check` gate.

Two contracts here are cross-crate. The crate never depends on `choreo-daemon`:
an in-process connection is built by stuffing the raw crossbeam channel ends of
an embedded daemon's link into `ConnectionMode::InProcess` (the GUI creates the
link and passes the ends in). And the dispatch layer enumerates the daemon's
payload enum with no wildcard arm, matching the wire-contract rule that the
variant set IS the protocol, so a new variant is a compile-time triage point.
Every other per-module contract — the unified command model and catalog, the
connect-time keystore handshake, the `known_servers` trust store, the
pending-request table, and the session-transcript view — lives in its module's
docs.


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
  means a torn editor save never un-authorizes live clients.
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

### `choreo-mcp` — MCP client (Model Context Protocol)

Communicates with MCP servers over the **official `rmcp` SDK**, using either the
stdio transport (a child subprocess) or the **Streamable HTTP** transport (a
remote POST endpoint). Used by `choreo-daemon` to reach external MCP servers
and register their tools (via the daemon's `mcp` cargo feature, on by default —
an embedder that cannot host MCP subprocesses opts out with
`default-features = false`, which excludes the crate and makes the daemon's
`mcp/` module compile to a no-op stub). Because `rmcp` is async and the daemon is
thread-only, the crate owns one process-wide sidecar tokio runtime (`runtime`)
and one **dispatcher thread per server** (`session`); the daemon never sees an
`rmcp` type — the boundary is typed on this crate's own `McpServer`,
`McpServerHandle`, `McpTool`, and `CallToolResult`.

The public API — modules, types, functions, and error variants — is documented
in-source: `cargo doc -p choreo-mcp` (or `just doc`), held complete by the
`#![warn(missing_docs)]` + `doc-check` gate.

Two values inside the `rmcp` engine are shared across threads as documented
exceptions to the repo's channel-only thread-communication rule (see AGENTS.md).
The engine's `running` field — the live `RunningService` handle — sits in a
`tokio::sync::Mutex<Option<..>>` (the 8th sanctioned exception): a
connection-lifecycle handle that must stay alive for the whole connection and be
closed exactly once, so it is never touched per message — it sits behind a mutex
only because `close` needs `&mut`, and `shutdown(&self)` `take`s it and holds the
lock across the bounded `close_with_timeout(..).await`. The `ClientHandler`'s
notification rate limiter carries a shared `Arc<std::sync::Mutex<LimiterState>>`
(the 9th exception): `rmcp` delivers notifications to the handler on more than
one task, so the fixed-window counter that bounds a connection's server logging
notifications is shared across those callbacks — the lock guards only a handful
of integers and is never held across an `await`.

### `choreo-sockreg` — Provider-socket registry + keepalive tuning

Two responsibilities, one small leaf crate: (1) `SocketRegistry` — tracks live
provider sockets (by duplicate fd it OWNS) so a control thread can
`shutdown_all` them out from under workers blocked in a provider
`read`/`write` — channels cannot reach into a syscall, but
`shutdown(fd, SHUT_RDWR)` makes the blocked read return immediately — and
`prune_dead` (a non-blocking liveness probe) bounds growth; (2) `SocketTuning` —
TCP keepalive options applied once right after connect so a half-dead connection
(NAT timeout, cable pull) is noticed by the kernel. Registered sockets are
duplicated at registration; the registry closes every fd it holds exactly once
(explicit `close`, never `Drop`, so an already-closed fd's EBADF is logged rather
than double-closed).

The public API — modules, types, functions, and error variants — is documented
in-source: `cargo doc -p choreo-sockreg` (or `just doc`), held complete by the
`#![warn(missing_docs)]` + `doc-check` gate.

The registry is the 7th sanctioned shared-state exception (see AGENTS.md). Each
SESSION owns a registry (in `SessionState`), plus one daemon-owned one for
non-session-scoped work (model prefetch, catalog fetch — never individually
cancelled); each provider client is constructed against the registry of the unit
of work that will use it. The daemon command loop holds one clone per live
session (`DaemonState::session_registries`, entered at session spawn, dropped at
session exit) so it can force-close a wedged session's sockets from the one
thread that DECIDED the cancel — a session thread blocked in a provider `read()`
cannot observe a channel message, and `shutdown` is the only thing that can
un-block it. Cancellation granularity is exactly the SESSION (a registry = a
session; sub-sessions own their own; a parent cancel closes the whole subtree).
The internal lock is held only for a handful of fd syscalls and carries no
message traffic, so channels (which cannot express "un-block another thread's
blocked syscall") are not applicable. RAII deregistration completes the
lifecycle: `register` returns a `SocketId`, and the `ureq` connector's
`RegisteredTcpTransport` holds it plus a registry clone so its `Drop` calls
`unregister` — entry removal is the single close-ownership-transfer signal, so
the unregister after a prior `shutdown_all`/`prune` is a documented no-op and the
guard can never double-close. In steady state the registry tracks only LIVE
connections; the prune and the 256-entry cap remain purely as backstops. Both
platforms are implemented — Unix via `nix`, Windows via `windows-sys` — behind
one documented `socket_handle` helper.

### `choreo-power-events` — Platform suspend/wake notifications

One small leaf crate emitting `SuspendEvent::Sleep`/`Wake` on an unbounded
crossbeam channel, consumed by the daemon's command loop (which forwards each
event as `DaemonCommand::PowerEvent` over a dedicated `spawn_power_event_forwarder`
thread — the same forwarder-into-command-channel shape as the config/ACL
watchers).

The public API — modules, types, functions, and error variants — is documented
in-source: `cargo doc -p choreo-power-events` (or `just doc`), held complete by
the `#![warn(missing_docs)]` + `doc-check` gate.

These events are a CONVENIENCE layer, never a correctness layer: a machine can
suspend without the platform API firing, so consumers must treat missed events as
a non-issue and rely on sockreg's kernel keepalives as the fallback.
`PowerMonitor::new` is strict (returns the underlying error);
`PowerMonitor::best_effort` logs once and falls back to the inert mode — the
daemon uses `best_effort`. `handle_suspend_event` force-closes EVERY session's
socket registry (plus the daemon's own) on `Sleep` (proactively — the logind
event precedes suspension) and logs only on `Wake`.

### `choreo-ai-protocols` — Provider protocols

The wire-protocol layer for LLM providers. Owns the three chat client
implementations (`openai`, `anthropic`, `google`), the image- and
video-generation clients, the `ProviderClient` trait and shared turn/error/message
types they all use, the retry machinery, and the static provider catalog. It is
free of daemon concerns — no metrics, no account configuration, no sessions — so
it can be consumed independently of `choreo-daemon` (the daemon supplies those
concerns at the boundary, e.g. via `ProviderOverrides` and by timing calls
itself).

The public API — modules, types, functions, and error variants — is documented
in-source: `cargo doc -p choreo-ai-protocols` (or `just doc`), held complete by
the `#![warn(missing_docs)]` + `doc-check` gate.

**Threading — the single-writer `ArcSwap` snapshots (catalog + tool registry).**
Two process-wide values are held in an `ArcSwap` as a documented exception to the
repo's channel-only thread-communication rule (see AGENTS.md): the provider
catalog (`PROVIDER_CATALOG`) and the daemon's tool registry
(`DaemonState::tool_registry`, the `Arc<ArcSwap<ToolRegistry>>` every session
and request worker shares). For both, readers are lock-free and the swap is an
atomic `store()`, but there is a strict **single-writer invariant** — only the
daemon command loop calls `replace_catalog` (after a catalog refresh, overlay
change, or `/refresh-models`) and only the command loop swaps the tool registry
(on `DaemonCommand::McpListChanged`, after an MCP server reports a list change).
Every *change request* still travels by channel (maintenance thread → daemon
loop → store; the MCP forwarder thread → daemon loop → store); no other thread
mutates either value. Neither carries per-message data — each is an immutable
snapshot atomically replaced wholesale.

### `choreo-acp` — ACP bridge (Agent Communication Protocol)

Its own crate: the `choreo-acp` package owns the binary (`src/main.rs`, a thin
wrapper calling `choreo_acp::main()`), so default and release builds of the
root package exclude it entirely; build it explicitly with `cargo build -p
choreo-acp`. Entry point `src/main.rs`: initializes logging, connects to the
daemon's Unix socket, spawns I/O threads, runs the main event loop.

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
├── daemon-writer thread: receives ClientMessages via a crossbeam channel
│   and writes length-prefixed MessagePack frames to the daemon socket
└── main thread: event loop — receives from the shared event channel
    and dispatches to the appropriate handler
```

The public API — modules, types, functions, and error variants — is documented
in-source: `cargo doc -p choreo-acp` (or `just doc`), held complete by the
`#![warn(missing_docs)]` + `doc-check` gate.

**Key behaviors:**

- **Concurrency:** Pure OS threads with `crossbeam_channel` message passing. No `Arc<Mutex>` shared state.
  Threads communicate exclusively through a single shared event channel (`crossbeam_channel::Receiver<Event>`).
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
and the indexer (`tungstenite`) are synchronous. The tools keep their pre-rename
`coord_*` names; only the tool GROUP is named `content`.

The public API — modules, types, functions, and error variants — is documented
in-source: `cargo doc -p choreo-content` (or `just doc`), held complete by the
`#![warn(missing_docs)]` + `doc-check` gate.

### `choreo-daemon` — Core server (binary `choreographr`)

Entry point: `choreo_daemon::main` — invoked from the root package's
`src/bin/choreographr.rs` wrapper — first applies `--base-dir` (equivalently
`CHOREOGRAPHR_BASE_DIR`; see **The base directory**), then initializes logging
through the shared `choreo_shared::logging::init`: a hardened, pid-keyed log
file (`{base}/log/daemon-<pid>.log` under a base, else
`$XDG_STATE_HOME/choreographr/daemon-<pid>.log`, else the platform temp dir) is
always written (create-new `O_EXCL` + `O_NOFOLLOW`, 0600, replacing only a
regular file the daemon owns when a reused pid finds a stale one; logs older
than a week are pruned on startup), and every event is **also mirrored to stderr** (the
console or journald), so `--base-dir` never silences the console. `--log-file
<path>` chooses the file's path only (used verbatim, no pid key) — it never
changes the level and never mutes stderr; a log file that cannot be opened is
never fatal (the run degrades to the stderr sink with a warning), and the first
line of every run names the resolved file. Then
`main` creates
`DaemonState`, runs socket server. `--auto-exit` shuts the daemon down
gracefully when the last client disconnects — a mode intended for the TUI's
autospawned daemon, never needed for a user-managed service. The one-shot
utility subcommands (`acl-add`, `fingerprint`, `migrate`) exit before any
daemon work; `migrate` is the base-dir relocation helper (see **The base
directory**).

**Concurrency model:** Pure OS threads with message passing (actor model). No async code
in the daemon's own logic. All I/O uses blocking `std` APIs on dedicated threads. The one
exception is the optional `blockchain` and `content` features: `choreo-blockchain` (linked only with the former) and
`choreo-content` (linked only with the latter) each hold a tokio sidecar runtime for their async
alloy/subxt clients, and the daemon calls their synchronous `execute_*` entry points (which
`block_on` internally).

The public API — modules, types, functions, and error variants — is documented
in-source: `cargo doc -p choreo-daemon` (or `just doc`), held complete by the
`#![warn(missing_docs)]` + `doc-check` gate.

The following are the daemon's deliberate, single-purpose exceptions to the
workspace's channel-only thread-communication rule (see AGENTS.md); each is
lock-free or minimally scoped and carries no protocol data:

- **The single-writer `ArcSwap` snapshots (4th).** The provider catalog and the
  daemon's tool registry (`DaemonState::tool_registry`, shared with every session
  and request worker) are read lock-free; only the command loop swaps either
  (the tool registry on `DaemonCommand::McpListChanged`, after an MCP server
  reports a list change), and every change *request* still travels by channel —
  the swap is the only mutation.
- **The live-connection counter (3rd).** `server/lifecycle.rs`'s
  `Arc<AtomicUsize>` enforces `MAX_CONCURRENT_CONNECTIONS` atomically across the
  two accept paths and is released by every connection thread's exit (RAII
  `ConnectionSlot`) — a channel cannot express that without a dedicated
  accounting thread.
- **The delivery-lag byte counters (6th).** Each subscriber's in-flight byte
  backlog is incremented by the producers on enqueue and decremented by the
  connection writer thread on dequeue — inherently shared state that bounds
  per-client memory rather than carrying messages.
- **The client-id counter (10th).** `broadcast::ClientId`'s process-wide
  `AtomicU32` mints one unique id per accepted connection *before* its thread
  spawns; a channel round-trip would add latency on the accept path for a token
  no thread ever reads back.
- **The Windows Job Object (5th).** `tools/shell_util.rs`'s `Arc<ChildJob>` is a
  kernel handle (an index into the kernel table, not an address-space pointer)
  whose operations are thread-safe kernel-side, so the timeout watchdog and the
  drain thread share one immutable value to terminate the whole process tree.

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
    pub max_output_tokens_override: Option<u32>,
    pub no_retry: bool,
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
Two fields are per-call request knobs the cache-warming ping sets (and ordinary
turns leave at their defaults `None`/`false`): `max_output_tokens_override`
caps THIS call's output (a 1-token cap keeps the ping cheap) and `no_retry`
makes it a single best-effort attempt (realised by collapsing the effective
`RetryConfig` to `max_attempts == 1` via `RetryConfig::with_no_retry`). Each
protocol maps the cap onto its own output-length field: Anthropic `max_tokens`
(the thinking budget still derives from the configured `max_tokens`, so a
small cap cannot collapse it; `0` is Anthropic's documented cache pre-warm),
OpenAI Chat Completions `max_tokens`/`max_completion_tokens` (the slot the model
uses), Responses `max_output_tokens`, and Google
`generationConfig.maxOutputTokens` (emitted only when the override is set, so
ordinary turns are unchanged). `context_window_for_model()` returns the model's context window size, using a
resolution chain: per-model config → global fallback → catalog fallback.
Each client implementation maps the `&str` effort slug to its wire format:
- **OpenAI**: `reasoning_effort` field (`None` for `"off"`, otherwise slug → API string). For the Zhipu slugs (`zai`/`zhipuai`) the chat adapter instead applies the documented z.ai model-specific mapping (`zhipu_reasoning_effort_api_value` in `choreo-ai-protocols/src/openai/mod.rs`): GLM-5.3/-flash accept only `low`/`high`/`max` (unsupported slugs are coerced, `off` omits the field — 5.3 cannot disable thinking and thinks at its `max` default), and GLM-5.2-and-below follow the documented 5.2 family mappings (`minimal` skips thinking, `low`/`medium`→`high`, `xhigh`→`max`).
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
  `limit.context` / `reasoning` / `reasoning_options`, plus the per-model
  `cost` object (`input`/`output`/`cache_read`/`cache_write`, USD per million
  tokens) where the snapshot records one. The `catalog-gen` binary
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
  their full model lists verbatim), and the prompt-cache TTL policy
  (`prompt_cache_short` / `prompt_cache_long`, in seconds) — a provider-level
  default (`[provider.<slug>]`, e.g. Anthropic's 300/3600) with a per-model
  override on `[provider.<slug>.models."<id>"]`; models.dev carries no TTL
  fact, so the whole TTL is overlay policy.
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
- `prompt_cache` — provider-level prompt-cache TTL default (`short_secs`/`long_secs` in seconds; overlay policy; `None` = the provider declares none)
- `models` — curated `ModelEntry` list with `context_window`, `max_output_tokens` (from the snapshot's `limit.output`; `0` = unknown), and **wired as a clamp**: the outgoing `max_tokens` / `max_completion_tokens` / `max_output_tokens` request fields are clamped *down* to this fact when the lookup resolves and the request would exceed it (clamp-down only — a smaller request is never raised; see `ServiceConfig::clamp_output_to_catalog`), `reasoning_supported`, explicit `openai_reasoning_levels`, whether the model uses the Responses API (`openai_responses`), `reasoning_content_required` (ingested from the snapshot's `interleaved.field == "reasoning_content"`; see the resolver paragraph below), `supports_temperature` (from the snapshot's `temperature` flag; absent → permissive `true` — currently a **recorded-but-unwired** fact: no request builder sends a `temperature` parameter today, so there is nothing to gate; the fact and the `model_supports_temperature` resolver are kept so the gate exists the moment temperature sending is added), `deprecated` (from the snapshot's `status == "deprecated"`), `supports_vision` (whether it accepts image input; derived from models.dev `modalities.input` and overridable in the overlay), `cost` (the snapshot's `ModelCost` token prices — `input`/`output`/`cache_read`/`cache_write` in USD per million tokens; `None` when unrecorded — consumed by the cache-warming `payg` cost gate), and `prompt_cache` (per-model TTL override, wins over the provider default — consumed by cache-warming TTL scheduling). All snapshot facts are overlay-overridable per model without regenerating the blob

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
     ├─ Anthropic non-streaming: MessagesResponse.usage (input + output tokens,
     │                           + cache_read_input_tokens → cached_tokens,
     │                           + cache_creation_input_tokens → cache_write_tokens)
     ├─ Anthropic streaming:     message_start (input + cache_read_input_tokens + cache_creation_input_tokens) + message_delta (output)
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

**Key type** — `TokenUsage` (choreo-proto/src/types/common.rs):
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

**Daemon autostart (`autostart.rs`).** In Unix-socket mode only, the TUI
connects to the daemon socket DIRECTLY — there is no pre-flight probe. The
dial is `choreo_proto::connect_unix` (the ONE cross-platform unix-socket dial
primitive: std's `UnixStream` on unix, the `uds_windows` shim on Windows),
kept as a single helper by `choreo_client_core`'s
`run_daemon_connection_with_autostart`, which keeps the stream of a successful
first dial and invokes the TUI's autostart hook ONLY when the dial itself
fails with `NotFound` or `ConnectionRefused` (nothing listening — classified via
`choreo_proto::dial_error_means_no_listener`, the same predicate the daemon's
stale-socket probe mirrors, so the two sides can never disagree). The hook spawns
the SIBLING
`choreographr` binary (same directory as its own executable, via
`current_exe` — so both must be installed side by side, which the
.tarball/.deb/binstall layouts already guarantee) as a detached child with
`--auto-exit --log-file <state-dir>/choreographr/daemon-<tui-pid>.log`, polls the
socket (100 ms interval, 5 s budget — waiting for OUR OWN spawned child, not
probing a foreign daemon) via `choreo_proto::socket_listening` and returns; the
connection is then retried.
Autostart runs on the connection thread after the alternate screen is up, so
nothing is printed to the terminal — feedback travels as a
`UiEvent::Status` message ("no daemon running — starting choreographr…",
then "daemon started") painted on the UI's status line. A `Status` event is
flagged transient (`App::status_is_transient`) so the first real daemon
message clears it — the reassurance must not linger once the connection is
live and the first turn is quiet. Failures surface
as the TUI quit message
naming the daemon's log path. TCP mode (`--tcp-addr`) never spawns — a remote
daemon is not launchable from the client machine, by definition.
Once connected, a protocol-version mismatch (`ProtoError::UnsupportedVersion`)
quits with the actionable "the daemon's protocol version is incompatible —
restart the daemon (it may be an older build)" instead of a raw codec
error (`connection_quit_message`); every other connection error keeps the
generic wording.

**Thread topology:**

```
main()
├── reader task: read DaemonMessages from socket → push to UI event channel
├── writer task: receive ClientMessages from a crossbeam channel → write to socket
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
  8. Publish terminal-visible state OUTSIDE the draw closure (a raw stdout
     write during draw would interleave with the frame): if `term_status_dirty`,
     diff and write the OSC 7501 program-status records (`App::desired_status_records`
     → `terminal::status::Publisher::sync`) and re-emit the OSC 2 window title
     when it changed; the OSC 9;4 progress bar is emitted directly by the
     event handlers (Done/SessionState) rather than through this flag
```

**Keystore-lock awareness (persistent banner).** The TUI latches the daemon's
keystore locked state in `App::keystore_locked`, defaulting to `true` (assume
locked until told otherwise). It is latched from the daemon's authoritative
`Keystore { state }` status push (subscribe time and every transition) and the
targeted operation replies (`Keystore { Unlocked }`/`Unlocked` → `false`;
`Keystore { Locked }`/`Locked`/`LockedError` → `true`) in `connection/daemon.rs`
(`handle_daemon_message`), and — unlike the transient `status`/`error` lines a
keypress clears — it is NOT reset by the per-keypress clear, so it drives a
PERSISTENT status-bar banner (`🔒 keystore locked`, `render/mod.rs`) that
survives every keystroke until the daemon reports unlocked, and makes client
B's unlock update client A's banner. Startup surfaces the locked state; when no
unlock key resolves the TUI awaits the daemon's status push — `Keystore
{ state: Unbound }` triggers the once-per-connection AUTO-BIND (mint a fresh
key, `BindKeystore`, confirm on `Bound`), so a first-run client binds the
daemon with no user action, while `Keystore { state: Locked }` (bound to
another key) leaves the banner and guides the user to `/unlock <base64-key>`. Submitting a prompt (`RunInput`) to
a locked daemon is rejected CLIENT-SIDE with a clear "daemon is locked —
unlock it first" status instead of sending a message that would silently fail
at inference time (the client-driven guard beats waiting for a transient
`Failed`); while locked the status-bar context readout is suppressed (no
misleading `X / ?` fill when the context window isn't loaded), and `/lock`
(re-)latches the banner via a `Locked` broadcast.

**New-turn submit guard (client-side).** Every action that begins a new
inference turn — a plain prompt (`RunInput`), Alt+Enter, and the `/continue`
command (the latter two both becoming a `ContinueGeneration`, which the daemon
turns into a `RunInput`) — runs through
the single `App::new_turn_rejection` helper. It returns the rejection message
(`None` = allowed) for two conditions, in order: the attached session is not
idle, or the daemon's keystore is locked. The idle branch reads
`App::attached_status` (kept fresh from session summaries,
`SessionStatusChanged` broadcasts, and `SessionAttached` replies) and uses
`SessionStatus::is_idle` (exactly `Inactive`; `Sleeping` — the session-thread
exit marker — is not idle): a `RunInput` can only begin a turn from `Inactive`,
so the guard beats sending a message the daemon would answer with a transient
`session already has an active request`. The locked branch mirrors the
persistent lock banner. Like the keystore-lock guard was, this is a client-side
UX guard only — the daemon stays authoritative, and an unknown status (`None`,
e.g. a fresh client) fails open. For a plain prompt the guard runs *before* the
input buffer is cleared and the per-session draft forgotten, so a rejected
prompt stays in the input bar to resubmit; slash-commands (e.g. `/cancel`) skip
the guard entirely so they stay available while a session is busy — the lone
exception being `/continue`, which is itself a new-turn trigger and so is
guarded too. The two `ContinueGeneration` senders (Alt+Enter and `/continue`)
share a single `connection::chat::send_continue_generation` helper that owns
the guard, the request-id allocation, the in-flight tracking and the send, so
the triggers cannot drift; they differ only in whether they echo `> continue`
(the typed command does, the bare keypress does not).
`attached_status` (and `attached_tool_groups`) are cleared when the attached
session is deleted (`handle_session_deleted`), so a later prompt with nothing
attached fails open instead of being rejected against a dead session's stale
status. A future change replaces this blunt guard with prompt queueing for
async tool calls. See `connection/chat.rs` and `state::App::new_turn_rejection`.

The public API — modules, types, functions, and error variants — is documented
in-source: `cargo doc -p choreo-tui` (or `just doc`), held complete by the
`#![warn(missing_docs)]` + `doc-check` gate.

**Session-manager list views (`SessionManagerState`, in `state/session_manager.rs`).**  The page keeps the
FULL list in `all` and the rendered rows in `sessions`; a private
`rebuild_view` re-derives `sessions` from `all` on every mutation — sorting by
the SHARED `SessionSummary::cmp_for_list` (pinned desc, `last_modified` desc,
`session_id` desc) that the daemon's `ListSessions` also uses, so the two
orders cannot drift — keeping only the rows the
current `view` shows (`List` → `archived_at.is_none()`, `Archived` →
`archived_at.is_some()`, `Detail` treated as `List`), and re-pointing the
selection at the same session by id (clamping to the old row index — a
neighbour — when it left the view).  When the one-shot `pending_select` (set by
`select_session` when the manager opens on the attached session) names a
session that lives in the OTHER partition — an ARCHIVED attached session, with
the live list as the default view — `set_sessions` switches to that view so the
highlight lands on the session the user was viewing, not a first row of the
live list.  `Tab` is the only view-switch key
(`toggle_view`, List ↔ Archived; Alt+A is the Chat-page accounts shortcut).
`p` and `a` send `SetSessionPinned`/`SetSessionArchived` but NEVER mutate local
state: the daemon is the authority — it persists the flag change BEFORE
touching its in-memory index and broadcasting, so a failed persist can never
leave the daemon's list ahead of disk — and its `SessionFlagsChanged` broadcast
(rides the activity bus, so the TUI receives it on any page) —
applied via `App::handle_session_flags_changed` → `SessionManagerState::apply_session_flags`
(`rebuild_view` plus an in-place update of an open `detail_data`) — is the
success signal (there is no targeted reply; a failure arrives as a targeted
`SessionFailed` and is shown on the page).  Full-list lookups (status changes,
the attached summary, `attach_to_session`) read `all`, never the filtered
`sessions`, so an archived attached session still updates correctly.  A newly
created session refreshes the manager list via the broadcast
`SessionCreated` (`note_session_created`) while the user is still on that page;
the requester's own `SessionCreatedForRequester` reply instead navigates the
creator to the new session (`App::handle_session_created` →
`attach_to_session`, which leaves the Session Manager for the Chat page), so a
create from `n` on this page lands the creator on the session it just made.

### `choreo-gui` — Desktop/Android client (iOS: embedded-daemon host)

Entry point: `src/bin/choreo-gui.rs` (thin wrapper calling `choreo_gui::main()`
in `src/lib.rs`) — the crate owns its binary, as do all the suite binaries
since the binary split (choreo-tui / choreo-im / choreo-acp own their
wrappers too).

Unix socket or Noise IK encrypted TCP transport (selected via `--tcp-addr` / `--server-pk` CLI flags)
rendered via Dioxus components on the Dioxus Native (Blitz/wgpu) renderer —
one renderer for desktop, Android and iOS (no webview anywhere; the crate is
built as a lib+cdylib so dx/gradle can package it as an APK). Uses hooks to spawn async reader/writer tasks inside
the Dioxus runtime. Subscribes to the session summary at connect
(`SubscribeSessionsSummary`, alongside the initial `ListSessions`) so its session
list stays live via daemon push broadcasts — required since the daemon stopped
auto-registering TCP clients as summary subscribers.

**iOS: embedded in-process daemon.** On `target_os = "ios"` (and ONLY there —
the whole construction is `#[cfg(target_os = "ios")]` and the choreo-daemon
dependency is target-gated in Cargo.toml, so desktop and Android builds never
compile or link any of it) the GUI runs the daemon in-process:
`default_connection_mode()` calls `embedded_connection_mode()`, which opens
`DaemonState` via `DaemonState::open` under `ToolPolicy::Mobile` (sandbox-safe:
no shell/exec/RISC-V tools, no MCP subprocess spawning) at the standard
`dirs`-based paths (which resolve inside the app sandbox), spawns it with
`choreo_daemon::spawn_embedded`, and mints an `EmbeddedLink` whose channel ends
become `ConnectionMode::InProcess`. Messages travel as values — no codec, no
socket — and the same `ClientConn` state machine serves the connection. Any
construction failure is logged (`error!`) and the mode degrades to the
previous `TcpPinned` remote-daemon fallback (`IOS_DEFAULT_TCP_ADDR`), so the
app always launches. The `EmbeddedDaemon` handle is kept in a static
`OnceLock<Mutex<…>>` (`EMBEDDED_DAEMON`): there is no graceful-shutdown hook
in the Dioxus Native lifecycle, so shutdown happens at process teardown — the
`Drop` warn in `embedded.rs` is expected there. The embedded daemon's keystore
binding is keyed under the distinct stable string `"embedded"`
(`client.rs::connection_addr`), never under `socket_path()`, so it cannot
collide with a real unix daemon's binding; the UI shows the label
"embedded daemon" for this mode. Desktop behavior is byte-for-byte unchanged
(UnixSocket default, pinned by `default_mode_is_unix_socket_on_host`; the iOS
branch is `#[cfg]`-selected, so an on-host test can only pin the desktop
branch — the iOS branch is exercised on-device and by `scripts/check-ios.sh`'s
target compile).

**On-device tools + the settings store (iOS only).** The embedded daemon gets
`OpenOptions.platform_tool_bridge: Some(SwiftIosToolBridge)` — the C-ABI bridge
to `ios/IosToolHost.swift` (see `choreo-daemon`'s `tools/ios_bridge` module
docs) —
which makes `DaemonState::open` register the four protected on-device tools
(`clipboard_write`, `clipboard_read`, `open_url`, `notify`). The bridge hand-off
is gated by the user's persisted **on-device tools** setting (default ON — all
four tools are iOS permission-free). `settings.rs` is the GUI's own minimal
preference store (the crate's FIRST settings mechanism): `gui-settings.toml` in
the shared config dir (via `choreo_keystore::paths::config_dir()` — inside the
iOS app sandbox, NOT the daemon DB, because the bridge decision happens BEFORE
any daemon exists). Load is deliberately tolerant (missing/corrupt/unparseable
file → defaults + warning; every field is `#[serde(default)]`, so older files
stay parseable), and persistence is a whole-file rewrite (tiny file, single
GUI writer — no advisory lock, unlike known_servers.toml). A toolbar toggle
(`OnDeviceToolsToggle`, iOS-only; desktop/Android render an empty component so
the call site compiles unchanged) flips the setting and persists it; BOTH the
button label and the confirmation status state that the change **applies on
next app start**, because the bridge is handed to `DaemonState::open` during
startup and the protected group cannot be re-registered live. The in-session
toggle reads/writes a startup-cached `AtomicBool` (a single startup-resolved
bit, no protocol data); the persisted FILE is the source of truth the next
launch reads, and it is written BEFORE the cache adopts the new value, so a
failed write leaves the cache matching disk. All of the settings surface is
compiled on every target (platform-neutral concept; host unit tests cover the
load/persist round-trip), with the iOS consumers cfg-gated; a plain desktop
build's behavior is unchanged.

The public API — modules, types, functions, and error variants — is documented
in-source: `cargo doc -p choreo-gui` (or `just doc`), held complete by the
`#![warn(missing_docs)]` + `doc-check` gate.

### `choreo-im` — IM platform bridge

Its own crate: the `choreo-im` package owns the binary, so default and release
builds of the root package exclude it entirely; build it explicitly with
`cargo build -p choreo-im`.

Entry point: `src/main.rs` (thin wrapper calling `choreo_im::main()` in the
library)

Single binary (`choreo-im`) that bridges IM platforms to the daemon.
The binary takes a single required positional platform argument via clap:
`choreo-im telegram`. Like the rest of the suite it supports `--help` and
`--version`, with help output styled by a per-crate `clap_styles()` helper
(duplicated in each CLI crate — choreo-proto is the wire protocol and does
not host CLI styling) and `ColorChoice::Auto` (color only on a TTY).

**Credentials:** The daemon serves platform credentials via the `GetCredential` wire
message. The admin stores credentials via `/add-key` or `/add-x` at runtime, which
encrypts them with the derived unlock-key public key. On unlock (`/unlock`) the daemon decrypts
all stored credentials into memory using its private key.

The public API — modules, types, functions, and error variants — is documented
in-source: `cargo doc -p choreo-im` (or `just doc`), held complete by the
`#![warn(missing_docs)]` + `doc-check` gate.

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

Each daemon's credential keystore is governed by a keypair whose private half (the
**unlock key**) is held client-side, one per daemon, in the client's
`known_servers.toml`. The daemon stores only the public half as a **binding**, and
it starts **locked** with no credentials in memory. The binding is created by
exactly ONE wire path, `ClientMessage::BindKeystore` (TOFU): on an unbound
keystore it ADOPTS the presented key (loud `KEYSTORE BOUND` log), runs the shared
unlock tail, and replies the targeted `DaemonMessage::Bound` — bind implies
unlock; on an already-bound keystore it verifies only (a mismatch is the usual
wrong-key rejection, never an overwrite). `Unlock` and `AddCredential` are
strictly VERIFY-ONLY: against an unbound keystore they answer the targeted
`DaemonMessage::KeystoreUnbound` (deliberately distinct from `LockedError`,
which means "bound but wrong key") and never adopt anything. Binding keys are
ALWAYS freshly generated by the client's CSPRNG at bind time (auto-bind — there
is no `/bind-key` command); pre-held keys (stored known_servers key or the
legacy raw `identity.pk` file) verify existing bindings and never create new
ones. Binding is TOFU-once — rotation is not implemented.

A client sends `ClientMessage::Unlock { private_key }` (resolved verify-only as
its stored per-daemon unlock key, else the legacy raw `identity.pk` file —
copied into the store on first use; a caller-supplied base64 key from
`/unlock <key>` is decoded WRITE-FREE — recording happens only on the daemon's
targeted confirmation). `AddCredential` REQUIRES the unlock key and, on a valid
blob, **implicitly unlocks** the daemon (decrypts all blobs into memory). The
client records the key per-daemon ONLY after the daemon's targeted confirmation
(`Unlocked` / `Bound` / `CredentialAdded`) — except the auto-bind key, which is
recorded PRE-SEND (mandatory: an unbound daemon adopts whatever key arrives
first, so even a lost confirmation leaves the recorded key matching the
binding). Nothing is ever auto-deleted; `KnownServers::remove(addr)` re-pair is
the only removal path.

**ORDERING INVARIANT:** the targeted reply to Unlock/BindKeystore/AddCredential
is enqueued by the daemon command loop into the acting client's writer sink
BEFORE any lock-state transition broadcast — both travel the same per-client
FIFO writer queue, so the broadcast can never overtake the reply. Client
key-recording correctness keys on this (implemented via `DaemonCommand`
carrying the client's `SubscriberSink`; pinned by the integration test
`unix_targeted_reply_precedes_lock_state_broadcast`).

```
startup              connect→unbound?        /unlock [key]            AddCredential{... unlock_key}
   │                       │                       │                          │
   │  locked               │  AUTO-BIND (once per  │ verify-only resolve      │ verify = binding;
   │  (no credentials)     │  connection): mint    │ unlock key; unbound →    │ test-decrypt the blob
   │                       │  fresh key, record    │ KeystoreUnbound →        │ (reject if bad); unbound
   │                       │  PRE-SEND, BindKeystore│ auto-bind; match →      │ → KeystoreUnbound;
   │                       │  → Bound (implies     │ decrypt all credential   │ then run the SAME unlock
   │                       │  unlock)              │ blobs, load accounts     │ tail → Unlock + CredentialAdded
   ▼                       ▼                       ▼                          ▼
```

- Credentials are encrypted per-credential with ECDH (X25519) + HKDF + AES-256-GCM, keyed
  to the daemon's unlock-key pubkey; the daemon test-decrypts every incoming blob with its
  bound key and refuses to persist one that does not decrypt (one key per keystore).
- The unlock key is sent over the channel; zeroized after use by the daemon
- `/lock` destroys all in-memory credentials, returning to locked state
- **The locked state is authoritative and broadcast.** `DaemonState` keeps a single
  `locked: bool` (starts `true`; `false` after a successful Unlock, BindKeystore,
  or AddCredential implicit unlock — the shared `unlock_tail` is the one place it is
  cleared; `true`
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
  `handle_list_models_inner`, which NEVER fetches on the command loop: a fresh cache answers
  immediately; a stale/missing cache triggers the same background prefetch (no-op while one
  is in flight, so an open picker cannot double-fetch) and serves the stale list if one
  exists, or a retryable "warming" error otherwise. A panic inside the fetch thread is
  caught (`catch_unwind`) and reported as a normal failure so the in-flight guard can never
  leak, and a result arriving for an account whose provider was removed or rebuilt mid-flight
  is discarded instead of cached.
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
  over-8-MiB frame drops the connection) and kill escalation on shutdown; a per-server
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
  `headers` (OAuth is a post-ship addition), never shared across issuers.


---

## Tool system

The daemon's tool machinery — the typed `Tool` trait, its type-erased `ToolDyn`
counterpart, the `ToolRegistry` that owns and dispatches every tool, and the
`define_tool!` macro — is documented in-source (see `choreo-daemon`'s `tools`
module, `cargo doc -p choreo-daemon` / `just doc`), held complete by the
`#![warn(missing_docs)]` + `doc-check` gate. A tool declares its own argument and
return types (both `serde`- and `schemars`-compatible, so its JSON schemas are
auto-derived and sanitized for provider compatibility), runs on a dedicated
per-call thread, and may stream incremental output over a channel. Tools are
`Send + Sync`, are registered once at daemon startup, and are shared across every
session — so a tool must keep no per-invocation state.

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

### Data model

Sessions are persisted to a `redb` (v4) embedded key-value store at
`~/.local/share/choreographr/state.redb`. The table schema — every table's key and value
codec, the zstd turn-value compression, the turn-attachment byte split, the deletion
tombstone, and the runtime catalog-refresh state — is documented in-source (see
`choreo-daemon`'s `db` module, `cargo doc -p choreo-daemon` / `just doc`), held complete by
the `#![warn(missing_docs)]` + `doc-check` gate.

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

The persisted fields live on `SessionConfig`; the runtime-only fields live on
`SessionState` (the in-memory turn map, the subscriber set, the in-flight request
map, the per-session provider client and socket registry, and the context/skill
caches). The structs and every field's rationale — the `last_response_id`
response-chain provenance, the per-session `next_stream_id` counter, the
`context_cache` fingerprint — are documented in-source (see `choreo-daemon`'s
`sessions` module, `cargo doc -p choreo-daemon` / `just doc`), held complete by
the `#![warn(missing_docs)]` + `doc-check` gate.

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

Sessions support undo/redo via an `undone` boolean flag on each `Turn`. The `Turn`
struct — its `turn_id`/`undone`/`user_text` fields, the assistant-response bundle,
and the opaque `reasoning_artifact`/`reasoning_producer` round-trip payload — is
documented in-source in `choreo-proto` (alongside its `ReasoningArtifact` and
`ReasoningProducer` companions; `cargo doc -p choreo-proto` / `just doc`), held
complete by the `#![warn(missing_docs)]` + `doc-check` gate. Two cross-cutting
facts about that payload: the daemon strips it from every client-bound
`DaemonMessage` (only its own request builder reads it), and the builder's
same-model provenance check drops the artifact after a mid-session model switch.

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

**Credential storage:** Credentials are encrypted per-credential in the `redb` database (`state.redb`). Each daemon's keystore is bound to one client-held unlock key (the daemon stores only the derived public binding, created once via the `BindKeystore` wire path — TOFU-once, no rotation); the legacy `identity.pk` / `public.pk` pair is removed, and the legacy raw `identity.pk` file is an unlock-verification fallback that is copied into `known_servers.toml` on first use (never deleted, never binds).

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

### Exposed metrics

The full metric set — names, types, labels, and the exported help text — is documented
in-source in the daemon's `metrics` module, where every `record_*` function names the metric
it updates (`cargo doc -p choreo-daemon --features metrics` / `just doc`), held complete by
the `#![warn(missing_docs)]` + `doc-check` gate.

Process-level metrics (RSS, CPU, FD count) are also exposed via the `prometheus`
crate's `process` feature.

### Implementation

The metrics module (`src/metrics.rs`) keeps the real implementation in a
feature-gated `backend` module (selected by `#[cfg(feature = "metrics")]`,
with a no-op stub backend when disabled, re-exported behind the same public
API) using `std::sync::OnceLock` for a single static `Metrics` struct that
wraps Prometheus counters/gauges/histograms. All operations are atomic (no
`Arc<Mutex>` needed). A dedicated thread serves the `/metrics` endpoint via
`tiny_http`; because a blocking `tiny_http` accept cannot be interrupted by a
channel, its serve loop is the one place that consults the exception-#1
shutdown flag on a bounded (1 s) `recv_timeout`, exiting cleanly once the flag
is set.

### Instrumentation points

Where each metric is recorded is documented at the call site — every `record_*` call lives
in the module it instruments (`sessions.rs`, `requests/`, `cache_warm.rs`, `providers/`, and
`server/`), and the daemon's `metrics` module names each `record_*` function and the metric it
emits.

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

Images are delivered out-of-band via a crossbeam channel rather than embedded in
`ToolOutput` (a tool can emit several — an MCP result may carry multiple image
blocks):

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
a decompression-bomb guard. An optional `region` (given as fractions of the image) crops
before that resize — raster/HEIC sources are cropped from the decoded pixels (after EXIF
orientation is baked), while SVG sources render only the region into the pixmap — so
a small crop reaches the model at native resolution instead of the ≤2000px downscale, and
no cropped copy is written to disk. The tool reports a text handle (path, dimensions, MIME,
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
  → build_chat_request_messages: decay gate + model_supports_vision gate
      turn in current request + vision model
                    → attach ImageReference.data → ChatRequestMessage.images (synthetic user msg)
      older turn (decay) OR turn in current request + text-only
                    → placeholder text message (never pixels) — decay / vision gate
  → provider serializer: OpenAI chat image_url / Responses input_image /
                         Anthropic image / Google inline_data
```

The vision image's **bytes** never ride the snapshots: `turn_for_client` keeps
`ToolResultRecord.image`'s `ImageReference` METADATA (path, mime, dimensions) but empties its
`data`, exactly as it strips a displayed image's bytes while keeping its `ImageMetadata` — so
the client learns the image exists and can size a placeholder. Displayed images
(`displayed_images`, from `display_image`/`generate_image`/`retrieve_webpage` screenshots) and
tool-result vision images persist in the SAME `session_attachments` table and are served to
clients **on demand** through ONE fetch pair: the session-scoped snapshots carry only
metadata, and a client fetches an attachment's bytes when it needs to render it
(`ClientMessage::GetImage { key: ImageKey }` → `DaemonMessage::Image`, where `ImageKey`
selects the attachment — `Displayed { index }` or `ToolResult { call_id }`; the CONNECTION
THREAD reads a single slot via `read_attachment` against its own shared
`Arc<redb::Database>` handle — no command-loop round-trip). `read_attachment` maps the key to
its slot (`d{index}` / `r{call_id}`) in the one place that owns slot naming, so both
attachment kinds share one reader. This is what lets the TUI show the user the exact
normalized image the model saw — even after the model-side decay gate stops re-attaching it.
`emit_image` persists each displayed image the instant the tool produces it (persist-at-emit)
and the tool-completion path persists each vision image the same way (slot `r{call_id}`), so
the DB is authoritative for the bytes even mid-request, when the turn's final write has not
happened yet — the on-demand fetch must never race that write. That emit-time write is a
SINGLE-slot insert (`db::write_attachment`: one row, one transaction, no stale-slot clear), so
an N-image turn costs O(N) disk writes across its emits rather than the O(N²) a whole-turn
rewrite-per-image would incur; the full turn (blob + all attachments) is still (re)written
atomically at `finalize_turn`.

**Image decay** replaces the old always-replay policy: previously the same normalized
bytes rode EVERY later request too, so a long image-heavy session re-attached megabytes
of PNG/JPEG to each request indefinitely. The builder now takes the request's first
turn id (captured exactly by `run_agent_loop` as `session.next_turn_id` before its first
`start_turn`; `None` for callers with no request in flight — the `session_inspect`
dry-run): only turns at or after that id (the request's own tool-loop turns, user turn
included) attach pixels, every OLDER turn emits the placeholder instead. Decayed
placeholders never convert back to pixels. A session loaded from disk decays
all historical images automatically (no persisted marker — the window is derived purely
from each request's own turn-id boundary, so no schema change), and undone turns are
skipped by the builder before the decay gate, so undo still never resurrects anything.
The placeholder names the source path so the model can re-read the file with a text tool
if it needs the image again.

The **vision gate** is `catalog::model_supports_vision(provider_slug, model)` (from the
models.dev `modalities.input` flag, overridable via the overlay): on a text-only model
no bytes are ever sent, whatever the decay marker says. Attached images contribute
a fixed `IMAGE_TOKEN_ESTIMATE` (1000)
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
      • preserves the per-session reading position as an ABSOLUTE anchor: on
        the way out it captures the content line at the top of the viewport
        (`ScrollRestore { top_line, at_bottom }`), and on the target's first
        rebuild the anchor is converted back to a from-bottom offset against
        the fresh total.  The raw offset is only a distance from the bottom,
        so it would slide the content when the viewport height differs on
        return (the help/status bands reflow on attach) or when the session
        streamed in the background — the anchor is immune to both.  A session
        left pinned to the bottom (`at_bottom`) follows new content instead.
      • resets only transient render state (markers, height caches), which is
        rebuilt on the next layout pass (markers_dirty); the anchor is applied
        at the end of that rebuild
      • the UI loop's pre-render `clamp_scroll_state` is guarded on
        `markers_dirty` (skip while a rebuild is pending), so it cannot clamp
        against the cleared (max_scroll 0) height cache before the draw-time
        rebuild runs; render still clamps for the frame, and the next frame's
        clamp settles any real overflow
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
different files stay parallel (pi's `withFileMutationQueue` is the same idea).
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
status bar.  A user cancellation is not a failure: the dispatcher routes
`SessionEvent::Cancelled` to its own required `handle_cancelled` (distinct
from `handle_failed`), which runs the same request teardown but reports
`idle` and writes no error text.

The one inference outcome that is *not* terminal is a truncated tool call
(`InferenceError::TruncatedToolCall`): the provider cut a response off at its
output-token limit mid-tool-call, so the partial (invalid-JSON) call was
discarded as unsafe to execute. Rather than fail the request, `run_agent_loop`
records the truncation as an explanatory assistant turn, seeds
`TRUNCATION_RECOVERY_INSTRUCTION` as the next turn's user text, and retries —
bounded by `MAX_TRUNCATION_RECOVERIES` so an output-limited model cannot spin
the loop forever (this bounding is necessary because sessions may run with
`max_turns == 0`, i.e. unlimited). For `ResponseId`-policy (Responses-API)
providers the retry also clears `previous_response_id` (and the persisted
session id): the truncated turn's own response id was never captured, so
chaining onto the pre-truncation id would replay a `function_call` whose
matching `function_call_output` was dropped, leaving an unpaired call on the
wire — the retry resends the full, self-consistent history instead. The
observed trigger is a single oversized `write_file` whose arguments exceed the
provider's output budget (e.g. `glm-5.3-flash` on `opencode-go`).

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
  Archived sessions are excluded first: they are hidden from the live list, so
  they must not be auto-opened either — when EVERY session is archived the
  bootstrap creates a fresh default session instead of attaching to one.
- The auto-attach rule keys on the EVENT, not on `parent_session_id`:
  `SessionCreatedForRequester` — the direct reply to THIS client's
  `CreateSession` (parent `None`) — is the ONLY create event that may attach.
  It navigates the creator to the new session from ANY page — including the
  Session Manager, where `n` (or `/new`, or `/session new`) creates — via the
  shared `App::attach_to_session` (`UnsubscribeSessionsSummary` + `AttachSession`,
  then the Chat page); a `ListSessions` is sent first so the summary is present
  when `SessionAttached` fills any remaining display gaps.
- Every broadcast `SessionCreated` is notification-only (`App::note_session_created`):
  the new session is never attached, whether it is a top-level session created
  by ANOTHER client (the phone-view-follows-laptop bug this split fixes) or an
  agent-spawned sub-session (`parent_session_id = Some`, e.g. from
  `spawn_subsession`).  The session list is refreshed only while the user is on
  the Session Manager page — an unsolicited `ListSessions` from the Chat page
  would make the daemon reply with `Sessions`, whose handler writes the
  global status line and reflows the viewed viewport.
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

3. **Lock/Unlock security** — the daemon starts without credentials in memory. The unlock key travels
   only over authenticated transport and is zeroized after use. Credentials are encrypted per-credential so
   they can be stored in the database without a global passphrase. The keystore binding is established once (TOFU) via the `BindKeystore` wire path — the only message that can create it; Unlock/AddCredential are verify-only.

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
    `Signature` / `ResponseId`, per-model override else protocol default). Display text stays in
    `Turn.assistant_reasoning`; the artifact bytes are never interpreted by the daemon.




---

## Context file discovery

The daemon automatically discovers and injects project-specific context files
(`AGENTS.md`, `CLAUDE.md`) and skills at session creation, and refreshes them
before every model call (every turn of the tool-call loop).

### System prompt construction

Each turn in the agent loop calls `build_system_content()`, which **always**
returns a full system prompt (a `String`, not an `Option<String>`), assembled
from four sources:

1. **Base prompt** — identity, tool group listing, available skill metadata, any
   loaded skill bodies (accumulated via `load_skill` calls), and the session
   title. These are built unconditionally, so a session with **no** working
   directory still receives a full system prompt.
2. **Project context files** (`AGENTS.md`, `CLAUDE.md`, etc.) — discovered by
   `discover_context()` and assembled by `assemble_context()`. This source
   requires a working directory and contributes nothing when the session has
   none. Results are cached
   on `SessionState::context_cache` (fingerprint + assembled text) and reused
   when the fingerprint is unchanged.
3. **Subdirectory hints** — hints accumulated from filesystem tool calls in the
   previous turn, appended under "## New context from project subdirectories".
   Requires a working directory.
4. **Loaded skills** — `<skill name="...">...</skill>` blocks injected after the
   "Available skills" listing.

Global skills (`~/.agents/skills`) are always scanned; only the project-local
skills walk (`.agents/skills/` under the working directory) needs a directory.
Skills are deduplicated by frontmatter `name`, and the project walk is scanned
before the global scope, so a project-local skill **shadows** a same-named
global one. The system prompt is rebuilt every turn so that newly loaded skills
and newly discovered subdirectory hints are visible to the model immediately.

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
- `~/.agents/skills/<name>/SKILL.md` (global — always scanned)
- `.agents/skills/<name>/SKILL.md` (project, relative to session working directory — scanned only when the session has a working directory)

The project scope is scanned before the global scope and results are
deduplicated by frontmatter `name`, so a project-local skill shadows a
same-named global one (and, within the project walk, a more-local skill shadows
one higher up the tree). Within a single scope the candidate directories are
visited in sorted order, so if two directories declare the same `name` the
lexicographically-first path wins deterministically (rather than depending on
the unspecified `read_dir` order).

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
| `SkillScopes { global_home, working_dir }` | The two skill-discovery scopes as a named struct — a project walk scoped to the session working directory, plus the always-on global `<global_home>/.agents/skills`. Named (not two positional `Option<&Path>`s) so the two same-typed scopes cannot be swapped by mistake. |
| `discover_skills(SkillScopes)` | Scan Agent Skills directories — the project-local walk under `scopes.working_dir` (only when a directory is present) first, then the global `<scopes.global_home>/.agents/skills` — deduplicated by frontmatter `name` so project skills shadow global ones; return `Vec<SkillMeta>`. `global_home` is injected so tests can supply a temp home. |
| `discover_skills_ambient(working_dir: Option<&Path>)` | `discover_skills` with `dirs::home_dir()` as the global scope — the production entry point |
| `assemble_context(bundle)` | Render discovered files into an XML-like format for injection |
| `build_base_prompt(skills, groups, loaded_skills)` | Build the stable system prompt (identity + tool groups + skill metadata + loaded skill bodies) |
| `recheck_context(working_dir, config, old_fp)` | Re-discover and compare fingerprints |
| `subdirectory_hints(tool_name, args, working_dir, known)` | Return `Option<(String, Vec<PathBuf>)>` — subdirectory hint text and newly discovered paths |
| `load_skill_body_from(skills: &[SkillMeta], name)` | Read a skill's body from an already-resolved skill list, stripping YAML frontmatter. Both the `load_skill` tool (via `ToolContext::discovered_skills`) and `persist_loaded_skill` resolve against the SAME `SessionState::discovered_skills` snapshot |
| `load_skill_body(name, working_dir: Option<&Path>)` | Discover then read a skill's body (a fresh ambient walk); the `load_skill` tool falls back to this only when it has no session snapshot to resolve against |

### Tool: `load_skill`

Registered alongside other tools in the tool loop (core group). When the model calls
`load_skill(name)`, the daemon:

1. Finds the matching skill in the session's `discovered_skills` snapshot, which the
   agent loop computes once per request and shares with tools via
   `ToolContext::discovered_skills` (falling back to a fresh ambient walk only when no
   snapshot is available, e.g. a direct unit-test call)
2. Strips the YAML frontmatter
3. Returns `"Loaded skill: <name>"` as the tool result

Resolving against the shared snapshot means the tool needs no second filesystem walk and
the body it returns can never diverge from the body `persist_loaded_skill` records into
the system prompt — both read the same list.

**Persistence:** After the tool result is collected, `run_agent_loop` detects the
`load_skill` call and pushes a `LoadedSkill { name, body }` into
`SessionState::loaded_skill_bodies`. On every subsequent turn, the
`build_system_content` helper includes all loaded skill bodies in the system prompt,
wrapped in `<skill name="...">` XML tags. This ensures skill instructions remain
visible to the model even as tool results scroll out of the context window.

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

Shell tools (`sh`, `fish`, `nu`, `powershell`, `exec`, and the streaming
variants) put the
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
AGENTS.md (exception #5) and `choreo-daemon`'s `tools/shell_util.rs` module docs.

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
| Daemon | Request lifecycle, session CRUD, cancellation, tool calls, model listing | `choreo-daemon/src/tests.rs`, `choreo-daemon/tests/it/session_integration.rs`, `choreo-daemon/tests/it/lifecycle_integration.rs` |
| MCP (choreo-mcp) | Server spawn / Streamable HTTP connect, tool discovery, echo/error/structured/image calls, both protocol eras (`server/discover` + `Auto` fallback to `initialize`), schema normalization, cancellation (including the fixture asserting the SERVER observed `notifications/cancelled`), progress notifications turned into streamed chunks, the MRTR decline-and-retry loop for `input_required`, resource list/read, the `subscriptions/listen` list-change forwarding (a `modern-list-changed` fixture emits a tools list-changed that must reach the caller's sink, and a server without the capability must NOT have a stream opened), crash/no-init/garbage/oversized scenarios (the oversized case now failing on the client's own 8 MiB frame cap, plus `BoundedLineReader` unit tests); over HTTP: JSON and SSE responses, generated-header validation, `Auto` fallback, a retryable `503` connect, and rejection of the removed HTTP+SSE transport; the dispatcher protocol (including the concurrency cap: `CallGate` unit tests and a deterministic dispatcher test proving an excess call is queued until a slot frees), error mapping, retry/backoff math, and config parsing are unit-tested against a mock engine (no sleeps); the P6 bounds are covered too — the tool-count cap, the schema depth/size bounds (`json_depth`, `normalize_input_schema`/`normalize_output_schema`), the configurable restart cap (`RestartPolicy::new(0)` disables reconnect), the notification rate limiter, and the bounded SSE retry policy each have wait-free unit tests, and a `stubborn` fixture (which ignores stdin EOF) backs both a `choreo-mcp` and a daemon test proving shutdown returns within its bounded wait; the official client conformance suite runs both protocol eras against the `mcp-conformance-client` harness with a committed expected-failures baseline | `choreo-mcp/tests/it/mcp_integration.rs` (stdio, against the in-tree `tests/fixtures/fixture_server.rs`) and `choreo-mcp/tests/it/mcp_http_integration.rs` (a local `TcpListener` HTTP fixture); dispatcher/config/engine/retry/stdio unit tests in `src/`; the conformance harness in `tests/fixtures/conformance_client.rs` via `scripts/mcp-conformance.sh` |
| MCP (daemon) | McpManager + ToolRegistry integration, dynamic group registration (tool wrappers plus the `resources`-capability catalogue tools), tool execution, progress streaming through the wrapper, image attachment via the sink, the `subscriptions/listen` → shared-channel forwarding and the `register_all` / `refresh_server` + `register_cached` catalogue re-registration (the latter pinned by `mcp_list_change_only_relists_the_changed_server`, which proves the unchanged server is not re-listed), the list-change handler's atomic registry swap (unit-tested with no servers), `McpManager::status` for the `/mcp` surface and `session_inspect`, `McpManager::reload` (an empty reload and a malformed-config hard failure, unit-tested) and the `/mcp reload` connection handler (success summary + refreshed list, and the config error reply), and `mcp.json` parsing (transport inference, `${VAR}` expansion, `cwd`/`~` expansion, `disabledTools`, `maxRestarts`, `shared`, and the two-tier daemon/project load with the trust-gated project expansion, plus a fuzz-style malformed-input corpus), the project-root walk, the trust store (exact-canonical matching, no ancestor inheritance, fail-closed on garbage), same-root re-resolve reuse (connection count) and the reload/reconnect/trust-reload reconciliation paths (a changed `shared = false` slot is dropped, a per-session slot reconnects, a server FLIPPED from `shared = true` to `false` drops its stale daemon shared slot, an UNTRUSTED project does not suppress a daemon per-session server, a failed project connect does not shadow the daemon group, and an unchanged trust set is a no-op) | `choreo-daemon/tests/it/mcp_integration.rs`; `choreo-daemon/tests/it/mcp_project_trust.rs`; the registry-swap unit test in `src/daemon/tests.rs`; config unit tests in `src/mcp/config.rs`, `src/mcp/trust.rs`, `src/mcp/mod.rs` |
| Providers (`choreo-ai-protocols`) | SSE parsing, HTTP request construction, chat completions + responses serialization, content-block deserialisation, config overrides, catalog lookups | `choreo-ai-protocols/src/openai/tests.rs`, `choreo-ai-protocols/src/openai/chat_completions.rs`, `choreo-ai-protocols/src/openai/config.rs`, `choreo-ai-protocols/src/anthropic/tests.rs`, `choreo-ai-protocols/src/google/tests.rs`, `choreo-ai-protocols/src/catalog/mod.rs` |
| choreo-tui | SVG rasterization, Unicode width, app state | `choreo-tui/src/app_tests.rs`, `choreo-tui/src/lib_tests.rs` |
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
`#[ignore]` suite), and `test-all` (both) invoke it with `--workspace`; plain
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
