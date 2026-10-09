# Plan: BSD release target

**Status:** proposed — dependency/source analysis complete; **no BSD build has been
attempted**. FreeBSD is the primary target (a drop-in Rust Tier-2 host); NetBSD
x86_64 is nearly free; OpenBSD and DragonFly are a tier harder (Tier 3, no
prebuilt `std`) and are deferred.
**Date:** 2026-10-09
**Targets:** `x86_64-unknown-freebsd` + `aarch64-unknown-freebsd` (both Tier 2
with host tools), then `x86_64-unknown-netbsd`; Tier-3 `x86_64-unknown-openbsd`
and `x86_64-unknown-dragonfly` as a stretch.
**Touches:** `scripts/release.sh`, `scripts/install.sh`, `justfile`,
`.github/workflows/release.yml`, `packaging/` (a FreeBSD `rc.d` unit),
`Cargo.toml` (binstall override, optional), plus docs (`README.md`,
`packaging/README.md`, `ARCHITECTURE.md`, `RELEASE.md`).

> **TL;DR.** The workspace is already written behind `unix`/`linux`/`macos`/
> `windows` cfgs, with generic-Unix fallbacks in exactly the Linux-specific
> places (pidfd process kill, keepalive tuning, power events) and one BSD branch
> already present (the shell resolver). FreeBSD/NetBSD are Rust **Tier-2 host
> targets** — `rustup` ships `std`, the release pipeline stays on **stable**, and
> the crypto stack is covered (aws-lc-rs lists `x86_64-unknown-freebsd` and
> `x86_64-unknown-netbsd` as build+test-passing). The real work is the
> **build/release/CI/packaging layer**, which is hardcoded to Linux+macOS today.
> Unlike the MIPS/RISC-V plans there is **no `zlob` blocker** — the BSD triples
> are Zig-known. The one genuinely new engineering cost is **CI**: GitHub has no
> hosted BSD runner, so a BSD job must run in a VM action.

---

## Table of contents

