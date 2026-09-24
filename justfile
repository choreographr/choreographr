# Choreographr — `just` task runner
#
# One-stop entry point for building, testing, linting, running, and releasing
# the workspace. Bare `just` lists all recipes.
#
# Requirements:
#   - cargo          Rust toolchain >= 1.94.1 (the workspace MSRV — see Cargo.toml)
#   - zig            builds `zlob`, the glob/walker dependency of choreo-daemon
#                    (install with `brew install zig` on macOS, `apt install zig`
#                    on Debian/Ubuntu, or from ziglang.org)
#   - cargo-nextest  optional but recommended: the primary test runner
#                    (install once with `just install-nextest`; every nextest-backed
#                    recipe fails with a hint until it is on PATH)
#   - git-cliff      release tooling: builds the release notes from commit
#                    messages (`just release-notes`). Required by the release
#                    gate; install once with `just install-git-cliff`
#
# The recipes mirror the README "Testing & development" section and the
# AGENTS.md pre-commit workflow (fmt + clippy + full test suite).

set shell := ["bash", "-euo", "pipefail", "-c"]

# ── configuration ─────────────────────────────────────────────────────────────

# Cargo build profile used by `build`, `check`, and the `run-*` recipes.
# Defaults to release (matching the README quick start); override for faster
# local iteration with: `just --set profile debug build`
profile := "release"

# Extra flags appended to every cargo invocation. Read from the environment so
# CI can inject e.g. `CARGO_FLAGS="--offline"` without editing this file.
CARGO_FLAGS := env_var_or_default("CARGO_FLAGS", "")

# Cross-target gates (check-macos / check-windows) shared flags. The workspace
# pins `-C target-cpu=native` in [profile.dev] (root Cargo.toml) for host speed,
# but `native` expands to the *host's* CPU, which a foreign target rejects (e.g.
# `znver3` on x86 for aarch64) — so the cross gates must not inherit it. Two
# prongs, because the two consumers read rustflags differently:
#   * cross_config clears the profile's rustflags for the actual cargo build —
#     that is where the workspace's native flag lives.
#   * cross_rustflags (a non-empty RUSTFLAGS; its content is irrelevant to a
#     type-check) masks any *machine-level* rustflags for cargo-zigbuild's
#     `zig cc` probe, which reads them via cargo-config2 and — unlike cargo —
#     ignores `--config`.
cross_config := "--config 'profile.dev.rustflags=[]'"
cross_rustflags := "-Cdebuginfo=0"

# ── entry points ──────────────────────────────────────────────────────────────

# Show all recipes (default — run with bare `just`)
default:
    @just --list

# Show all recipes (alias of `default`)
help:
    @just --list

# Verify the toolchain: cargo + zig + git-cliff required, cargo-nextest recommended
preflight:
    @echo "==> checking toolchain"
    @command -v cargo >/dev/null 2>&1 || { echo "error: cargo not found — install Rust via rustup (https://rustup.rs/)" >&2; exit 1; }
    @command -v zig >/dev/null 2>&1 || { echo "error: zig not found — install it (choreo-daemon's zlob dependency needs it)" >&2; exit 1; }
    @command -v git-cliff >/dev/null 2>&1 || { echo "error: git-cliff not found — release notes are generated from commit messages (run \`just install-git-cliff\`)" >&2; exit 1; }
    @command -v cargo-nextest >/dev/null 2>&1 || echo "note: cargo-nextest not found (recommended — run \`just install-nextest\`)"
    @echo "==> toolchain OK: cargo $(cargo --version | cut -d' ' -f2) · zig $(zig version) · git-cliff $(git-cliff --version | cut -d' ' -f2)"

# Install the primary test runner (cargo-nextest). `brew install nextest` on macOS.
install-nextest:
    cargo install cargo-nextest

# Install git-cliff (the release-notes generator). Prebuilt binaries from
# https://github.com/orhun/git-cliff are fine too.
install-git-cliff:
    cargo install git-cliff

# ── hidden prerequisites ──────────────────────────────────────────────────────

# Fail fast with a hint when zig is missing. zig builds the zlob glob/walker
# dependency of choreographr, so every compile/test recipe needs it.
_require-zig:
    @command -v zig >/dev/null 2>&1 || { echo "error: zig not found — run \`brew install zig\` (choreo-daemon's zlob dependency needs it)" >&2; exit 1; }

