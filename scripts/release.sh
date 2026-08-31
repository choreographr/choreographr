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
#
# Requires: cargo install cargo-zigbuild (the Linux x86_64 musl tarball build
# below cross-compiles via zig).
#
# Dist/release binaries are built on STABLE Rust (reproducible; matches the
# crates.io/MSRV story) under the workspace's dedicated [profile.dist] profile
# (see root Cargo.toml): `--profile dist` rather than `--release` puts the
# shipped artifacts in target/<triple>/dist, separate from any local
# `cargo build --release` output, so the staging below can only ever pick up
# binaries this pipeline produced. Each cargo build goes through
# scripts/build-stable.sh, which temporarily strips the nightly-only
# profile-rustflags bits the dev toolchain uses, runs the build under
# `cargo +stable`, then restores them.
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

# The release build links the four large binaries in one go; linking can open
# thousands of files at once, and on machines with a low default soft fd limit
# (e.g. 1024) the link dies with "ProcessFdQuotaExceeded". Raise the soft
# limit best-effort — the hard limit is typically far higher (here 1048576).
# If this fails, the operator must run the build under a raised limit
# (e.g. `ulimit -n 65536 && just release`) or the link will fail the same way.
if ! ulimit -n 65536 2>/dev/null; then
    echo "warning: could not raise fd limit — release linking may fail with ProcessFdQuotaExceeded; run under a raised limit (ulimit -n 65536)" >&2
fi

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$REPO_ROOT"

# Dist/release binaries are built on STABLE Rust (see the header comment), so a
# stable toolchain is a hard prerequisite — fail fast with a clear hint instead
# of letting `cargo +stable` fail obscurely mid-build.
if ! rustup toolchain list 2>/dev/null | grep -q '^stable'; then
    echo "error: a stable Rust toolchain is required for release builds — run \`rustup toolchain install stable\`" >&2
    exit 1
fi

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
# Linux-x86_64 maps to the static musl triple — the Linux release tarball is a
# fully static musl build (see below); macOS stays the native host build.
case "$(uname -s)-$(uname -m)" in
    Linux-x86_64) TARGET="x86_64-unknown-linux-musl" ;;
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
# `--features metrics,blockchain` enables the Prometheus `/metrics` endpoint and
# the EVM/Substrate blockchain tools for the shipped binaries — both are off by
# default so the published crates.io manifests stay lean (the metrics machinery
# and the optional `choreo-blockchain` crate, which pulls tokio/alloy/subxt into
# the binary). The native PDF tools (pdf_classify / pdf_to_markdown) need no
# feature flag: `pdf-inspector` has been an unconditional dependency since 1.15.0
# replaced the RUSTSEC-2026-0187-vulnerable lopdf ^0.41 pin.

# ── Tarball build ────────────────────────────────────────────────────────────
# The Linux tarball is a fully static x86_64-unknown-linux-musl cross-build.
# A static musl build is viable because the shipped binaries link no C
# libraries: the desktop-notify tool (notify-rust/libdbus-sys — the last C
# dependency) was removed from the daemon, so nothing requires glibc anymore.
# Static musl also replaces the old "build inside an old-glibc container"
# compatibility dance: a musl binary runs on any Linux kernel regardless of
# the host's glibc version. The `mimalloc` feature swaps in mimalloc's
# per-thread allocator, which is markedly better than musl's default malloc
# (see the `#[global_allocator]` blocks in src/bin/*.rs). The macOS tarball is
# the native aarch64-apple-darwin host build (no musl, no mimalloc — Apple
# builds keep the system allocator).
#
# The cross-build runs through cargo-zigbuild because cc-rs passes the full
# Rust triple `x86_64-unknown-linux-musl` to the C compiler, and `zig cc`'s
# target-query grammar rejects the `unknown` vendor slot
# (`UnknownOperatingSystem`); cargo-zigbuild translates the Rust triple to
# zig's grammar (`x86_64-linux-musl`) for both cc-rs and the linker, which is
# the standard solution. zlob is unaffected — its build.rs already maps the
# triple itself.
# `--locked` on every release build is a supply-chain control: it makes the
# committed Cargo.lock authoritative, so a silent lockfile regeneration during
# a release can never pick up a freshly republished (potentially compromised)
# semver-compatible version like the 2026-08-20 arrayref@0.3.10 attack
# (RUSTSEC-2026-0260). The lockfile itself is also checked by
# scripts/check-supply-chain.sh (deny.toml bans) in `just pre-commit`/`ci`.
if [ "$TARGET" = "x86_64-unknown-linux-musl" ]; then
    # CPU floor: x86-64-v2 (SSE3/SSSE3/SSE4.1/SSE4.2/POPCNT/CMPXCHG16B) — the
    # level every AMD64 CPU since Intel Nehalem (2008) / AMD Bulldozer (2011)
    # implements, and the direction enterprise distros have moved (RHEL 10
    # baseline = v3, SLES 16 = v2; Debian/Arch/Fedora stay v1). RUSTFLAGS env,
    # NOT profile rustflags, because (a) profile rustflags ignore --target and
    # would poison cross builds (the build-android.sh lesson), and (b) env
    # rustflags override any user ~/.cargo/config.toml [build] rustflags (e.g.
    # a developer's target-cpu=native), so local and CI artifacts are
    # comparable. Future per-CPU-level artifacts (e.g. a v3 tarball) reuse this
    # exact mechanism with a different value.
    RUSTFLAGS="-C target-cpu=x86-64-v2" ./scripts/build-stable.sh zigbuild --locked --profile dist -p choreographr --target x86_64-unknown-linux-musl --features metrics,mimalloc,blockchain
    TARBALL_BIN_DIR="target/x86_64-unknown-linux-musl/dist"
