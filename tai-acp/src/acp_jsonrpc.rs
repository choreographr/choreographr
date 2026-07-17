use crate::error::AcpError;

// ---------------------------------------------------------------------------
// JSON-RPC 2.0 wire types
// ---------------------------------------------------------------------------

pub type RequestId = u64;

/// A JSON-RPC 2.0 request — always carries an `id` so the peer can match
/// the response.
#[derive(serde::Serialize, serde::Deserialize, Debug)]
pub struct JsonRpcRequest {
    pub jsonrpc: String,
    pub id: RequestId,
    pub method: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub params: Option<serde_json::Value>,
}

/// A JSON-RPC 2.0 response — carries either a `result` or an `error`.
#[derive(serde::Serialize, serde::Deserialize, Debug)]
pub struct JsonRpcResponse {
    pub jsonrpc: String,
    pub id: RequestId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<JsonRpcError>,
}

/// Structured error object inside a JSON-RPC error response.
#[derive(serde::Serialize, serde::Deserialize, Debug)]
pub struct JsonRpcError {
    pub code: i64,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<serde_json::Value>,
}

/// A JSON-RPC 2.0 notification — like a request but without an `id`
/// (the peer does not reply).
///
/// `deny_unknown_fields` ensures a message with an `id` field that fails
/// to parse as a `Request` also fails as a `Notification`, rather than
/// silently dropping the `id`.
#[derive(serde::Serialize, serde::Deserialize, Debug)]
#[serde(deny_unknown_fields)]
pub struct JsonRpcNotification {
    pub jsonrpc: String,
    pub method: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub params: Option<serde_json::Value>,
}

// ---------------------------------------------------------------------------
// Incoming message dispatch
// ---------------------------------------------------------------------------

/// A parsed incoming JSON-RPC message: either a request (expects a reply)
/// or a notification (fire-and-forget).
///
/// Uses `untagged` so serde tries `Request` first (which requires `id`),
/// then `Notification` (which rejects `id` via `deny_unknown_fields`).
#[derive(Debug, serde::Deserialize)]
#[serde(untagged)]
pub enum RpcMessage {
    Request(JsonRpcRequest),
    Notification(JsonRpcNotification),
}

impl RpcMessage {
    /// Get the JSON-RPC request ID if this is a request, `None` if a notification.
    pub fn id(&self) -> Option<u64> {
        match self {
            RpcMessage::Request(req) => Some(req.id),
            RpcMessage::Notification(_) => None,
        }
    }

    /// Get the method name.
    pub fn method(&self) -> &str {
        match self {
            RpcMessage::Request(req) => &req.method,
            RpcMessage::Notification(notif) => &notif.method,
        }
    }
}

/// Helper: build a successful JSON-RPC response.
pub fn make_response(id: RequestId, result: serde_json::Value) -> JsonRpcResponse {
    tracing::trace!(id, "building JSON-RPC response");
    JsonRpcResponse {
        jsonrpc: "2.0".into(),
        id,
        result: Some(result),
        error: None,
    }
}

/// Helper: build a JSON-RPC error response.
pub fn make_error(id: RequestId, code: i64, message: &str) -> JsonRpcResponse {
    tracing::trace!(id, code, message, "building JSON-RPC error response");
    JsonRpcResponse {
        jsonrpc: "2.0".into(),
        id,
        result: None,
        error: Some(JsonRpcError {
            code,
            message: message.to_string(),
            data: None,
        }),
    }
}

/// Helper: build a JSON-RPC notification.
pub fn make_notification(method: &str, params: serde_json::Value) -> JsonRpcNotification {
    tracing::trace!(method, "building JSON-RPC notification");
    JsonRpcNotification {
        jsonrpc: "2.0".into(),
        method: method.into(),
        params: Some(params),
    }
}

/// Parse a single JSON-RPC line into either a `Request` (has an `id` field)
/// or a `Notification` (no `id` field).
///
/// Uses untagged deserialisation on `RpcMessage` so serde handles the
/// routing in a single pass — no intermediate `Value` allocation needed.
pub fn parse_request(line: &str) -> Result<RpcMessage, AcpError> {
    let msg: RpcMessage = serde_json::from_str(line)?;
    match &msg {
        RpcMessage::Request(req) => {
            tracing::debug!(id = req.id, method = %req.method, "parsed JSON-RPC request");
        }
        RpcMessage::Notification(notif) => {
            tracing::debug!(method = %notif.method, "parsed JSON-RPC notification");
        }
    }
    Ok(msg)
}