# Fail fast with a hint when cargo-nextest is missing. The test-* recipes are
# the README's primary runner; failing here beats cargo's cryptic "no such
# command: test-fast".
_require-nextest:
    @command -v cargo-nextest >/dev/null 2>&1 || { echo "error: cargo-nextest not found — run \`just install-nextest\`" >&2; exit 1; }

# Fail fast with a hint when git-cliff is missing (release-notes generation).
_require-git-cliff:
    @command -v git-cliff >/dev/null 2>&1 || { echo "error: git-cliff not found — run \`just install-git-cliff\`" >&2; exit 1; }

# ── build & check ─────────────────────────────────────────────────────────────

# Build the whole workspace with `profile` (release by default)
build: _require-zig
    cargo build {{ CARGO_FLAGS }} --workspace --profile "{{ profile }}"

# Build the whole workspace in debug mode (fast local iteration)
build-debug: _require-zig
    cargo build {{ CARGO_FLAGS }} --workspace --profile debug

# ── stable builds (escape hatch from the nightly default) ─────────────────────
# The default toolchain is NIGHTLY (see rust-toolchain.toml), so every adhoc
# `cargo` command — including per-crate `cargo build -p choreo-x` / `cargo
# check -p x` / `cargo nextest run -p x` — automatically applies the fast
# per-profile `-Z` flags via [unstable] profile-rustflags (see .cargo/config.toml).
# Those same nightly-only bits HARD-BLOCK stable Cargo, so a stable build is an
# explicit opt-out: these recipes run through scripts/build-stable.sh, which
# temporarily strips the nightly-only config/manifest keys for one command and
# restores them afterwards. Stable compatibility itself is guaranteed by the
# code (no nightly features) and the CI MSRV job, independent of this config.

# Stable build (the `profile` — release by default — plus any extra args)
build-stable: _require-zig
    ./scripts/build-stable.sh build --workspace --profile "{{ profile }}" {{ CARGO_FLAGS }}

# Stable type-check (all targets, no linking)
check-stable: _require-zig
    ./scripts/build-stable.sh check --workspace --all-targets {{ CARGO_FLAGS }}

# Stable unit tests (libtest, serialized — no nextest needed)
test-stable: _require-zig
    ./scripts/build-stable.sh test --workspace {{ CARGO_FLAGS }}

# Publish the 14 crates to crates.io (RELEASE.md Phase 2) with the nightly-only
# per-profile rustflags stripped, so published manifests stay buildable by
# stable consumers (`cargo install choreographr --locked`). Runs
# `cargo release publish --workspace` (dry-run by default; add `-- -x` to
# execute, or pass any other cargo-release args after `--`; add `-- --allow-dirty`
# to skip the wrapper's clean-tree gate — see scripts/publish-stable.sh).
publish-stable args="publish --workspace":
    ./scripts/publish-stable.sh {{ args }}

# No linking, but zig is still required (resolution runs zlob's build script).
# Type-check the whole workspace (all targets) — the fastest CI signal.
check: _require-zig
    cargo check {{ CARGO_FLAGS }} --workspace --all-targets

# macOS cross-compile gate (works from Linux or macOS): type-check every library
# crate for aarch64-apple-darwin using zig's clang+lld via cargo-zigbuild's
# `check` subcommand. Note the direct `cargo-zigbuild` invocation — `cargo
# zigbuild check` would misroute to the build subcommand, because cargo passes
# the subcommand name through (cargo-zigbuild 0.23+).
#
# Libs only, deliberately: the final Mach-O binary link needs the Apple SDK,
# which cannot be redistributed (it fails with "unable to find framework
# 'CoreFoundation'"), so real macOS binaries are built on a macOS host in CI.
# `cargo check` skips linking, so this gate passes — but C build scripts (ring,
# zlob, ckb-vm, onig) DO run, so target-specific C breakage is caught too.
#
# Do NOT add `--all-features`: the `blockchain` feature pulls subxt →
# native-tls → security-framework-sys, whose bindgen step reads Apple's
# Security.framework headers (SDK territory — fails from Linux).
#
# cross_config + cross_rustflags clear the host-arch `-C target-cpu=native`
# the workspace pins in [profile.dev] — without them this gate fails on any
# host whose arch differs from the target (see the cross_config definition).
check-macos: _require-zig
    rustup target add aarch64-apple-darwin
    RUSTFLAGS="{{ cross_rustflags }}" cargo-zigbuild check {{ CARGO_FLAGS }} {{ cross_config }} --target aarch64-apple-darwin --workspace --lib

