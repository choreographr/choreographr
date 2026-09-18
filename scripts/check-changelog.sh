#!/usr/bin/env bash
# scripts/check-changelog.sh — keep CHANGELOG.md structurally well-formed.
#
# The release workflow extracts a version's `## [X.Y.Z]` section and ships it
# verbatim as the GitHub release body (`.github/workflows/release.yml`, "Extract
# the changelog section"). That extraction is heading-agnostic: it does NOT care
# whether a section repeats a category heading — it just prints every line up to
# the next `## `. So a section with two `### Fixed` blocks still "extracts", but
# the rendered release page shows two Fixed sections — malformed per the
# AGENTS.md changelog rules ("One heading per category, at most ... never open a
# second `### Changed` (or any other) block"). Nothing in the toolchain caught
# this until now: a fresh `### Fixed` was inserted at the top of `[Unreleased]`
# instead of appending to the existing `### Fixed` further down, and every later
# commit added to the new top block, entrenching the duplicate.
#
# This guard makes the rule enforceable. It fails if, within any `## [...]`
# section:
#   * a category heading appears more than once, or
#   * a `### ` heading is not one of the Keep a Changelog categories, or
#   * a category heading has no entries (an empty section — AGENTS.md: "do not
#     add an empty one").
#
# Wired into `just check-changelog`, `just pre-commit`, and the release workflow
# (before the changelog section is extracted).
set -euo pipefail
cd "$(dirname "$0")/.."

[ -f CHANGELOG.md ] || {
    echo "error: CHANGELOG.md not found" >&2
    exit 1
}

violations="$(
    awk '
        # Keep a Changelog category headings; anything else under a "## " is a
        # typo or a stray sub-heading the release extractor would ship as-is.
        function allowed(c) {
            return c == "Added" || c == "Changed" || c == "Deprecated" \
                || c == "Removed" || c == "Fixed" || c == "Security"
        }
        # A category with zero "- " entries is an empty block.
        function check_cat() {
            if (curcat != "" && bullets == 0)
                printf "error: %s has an empty %s section (no entries)\n", section, curcat
        }
        function check_section() {
            check_cat()
            for (c in seen)
                if (seen[c] > 1)
                    printf "error: %s repeats the %s category heading %d times\n", section, c, seen[c]
        }
        /^## / {
            check_section()
            delete seen
            section = $0
            curcat = ""
            bullets = 0
            next
        }
        /^### / {
            check_cat()
            curcat = substr($0, 5)
            sub(/[[:space:]]+$/, "", curcat)
            if (!allowed(curcat))
                printf "error: %s has an unknown category heading: %s\n", section, $0
            seen[curcat]++
            bullets = 0
            next
        }
        /^- / { bullets++ }
        END { check_section() }
    ' CHANGELOG.md
)"

if [ -n "$violations" ]; then
    echo "$violations" >&2
    echo "" >&2
    echo "CHANGELOG.md is malformed — see the AGENTS.md changelog rules:" >&2
    echo "  append each bullet under the matching EXISTING category heading;" >&2
    echo "  never open a second heading for a category that already exists." >&2
    exit 1
fi

echo "OK: CHANGELOG.md is well-formed"