// ---------------------------------------------------------------------------
// ACP protocol payload types
// ---------------------------------------------------------------------------

// --- Initialize ---

#[derive(serde::Serialize, serde::Deserialize, Debug)]
pub struct InitializeParams {
    pub protocol_version: u32,
    #[serde(default)]
    pub capabilities: ClientCapabilities,
    #[serde(default)]
    pub client_info: ClientInfo,
}

#[derive(serde::Serialize, serde::Deserialize, Debug, Default)]
pub struct ClientCapabilities {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt: Option<PromptCapabilities>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fs: Option<FsCapabilities>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub terminal: Option<serde_json::Value>,
}

#[derive(serde::Serialize, serde::Deserialize, Debug, Default)]
pub struct PromptCapabilities {
    #[serde(default)]
    pub image: bool,
    #[serde(default)]
    pub audio: bool,
    #[serde(default)]
    pub embedded_context: bool,
}

#[derive(serde::Serialize, serde::Deserialize, Debug, Default)]
pub struct FsCapabilities {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub read_text_file: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub write_text_file: Option<serde_json::Value>,
}

#[derive(serde::Serialize, serde::Deserialize, Debug, Default)]
pub struct ClientInfo {
    pub name: String,
    pub version: String,
}

// --- Initialize response — what the agent advertises back ---

#[derive(serde::Serialize, serde::Deserialize, Debug)]
pub struct InitializeResult {
    pub protocol_version: u32,
    pub agent_capabilities: AgentCapabilities,
    pub agent_info: AgentInfo,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config_options: Option<Vec<ConfigOption>>,
}

#[derive(serde::Serialize, serde::Deserialize, Debug)]
pub struct AgentCapabilities {
    #[serde(default)]
    pub load_session: bool,
    #[serde(default)]
    pub prompt_capabilities: PromptCapabilities,
    #[serde(default)]
    pub session_capabilities: SessionCapabilities,
    #[serde(default)]
    pub mcp_capabilities: McpCapabilities,
}

#[derive(serde::Serialize, serde::Deserialize, Debug, Default)]
pub struct SessionCapabilities {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub list: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delete: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub close: Option<serde_json::Value>,
}

#[derive(serde::Serialize, serde::Deserialize, Debug, Default)]
pub struct McpCapabilities {
    #[serde(default)]
    pub http: bool,
    #[serde(default)]
    pub sse: bool,
}

#[derive(serde::Serialize, serde::Deserialize, Debug)]
pub struct AgentInfo {
    pub name: String,
    pub version: String,
}

// --- Configuration options ---

#[derive(serde::Serialize, serde::Deserialize, Debug)]
pub struct ConfigOption {
    pub id: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub category: Option<String>,
    pub option_type: ConfigOptionType,
    pub current_value: ConfigOptionValue,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub options: Option<Vec<SelectOption>>,
}

#[derive(serde::Serialize, serde::Deserialize, Debug)]
#[serde(rename_all = "snake_case")]
pub enum ConfigOptionType {
    Select,
    TextField,
    Switch,
}

#[derive(serde::Serialize, serde::Deserialize, Debug)]
#[serde(untagged)]
pub enum ConfigOptionValue {
    String(String),
    Bool(bool),
}

