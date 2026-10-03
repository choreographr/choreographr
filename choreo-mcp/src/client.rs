//! MCP client: spawns a server subprocess, performs the handshake, discovers
//! its tools, and invokes them over a [`StdioTransport`].

use crate::error::McpError;
use crate::protocol::{
    CallToolParams, CallToolResult, JsonRpcNotification, JsonRpcRequest, McpTool,
    make_initialize_request, normalize_input_schema,
};
use crate::transport::StdioTransport;
use serde_json::Value;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

/// Default timeout for MCP tool calls.
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(60);

/// Default timeout for the initialize handshake.
const INIT_TIMEOUT: Duration = Duration::from_secs(10);

/// Default timeout for tools/list.
const LIST_TOOLS_TIMEOUT: Duration = Duration::from_secs(10);

/// Configuration for spawning an MCP server subprocess.
#[derive(Debug, Clone)]
pub struct McpServerConfig {
    /// Stable identifier for this server, used as a tool-name prefix.
    pub slug: String,
    /// The executable to launch (e.g. `npx`, `uvx`, or a server binary).
    pub command: String,
    /// Command-line arguments passed to `command`.
    pub args: Vec<String>,
    /// Extra environment variables for the subprocess.
    pub env: HashMap<String, String>,
    /// Whether this server is enabled for use.
    pub enabled: bool,
    /// Optional per-server request timeout, applied to the handshake, tool
    /// listing, and tool calls. When `None`, the built-in defaults are used.
    pub timeout: Option<Duration>,
}

impl McpServerConfig {
    /// Timeout for the `initialize` handshake: the configured value if set,
    /// otherwise the built-in default.
    fn init_timeout(&self) -> Duration {
        self.timeout.unwrap_or(INIT_TIMEOUT)
    }
}

/// A client connected to a single MCP server subprocess.
pub struct McpClient {
    transport: StdioTransport,
    next_id: AtomicU64,
    server_name: String,
    server_version: String,
    /// Per-server default call timeout (from config), used when a call does not
    /// pass its own.
    timeout: Option<Duration>,
}

impl McpClient {
    /// Spawn the MCP server subprocess and perform the initialize handshake.
    ///
    /// # Errors
    ///
    /// Returns [`McpError::SpawnFailed`] when the subprocess cannot be spawned
    /// or its stdio pipes captured, [`McpError::InitializeFailed`] when the
    /// handshake fails or the response is malformed, and [`McpError::Io`] /
    /// [`McpError::Timeout`] when the wire exchange fails.
    pub fn spawn(config: &McpServerConfig) -> Result<Self, McpError> {
        let mut transport = StdioTransport::spawn(&config.command, &config.args, &config.env)?;

        // Send initialize request.
        let init_req = make_initialize_request(1)?;
        transport.send_request(&init_req)?;

        let resp = transport.recv_response(1, config.init_timeout())?;

        // Check for JSON-RPC error in response.
        if let Some(err) = resp.error {
            return Err(McpError::InitializeFailed(format!(
                "initialize failed: code={} message={}",
                err.code, err.message
            )));
        }

        let result = resp.result.ok_or_else(|| {
            McpError::InitializeFailed("initialize response missing result".into())
        })?;

        let protocol_version = result
            .get("protocolVersion")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown")
            .to_string();

        let server_info = result.get("serverInfo").cloned().unwrap_or_default();
        let server_name = server_info
            .get("name")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown")
            .to_string();
        let server_version = server_info
            .get("version")
            .and_then(|v| v.as_str())
            .unwrap_or("0.0.0")
            .to_string();

        tracing::info!(
            server = %server_name,
            version = %server_version,
            protocol = %protocol_version,
            "MCP server initialized"
        );

        // Send initialized notification (fire-and-forget). Per the MCP spec
        // this is a notification, so it must carry NO `id` — servers validate
        // the shape and may reject a request with an unknown method name.
        let initialized = JsonRpcNotification {
            jsonrpc: "2.0".into(),
            method: "notifications/initialized".into(),
            params: None,
        };
        let _ = transport.send_notification(&initialized);

        Ok(Self {
            transport,
            next_id: AtomicU64::new(3),
            server_name,
            server_version,
            timeout: config.timeout,
        })
    }

