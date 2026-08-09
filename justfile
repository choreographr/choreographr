# Choreographr — `just` task runner
#
# One-stop entry point for building, testing, linting, and running the
# workspace. Bare `just` lists all recipes.
#
# Requirements:
#   - cargo          Rust toolchain >= 1.91 (the workspace MSRV — see Cargo.toml)
#   - zig            builds `zlob`, the glob/walker dependency of choreographr
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
    @command -v zig >/dev/null 2>&1 || { echo "error: zig not found — install it (choreographr's zlob dependency needs it)" >&2; exit 1; }
    @command -v cargo-nextest >/dev/null 2>&1 || echo "note: cargo-nextest not found (recommended — run \`just install-nextest\`)"
    @echo "==> toolchain OK: cargo $(cargo --version | cut -d' ' -f2) · zig $(zig version)"

# Install the primary test runner (cargo-nextest). `brew install nextest` on macOS.
install-nextest:
    cargo install cargo-nextest

# ── hidden prerequisites ──────────────────────────────────────────────────────

# Fail fast with a hint when zig is missing. zig builds the zlob glob/walker
# dependency of choreographr, so every compile/test recipe needs it.
_require-zig:
    @command -v zig >/dev/null 2>&1 || { echo "error: zig not found — run \`brew install zig\` (choreographr's zlob dependency needs it)" >&2; exit 1; }

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

# No linking, but zig is still required (resolution runs zlob's build script).
# Type-check the whole workspace (all targets) — the fastest CI signal.
check: _require-zig
    cargo check {{ CARGO_FLAGS }} --workspace --all-targets

# ── testing ───────────────────────────────────────────────────────────────────

# Full suite — unit + integration — via nextest in one pass (alias of `test-all`)
test: test-all

# Unit tests via nextest (parallel, every test in its own process)
test-fast: _require-nextest _require-zig
    cargo test-fast

# Integration tests — the #[ignore] suite — via nextest
test-integration: _require-nextest _require-zig
    cargo test-integration

# Everything: unit + integration via nextest in one pass
test-all: _require-nextest _require-zig
    cargo test-all

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

# Lint the whole workspace, all targets
clippy: _require-zig
    cargo clippy {{ CARGO_FLAGS }} --workspace --all-targets

# Lint with warnings denied — the CI-grade gate (stricter than `clippy`)
clippy-strict: _require-zig
    cargo clippy {{ CARGO_FLAGS }} --workspace --all-targets -- -D warnings

# Run this before `git commit` — it must pass green.
# The pre-commit gate from AGENTS.md: formatting, lints, and the full suite.
pre-commit: fmt-check clippy test-all

# CI gate: format check + warnings-denied lints + full suite
ci: fmt-check clippy-strict test-all

# ── running ───────────────────────────────────────────────────────────────────

#   just daemon -v            # debug logging
# just daemon "-v -q"       # multiple flags
# Run the daemon (`choreographr`) — default-run selects it from the root package
daemon args="": _require-zig
    cargo run {{ CARGO_FLAGS }} --profile "{{ profile }}" -p choreographr -- {{ args }}

# Run the terminal UI client (root package bin)
tui args="": _require-zig
    cargo run {{ CARGO_FLAGS }} --profile "{{ profile }}" -p choreographr --bin choreo-tui -- {{ args }}

# Run the desktop GUI client (root package bin, gated behind the gui feature)
gui args="": _require-zig
    cargo run {{ CARGO_FLAGS }} --profile "{{ profile }}" -p choreographr --bin choreo-gui --features gui -- {{ args }}

# Run the instant-messaging bridge (e.g. `just im telegram`)
im args="": _require-zig
    cargo run {{ CARGO_FLAGS }} --profile "{{ profile }}" -p choreographr --bin choreo-im -- {{ args }}

# Run the ACP bridge for ACP-compatible editors
acp args="": _require-zig
    cargo run {{ CARGO_FLAGS }} --profile "{{ profile }}" -p choreographr --bin choreo-acp -- {{ args }}

# Run any workspace crate's binary. e.g. `just run choreographr -v`
run crate args="": _require-zig
    cargo run {{ CARGO_FLAGS }} --profile "{{ profile }}" -p {{ crate }} -- {{ args }}

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
