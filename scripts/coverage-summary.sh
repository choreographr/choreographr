#!/usr/bin/env bash
# scripts/coverage-summary.sh — render an lcov coverage report as a per-crate
# Markdown table of line coverage.
#
# `just coverage-lcov` (the `cargo coverage` alias) writes one lcov file for the
# whole `test-all` suite. cargo-llvm-cov can print its own summary, but only from
# the same invocation that ran the tests — a later `cargo llvm-cov report` does
# not find the nextest binaries in the nightly build-dir layout (see the alias in
# .cargo/config.toml) — so the summary is derived from the lcov file instead,
# and one test run yields both the machine-readable report and this table. The
# coverage CI workflow (.github/workflows/coverage.yml) appends the table to the
# job summary; locally it is just printed.
#
# Usage:
#   scripts/coverage-summary.sh target/coverage/lcov.info
#
# Rows are grouped by the first path component under the workspace root (each
# workspace member is a top-level directory), sorted by size, with the root
# `choreographr` package's `src/` reported as `choreographr`. Files outside the
# workspace (registry sources, the toolchain) never appear: cargo-llvm-cov
# already filters them out of the lcov file.
set -euo pipefail

lcov=${1:?usage: coverage-summary.sh <lcov.info>}
[ -f "$lcov" ] || { echo "error: no lcov file at $lcov" >&2; exit 1; }
root=$(cd "$(dirname "$0")/.." && pwd)

# lcov records: `SF:<path>` opens a file, `LF:` is its instrumented line count,
# `LH:` its hit line count. Sum both per crate; `LF:0` files (no executable
# lines) add nothing, so they need no special case.
awk -v root="$root/" '
    /^SF:/ {
        path = substr($0, 4)
        if (index(path, root) == 1) path = substr(path, length(root) + 1)
        split(path, part, "/")
        crate = (part[1] == "src") ? "choreographr" : part[1]
    }
    /^LF:/ { found[crate] += substr($0, 4); total_found += substr($0, 4) }
    /^LH:/ { hit[crate] += substr($0, 4); total_hit += substr($0, 4) }
    END {
        if (total_found == 0) { print "error: lcov file has no instrumented lines" > "/dev/stderr"; exit 1 }
        print "| Crate | Lines | Covered | Line coverage |"
        print "|---|---:|---:|---:|"
        # Largest crates first: they dominate the total, so they lead the table.
        n = 0
        for (c in found) order[++n] = c
        for (i = 2; i <= n; i++) {
            c = order[i]
            for (j = i - 1; j >= 1 && found[order[j]] < found[c]; j--) order[j + 1] = order[j]
            order[j + 1] = c
        }
        for (i = 1; i <= n; i++) {
            c = order[i]
            if (found[c] == 0) continue
            printf "| %s | %d | %d | %.1f%% |\n", c, found[c], hit[c], 100 * hit[c] / found[c]
        }
        printf "| **Total** | **%d** | **%d** | **%.1f%%** |\n", total_found, total_hit, 100 * total_hit / total_found
    }
' "$lcov"
