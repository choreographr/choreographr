//! Model Context Protocol (MCP) client for Choreographr.
//!
//! This crate implements the client half of the [Model Context Protocol]: it
//! speaks JSON-RPC 2.0 over a transport (stdio child process or HTTP) to an
//! external MCP server, discovers the tools that server exposes, and invokes
//! them on the model's behalf. The daemon depends on it behind its `mcp` cargo
//! feature (off by default) and registers thin `Tool` wrappers over
//! [`McpClient`].
//!
//! The split is deliberate: [`protocol`] owns the wire types, [`transport`] the
//! byte-level framing and lifecycle of a connection, [`client`] the request /
//! response correlation and handshake, and [`error`] the shared failure type.
//!
//! [Model Context Protocol]: https://modelcontextprotocol.io

// Part of the ARCHITECTURE.md → rustdoc migration (see AGENTS.md → Documentation):
// every public item carries docs, enforced as a hard error by clippy-strict's
// `-D warnings`.
#![warn(missing_docs)]

pub mod client;
pub mod error;
pub mod protocol;
pub mod transport;

pub use client::{McpClient, McpServerConfig};
pub use error::McpError;
pub use protocol::{CallToolResult, McpContent, McpTool};
