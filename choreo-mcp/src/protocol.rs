//! MCP wire types and the `initialize` request builder.
//!
//! This module owns the JSON shapes exchanged with an MCP server: the generic
//! JSON-RPC 2.0 envelope ([`JsonRpcRequest`] / [`JsonRpcResponse`] /
//! [`JsonRpcNotification`]) and the MCP-specific payloads carried inside it
//! (handshake params, tool descriptors, call results). Field names follow the
//! MCP spec's `camelCase` wire form via `serde(rename_all = "camelCase")`.

use crate::McpError;
use std::collections::HashMap;

// ---------------------------------------------------------------------------
// JSON-RPC 2.0 wire types
// ---------------------------------------------------------------------------

/// Monotonic request identifier correlating a request with its response.
///
/// The client assigns these from a process-local counter; the transport drops
/// any response whose id does not match the one it is awaiting.
pub type RequestId = u64;

/// A JSON-RPC 2.0 request: a method invocation that expects a matching
/// response.
#[derive(serde::Serialize, serde::Deserialize, Debug)]
pub struct JsonRpcRequest {
    /// Protocol version string; always `"2.0"`.
    pub jsonrpc: String,
    /// Identifier echoed back on the matching response.
    pub id: RequestId,
    /// Method name (e.g. `initialize`, `tools/list`, `tools/call`).
    pub method: String,
    /// Method parameters; omitted from the wire form when `None`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub params: Option<serde_json::Value>,
}

/// A JSON-RPC 2.0 response: either a `result` or an `error`, correlated to its
/// request by `id`.
#[derive(serde::Serialize, serde::Deserialize, Debug)]
pub struct JsonRpcResponse {
    /// Protocol version string; always `"2.0"`.
    pub jsonrpc: String,
    /// Identifier of the request this response answers.
    pub id: RequestId,
    /// Successful result payload, present when the call succeeded.
    #[serde(default)]
    pub result: Option<serde_json::Value>,
    /// Error payload, present when the call failed; mutually exclusive with
    /// `result`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<JsonRpcErrorObject>,
}

/// The `error` member of a JSON-RPC 2.0 error response.
#[derive(serde::Serialize, serde::Deserialize, Debug)]
pub struct JsonRpcErrorObject {
    /// Numeric JSON-RPC error code (e.g. `-32601` for method not found).
    pub code: i64,
    /// Human-readable error message.
    pub message: String,
    /// Optional structured error details supplied by the server.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<serde_json::Value>,
}

/// A JSON-RPC 2.0 notification: a method invocation that carries **no** `id`
/// and therefore expects no response.
#[derive(serde::Serialize, serde::Deserialize, Debug)]
pub struct JsonRpcNotification {
    /// Protocol version string; always `"2.0"`.
    pub jsonrpc: String,
    /// Notification method (e.g. `notifications/initialized`).
    pub method: String,
    /// Notification parameters; omitted from the wire form when `None`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub params: Option<serde_json::Value>,
}

// ---------------------------------------------------------------------------
// MCP protocol types
// ---------------------------------------------------------------------------

/// Parameters for the MCP `initialize` handshake request.
#[derive(serde::Serialize, serde::Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct InitializeParams {
    /// The MCP protocol version the client speaks (e.g. `2024-11-05`).
    pub protocol_version: String,
    /// Capabilities this client declares to the server.
    pub capabilities: ClientCapabilities,
    /// Identifying name and version of this client.
    pub client_info: ClientInfo,
}

/// The capabilities an MCP client declares during the handshake.
///
/// Each field is an optional capability object; it is omitted from the wire
/// form when `None` (an empty capability is not advertised).
#[derive(serde::Serialize, serde::Deserialize, Debug, Default)]
#[serde(rename_all = "camelCase")]
pub struct ClientCapabilities {
    /// Tools capability; `Some` advertises interest in tool listing/calls.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tools: Option<HashMap<String, serde_json::Value>>,
    /// Resources capability, if the client supports resources.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resources: Option<HashMap<String, serde_json::Value>>,
    /// Prompts capability, if the client supports prompts.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompts: Option<HashMap<String, serde_json::Value>>,
}

/// Identifying name and version of the MCP client, sent in the handshake.
#[derive(serde::Serialize, serde::Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct ClientInfo {
    /// Client name (e.g. `choreographr`).
    pub name: String,
    /// Client version string.
    pub version: String,
}

/// The server's response to the `initialize` handshake.
#[derive(serde::Serialize, serde::Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct ServerCapabilities {
    /// The MCP protocol version the server selected.
    pub protocol_version: String,
    /// Identifying name and version of the server.
    pub server_info: ServerInfo,
    /// Opaque capability object advertised by the server.
    #[serde(default)]
    pub capabilities: serde_json::Value,
}

