#!/usr/bin/env bash
# release.sh — orchestrates a Choreographr release: versioned build, release
# tarball, checksums, optional .deb/.rpm, and (only with --upload) the GitHub
# release. The default is a DRY RUN: it produces all artifacts in dist/ and
# prints the exact commands to run, but never uploads anything.
#
# Usage:
#   scripts/release.sh                 # dry-run: artifacts + instructions
#   scripts/release.sh --upload        # also run `gh release create`
#   scripts/release.sh --allow-dirty   # skip the dirty-tree guard
set -euo pipefail

usage() {
    cat <<EOF
Usage: $0 [--upload] [--allow-dirty] [--help]

Dry-run by default: builds, tarballs, checksums, .deb/.rpm, and prints the
exact upload + checklist commands. Never uploads unless --upload is passed.

  --upload        also run \`gh release create\` with the built artifacts
  --allow-dirty   skip the "working tree must be clean" guard
  --help          show this help
EOF
}

UPLOAD=0
ALLOW_DIRTY=0
for arg in "$@"; do
    case "$arg" in
        --upload) UPLOAD=1 ;;
        --allow-dirty) ALLOW_DIRTY=1 ;;
        --help|-h) usage; exit 0 ;;
        *) echo "$0: error: unknown option: $arg" >&2; usage >&2; exit 1 ;;
    esac
done

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$REPO_ROOT"

# Version comes from the workspace manifest — the single source of truth that
# packaging/homebrew/choreographr.rb and packaging/aur/PKGBUILD mirror.
VERSION="$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -n1)"
[ -n "$VERSION" ] || { echo "error: could not read version from Cargo.toml" >&2; exit 1; }

# Releasing from a dirty tree risks shipping uncommitted changes; the flag is
# an explicit escape hatch for CI-style flows that stage files first.
if [ "$ALLOW_DIRTY" -eq 0 ] && [ -n "$(git status --porcelain)" ]; then
    echo "error: working tree is dirty — commit or stash changes first (or pass --allow-dirty)" >&2
    exit 1
fi

# Host-target detection mirrors scripts/install.sh: exactly two targets in
# $VERSION. (Cross-compiling other targets is out of scope for this script.)
case "$(uname -s)-$(uname -m)" in
    Linux-x86_64) TARGET="x86_64-unknown-linux-gnu" ;;
    Darwin-arm64) TARGET="aarch64-apple-darwin" ;;
    *)
        echo "error: unsupported platform: $(uname -s) $(uname -m)" >&2
        echo "error: ${VERSION} ships Linux x86_64 and macOS arm64 only" >&2
        exit 1
        ;;
esac

# The four release binaries (must match scripts/install.sh and the formula).
BINARIES=(choreographr choreo-tui choreo-im choreo-acp)

echo "==> building release binaries (root package)"
# Build only the four shipped binaries: `-p choreographr` pulls in the daemon,
# TUI, IM, and ACP transitively but NOT choreo-gui (its dioxus/webkit2gtk stack
# is not shipped and must not be a build requirement of the release machine).
# `--features pdf` enables the native PDF tools (pdf_classify / pdf_to_markdown)
# for the shipped binaries: the pdf-inspector dep is feature-gated off by default
# so the published crates.io manifests stay free of the git-pinned security fork
# (crates.io rejects git deps — see the root Cargo.toml [patch.crates-io] and
# choreo-daemon/Cargo.toml). Building from the git tree applies the workspace
# patch, so release binaries get the RUSTSEC-2026-0187-hardened parser.
cargo build --release -p choreographr --features pdf

# Stage the tarball contents: the four binaries plus both service files, all
# at the top level of the archive (no bin/ prefix) so install.sh and the
# Homebrew formula can reference them directly. tar preserves exec bits.
mkdir -p dist
STAGE="$(mktemp -d)"
trap 'rm -rf "$STAGE"' EXIT
for b in "${BINARIES[@]}"; do
    [ -x "target/release/$b" ] || {
        echo "error: missing target/release/$b — build did not produce it" >&2
        exit 1
    }
    install -m 0755 "target/release/$b" "$STAGE/$b"
done
install -m 0644 packaging/choreographr.service "$STAGE/choreographr.service"
install -m 0644 packaging/com.choreographr.daemon.plist "$STAGE/com.choreographr.daemon.plist"

# NOTE: for wide glibc compatibility the tarball should be produced from a
# build inside an old-glibc container (Debian oldstable / Ubuntu 20.04); a
# host build on a rolling-release distro links a newer glibc than many users
# have. A static musl build is NOT viable: libdbus-sys (notify-rust's Linux
# DBus backend) requires the glibc-linked system libdbus.
TARBALL="dist/choreographr-${VERSION}-${TARGET}.tar.gz"
tar czf "$TARBALL" -C "$STAGE" \
    "${BINARIES[@]}" choreographr.service com.choreographr.daemon.plist

# Checksums ship beside the tarball (install.sh verifies against this file).
( cd dist && sha256sum "choreographr-${VERSION}-${TARGET}.tar.gz" > SHA256SUMS )

# .deb/.rpm are best-effort: skip with a warning when the toolchain is absent
# so a Linux-x86_64 release can still proceed without dpkg/rpmbuild installed.
if command -v dpkg-deb >/dev/null 2>&1; then
    "$REPO_ROOT/scripts/build-deb.sh"
else
    echo "warning: dpkg-deb not found — skipping .deb (install with: pacman -S dpkg)" >&2
fi
if command -v rpmbuild >/dev/null 2>&1; then
    "$REPO_ROOT/scripts/build-rpm.sh"
else
    echo "warning: rpmbuild not found — skipping .rpm (install with: pacman -S rpm-tools)" >&2
fi

echo
echo "==> artifacts in dist/:"
ls -lh dist/

# Assemble the artifact list the same way for printing and uploading.
GH_ARTIFACTS=("dist/choreographr-${VERSION}-${TARGET}.tar.gz" "dist/SHA256SUMS")
[ -f "dist/choreographr-${VERSION}-x86_64.deb" ] && GH_ARTIFACTS+=("dist/choreographr-${VERSION}-x86_64.deb")
[ -f "dist/choreographr-${VERSION}-x86_64.rpm" ] && GH_ARTIFACTS+=("dist/choreographr-${VERSION}-x86_64.rpm")

echo
echo "==> validate before uploading:"
echo "    scripts/smoke-test.sh ${TARBALL}"
echo
echo "==> release command (run manually, or re-run this script with --upload):"
echo "    gh release create v${VERSION} ${GH_ARTIFACTS[*]} --title \"choreographr ${VERSION}\" --generate-notes"
echo
echo "==> post-publish checklist:"
echo "  - Homebrew: bump packaging/homebrew/choreographr.rb (version, urls,"
echo "    shasum -a 256) and push to the ethernomad/homebrew-choreographr tap"
echo "  - AUR: bump pkgver in packaging/aur/PKGBUILD + regenerate .SRCINFO"
echo "    (makepkg --printsrcinfo > .SRCINFO)"
echo "  - crates.io: cargo release publish (publish-set members in dependency"
echo "    order; the native PDF tools are feature-gated and off by default on"
echo "    crates.io — release binaries build them via --features pdf, see the"
echo "    [patch.crates-io] section of the root Cargo.toml)"
echo "  - choreographr.com: publish scripts/install.sh and add download"
echo "    redirects for v${VERSION}"

if [ "$UPLOAD" -eq 1 ]; then
    command -v gh >/dev/null 2>&1 || {
        echo "error: gh not found — install it (e.g. pacman -S github-cli)" >&2
        exit 1
    }
    echo
    echo "==> uploading release v${VERSION}"
    gh release create "v${VERSION}" "${GH_ARTIFACTS[@]}" \
        --title "choreographr ${VERSION}" --generate-notes
    echo "==> upload complete"
fi
