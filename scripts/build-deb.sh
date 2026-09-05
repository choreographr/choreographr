#!/usr/bin/env bash
# build-deb.sh — builds the single "fat" .deb containing the release
# binaries plus the systemd user unit.
#
# Prerequisites:
#   - dist-profile artifacts in target/dist/ — produced by scripts/release.sh
#     or `./scripts/build-stable.sh build --locked --profile dist --workspace`
#     (binaries: choreographr choreo-tui)
#   - packaging/choreographr.service
#   - dpkg-deb (install on Arch with: pacman -S dpkg)
#
# No postinst: the daemon is a user service, installed but never auto-enabled.
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
# Single source of truth for the version: the workspace manifest.
VERSION="$(sed -n 's/^version = "\(.*\)"/\1/p' "$REPO_ROOT/Cargo.toml" | head -n1)"
[ -n "$VERSION" ] || { echo "error: could not read version from Cargo.toml" >&2; exit 1; }

BINARIES=(choreographr choreo-tui)

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
# -Zxz: force xz members — NOT dpkg-deb's default. dpkg >= 1.22 defaults to
# zstd, but this package targets old-dpkg distros (the same old-CPU floor the
# build targets — Ubuntu 22.04's dpkg 1.21.1 has no zstd support), which fail
# with "could not locate member control.tar{xz,lzma,}" on a zstd package.
# xz is universally supported; the assertion below guards the choice so a
# future dpkg default flip fails here, not at install time.
DEB="$REPO_ROOT/dist/choreographr-${VERSION}-x86_64.deb"
# --root-owner-group stamps the archive files as root:root regardless of who
# builds, so the .deb is byte-stable across builders.
dpkg-deb --build --root-owner-group -Zxz "$STAGE" "$DEB"

# The archive members must be exactly the xz flavor (see the -Zxz comment).
MEMBERS="$(ar t "$DEB")"
if [ "$MEMBERS" != "$(printf 'debian-binary\ncontrol.tar.xz\ndata.tar.xz')" ]; then
    echo "error: $DEB archive members are not the compat-safe xz set, found:" >&2
    echo "$MEMBERS" >&2
    exit 1
fi
echo "==> members ok: debian-binary + control.tar.xz + data.tar.xz (no zstd)"

echo "built dist/choreographr-${VERSION}-x86_64.deb"
