#!/usr/bin/env bash
# scripts/check-release-name.sh — keep the release-name source of truth and the
# CHANGELOG heading in sync.
#
# `choreo-proto/release-name.txt` is the machine source of truth (compiled into
# the binaries AND read by the CI release job for the release title). The
# CHANGELOG heading for the current version records the same dance-style name
# for humans (`## [X.Y.Z] - YYYY-MM-DD (Lindy)`). Nothing else ties the two
# together, so this guard fails loudly on drift: a conductor who bumps to a new
# minor series must update BOTH (or neither — patch releases keep the existing
# name, so both stay equal).
#
# Empty == empty passes: the unnamed 0.1.0 series has an empty name file and a
# bare `## [0.1.0]` heading.
#
# Wired into `just check-release-name` and the release workflow (before "Create
# the GitHub release").
set -euo pipefail
cd "$(dirname "$0")/.."

# The version the rest of the release tooling keys off — the workspace manifest.
VER="$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -n1)"
if [ -z "$VER" ]; then
    echo "error: could not read the workspace version from Cargo.toml" >&2
    exit 1
fi

# The name baked into the binaries: first line of the source-of-truth file,
# trimmed of surrounding whitespace (empty when the file is empty).
FILE="$(head -n1 choreo-proto/release-name.txt 2>/dev/null | sed -e 's/^[[:space:]]*//' -e 's/[[:space:]]*$//' || true)"

# The name recorded on the CHANGELOG heading for this version — the `(Name)`
# substring, or empty when the heading is absent or carries no parentheses.
HEAD="$(awk -v ver="$VER" 'index($0,"## ["ver"]")==1 { if (match($0,/\([^)]*\)/)) print substr($0,RSTART+1,RLENGTH-2); exit }' CHANGELOG.md)"

if [ "$FILE" != "$HEAD" ]; then
    echo "error: release-name drift detected" >&2
    echo "  choreo-proto/release-name.txt: '${FILE}'" >&2
    echo "  CHANGELOG.md ## [${VER}]:      '${HEAD}'" >&2
    echo "Update both (major/minor sets a new name; patch releases keep it)." >&2
    exit 1
fi

if [ -n "$FILE" ]; then
    echo "OK: release name '${FILE}' in sync for ${VER}"
else
    echo "OK: no release name for ${VER} (unnamed series)"
fi
