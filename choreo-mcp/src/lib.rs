//! Model Context Protocol (MCP) client for Choreographr.
//!
//! This crate implements the client half of the [Model Context Protocol] on top
//! of the official Rust SDK, [`rmcp`](https://docs.rs/rmcp). It connects to an
//! external MCP server over the **stdio** transport, negotiates the protocol era
//! (`server/discover` with an `initialize` fallback, or one pinned era), lists
//! the server's tools, and invokes them on the model's behalf. The daemon
//! depends on it behind its `mcp` cargo feature (off by default) and registers
//! thin `Tool` wrappers over [`McpServerHandle`].
//!
//! Because `rmcp` is async and the daemon is thread-only, this crate owns the
//! sidecar tokio runtime for its async client ([`runtime`]) and a per-server
//! **dispatcher thread** ([`session`]) that turns every operation into a
//! blocking, channel-based round-trip. The daemon never sees an `rmcp` type:
//! the boundary is typed entirely on this crate's own [`McpServer`],
//! [`McpServerHandle`], [`McpTool`], and [`CallToolResult`].
//!
//! The split is deliberate: [`protocol`] owns the daemon-facing value types,
//! [`config`] the per-server configuration and protocol-era selection, [`error`]
//! the shared failure type, [`runtime`] the async sidecar, [`session`] the
//! dispatcher + blocking facade, and the private `engine` module the `rmcp`
//! plumbing.
//!
//! [Model Context Protocol]: https://modelcontextprotocol.io

// Part of the ARCHITECTURE.md → rustdoc migration (see AGENTS.md → Documentation):
// every public item carries docs, enforced as a hard error by clippy-strict's
// `-D warnings`.
#![warn(missing_docs)]

pub mod config;
mod engine;
pub mod error;
pub mod protocol;
pub mod runtime;
pub mod session;

pub use config::{DEFAULT_TIMEOUT, McpProtocolMode, McpServerConfig};
pub use error::McpError;
pub use protocol::{
    CallToolResult, EMPTY_INPUT_SCHEMA, MAX_SCHEMA_BYTES, McpContent, McpTool,
    normalize_input_schema,
};
pub use session::{McpServer, McpServerHandle};