/// Identifying name and version of the MCP server.
#[derive(serde::Serialize, serde::Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct ServerInfo {
    /// Server name.
    pub name: String,
    /// Server version string.
    pub version: String,
}

/// A tool advertised by an MCP server in a `tools/list` response.
#[derive(serde::Serialize, serde::Deserialize, Debug, Clone)]
pub struct McpTool {
    /// The tool name used to invoke it via `tools/call`.
    pub name: String,
    /// Human-readable description, if the server supplied one.
    pub description: Option<String>,
    /// JSON Schema for the tool's arguments (empty when the server omits it).
    #[serde(default)]
    pub input_schema: serde_json::Value,
}

/// Parameters for a `tools/call` request.
#[derive(serde::Serialize, serde::Deserialize, Debug)]
pub struct CallToolParams {
    /// Name of the tool to invoke.
    pub name: String,
    /// Arguments matching the tool's input schema, if any.
    pub arguments: Option<serde_json::Value>,
}

/// The result of a `tools/call` request.
#[derive(serde::Serialize, serde::Deserialize, Debug)]
pub struct CallToolResult {
    /// The content blocks the tool returned.
    pub content: Vec<McpContent>,
    /// Whether the server marked the call as an error (`isError` on the wire).
    #[serde(default)]
    pub is_error: bool,
}

/// A single content block in a [`CallToolResult`].
///
/// Tagged by `type` on the wire, so each variant serializes with an explicit
/// content-type discriminator.
#[derive(serde::Serialize, serde::Deserialize, Debug)]
#[serde(tag = "type")]
pub enum McpContent {
    /// Plain text content.
    #[serde(rename = "text")]
    Text {
        /// The text payload.
        text: String,
    },
    /// Base64-encoded binary content (e.g. an image).
    #[serde(rename = "image")]
    Image {
        /// Base64-encoded image data.
        data: String,
        /// The image MIME type, if the server supplied one.
        mime_type: Option<String>,
    },
    /// An embedded resource reference.
    #[serde(rename = "resource")]
    Resource {
        /// The opaque resource object (e.g. `{ "uri": ... }`).
        resource: serde_json::Value,
    },
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Build an `initialize` request for the MCP handshake.
///
/// # Errors
///
/// Returns [`McpError::ProtocolError`] when the initialize params cannot be
/// serialized to JSON (practically unreachable for these fixed fields).
pub fn make_initialize_request(id: RequestId) -> Result<JsonRpcRequest, McpError> {
    Ok(JsonRpcRequest {
        jsonrpc: "2.0".into(),
        id,
        method: "initialize".into(),
        params: Some(
            serde_json::to_value(InitializeParams {
                protocol_version: "2024-11-05".into(),
                capabilities: ClientCapabilities {
                    tools: Some(HashMap::from([(
                        "listChanged".into(),
                        serde_json::Value::Bool(true),
                    )])),
                    resources: None,
                    prompts: None,
                },
                client_info: ClientInfo {
                    name: "choreographr".into(),
                    version: "0.1.0".into(),
                },
            })
            .map_err(|e| McpError::ProtocolError(format!("serialize initialize params: {e}")))?,
        ),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_rpc_request_round_trip() {
        let req = JsonRpcRequest {
            jsonrpc: "2.0".into(),
            id: 1,
            method: "tools/list".into(),
            params: None,
        };
        let json = serde_json::to_string(&req).unwrap();
        let parsed: JsonRpcRequest = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.id, 1);
        assert_eq!(parsed.method, "tools/list");
        assert!(parsed.params.is_none());
    }

    #[test]
    fn json_rpc_request_with_params_round_trip() {
        let req = JsonRpcRequest {
            jsonrpc: "2.0".into(),
            id: 2,
            method: "tools/call".into(),
            params: Some(serde_json::json!({"name": "echo", "arguments": {}})),
        };
        let json = serde_json::to_string(&req).unwrap();
        let parsed: JsonRpcRequest = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.id, 2);
        assert!(parsed.params.is_some());
    }

    #[test]
    fn json_rpc_response_with_result_round_trip() {
        let resp = JsonRpcResponse {
            jsonrpc: "2.0".into(),
            id: 1,
            result: Some(serde_json::json!({"tools": []})),
            error: None,
        };
        let json = serde_json::to_string(&resp).unwrap();
        let parsed: JsonRpcResponse = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.id, 1);
        assert!(parsed.result.is_some());
        assert!(parsed.error.is_none());
    }

    #[test]
    fn json_rpc_response_with_error_round_trip() {
        let resp = JsonRpcResponse {
            jsonrpc: "2.0".into(),
            id: 1,
            result: None,
            error: Some(JsonRpcErrorObject {
                code: -32601,
                message: "Method not found".into(),
                data: None,
            }),
        };
        let json = serde_json::to_string(&resp).unwrap();
        let parsed: JsonRpcResponse = serde_json::from_str(&json).unwrap();
        assert!(parsed.error.is_some());
        assert_eq!(parsed.error.as_ref().unwrap().code, -32601);
    }