else
    # macOS: NO target-cpu flag — the aarch64-apple-darwin target spec already
    # defaults to apple-a14 (Apple-Silicon-tuned), and the fleet is homogeneous
    # by definition, so the target default is the right answer here.
    ./scripts/build-stable.sh build --locked --profile dist -p choreographr --features metrics,blockchain
    TARBALL_BIN_DIR="target/dist"
fi

# Stage the tarball contents: the four binaries plus both service files, all
# at the top level of the archive (no bin/ prefix) so install.sh and the
# Homebrew formula can reference them directly. tar preserves exec bits.
mkdir -p dist
STAGE="$(mktemp -d)"
trap 'rm -rf "$STAGE"' EXIT
for b in "${BINARIES[@]}"; do
    [ -x "$TARBALL_BIN_DIR/$b" ] || {
        echo "error: missing $TARBALL_BIN_DIR/$b — build did not produce it" >&2
        exit 1
    }
    install -m 0755 "$TARBALL_BIN_DIR/$b" "$STAGE/$b"
done
install -m 0644 packaging/choreographr.service "$STAGE/choreographr.service"
install -m 0644 packaging/com.choreographr.daemon.plist "$STAGE/com.choreographr.daemon.plist"

TARBALL="dist/choreographr-${VERSION}-${TARGET}.tar.gz"
tar czf "$TARBALL" -C "$STAGE" \
    "${BINARIES[@]}" choreographr.service com.choreographr.daemon.plist

# ── .deb/.rpm build (host glibc, no mimalloc) ───────────────────────────────
# The .deb/.rpm stay native glibc host-target builds WITHOUT the mimalloc
# feature and WITHOUT the musl target: they target glibc distros
# (Debian/Fedora/openSUSE), where the system allocator is competitive — static
# musl + mimalloc is a property of the tarball (which serves general Linux AND
# the AUR `-bin` package in one artifact), not of the distro packages. They
# consume `target/dist/` from a plain host build; the musl tarball build
# above does NOT populate that directory, so build it here. (On macOS this
# step is skipped — dpkg/rpmbuild are not present.)
if [ "$TARGET" = "x86_64-unknown-linux-musl" ]; then
    echo "==> building host (glibc) dist binaries for .deb/.rpm"
    # Deliberately NO target-cpu: the .deb/.rpm serve the full glibc-distro
    # range, whose baselines are split (Debian/Arch/Fedora = v1, RHEL 10 =
    # v3), so baseline (2003 SSE2) is the only level that covers them all.
    ./scripts/build-stable.sh build --locked --profile dist -p choreographr --features metrics,blockchain

    # .deb/.rpm are best-effort: skip with a warning when the toolchain is
    # absent so a Linux-x86_64 release can still proceed without dpkg/rpmbuild
    # installed. The whole block is gated on the Linux-musl target because
    # .deb/.rpm are Linux-only artifacts — on the macOS build these checks
    # would otherwise print irrelevant "dpkg-deb/rpmbuild not found" warnings
    # (seen for real in the 2026-08-31 workflow_dispatch macOS job).
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
fi

# ── Checksums over EVERY artifact for this version ──────────────────────────
# SHA256SUMS ships beside the tarball (install.sh verifies against this file).
# It covers every `choreographr-${VERSION}-*` file already in dist/: this host's
# tarball, the .deb/.rpm just built above, and any other-arch tarball an
# operator staged into dist/ before upload (the macOS tarball copied over from
# the MacBook — see RELEASE.md Phase 4). Regenerating here, after the .deb/.rpm
# step and from the glob rather than the single host tarball, means a combined
# file is produced and `--upload` never clobbers it with a single-host one.
( cd dist && sha256sum choreographr-${VERSION}-* > SHA256SUMS )

echo
echo "==> artifacts in dist/:"
ls -lh dist/

# Assemble the artifact list the same way for printing and uploading: every
# tarball present in dist/ for this version (the host's plus any staged
# other-arch tarballs), the checksum file, then the .deb/.rpm when built. The
# glob always matches at least the host tarball just created, so it needs no
# nullglob guard under `set -u`.
GH_ARTIFACTS=("dist/SHA256SUMS")
for tarball in dist/choreographr-${VERSION}-*.tar.gz; do
    GH_ARTIFACTS+=("$tarball")
done
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
echo "    shasum -a 256) and push to the choreographr/homebrew-choreographr tap"
echo "  - AUR: bump pkgver in packaging/aur/PKGBUILD + regenerate .SRCINFO"
echo "    (makepkg --printsrcinfo > .SRCINFO)"
echo "  - crates.io: cargo release (version bump + tag + publish of the 13"
echo "    publish-set members in dependency order) runs BEFORE this script;"
echo "    the metrics and blockchain tools are feature-gated and off by default"
echo "    on crates.io — release binaries build them via"
echo "    --features metrics,blockchain)"
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
