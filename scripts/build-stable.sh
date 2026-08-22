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
BACKUP_DIR="$(mktemp -d)"

# Restore the manifest + config exactly as we found them, then drop the backup.
restore() {
    cp -f "$BACKUP_DIR/Cargo.toml" "$MANIFEST"
    cp -f "$BACKUP_DIR/config.toml" "$CONFIG"
    rm -rf "$BACKUP_DIR"
}
trap restore EXIT

cp "$MANIFEST" "$BACKUP_DIR/Cargo.toml"
cp "$CONFIG"   "$BACKUP_DIR/config.toml"

# Strip the per-profile `rustflags` keys (the `profile-rustflags`-gated ones).
# These are the only `rustflags = [` occurrences in the manifest.
sed -i '/^rustflags = \[/d' "$MANIFEST"
# Strip the `[unstable] profile-rustflags` opt-in block from the cargo config
# (it is the file's final section; removing its two lines is sufficient).
sed -i '/^\[unstable\]$/d; /^profile-rustflags = true$/d' "$CONFIG"

cargo +stable "$@"
