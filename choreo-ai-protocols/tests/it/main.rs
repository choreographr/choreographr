//! Single integration-test binary for `choreo-ai-protocols`.
//!
//! Every integration test in this crate is a module here rather than its own
//! `tests/*.rs` target. Cargo compiles each `tests/*.rs` file into a SEPARATE
//! test binary that statically links the whole library, so a crate with N test
//! files rebuilds N binaries — and clippy re-checks N test crates — on every
//! change to the library or its dependencies. Folding them into one
//! `tests/it/main.rs` collapses that fan-out to a single compile + link.
//!
//! Test behaviour is unchanged: cargo-nextest runs every `#[test]` in its own
//! process (process-per-test is a nextest property, not a per-binary one), so
//! tests that rely on process isolation keep it.
//!
//! A former `tests/foo.rs` keeps its file-level `#![cfg(...)]` / `#![allow(...)]`
//! inner attributes — inside a module they apply to that module.

mod images_wire;
mod modelsdev_refresh;
mod reasoning_roundtrip;
mod socket_registry_wire;