# Windows cross-compile gate: type-check every library crate for
# x86_64-pc-windows-gnu via zig (MinGW bundled — no mingw install needed).
# Same libs-only rationale and host-arch neutralization as check-macos; this is
# the recipe to iterate the Windows port against. A clean build also surfaces
# the zlob archive-naming quirk (zig emits `zlob.lib`, the windows-gnu target
# wants `libzlob.a`).
check-windows: _require-zig
    rustup target add x86_64-pc-windows-gnu
    RUSTFLAGS="{{ cross_rustflags }}" cargo-zigbuild check {{ CARGO_FLAGS }} {{ cross_config }} --target x86_64-pc-windows-gnu --workspace --lib

# Both foreign-target gates in one command: `check-windows` + `check-macos`.
# This is the local stand-in for the release workflow's windows-msvc and macos
# jobs minus the multi-minute full build/link. It catches `#[cfg(windows)]` /
# `#[cfg(target_os = "macos")]` breakage that the host gate is structurally
# blind to — those blocks are cfg'd out on Linux, so `pre-commit`'s
# clippy/test never even parse them (this is how a Windows-only `Ok(())` vs
# `Result<SigId, _>` mismatch once reached a release build). Deliberately NOT
# part of `pre-commit`: it needs zig + cargo-zigbuild and rebuilds the whole
# dependency tree once per target. Run it by hand when touching platform-gated
# code or before a release.
check-cross: check-windows check-macos

# ── Android ───────────────────────────────────────────────────────────────────
# Termux is the Android runtime for the four suite binaries: they are pushed
# into Termux's $PREFIX/bin as plain executables. choreo-gui is different —
# on Android it is built by `dx` as a cdylib (APK payload), not a suite binary.

# Cross-build the four suite binaries for aarch64-linux-android via
# cargo-ndk. Requires an Android NDK (ANDROID_NDK_HOME / sdkmanager layout);
# add `emulator=true` for an x86_64 build too. See scripts/build-android.sh.
android-binaries *args="":
    ./scripts/build-android.sh {{ args }}

# Prerequisite check + dry run for `android-binaries` — validates cargo-ndk,
# the NDK, and the rustup Android targets, then prints what would run without
# touching the tree. This is the script's verification path (no NDK needed to
# at least see it fail with an actionable message).
android-check:
    ./scripts/build-android.sh --check

