#!/usr/bin/env bash
# scripts/release-notes.sh — build a version's release notes from commit
# messages with git-cliff.
#
# The commit messages ARE the changelog (AGENTS.md → Documentation → "Release
# notes via commits"): git-cliff (cliff.toml) renders each release-worthy
# commit as a Keep a Changelog bullet — the subject in bold, the body as its
# indented paragraph. This script is the ONE place that invocation lives, shared
# by the CI `release` job (`.github/workflows/release.yml`), `just release-notes`,
# and RELEASE.md's manual fallback, so the pipeline and a local preview cannot
# drift. There is no hand-maintained CHANGELOG.md — it was retired.
#
# Usage:
#   scripts/release-notes.sh            # the workspace version in Cargo.toml
#   scripts/release-notes.sh 0.3.0      # preview the upcoming version (pre-tag)
#   scripts/release-notes.sh v0.3.0     # a leading `v` is accepted too
#
# Output goes to stdout, so CI redirects it to --notes-file and a human just
# reads it. The section is rendered with `--strip header` (drop the file-level
# "# Changelog" intro; keep the `## [X.Y.Z] - DATE` heading). A trailing
# `**Full Changelog**` compare link is appended here rather than via git-cliff's
# `footer`, because git-cliff only populates its `previous` context when several
# releases are rendered in one pass — and we render exactly one.
set -euo pipefail
cd "$(dirname "$0")/.."

command -v git-cliff >/dev/null 2>&1 || {
    echo "error: git-cliff not found — install it (\`just install-git-cliff\`, or 'cargo install git-cliff')" >&2
    exit 1
}

VER="${1:-$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -n1)}"
VER="${VER#v}"
[ -n "$VER" ] || { echo "error: could not determine the version (pass it explicitly)" >&2; exit 1; }

# Two modes, matching how the notes are produced in practice:
#   * CI runs at the tag push, so HEAD is exactly tagged `vVER` → render that
#     tag's section (`--latest`), and the previous release is the second-newest
#     `v*` tag.
#   * A pre-tag preview has no `vVER` tag yet → render the unreleased commits
#     under the not-yet-created tag, and the previous release is the newest
#     existing `v*` tag.
if [ "$(git describe --tags --exact-match 2>/dev/null || true)" = "v${VER}" ]; then
    range=(--latest)
    prev="$(git tag -l 'v[0-9]*' --sort=-v:refname | sed -n '2p')"
else
    range=(--unreleased --tag "v${VER}")
    prev="$(git tag -l 'v[0-9]*' --sort=-v:refname | sed -n '1p')"
fi

git-cliff "${range[@]}" --strip header

# Compare link, matching GitHub's own "Full Changelog" line. Omitted only for the
# very first release (no previous tag).
if [ -n "$prev" ]; then
    printf '\n**Full Changelog**: https://github.com/choreographr/choreographr/compare/%s...v%s\n' \
        "$prev" "$VER"
fi
