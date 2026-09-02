#!/usr/bin/env bash
# build-deb-termux.sh — build the Termux-native .deb from the ALREADY-BUILT
# Android binaries in target/android/arm64-v8a/. NO REBUILD happens here: the
# binaries are cross-compiled by scripts/build-android.sh (locally) or by the
# android-termux CI job (release.yml) before this script runs.
#
# Result: dist/choreographr-termux_<version>_aarch64.deb, installable inside a
# Termux shell with:  pkg install ./choreographr-termux_<version>_aarch64.deb
# (pkg wraps dpkg, which maps the package's ./bin/ layout onto $PREFIX/bin).
# The `-termux-` infix keeps the filename unambiguous next to the desktop
# `choreographr-<version>-x86_64.deb` on the release page.
#
# Why this is NOT scripts/build-deb.sh reused:
#   - Architecture tag: Termux's dpkg expects `aarch64`, not Debian's `arm64`
#     (mismatched tags make dpkg refuse the package as "wrong architecture").
#   - Install layout: files live at ./data/data/com.termux/files/usr/bin/<name>
#     — the REAL on-device path of Termux's fixed $PREFIX. Termux's dpkg runs
#     with install-root / (an app cannot chroot), so packages must carry the
#     absolute path, exactly like Termux's own repo packages (verified against
#     upstream dash_0.5.12: ./data/data/com.termux/files/usr/bin/dash). The
#     earlier ./bin/ convention was wrong — dpkg tried to create /bin at the
#     read-only device root (on-device failure, 2026-09-02). This hardcodes
#     the official Termux app id (com.termux); forks with a different app id
#     have a different private dir and need a rebuilt package.
#   - No `Depends:`: the four binaries are static-bionic executables linking
#     only Android system libs (interpreter /system/bin/linker64), so there is
#     nothing in Termux's package universe to depend on.
#   - No maintainer scripts (postinst et al.) and no conffiles: Termux has no
#     root — dpkg runs as the app uid, and maintainer-script conventions do
#     not apply. Runtime state lives in ~/.local/share/choreographr/
#     (unpackaged), so there is nothing to preserve either.
#   - No systemd unit: there is no systemd on Android (the "installed, never
#     auto-enabled" policy degenerates to "never auto-enabled" — the user
#     starts the daemon themselves).
#   - Compression: -Zxz, NOT dpkg-deb's default. dpkg >= 1.22 defaults to
#     zstd (control.tar.zst/data.tar.zst), but Termux's dpkg build has no
#     zstd support — it searches only for control.tar{xz,lzma,} and rejects
#     the package with "could not locate member control.tar{xz,lzma,}"
#     (observed on-device, 2026-09-02). xz is universally supported by every
#     dpkg, so it is forced explicitly; the validation below also asserts the
#     members are xz so a future dpkg default flip fails HERE, not on-device.
#
# Validation ceiling: there is no Termux on the build machine (and CI runners
# are x86_64), so the checks below are STRUCTURAL ONLY — dpkg-deb field and
# content inspection. Actual on-device installation is exercised by the user
# (and documented in README's Termux section); a green CI run proves the
# package is well-formed, not that Termux accepts it.
#
# Prerequisites: the four binaries in target/android/arm64-v8a/ (see
# scripts/build-android.sh) and dpkg-deb (preinstalled on ubuntu runners; on
# Arch install with: pacman -S dpkg).
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
# Single source of truth for the version: the workspace manifest (same sed
# pattern as build-deb.sh / release.yml — one version everywhere).
VERSION="$(sed -n 's/^version = "\(.*\)"/\1/p' "$REPO_ROOT/Cargo.toml" | head -n1)"
[ -n "$VERSION" ] || { echo "error: could not read version from Cargo.toml" >&2; exit 1; }

BINARIES=(choreographr choreo-tui choreo-im choreo-acp)
# Termux's architecture tag. NOT Debian's `arm64` — dpkg compares this string
# against Termux's own dpkg architecture and rejects any other spelling.
ARCH="aarch64"
# Build-ABI subdirectory build-android.sh stages into (cargo-ndk's ABI name).
ABI_DIR="arm64-v8a"