# Build choreo-gui for Android via Dioxus CLI (NOT part of build-android.sh —
# different toolchain: dx drives the Android Gradle/cdylib packaging itself).
# `dx` infers the native renderer on Android; requires the NDK for the final
# link. `--package` scopes the build to the choreo-gui crate (dx reads the
# workspace root; the Dioxus config lives in the crate).
#
# Self-sufficient: the `android` CLI (cmdline-tools 23+) replaces the old
# sdkmanager and auto-accepts licenses, so any SDK packages gradle wants
# (platform, build-tools) are fetched here on first run instead of failing
# mid-build. Root-requiring setup (the ANDROID_HOME/ndk symlink for the AUR
# standalone-NDK layout) is NOT attempted here — one-time setup is validated
# below with exact instructions; everything else runs as the normal user.
#
# JDK note: the dx-generated Gradle build (Gradle 9.1) cannot run on JDK 26+
# — its Groovy build-script compiler rejects class file major version 70
# ("Unsupported class file major version 70"). JAVA_HOME is therefore
# normalized to a supported JDK the same way ANDROID_HOME/ANDROID_NDK_HOME
# are: only when the caller has not already set it, never overriding an
# explicit choice. Preference order: JAVA_HOME → JDK 21 (LTS) → JDK 25 →
# whatever `java` resolves to (with a warning, since a too-new default is
# exactly the failure mode above).
gui-android args="":
    #!/usr/bin/env bash
    set -euo pipefail
    # Read-only env normalization (see build-android.sh): dx/gradle only look
    # for the NDK under ANDROID_HOME/ndk, never via ANDROID_NDK_HOME.
    if [ -z "${ANDROID_NDK_HOME:-}" ] && [ -d /opt/android-ndk ]; then
        export ANDROID_NDK_HOME=/opt/android-ndk
    fi
    if [ -z "${ANDROID_HOME:-}" ] && [ -d "$HOME/Android/Sdk" ]; then
        export ANDROID_HOME="$HOME/Android/Sdk"
    elif [ -z "${ANDROID_HOME:-}" ] && [ -d /opt/android-sdk ]; then
        export ANDROID_HOME=/opt/android-sdk   # read-only fallback; SDK installs need a writable ANDROID_HOME
    fi
    # Normalize JAVA_HOME to a Gradle-9.1-compatible JDK (see the comment
    # above): explicit JAVA_HOME wins, then the known JDK install roots, then
    # the PATH default with a warning. Exported so gradle (a JVM process dx
    # spawns) and the dx JVM toolchain resolution both inherit it.
    if [ -z "${JAVA_HOME:-}" ]; then
        for jdk in /usr/lib/jvm/java-21-openjdk /usr/lib/jvm/java-25-openjdk \
                   /usr/lib/jvm/java-17-openjdk; do
            if [ -x "$jdk/bin/java" ]; then
                export JAVA_HOME="$jdk"
                break
            fi
        done
        if [ -z "${JAVA_HOME:-}" ]; then
            echo "warning: no JAVA_HOME set and no known JDK found under /usr/lib/jvm; gradle will use the default java (must be JDK ≤ 25 — newer JDKs fail with 'Unsupported class file major version 70')" >&2
        fi
    fi
    [ -n "${ANDROID_HOME:-}" ] || { echo "error: no Android SDK found (set ANDROID_HOME)" >&2; exit 1; }
    [ -n "${ANDROID_NDK_HOME:-}" ] || { echo "error: no Android NDK found (set ANDROID_NDK_HOME)" >&2; exit 1; }
    # Validate (read-only) that the NDK the build will actually use exists.
    # dx resolves the NDK via ANDROID_NDK_HOME alone (verified empirically) and
    # does NOT require the ANDROID_HOME/ndk/<version> convention, so requiring
    # that symlink would wrongly reject valid setups like /opt/android-ndk.
    [ -e "$ANDROID_NDK_HOME/source.properties" ] || {
        echo "error: $ANDROID_NDK_HOME does not look like an NDK (no source.properties)" >&2
        exit 1
    }
    # Suppress user-level cargo config rustflags for the Android codegen, the
    # same way scripts/build-android.sh does: ~/.cargo/config.toml typically
    # sets [build] rustflags = -C target-cpu=native (host-CPU flags that poison
    # Android codegen and can even SEGFAULT nightly LLVM on aarch64). When
    # RUSTFLAGS is set, cargo ignores build/target rustflags from config files
    # entirely; an empty-string override is treated as unset and --config
    # build.rustflags=[] does not reliably win, so exporting RUSTFLAGS is the
    # only dependable suppression. Exported (not per-command) so it also covers
    # any nested cargo invocation dx makes.
    export RUSTFLAGS="-C target-cpu=generic"
    # Pre-fetch the SDK packages gradle/dx need if the `android` CLI is
    # available (best effort — if dx wants a different version it will say so
    # and the user can `android sdk install` it explicitly).
    if command -v android >/dev/null 2>&1 && [ -n "${ANDROID_HOME:-}" ]; then
        android sdk install platforms/android-35 build-tools/35.0.0 || \
            echo "warning: could not pre-install SDK packages; dx will report exactly what it needs" >&2
    fi
    exec dx build --platform android --release --package choreo-gui {{ args }}

# ── testing ───────────────────────────────────────────────────────────────────

# Full suite — unit + integration — via nextest in one pass (alias of `test-all`)
test: test-all

# Unit tests via nextest (parallel, every test in its own process)
test-fast: _require-nextest _require-zig
    cargo test-fast

# Unit tests with every optional feature off (the default config) — compiles
# the metrics no-op stubs and the feature-off `--metrics-addr` refusal that the
# `--all-features` recipes never build
# (see .cargo/config.toml for why this guards against stub drift)
test-lean: _require-nextest _require-zig
    cargo test-lean

# Integration tests — the #[ignore] suite — via nextest
test-integration: _require-nextest _require-zig
    cargo test-integration

