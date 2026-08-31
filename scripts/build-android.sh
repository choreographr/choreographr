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
#   build-android.sh                    real build (aarch64 only)
#   build-android.sh --emulator         real build (aarch64 + x86_64 for the emulator)
#   build-android.sh --check            dry run: validate prerequisites, print the
#                                       commands that would run, touch nothing
#   --features <comma-list>             append `--features <list>` to the build —
#                                       release CI passes `metrics,blockchain` so
#                                       the Termux binaries match the desktop
#                                       release binaries (scripts/release.sh);
#                                       local deploys default to no features
#
# Like scripts/release.sh, the real build runs on the STABLE toolchain
# (RUSTUP_TOOLCHAIN=stable overrides the workspace's rust-toolchain.toml
# nightly pin for the outer `cargo ndk` AND the nested `cargo build` it
# spawns) — reproducible release artifacts, consistent with the desktop
# binaries. The nightly-only `[unstable] profile-rustflags` config block and
# per-profile `rustflags` that hard-block stable Cargo are stripped from
# Cargo.toml AND .cargo/config.toml for the duration (same approach as
# scripts/build-stable.sh) and restored on any exit.
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
CONFIG=".cargo/config.toml"
EMULATOR=0
CHECK=0
FEATURES=""

# ── flag parsing ──────────────────────────────────────────────────────────────
# Only literal flags — no positionals, no eval, nothing that could smuggle in
# a command. Unknown flags fail loudly rather than being passed to cargo.
# `--features` takes a comma-separated cargo feature list from the next argv
# slot (shifted off) and is forwarded verbatim as one `--features <list>` arg;
# it is NOT validated here — an unknown feature must fail as a normal cargo
# feature error, not as a parser guess.
while [ "$#" -gt 0 ]; do
    case "$1" in
        --emulator) EMULATOR=1 ;;
        --check)    CHECK=1 ;;
        --features)
            [ "$#" -ge 2 ] || { echo "error: --features requires a value" >&2; exit 1; }
            FEATURES="$2"
            shift ;;
        -h|--help)  sed -n '2,30p' "$0"; exit 0 ;;
        *) echo "error: unknown flag: $1 (supported: --emulator, --check, --features)" >&2; exit 1 ;;
    esac
    shift
done

# ── stable toolchain policy ───────────────────────────────────────────────────
# Exported BEFORE the prerequisite checks so `rustup target list --installed`
# below resolves against stable (the target check must match the toolchain
# the build will actually use). See the header comment for why stable and how
# the nightly-only config bits are neutralized.
export RUSTUP_TOOLCHAIN=stable

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
# ANDROID_HOME: reuse the user's if set. Otherwise prefer the standard
# user-owned SDK locations (Google's own default is ~/Android/Sdk — gradle
# and `android sdk install` write there, which is why /opt package trees
# must NOT be used as ANDROID_HOME even when present) and only then fall
# back to package-manager layouts. This covers dx/gradle, which do not read
# ANDROID_NDK_HOME.
if [ -z "${ANDROID_HOME:-}" ]; then
    for candidate in "$HOME/Android/Sdk" /opt/android-sdk; do
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
# so the script never installs them implicitly — an explicit opt-in). Only the
# targets that will actually be built are required: aarch64 always (every real
# device), x86_64 only for the emulator flavor — CI installs just the aarch64
# target, and demanding an unused target there would hard-fail the job. The
# list resolves against the STABLE toolchain selected above, not the
# workspace's nightly pin.
targets_to_check="aarch64-linux-android"
[ "$EMULATOR" = 1 ] && targets_to_check="$targets_to_check x86_64-linux-android"
for target in $targets_to_check; do
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
    log "dry run — no changes made. Would build (on the stable toolchain):"
    for abi in $TARGETS; do
        log "  cargo ndk -t $abi -o dist/android/$abi -- build --locked --profile dist \\"
        [ -n "$FEATURES" ] && log "      --features $FEATURES \\"
        for bin in $BINS; do
            log "      -p choreographr --bin $bin"
        done
    done
    log "output layout: dist/android/<abi>/{choreographr,choreo-tui,choreo-im,choreo-acp}"
    exit 0
fi

# ── real build ────────────────────────────────────────────────────────────────

# ── portable in-place sed ───────────────────────────────────────────────────
# The manifest/config strips below need in-place editing. GNU sed (Linux CI)
# takes `sed -i 'expr' file`; BSD sed (macOS) parses that as `-i <suffix>` and
# then eats the FILE argument as the sed SCRIPT ("invalid command code C"),
# so the backup suffix must be attached (`sed -i ''`). Detect once via
# --version (GNU supports it, BSD errors) and build the invocation prefix.
if sed --version >/dev/null 2>&1; then
    SED_I=(sed -i)