1. [Motivation & evidence](#1-motivation--evidence)
2. [Target tiers & which BSD to ship first](#2-target-tiers--which-bsd-to-ship-first)
3. [The portability surface (what already works)](#3-the-portability-surface-what-already-works)
4. [Dependency analysis](#4-dependency-analysis)
5. [Target & artifact definition](#5-target--artifact-definition)
6. [Change inventory](#6-change-inventory)
7. [CI job design](#7-ci-job-design)
8. [Testing strategy](#8-testing-strategy)
9. [Phased execution plan](#9-phased-execution-plan)
10. [Decisions](#10-decisions)
11. [Risks & mitigations](#11-risks--mitigations)
12. [Out of scope / future work](#12-out-of-scope--future-work)
13. [Verification / definition of done](#13-verification--definition-of-done)
14. [References](#14-references)

---

## 1. Motivation & evidence

The shipped artifact set is Linux (x86_64 + aarch64, static musl), macOS
(aarch64 + x86_64), Windows (x86_64, currently unshipped), and Android/Termux
(aarch64). BSD is absent. FreeBSD is the natural next desktop/server target: a
first-class Rust host with a long-lived package ecosystem (FreeBSD ports,
pkgsrc), and the daemon's runtime dependencies (a Unix socket, a `redb` DB, a
POSIX shell) are all native to it.

### What was verified (source / primary-doc inspection, this change)

- **Rust tiers** (doc.rust-lang.org platform-support, last modified 2026-10-01):
  - `x86_64-unknown-freebsd` and `aarch64-unknown-freebsd` — **Tier 2 with host
    tools** (prebuilt `std` via rustup; rustc/cargo run natively).
  - `x86_64-unknown-netbsd` — **Tier 2 with host tools**.
  - `x86_64-unknown-openbsd`, `aarch64-unknown-openbsd`, `x86_64-unknown-dragonfly`
    — **Tier 3** (no prebuilt `std`; needs nightly `-Z build-std`).
- **Crypto provider on BSD** (aws-lc-rs User Guide → Platform Support):
  `x86_64-unknown-freebsd` (build ✓ / test ✓ / FIPS ✓) and
  `x86_64-unknown-netbsd` (build ✓ / test ✓) are supported non-FIPS with only a
  C/C++ compiler. **OpenBSD is not listed.**
- **The portability surface is small** and already correct:
  - `choreo-daemon/src/tools/shell_resolver.rs` **already has a `Platform::Bsd`
    variant** with a `freebsd|netbsd|openbsd|dragonfly` arm (lines ~262–277).
  - pidfd process termination is `#[cfg(target_os = "linux")]` in
    `choreo-daemon/src/tools/shell_util.rs`, with a
    `#[cfg(not(target_os = "linux"))]` PID-based `killpg` fallback already present
    (lines 176/202/243).
  - `choreo-sockreg/src/tuning.rs` has a
    `#[cfg(not(any(linux, android, macos)))]` branch that still sets
    `SO_KEEPALIVE`.
  - `choreo-power-events/src/platform/mod.rs` returns the inert monitor under
    `#[cfg(not(any(linux, macos, windows)))]`; `zbus` is gated to
    `cfg(target_os = "linux")` in its manifest, so it never compiles on BSD.
  - `choreo-tui/src/connection/mod.rs` uses the **self-pipe** signal trick
    (`signal-hook` + `mio`) deliberately instead of Linux `signalfd` — already
    POSIX-portable.
- **`prometheus`'s `process` feature adds no BSD dependency**: upstream declares
  `procfs` under `[target.'cfg(target_os = "linux")'.dependencies]`. So
  `--features metrics` **compiles on BSD**, losing only the RSS/CPU/FD process
  collector (application metrics are unaffected).

### What was **not** verified

No BSD `cargo check`, build, or run has been attempted. The per-crate `rustix`
/ `nix` API coverage, the C build scripts (`aws-lc-sys`, `onig_sys`,
`secp256k1-sys`, `zlob`), and the `notify` backend selection are reasoned
expectations from source/manifest inspection, not proven. The Phase 1 spike
exists to replace those expectations with evidence. See
[§9](#9-phased-execution-plan).

---

## 2. Target tiers & which BSD to ship first

| Triple | Tier | `std` via rustup | Verdict |
|---|---|---|---|
| `x86_64-unknown-freebsd` | 2 (host tools) | yes | **primary** |
| `aarch64-unknown-freebsd` | 2 (host tools) | yes | **primary (arm64)** |
| `x86_64-unknown-netbsd` | 2 (host tools) | yes | **easy follow-on** |
| `aarch64-unknown-netbsd` | 3 | no | deferred |
| `x86_64-unknown-openbsd`, `aarch64-unknown-openbsd` | 3 | no | deferred (Tier 3 + crypto provider) |
| `x86_64-unknown-dragonfly` | 3 | no | deferred |

Because FreeBSD/NetBSD are Tier 2, **the release pipeline does not need a
nightly `build-std` path** (unlike
[`mips32-linux-target.md`](./mips32-linux-target.md) and
[`risc-v-linux-target.md`](./risc-v-linux-target.md)) — `scripts/build-stable.sh`
+ `cargo-zigbuild` + `[profile.dist]` apply unchanged. OpenBSD/DragonFly would
each need the MIPS-style nightly `-Z build-std` treatment **and** OpenBSD would
need rustls switched from aws-lc-rs to the `ring` provider.

---

## 3. The portability surface (what already works)

Every platform-gated site, and its behavior on BSD:

| Area | Location | BSD behavior | Status |
|---|---|---|---|
| Process-tree kill | `choreo-daemon/src/tools/shell_util.rs` | non-Linux PID `killpg` fallback | ✅ already present |
| Shell discovery | `choreo-daemon/src/tools/shell_resolver.rs` | `Platform::Bsd` arm | ✅ already present |
| TCP keepalive | `choreo-sockreg/src/tuning.rs` | generic-Unix `SO_KEEPALIVE` only | ✅ compiles; timings optional |
| Power/suspend events | `choreo-power-events/src/platform/mod.rs` | inert monitor (logs once) | ✅ already present |
| Signals / terminal | `choreo-tui/src/connection/mod.rs` | self-pipe (`signal-hook`+`mio`) | ✅ already present |
| Log-file open (`O_NOFOLLOW`, euid) | `choreo-shared/src/logging.rs` | `rustix` (Unix-general) | ✅ expected |
| Config watching | `choreo-daemon/src/config_watch.rs` | `notify` kqueue backend | ✅ expected |
| Unix socket / server | `choreo-daemon/src/server/*`, `choreo-proto/src/io.rs` | `std::os::unix` | ✅ |
| RISC-V VM guest compile | `choreo-daemon/src/tools/vm.rs` | host-independent (`rustc +stable --target riscv64imac-unknown-none-elf`) | ✅ |
| OS sandbox | — | not implemented yet (Landlock/Seatbelt "coming soon") | n/a |

Two nice-to-haves that are **not** required for a first cut:

- **Real keepalive timings on BSD** — FreeBSD exposes `TCP_KEEPIDLE` /
  `TCP_KEEPINTVL` / `TCP_KEEPCNT` like Linux, so `tuning.rs` could gain a
  FreeBSD branch mirroring the Linux one (nix gates the specific sockopts, so this
  needs a nix-feature check). Falls back correctly today; improves dead-peer
  detection.
- **A FreeBSD power monitor** (`devctl(4)` / ACPI suspend) — nice, not required.

---

## 4. Dependency analysis

Cargo.lock holds the *whole optional* graph, so this is scoped to the **shipped
daemon + TUI** build (default `pdf`+`mcp`, plus `metrics`+`blockchain`, which is
what `scripts/release.sh` selects).

| Dependency | Role | BSD outlook |
|---|---|---|
| `aws-lc-sys` (rustls default provider, via `ureq`/`reqwest`) | TLS | **freebsd ✓, netbsd ✓** (aws-lc-rs table); **openbsd ✗ → use `ring`** |
| `ring` 0.17 | TLS (alt provider) | portable C; freebsd/netbsd ✓ |
| `onig_sys` | `syntect` (TUI) | portable C; expected ✓ — **confirm** |
| `secp256k1-sys` | `blockchain`/`content` | portable C; expected ✓ — **confirm** |
| `openssl-sys` / `native-tls` | optional reqwest/hyper-tls | **not in the shipping set** (reqwest built `default-features=false`+`rustls`); **confirm with `cargo tree`** |
| `procfs` | prometheus `process` | Linux-gated upstream → not built on BSD; process metrics simply absent |
| `zlob` | daemon `find`/walker | builds Zig C in `build.rs`; needs a Zig toolchain (same as Linux/macOS). BSD triples are Zig-known — **no `zlob` blocker** |
| `mimalloc` | musl-tarball allocator | irrelevant on BSD (system allocator); do **not** enable |
| `zbus` | power-events logind | `cfg(target_os="linux")`-gated → never built on BSD |
| `windows-sys`, `uds_windows` | Windows FFI | `cfg(windows)`-gated |
| `pdf-inspector` (default `pdf`) | PDF tools | pure Rust (`lopdf`); the C-dylib link is Apple-only; expected ✓ |
| `avif` (opt-in) | AVIF decode | needs a cross `libdav1d`; stays **off** |

Pure-Rust and expected-unproblematic: `nix`, `rustix` (both support the BSDs; the
calls used — `poll`, `getpgid`, `kill_process_group`, `fcntl_getfl/setfl`,
`geteuid` — are Unix-general), `redb`, `gix`, `image`, `resvg`, `heif-oxide`,
`ckb-vm`, `subxt`/`jsonrpsee`, `tungstenite`, `crossterm`, `ratatui`,
`signal-hook`, `mio`, `snow`, `x25519-dalek`.

**Net:** no expected hard blocker on FreeBSD/NetBSD; two dependency
confirmations (`onig_sys`, `secp256k1-sys`) and one tree-audit
(`native-tls`/`openssl-sys` absence) are the open items.

---

## 5. Target & artifact definition

| Property | Value |
|---|---|
| Rust target (primary) | `x86_64-unknown-freebsd` |
| Rust target (arm64) | `aarch64-unknown-freebsd` |
| Rust target (follow-on) | `x86_64-unknown-netbsd` |
| Tarball | `dist/choreographr-<version>-x86_64-unknown-freebsd.tar.gz` |
| Contents | `choreographr`, `choreo-tui`, `choreographr.rc` (top level, exec bits preserved) |
| Features | default `pdf`+`mcp` + `metrics` + `blockchain` — **no** `mimalloc`, **no** `avif` |
| Linking | native (base-system clang/ld; **no** musl, **no** static-pie) |
| CPU floor | generic x86-64 (no `target-cpu=native`, no v2/v3) |
| Toolchain | **stable** (`scripts/build-stable.sh`), no `build-std` |
| `uname -s` | `FreeBSD` → `release.sh`/`install.sh` case `FreeBSD-x86_64`/`FreeBSD-arm64` |

---

## 6. Change inventory

### Build orchestration

- **`scripts/release.sh`** — the host-target `case` (lines ~93–102) hard-fails
  every non-Linux/macOS host with "unsupported platform". Add `FreeBSD-x86_64`
  → `x86_64-unknown-freebsd` and `FreeBSD-arm64` → `aarch64-unknown-freebsd`,
  plus a tarball-build branch mirroring the Darwin branch (native build, **no**
  musl, **no** mimalloc):
  ```sh
  RUSTFLAGS="-C target-cpu=x86-64" \
    ./scripts/build-stable.sh build --locked --profile dist \
      -p choreographr -p choreo-tui \
      --features choreographr/metrics,choreographr/blockchain
  ```
  Leave `.deb`/`.rpm` gated off on BSD (`PKG_ARCH` stays empty); BSD packaging is
  a tarball + `rc.d` unit.
- **`scripts/install.sh`** — add a `FreeBSD-*)` case selecting the BSD tarball
  and installing the `rc.d` service; update the "ships … only" error text.
- **`justfile`** — extend the host `TARGET` case (line ~530) with the FreeBSD
  triples so `just release`/`smoke-test` resolve on a BSD host.
- **`Cargo.toml`** (optional) — a binstall block is **not required** for BSD
  (there is no prebuilt glibc/musl host to remap; `cargo install choreographr`
  builds from source). Add an override only if a prebuilt BSD tarball is
  published and binstall should fetch it.

### Packaging

- **`packaging/freebsd/choreographr.rc`** — a `rc.d` script (FreeBSD `rcorder`
  conventions: `PROVIDE`, `REQUIRE: NETWORKING`, `command=…`, `run as user`),
  analogous to `packaging/choreographr.service` (systemd) and
  `com.choreographr.daemon.plist` (launchd). NetBSD/OpenBSD `rc` variants are
  follow-ons.
- **`packaging/README.md`** — document the BSD unit and install path.

### Tooling & docs

- **`justfile`** — a `check-freebsd` cross gate reusing the existing
  `cross_config`/`cross_rustflags` idiom (clears the host `-C target-cpu=native`
  that `[profile.dev]` pins), routed through `cargo-zigbuild` exactly like
  `check-macos`/`check-windows`:
  ```make
  check-freebsd: _require-zig
      rustup target add x86_64-unknown-freebsd
      RUSTFLAGS="{{ cross_rustflags }}" cargo-zigbuild check {{ CARGO_FLAGS }} {{ cross_config }} --target x86_64-unknown-freebsd --workspace --lib
  ```
  Add it to `check-cross` so `pre-release` compiles BSD-gated code locally.
- **Docs** — `README.md` (supported-platforms list + FreeBSD install section),
  `packaging/README.md`, `ARCHITECTURE.md` (shipped-target matrix at ~line 220 +
  the platform-cfg notes), `RELEASE.md` (the BSD tarball phase).

---

## 7. CI job design

GitHub provides **no hosted BSD runner**, so the job runs the build inside a
BSD VM action. The two established actions are
[`vmactions/freebsd-vm`](https://github.com/vmactions/freebsd-vm) and
[`cross-platform-actions/action`](https://github.com/cross-platform-actions/action)
(FreeBSD/NetBSD/OpenBSD).

New `freebsd-x86_64` job in `.github/workflows/release.yml`, modeled on the
Linux jobs but self-contained:

1. Run on `ubuntu-latest`; step into the FreeBSD VM action, `sync` the checkout.
2. Inside the VM: `pkg install -y rust zig` (or rustup), then
   `scripts/release.sh` for `x86_64-unknown-freebsd` — **native**, stable
   toolchain, `--profile dist`, features `metrics,blockchain`.
3. `scripts/smoke-test.sh` and `scripts/daemon-smoke.sh` on the tarball **inside
   the VM** (both honour redirected state paths, so they run hermetically).
4. `actions/upload-artifact dist/choreographr-*`; then add `freebsd-x86_64` to
   the `release` job's `needs:` list (it checksums `choreographr-*` generically).

Given the Tier-2 status and that no native build has been run yet, the job should
be **allowed to fail non-blocking** until Phase 2 proves it, then promoted.
[§10 D5](#10-decisions)

---

## 8. Testing strategy

| Layer | Command |
|---|---|
| Compile gate | `just check-freebsd` (`cargo-zigbuild check --target x86_64-unknown-freebsd --workspace --lib`) |
| Full artifact | `--profile dist` tarball via `release.sh` on a FreeBSD host/VM |
| Clap surface | `scripts/smoke-test.sh` on the tarball |
| Daemon boot | `scripts/daemon-smoke.sh` on the tarball |
| Dependency confirm | `cargo tree -e features` on the target → no `native-tls`/`openssl-sys` in the shipping set |
| Unit/integration suite | `just test-all` unchanged (host-target only; BSD is exercised via CI) |

The nextest suite is unchanged; no `-p` cross run is part of the commit gate. A
native FreeBSD VM run is the authoritative check (§9 Phase 2).

---

## 9. Phased execution plan

Each phase is one commit; overlapping-file phases run as **serial subsessions**
(per `AGENTS.md`).

- **Phase 1 — compile spike (local, throwaway).** Add `just check-freebsd` and
  iterate `cargo-zigbuild check --target x86_64-unknown-freebsd --workspace --lib`
  until green. This is the single cheap step that turns "expected to work" into
  "known to compile" and surfaces the `onig_sys`/`secp256k1-sys`/`native-tls`
  questions. Also run `cargo tree` to confirm the TLS-provider audit. *No commit
  unless it works; this de-risks the plan.*
- **Phase 2 — native build + smoke (FreeBSD VM).** On a FreeBSD host/VM: full
  `cargo build --profile dist` for the shipping feature set, then
  `smoke-test.sh` + `daemon-smoke.sh`. Catches link-time and runtime issues a
  type-check cannot; confirms aws-lc-sys/oniguruma compile with base clang.
- **Phase 3 — build + installer.** `release.sh` FreeBSD branches;
  `install.sh` case; `justfile` host `TARGET`; the `packaging/freebsd` `rc.d`
  unit.
- **Phase 4 — CI.** `freebsd-x86_64` job via a VM action, initially
  non-blocking; wire into the `release` `needs`.
- **Phase 5 — docs + NetBSD.** README / packaging/README / ARCHITECTURE /
  RELEASE; then `x86_64-unknown-netbsd` (expected to be a near-copy); mark this
  plan done.

Dependencies: 1 → 2 → 3 → 4 → 5. (Phase 3 can overlap Phase 2's fixes but is
committed after it.)

---

## 10. Decisions

**Locked:**

- **D1 — FreeBSD first.** It is a Tier-2 host target with the largest BSD
  ecosystem; NetBSD follows at near-zero marginal cost.
- **D2 — Tier-3 BSDs (OpenBSD, DragonFly) are deferred.** They need a nightly
  `build-std` path, and OpenBSD additionally needs rustls → `ring` (aws-lc-rs
  does not list it).
- **D3 — Ship a tarball + `rc.d` unit; no `.deb`/`.rpm`.** Those are Linux-only
  artifacts.
- **D4 — No `mimalloc`, no `avif` on BSD.** `mimalloc` is a musl-tarball
  concern; `avif` needs a cross `libdav1d`.

**To confirm before/while implementing:**

- **D5 — CI job blocking?** *Recommendation: non-blocking until Phase 2 proves a
  native build, then promote.*
- **D6 — `blockchain` feature on BSD?** aws-lc-rs/`secp256k1-sys` are expected to
  build, so keep it; *fall back to `metrics`-only if a C dep fails to compile.*
- **D7 — binstall override?** *Recommendation: skip — no prebuilt-host remap is
  needed; `cargo install` builds from source.*
- **D8 — Keepalive timings on BSD?** *Recommendation: optional follow-on; the
  generic `SO_KEEPALIVE` path is correct today.*
- **D9 — Ship both arches in v1?** *Recommendation: `x86_64` first; add
  `aarch64-unknown-freebsd` once the x86_64 job is green.*

---

## 11. Risks & mitigations

- **Unproven compile.** No BSD build has run. Mitigation: Phase 1 spike before
  anything is committed to the release pipeline.
- **CI is the largest new cost.** No hosted BSD runner. Mitigation: a VM action
  (`vmactions/freebsd-vm` / `cross-platform-actions`), initially non-blocking.
- **C deps under base clang.** `aws-lc-sys` is verified for freebsd/netbsd;
  `onig_sys` and `secp256k1-sys` are expected but unproven. Mitigation: Phase 2
  native build; drop `blockchain` (D6) if `secp256k1-sys` fails.
- **A stray `native-tls` path.** Would add an OpenSSL link dependency.
  Mitigation: `cargo tree` audit in Phase 1.
- **`notify` backend differences.** BSD uses kqueue (or the poll fallback) rather
  than inotify; `config_watch.rs` already handles non-inotify overflow reporting,
  but watch semantics differ subtly. Mitigation: exercise the config-watch
  integration tests on the VM.
- **Metrics surface shrinks.** `prometheus`'s process collector is Linux-gated;
  RSS/CPU/FD gauges are absent on BSD. Acceptable; document it.
- **OpenBSD crypto provider.** Would need `rustls`/`ring`, not aws-lc-rs — a
  dependency-graph change, hence deferred (D2).

---

## 12. Out of scope / future work

- **OpenBSD / DragonFly** — Tier 3; nightly `build-std` + (OpenBSD) a rustls
  provider switch.
- **`choreo-gui` (Dioxus/Blitz/wgpu)** — already excluded from every release
  job; would need Vulkan/mesa; not shipped.
- **FreeBSD power/suspend monitor** — `devctl(4)`/ACPI; inert fallback is fine
  for v1.
- **BSD `.deb`/`.rpm`** — N/A.
- **pkgsrc / FreeBSD ports metadata** — a source package, if packaged installs
  are wanted beyond the tarball.

---

## 13. Verification / definition of done

- `just check-freebsd` green with **no** env workarounds.
- A **native** FreeBSD build (`--profile dist`, features
  `pdf,mcp,metrics,blockchain`) produces
  `dist/choreographr-<v>-x86_64-unknown-freebsd.tar.gz`, and both
  `smoke-test.sh` and `daemon-smoke.sh` pass on the tarball.
- `cargo tree` confirms no `native-tls`/`openssl-sys` in the shipping set.
- The `freebsd-x86_64` CI job builds + smokes the tarball in a VM and attaches it
  to a GitHub release via the `release` job.
- `install.sh` and the `rc.d` unit install and run the daemon on FreeBSD.
- Docs updated; `just pre-commit` green; commits written as release notes
  (`docs/plans/*` precedent is a plain `docs:` commit, no CHANGELOG entry).

---

## 14. References

- Rust platform support — <https://doc.rust-lang.org/rustc/platform-support.html>
  (FreeBSD/NetBSD Tier 2 with host tools; OpenBSD/DragonFly Tier 3).
- aws-lc-rs platform support —
  <https://aws.github.io/aws-lc-rs/platform_support.html> (BSD table).
- `prometheus` manifest (Linux-gated `procfs`) — `tikv/rust-prometheus` `v0.14.0`.
- Source sites cited in [§3](#3-the-portability-surface-what-already-works):
  `choreo-daemon/src/tools/shell_resolver.rs`,
  `choreo-daemon/src/tools/shell_util.rs`,
  `choreo-sockreg/src/tuning.rs`, `choreo-power-events/src/platform/mod.rs`,
  `choreo-tui/src/connection/mod.rs`, `choreo-shared/src/logging.rs`.
- Build/release sites: `scripts/release.sh` (host `case`), `scripts/install.sh`,
  `justfile` (`TARGET`, `check-cross`, `cross_config`),
  `.github/workflows/release.yml`.
- Sibling plans: [`docs/plans/risc-v-linux-target.md`](./risc-v-linux-target.md),
  [`docs/plans/mips32-linux-target.md`](./mips32-linux-target.md).
