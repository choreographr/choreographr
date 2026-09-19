//! Single integration-test binary for `choreo-daemon`.
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
//! Files that declared `mod common;` now `use crate::common;`: `common` is
//! declared once here at the crate root instead of being recompiled into each
//! former test binary.

mod common;

mod acl_hot_reload;
mod cancel_force_close;
mod config_watch;
mod ctrlc_after_connection;
mod daemon_client_noise;
mod daemon_client_unix;
mod db_migrations;
mod edit_file_integration;
mod embedded;
mod exec_tool_integration;
mod find_integration;
mod fish_tool_integration;
mod git_tool_integration;
mod grep_integration;
mod http_tool_tests;
mod image_gen_integration;
mod lifecycle_integration;
mod list_files_integration;
mod mcp_integration;
mod metrics_integration;
mod nu_tool_integration;
mod pdf_tool_integration;
mod reasoning_roundtrip_integration;
mod retrieve_webpage_integration;
mod retry_cancellation;
mod session_integration;
mod session_shutdown;
mod sh_tool_integration;
mod shell_streaming_integration;
mod spawn_subsession_integration;
mod stream_integrity;
mod vm_integration;
mod write_file_integration;
