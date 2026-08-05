use crate::error::McpError;
use crate::protocol::{
    CallToolParams, CallToolResult, JsonRpcNotification, JsonRpcRequest, McpTool,
    make_initialize_request,
};
use crate::transport::StdioTransport;
use serde_json::Value;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

/// Default timeout for MCP tool calls.
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(60);

/// Timeout for the initialize handshake.
const INIT_TIMEOUT: Duration = Duration::from_secs(10);

/// Timeout for tools/list.
const LIST_TOOLS_TIMEOUT: Duration = Duration::from_secs(10);

/// Configuration for spawning an MCP server subprocess.
#[derive(Debug, Clone)]
pub struct McpServerConfig {
    pub slug: String,
    pub command: String,
    pub args: Vec<String>,
    pub env: HashMap<String, String>,
    pub enabled: bool,
    pub auto_load: bool,
}

/// A client connected to a single MCP server subprocess.
pub struct McpClient {
    transport: StdioTransport,
    next_id: AtomicU64,
    server_name: String,
    server_version: String,
}

impl McpClient {
    /// Spawn the MCP server subprocess and perform the initialize handshake.
    pub fn spawn(config: &McpServerConfig) -> Result<Self, McpError> {
        let mut transport = StdioTransport::spawn(&config.command, &config.args, &config.env)?;

        // Send initialize request.
        let init_req = make_initialize_request(1)?;
        transport.send_request(&init_req)?;

        let resp = transport.recv_response(1, INIT_TIMEOUT)?;

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
        })
    }

    /// Fetch the list of tools from the server.
    pub fn list_tools(&mut self) -> Result<Vec<McpTool>, McpError> {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let req = JsonRpcRequest {
            jsonrpc: "2.0".into(),
            id,
            method: "tools/list".into(),
            params: None,
        };
        self.transport.send_request(&req)?;
        let resp = self.transport.recv_response(id, LIST_TOOLS_TIMEOUT)?;

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

        Ok(tools)
    }

    /// Call a tool on the server.
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
            .recv_response(id, timeout.unwrap_or(DEFAULT_TIMEOUT))?;

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

    pub fn server_name(&self) -> &str {
        &self.server_name
    }

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
            auto_load: true,
        };
        assert_eq!(config.slug, "test");
        assert_eq!(config.command, "echo");
        assert!(config.enabled);
        assert!(config.auto_load);
    }
}