#[derive(serde::Serialize, serde::Deserialize, Debug)]
pub struct SelectOption {
    pub value: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

// --- Session management ---

#[derive(serde::Serialize, serde::Deserialize, Debug)]
pub struct NewSessionRequest {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config_options: Option<Vec<ConfigOptionValue>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<serde_json::Value>,
}

#[derive(serde::Serialize, serde::Deserialize, Debug)]
pub struct NewSessionResult {
    pub session_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config_options: Option<Vec<ConfigOption>>,
}

#[derive(serde::Serialize, serde::Deserialize, Debug)]
pub struct LoadSessionRequest {
    pub session_id: String,
}

#[derive(serde::Serialize, serde::Deserialize, Debug)]
pub struct LoadSessionResult {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config_options: Option<Vec<ConfigOption>>,
}

// --- Prompt ---

#[derive(serde::Serialize, serde::Deserialize, Debug)]
pub struct PromptRequest {
    pub session_id: String,
    pub prompt: Vec<ContentBlock>,
}

#[derive(serde::Serialize, serde::Deserialize, Debug)]
#[serde(tag = "type")]
pub enum ContentBlock {
    #[serde(rename = "text")]
    Text { text: String },
    #[serde(rename = "resource")]
    Resource { resource: ResourceContent },
    #[serde(rename = "image")]
    Image { image: ImageContent },
}

#[derive(serde::Serialize, serde::Deserialize, Debug)]
pub struct ResourceContent {
    pub uri: String,
    pub content: serde_json::Value,
}

#[derive(serde::Serialize, serde::Deserialize, Debug)]
pub struct ImageContent {
    pub data: String,
    pub mime_type: Option<String>,
}

#[derive(serde::Serialize, serde::Deserialize, Debug)]
pub struct PromptResult {
    pub stop_reason: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<UsageInfo>,
}

#[derive(serde::Serialize, serde::Deserialize, Debug)]
pub struct UsageInfo {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub used_input_tokens: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub used_output_tokens: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub used_reasoning_tokens: Option<u32>,
}

// --- Cancel ---

#[derive(serde::Serialize, serde::Deserialize, Debug)]
pub struct CancelNotification {
    pub session_id: String,
}

// --- List sessions ---

#[derive(serde::Serialize, serde::Deserialize, Debug)]
pub struct ListSessionsResult {
    pub sessions: Vec<SessionInfo>,
}

#[derive(serde::Serialize, serde::Deserialize, Debug)]
pub struct SessionInfo {
    pub session_id: String,
    pub title: Option<String>,
    pub model: Option<String>,
    pub created_at: Option<i64>,
}

// --- Delete session ---

#[derive(serde::Serialize, serde::Deserialize, Debug)]
pub struct DeleteSessionRequest {
    pub session_id: String,
}

// --- Close session ---

#[derive(serde::Serialize, serde::Deserialize, Debug)]
pub struct CloseSessionRequest {
    pub session_id: String,
}

// --- Set config option ---

#[derive(serde::Serialize, serde::Deserialize, Debug)]
pub struct SetConfigOptionRequest {
    pub session_id: String,
    pub config_id: String,
    pub value: ConfigOptionValue,
}

// --- Session update notifications (streaming) ---

#[derive(serde::Serialize, serde::Deserialize, Debug)]
pub struct SessionUpdateParams {
    pub session_id: String,
    #[serde(flatten)]
    pub variant: SessionUpdateVariant,
}

#[derive(serde::Serialize, serde::Deserialize, Debug)]
#[serde(tag = "type")]
pub enum SessionUpdateVariant {
    #[serde(rename = "agent_message_chunk")]
    AgentMessageChunk {
        message_id: String,
        content: ContentBlock,
    },
    #[serde(rename = "tool_call")]
    ToolCall {
        tool_call_id: String,
        title: String,
        kind: String,
        status: String,
        content: Vec<ContentBlock>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        locations: Option<Vec<serde_json::Value>>,
    },
    #[serde(rename = "tool_call_update")]
    ToolCallUpdate {
        tool_call_id: String,
        status: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        content: Option<Vec<ContentBlock>>,
    },
    #[serde(rename = "usage_update")]
    UsageUpdate {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        used_input_tokens: Option<u32>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        used_output_tokens: Option<u32>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        used_reasoning_tokens: Option<u32>,
    },
    #[serde(rename = "status_update")]
    StatusUpdate { status: String },
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // ---------------------------------------------------------------
    // JSON-RPC wire type round-trips
    // ---------------------------------------------------------------

    #[test]
    fn json_rpc_request_round_trip() {
        let req = JsonRpcRequest {
            jsonrpc: "2.0".into(),
            id: 42,
            method: "acp/sessions/new".into(),
            params: None,
        };
        let json = serde_json::to_string(&req).unwrap();
        let parsed: JsonRpcRequest = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.id, 42);
        assert_eq!(parsed.method, "acp/sessions/new");
        assert!(parsed.params.is_none());
    }