    #[test]
    fn json_rpc_notification_round_trip() {
        let notif = JsonRpcNotification {
            jsonrpc: "2.0".into(),
            method: "notifications/initialized".into(),
            params: None,
        };
        let json = serde_json::to_string(&notif).unwrap();
        let parsed: JsonRpcNotification = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.method, "notifications/initialized");
        assert!(parsed.params.is_none());
    }

    #[test]
    fn mcp_content_text_round_trip() {
        let content = McpContent::Text {
            text: "hello".into(),
        };
        let json = serde_json::to_string(&content).unwrap();
        assert!(json.contains(r#""type":"text""#));
        let parsed: McpContent = serde_json::from_str(&json).unwrap();
        assert!(matches!(parsed, McpContent::Text { text } if text == "hello"));
    }

    #[test]
    fn mcp_content_image_round_trip() {
        let content = McpContent::Image {
            data: "base64data".into(),
            mime_type: Some("image/png".into()),
        };
        let json = serde_json::to_string(&content).unwrap();
        let parsed: McpContent = serde_json::from_str(&json).unwrap();
        assert!(
            matches!(parsed, McpContent::Image { ref mime_type, .. } if mime_type.as_deref() == Some("image/png"))
        );
    }

    #[test]
    fn mcp_content_image_no_mime_round_trip() {
        let content = McpContent::Image {
            data: "base64data".into(),
            mime_type: None,
        };
        let json = serde_json::to_string(&content).unwrap();
        let parsed: McpContent = serde_json::from_str(&json).unwrap();
        assert!(matches!(parsed, McpContent::Image { ref mime_type, .. } if mime_type.is_none()));
    }

    #[test]
    fn mcp_content_resource_round_trip() {
        let content = McpContent::Resource {
            resource: serde_json::json!({"uri": "file:///tmp/test.txt"}),
        };
        let json = serde_json::to_string(&content).unwrap();
        let parsed: McpContent = serde_json::from_str(&json).unwrap();
        assert!(matches!(parsed, McpContent::Resource { .. }));
    }

    #[test]
    fn make_initialize_request_succeeds() {
        let req = make_initialize_request(1).expect("initialize request should succeed");
        assert_eq!(req.id, 1);
        assert_eq!(req.method, "initialize");
        assert!(req.params.is_some());
    }

    #[test]
    fn initialize_request_uses_camel_case_wire_names() {
        // The MCP spec requires camelCase field names on the wire; the server
        // rejects snake_case (`protocol_version` / `client_info`) with -32603.
        let req = make_initialize_request(1).expect("initialize request should succeed");
        let json = serde_json::to_string(&req).expect("serialize initialize request");
        assert!(
            json.contains("\"protocolVersion\""),
            "missing protocolVersion: {json}"
        );
        assert!(
            json.contains("\"clientInfo\""),
            "missing clientInfo: {json}"
        );
        assert!(
            !json.contains("protocol_version") && !json.contains("client_info"),
            "snake_case leaked onto the wire: {json}"
        );
    }

    #[test]
    fn mcp_tool_round_trip() {
        let tool = McpTool {
            name: "echo".into(),
            description: Some("Echo back input".into()),
            input_schema: serde_json::json!({"type": "object"}),
        };
        let json = serde_json::to_string(&tool).unwrap();
        let parsed: McpTool = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.name, "echo");
        assert_eq!(parsed.description.as_deref(), Some("Echo back input"));
    }

    #[test]
    fn mcp_tool_no_description() {
        let tool = McpTool {
            name: "no_desc".into(),
            description: None,
            input_schema: serde_json::json!({}),
        };
        let json = serde_json::to_string(&tool).unwrap();
        let parsed: McpTool = serde_json::from_str(&json).unwrap();
        assert!(parsed.description.is_none());
    }

    #[test]
    fn call_tool_result_round_trip() {
        let result = CallToolResult {
            content: vec![McpContent::Text { text: "ok".into() }],
            is_error: false,
        };
        let json = serde_json::to_string(&result).unwrap();
        let parsed: CallToolResult = serde_json::from_str(&json).unwrap();
        assert!(!parsed.is_error);
        assert_eq!(parsed.content.len(), 1);
    }

    #[test]
    fn call_tool_params_round_trip() {
        let params = CallToolParams {
            name: "echo".into(),
            arguments: Some(serde_json::json!({"message": "hi"})),
        };
        let json = serde_json::to_string(&params).unwrap();
        let parsed: CallToolParams = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.name, "echo");
    }
}