# Everything: unit + integration via nextest in one pass.
# `--locked` keeps the committed Cargo.lock authoritative on the gate path
# (pre-commit): a stale/regenerated lockfile fails instead of silently
# re-resolving against the live registry (see release.sh for the same control).
test-all: _require-nextest _require-zig
    cargo test-all {{ CARGO_FLAGS }} --locked

# Unit tests via libtest (serialized across binaries; no nextest required)
test-libtest: _require-zig
    cargo test {{ CARGO_FLAGS }} --workspace

# Integration tests via libtest (the #[ignore] suite; no nextest required)
test-libtest-ignored: _require-zig
    cargo test {{ CARGO_FLAGS }} --workspace -- --ignored

# Note: the cargo test-* aliases bake in `--workspace` and reject `-p`, so this
# calls nextest directly (see README).
# Test a single crate with nextest (e.g. `just test-crate choreo-proto`)
test-crate crate: _require-nextest _require-zig
    cargo nextest run {{ CARGO_FLAGS }} -p {{ crate }}

# Run doc tests across the workspace
test-doc: _require-zig
    cargo test {{ CARGO_FLAGS }} --workspace --doc

# Run a shard of the nextest suite (CI parallelism). e.g. `just shard 1/2`
shard part: _require-nextest _require-zig
    cargo nextest run {{ CARGO_FLAGS }} --workspace --partition count:{{ part }}

# Re-run flaky nextest tests up to N times. e.g. `just retry 2`
retry n="2": _require-nextest _require-zig
    cargo nextest run {{ CARGO_FLAGS }} --workspace --retries {{ n }}

# ── lint & format ─────────────────────────────────────────────────────────────

# Format all code in place (rustfmt). Flags live in .cargo/config.toml (`fmt-all`).
fmt:
    cargo fmt-all {{ CARGO_FLAGS }}

# Check formatting without modifying files (tree-stays-clean gate).
fmt-check:
    cargo fmt-all {{ CARGO_FLAGS }} -- --check

# Lint the whole workspace, all targets + all features (flags in `clippy-all`).
clippy: _require-zig
    cargo clippy-all {{ CARGO_FLAGS }}

# Lint with warnings denied — a genuinely clean-clippy gate (any warning fails).
clippy-strict: _require-zig
    cargo clippy-all {{ CARGO_FLAGS }} -- -D warnings

# Auto-apply machine-applicable lints in place; the rest are hand-fixed afterwards.
# `--allow-dirty --allow-staged` let it edit a tree that already holds the in-progress
# change. NOT part of `pre-commit` — the commit gate only VERIFIES lint cleanliness
# (via `clippy-strict`); run this by hand first when you want the autofixable lints
# applied before looping the gate.
clippy-fix: _require-zig
    cargo clippy-all-fix {{ CARGO_FLAGS }}

# Supply-chain gate: deny.toml bans (the 2026-08-20 arrayref@0.3.10 attacker
# versions — RUSTSEC-2026-0260 — plus the six deleted payload crates), RustSec
# advisories, crates.io-only sources, and a local registry-cache scan for the
# deleted malicious .crate files. See scripts/check-supply-chain.sh.
check-supply-chain:
    ./scripts/check-supply-chain.sh

# Preview the release notes, generated from commit messages by git-cliff
# (cliff.toml, via scripts/release-notes.sh). Defaults to the workspace version;
# pass one to preview an upcoming release, e.g. `just release-notes 0.3.0`.
release-notes args="": _require-git-cliff
    ./scripts/release-notes.sh {{ args }}

# Release preflight: the release is cut from `master`, up to date with origin, on
# a clean working tree (RELEASE.md "Preflight"). Read-only — it only inspects git
# state; it never fetches, checks out, or pulls (those are the conductor's move).
check-release-state:
    #!/usr/bin/env bash
    set -euo pipefail
    branch="$(git rev-parse --abbrev-ref HEAD)"
    if [ "$branch" != "master" ]; then
        echo "error: releases are cut from master, not '$branch'" >&2
        exit 1
    fi
    if [ -n "$(git status --porcelain)" ]; then
        echo "error: working tree is not clean — commit or stash first:" >&2
        git status --short >&2
        exit 1
    fi
    # "Up to date with origin" is checked against the LOCAL remote-tracking ref
    # (no network): compare HEAD to where origin/master was last fetched to.
    if git rev-parse --verify --quiet origin/master >/dev/null; then
        behind="$(git rev-list --count HEAD..origin/master)"
        if [ "$behind" -ne 0 ]; then
            echo "error: HEAD is $behind commit(s) behind origin/master — run \`git pull --ff-only\`" >&2
            exit 1
        fi
    else
        echo "note: no origin/master ref found — skipping the up-to-date check" >&2
    fi
    echo "OK: on master, clean, and not behind origin/master"

