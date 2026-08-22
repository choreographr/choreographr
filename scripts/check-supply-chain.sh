#!/usr/bin/env bash
# scripts/check-supply-chain.sh — the workspace's dependency supply-chain gate.
#
# Wired into `just check-supply-chain`, `just pre-commit`, and `just ci`;
# also runs from a plain `./scripts/check-supply-chain.sh`. It exists because
# of the 2026-08-20 `arrayref` supply-chain attack
# (https://blog.rust-lang.org/2026/08/20/supply-chain-attack-on-arrayref/):
# a compromised maintainer republished a popular crate (arrayref@0.3.10) with
# a dependency on payload-downloading crates, online for ~86 minutes before
# deletion. Three independent layers guard against that class of compromise:
#
#   1. Cache scan — the Rust blog's own remediation `find`, run fresh: the
#      attacker's versions were DELETED from crates.io, so no dependency graph
#      tool can see them anymore, but a machine that downloaded one during the
#      ~2h window still has the .crate file sitting in ~/.cargo/registry/cache.
#      Neither cargo-deny nor cargo-audit inspects idle cache files — this does.
#   2. cargo-deny (deny.toml, preferred) — hard BANS on the attacker's versions
#      (fails even if the lockfile is regenerated), RustSec advisories
#      (vulnerability/malicious always fail in 0.20), and a crates.io-only
#      source restriction. This is the authoritative dependency-graph check.
#   3. cargo-audit fallback — same RustSec DB when cargo-deny isn't installed,
#      plus a literal lockfile scan for the banned names/versions.
#
# Exits non-zero if any layer finds a hit. Requires cargo-deny or cargo-audit
# (install with `cargo install cargo-deny`, or `just install-cargo-deny`).
set -euo pipefail
cd "$(dirname "$0")/.."

MALICIOUS_CACHE_PATTERNS=(
    'append-only-vec-0.1.9.crate'
    'arrayref-0.3.10.crate'
    'internment-0.8.7.crate'
    'proc-macro1-*.crate'
    'proc-macro-en-*.crate'
    'aovine-*.crate'
    'arone-*.crate'
    'aronenao-*.crate'
    'tinymember-*.crate'
)

# The pinned attacker versions, for the no-cargo-deny fallback lockfile scan.
BANNED_LOCKFILE=( 'arrayref 0.3.10' 'internment 0.8.7' 'append-only-vec 0.1.9' )
# The six attacker crates never shipped anything legitimate — ANY version is
# malicious, so their bare names are banned too.
BANNED_NAMES=( proc-macro1 proc-macro-en aovine arone aronenao tinymember )

echo "==> layer 1/3: local cargo registry scan for deleted malicious crate files"
registry="${CARGO_HOME:-$HOME/.cargo}/registry"
cache_hits=0
if [ -d "$registry" ]; then
    for pat in "${MALICIOUS_CACHE_PATTERNS[@]}"; do
        while IFS= read -r f; do
            cache_hits=1
            echo "  ! known-malicious crate in local registry: $f"
        done < <(find "$registry" -type f -name "$pat" 2>/dev/null)
    done
else
    echo "    no local cargo registry yet (fresh machine) — scan skipped"
fi
if [ "$cache_hits" -ne 0 ]; then
    echo "error: local cargo registry contains a version from the 2026-08-20 attack." >&2
    echo "      delete the file(s) above, then \`cargo update -p <crate>\` to re-resolve." >&2
    exit 1
fi

echo "==> layer 2/3: dependency-graph check (deny.toml bans + RustSec advisories)"
if command -v cargo-deny >/dev/null 2>&1; then
    cargo deny check advisories bans sources
elif command -v cargo-audit >/dev/null 2>&1; then
    echo "    cargo-deny not installed — falling back to cargo-audit + lockfile scan"
    echo "    (install cargo-deny for hard version bans and the source restriction:"
    echo "     \`cargo install cargo-deny\` or \`just install-cargo-deny\`)"
    cargo audit
    echo "==> layer 2b/3: literal Cargo.lock scan for banned names/versions"
    # denormalize the lockfile to "<name> <version>" lines and look for hits.
    bad=0
    while read -r hv; do
        for ban in "${BANNED_LOCKFILE[@]}" "${BANNED_NAMES[@]}"; do
            case "$hv" in
                "$ban"*) echo "  ! banned dependency in Cargo.lock: $hv"; bad=1 ;;
            esac
        done
    done < <(awk '
        /^name = /    { name = $3; gsub(/"/, "", name) }
        /^version = / { ver = $3; gsub(/"/, "", ver); print name, ver }
    ' Cargo.lock)
    [ "$bad" -eq 0 ] || { echo "error: Cargo.lock contains a banned dependency (see deny.toml)." >&2; exit 1; }
else
    echo "error: neither cargo-deny nor cargo-audit is installed." >&2
    echo "      install cargo-deny: \`cargo install cargo-deny\` or \`just install-cargo-deny\`" >&2
    exit 1
fi

echo "==> supply-chain checks passed"