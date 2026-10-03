#!/usr/bin/env bash
# MCP client conformance runner.
#
# Runs the official `@modelcontextprotocol/conformance` client suite against the
# `choreo-mcp` conformance harness (`mcp-conformance-client`), diffed against the
# committed expected-failures baseline. The suite starts a scenario server, runs
# the harness with the server URL as its only argument (and the scenario name in
# `MCP_CONFORMANCE_SCENARIO`), and checks the wire traffic against the spec.
#
# The suite version is PINNED so the baseline is meaningful: a new suite release
# can add scenarios, which must be triaged (pass, or baselined) before bumping.
# Override with MCP_CONFORMANCE_VERSION / MCP_CONFORMANCE_SPEC_VERSION.
#
# Usage:
#   scripts/mcp-conformance.sh                 # the full client suite, baselined
#   scripts/mcp-conformance.sh --scenario tools_call   # one scenario
#
# Requires Node.js (npx) and network access; CI provides both.
set -euo pipefail

CONFORMANCE_VERSION="${MCP_CONFORMANCE_VERSION:-0.1.16}"
SPEC_VERSION="${MCP_CONFORMANCE_SPEC_VERSION:-2025-11-25}"

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$REPO_ROOT"

BASELINE="choreo-mcp/tests/conformance/expected-failures.yml"

if ! command -v npx >/dev/null 2>&1; then
    echo "error: npx not found — the conformance suite is an npm package (install Node.js)" >&2
    exit 1
fi

echo "==> building the conformance harness (mcp-conformance-client)"
cargo build -p choreo-mcp --bin mcp-conformance-client

# The suite invokes the binary directly (it appends the server URL as an
# argument), so it must be a path, not a `cargo run` command.
TARGET_DIR="${CARGO_TARGET_DIR:-$REPO_ROOT/target}"
BIN="$TARGET_DIR/debug/mcp-conformance-client"
if [ ! -x "$BIN" ]; then
    echo "error: conformance harness not found at $BIN" >&2
    exit 1
fi

echo "==> running @modelcontextprotocol/conformance@$CONFORMANCE_VERSION (client, spec $SPEC_VERSION)"
if [ "$#" -gt 0 ]; then
    # A caller-specified scenario/suite/flag wins; do not also pass --suite.
    exec npx -y "@modelcontextprotocol/conformance@$CONFORMANCE_VERSION" client \
        --command "$BIN" \
        --spec-version "$SPEC_VERSION" \
        --expected-failures "$BASELINE" \
        "$@"
else
    exec npx -y "@modelcontextprotocol/conformance@$CONFORMANCE_VERSION" client \
        --command "$BIN" \
        --spec-version "$SPEC_VERSION" \
        --expected-failures "$BASELINE" \
        --suite all
fi