# Release preflight: confirm the stored crates.io token authenticates, so
# Phase 2's publish cannot die mid-batch on a missing/expired/revoked token.
# `GET /api/v1/me` is cookie-only (crates.io forbids API tokens on it), so the
# script probes the auth RESPONSE instead of expecting a 200 — see the header of
# scripts/check-crates-io-token.sh. Read-only.
check-crates-io-token:
    ./scripts/check-crates-io-token.sh

# Release preflight, final step: publish master and kick the release workflow via
# `workflow_dispatch`. That run builds every platform exactly like a tag does but
# creates NO GitHub release (the `release` job is tag-only: `if: startsWith(ref,
# 'refs/tags/v')`), so the pipeline is exercised on GitHub before Phase 1 ever
# tags — the first real tag is never the first test of the pipeline. Requires an
# authenticated `gh` and push access to origin.
release-workflow-dry-run:
    #!/usr/bin/env bash
    set -euo pipefail
    git push origin master
    gh workflow run release.yml --ref master
    echo "release workflow dispatched (dry run — no release created)"
    echo "watch it with: gh run watch   (or the repo's Actions tab)"

# Install the dependency-policy tool cargo-deny (the authoritative layer of
# check-supply-chain). Without it the script falls back to cargo-audit + a
# literal lockfile scan, which covers advisories but not hard version bans.
install-cargo-deny:
    cargo install cargo-deny

# The commit gate (AGENTS.md → Commit Workflow): prove clippy-clean → full test
# suite → format LAST. `clippy-strict` denies warnings, so any remaining lint
# fails the gate and is hand-fixed (run `just clippy-fix` first if you want the
# machine-applicable lints applied automatically). Formatting runs last, not
# first, so the formatted bytes stay behaviourally identical to the tested bytes.
# Safe to re-run: loop it (fix by hand, re-run) until it passes green, then
# `git commit`. The supply-chain guard is a RELEASE guard — it runs in the
# release workflow, not here.
pre-commit: clippy-strict test-all fmt

# The release gate (RELEASE.md → Preflight) — the single command the conductor
# runs before Phase 1. Every local check, in one pass: the toolchain
# (`preflight`), the git release state (on master, clean, not behind origin), the
# quality gate with its one tree-mutating step — `fmt` — replaced by the
# non-mutating `fmt-check` (alongside `clippy-strict`) — the full test suite, the
# release-only guard `pre-commit` omits (`check-supply-chain`), the crates.io
# credential check, and finally the GitHub dry run (`release-workflow-dry-run`):
# push master + kick the release workflow so the pipeline is proven before a tag.
# Nothing here edits the working tree. Never use `pre-commit` as a release gate —
# it still rewrites the source with `cargo fmt`.
pre-release: preflight check-release-state fmt-check clippy-strict test-all \
    check-supply-chain check-crates-io-token release-workflow-dry-run

# ── running ───────────────────────────────────────────────────────────────────

#   just daemon -v            # debug logging
# just daemon "-v -q"       # multiple flags
# Run the daemon (`choreographr`) — default-run selects it from the root package
daemon args="": _require-zig
    cargo run {{ CARGO_FLAGS }} --profile "{{ profile }}" -p choreographr -- {{ args }}

# Run the terminal UI client (its own crate — owns its binary)
tui args="": _require-zig
    cargo run {{ CARGO_FLAGS }} --profile "{{ profile }}" -p choreo-tui -- {{ args }}

# Run the desktop GUI client (its own crate — owns its binary)
gui args="": _require-zig
    cargo run {{ CARGO_FLAGS }} --profile "{{ profile }}" -p choreo-gui -- {{ args }}

# Run the instant-messaging bridge (e.g. `just im telegram`).
# Its own crate (choreo-im) owns the binary — no root feature gating anymore.
im args="":
    cargo run {{ CARGO_FLAGS }} --profile "{{ profile }}" -p choreo-im -- {{ args }}

