#!/bin/sh
# Choreographr installer — fetches the prebuilt release tarball, verifies its
# SHA-256 checksum, and installs the four binaries plus the platform service
# file (systemd user unit on Linux, launchd agent on macOS).
#
# Dependencies: curl + tar (+ sha256sum on Linux / shasum on macOS). Nothing
# else — no build toolchain, no root, no package manager.
#
# Security choices (why):
#   - Pinned VERSION + per-file sha256 from SHA256SUMS: there is no "latest"
#     redirect and no trust-on-first-use. The version string and checksums are
#     embedded here, so a compromised download server cannot silently swap in
#     different (older or modified) binaries.
#   - The checksum file itself is fetched over the same TLS channel as the
#     tarball; the pin protects against downgrade/swap attacks and TLS
#     protects the pin's transport. Defense in depth, not a chain of trust.
#   - No eval and no shelling out to downloaded content: the tarball is only
#     unpacked with `tar -xzf` for an explicit member list (never `tar -xzf .`
#     on a remote archive), and the installed files are plain binaries the
#     user invokes themselves.
#
# The service file is installed but NEVER auto-enabled: starting the daemon is
# an explicit user decision (it needs accounts/API keys configured first).

set -eu

VERSION="0.1.0" # embedded release version — bump with every release
# Overridable for testing/mirrors (e.g. a local HTTP server serving the same
# layout). Default points at the canonical download root for this version.
: "${CHOREOGRAPHR_BASE_URL:=https://choreographr.com/download/${VERSION}/}"
BASE="${CHOREOGRAPHR_BASE_URL%/}" # normalize: no trailing slash

# The four release binaries (must match scripts/release.sh). choreo-mcp is a
# library-only crate (no binary); the shipped set is these four.
BINARIES="choreographr choreo-tui choreo-im choreo-acp"

usage() {
    cat <<EOF
Usage: $0 [--uninstall] [--help]

Install (default) or remove the Choreographr release binaries and the
platform service file. The service file is installed but never auto-enabled.

  --uninstall   remove the four binaries and the service file
  --help        show this help

Environment:
  CHOREOGRAPHR_BASE_URL  override the download base URL (testing/mirrors)
EOF
}

UNINSTALL=0
for arg in "$@"; do
    case "$arg" in
        --uninstall) UNINSTALL=1 ;;
        --help|-h) usage; exit 0 ;;
        *)
            echo "$0: error: unknown option: $arg" >&2
            usage >&2
            exit 1
            ;;
    esac
done

# Platform detection — $VERSION ships exactly these two targets.
OS="$(uname -s)"
ARCH="$(uname -m)"
case "${OS}-${ARCH}" in
    Linux-x86_64) ASSET="choreographr-${VERSION}-x86_64-unknown-linux-gnu.tar.gz" ;;
    Darwin-arm64) ASSET="choreographr-${VERSION}-aarch64-apple-darwin.tar.gz" ;;
    *)
        echo "$0: error: unsupported platform: ${OS} ${ARCH}" >&2
        echo "$0: error: ${VERSION} ships Linux x86_64 and macOS arm64 only" >&2
        exit 1
        ;;
esac

# sha256 tool differs per OS; both print "<hash>  <file>", which the awk
# below parses. Prefer sha256sum (Linux), fall back to shasum (macOS).
if command -v sha256sum >/dev/null 2>&1; then
    sha256_of() { sha256sum "$1" | awk '{print $1}'; }
elif command -v shasum >/dev/null 2>&1; then
    sha256_of() { shasum -a 256 "$1" | awk '{print $1}'; }
else
    echo "$0: error: no sha256 tool found (need sha256sum or shasum)" >&2
    exit 1
fi

# Where the binaries go. XDG_BIN_HOME (if set) is already the bin directory
# per the XDG convention — do NOT append "/bin" to it; the default matches
# the systemd unit's ExecStart=%h/.local/bin/choreographr.
BIN_DIR="${XDG_BIN_HOME:-$HOME/.local/bin}"

