#!/usr/bin/env bash
# scripts/build-android.sh — cross-build the four suite binaries for Android
# (Termux) via cargo-ndk.
#
# The four suite binaries (choreographr, choreo-tui, choreo-im, choreo-acp)
# are the Termux runtime targets: they run as plain executables inside a Termux
# shell, so they are built as regular cargo binaries against the Android NDK's
# bionic toolchain and pushed into `$PREFIX/bin` with adb.
#
# choreo-gui is deliberately NOT part of this script — on Android it is built
# by `dx build --platform android` as a cdylib APK payload (see the
# `gui-android` justfile recipe). Different tool, different output, different
# consumer.
#
# IMPORTANT (why the manifest gets stripped): the workspace Cargo.toml carries
# per-profile `rustflags` (nightly `profile-rustflags`, including
# `-C target-cpu=native`). Rustflags from profiles apply REGARDLESS of
# `--target`, so a native-CPU build would emit x86-64-v3 instructions that
# SIGILL on every Android device. This script strips those keys for the
# duration (same approach as scripts/build-stable.sh) and restores them on any
# exit.
#
# Modes:
#   build-android.sh               real build (aarch64 only)
#   build-android.sh --emulator    real build (aarch64 + x86_64 for the emulator)
#   build-android.sh --check       dry run: validate prerequisites, print the
#                                  commands that would run, touch nothing
#
# Prerequisites: cargo-ndk (`cargo install cargo-ndk`), an Android NDK
# (ANDROID_NDK_HOME or ANDROID_NDK_ROOT, or under ANDROID_HOME/ndk/<ver>), and
# the rustup Android targets (rustup target add aarch64-linux-android
# x86_64-linux-android).
#
# Single-authored only (not safe under concurrent runs, like build-stable.sh).
set -euo pipefail
cd "$(dirname "$0")/.."

MANIFEST="Cargo.toml"
EMULATOR=0
CHECK=0

# ── flag parsing ──────────────────────────────────────────────────────────────
# Only literal flags — no positionals, no eval, nothing that could smuggle in
# a command. Unknown flags fail loudly rather than being passed to cargo.
for arg in "$@"; do
    case "$arg" in
        --emulator) EMULATOR=1 ;;
        --check)    CHECK=1 ;;
        -h|--help)  sed -n '2,30p' "$0"; exit 0 ;;
        *) echo "error: unknown flag: $arg (supported: --emulator, --check)" >&2; exit 1 ;;
    esac
done

log() { echo "==> $*"; }

# ── prerequisite checks (run in --check mode too — that is the point) ─────────

# cargo-ndk must be on PATH; it is the only supported way to wire the NDK
# clang toolchain into a cargo cross build without hand-maintaining linkers.
if ! command -v cargo-ndk >/dev/null 2>&1; then
    echo "error: cargo-ndk not found — install it with: cargo install cargo-ndk" >&2
    exit 1
fi

# The NDK itself is what cargo-ndk drives. Search the standard env vars and the
# sdkmanager layout (ANDROID_HOME/ndk/<version>) so users of either convention
# work without extra configuration.
NDK_DIR=""
for candidate in "${ANDROID_NDK_HOME:-}" "${ANDROID_NDK_ROOT:-}"; do
    if [ -n "$candidate" ] && [ -d "$candidate" ]; then
        NDK_DIR="$candidate"
        break
    fi
done
if [ -z "$NDK_DIR" ] && [ -n "${ANDROID_HOME:-}" ] && [ -d "$ANDROID_HOME/ndk" ]; then
    # Pick the highest-versioned NDK under the SDK (sdkmanager can keep several).
    NDK_DIR="$(ls -1d "$ANDROID_HOME"/ndk/* 2>/dev/null | sort -V | tail -n1 || true)"
