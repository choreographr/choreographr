#!/usr/bin/env bash
# smoke-test.sh — smoke test for a release artifact produced by the release
# pipeline. Two artifact kinds, dispatched on the filename suffix:
#
#   *.tar.gz  — scripts/release.sh's release tarball: extracts it, checks the
#               four binaries are present and executable, verifies each
#               binary's `--version` reports the release version, and confirms
#               each binary's `--help` exits 0.
#   *.deb     — scripts/build-deb-termux.sh's Termux package: STRUCTURAL
#               validation only (control fields, file list, exec bits) — there
#               is no Termux on the host, so the Android binaries inside cannot
#               be executed here; the tarball path is where --version/--help
#               get exercised, and on-device install is the user's smoke test.
#
# Why the timeout guard: the TUI/ACP/IM clients cannot run fully headless
# (they try to connect to the daemon), but --help/--version must never hang
# or crash. timeout(1) converts a hang into a 124 failure instead of wedging
# the test.
set -euo pipefail

if [ "$#" -ne 1 ]; then
    echo "usage: $0 <release-artifact>" >&2
    echo "  e.g. $0 dist/choreographr-0.1.0-x86_64-unknown-linux-musl.tar.gz" >&2
    echo "       $0 dist/choreographr-termux_0.1.0_aarch64.deb" >&2
    exit 1
fi
TARBALL="$1"
[ -f "$TARBALL" ] || { echo "error: tarball not found: $TARBALL" >&2; exit 1; }

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
# The release version lives in the workspace manifest; the fallback keeps the
# assertion meaningful if the script is run outside the repo checkout.
VERSION="$(sed -n 's/^version = "\(.*\)"/\1/p' "$REPO_ROOT/Cargo.toml" | head -n1)"
VERSION="${VERSION:-0.1.0}"

BINARIES=(choreographr choreo-tui choreo-im choreo-acp)
FAIL=0

# ── Termux .deb path (structural only — see the header comment) ──────────────
# Runs on the CI android job (ubuntu: dpkg-deb preinstalled) and locally; it
# validates whatever .deb is handed to it, so it also catches a partially
# rebuilt or hand-edited package, not just a freshly built one.
if [[ "$TARBALL" == *.deb ]]; then
    command -v dpkg-deb >/dev/null 2>&1 || {
        echo "error: dpkg-deb not found — needed to inspect $TARBALL (pacman -S dpkg)" >&2
        exit 1
    }
    INFO="$(dpkg-deb --info "$TARBALL")"
    CONTENTS="$(dpkg-deb --contents "$TARBALL")"

    echo "==> checking control fields"
    # dpkg-deb --info indents the verbatim control dump by one space.
    for field in \
        "Package: choreographr" \
        "Version: $VERSION" \
        "Architecture: aarch64"; do   # Termux's tag — NOT Debian's arm64
        if grep -q "^ $field\$" <<<"$INFO"; then
            echo "  ok: $field"
        else
            echo "  FAIL: control field missing/wrong: $field" >&2
            FAIL=1
        fi
    done
    if grep -q '^ Depends:' <<<"$INFO"; then
        echo "  FAIL: must not declare Depends (static bionic binaries)" >&2
        FAIL=1
    else
        echo "  ok: no Depends"
    fi
    # Archive members must be xz, not zstd: Termux's dpkg has no zstd support
    # (it searches control.tar{xz,lzma,} only), and dpkg >= 1.22 build hosts
    # default to zstd — exactly the failure seen on-device 2026-09-02.
    MEMBERS="$(ar t "$TARBALL")"
    if [ "$MEMBERS" = "$(printf 'debian-binary\ncontrol.tar.xz\ndata.tar.xz')" ]; then
        echo "  ok: members are xz (debian-binary + control.tar.xz + data.tar.xz)"
    else
        echo "  FAIL: archive members not the Termux-compatible xz set: $MEMBERS" >&2
        FAIL=1
    fi
    # No maintainer scripts / conffiles: the control archive holds only the
    # control file itself (plus tar's "./" root entry).
    CTRL_TAR="$(dpkg-deb --ctrl-tarfile "$TARBALL" | tar -t)"
    CTRL_BAD=0
    for entry in $CTRL_TAR; do
        case "$entry" in ./|./control) ;; *) CTRL_BAD=1 ;; esac
    done
    if [ "$CTRL_BAD" -eq 0 ]; then
        echo "  ok: no maintainer scripts or conffiles"
    else
        echo "  FAIL: unexpected control-archive entries (postinst/conffiles?):" \
            "$CTRL_TAR" >&2
        FAIL=1
    fi

    echo "==> checking contents (the four binaries at ./bin/, mode 0755)"
    for b in "${BINARIES[@]}"; do
        if grep -q "^-rwxr-xr-x .* ./bin/$b\$" <<<"$CONTENTS"; then
            echo "  ok: ./bin/$b (0755)"
        else
            echo "  FAIL: ./bin/$b missing, wrong mode, or not executable" >&2
            FAIL=1
        fi
    done

    if [ "$FAIL" -ne 0 ]; then
        echo "SMOKE TEST FAILED" >&2
        exit 1
    fi
    echo "SMOKE TEST PASSED"
    exit 0
fi

# timeout(1) is not on every platform (macOS lacks it by default); when it is
# present, use it as a 5s guard on every binary invocation.
if command -v timeout >/dev/null 2>&1; then
    run_guarded() { timeout 5 "$@"; }
else
    run_guarded() { "$@"; }
fi

TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

echo "==> extracting $TARBALL"
tar -xzf "$TARBALL" -C "$TMP"

echo "==> checking the four binaries exist and are executable (tarball path)"
for b in "${BINARIES[@]}"; do
    if [ -x "$TMP/$b" ]; then
        echo "  ok: $b"
    else
        echo "  FAIL: $b missing or not executable" >&2
        FAIL=1
    fi
done

echo "==> --version must report $VERSION (all four binaries)"
for b in "${BINARIES[@]}"; do
    VER_OUT="$(run_guarded "$TMP/$b" --version 2>&1)" || {
        echo "  FAIL: $b --version exited non-zero" >&2
        FAIL=1
        continue
    }
    case "$VER_OUT" in
        *"$VERSION"*) echo "  ok: $b --version reports $VERSION" ;;
        *)
            echo "  FAIL: $b --version output did not contain $VERSION: $(printf '%s' "$VER_OUT" | head -c 200)" >&2
            FAIL=1
            ;;
    esac
done

# All four binaries are clap clients, so --help is safe on every one of
# them: clap handles it before any daemon connection attempt.
echo "==> --help must exit 0 without hanging (all four binaries)"
for b in "${BINARIES[@]}"; do
    if run_guarded "$TMP/$b" --help >/dev/null 2>&1; then
        echo "  ok: $b --help"
    else
        echo "  FAIL: $b --help exited non-zero (or timed out)" >&2
        FAIL=1
    fi
done

if [ "$FAIL" -ne 0 ]; then
    echo "SMOKE TEST FAILED" >&2
    exit 1
fi
echo "SMOKE TEST PASSED"
