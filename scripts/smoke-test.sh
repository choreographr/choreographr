#!/usr/bin/env bash
# smoke-test.sh — smoke test for a release tarball produced by
# scripts/release.sh. Extracts the tarball, checks the four binaries are
# present and executable, verifies `choreographr --version` reports the
# release version, and confirms the clap clients' --help exits 0.
#
# Why the timeout guard: the TUI/ACP clients cannot run fully headless (they
# try to connect to the daemon), but --help must never hang or crash.
# timeout(1) converts a hang into a 124 failure instead of wedging the test.
#
# choreo-im special case: it has NO clap CLI — it does not support --help or
# --version (pre-existing behavior; it requires a running daemon). We only
# assert that the binary exists and is executable, and never run it here.
set -euo pipefail

if [ "$#" -ne 1 ]; then
    echo "usage: $0 <release-tarball>" >&2
    echo "  e.g. $0 dist/choreographr-0.1.0-x86_64-unknown-linux-gnu.tar.gz" >&2
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

echo "==> choreographr --version (expect $VERSION)"
VER_OUT="$(run_guarded "$TMP/choreographr" --version 2>&1)" || {
    echo "  FAIL: choreographr --version exited non-zero" >&2
    FAIL=1
}
case "$VER_OUT" in
    *"$VERSION"*) echo "  ok: reports $VERSION" ;;
    *)
        echo "  FAIL: --version output did not contain $VERSION: $(printf '%s' "$VER_OUT" | head -c 200)" >&2
        FAIL=1
        ;;
esac

# choreo-im: its existence/executable check above is the complete assertion —
# it has no clap CLI, so a --help/--version run would just hang waiting for
# the daemon it requires. Only the clap clients get a --help run (exit 0).
echo "  note: choreo-im has no clap CLI — existence/executable check only"
echo "==> clap clients: --help must exit 0 without hanging"
for b in choreo-tui choreo-acp; do
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
