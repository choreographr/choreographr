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
# Kill-safety (same SIGKILL hazard class as the pre-hardening build-stable.sh,
# fixed the same way): the strip + skip-worktree masking is NOT atomic with the
# restore. A hard-killed run (SIGKILL bypasses every trap; CI-style timeouts
# kill hard) could leave TWO kinds of residue:
#
#   1. Stripped files + lost backup. A mktemp backup dies with /tmp cleanup and
#      gave no warning — the NEXT run would back up the already-stripped files
#      and later "restore" them, permanently poisoning the manifest/config
#      (this actually happened to build-stable.sh twice before its hardening).
#      Fix: backups live in a PERSISTENT, gitignored dir under target/ and the
#      next run's startup self-heal restores from them before taking fresh ones.
#   2. Stale skip-worktree marks. The marks hide real future changes to these
#      two files from `git status` FOREVER — a poisoned tree that looks clean.
#      The self-heal clears any stale marks on these two files at startup (this
#      script is their only legitimate setter, so a set mark here can only
#      mean an interrupted prior run).
#
# Motivation for the fix: publish runs take long enough (workspace `cargo
# release` verification) that a CI/operator timeout mid-publish is realistic.
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
# (Cleared stale marks from a killed run would otherwise make this status lie:
# a stripped file with a stale skip-worktree mark reports as clean.)
git update-index --no-skip-worktree "$MANIFEST" "$CONFIG" >/dev/null 2>&1 || true
START_STATUS="$(git status --porcelain)"

# Gate: refuse by default unless the caller explicitly owns the dirtiness.
if [ "$ALLOW_DIRTY" -eq 0 ] && [ -n "$START_STATUS" ]; then
    echo "error: working tree is dirty — commit or stash changes first (or pass --allow-dirty)" >&2
    echo "$START_STATUS" | sed 's/^/         /' >&2
    exit 1
fi

# Backup location — deliberately PERSISTENT (inside target/, gitignored)
# rather than a mktemp dir. Same reasoning as build-stable.sh: a mktemp backup
# is lost with a hard kill, so the next run would back up the already-stripped
# files and "restore" them — the poisoning failure mode, observed for real on
# the build-stable.sh side. With a persistent location, a killed run's backups
# survive and the next run self-heals (see below) before taking fresh backups.
BACKUP_DIR="target/.publish-stable-backup"
mkdir -p "$BACKUP_DIR"

# Interrupted-run self-heal: backups still present means a previous run was
# killed after stripping but before its EXIT-trap restore. Put the originals
# back FIRST so the fresh backup below is taken from an unstripped tree.
# (If the operator edited these files between the interruption and now, those
# edits are clobbered — hence the loud warning; the alternative is a silently
# poisoned manifest, which is strictly worse. The stale skip-worktree marks
# were already cleared above, before the honest status snapshot.)
if [ -e "$BACKUP_DIR/Cargo.toml" ]; then
    echo "warning: $0: a previous run was interrupted before its restore completed — restoring the tree from that run's backups" >&2
    echo "warning: if you edited $MANIFEST or $CONFIG between the interruption and now, re-apply those edits (they have been overwritten)" >&2
    cp -f "$BACKUP_DIR/Cargo.toml" "$MANIFEST"
    cp -f "$BACKUP_DIR/config.toml" "$CONFIG"
fi
rm -rf "$BACKUP_DIR"
mkdir -p "$BACKUP_DIR"

# Take the fresh backups BEFORE arming any trap, so there is no window where
# the EXIT trap could fire against missing backup files.
cp "$MANIFEST" "$BACKUP_DIR/Cargo.toml"
cp "$CONFIG"   "$BACKUP_DIR/config.toml"

cleanup() {
    # 1. Restore the stripped files to their pre-run content. `if` guards
    #    rather than `[ ] &&` chains so a missing file doesn't trip `set -e`
    #    inside the trap (the only way a backup is missing now is a kill in
    #    the window between the two cps above).
    if [ -f "$BACKUP_DIR/Cargo.toml" ]; then
        cp -f "$BACKUP_DIR/Cargo.toml" "$MANIFEST"
    fi
    if [ -f "$BACKUP_DIR/config.toml" ]; then
        cp -f "$BACKUP_DIR/config.toml" "$CONFIG"
    fi
    # 2. Clear the skip-worktree masks (idempotent; survives a `cargo release`
    #    failure via the EXIT trap. A SIGKILL bypasses this — the startup
    #    self-heal is the safety net for that case, see the header note).
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

# Signal hygiene: on Ctrl-C / TERM / HUP, exit as soon as the signal-hit cargo
# child dies, which runs the EXIT trap (cleanup). Without these, bash defers
# trap handling until the foreground child finishes on its own. SIGKILL cannot
# be trapped by anyone — the startup self-heal is the safety net for that case.
trap 'exit 130' INT
trap 'exit 143' TERM HUP

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