#!/usr/bin/env bash
# update-homebrew-tap.sh — bump the choreographr/homebrew-choreographr tap
# formula to the release version in the workspace Cargo.toml, recompute the
# macOS tarball digests from dist/, and (only with --push) commit + push to
# the tap repo. Dry-run by default: clones the tap, rewrites the formula,
# validates it, and prints the diff — the remote is never touched.
#
# Why a script instead of GitHub Actions: this repo has no CI by design
# (RELEASE.md — every release step runs on owned machines; GitHub is only
# artifact hosting). The tap bump is therefore a release-script step on the
# Linux box, reusing the tarballs already staged in dist/ by Phase 4. The
# digests are computed locally from the exact artifacts that were uploaded —
# no re-download, no trust in a third-party service.
#
# Usage:
#   scripts/update-homebrew-tap.sh              # dry-run: diff, no push
#   scripts/update-homebrew-tap.sh --push       # commit + push to the tap
#
# Env:
#   CHOREOGRAPH_TAP_REMOTE   override the tap remote (default: SSH URL of
#                            choreographr/homebrew-choreographr). Useful for
#                            testing against a local bare repo.
#
# Requires: git, sha256sum (or shasum -a 256), sed, grep. Optional: ruby
# (formula syntax check), gh (release-existence check).
#
# The one step this cannot do is the Homebrew sanity check
# (`brew install ./choreographr.rb && choreographr --version`) — that needs a
# real Homebrew and stays a MacBook step (the formula's `service do` block is
# macOS-only).
set -euo pipefail

usage() {
    cat <<'EOF'
Usage: scripts/update-homebrew-tap.sh [--push] [--help]

Dry-run by default: clones choreographr/homebrew-choreographr, bumps the
formula to the version in Cargo.toml, recomputes both sha256 digests from
the macOS tarballs in dist/, validates, and prints the diff. Never touches
the remote unless --push is passed.

  --push   commit the bump and push to the tap repo's default branch
  --help   show this help

Env:
  CHOREOGRAPH_TAP_REMOTE   tap remote to clone/push (default: the SSH URL of
                           choreographr/homebrew-choreographr)
EOF
}

PUSH=0
for arg in "$@"; do
    case "$arg" in
        --push) PUSH=1 ;;
        --help|-h) usage; exit 0 ;;
        *) echo "error: unknown option: $arg" >&2; usage >&2; exit 1 ;;
    esac
done

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$REPO_ROOT"

# Version comes from the workspace manifest — the single source of truth that
# scripts/release.sh, the Homebrew formula, and the AUR PKGBUILD mirror.
VERSION="$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -n1)"
[ -n "$VERSION" ] || { echo "error: could not read version from Cargo.toml" >&2; exit 1; }
# The version is interpolated into sed/grep patterns below; pin the format so
# a malformed manifest aborts here instead of corrupting the formula silently.
if ! [[ "$VERSION" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]]; then
    echo "error: unexpected version format in Cargo.toml: $VERSION" >&2
    exit 1
fi

TAP_REMOTE="${CHOREOGRAPH_TAP_REMOTE:-git@github.com:choreographr/homebrew-choreographr.git}"

# sha256sum is GNU coreutils; macOS ships `shasum -a 256` instead. Either is
# fine — the script is designed for the Linux box but should not hard-require
# GNU tools.
if command -v sha256sum >/dev/null 2>&1; then
    SHA256=(sha256sum)
elif command -v shasum >/dev/null 2>&1; then
    SHA256=(shasum -a 256)
else
    echo "error: need sha256sum or shasum on PATH" >&2
    exit 1
fi

# No `sed -i`: GNU and BSD disagree on the backup-suffix argument (GNU rejects
# an empty one, BSD requires it). Rewrites go through a temp file + mv instead,
# which behaves identically everywhere.

# ── macOS tarballs in dist/ ──────────────────────────────────────────────────
# The formula downloads the macOS tarball from the GitHub release, so the
# arm64 tarball is REQUIRED — Phase 4 staged it here via scp from the
# MacBook. The x86_64-apple-darwin tarball is not shipped yet (the formula's
# else-branch is a documented placeholder for future Intel support); when it
# exists its digest is bumped too, otherwise the branch is left untouched.
ARM64_TARBALL="dist/choreographr-${VERSION}-aarch64-apple-darwin.tar.gz"
if [ ! -f "$ARM64_TARBALL" ]; then
    echo "error: $ARM64_TARBALL not found — the formula cannot point at a release asset" >&2
    echo "error: build it on the MacBook (just release) and scp it to dist/ (RELEASE.md Phase 4)" >&2
    exit 1
fi

ARM64_SHA="$("${SHA256[@]}" "$ARM64_TARBALL" | awk '{print $1}')"
echo "==> arm64 tarball   $ARM64_TARBALL   (sha256 ${ARM64_SHA:0:12}…)"