# Run the ACP bridge for ACP-compatible editors.
# Its own crate (choreo-acp) owns the binary. No _require-zig: these bridge
# crates do not depend on choreo-daemon (the only zlob consumer), so the
# zig requirement the old root-feature-gated recipes inherited is gone.
acp args="":
    cargo run {{ CARGO_FLAGS }} --profile "{{ profile }}" -p choreo-acp -- {{ args }}

# Run any workspace crate's binary. e.g. `just run choreographr -v`
run crate args="": _require-zig
    cargo run {{ CARGO_FLAGS }} --profile "{{ profile }}" -p {{ crate }} -- {{ args }}

# ── release & packaging ──────────────────────────────────────────────────────

# Release version — read from the workspace manifest (the single source of
# truth that scripts/release.sh, the Homebrew formula, and the AUR PKGBUILD
# mirror). Evaluated when `just` loads; the release scripts re-read it.
VERSION := `sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -n1`

# Host release target — the same target set scripts/release.sh and
# scripts/install.sh hardcode (Linux x86_64 + arm64 — both static musl — plus
# both darwin triples, which release.sh builds in one pass on a Darwin-arm64
# host; smoke-test defaults to the host triple's tarball).
TARGET := `case "$(uname -s)-$(uname -m)" in Linux-x86_64) echo x86_64-unknown-linux-musl ;; Linux-aarch64) echo aarch64-unknown-linux-musl ;; Darwin-arm64) echo aarch64-apple-darwin ;; *) echo unsupported ;; esac`

# The tarball `just release` produces — used as the default for
# `just smoke-test` (built by concatenation: just does not recursively
# interpolate `{{ }}` inside a variable's value, so the pieces must be
# joined here, not nested).
release-tarball := "dist/choreographr-" + VERSION + "-" + TARGET + ".tar.gz"

# Pass extra flags through after `--`, e.g. `just release -- --upload --allow-dirty`.
# Dry-run release: build the four shipped binaries, pack the release tarball,
# write SHA256SUMS, build the .deb/.rpm when the tools are present — never uploads.
release args="": _require-zig
    ./scripts/release.sh {{ args }}

# Build everything AND run `gh release create` (requires gh, a clean tree,
# and a tag).
# Full release: the dry run plus upload.
release-upload: _require-zig
    ./scripts/release.sh --upload

# Release from a dirty tree (CI-style flows that stage files first).
release-allow-dirty: _require-zig
    ./scripts/release.sh --allow-dirty

# Pass flags through, e.g. `just release-tap -- --push`.
# Bump the Homebrew tap formula to the release version — dry-run by default:
# clones choreographr/homebrew-choreographr, rewrites Formula/choreographr.rb
# from the dist/ tarballs (version, urls, sha256 digests), validates, and
# prints the diff. `--push` commits + pushes to the tap repo. Run on the
# Linux box after Phase 4 (macOS tarball staged in dist/).
release-tap args="":
    ./scripts/update-homebrew-tap.sh {{ args }}

# (just parameter defaults are literal, so the fallback is an `if` expression.)
# Smoke-test a release tarball — defaults to the one `just release` just built.
smoke-test tarball="":
    ./scripts/smoke-test.sh "{{ if tarball == "" { release-tarball } else { tarball } }}"

# Build the fat .deb from existing target/dist artifacts (Linux only)
package-deb:
    ./scripts/build-deb.sh

# Build the fat .rpm from existing target/dist artifacts (Linux only)
package-rpm:
    ./scripts/build-rpm.sh

# Build the Termux .deb from existing target/android/arm64-v8a binaries
# (Linux only; build those first with `just android-binaries`)
package-deb-termux:
    ./scripts/build-deb-termux.sh

# Pass flags through, e.g. `just install -- --uninstall`.
# Run the pinned-version installer locally (instead of curl|sh).
install args="":
    ./scripts/install.sh {{ args }}

# ── docs & maintenance ────────────────────────────────────────────────────────

# Build API documentation for the workspace (without dependencies)
doc: _require-zig
    cargo doc {{ CARGO_FLAGS }} --workspace --no-deps

# Build and open the API documentation in the browser
doc-open: _require-zig
    cargo doc {{ CARGO_FLAGS }} --workspace --no-deps --open

# Remove all build artifacts
clean:
    cargo clean

# Update all dependencies to the latest compatible versions
update:
    cargo update

# Show the workspace dependency tree
tree:
    cargo tree {{ CARGO_FLAGS }}
