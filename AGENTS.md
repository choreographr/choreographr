# Agent Instructions

## Refactoring

Always try to refactor when implementing new features. Look for opportunities to improve code structure, reduce duplication, and simplify existing code alongside any additions.

## Documentation

When making changes, ensure [ARCHITECTURE.md](./ARCHITECTURE.md) and [README.md](./README.md) are kept up to date. If a change affects the architectural decisions, module structure, data flow, or any other documented aspect, update the files accordingly.

## Test Discipline

- **Unit tests** (in <code>src/</code> <code>#[cfg(test)]</code> modules) must never use time-based waits (`sleep`, `delay_for`, etc.). Use deterministic patterns only.
- **Integration tests** (tests that bind network sockets, spawn external processes, use `UnixStream::pair()` to exercise the full handler pipeline, or perform filesystem I/O exercising the system boundary) belong in crate-level `tests/` directories, not in `src/`.
- Integration tests are marked <code>#[ignore]</code>. <code>cargo test</code> runs only unit tests. To run integration tests: <code>cargo test -- --ignored</code>.

## Task Execution

When implementing a list of code changes across multiple files, delegate each task to a subagent and run them in series (one at a time), not in parallel. This avoids filesystem conflicts from concurrent edits to overlapping files and keeps each subagent's context focused. Subagents should verify their work by running `cargo check --workspace` and `cargo test --workspace` after making changes.

## Dependency Management

Always use the latest stable version of crates where possible. When adding or upgrading a dependency:

1. Use the latest stable semver-compatible release for each crate (check `cargo search <name> --limit 1` for the current version).
2. If a dependency is locked to an older version upstream, accept the duplication rather than patching — upstream issues should resolve naturally over time.
3. If a dependency is used by two or more workspace members, declare it in `[workspace.dependencies]` and reference it with `dep.workspace = true` in member crates. This is not optional — when adding a crate-level dependency that already exists (or is being introduced simultaneously) in another workspace member, promote it to the workspace and update both crates in the same change.
