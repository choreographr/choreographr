use crate::McpError;
use std::collections::HashMap;

// ---------------------------------------------------------------------------
// JSON-RPC 2.0 wire types
// ---------------------------------------------------------------------------

pub type RequestId = u64;

#[derive(serde::Serialize, serde::Deserialize, Debug)]
pub struct JsonRpcRequest {
    pub jsonrpc: String,
    pub id: RequestId,
    pub method: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub params: Option<serde_json::Value>,
}

#[derive(serde::Serialize, serde::Deserialize, Debug)]
pub struct JsonRpcResponse {
    pub jsonrpc: String,
    pub id: RequestId,
    #[serde(default)]
    pub result: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<JsonRpcErrorObject>,
}

#[derive(serde::Serialize, serde::Deserialize, Debug)]
pub struct JsonRpcErrorObject {
    pub code: i64,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<serde_json::Value>,
}

#[derive(serde::Serialize, serde::Deserialize, Debug)]
pub struct JsonRpcNotification {
    pub jsonrpc: String,
    pub method: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub params: Option<serde_json::Value>,
}

// ---------------------------------------------------------------------------
// MCP protocol types
// ---------------------------------------------------------------------------

#[derive(serde::Serialize, serde::Deserialize, Debug)]
pub struct InitializeParams {
    pub protocol_version: String,
    pub capabilities: ClientCapabilities,
    pub client_info: ClientInfo,
}

#[derive(serde::Serialize, serde::Deserialize, Debug, Default)]
pub struct ClientCapabilities {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tools: Option<HashMap<String, serde_json::Value>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resources: Option<HashMap<String, serde_json::Value>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompts: Option<HashMap<String, serde_json::Value>>,
}

#[derive(serde::Serialize, serde::Deserialize, Debug)]
pub struct ClientInfo {
    pub name: String,
    pub version: String,
}

#[derive(serde::Serialize, serde::Deserialize, Debug)]
pub struct ServerCapabilities {
    pub protocol_version: String,
    pub server_info: ServerInfo,
    #[serde(default)]
    pub capabilities: serde_json::Value,
}

#[derive(serde::Serialize, serde::Deserialize, Debug)]
pub struct ServerInfo {
    pub name: String,
    pub version: String,
}

#[derive(serde::Serialize, serde::Deserialize, Debug, Clone)]
pub struct McpTool {
    pub name: String,
    pub description: Option<String>,
    #[serde(default)]
    pub input_schema: serde_json::Value,
}

#[derive(serde::Serialize, serde::Deserialize, Debug)]
pub struct CallToolParams {
    pub name: String,
    pub arguments: Option<serde_json::Value>,
}

#[derive(serde::Serialize, serde::Deserialize, Debug)]
pub struct CallToolResult {
    pub content: Vec<McpContent>,
    #[serde(default)]
    pub is_error: bool,
}

#[derive(serde::Serialize, serde::Deserialize, Debug)]
#[serde(tag = "type")]
pub enum McpContent {
    #[serde(rename = "text")]
    Text { text: String },
    #[serde(rename = "image")]
    Image {
        data: String,
        mime_type: Option<String>,
    },
    #[serde(rename = "resource")]
    Resource { resource: serde_json::Value },
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Build an `initialize` request for the MCP handshake.
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
                    name: "tai-daemon".into(),
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