command -v dpkg-deb >/dev/null 2>&1 || {
    echo "error: dpkg-deb not found — install it with: pacman -S dpkg (preinstalled on ubuntu runners)" >&2
    exit 1
}

# Fail fast on missing inputs so we never ship a half-built package. The
# binaries must ALREADY be built (build-android.sh); this script never builds.
for b in "${BINARIES[@]}"; do
    [ -f "$REPO_ROOT/target/android/$ABI_DIR/$b" ] || {
        echo "error: missing $REPO_ROOT/target/android/$ABI_DIR/$b — build the Android binaries first: ./scripts/build-android.sh" >&2
        exit 1
    }
done

# Stage the Termux layout in a throwaway root, then let dpkg-deb archive it.
# The path is Termux's fixed $PREFIX as it exists ON DEVICE — Termux's dpkg
# installs against / with no chroot (see header), so the package must spell
# out data/data/com.termux/files/usr/bin exactly. These are still RELATIVE
# paths inside the archive (./data/...), anchored at the device root.
TERMUX_USR="data/data/com.termux/files/usr"
STAGE="$(mktemp -d)"
trap 'rm -rf "$STAGE"' EXIT
# mktemp creates the root 0700; the package's directory entries should carry
# sane 0755 modes (dpkg-deb preserves the staging tree's modes verbatim).
chmod 0755 "$STAGE"
mkdir -p "$STAGE/$TERMUX_USR/bin" "$STAGE/DEBIAN"
chmod 0755 "$STAGE/$TERMUX_USR/bin"
for b in "${BINARIES[@]}"; do
    # 0755 explicitly: dpkg-deb preserves the staging tree's modes verbatim,
    # and the source bits are whatever the cross-build left on disk.
    install -m 0755 "$REPO_ROOT/target/android/$ABI_DIR/$b" "$STAGE/$TERMUX_USR/bin/$b"
done

