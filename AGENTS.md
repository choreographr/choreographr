# Agent Instructions

## Refactoring

Always try to refactor when implementing new features. Look for opportunities to improve code structure, reduce duplication, and simplify existing code alongside any additions.

## Documentation

When making changes, ensure [ARCHITECTURE.md](./ARCHITECTURE.md) and [README.md](./README.md) are kept up to date. If a change affects the architectural decisions, module structure, data flow, or any other documented aspect, update the files accordingly.

## Test Discipline

- **Unit tests** (in <code>src/</code> <code>#[cfg(test)]</code> modules) must never use time-based waits (`sleep`, `delay_for`, etc.). Use deterministic patterns only.
- **Integration tests** (tests that bind network sockets, spawn external processes, use `UnixStream::pair()` to exercise the full handler pipeline, or perform filesystem I/O exercising the system boundary) belong in crate-level `tests/` directories, not in `src/`.
- Integration tests are marked <code>#[ignore]</code>. Use the nextest aliases defined in <code>.cargo/config.toml</code>: <code>cargo test-fast</code> (unit tests), <code>cargo test-integration</code> (the <code>#[ignore]</code> suite), and <code>cargo test-all</code> (everything in one pass). Plain <code>cargo test</code> runs libtest (serialized) and is only a fallback when nextest is unavailable.

## Task Execution

When implementing a list of code changes across multiple files, delegate each task to a subagent and run them in series (one at a time), not in parallel. This avoids filesystem conflicts from concurrent edits to overlapping files and keeps each subagent's context focused. Subagents should verify their work by running `cargo nextest run -p <crates>` on only the crates they modified. (The `cargo test-*` aliases bake in `--workspace` and reject `-p`, so call nextest directly when targeting specific crates.)

## Dependency Management

Always use the latest stable version of crates where possible. When adding or upgrading a dependency:

1. Use the latest stable semver-compatible release for each crate (check `cargo search <name> --limit 1` for the current version).
2. If a dependency is locked to an older version upstream, accept the duplication rather than patching — upstream issues should resolve naturally over time.
3. If a dependency is used by two or more workspace members, declare it in `[workspace.dependencies]` and reference it with `dep.workspace = true` in member crates. This is not optional — when adding a crate-level dependency that already exists (or is being introduced simultaneously) in another workspace member, promote it to the workspace and update both crates in the same change.

## Testing New Code

Always write unit and/or integration tests for any new code added to the codebase. Unit tests belong in `src/` `#[cfg(test)]` modules; integration tests belong in crate-level `tests/` directories. Follow the conventions in the **Test Discipline** section above.

## Error Handling

Never use `expect()`, `unwrap()`, or `panic!()` in production code. These create crash surfaces that can take down the daemon. Follow these rules:

1. **Library crates** — define structured error types with `thiserror` and propagate errors with `?`.
2. **Binary crates** — use `anyhow::Context` / `.context()` to attach meaningful context to errors at key boundaries, then propagate with `?`.
3. **Infallible operations** — if an operation truly cannot fail, use `unwrap_or_default()` or `unwrap_or(fallback)` rather than bare `unwrap()`.
4. **Mutex poisoning** — use `.lock().unwrap_or_else(|e| e.into_inner())` to recover from poisoned mutexes instead of panicking.
5. **`unwrap()`/`expect()`/`panic!()` are permitted only in `#[cfg(test)]` modules and `tests/` integration test files.**

## Logging

All crates in the workspace (`choreographr`, `choreo-client-core`, `choreo-keystore`, `choreo-im`, `choreo-gui`, `choreo-markdown`, `choreo-proto`, `choreo-tui`) must log extensively using the `tracing` crate. Every module should emit `tracing` events (`info!`, `warn!`, `error!`, `debug!`, `trace!`) at appropriate levels to provide observability into key operations, state transitions, and error conditions.

In the `choreo-tui` crate specifically, do not use `eprintln!` for diagnostics — output goes to `/tmp/choreo-tui.log`.

## Thread Communication

Do not share mutable state between threads. Use message passing (`mpsc` channels) for all cross-thread communication. Shared-state patterns (`Arc<RwLock<…>>`, `Arc<Mutex<…>>`) should be avoided in favor of channel-based designs.

## Inline Comments

Always write inline comments around new code explaining how it works. Focus on the "why" — the reasoning, intent, and non-obvious details — rather than restating what the code literally does.

## Pre-Commit Workflow

Before committing:
1. Run `cargo test-all` — full suite (unit + integration) via nextest must pass
2. Stage changes with `git add`
3. Commit with `git commit`

The `.githooks/pre-commit` hook has been removed. Run `cargo clippy` and `cargo fmt` manually before committing.
