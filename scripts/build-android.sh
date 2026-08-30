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
# IMPORTANT (why the manifest gets stripped AND RUSTFLAGS is set): host-CPU
# rustflags leak into Android builds from TWO places, and each needs its own
# countermeasure:
#
# 1. The workspace Cargo.toml carries per-profile `rustflags` (nightly
#    `profile-rustflags`, including `-C target-cpu=native`). Profile rustflags
#    apply REGARDLESS of `--target` AND are not suppressed by a RUSTFLAGS env
#    var, so they must be stripped from the manifest for the duration (same
#    approach as scripts/build-stable.sh); restored on any exit.
# 2. The developer's ~/.cargo/config.toml very often sets `[build] rustflags`
#    (this machine's does: `-C target-cpu=native`) plus cfg-target rustflags
#    (here: `-fuse-ld=wild` for `cfg(all(target_os = "linux"))`, which ALSO
#    matches Android and would break the bionic link). An empty-string env
#    override is treated as unset, and `--config build.rustflags=[]` does not
#    reliably win, so the only dependable suppression is setting RUSTFLAGS
#    itself: when RUSTFLAGS is set, cargo ignores config build/target
#    rustflags entirely. `-C target-cpu=generic` is the correct baseline for
#    Android artifacts (and empty-ish alternatives like `-Cdebug-assertions=off`
#    are worse than stating the intent).
#
# Why this matters beyond correctness: nightly rustc/LLVM has been observed to
# SEGFAULT while compiling aarch64 code that carries x86 host-CPU features
# (e.g. `-C target-cpu=native` on a znver3 host), so a leak here is not just
# "wrong instructions at runtime" — the build dies mid-compile.
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
# work without extra configuration. Whatever is found is then exported as BOTH
# variables (ANDROID_NDK_HOME *and* ANDROID_HOME/ndk discovery via a version
# probe) so downstream tools — cargo-ndk, dx/gradle — agree on one NDK no
# matter which variable the user set. dx/gradle in particular do not read
# ANDROID_NDK_HOME; they only look under ANDROID_HOME/ndk, so a user who set
# only ANDROID_NDK_HOME (the common AUR layout, /opt/android-ndk) would break
# the GUI build without this normalization.
NDK_DIR=""
# Env vars first (ANDROID_NDK_HOME, ANDROID_NDK_ROOT), then the SDK-layout
# search under ANDROID_HOME, then — for fully zero-config operation — the
# common package-manager install locations (Arch/AUR: /opt/android-ndk;
# Studio SDK layout is covered by the ANDROID_HOME search below).
for candidate in "${ANDROID_NDK_HOME:-}" "${ANDROID_NDK_ROOT:-}"; do
    if [ -n "$candidate" ] && [ -d "$candidate" ]; then
        NDK_DIR="$candidate"
        break
    fi
done
if [ -z "$NDK_DIR" ] && [ -d /opt/android-ndk ]; then
    NDK_DIR=/opt/android-ndk
fi
if [ -z "$NDK_DIR" ] && [ -n "${ANDROID_HOME:-}" ] && [ -d "$ANDROID_HOME/ndk" ]; then
    # Pick the highest-versioned NDK under the SDK (sdkmanager can keep several).
    NDK_DIR="$(ls -1d "$ANDROID_HOME"/ndk/* 2>/dev/null | sort -V | tail -n1 || true)"
fi
if [ -z "$NDK_DIR" ]; then
    echo "error: Android NDK not found (this machine has none installed)." >&2
    echo "  Install one, e.g.: android sdk install ndk/latest" >&2
    echo "  and set ANDROID_NDK_HOME=<ndk-dir> (or ANDROID_NDK_ROOT, or ANDROID_HOME with ndk/<ver> inside)." >&2
    exit 1
fi
# Read the NDK's own version stamp (source.properties carries Pkg.Revision,
# e.g. "29.0.14206865") so ANDROID_HOME/ndk/<version> can be derived without
# requiring the user to have created the symlink themselves.
NDK_VERSION=""
if [ -f "$NDK_DIR/source.properties" ]; then
    NDK_VERSION="$(sed -n 's/^Pkg.Revision *= *//p' "$NDK_DIR/source.properties" | head -n1)"