# Minimal control file: no Depends (static bionic binaries — see header), no
# conffiles, no maintainer scripts (Termux dpkg runs as the app uid, no root).
cat > "$STAGE/DEBIAN/control" <<EOF
Package: choreographr
Version: $VERSION
Architecture: $ARCH
Maintainer: Choreographr Maintainers <maintainers@choreographr.com>
Section: utils
Priority: optional
Description: Agentic coding assistant — daemon, TUI, and bridges (Termux)
 Prebuilt aarch64 Android binaries for the Choreographr daemon and its
 clients, installed into \$PREFIX/bin. No daemon auto-start — run
 \`choreographr\` yourself inside the Termux session; runtime state lives in
 ~/.local/share/choreographr/.
EOF

mkdir -p "$REPO_ROOT/dist"
DEB="$REPO_ROOT/dist/choreographr-termux_${VERSION}_${ARCH}.deb"
# --root-owner-group stamps the archive files as root:root regardless of who
# builds — non-root builders otherwise get chown failures (and CI runners are
# always non-root), and the field is meaningless on Termux (single-uid app).
# -Zxz: see the header's compression bullet — Termux's dpkg cannot read zstd,
# which is the dpkg >= 1.22 default on the ubuntu runners.
dpkg-deb --build --root-owner-group -Zxz "$STAGE" "$DEB"

# ── structural validation (the CI ceiling — see the header comment) ──────────
# Field values via dpkg-deb --info's verbatim control dump (fields are indented
# by exactly one space), so a wrong architecture or version fails the build
# right here instead of failing on-device with dpkg's far less actionable
# error.
log() { echo "==> $*"; }

log "verifying $DEB"
INFO="$(dpkg-deb --info "$DEB")"

# Archive members must be the xz flavor: dpkg-deb's compression default is a
# moving target (zstd since 1.22 on ubuntu runners), and Termux's dpkg cannot
# decompress zstd — a .zst member here means an uninstallable-on-Termux
# package, so assert instead of trusting the -Zxz flag above.
MEMBERS="$(ar t "$DEB")"
MEMBERS_OK=1
[ "$MEMBERS" = "$(printf 'debian-binary\ncontrol.tar.xz\ndata.tar.xz')" ] || MEMBERS_OK=0
if [ "$MEMBERS_OK" -ne 1 ]; then
    echo "error: $DEB archive members are not the Termux-compatible xz set, found:" >&2
    echo "$MEMBERS" >&2
    echo "  (Termux's dpkg supports control.tar{xz,lzma,} only — no zstd)" >&2
    exit 1
fi
echo "  ok: members are debian-binary + control.tar.xz + data.tar.xz (no zstd)"

check_field() {
    # check_field <field> <expected-value> — anchored at line start in the
    # verbatim control dump; anything else is a packaging bug.
    # dpkg-deb --info indents the verbatim control dump by one space.
    if grep -q "^ $1: $2\$" <<<"$INFO"; then
        echo "  ok: $1: $2"
    else
        echo "error: $DEB control field check failed: expected '$1: $2'" >&2
        exit 1
    fi
}
check_field Package choreographr
check_field Version "$VERSION"
check_field Architecture "$ARCH"

# No Depends: the header explains why there must be none; a leftover one would
# make pkg try to resolve dependencies in a universe that does not have them.
if grep -q '^ Depends:' <<<"$INFO"; then
    echo "error: $DEB must not declare Depends (static bionic binaries)" >&2
    exit 1
fi
echo "  ok: no Depends"

# No maintainer scripts / conffiles: the control archive must contain ONLY the
# control file itself (plus the tar "./" root entry; dpkg-deb --build does not
# synthesize md5sums here).
CTRL_TAR="$(dpkg-deb --ctrl-tarfile "$DEB" | tar -t)"
CTRL_OK=1
for entry in $CTRL_TAR; do
    case "$entry" in
        ./|./control) ;;            # expected
        *) CTRL_OK=0 ;;             # anything else = postinst/conffiles/md5sums drift
    esac
done
if [ "$CTRL_OK" -ne 1 ]; then
    echo "error: $DEB control archive must contain only the control file, found:" >&2
    echo "$CTRL_TAR" >&2
    exit 1
fi
echo "  ok: no maintainer scripts or conffiles"

# Contents + exec bits: all four binaries at Termux's $PREFIX/bin (the real
# on-device path — see the header), mode -rwxr-xr-x (dpkg-deb --contents
# prints the staging tree's modes verbatim).
CONTENTS="$(dpkg-deb --contents "$DEB")"
for b in "${BINARIES[@]}"; do
    if grep -q "^-rwxr-xr-x .* \./$TERMUX_USR/bin/$b\$" <<<"$CONTENTS"; then
        echo "  ok: \$PREFIX/bin/$b (0755)"
    else
        echo "error: $DEB contents check failed: expected executable ./$TERMUX_USR/bin/$b" >&2
        echo "$CONTENTS" >&2
        exit 1
    fi
done

# Nothing else may be in the data archive — the only directory chain allowed
# is Termux's $PREFIX path (+ its parents), plus the four binaries. An explicit
# whitelist loop (not a regex) so an unbalanced pattern can't silently invert
# the check.
STRAY_OK=1
while IFS= read -r entry; do
    # The archive path is the entry's last whitespace-separated field.
    case "${entry##* }" in
        ./|./data/|./data/data/|./data/data/com.termux/|./data/data/com.termux/files/|./data/data/com.termux/files/usr/|./data/data/com.termux/files/usr/bin/) ;;
        ./data/data/com.termux/files/usr/bin/choreographr|./data/data/com.termux/files/usr/bin/choreo-tui|./data/data/com.termux/files/usr/bin/choreo-im|./data/data/com.termux/files/usr/bin/choreo-acp) ;;
        *) STRAY_OK=0 ;;
    esac
done <<<"$CONTENTS"
if [ "$STRAY_OK" -ne 1 ]; then
    echo "error: $DEB contains unexpected entries (only \$PREFIX/bin/* allowed):" >&2
    echo "$CONTENTS" >&2
    exit 1
fi
echo "  ok: data archive contains only \$PREFIX/bin/*"

log "built $DEB"
log "install inside a Termux shell: pkg install ./choreographr-termux_${VERSION}_${ARCH}.deb"
