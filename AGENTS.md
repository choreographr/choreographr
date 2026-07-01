# Agent Instructions

## Refactoring

Always try to refactor when implementing new features. Look for opportunities to improve code structure, reduce duplication, and simplify existing code alongside any additions.

## Architecture Documentation

When making changes, ensure [ARCHITECTURE.md](./ARCHITECTURE.md) is kept up to date. If a change affects the architectural decisions, module structure, data flow, or any other documented aspect, update the file accordingly.

## Dependency Management

Always use the latest stable version of crates where possible, and avoid introducing duplicate versions of the same crate. When adding or upgrading a dependency:

1. Check `cargo tree --duplicates` to understand the current duplication landscape before making changes.
2. Use the latest stable semver-compatible release for each crate (check `cargo search <name> --limit 1` for the current version).
3. Match the version already used by the workspace's major upstream crates (alloy, subxt, dioxus, reqwest, gix). Prefer the higher version when both old and new are present.
4. If a dependency is locked to an older version upstream, accept the duplication rather than patching — upstream issues should resolve naturally over time.
5. After adding or upgrading a dependency, run `cargo tree --duplicates` to verify no new duplication was introduced.
