pub mod client;
pub mod error;
pub mod protocol;
pub mod transport;

pub use client::{McpClient, McpServerConfig};
pub use error::McpError;
pub use protocol::{CallToolResult, McpContent, McpTool};
