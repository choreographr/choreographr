# Plan: MIPS32 Linux release target

**Status:** blocked — *waiting on upstream `zlob`*, and **conditional on the board
having enough RAM**. The build is gated by a missing MIPS entry in `zlob`'s Zig
target map (the same one-file area as
[dmtrKovalenko/zlob#32](https://github.com/dmtrKovalenko/zlob/pull/32)); nothing
can be built until that lands **and** ships in a crates.io release. Separately,
the daemon's measured idle footprint (~36 MB RSS) rules out the 16 MB boards
that motivated this investigation — the target only makes sense on a ≥128 MB
MIPS32 board.
**Date:** 2026-10-04
**Target:** `mipsel-unknown-linux-musl` (little-endian; the common case —
MediaTek/Ingenic) shipped as a static tarball. Big-endian
`mips-unknown-linux-musl` is the same work with a different triple (see
[§4](#4-target--artifact-definition)).
**Touches:** `scripts/release.sh`, `scripts/build-stable.sh` (new nightly
`build-std` path), `scripts/install.sh`, `Cargo.toml` (binstall metadata),
`packaging/aur/PKGBUILD`, `.github/workflows/release.yml`, `justfile`, plus docs
(`README.md`, `packaging/README.md`, `ARCHITECTURE.md`, `RELEASE.md`).

> **TL;DR.** Unlike RISC-V, MIPS32 Linux is **not** a drop-in Rust target:
> `mips`/`mipsel-unknown-linux-{musl,gnu}` are **Tier 3** (no prebuilt `std`, so
> the build needs nightly `-Z build-std`), and `zlob` never mapped MIPS at all
> (its map falls through to `"native"`, producing host-arch objects → link
> failure). The good news, from dependency-source inspection: the crypto stack
> needs no port — `snow` is pure-Rust and `ring` has an unconditional portable C
> path (`aes_nohw` + portable curve25519), so the two heaviest C deps are a
> non-issue. The buildable footprint is narrowed by **dropping two features**
> (`mimalloc`, `blockchain`), whose C deps (`mimalloc`, `aws-lc-sys`) have no
> MIPS support. So MIPS32 is "hard but tractable, same shape as RISC-V, gated on
> one upstream `zlob` mapping" **provided the board clears the RAM floor**. No
> 32-bit build of this workspace has been attempted yet — this plan is a
> dependency-and-toolchain analysis, not a proven build.

---

## Table of contents

1. [Motivation & evidence](#1-motivation--evidence)
2. [RAM floor & board selection](#2-ram-floor--board-selection)
3. [The build blockers](#3-the-build-blockers)
4. [Target & artifact definition](#4-target--artifact-definition)
5. [Change inventory](#5-change-inventory)
6. [CI job design](#6-ci-job-design)
7. [Testing strategy](#7-testing-strategy)
8. [Phased execution plan](#8-phased-execution-plan)
9. [Decisions](#9-decisions)
10. [Risks & mitigations](#10-risks--mitigations)
11. [Out of scope / future work](#11-out-of-scope--future-work)
12. [Verification / definition of done](#12-verification--definition-of-done)
13. [References](#13-references)

---

## 1. Motivation & evidence

MIPS32 is the architecture of a large installed base of consumer routers,
appliances, and a smaller set of dev boards (MediaTek MT7620/MT7621/MT7628,
Qualcomm Atheros AR71xx/AR9xxx, Ingenic XBurst, Loongson). It is **not** a
project target today, and it is materially harder than RISC-V. This plan records
what was checked and what would have to change.

### What was actually verified

- **Runtime footprint (measured, x86_64/glibc).** The release daemon, started
  idle with a scratch `HOME`/DB, reports `VmRSS ≈ 36 932 kB` (~36 MB) across 15
  threads. This is the number that disqualifies 16 MB boards and sets the RAM
  floor in [§2](#2-ram-floor--board-selection). The musl/mimalloc build would be
  smaller, but the same order.
- **Crypto stack needs no port** (source inspection):
  - `snow` (the Noise transport) uses its own **pure-Rust**
    `default-resolver-crypto` (`aes-gcm`, `chacha20poly1305`, `blake2`, `sha2`,
    `curve25519`, `getrandom`) — **no `ring`**.
  - `ring` (pulled only by `ureq` → `rustls`) **always compiles** its portable C
    core — `crypto/curve25519/curve25519.c`, `crypto/fipsmodule/aes/aes_nohw.c`,
    `montgomery.c`, … are `&[]` (arch-agnostic); only the *optimized asm* is
    arch-gated. So `ring` builds for any target via `aes_nohw`, unaccelerated.
    (This is the same path RISC-V used.)

### What was **not** verified

No MIPS build has been attempted — no compiler target, no `-Z build-std`, no C
cross-configure. The Tier-3 and dependency conclusions below are from source
inspection and the Rust platform-support tier data, and must be confirmed by a
spike ([§8](#8-phased-execution-plan)).

---

## 2. RAM floor & board selection

The 16 MB class is out; the daemon does not fit. Practical guidance:

| Board class | RAM | Verdict |
|---|---|---|
| 16 MB (e.g. low-end AR71xx/MT7628) | 16 MB | ✗ daemon OOMs on startup (~36 MB idle) |
| Small MT7620/MT7628 routers | 32–64 MB | ~ tight; flash likely also a limit |
| **MT7621 routers** (1004Kc, mipsel) | **128–512 MB** | ✓ RAM fine |
| **Ingenic JZ4780 / Creator Ci20** (mipsel) | **1 GB** | ✓ RAM trivial |
| Loongson 1B/1C/2K boards | 128–512 MB | ✓ RAM fine |

Notes:

- **Endianness must match the SoC**: MediaTek/Ingenic are little-endian
  (`mipsel`); Atheros AR71xx/ath79 are big-endian (`mips`).
- **CPU level & float ABI must match the SoC** (mips32r2; many cores have no FPU
  → the soft-float ABI).
- **Flash/rootfs**: the shipped static tarball is ~29 MB compressed and the
  stripped binaries land in the ~15–25 MB range — prefer ≥64–128 MB flash **or an
  SD/USB rootfs**, plus writable space for the redb DB.
- **No-MMU** MIPS32 parts (µClinux-only) and PIC32 microcontrollers cannot run
  Linux and are out of scope.

The daemon's memory model still matters on smaller boards: all session turns are
held in RAM and the whole session state is cloned per active request, and
`display_image`/PDF tool calls can spike. On a 128 MB board, keep concurrent
sessions modest.

---

## 3. The build blockers

### 3.1 Rust MIPS Linux is Tier 3 (build std from source)

All 32-bit MIPS Linux targets (`mips`/`mipsel-unknown-linux-{musl,gnu}`) are
**Tier 3** — the Rust project publishes **no** builds and **no prebuilt `std`**.
Consequences:

- `rustup target add` cannot provide `std`; the build must use
  `+nightly -Z build-std=std,panic_abort` with the `rust-src` component.
- The project's **release pipeline is stable** (`scripts/build-stable.sh` +
  `cargo-zigbuild` + `[profile.dist]`). MIPS therefore needs a **new nightly
  `build-std` path** — `build-stable.sh` exists precisely to run stable cargo, so
  it is the wrong tool here.
- Tier 3 means unvalidated upstream: expect to be the first to exercise the
  target-specific std and the C crates' configure steps.
- `cargo-zigbuild` + `-Z build-std` composition is **unverified**; if it does not
  compose, fall back to a mips-musl cross toolchain (buildroot / musl.cc) as the
  linker instead of zig.

Also note the documented MIPS non-conformance: the bit pattern for signaling NaNs
is inverted vs. what Rust expects
([rust-lang/rust#68925](https://github.com/rust-lang/rust/issues/68925)). Low
risk here (the daemon does little FP), but recorded.

### 3.2 `zlob` (hard dependency — the gating blocker)

`zlob` is pulled in unconditionally by `choreo-daemon` (the `find`/walker tool).
Its `rust_target_to_zig()` has **no MIPS entry at all** — not even the `-gnu`
variant RISC-V has:

```rust
"riscv64gc-unknown-linux-gnu" => "riscv64-linux-gnu",
// … no mips-* entry …
_ => "native",
```

So every MIPS triple falls through to `"native"`, Zig compiles the bundled C for
the **host** arch, and the link fails on architecture mismatch. This is the same
*bug class* as RISC-V, except RISC-V at least had a `-gnu` mapping.

**Fix:** add the MIPS mappings to `rust_target_to_zig` (and, if clang rejects the
triple, the `rust_target_to_clang` normalizer from zlob#32). This is an
**upstream** change:

- Option A — extend/rebase onto
  [zlob#32](https://github.com/dmtrKovalenko/zlob/pull/32) while it is open.
- Option B — a follow-up PR after #32 merges.

**Why we cannot work around it locally.** `deny.toml` sets `[sources]
unknown-git = "deny"`, crates.io-only; a `[patch.crates-io] zlob = { git = … }`
or vendored-path override would fail `just check-supply-chain`. So MIPS is
blocked until an upstream `zlob` release carries the mapping.

Note on the bindgen triple (bug 1 of #32): the riscv failure was clang rejecting
the Rust-only **`gc`** arch suffix. MIPS arch strings (`mips`/`mipsel`) are
clang-known, so that particular bug **may not** apply — to be confirmed in the
spike; the missing Zig mapping (bug 2) definitely does.

> **Decision (locked): wait for upstream `zlob`.** Land the MIPS mappings, cut a
> `zlob` release, bump the workspace, then proceed. No local workaround ships.

### 3.3 C deps to drop (no MIPS support)

Both are **optional features** and are avoided for the MIPS build:

- **`mimalloc`** — mimalloc v3's arch detection covers only
  ARM64/X64/X86/ARM32/RISCV; there is **no MIPS branch and no MIPS code**
  anywhere. It exists solely to back the static-musl tarball, so build MIPS
  **without** `choreographr/mimalloc`: the shim crate degrades to the system
  allocator when the feature is off.
- **`blockchain` / `content`** — these pull `aws-lc-sys` (via
  alloy/subxt → reqwest → rustls's default provider). AWS-LC has *some* MIPS
  awareness (`__MIPSEL__`/`__MIPSEB__`; `target_chokes_on_u1()` handles the
  bindgen `u1` quirk) but is not an officially supported platform. Drop both
  features for MIPS; TLS still works through `ureq` → `ring` (portable C).

### 3.4 Non-blockers / soft dependencies

- **`ring`** — builds via portable C (`aes_nohw`); unaccelerated, not blocked.
- **`snow`** — pure-Rust crypto; not blocked.
- **`secp256k1-sys`** (keystore) — portable C fallback; builds (slow), only
  relevant if Polkadot accounts are used.
- **`zstd-sys`** (session compression) — portable C; expected to build.
- **power-events (`systemd-logind` over D-Bus)** — the daemon calls
  `PowerMonitor::best_effort()` (`server/core.rs`), which logs once and falls back
  to inert on a board with no systemd/D-Bus (OpenWrt). Suspension handling is
  lost; startup is not.
- **`avif`** — off by default (needs a cross `libdav1d`); stays off.

---

## 4. Target & artifact definition

| Property | Value |
|---|---|
| Rust target (primary) | `mipsel-unknown-linux-musl` (little-endian) |
| Rust target (alt) | `mips-unknown-linux-musl` (big-endian, e.g. Atheros) |
| Tarball | `dist/choreographr-<version>-mipsel-unknown-linux-musl.tar.gz` |
| Contents | `choreographr`, `choreo-tui`, `choreographr.service` (top level, exec bits preserved) |
| Features | `choreographr/metrics,choreo-tui/…` — **no** `mimalloc`, **no** `blockchain`/`content`, **no** `avif` |
| Linking | static musl (`-static`, static-pie) |
| CPU floor | `mips32r2`, soft-float unless the SoC has an FPU |
| Toolchain | **nightly** `-Z build-std=std,panic_abort` (+ `rust-src`) |
| `uname -m` | `mipsel` (or `mips`) → `install.sh` case `Linux-mipsel` |

---

## 5. Change inventory

### Build orchestration

- **`scripts/build-stable.sh`** — cannot build MIPS (stable can't build a Tier-3
  `std`). Add a sibling path (or a `build-nightly.sh`) that runs
  `cargo +nightly -Z build-std=std,panic_abort --target mipsel-unknown-linux-musl`
  with the `[profile.dist]` profile, `rust-src` installed, and the manifest's
  `-C target-cpu=native` rustflags stripped (as `build-stable.sh` already does).
- **`scripts/release.sh`** — add an explicit target override (env
  `RELEASE_TARGET`/`--target <triple>`), a `Linux-mipsel)` native case, and a
  MIPS build branch mirroring the x86_64-musl branch **minus** `mimalloc` and
  `blockchain`:
  ```sh
  RUSTFLAGS="-C target-feature=+… -C target-cpu=mips32r2" \
    ./scripts/build-nightly.sh zigbuild --locked --profile dist \
      -p choreographr -p choreo-tui --target mipsel-unknown-linux-musl \
      --features choreographr/metrics
  ```
- **`Cargo.toml`** — a binstall override mapping the glibc `mipsel`/`mips`
  triples to the musl tarball, mirroring the existing Linux overrides.

### Installer & packaging

- **`scripts/install.sh`** — add `Linux-mipsel)`/`Linux-mips)` cases → the
  musl tarball; update the "ships … only" error text.
- **`packaging/aur/PKGBUILD`** — optional `mipsel` arch entry (AUR host arch is
  usually x86_64; low value). [§9 D3](#9-decisions)
- **`.deb`/`.rpm`** — out of scope for v1 (the host build is native glibc; a
  MIPS `.deb`/`.rpm` would need a MIPS-host or cross glibc build). [§9 D2](#9-decisions)

### Tooling & docs

- **`justfile`** — a `cross-check-mipsel` gate
  (`cargo +nightly zigbuild check --target mipsel-unknown-linux-musl --workspace --lib`),
  mirroring the existing cross-compile gates.
- **Docs** — `README.md` (supported platforms + the RAM/board caveat),
  `packaging/README.md`, `ARCHITECTURE.md` (shipped-target matrix + a note that
  MIPS drops `mimalloc`/`blockchain`), `RELEASE.md` (the nightly `build-std`
  phase).

---

## 6. CI job design

New `linux-mipsel` job in `.github/workflows/release.yml`, modeled on
`linux-x86_64`, **but nightly**:

1. `ubuntu-latest`; install **nightly** + `rust-src`
   (`rustup toolchain install nightly --component rust-src`).
2. `mlugg/setup-zig@v2` + `taiki-e/install-action` (cargo-zigbuild), as
   `linux-x86_64`.
3. Build via `release.sh` with `RELEASE_TARGET=mipsel-unknown-linux-musl`
   (nightly `build-std` path; features `metrics` only).
4. `sudo apt-get install -y qemu-user-static` — registers mipsel binfmt so the
   tarball binaries run transparently; then `scripts/smoke-test.sh` and
   `scripts/daemon-smoke.sh` unchanged (the latter honours
   `CHOREOGRAPHR_SOCKET_PATH`/`CHOREOGRAPHR_DB_PATH`, so it runs hermetically
   under emulation).
5. `actions/upload-artifact` `dist/choreographr-*`.
6. Add `linux-mipsel` to the `release` job's `needs:` list (it already
   checksums `choreographr-*` generically).

Given the Tier-3 status and nightly churn, this job should be **allowed to fail
non-blocking** until the target is proven, then promoted. [§9 D5](#9-decisions)

> **CI caveat.** This target gates on an upstream `zlob` release, so the job
> cannot be green until that dependency bump lands. Wire the job in only after
> Phase 1.

---

## 7. Testing strategy

| Layer | Command |
|---|---|
| Compile gate | `cargo +nightly zigbuild check --target mipsel-unknown-linux-musl --workspace --lib` (new `just cross-check-mipsel`) |
| Full artifact | `--profile dist` tarball via `release.sh` (nightly `build-std`) |
| Clap surface | `scripts/smoke-test.sh` on the tarball |
| Daemon boot | `scripts/daemon-smoke.sh` on the tarball |
| Emulation | both of the above under `qemu-user-static` (binfmt) in CI |
| 32-bit sanity | confirm no `usize`/64-bit-atomic assumptions in the **reduced** feature set |

The nextest suite is unchanged; no `-p` cross run is part of the commit gate.
Emulated smoke covers startup and clap/crypto paths but not syscall/perf corners
— a native board run is the final check.

---

## 8. Phased execution plan

Each phase is one commit; overlapping-file phases run as **serial subsessions**
(per `AGENTS.md`).

- **Phase 0 — upstream (external).** Add the MIPS mappings to `zlob`
  (`rust_target_to_zig`; the `rust_target_to_clang` normalizer if clang rejects
  the triple) — either onto
  [zlob#32](https://github.com/dmtrKovalenko/zlob/pull/32) or as a follow-up.
  Merge + cut a release.
- **Phase 1 — spike (local, throwaway).** Bump `zlob`; attempt the nightly
  `build-std` interleaved with zigbuild (or a buildroot/musl.cc toolchain) for
  `mipsel-unknown-linux-musl` with `metrics` only. Answer: does it compile and
  link? Record the exact invocation. *No commit unless it works; this de-risks
  the whole plan.*
- **Phase 2 — build + installer.** `build-nightly.sh`; `release.sh` target
  override + mipsel branch; `install.sh` cases; binstall override.
- **Phase 3 — CI.** `linux-mipsel` job (nightly + qemu-user-static smoke),
  initially non-blocking; wire into `release` `needs`.
- **Phase 4 — optional packaging.** AUR `mipsel`; `.deb`/`.rpm` only if wanted.
- **Phase 5 — docs.** README / packaging/README / ARCHITECTURE / RELEASE; mark
  this plan done.

Dependencies: 0 → 1 (gate) → 2 → 3 → 4 → 5.

---

## 9. Decisions

**Locked:**

- **D1 — Wait for upstream `zlob`.** No git/path patch (`deny.toml`
  crates.io-only). The MIPS mappings are an upstream change.
- **D2 — RAM floor ≥128 MB (prefer ≥256 MB).** The 16 MB class is disqualified by
  the measured ~36 MB idle footprint.
- **D3 — Drop `mimalloc` and `blockchain`/`content` for MIPS.** Their C deps have
  no MIPS support; TLS stays via `ureq`→`ring`.
- **D4 — Little-endian `mipsel` is the primary target.** Big-endian `mips` is the
  same work with a different triple and a different board.

**To confirm before/while implementing:**

- **D5 — CI job blocking?** *Recommendation: non-blocking until the target is
  proven, then promote to blocking.*
- **D6 — `.deb`/`.rpm` for MIPS:** include or tarball-only for v1?
  *Recommendation: tarball-only; skip.*
- **D7 — AUR:** add `mipsel`, or leave x86_64/aarch64-only? *Recommendation:
  skip (AUR hosts are x86_64).*
- **D8 — CPU floor:** `mips32r2`, soft-float by default; hard-float variant only
  for FPU SoCs? *Recommendation: soft-float default; document the hard-float
  opt-in.*
- **D9 — Ship both endiannesses?** *Recommendation: `mipsel` only for v1; add
  `mips` if a specific big-endian board is targeted.*

---

## 10. Risks & mitigations

- **Upstream timing (zlob).** The whole plan gates on the MIPS mapping + a
  release. Mitigation: fold it into/alongside
  [zlob#32](https://github.com/dmtrKovalenko/zlob/pull/32); the change is tiny.
- **Tier 3 = no upstream validation.** Expect rough edges in the target's `std`
  and C crates' configure steps. Mitigation: Phase 1 spike before committing to
  the plan; keep the CI job non-blocking at first.
- **`build-std` × `cargo-zigbuild`.** Composition unverified. Mitigation:
  fall back to a mips-musl cross toolchain as the linker.
- **32-bit assumptions.** The workspace has never been built 32-bit; dropping
  `blockchain`/`content` removes the riskiest crates, but `usize`/atomic
  assumptions elsewhere are untested. Mitigation: the spike; keep the feature set
  lean.
- **`ring` unaccelerated** (no asm) — performance, not correctness; note in docs.
- **Emulated smoke ≠ native.** QEMU-user covers startup/clap/crypto but not
  syscall/perf corners; a native board run is the final check.
- **MIPS NaN non-conformance**
  ([rust-lang/rust#68925](https://github.com/rust-lang/rust/issues/68925)) — low
  risk (little FP).
- **Disk/flash.** Not the daemon's fault, but it disqualifies small-flash boards;
  document the ≥64–128 MB flash / SD-rootfs requirement.

---

## 11. Out of scope / future work

- 16 MB (and generally <128 MB) boards — out on RAM.
- `choreo-gui` (dioxus/blitz/wgpu) on MIPS — heavy C/GTK stack, not shipped.
- `mimalloc` / `aws-lc-sys` on MIPS — would need real porting; avoided.
- `avif` decode on MIPS — needs a cross `libdav1d`; opt-in only.
- A MIPS **native** CI runner (cross + QEMU is sufficient).
- MIPS64 (`mips64el-…`) — a different target class; not covered here.
- A MIPS source package (AUR `makedepends`) vs the prebuilt `-bin`.

---

## 12. Verification / definition of done

- `zlob` bumped past the release carrying the MIPS mapping; the mipsel musl build
  is green with **no** env workarounds.
- `dist/choreographr-<v>-mipsel-unknown-linux-musl.tar.gz` produced, checksummed,
  and attached to a GitHub release by the `release` job.
- `smoke-test.sh` **and** `daemon-smoke.sh` pass on the tarball under
  `qemu-user-static` in CI.
- A **native** run on a real ≥128 MB MIPS32 board boots the daemon and serves a
  session.
- `cargo binstall` resolves on a glibc MIPS host (override present).
- Docs updated; `just pre-commit` green; commit messages written as release
  notes (note: `docs/plans/*` precedent is a plain `docs:` commit, no CHANGELOG
  entry).

---

## 13. References

- Rust platform support (Tier 3 MIPS Linux targets):
  <https://doc.rust-lang.org/nightly/rustc/platform-support.html>
- MIPS NaN note: [rust-lang/rust#68925](https://github.com/rust-lang/rust/issues/68925).
- Upstream fix area: [dmtrKovalenko/zlob#32](https://github.com/dmtrKovalenko/zlob/pull/32)
  and `zlob` `rust/build.rs` (`rust_target_to_clang`, `rust_target_to_zig`).
- `deny.toml` — `[sources]` crates.io-only policy.
- `ring` portable path: `build.rs` `RING_SRCS` (`&[]` entries), `include/ring-core/target.h`.
- `snow` crypto backend: `snow 0.10` `default-resolver-crypto`.
- Power-events soft dependency: `choreo-power-events/src/platform/linux.rs`,
  `choreo-daemon/src/server/core.rs` (`PowerMonitor::best_effort()`).
- Sibling plan: [`docs/plans/risc-v-linux-target.md`](./risc-v-linux-target.md).