if [ "$UNINSTALL" -eq 1 ]; then
    echo "==> removing binaries from $BIN_DIR" >&2
    for b in $BINARIES; do
        rm -f "$BIN_DIR/$b"
    done
    case "${OS}-${ARCH}" in
        Linux-*)
            # Also drop the unit and reload systemd so it forgets the removal.
            UNIT_DIR="${XDG_CONFIG_HOME:-$HOME/.config}/systemd/user"
            rm -f "$UNIT_DIR/choreographr.service"
            if command -v systemctl >/dev/null 2>&1; then
                systemctl --user daemon-reload 2>/dev/null || true
            fi
            ;;
        Darwin-*) # remove the launchd agent plist (we never loaded it)
            rm -f "$HOME/Library/LaunchAgents/com.choreographr.daemon.plist"
            ;;
    esac
    echo "==> uninstall complete" >&2
    exit 0
fi

echo "==> downloading ${ASSET} from ${BASE}/" >&2
TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT HUP INT TERM
curl -fsSL -o "$TMP/$ASSET" "${BASE}/${ASSET}"
curl -fsSL -o "$TMP/SHA256SUMS" "${BASE}/SHA256SUMS"

# Look up the expected digest for exactly our asset. Tolerate the '*' binary
# marker some sha256 tools emit: gsub strips it from the filename field.
expected="$(awk -v a="$ASSET" '{ gsub(/^\*/, "", $2); if ($2 == a) print $1 }' "$TMP/SHA256SUMS")"
if [ -z "$expected" ]; then
    echo "$0: error: no checksum for ${ASSET} in SHA256SUMS" >&2
    exit 1
fi
actual="$(sha256_of "$TMP/$ASSET")"
if [ "$actual" != "$expected" ]; then
    echo "$0: error: checksum mismatch for ${ASSET}" >&2
    echo "  expected: $expected" >&2
    echo "  actual:   $actual" >&2
    exit 1
fi
echo "==> checksum OK" >&2

# Extract only the members we need — an explicit member list keeps the blast
# radius of a (checksum-verified, but still remote) archive to known files.
mkdir -p "$BIN_DIR"
# shellcheck disable=SC2086 # deliberate: split the fixed, non-user BINARIES list into member names (POSIX sh has no arrays)
tar -xzf "$TMP/$ASSET" -C "$BIN_DIR" $BINARIES
for b in $BINARIES; do
    # Belt-and-braces: tar preserves modes, but never chmod through a symlink
    # (chmod follows links — a hostile archive member could point at a user
    # file). Refuse symlinks and non-files instead.
    if [ -L "$BIN_DIR/$b" ] || [ ! -f "$BIN_DIR/$b" ]; then
        echo "$0: error: extracted '$b' is not a regular file — aborting" >&2
        rm -f "$BIN_DIR/$b"
        exit 1
    fi
    chmod +x "$BIN_DIR/$b"
done

case "${OS}-${ARCH}" in
    Linux-*) # systemd USER unit -> ~/.config/systemd/user, NOT enabled
        UNIT_DIR="${XDG_CONFIG_HOME:-$HOME/.config}/systemd/user"
        mkdir -p "$UNIT_DIR"
        tar -xzf "$TMP/$ASSET" -C "$UNIT_DIR" choreographr.service
        echo "==> installed systemd unit: $UNIT_DIR/choreographr.service" >&2
        echo "    enable it when ready with:" >&2
        echo "      systemctl --user enable --now choreographr" >&2
        ;;
    Darwin-*) # launchd agent -> ~/Library/LaunchAgents, NOT loaded
        AGENT_DIR="$HOME/Library/LaunchAgents"
        mkdir -p "$AGENT_DIR"
        tar -xzf "$TMP/$ASSET" -C "$AGENT_DIR" com.choreographr.daemon.plist
        echo "==> installed launchd agent: $AGENT_DIR/com.choreographr.daemon.plist" >&2
        echo "    load it when ready with:" >&2
        echo "      launchctl load $AGENT_DIR/com.choreographr.daemon.plist" >&2
        # NOTE: the shipped plist hardcodes /opt/homebrew/bin/choreographr
        # (the Homebrew layout). Non-Homebrew installs that put the binaries
        # elsewhere must edit ProgramArguments in the plist before loading.
        ;;
esac

echo "==> done. Binaries installed in $BIN_DIR" >&2