else
    SED_I=(sed -i '')
fi

# Backup location — persistent (target/ is gitignored), same kill-safety
# rationale as scripts/build-stable.sh: a hard-killed run (CI-style timeout,
# SIGKILL) bypasses the EXIT trap, and with a mktemp backup dir that meant the
# next run would back up the ALREADY-STRIPPED files and "restore" them —
# permanently poisoning the manifest/config. A persistent dir lets the next
# run self-heal (below) from the killed run's surviving backups.
BACKUP_DIR="target/.build-android-backup"
mkdir -p "$BACKUP_DIR"

# Interrupted-run self-heal (same semantics as build-stable.sh): backups still
# present means the previous run died after stripping, before restoring.
if [ -e "$BACKUP_DIR/Cargo.toml" ]; then
    echo "warning: $0: a previous run was interrupted before its restore completed — restoring the tree from that run's backups" >&2
    echo "warning: if you edited $MANIFEST or $CONFIG between the interruption and now, re-apply those edits (they have been overwritten)" >&2
    cp -f "$BACKUP_DIR/Cargo.toml" "$MANIFEST"
    cp -f "$BACKUP_DIR/config.toml" "$CONFIG"
fi
rm -rf "$BACKUP_DIR"
mkdir -p "$BACKUP_DIR"

# Fresh backups BEFORE arming traps — no window where the EXIT trap fires
# against missing backup files.
cp "$MANIFEST" "$BACKUP_DIR/Cargo.toml"
cp "$CONFIG" "$BACKUP_DIR/config.toml"

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

# Signal hygiene (same as build-stable.sh): Ctrl-C / TERM / HUP exit through
# the EXIT trap (restore) as soon as the signal-hit cargo dies. SIGKILL can't
# be trapped — the startup self-heal covers that case.
trap 'exit 130' INT
trap 'exit 143' TERM HUP

# Strip the per-profile `rustflags` arrays (the nightly profile-rustflags keys,
# including -C target-cpu=native). Required for every cross build: profile
# rustflags ignore --target and would poison the Android codegen with
# host-CPU flags. These are the only `rustflags = [` occurrences in the manifest.
"${SED_I[@]}" '/^rustflags = \[/d' "$MANIFEST"

# Also strip the `[unstable] profile-rustflags` opt-in from the cargo config —
# this is the bit that HARD-BLOCKS stable Cargo (stable errors out at config
# parse time; see scripts/build-stable.sh). The block is the config file's
# final section, so removing its two lines is sufficient.
"${SED_I[@]}" '/^\[unstable\]$/d; /^profile-rustflags = true$/d' "$CONFIG"

# See the header comment: RUSTFLAGS suppresses the user-level ~/.cargo
# config.toml rustflags (build + cfg-target) that the manifest strip cannot
# reach. Exported (not per-command) so it also covers any nested cargo
# invocation cargo-ndk makes.
export RUSTFLAGS="-C target-cpu=generic"

mkdir -p dist/android
# Map each cargo-ndk ABI name to its rustup triple (for locating the linked
# binaries) — cargo-ndk's `-o` flag only copies *cdylib* artifacts into the
# output dir (verified on a real run: plain `bin` executables are left in
# target/<triple>/<profile>/), so the executables are copied explicitly below
# — from target/<triple>/dist/ now that the build uses --profile dist.
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
    # --locked mirrors scripts/release.sh: the committed Cargo.lock is
    # authoritative for release artifacts (supply-chain control — a silent
    # lockfile regeneration must never repick semver-compatible versions).
    # FEATURE_ARGS stays empty without --features, so local zero-flag Termux
    # deploys keep building the default feature set exactly as before.
    FEATURE_ARGS=()
    [ -n "$FEATURES" ] && FEATURE_ARGS+=("--features" "$FEATURES")
    # --profile dist matches the desktop release pipeline (release.sh): the
    # shipped Termux binaries come from the same [profile.dist] profile, and
    # land in target/<triple>/dist rather than target/<triple>/release.
    cargo ndk -t "$abi" -- build --locked --profile dist "${FEATURE_ARGS[@]}" "${PKG_ARGS[@]}"
    # Copy the executables into the per-ABI output dir (see abi_to_triple note:
    # cargo-ndk does not do this for plain bins).
    mkdir -p "dist/android/$abi"
    for bin in $BINS; do
        cp "target/$triple/dist/$bin" "dist/android/$abi/$bin"
    done
done

# The binaries were copied from target/<triple>/dist/ above; verify the
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
