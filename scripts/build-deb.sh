#!/usr/bin/env bash
# build-deb.sh — builds the single "fat" .deb containing the four release
# binaries plus the systemd user unit.
#
# Prerequisites:
#   - dist-profile artifacts in target/dist/ — produced by scripts/release.sh
#     or `./scripts/build-stable.sh build --locked --profile dist --workspace`
#     (binaries: choreographr choreo-tui choreo-im choreo-acp)
#   - packaging/choreographr.service
#   - dpkg-deb (install on Arch with: pacman -S dpkg)
#
# No postinst: the daemon is a user service, installed but never auto-enabled.
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
# Single source of truth for the version: the workspace manifest.
VERSION="$(sed -n 's/^version = "\(.*\)"/\1/p' "$REPO_ROOT/Cargo.toml" | head -n1)"
[ -n "$VERSION" ] || { echo "error: could not read version from Cargo.toml" >&2; exit 1; }

BINARIES=(choreographr choreo-tui choreo-im choreo-acp)

command -v dpkg-deb >/dev/null 2>&1 || {
    echo "error: dpkg-deb not found — install it with: pacman -S dpkg" >&2
    exit 1
}

# Fail fast on missing inputs so we never ship a half-built package.
for b in "${BINARIES[@]}"; do
    [ -x "$REPO_ROOT/target/dist/$b" ] || {
        echo "error: missing $REPO_ROOT/target/dist/$b — run the release pipeline first (scripts/release.sh) or: ./scripts/build-stable.sh build --locked --profile dist --workspace" >&2
        exit 1
    }
done
[ -f "$REPO_ROOT/packaging/choreographr.service" ] || {
    echo "error: missing $REPO_ROOT/packaging/choreographr.service" >&2
    exit 1
}

# Stage the Debian layout in a throwaway root, then let dpkg-deb archive it.
STAGE="$(mktemp -d)"
trap 'rm -rf "$STAGE"' EXIT
mkdir -p "$STAGE/usr/bin" "$STAGE/usr/lib/systemd/user" "$STAGE/DEBIAN"
for b in "${BINARIES[@]}"; do
    install -m 0755 "$REPO_ROOT/target/dist/$b" "$STAGE/usr/bin/$b"
done
install -m 0644 "$REPO_ROOT/packaging/choreographr.service" \
    "$STAGE/usr/lib/systemd/user/choreographr.service"

# Minimal control file: empty Depends (static binaries), no postinst — the
# user starts the daemon themselves (installed, never auto-enabled policy).
cat > "$STAGE/DEBIAN/control" <<EOF
Package: choreographr
Version: $VERSION
Architecture: amd64
Maintainer: Choreographr Maintainers <maintainers@choreographr.com>
Depends:
Section: utils
Priority: optional
Description: Agentic coding assistant — daemon, TUI, and bridges
 Prebuilt 0.1.0 binaries for the Choreographr daemon and its clients, plus the
 systemd user unit. The unit is installed but never auto-enabled; start the
 daemon with: systemctl --user enable --now choreographr
EOF

mkdir -p "$REPO_ROOT/dist"
# --root-owner-group stamps the archive files as root:root regardless of who
# builds, so the .deb is byte-stable across builders.
dpkg-deb --build --root-owner-group "$STAGE" \
    "$REPO_ROOT/dist/choreographr-${VERSION}-x86_64.deb"
echo "built dist/choreographr-${VERSION}-x86_64.deb"