X86_64_TARBALL="dist/choreographr-${VERSION}-x86_64-apple-darwin.tar.gz"
X86_64_SHA=""
if [ -f "$X86_64_TARBALL" ]; then
    X86_64_SHA="$("${SHA256[@]}" "$X86_64_TARBALL" | awk '{print $1}')"
    echo "==> x86_64 tarball $X86_64_TARBALL   (sha256 ${X86_64_SHA:0:12}…)"
else
    echo "==> no $X86_64_TARBALL — leaving the formula's x86_64 branch untouched"
fi

# A digest field that is not exactly 64 lowercase hex would fail brew install;
# reject it up front rather than writing a broken formula.
[[ "$ARM64_SHA" =~ ^[0-9a-f]{64}$ ]] || { echo "error: arm64 digest is not 64 hex chars: $ARM64_SHA" >&2; exit 1; }
if [ -n "$X86_64_SHA" ]; then
    [[ "$X86_64_SHA" =~ ^[0-9a-f]{64}$ ]] || { echo "error: x86_64 digest is not 64 hex chars: $X86_64_SHA" >&2; exit 1; }
fi

# ── best-effort: the release exists on GitHub ────────────────────────────────
# The formula URL must resolve; confirm the release is live before pointing
# the tap at it. Best-effort only: gh is release tooling, not a hard
# requirement of this script (an unauthenticated gh must not block the bump).
if command -v gh >/dev/null 2>&1; then
    if gh release view "v${VERSION}" >/dev/null 2>&1; then
        echo "==> confirmed GitHub release v${VERSION} exists"
    else
        echo "warning: could not confirm release v${VERSION} via gh — is gh installed/authenticated?" >&2
        echo "warning: the formula URL must resolve before brew install can work" >&2
    fi
else
    echo "warning: gh not found — skipping release-existence check" >&2
fi

# ── clone the tap repo ───────────────────────────────────────────────────────
TAP_DIR="$(mktemp -d)"
trap 'rm -rf "$TAP_DIR"' EXIT
echo "==> cloning tap from $TAP_REMOTE"
git clone -q "$TAP_REMOTE" "$TAP_DIR" || {
    echo "error: could not clone $TAP_REMOTE — does the tap repo exist?" >&2
    echo "error: create choreographr/homebrew-choreographr (with Formula/choreographr.rb) first" >&2
    exit 1
}
FORMULA="$TAP_DIR/Formula/choreographr.rb"
if [ ! -f "$FORMULA" ]; then
    echo "error: $FORMULA not found in the tap — expected layout Formula/choreographr.rb" >&2
    exit 1
fi

