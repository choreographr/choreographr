//! Core server for the Choreographr suite (the library half of the shipped
//! `choreographr` binary).
//!
//! This crate is the engine behind every client. It owns the session tree, the
//! tool registry, the provider connections, and the durable stores (the redb
//! database, the accounts and daemon config, the catalog cache), and it exposes
//! them over three transports: a Unix socket, TCP behind the Noise IK/XX
//! encrypted handshake, and an in-process [embedded link](embedded).
//!
//! ## Concurrency model
//!
//! The daemon is pure OS threads with message passing (an actor model): one
//! command-loop thread owns [`DaemonState`] and every
//! session has its own control thread, with crossbeam channels carrying
//! [`DaemonCommand`] and
//! [`SessionCommand`] between them. No async code runs
//! in the daemon's own logic; the only tokio runtimes live in the optional
//! `blockchain` and `content` features, whose crates expose blocking
//! `execute_*` entry points the daemon calls directly.
//!
//! ## Public surface
//!
//! The modules are exposed so an embedder (the GUI's on-device daemon) can open
//! state, register tools, and drive the server without the CLI. The crate root
//! re-exports the types the embedder and the integration tests use most
//! ([`DaemonCommand`], [`OpenOptions`],
//! [`run_server`], [`spawn_embedded`],
//! and the tool argument types and `execute_*` functions).

// Part of the ARCHITECTURE.md → rustdoc migration (see AGENTS.md → Documentation):
// every public item carries docs, enforced as a hard error by clippy-strict's
// `-D warnings`.
#![warn(missing_docs)]

pub mod accounts;
pub mod broadcast;
pub mod cache_warm;
pub mod catalog;
pub mod cli;
pub use cli::main;
pub mod config;
pub mod config_watch;
pub mod context;
pub mod daemon;
pub mod db;
pub mod diff_util;
pub mod embedded;
pub mod image_prep;
pub mod mcp;
pub mod metrics;
pub mod migrate;
pub mod providers;
mod reasoning;
mod requests;
pub mod server;
mod sessions;
pub mod tools;

pub use crate::daemon::{DaemonCommand, DaemonState, OpenOptions};
pub use crate::embedded::{EmbeddedDaemon, EmbeddedLink, EmbeddedOptions, spawn_embedded};
#[cfg(feature = "test-utils")]
pub use crate::reasoning::build_chat_request_messages;
pub use crate::server::run_server;
#[cfg(feature = "test-utils")]
pub use crate::sessions::join_session_shutdown_with_grace_for_test;
pub use crate::sessions::{
    ActiveSessionEntry, AssistantResponse, ChildResult, RequestContext, SessionCommand,
    SessionMetadata, SessionState, session_main,
};
pub use crate::tools::ToolPolicy;
pub use crate::tools::exec::{ExecArgs, execute_exec_tool};
pub use crate::tools::find::{FindArgs, execute_find_tool};
pub use crate::tools::fish::{FishArgs, execute_fish_tool};
pub use crate::tools::fs::{
    EditFileArgs, ListFilesArgs, TextEditArgs, WriteFileArgs, execute_edit_file_tool,
    execute_list_files_tool, execute_write_file_tool,
};
pub use crate::tools::git::{
    GitAddArgs, GitCommitArgs, GitDiffArgs, GitLogArgs, GitPushArgs, GitRepoArgs, GitShowArgs,
    execute_git_add_tool, execute_git_commit_tool, execute_git_diff_tool, execute_git_log_tool,
    execute_git_push_tool, execute_git_show_tool, execute_git_status_tool,
};
pub use crate::tools::grep::{GrepArgs, GrepOutputMode, execute_grep_tool};
pub use crate::tools::nu::{NuArgs, execute_nu_tool};
// PDF tools — feature-gated (see tools/mod.rs's `mod pdf` comment).
#[cfg(feature = "pdf")]
pub use crate::tools::pdf::{
    PdfClassifyArgs, PdfToMarkdownArgs, execute_pdf_classify, execute_pdf_to_markdown,
};
pub use crate::tools::sh::{ShArgs, execute_sh_tool};
#[cfg(test)]
pub(crate) use crate::tools::sha256_hex;
pub use crate::tools::vm::{RunRiscVInput, execute_run_riscv_tool};

#[cfg(test)]
mod tests;
