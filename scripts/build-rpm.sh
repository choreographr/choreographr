#!/usr/bin/env bash
# build-rpm.sh — builds the single "fat" .rpm containing the four release
# binaries plus the systemd user unit.
#
# Prerequisites:
#   - `cargo build --release --workspace` artifacts in target/release/
#   - packaging/choreographr.service + packaging/rpm/choreographr.spec
#   - rpmbuild (install on Arch with: pacman -S rpm-tools)
#
# How it works (why): the spec (packaging/rpm/choreographr.spec) builds
# nothing — the binaries are prebuilt and staged here into a build root at the
# exact layout %files expects (/usr/bin/*, /usr/lib/systemd/user/...). We then
# point rpmbuild's --buildroot at that staging root so the spec's %files
# section picks up the staged files verbatim. __os_install_post is disabled so
# rpm's brp scripts don't re-process the prebuilt binaries — which are already
# stripped by the workspace [profile.release] strip = "symbols" setting.
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
VERSION="$(sed -n 's/^version = "\(.*\)"/\1/p' "$REPO_ROOT/Cargo.toml" | head -n1)"
[ -n "$VERSION" ] || { echo "error: could not read version from Cargo.toml" >&2; exit 1; }

BINARIES=(choreographr choreo-tui choreo-im choreo-acp)

command -v rpmbuild >/dev/null 2>&1 || {
    echo "error: rpmbuild not found — install it with: pacman -S rpm-tools" >&2
    exit 1
}

for b in "${BINARIES[@]}"; do
    [ -x "$REPO_ROOT/target/release/$b" ] || {
        echo "error: missing $REPO_ROOT/target/release/$b — run 'cargo build --release --workspace' first" >&2
        exit 1
    }
done
[ -f "$REPO_ROOT/packaging/choreographr.service" ] || {
    echo "error: missing $REPO_ROOT/packaging/choreographr.service" >&2
    exit 1
}

# STAGE doubles as the rpm build root; TOPDIR is rpmbuild's private scratch
# tree (kept out of $HOME/rpmbuild so builds are hermetic and cleanable).
STAGE="$(mktemp -d)"
TOPDIR="$(mktemp -d)"
trap 'rm -rf "$STAGE" "$TOPDIR"' EXIT
mkdir -p "$STAGE/usr/bin" "$STAGE/usr/lib/systemd/user"
mkdir -p "$TOPDIR"/{BUILD,BUILDROOT,RPMS,SOURCES,SPECS,SRPMS,tmp}
for b in "${BINARIES[@]}"; do
    install -m 0755 "$REPO_ROOT/target/release/$b" "$STAGE/usr/bin/$b"
done
install -m 0644 "$REPO_ROOT/packaging/choreographr.service" \
    "$STAGE/usr/lib/systemd/user/choreographr.service"

rpmbuild -bb \
    --buildroot "$STAGE" \
    --define "_topdir $TOPDIR" \
    --define "_tmppath $TOPDIR/tmp" \
    --define "__os_install_post %{nil}" \
    "$REPO_ROOT/packaging/rpm/choreographr.spec"

mkdir -p "$REPO_ROOT/dist"
RPM="$(find "$TOPDIR/RPMS" -type f -name 'choreographr-*.rpm' | head -n1)"
[ -n "$RPM" ] || { echo "error: rpmbuild produced no .rpm" >&2; exit 1; }
install -m 0644 "$RPM" "$REPO_ROOT/dist/choreographr-${VERSION}-x86_64.rpm"
echo "built dist/choreographr-${VERSION}-x86_64.rpm"
