#!/usr/bin/env bash
# scripts/publish-stable.sh — run a cargo-release publish with a manifest that
# stable consumers can actually build.
#
# The per-profile `rustflags` keys in the root Cargo.toml (`-Z…` frontend flags
# and `-C target-cpu=native`) require the nightly-only `profile-rustflags`
# cargo feature. That is fine for local builds under the nightly default
# toolchain, but the keys ride along into the `.crate` that `cargo publish`
# uploads — and a published manifest that contains `[profile.*] rustflags`
# HARD-BREAKS stable `cargo install` ("The package requires the Cargo feature
# called `profile-rustflags`"), killing the crates.io install route that
# RELEASE.md Phase 2/6 verify. So publishing must ship a STRIPPED manifest:
# no per-profile rustflags, and no `[unstable]` opt-in in the config.
#
# Why the strip is masked: cargo-release 1.1.5's `publish` step runs an
# UNCONDITIONAL clean-tree check (`verify_git_is_clean` in src/steps/mod.rs —
# there is no `--allow-dirty` flag and no `allow_dirty` config key in this
# version; the check hard-fails on any dirty tree in execute mode), and the
# check reads git status through libgit2. The strip itself dirties the tree,
# so it would always trip that check. The script therefore marks exactly the
# two files it modifies (`Cargo.toml`, `.cargo/config.toml`) with
# `git update-index --skip-worktree` before stripping — libgit2 then reports
# them clean (verified) — runs `cargo release`, and restores both files and
# clears the marks on every exit path. `cargo package` itself tolerates the
# dirtiness internally (cargo-release already passes `--allow-dirty` there).
#
# Gatekeeping: publishing ships the working-tree state (the `.crate` is built
# from the tree, not from HEAD), so uncommitted changes must never silently
# ride along. By default the script refuses to start on a dirty tree. Pass
# `--allow-dirty` to skip that gate — intended for DRY-RUN planning from a
# dirty tree, not for executing a publish that is not committed-clean (on
# execute, cargo-release's own gate still requires every file except our two
# masked ones to match HEAD).
#
# Usage:
#   ./scripts/publish-stable.sh [--allow-dirty] [cargo-release args...]
#   # e.g. ./scripts/publish-stable.sh publish --workspace        (dry-run)
#   #      ./scripts/publish-stable.sh publish --workspace -x      (execute)
#
# This wrapper handles the `publish` STEP ONLY. Do not run the full
# multi-step `cargo release` (version+commit+tag) through it — the masks would
# confuse cargo-release's own commit step.
#
# choreo-gui is always excluded from the publish selection (the wrapper
# appends `--exclude choreo-gui`): cargo-release 1.1.5 does NOT honor
# `publish = false` in `--workspace` selection (its plan lists the GUI crate
# and a real publish would then hit cargo's own refusal for a publish=false
# crate), and the GUI stub must never be published. Verified against 1.1.5.
#
# Single-authored only (not safe under concurrent runs).
set -euo pipefail
cd "$(dirname "$0")/.."

if ! command -v cargo-release >/dev/null 2>&1; then
    echo "error: cargo-release not found — install it (e.g. cargo install cargo-release)" >&2
    exit 1
fi

ALLOW_DIRTY=0
ARGS=()
for arg in "$@"; do
    case "$arg" in
        --allow-dirty) ALLOW_DIRTY=1 ;;
        *) ARGS+=("$arg") ;;
    esac
done

MANIFEST="Cargo.toml"
CONFIG=".cargo/config.toml"

# Snapshot the honest tree state BEFORE any masking or stripping.
START_STATUS="$(git status --porcelain)"

# Gate: refuse by default unless the caller explicitly owns the dirtiness.
if [ "$ALLOW_DIRTY" -eq 0 ] && [ -n "$START_STATUS" ]; then
    echo "error: working tree is dirty — commit or stash changes first (or pass --allow-dirty)" >&2
    echo "$START_STATUS" | sed 's/^/         /' >&2
    exit 1
fi

BACKUP_DIR="$(mktemp -d)"
cp "$MANIFEST" "$BACKUP_DIR/Cargo.toml"
cp "$CONFIG"   "$BACKUP_DIR/config.toml"

cleanup() {
    # 1. Restore the stripped files to their pre-run content.
    cp -f "$BACKUP_DIR/Cargo.toml" "$MANIFEST" 2>/dev/null || true
    cp -f "$BACKUP_DIR/config.toml" "$CONFIG" 2>/dev/null || true
    # 2. Clear the skip-worktree masks (idempotent; survives a `cargo release`
    #    failure via the EXIT trap, not a SIGKILL — see the header note).
    git update-index --no-skip-worktree "$MANIFEST" "$CONFIG" >/dev/null 2>&1 || true
    rm -rf "$BACKUP_DIR"
    # 3. Defensive end-check: the tree must be exactly as we found it. A diff
    #    is only a warning — cargo release can legitimately touch Cargo.lock
    #    during verification — but it must never go unnoticed.
    if [ "$(git status --porcelain)" != "$START_STATUS" ]; then
        echo "warning: publish-stable.sh left the tree changed vs start:" >&2
        git status --porcelain >&2
    fi
}
trap cleanup EXIT

# Mask the two files from cargo-release's clean-tree gate (libgit2 status):
# the strip below would otherwise trip the unconditional check that
# cargo-release 1.1.5's publish step enforces in execute mode. Scoped to
# exactly the files we modify; cleared by cleanup() on every exit path.
git update-index --skip-worktree "$MANIFEST" "$CONFIG"

# Strip the per-profile `rustflags` keys (the `profile-rustflags`-gated ones).
# These are the only `rustflags = [` occurrences in the manifest, and — like
# build-stable.sh — the arrays MUST stay single-line for this sed to work.
sed -i '/^rustflags = \[/d' "$MANIFEST"
# Strip the `[unstable] profile-rustflags` opt-in block from the cargo config
# (it is the file's final section; removing its two lines is sufficient).
# Note: this leaves the STRIPPED manifest with no profile rustflags, so the
# publish-time cargo build needs no nightly feature opt-in on any toolchain.
sed -i '/^\[unstable\]$/d; /^profile-rustflags = true$/d' "$CONFIG"

cargo release "${ARGS[@]}" --exclude choreo-gui