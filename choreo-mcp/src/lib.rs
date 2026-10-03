//! Model Context Protocol (MCP) client for Choreographr.
//!
//! This crate implements the client half of the [Model Context Protocol] on top
//! of the official Rust SDK, [`rmcp`](https://docs.rs/rmcp). It connects to an
//! external MCP server over either the **stdio** transport (a child subprocess)
//! or the **Streamable HTTP** transport (a remote POST endpoint), negotiates the
//! protocol era (`server/discover` with an `initialize` fallback, or one pinned
//! era), lists the server's tools, and invokes them on the model's behalf. When
//! the negotiated era and the server's capabilities allow it, it also opens a
//! `subscriptions/listen` stream and forwards each list-changed event as a
//! [`McpListChange`] so the daemon can refresh its tool catalogue live. The
//! daemon depends on it behind its `mcp` cargo feature (on by default; an
//! embedder opts out with `default-features = false`) and registers thin `Tool`
//! wrappers over [`McpServerHandle`].
//!
//! Because `rmcp` is async and the daemon is thread-only, this crate owns the
//! sidecar tokio runtime for its async client ([`runtime`]) and a per-server
//! **dispatcher thread** ([`session`]) that turns every operation into a
//! blocking, channel-based round-trip. The daemon never sees an `rmcp` type:
//! the boundary is typed entirely on this crate's own [`McpServer`],
//! [`McpServerHandle`], [`McpTool`], and [`CallToolResult`].
//!
//! The split is deliberate: [`protocol`] owns the daemon-facing value types,
//! [`config`] the per-server configuration (transport + protocol era), [`error`]
//! the shared failure type, [`naming`] the provider-safe tool/group name
//! construction, [`runtime`] the async sidecar, [`session`] the dispatcher +
//! blocking facade, and the private `engine` / `retry` / `stdio` modules the
//! `rmcp` plumbing, the HTTP connect-retry policy, and the capped child-process
//! transport.
//!
//! [Model Context Protocol]: https://modelcontextprotocol.io

// Part of the ARCHITECTURE.md → rustdoc migration (see AGENTS.md → Documentation):
// every public item carries docs, enforced as a hard error by clippy-strict's
// `-D warnings`.
#![warn(missing_docs)]

pub mod config;
mod engine;
pub mod error;
pub mod naming;
pub mod protocol;
mod retry;
pub mod runtime;
pub mod session;
mod stdio;

pub use config::{
    DEFAULT_MAX_CONCURRENT_CALLS, DEFAULT_MAX_RESTARTS, DEFAULT_TIMEOUT, McpProtocolMode,
    McpServerConfig, McpTransport, McpTransportKind,
};
pub use error::McpError;
pub use naming::{
    MAX_TOOL_NAME_LEN, TOOL_NAME_PREFIX, build_tool_name, build_tool_name_with_suffix, group_name,
    sanitize_segment, short_hash,
};
pub use protocol::{
    CallToolResult, EMPTY_INPUT_SCHEMA, MAX_SCHEMA_BYTES, MAX_SCHEMA_DEPTH, MAX_TOOLS_PER_SERVER,
    McpContent, McpListChange, McpListKind, McpResource, McpTool, cap_tools, json_depth,
    normalize_input_schema, normalize_output_schema,
};
pub use session::{McpServer, McpServerHandle};
pub use stdio::MAX_STDIO_FRAME_BYTES;