    #[test]
    fn json_rpc_request_with_params_round_trip() {
        let req = JsonRpcRequest {
            jsonrpc: "2.0".into(),
            id: 7,
            method: "acp/sessions/new".into(),
            params: Some(serde_json::json!({"session_id": "abc-123"})),
        };
        let json = serde_json::to_string(&req).unwrap();
        let parsed: JsonRpcRequest = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.id, 7);
        assert!(parsed.params.is_some());
    }

    #[test]
    fn json_rpc_response_with_result_round_trip() {
        let resp = JsonRpcResponse {
            jsonrpc: "2.0".into(),
            id: 1,
            result: Some(serde_json::json!({"session_id": "abc"})),
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
            error: Some(JsonRpcError {
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
            method: "notifications/cancelled".into(),
            params: None,
        };
        let json = serde_json::to_string(&notif).unwrap();
        let parsed: JsonRpcNotification = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.method, "notifications/cancelled");
        assert!(parsed.params.is_none());
    }

    // ---------------------------------------------------------------
    // Helper function tests
    // ---------------------------------------------------------------

    #[test]
    fn make_response_sets_fields_correctly() {
        let resp = make_response(5, serde_json::json!({"ok": true}));
        assert_eq!(resp.id, 5);
        assert_eq!(resp.jsonrpc, "2.0");
        assert!(resp.result.is_some());
        assert!(resp.error.is_none());
    }

    #[test]
    fn make_error_sets_fields_correctly() {
        let resp = make_error(3, -32602, "Invalid params");
        assert_eq!(resp.id, 3);
        assert!(resp.result.is_none());
        let err = resp.error.unwrap();
        assert_eq!(err.code, -32602);
        assert_eq!(err.message, "Invalid params");
        assert!(err.data.is_none());
    }

    #[test]
    fn make_notification_sets_fields_correctly() {
        let notif = make_notification("test/event", serde_json::json!({"key": "val"}));
        assert_eq!(notif.jsonrpc, "2.0");
        assert_eq!(notif.method, "test/event");
        assert!(notif.params.is_some());
    }

    // ---------------------------------------------------------------
    // parse_request tests
    // ---------------------------------------------------------------

    #[test]
    fn parse_request_detects_request_with_id() {
        let line = r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#;
        match parse_request(line).unwrap() {
            RpcMessage::Request(req) => {
                assert_eq!(req.id, 1);
                assert_eq!(req.method, "initialize");
            }
            RpcMessage::Notification(_) => panic!("expected Request, got Notification"),
        }
    }

    #[test]
    fn parse_request_detects_notification_without_id() {
        let line =
            r#"{"jsonrpc":"2.0","method":"notifications/cancelled","params":{"session_id":"x"}}"#;
        match parse_request(line).unwrap() {
            RpcMessage::Notification(notif) => {
                assert_eq!(notif.method, "notifications/cancelled");
            }
            RpcMessage::Request(_) => panic!("expected Notification, got Request"),
        }
    }

    #[test]
    fn parse_request_rejects_invalid_json() {
        let result = parse_request("not json");
        assert!(result.is_err());
    }

    // ---------------------------------------------------------------
    // ContentBlock round-trips
    // ---------------------------------------------------------------

    #[test]
    fn content_block_text_round_trip() {
        let block = ContentBlock::Text {
            text: "Hello world".into(),
        };
        let json = serde_json::to_string(&block).unwrap();
        assert!(json.contains(r#""type":"text""#));
        let parsed: ContentBlock = serde_json::from_str(&json).unwrap();
        assert!(matches!(parsed, ContentBlock::Text { ref text } if text == "Hello world"));
    }

    #[test]
    fn content_block_resource_round_trip() {
        let block = ContentBlock::Resource {
            resource: ResourceContent {
                uri: "file:///tmp/doc.txt".into(),
                content: serde_json::json!({"text": "hello"}),
            },
        };
        let json = serde_json::to_string(&block).unwrap();
        let parsed: ContentBlock = serde_json::from_str(&json).unwrap();
        assert!(matches!(parsed, ContentBlock::Resource { .. }));
        if let ContentBlock::Resource { resource } = parsed {
            assert_eq!(resource.uri, "file:///tmp/doc.txt");
        }
    }

    #[test]
    fn content_block_image_round_trip() {
        let block = ContentBlock::Image {
            image: ImageContent {
                data: "aGVsbG8=".into(),
                mime_type: Some("image/png".into()),
            },
        };
        let json = serde_json::to_string(&block).unwrap();
        let parsed: ContentBlock = serde_json::from_str(&json).unwrap();
        assert!(
            matches!(&parsed, ContentBlock::Image { image } if image.mime_type.as_deref() == Some("image/png"))
        );
    }

    #[test]
    fn content_block_image_no_mime_round_trip() {
        let block = ContentBlock::Image {
            image: ImageContent {
                data: "aGVsbG8=".into(),
                mime_type: None,
            },
        };
        let json = serde_json::to_string(&block).unwrap();
        let parsed: ContentBlock = serde_json::from_str(&json).unwrap();
        assert!(matches!(&parsed, ContentBlock::Image { image } if image.mime_type.is_none()));
    }

    // ---------------------------------------------------------------
    // ConfigOption serialization
    // ---------------------------------------------------------------

    #[test]
    fn config_option_round_trip() {
        let opt = ConfigOption {
            id: "model".into(),
            name: "Model".into(),
            description: Some("The AI model to use".into()),
            category: Some("general".into()),
            option_type: ConfigOptionType::Select,
            current_value: ConfigOptionValue::String("claude-4".into()),
            options: Some(vec![
                SelectOption {
                    value: "claude-4".into(),
                    name: Some("Claude 4".into()),
                },
                SelectOption {
                    value: "gpt-5".into(),
                    name: None,
                },
            ]),
        };
        let json = serde_json::to_string(&opt).unwrap();
        assert!(json.contains(r#""option_type":"select""#));
        assert!(json.contains(r#""current_value":"claude-4""#));
        let parsed: ConfigOption = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.id, "model");
        match parsed.option_type {
            ConfigOptionType::Select => {}
            _ => panic!("expected Select"),
        }
    }

    #[test]
    fn config_option_switch_round_trip() {
        let opt = ConfigOption {
            id: "notifications".into(),
            name: "Notifications".into(),
            description: None,
            category: None,
            option_type: ConfigOptionType::Switch,
            current_value: ConfigOptionValue::Bool(true),
            options: None,
        };
        let json = serde_json::to_string(&opt).unwrap();
        assert!(json.contains(r#""option_type":"switch""#));
        assert!(json.contains(r#""current_value":true"#));
        let parsed: ConfigOption = serde_json::from_str(&json).unwrap();
        match parsed.option_type {
            ConfigOptionType::Switch => {}
            _ => panic!("expected Switch"),
        }
        match parsed.current_value {
            ConfigOptionValue::Bool(v) => assert!(v),
            _ => panic!("expected Bool"),
        }
    }

    // ---------------------------------------------------------------
    // InitializeResult serialization
    // ---------------------------------------------------------------

    #[test]
    fn initialize_result_round_trip() {
        let result = InitializeResult {
            protocol_version: 1,
            agent_capabilities: AgentCapabilities {
                load_session: true,
                prompt_capabilities: PromptCapabilities {
                    image: true,
                    audio: false,
                    embedded_context: true,
                },
                session_capabilities: SessionCapabilities {
                    list: Some(serde_json::json!({})),
                    delete: Some(serde_json::json!({})),
                    close: None,
                },
                mcp_capabilities: McpCapabilities {
                    http: true,
                    sse: false,
                },
            },
            agent_info: AgentInfo {
                name: "tai-agent".into(),
                version: "0.1.0".into(),
            },
            config_options: Some(vec![ConfigOption {
                id: "model".into(),
                name: "Model".into(),
                description: None,
                category: None,
                option_type: ConfigOptionType::TextField,
                current_value: ConfigOptionValue::String("claude-4".into()),
                options: None,
            }]),
        };
        let json = serde_json::to_string(&result).unwrap();
        assert!(json.contains(r#""protocol_version":1"#));
        assert!(json.contains(r#""load_session":true"#));
        assert!(json.contains(r#""name":"tai-agent""#));
        let parsed: InitializeResult = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.protocol_version, 1);
        assert!(parsed.agent_capabilities.load_session);
        assert!(parsed.config_options.is_some());
    }

    #[test]
    fn initialize_result_no_config_options() {
        let result = InitializeResult {
            protocol_version: 1,
            agent_capabilities: AgentCapabilities {
                load_session: false,
                prompt_capabilities: PromptCapabilities::default(),
                session_capabilities: SessionCapabilities::default(),
                mcp_capabilities: McpCapabilities {
                    http: false,
                    sse: false,
                },
            },
            agent_info: AgentInfo {
                name: "test-agent".into(),
                version: "1.0".into(),
            },
            config_options: None,
        };
        let json = serde_json::to_string(&result).unwrap();
        // config_options should be absent (not null) when None
        assert!(!json.contains("config_options"));
        let parsed: InitializeResult = serde_json::from_str(&json).unwrap();
        assert!(parsed.config_options.is_none());
    }
}