fi
# ANDROID_HOME: reuse the user's if set. Otherwise auto-detect the common
# locations (Arch/AUR layout: /opt/android-sdk; Google Studio layout:
# ~/Android/Sdk) and only then fall back to the NDK's parent — this covers
# dx/gradle, which do not read ANDROID_NDK_HOME and would otherwise find
# nothing in the standalone /opt/android-ndk layout.
if [ -z "${ANDROID_HOME:-}" ]; then
    for candidate in /opt/android-sdk "$HOME/Android/Sdk"; do
        if [ -d "$candidate" ]; then ANDROID_HOME="$candidate"; break; fi
    done
    if [ -z "$ANDROID_HOME" ]; then
        ANDROID_HOME="$(dirname "$NDK_DIR")"
    fi
    export ANDROID_HOME
fi
# Layout validation (read-only — this script NEVER writes outside the
# project; anything needing root is documented, not done silently): the NDK
# found above is enough for cargo-ndk (suite binaries). The ANDROID_HOME/ndk
# symlink convention is only needed by dx/gradle (the GUI APK build, see the
# gui-android recipe); if it is missing here we note it and move on.
if [ -n "$NDK_VERSION" ] && [ -n "${ANDROID_HOME:-}" ]; then
    ndk_link="$ANDROID_HOME/ndk/$NDK_VERSION"
    if [ ! -e "$ndk_link" ]; then
        log "note: $ndk_link does not exist — fine for this script (cargo-ndk uses $NDK_DIR directly),"
        log "      but dx/gradle (gui-android) will need it. One-time root setup:"
        log "        sudo mkdir -p '$ANDROID_HOME/ndk' && sudo ln -sfn '$NDK_DIR' '$ndk_link'"
    fi
fi
export ANDROID_NDK_HOME="$NDK_DIR"
log "NDK found: $NDK_DIR (ANDROID_HOME=$ANDROID_HOME)"

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

# See the header comment: RUSTFLAGS suppresses the user-level ~/.cargo
# config.toml rustflags (build + cfg-target) that the manifest strip cannot
# reach. Exported (not per-command) so it also covers any nested cargo
# invocation cargo-ndk makes.
export RUSTFLAGS="-C target-cpu=generic"

mkdir -p dist/android
# Map each cargo-ndk ABI name to its rustup triple (for locating the linked
# binaries) — cargo-ndk's `-o` flag only copies *cdylib* artifacts into the
# output dir (verified on a real run: plain `bin` executables are left in
# target/<triple>/release/), so the executables are copied explicitly below.
abi_to_triple() {
    case "$1" in
        arm64-v8a) echo aarch64-linux-android ;;
        x86_64)    echo x86_64-linux-android ;;
        *) echo "error: unmapped ABI: $1" >&2; return 1 ;;
    esac
}
for abi in $TARGETS; do
    triple="$(abi_to_triple "$abi")"
    log "building suite binaries for $abi ($triple)"
    # cargo ndk syntax verified against `cargo ndk --help` (cargo-ndk 3.x):
    # `-t` takes the Android ABI name, cargo args follow after `--`. All four
    # bins live in the root package, so one invocation per ABI builds them
    # against shared artifacts.
    PKG_ARGS=()
    for bin in $BINS; do PKG_ARGS+=("-p" choreographr "--bin" "$bin"); done
    cargo ndk -t "$abi" -- build --release "${PKG_ARGS[@]}"
    # Copy the executables into the per-ABI output dir (see abi_to_triple note:
    # cargo-ndk does not do this for plain bins).
    mkdir -p "dist/android/$abi"
    for bin in $BINS; do
        cp "target/$triple/release/$bin" "dist/android/$abi/$bin"
    done
done

# The binaries were copied from target/<triple>/release/ above; verify the
# layout and print the push commands the Termux side needs.
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
