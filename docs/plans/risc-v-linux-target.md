# Plan: RISC-V Linux release target

**Status:** blocked — *waiting on upstream `zlob`*. The build is blocked by two
bugs in the `zlob` crate (via `choreo-daemon`); both are fixed by
[dmtrKovalenko/zlob#32](https://github.com/dmtrKovalenko/zlob/pull/32), which
must merge **and** ship in a crates.io release before this work can start.
**Date:** 2026-09-19
**Target:** `riscv64gc-unknown-linux-musl` (fully static), shipped as a fourth
release tarball alongside the current three.
**Touches:** `scripts/release.sh`, `scripts/install.sh`, `Cargo.toml`
(binstall metadata), `packaging/aur/PKGBUILD`, `.github/workflows/release.yml`,
`justfile`, plus docs (`README.md`, `packaging/README.md`, `ARCHITECTURE.md`,
`RELEASE.md`) and `CHANGELOG.md`. Optional: `scripts/build-deb.sh`,
`scripts/build-rpm.sh`, `packaging/rpm/choreographr.spec`.

> **TL;DR.** The whole workspace already cross-compiles to and **runs** on
> RISC-V Linux — I verified both the static-musl daemon/TUI under QEMU and the
> full release feature set (`metrics`, `blockchain`). The *only* thing standing
> in the way is the `zlob` dependency, which fails to build for RISC-V for two
> independent reasons (a bad libclang triple and a missing Zig target mapping).
> We filed a one-file fix upstream (#32). Because `deny.toml` allows
> **crates.io sources only**, we cannot paper over it with a git/\[patch\]
> override — so this plan is *blocked* until that fix is released. Everything
> else is mechanical and enumerated below.

---

## Table of contents

1. [Motivation & evidence](#1-motivation--evidence)
2. [The `zlob` blocker (why we must wait)](#2-the-zlob-blocker-why-we-must-wait)
3. [Target & artifact definition](#3-target--artifact-definition)
4. [Change inventory](#4-change-inventory)
5. [CI job design](#5-ci-job-design)
6. [Testing strategy](#6-testing-strategy)
7. [Phased execution plan](#7-phased-execution-plan)
8. [Decisions](#8-decisions)
9. [Risks & mitigations](#9-risks--mitigations)
10. [Out of scope / future work](#10-out-of-scope--future-work)
11. [Verification / definition of done](#11-verification--definition-of-done)
12. [References](#12-references)

---

## 1. Motivation & evidence

RISC-V Linux is a supported Rust Tier-2 host (`riscv64gc-unknown-linux-gnu`,
`riscv64gc-unknown-linux-musl`) and is the target of the
[`run_riscv`](../../choreo-daemon/src/tools/vm.rs) sandbox tool — the project
already has RISC-V *guests*; this plan adds a RISC-V *host*.

I validated feasibility end-to-end (2026-09-19) with `cargo-zigbuild` + zig
0.16.0 + rustc 1.98.1:

- `--target riscv64gc-unknown-linux-gnu` builds `choreographr` + `choreo-tui`;
  the binaries are genuine `UCB RISC-V` ELFs.
- `--target riscv64gc-unknown-linux-musl` builds a **static-pie** binary that
  **runs under `qemu-riscv64-static`**: `--version`, `--help`, `fingerprint`
  (Noise/`snow`/`ring` crypto), and a full daemon boot (redb open, 1→2 zstd
  migration, Noise IK keypair generation, socket bind).
- Release feature set `--features metrics,blockchain` (alloy/subxt/reqwest)
  builds; `content`, `mcp`, and `mimalloc` build. `avif` does **not** (needs a
  riscv64 `libdav1d` via cross pkg-config) — it is off by default and stays off.
- `ring` has no RISC-V assembly, so it uses its portable C path (correct, just
  unaccelerated). `secp256k1-sys`, `pdf-inspector`, `ckb-vm`, `structured-zstd`,
  `heif-oxide` all build. `choreo-gui` (dioxus/blitz/wgpu) is not shipped and
  is out of scope.

**Net:** no choreographr source changes are required. The blockers are
dependency-side.

---

## 2. The `zlob` blocker (why we must wait)

`zlob` is a Zig-implemented zlib pulled in by `choreo-daemon`; its `rust/build.rs`
has two bugs that break RISC-V cross-compilation:

1. **libclang gets the raw Rust triple.** `build.rs` passes
   `--target=riscv64gc-unknown-linux-gnu` to bindgen/clang. Clang's target
   parser rejects the Rust-only `gc` suffix (`unknown target triple`). (The
   project already carries a fix for the *same class* of bug on iOS simulator:
   `BINDGEN_EXTRA_CLANG_ARGS_aarch64_apple_ios_sim` in `release.yml` — see
   [§9](#9-risks--mitigations).)
2. **Missing Zig target mapping.** `rust_target_to_zig()` has
   `riscv64gc-unknown-linux-gnu => riscv64-linux-gnu` but **no musl entry**, so
   `riscv64gc-unknown-linux-musl` falls through to `_ => "native"` and Zig
   compiles the bundled C library for the **host** arch; the linker then fails
   with `c_lib.o is incompatible with elf64lriscv`.

Fix: [zlob#32](https://github.com/dmtrKovalenko/zlob/pull/32) — one file,
`rust/build.rs`; adds a `rust_target_to_clang()` helper and the missing musl
mapping. **Verified**: with the patch, both targets build cleanly and the
emitted objects are `UCB RISC-V`.

**Why we cannot work around it locally.** The workspace's supply-chain policy
(`deny.toml`) sets `[sources] unknown-git = "deny"` and `unknown-registry =
"deny"`, `allow-registry = [crates.io-index only]`. A
`[patch.crates-io] zlob = { git = … }` or vendored-path override would fail
`just check-supply-chain` (and weakens the *arrayref-attack* posture the policy
exists for). Also note the `BINDGEN_EXTRA_CLANG_ARGS` env workaround alone is
**insufficient** for musl — it fixes bug 1 but not bug 2, which has no env
override. Therefore:

> **Decision (locked): wait for upstream.** Land zlob#32, cut a `zlob` release
> containing it, then bump `zlob` in the workspace and proceed. No workaround
> ships in choreographr.

Interim option, only if RISC-V artifacts are needed *before* the next `zlob`
release: ship a **glibc** `riscv64gc-unknown-linux-gnu` tarball instead (its
Zig mapping already exists, so only the `BINDGEN_EXTRA_CLANG_ARGS` env var is
needed — the exact iOS pattern). This diverges from the "one static artifact"
policy and is **not** the recommended path; recorded for completeness. (A
`ZIG`-wrapper trick also exists but is explicitly rejected as too fragile to
ship.)

---

## 3. Target & artifact definition

| Property | Value |
|---|---|
| Rust target | `riscv64gc-unknown-linux-musl` |
| Tarball | `dist/choreographr-<version>-riscv64gc-unknown-linux-musl.tar.gz` |
| Contents | `choreographr`, `choreo-tui`, `choreographr.service`, `com.choreographr.daemon.plist` (top level, exec bits preserved — same as the x86_64 musl tarball) |
| Features | `choreographr/metrics,choreographr/blockchain,choreographr/mimalloc,choreo-tui/mimalloc` (identical to the x86_64 musl tarball) |
| Linking | static musl (`-static`, static-pie) |
| CPU floor | none — plain `riscv64gc` baseline (no `-C target-cpu`), analogous to the aarch64-apple case |
| `avif` | excluded (no riscv64 `libdav1d`); matches the default/release C-free policy |
| Bridges / gui | excluded (feature-gated / not shipped) |

`uname -m` on a riscv64 Linux host is `riscv64`, so `install.sh`'s
platform case keys on `Linux-riscv64`.

---

## 4. Change inventory

### Build orchestration

- **`scripts/release.sh`** — today the target is derived from `uname` and the
  musl build path is gated on the Linux package arch (x86_64/aarch64 share the
  `zigbuild` branch, keyed off `PKG_ARCH`). Add:
  - an explicit **target override** (env `RELEASE_TARGET` or `--target <triple>`)
    so the x86_64 CI runner can emit the riscv tarball (release.sh is otherwise
    host-keyed);
  - a `Linux-riscv64)` native case;
  - a riscv build branch mirroring the x86_64-musl one:
    ```sh
    ./scripts/build-stable.sh zigbuild --locked --profile dist \
      -p choreographr -p choreo-tui --target riscv64gc-unknown-linux-musl \
      --features choreographr/metrics,choreographr/blockchain,choreographr/mimalloc,choreo-tui/mimalloc
    ```
    (No `RUSTFLAGS=-C target-cpu=…`; no env workarounds once zlob#32 ships.)
- **`Cargo.toml`** — add `[package.metadata.binstall.overrides.riscv64gc-unknown-linux-gnu]`
  → the musl asset, mirroring the existing `x86_64-unknown-linux-gnu` override,
  so `cargo binstall` resolves on glibc riscv distros. The generic
  `pkg-url` template already handles `riscv64gc-unknown-linux-musl` directly.

### Installer & packaging

- **`scripts/install.sh`** — add `Linux-riscv64) ASSET=…riscv64gc-unknown-linux-musl.tar.gz`
  to the platform case; update the "ships … only" error text.
- **`packaging/aur/PKGBUILD`** — `arch=('x86_64' 'riscv64')` with a
  `CARCH`-conditional `source`/`sha256sums` (both arch use the musl tarball,
  different filenames); `.SRCINFO` regenerated. (The PKGBUILD already carries
  arch-conditional sources for x86_64/aarch64 — extend that pattern.)
- **`.deb`/`.rpm`** — `scripts/build-deb.sh` / `build-rpm.sh` now detect the
  host arch (`uname -m`) and tag the package from it (x86_64/aarch64 today);
  both build from the **static** musl binaries' sibling glibc build, so adding
  riscv64 is an arch-string case plus a glibc-host build (gated behind a
  decision, [§8](#8-decisions)).

### Tooling & docs

- **`justfile`** — a `cross-check-riscv` gate
  (`cargo-zigbuild check --target riscv64gc-unknown-linux-musl --workspace --lib`),
  mirroring the darwin/windows cross-compile gates. Extend the `TARGET` case if
  useful for `just smoke-test`.
- **Docs** — `README.md` (supported platforms), `packaging/README.md`
  (release-tarball section), `ARCHITECTURE.md` (shipped-target matrix),
  `RELEASE.md` (phases), and a `CHANGELOG.md` `[Unreleased]` entry.

---

## 5. CI job design

New `linux-riscv64` job in `.github/workflows/release.yml`, modeled on the
existing `linux-x86_64` job (cross-builds on `ubuntu-latest`; no native runner
needed):

1. `ubuntu-latest`; install stable toolchain +
   `rustup target add --toolchain stable riscv64gc-unknown-linux-musl`.
2. `mlugg/setup-zig@v2` + `taiki-e/install-action` (cargo-zigbuild), same as
   `linux-x86_64`.
3. Build via `release.sh` with `RELEASE_TARGET=riscv64gc-unknown-linux-musl`
   (or inline like the windows job).
4. `sudo apt-get install -y qemu-user-static` — this registers riscv64 binfmt on
   the runner, so the riscv tarball's binaries run **transparently**; then run
   `scripts/smoke-test.sh` and `scripts/daemon-smoke.sh` unchanged.
   (`daemon-smoke.sh` already honours `CHOREOGRAPHR_SOCKET_PATH` /
   `CHOREOGRAPHR_DB_PATH`, so it runs hermetically under emulation — I confirmed
   a full daemon boot under qemu.)
5. `actions/upload-artifact` `dist/choreographr-*` (name e.g. `linux-riscv64`).
6. Add `linux-riscv64` to the `release` job's `needs:` list. Nothing else in
   `release` changes — it already does `sha256sum choreographr-*` generically.

The `homebrew-verify` workflow is unaffected (macOS-only).

---

## 6. Testing strategy

| Layer | Command |
|---|---|
| Compile gate | `cargo-zigbuild check --target riscv64gc-unknown-linux-musl --workspace --lib` (new `just cross-check-riscv`) |
| Full artifact | `--profile dist` tarball via `release.sh` |
| Clap surface | `scripts/smoke-test.sh` on the tarball |
| Daemon boot | `scripts/daemon-smoke.sh` on the tarball |
| Emulation | both of the above under `qemu-user-static` (binfmt) in CI |

The nextest suite is unchanged; no `-p` cross run is part of the commit gate.
Local validation of the *blocker* is already done (gnu + musl builds, QEMU
run); post-fix CI just re-runs it.

---

## 7. Phased execution plan

Each phase is one commit; overlapping-file phases run as **serial subsessions**
(per `AGENTS.md`).

- **Phase 0 — upstream (external).** Merge [zlob#32], cut a `zlob` release.
- **Phase 1 — dependency bump.** Bump `zlob` in `Cargo.toml`/`Cargo.lock`; verify
  local riscv musl build is green with **no** env workarounds.
- **Phase 2 — build + installer.** `release.sh` target override + riscv branch;
  `install.sh` case; binstall override.
- **Phase 3 — CI.** `linux-riscv64` job + QEMU smoke; wire into `release` `needs`.
- **Phase 4 — optional packaging.** riscv `.deb`/`.rpm` + AUR `riscv64`.
- **Phase 5 — docs + CHANGELOG.** README / packaging/README / ARCHITECTURE /
  RELEASE; mark this plan done.

Dependencies: 0 → 1 → (2) → (3) → (4) → 5.

---

## 8. Decisions

**Locked:**

- **D1 — Wait for upstream `zlob`.** No git/path patch (`deny.toml`
  crates.io-only). Static musl tarball, not a glibc interim.

**To confirm before implementing:**

- **D2 — `.deb`/`.rpm` for riscv:** include (cheap, from the static binaries) or
  tarball-only for v1? *Recommendation: tarball-only first; add packages in
  Phase 4 if wanted.*
- **D3 — AUR:** add `riscv64` to `choreographr-bin`, or leave x86_64-only?
  *Recommendation: add it.*
- **D4 — CPU floor:** plain `riscv64gc` baseline (no `-C target-cpu`)?
  *Recommendation: yes.*
- **D5 — Features:** `metrics,blockchain,mimalloc`; `avif` off? *Recommendation:
  yes (matches x86_64).*
- **D6 — Binary set:** daemon + TUI only? *Recommendation: yes.*

---

## 9. Risks & mitigations

- **Upstream timing.** The whole plan gates on zlob#32 + a release. Mitigation:
  track the PR; the change is tiny and the maintainer merges community PRs.
- **Don't reintroduce the env hack.** The iOS job's
  `BINDGEN_EXTRA_CLANG_ARGS_aarch64_apple_ios_sim` is a precedent, but for riscv
  it is *only* a stopgap for bug 1 and does nothing for bug 2. Once zlob is
  fixed, the CI job and `release.sh` must carry **no** `BINDGEN_EXTRA_CLANG_ARGS`
  / `ZIG` wrapper.
- **`ring` is unaccelerated on riscv** (no asm) — a performance, not a
  correctness, caveat; note it in docs.
- **`avif` unavailable** on riscv (no `libdav1d`); keep it off, document it.
- **Emulated smoke ≠ native.** QEMU-user covers startup and clap/crypto paths
  but not syscall/perf corners; on-device/native runs remain the final check.
- **musl static + mimalloc** on riscv is validated by the build/link; a real
  daemon boot under QEMU already passed.

---

## 10. Out of scope / future work

- `choreo-gui` (dioxus/blitz/wgpu) on riscv — heavy C/GTK stack, not shipped.
- `avif` decode on riscv (drag in a `libdav1d` riscv build) — opt-in only.
- A riscv **source** package (AUR `makedepends=(zig)`) vs the prebuilt `-bin`.
- A riscv64 **native** CI runner (cross + QEMU is sufficient).
- A riscv CPU-level floor (e.g. `+v` / `Zba`) if a homogeneous fleet ever
  justifies one.

---

## 11. Verification / definition of done

- `zlob` bumped past the fixing release; riscv musl build green **without** env
  workarounds.
- `dist/choreographr-<v>-riscv64gc-unknown-linux-musl.tar.gz` produced,
  checksummed, and attached to a GitHub release by the `release` job.
- `smoke-test.sh` **and** `daemon-smoke.sh` pass on the tarball under
  `qemu-user-static` in CI.
- `cargo binstall` resolves on a glibc riscv host (override present).
- Docs updated; `just pre-commit` green; `CHANGELOG.md` `[Unreleased]` entry.

---

## 12. References

- Upstream fix: [dmtrKovalenko/zlob#32](https://github.com/dmtrKovalenko/zlob/pull/32)
  — `fix: support riscv64 gnu/musl targets`.
- `zlob` `rust/build.rs` (`rust_target_to_clang`, `rust_target_to_zig`).
- `deny.toml` — `[sources]` crates.io-only policy.
- `.github/workflows/release.yml` — `linux-x86_64`, `android-termux` (qemu-user),
  `ios-build` (`BINDGEN_EXTRA_CLANG_ARGS_*`) jobs.
- `run_riscv` guest tool: `choreo-daemon/src/tools/vm.rs`.