# The current version inside the tap formula. Every replacement below is
# keyed off the OLD strings with an exact-count assertion, so an unexpected
# formula structure aborts with a count mismatch instead of silently
# corrupting the file.
OLD_VERSION="$(sed -n 's/^  version "\([^"]*\)"/\1/p' "$FORMULA" | head -n1)"
[ -n "$OLD_VERSION" ] || { echo "error: could not read version from $FORMULA" >&2; exit 1; }

if [ "$OLD_VERSION" = "$VERSION" ]; then
    echo "==> tap formula already at v${VERSION} — nothing to do"
    exit 0
fi
echo "==> tap formula at v${OLD_VERSION} — bumping to v${VERSION}"

# The old digests are needed as literal needles (the arm64 one is a
# "<sha256-aarch64>" placeholder until the first real release).
OLD_ARM64_SHA="$(sed -n '/if Hardware::CPU.arm?/,/^  else/ s/^    sha256 "\([^"]*\)"/\1/p' "$FORMULA" | head -n1)"
OLD_X86_64_SHA="$(sed -n '/^  else/,/^  end/ s/^    sha256 "\([^"]*\)"/\1/p' "$FORMULA" | head -n1)"
[ -n "$OLD_ARM64_SHA" ] || { echo "error: could not read arm64 sha256 from $FORMULA" >&2; exit 1; }
[ -n "$OLD_X86_64_SHA" ] || { echo "error: could not read x86_64 sha256 from $FORMULA" >&2; exit 1; }

# ── rewrite ──────────────────────────────────────────────────────────────────
# replace_literal <needle> <replacement> <expected_count> <what>
# Replaces every occurrence of the literal <needle> with <replacement>, but
# only if it appears exactly <expected_count> times. A count mismatch means
# the formula structure has drifted and the rewrite would corrupt it, so we
# abort instead of guessing. Safe for version strings/digests: the needles
# carry only [0-9A-Za-z.<>"-], none of which are special to sed's s||| form
# (no |, &, or backslashes can appear).
replace_literal() {
    local needle="$1" replacement="$2" expected="$3" what="$4"
    local count
    count="$(grep -c -F "$needle" "$FORMULA" || true)"
    if [ "$count" -ne "$expected" ]; then
        echo "error: expected ${expected} occurrence(s) of '${needle}' (${what}), found ${count} — formula drifted?" >&2
        exit 1
    fi
    # In-place without -i: write the transformed formula to a temp file beside
    # it and atomically move it over the original.
    sed "s|$needle|$replacement|g" "$FORMULA" > "$FORMULA.tmp"
    mv "$FORMULA.tmp" "$FORMULA"
}

echo "==> rewriting $FORMULA"

# 1. The class-level `version "X.Y.Z"` line (exactly one in the formula).
replace_literal "version \"$OLD_VERSION\"" "version \"$VERSION\"" 1 "version line"
# 2. The tag inside both download URLs: /download/vOLD/ → /download/vNEW/.
replace_literal "v$OLD_VERSION/" "v$VERSION/" 2 "url tag"
# 3. The embedded version inside both tarball filenames.
replace_literal "choreographr-$OLD_VERSION-" "choreographr-$VERSION-" 2 "url filename"
# 4. The arm64 digest (the sha256 line inside the `if Hardware::CPU.arm?` block).
replace_literal "sha256 \"$OLD_ARM64_SHA\"" "sha256 \"$ARM64_SHA\"" 1 "arm64 digest"
# 5. The x86_64 digest (the sha256 line in the `else` branch) — only when the
#    tarball exists; otherwise the documented placeholder stays in place.
if [ -n "$X86_64_SHA" ]; then
    replace_literal "sha256 \"$OLD_X86_64_SHA\"" "sha256 \"$X86_64_SHA\"" 1 "x86_64 digest"
fi

# ── post-verification: every field must now be exactly what we expect ────────
echo "==> verifying rewritten formula"

ARM64_URL="https://github.com/choreographr/choreographr/releases/download/v${VERSION}/choreographr-${VERSION}-aarch64-apple-darwin.tar.gz"
X86_64_URL="https://github.com/choreographr/choreographr/releases/download/v${VERSION}/choreographr-${VERSION}-x86_64-apple-darwin.tar.gz"

verify_count() { # <needle> <expected> <what>
    local count
    count="$(grep -c -F "$1" "$FORMULA" || true)"
    [ "$count" -eq "$2" ] || {
        echo "error: post-check failed — expected $2 occurrence(s) of '$1' (${3}), found $count" >&2
        exit 1
    }
}

verify_count "version \"$VERSION\"" 1 "version line"
verify_count "$ARM64_URL" 1 "arm64 url"
verify_count "$X86_64_URL" 1 "x86_64 url"
verify_count "sha256 \"$ARM64_SHA\"" 1 "arm64 digest"
if [ -n "$X86_64_SHA" ]; then
    verify_count "sha256 \"$X86_64_SHA\"" 1 "x86_64 digest"
else
    # No x86_64 tarball this release: the branch must be untouched, i.e. its
    # digest still matches what we read before the rewrite.
    verify_count "sha256 \"$OLD_X86_64_SHA\"" 1 "x86_64 digest (unchanged)"
fi
# Exactly two sha256 fields must remain (arm64 + x86_64 branches).
verify_count 'sha256 "' 2 "sha256 fields"
# No stale version references may survive in the URL lines.
verify_count "v$OLD_VERSION/" 0 "stale url tag"
verify_count "choreographr-$OLD_VERSION-" 0 "stale url filename"
# The arm64 placeholder must be gone (it would break checksum verification).
verify_count '<sha256-aarch64>' 0 "arm64 placeholder"

# Syntax-check the Ruby when ruby is available; it often is not on the Linux
# box, so warn and continue rather than block.
if command -v ruby >/dev/null 2>&1; then
    if ruby -c "$FORMULA" >/dev/null 2>&1; then
        echo "==> ruby syntax check passed"
    else
        echo "error: ruby syntax check failed on $FORMULA" >&2
        exit 1
    fi
else
    echo "warning: ruby not found — skipping formula syntax check" >&2
fi

# ── show the change, then commit/push only on --push ─────────────────────────
echo
echo "==> formula changes (tap repo):"
git -C "$TAP_DIR" --no-pager diff

if [ "$PUSH" -eq 1 ]; then
    TAP_BRANCH="$(git -C "$TAP_DIR" branch --show-current)"
    git -C "$TAP_DIR" add Formula/choreographr.rb
    git -C "$TAP_DIR" commit -m "release: choreographr v${VERSION}"
    echo "==> pushing to origin/$TAP_BRANCH"
    git -C "$TAP_DIR" push origin "HEAD:$TAP_BRANCH"
else
    echo
    echo "==> dry run — nothing committed or pushed. Re-run with --push to apply."
fi

cat <<EOF

==> remaining manual step (MacBook, needs real Homebrew):
    brew install ./choreographr.rb && choreographr --version

==> remember to sync the mirrored formula in this repo:
    packaging/homebrew/choreographr.rb  (commit the drift — RELEASE.md Phase 6)
EOF