fi
if [ -z "$NDK_DIR" ]; then
    echo "error: Android NDK not found (this machine has none installed)." >&2
    echo "  Install one, e.g.: sdkmanager 'ndk;27.2.12479018'" >&2
    echo "  and set ANDROID_NDK_HOME=<ndk-dir> (or ANDROID_NDK_ROOT, or ANDROID_HOME with ndk/<ver> inside)." >&2
    exit 1
fi
log "NDK found: $NDK_DIR"

# The rustup targets must already be installed (they are a plain rustup add,
# so the script never installs them implicitly — an explicit opt-in).
for target in aarch64-linux-android x86_64-linux-android; do
    if ! rustup target list --installed | grep -qx "$target"; then
        echo "error: rustup target not installed: $target" >&2
        echo "  install it with: rustup target add $target" >&2
        exit 1
    fi
done

# Which triples to build. arm64-v8a covers every real device; x86_64 is only
# useful for running in the Android emulator, hence the flag.
TARGETS="arm64-v8a"
[ "$EMULATOR" = 1 ] && TARGETS="$TARGETS x86_64"

# The four suite binaries, as cargo -p/-bin names (all live in the root package).
BINS="choreographr choreo-tui choreo-im choreo-acp"

if [ "$CHECK" = 1 ]; then
    # Dry run: report what WOULD happen and exit before touching the manifest.
    # This is the script's verification path — it must be safe on any machine,
    # NDK or not is decided above, and nothing below mutates the tree.
    log "dry run — no changes made. Would build:"
    for abi in $TARGETS; do
        log "  cargo ndk -t $abi -o dist/android/$abi -- build --release \\"
        for bin in $BINS; do
            log "      -p choreographr --bin $bin"
        done
    done
    log "output layout: dist/android/<abi>/{choreographr,choreo-tui,choreo-im,choreo-acp}"
    exit 0
fi

# ── real build ────────────────────────────────────────────────────────────────

BACKUP_DIR="$(mktemp -d)"
restore() {
    cp -f "$BACKUP_DIR/Cargo.toml" "$MANIFEST"
    rm -rf "$BACKUP_DIR"
}
trap restore EXIT
cp "$MANIFEST" "$BACKUP_DIR/Cargo.toml"

# Strip the per-profile `rustflags` arrays (the nightly profile-rustflags keys,
# including -C target-cpu=native). Required for every cross build: profile
# rustflags ignore --target and would poison the Android codegen with
# host-CPU flags. These are the only `rustflags = [` occurrences in the manifest.
sed -i '/^rustflags = \[/d' "$MANIFEST"

mkdir -p dist/android
for abi in $TARGETS; do
    log "building suite binaries for $abi"
    # cargo ndk syntax verified against `cargo ndk --help` (cargo-ndk 3.x):
    # `-t` takes the Android ABI name, `-o` places the linked binaries in DIR,
    # cargo args follow after `--`. All four bins live in the root package, so
    # one invocation per ABI builds them against shared artifacts.
    PKG_ARGS=()
    for bin in $BINS; do PKG_ARGS+=("-p" choreographr "--bin" "$bin"); done
    cargo ndk -t "$abi" -o "dist/android/$abi" -- build --release "${PKG_ARGS[@]}"
done

# cargo-ndk -o already places the binaries in the output dir; make the layout
# explicit and print the push commands the Termux side needs.
log "binaries ready:"
for bin in $BINS; do
    for abi in $TARGETS; do
        src="dist/android/$abi/$bin"
        [ -f "$src" ] || { echo "error: expected binary missing: $src" >&2; exit 1; }
        echo "  $src"
    done
done

log "deploy to Termux (adb) — push to /sdcard, then copy from inside the Termux shell
(because adb cannot write directly into Termux's private app dir):"
cat <<EOF
  adb push dist/android/arm64-v8a/* /sdcard/choreo/
  # then inside the Termux shell:
  cp /sdcard/choreo/* \$PREFIX/bin/ && chmod +x \
    \$PREFIX/bin/choreographr \$PREFIX/bin/choreo-tui \$PREFIX/bin/choreo-im \$PREFIX/bin/choreo-acp
EOF
log "done"
