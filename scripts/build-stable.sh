#!/usr/bin/env bash
# scripts/build-stable.sh — run one cargo command on STABLE Rust.
#
# The workspace defaults to the nightly toolchain (rust-toolchain.toml) so that
# every adhoc `cargo` invocation auto-applies the fast per-profile `-Z` flags.
# That wiring has a cost: the `[unstable] profile-rustflags` opt-in and the
# per-profile `rustflags` keys it enables HARD-BLOCK stable Cargo (they require
# the nightly-only `profile-rustflags` feature). This script is the supported
# escape hatch — it lets you verify/build/test on stable from the same tree.
#
# It backs up the two files that carry the nightly-only bits (the root manifest
# and the cargo config), strips exactly those keys, runs `cargo +stable "$@"`,
# and restores both files on exit (success or failure). Stable compatibility
# itself never depended on this config: the sources use no nightly features, so
# this is purely about un-blocking stable *Cargo* from the manifest/config.
#
# (Publishing has the same problem from the other side — a published manifest
# that still carries per-profile `rustflags` breaks stable `cargo install` — so
# the crates.io publish step has a sibling: scripts/publish-stable.sh.)
#
# Single-authored only (not safe under concurrent runs).
set -euo pipefail
cd "$(dirname "$0")/.."

MANIFEST="Cargo.toml"
CONFIG=".cargo/config.toml"

# Backup location — deliberately PERSISTENT (inside target/, gitignored)
# rather than a mktemp dir. The mktemp approach had a kill-safety hole: a
# hard-killed run (SIGKILL bypasses every trap, and CI-style timeouts kill
# hard) left the tree stripped AND its backup lost — so the NEXT run backed up
# the already-stripped files and later "restored" them, permanently poisoning
# the manifest/config. Observed for real: a timeout-killed build-stable.sh run
# cost the tree its [unstable] block and both profile rustflags arrays. With a
# persistent location, a killed run's backups survive and the next run
# self-heals (see below) before taking fresh backups.
BACKUP_DIR="target/.build-stable-backup"
mkdir -p "$BACKUP_DIR"

# Interrupted-run self-heal: backups still present means a previous run was
# killed after stripping but before its EXIT-trap restore. Put the originals
# back FIRST so the fresh backup below is taken from an unstripped tree.
# (If the operator edited these files between the interruption and now, those
# edits are clobbered — hence the loud warning; the alternative is a silently
# poisoned manifest, which is strictly worse.)
if [ -e "$BACKUP_DIR/Cargo.toml" ]; then
    echo "warning: $0: a previous run was interrupted before its restore completed — restoring the tree from that run's backups" >&2
    echo "warning: if you edited $MANIFEST or $CONFIG between the interruption and now, re-apply those edits (they have been overwritten)" >&2
    cp -f "$BACKUP_DIR/Cargo.toml" "$MANIFEST"
    cp -f "$BACKUP_DIR/config.toml" "$CONFIG"
fi
rm -rf "$BACKUP_DIR"
mkdir -p "$BACKUP_DIR"

# Portable in-place sed: GNU sed (Linux/CI) takes `sed -i 'expr' file`, but
# BSD sed (macOS) parses that as `-i <suffix>` and then treats the FILE
# argument as the sed SCRIPT ("invalid command code C") — the backup suffix
# must be attached (`sed -i ''`). Detect once via --version (GNU supports it,
# BSD errors) so the strips below work on both macOS and Linux.
if sed --version >/dev/null 2>&1; then
    SED_I=(sed -i)
else
    SED_I=(sed -i '')
fi

# Take the fresh backups BEFORE arming any trap, so there is no window where
# the EXIT trap could fire against missing backup files.
cp "$MANIFEST" "$BACKUP_DIR/Cargo.toml"
cp "$CONFIG"   "$BACKUP_DIR/config.toml"

# Restore the manifest + config exactly as we found them, then drop the
# backup. Defensive against being invoked with the backup dir empty (the only
# way that happens now is a kill inside the window between the two cps above):
# `if` guards rather than `[ ] &&` chains so a missing file doesn't trip
# `set -e` inside the trap.
restore() {
    if [ -f "$BACKUP_DIR/Cargo.toml" ]; then
        cp -f "$BACKUP_DIR/Cargo.toml" "$MANIFEST"
    fi
    if [ -f "$BACKUP_DIR/config.toml" ]; then
        cp -f "$BACKUP_DIR/config.toml" "$CONFIG"
    fi
    rm -rf "$BACKUP_DIR"
}
trap restore EXIT

# Signal hygiene: on Ctrl-C / TERM / HUP, exit as soon as the signal-hit cargo
# child dies, which runs the EXIT trap (restore). Without these, bash defers
# trap handling until the foreground child finishes on its own. SIGKILL cannot
# be trapped by anyone — the startup self-heal is the safety net for that case.
trap 'exit 130' INT
trap 'exit 143' TERM HUP

# Strip the per-profile `rustflags` keys (the `profile-rustflags`-gated ones).
# These are the only `rustflags = [` occurrences in the manifest.
"${SED_I[@]}" '/^rustflags = \[/d' "$MANIFEST"
# Strip the `[unstable] profile-rustflags` opt-in block from the cargo config
# (it is the file's final section; removing its two lines is sufficient).
"${SED_I[@]}" '/^\[unstable\]$/d; /^profile-rustflags = true$/d' "$CONFIG"

cargo +stable "$@"
