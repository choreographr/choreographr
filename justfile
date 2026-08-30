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

# ── entry points ──────────────────────────────────────────────────────────────

# Show all recipes (default — run with bare `just`)
default:
    @just --list

# Show all recipes (alias of `default`)
help:
    @just --list

# Verify the toolchain: cargo + zig required, cargo-nextest recommended
preflight:
    @echo "==> checking toolchain"
    @command -v cargo >/dev/null 2>&1 || { echo "error: cargo not found — install Rust via rustup (https://rustup.rs/)" >&2; exit 1; }
    @command -v zig >/dev/null 2>&1 || { echo "error: zig not found — install it (choreo-daemon's zlob dependency needs it)" >&2; exit 1; }
    @command -v cargo-nextest >/dev/null 2>&1 || echo "note: cargo-nextest not found (recommended — run \`just install-nextest\`)"
    @echo "==> toolchain OK: cargo $(cargo --version | cut -d' ' -f2) · zig $(zig version)"

# Install the primary test runner (cargo-nextest). `brew install nextest` on macOS.
install-nextest:
    cargo install cargo-nextest

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
check-macos: _require-zig
    rustup target add aarch64-apple-darwin
    cargo-zigbuild check {{ CARGO_FLAGS }} --target aarch64-apple-darwin --workspace --lib

# Windows cross-compile gate: type-check every library crate for
# x86_64-pc-windows-gnu via zig (MinGW bundled — no mingw install needed).
# Same libs-only rationale as check-macos; this is the recipe to iterate the
# Windows port against. A clean build also surfaces the zlob archive-naming
# quirk (zig emits `zlob.lib`, the windows-gnu target wants `libzlob.a`).
check-windows: _require-zig
    rustup target add x86_64-pc-windows-gnu
    cargo-zigbuild check {{ CARGO_FLAGS }} --target x86_64-pc-windows-gnu --workspace --lib

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
gui-android args="":
    #!/usr/bin/env bash
    set -euo pipefail
    # Read-only env normalization (see build-android.sh): dx/gradle only look
    # for the NDK under ANDROID_HOME/ndk, never via ANDROID_NDK_HOME.
    if [ -z "${ANDROID_NDK_HOME:-}" ] && [ -d /opt/android-ndk ]; then
        export ANDROID_NDK_HOME=/opt/android-ndk
    fi
    if [ -z "${ANDROID_HOME:-}" ] && [ -d /opt/android-sdk ]; then
        export ANDROID_HOME=/opt/android-sdk
    fi
    [ -n "${ANDROID_HOME:-}" ] || { echo "error: no Android SDK found (set ANDROID_HOME)" >&2; exit 1; }
    [ -n "${ANDROID_NDK_HOME:-}" ] || { echo "error: no Android NDK found (set ANDROID_NDK_HOME)" >&2; exit 1; }
    # Validate (not mutate) the ANDROID_HOME/ndk/<version> convention. If it
    # is missing, print the exact one-time root setup and stop — the build
    # itself must never write to /opt.
    ver="$(sed -n 's/^Pkg.Revision *= *//p' "$ANDROID_NDK_HOME/source.properties" 2>/dev/null | head -n1)"
    if [ -n "$ver" ] && [ ! -e "$ANDROID_HOME/ndk/$ver" ]; then
        echo "error: $ANDROID_HOME/ndk/$ver does not exist (dx/gradle look for the NDK there)." >&2
        echo "  One-time root setup, then re-run:" >&2
        echo "    sudo mkdir -p '$ANDROID_HOME/ndk' && sudo ln -sfn '$ANDROID_NDK_HOME' '$ANDROID_HOME/ndk/$ver'" >&2
        exit 1
    fi
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
# `--locked` keeps the committed Cargo.lock authoritative on the gate path (CI
# and pre-commit): a stale/regenerated lockfile fails instead of silently
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

# Format all code in place (rustfmt)
fmt:
    cargo fmt {{ CARGO_FLAGS }} --all

