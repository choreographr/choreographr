#!/usr/bin/env bash
# smoke-test.sh — smoke test for a release tarball produced by
# scripts/release.sh. Extracts the tarball, checks the four binaries are
# present and executable, verifies each binary's `--version` reports the
# release version, and confirms each binary's `--help` exits 0.
#
# Why the timeout guard: the TUI/ACP/IM clients cannot run fully headless
# (they try to connect to the daemon), but --help/--version must never hang
# or crash. timeout(1) converts a hang into a 124 failure instead of wedging
# the test.
set -euo pipefail

if [ "$#" -ne 1 ]; then
    echo "usage: $0 <release-tarball>" >&2
    echo "  e.g. $0 dist/choreographr-0.1.0-x86_64-unknown-linux-musl.tar.gz" >&2
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

echo "==> checking the four binaries exist and are executable"
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