    /// Fetch the list of tools from the server.
    ///
    /// # Errors
    ///
    /// Returns [`McpError::ServerShutdown`] when the transport is closed,
    /// [`McpError::Io`] on write failures, [`McpError::Timeout`] when the
    /// response does not arrive in time, and [`McpError::JsonRpcError`] /
    /// [`McpError::ProtocolError`] for a malformed or error response.
    pub fn list_tools(&mut self) -> Result<Vec<McpTool>, McpError> {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let req = JsonRpcRequest {
            jsonrpc: "2.0".into(),
            id,
            method: "tools/list".into(),
            params: None,
        };
        self.transport.send_request(&req)?;
        let resp = self.transport.recv_response(id, self.list_timeout())?;

        if let Some(err) = resp.error {
            return Err(McpError::JsonRpcError {
                code: err.code,
                message: err.message,
            });
        }

        let result = resp
            .result
            .ok_or_else(|| McpError::ProtocolError("tools/list response missing result".into()))?;

        let tools: Vec<McpTool> =
            serde_json::from_value(result.get("tools").cloned().unwrap_or(Value::Array(vec![])))
                .map_err(|e| McpError::ProtocolError(format!("invalid tools/list result: {e}")))?;

        // Drop tools whose schema cannot be represented safely (non-object or
        // over the size cap) rather than forwarding an invalid definition; a
        // `null`/absent schema is normalized to the empty-object fallback. The
        // spec's rule is to exclude the offending tool and keep the rest.
        let tools = tools
            .into_iter()
            .filter_map(|mut tool| {
                if let Some(schema) = normalize_input_schema(tool.input_schema) {
                    tool.input_schema = schema;
                    Some(tool)
                } else {
                    tracing::warn!(
                        tool = %tool.name,
                        "dropping MCP tool with invalid input schema"
                    );
                    None
                }
            })
            .collect();

        Ok(tools)
    }

    /// The per-server `tools/list` timeout (configured value or default).
    fn list_timeout(&self) -> Duration {
        self.timeout.unwrap_or(LIST_TOOLS_TIMEOUT)
    }

    /// Call a tool on the server.
    ///
    /// # Errors
    ///
    /// Returns [`McpError::ServerShutdown`] when the transport is closed,
    /// [`McpError::ProtocolError`] when params or the response cannot be
    /// serialized/deserialized, [`McpError::Io`] on write failures,
    /// [`McpError::Timeout`] when the response does not arrive in time, and
    /// [`McpError::JsonRpcError`] when the server returns an error response.
    pub fn call_tool(
        &mut self,
        name: &str,
        args: Option<Value>,
        timeout: Option<Duration>,
    ) -> Result<CallToolResult, McpError> {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let params = CallToolParams {
            name: name.to_string(),
            arguments: args,
        };
        let req = JsonRpcRequest {
            jsonrpc: "2.0".into(),
            id,
            method: "tools/call".into(),
            params: Some(serde_json::to_value(params).map_err(|e| {
                McpError::ProtocolError(format!("serialize call_tool params: {e}"))
            })?),
        };
        self.transport.send_request(&req)?;
        let resp = self
            .transport
            .recv_response(id, timeout.or(self.timeout).unwrap_or(DEFAULT_TIMEOUT))?;

        if let Some(err) = resp.error {
            return Err(McpError::JsonRpcError {
                code: err.code,
                message: err.message,
            });
        }

        let result = resp
            .result
            .ok_or_else(|| McpError::ProtocolError("tools/call response missing result".into()))?;

        let call_result: CallToolResult = serde_json::from_value(result)
            .map_err(|e| McpError::ProtocolError(format!("invalid tools/call result: {e}")))?;

        Ok(call_result)
    }

    /// Shut down the server.
    pub fn shutdown(&mut self) {
        self.transport.shutdown();
    }

    /// The server's advertised name, captured during the handshake.
    pub fn server_name(&self) -> &str {
        &self.server_name
    }

    /// The server's advertised version, captured during the handshake.
    pub fn server_version(&self) -> &str {
        &self.server_version
    }
}

impl Drop for McpClient {
    fn drop(&mut self) {
        self.shutdown();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mcp_server_config_defaults() {
        let config = McpServerConfig {
            slug: "test".into(),
            command: "echo".into(),
            args: vec![],
            env: HashMap::new(),
            enabled: true,
            timeout: None,
        };
        assert_eq!(config.slug, "test");
        assert_eq!(config.command, "echo");
        assert!(config.enabled);
        assert_eq!(config.init_timeout(), INIT_TIMEOUT);
    }

    #[test]
    fn configured_timeout_overrides_defaults() {
        let config = McpServerConfig {
            slug: "test".into(),
            command: "echo".into(),
            args: vec![],
            env: HashMap::new(),
            enabled: true,
            timeout: Some(Duration::from_secs(5)),
        };
        assert_eq!(config.init_timeout(), Duration::from_secs(5));
    }
}