# Check formatting without modifying files (CI gate)
fmt-check:
    cargo fmt {{ CARGO_FLAGS }} --all -- --check

# Lint the whole workspace, all targets. `--locked` = no silent lockfile reuse.
clippy: _require-zig
    cargo clippy {{ CARGO_FLAGS }} --locked --workspace --all-targets

# Lint with warnings denied — the CI-grade gate (stricter than `clippy`)
clippy-strict: _require-zig
    cargo clippy {{ CARGO_FLAGS }} --locked --workspace --all-targets -- -D warnings

# Supply-chain gate: deny.toml bans (the 2026-08-20 arrayref@0.3.10 attacker
# versions — RUSTSEC-2026-0260 — plus the six deleted payload crates), RustSec
# advisories, crates.io-only sources, and a local registry-cache scan for the
# deleted malicious .crate files. See scripts/check-supply-chain.sh.
check-supply-chain:
    ./scripts/check-supply-chain.sh

# Install the dependency-policy tool cargo-deny (the authoritative layer of
# check-supply-chain). Without it the script falls back to cargo-audit + a
# literal lockfile scan, which covers advisories but not hard version bans.
install-cargo-deny:
    cargo install cargo-deny

# Run this before `git commit` — it must pass green.
# The pre-commit gate from AGENTS.md: formatting, lints, the full suite, and
# the supply-chain checks (deny.toml bans + RustSec advisories + cache scan).
pre-commit: fmt-check clippy test-all check-supply-chain

# CI gate: format check + warnings-denied lints + full suite + supply chain
ci: fmt-check clippy-strict test-all check-supply-chain

# ── running ───────────────────────────────────────────────────────────────────

#   just daemon -v            # debug logging
# just daemon "-v -q"       # multiple flags
# Run the daemon (`choreographr`) — default-run selects it from the root package
daemon args="": _require-zig
    cargo run {{ CARGO_FLAGS }} --profile "{{ profile }}" -p choreographr -- {{ args }}

# Run the terminal UI client (root package bin)
tui args="": _require-zig
    cargo run {{ CARGO_FLAGS }} --profile "{{ profile }}" -p choreographr --bin choreo-tui -- {{ args }}

# Run the desktop GUI client (its own crate — owns its binary)
gui args="": _require-zig
    cargo run {{ CARGO_FLAGS }} --profile "{{ profile }}" -p choreo-gui -- {{ args }}

# Run the instant-messaging bridge (e.g. `just im telegram`)
im args="": _require-zig
    cargo run {{ CARGO_FLAGS }} --profile "{{ profile }}" -p choreographr --bin choreo-im -- {{ args }}

# Run the ACP bridge for ACP-compatible editors
acp args="": _require-zig
    cargo run {{ CARGO_FLAGS }} --profile "{{ profile }}" -p choreographr --bin choreo-acp -- {{ args }}

# Run any workspace crate's binary. e.g. `just run choreographr -v`
run crate args="": _require-zig
    cargo run {{ CARGO_FLAGS }} --profile "{{ profile }}" -p {{ crate }} -- {{ args }}

# ── release & packaging ──────────────────────────────────────────────────────

# Release version — read from the workspace manifest (the single source of
# truth that scripts/release.sh, the Homebrew formula, and the AUR PKGBUILD
# mirror). Evaluated when `just` loads; the release scripts re-read it.
VERSION := `sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -n1`

# Host release target — the same two-target set scripts/release.sh and
# scripts/install.sh hardcode (Linux x86_64 / macOS arm64). Linux maps to the
# static musl triple: the release tarball is a fully static musl build.
TARGET := `case "$(uname -s)-$(uname -m)" in Linux-x86_64) echo x86_64-unknown-linux-musl ;; Darwin-arm64) echo aarch64-apple-darwin ;; *) echo unsupported ;; esac`

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

# Build the fat .deb from existing target/release artifacts (Linux only)
package-deb:
    ./scripts/build-deb.sh

# Build the fat .rpm from existing target/release artifacts (Linux only)
package-rpm:
    ./scripts/build-rpm.sh

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
