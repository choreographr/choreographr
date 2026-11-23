//! Single integration-test binary for `choreo-transport`.
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
//!
//! The modules split by subject: `handshake` (the exchange, the mode dispatch
//! and the deadline), `data_plane` (framing, fragmentation and the caps),
//! `socket_policy` (the control-plane `TCP_NODELAY` + keepalive policy), and `common` — the shared
//! keypair/listener harness and the raw handshake halves a test needs to put
//! bytes on the wire the public API cannot express.

mod common;
mod data_plane;
mod handshake;
mod socket_policy;
