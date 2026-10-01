//! JSON-RPC 2.0 wire types and the ACP protocol payload types.
//!
//! The first half of this module is the generic JSON-RPC 2.0 envelope
//! ([`JsonRpcRequest`], [`JsonRpcResponse`], [`JsonRpcError`],
//! [`JsonRpcNotification`], and the [`RpcMessage`] dispatch enum) plus the
//! `make_*` / [`parse_request`] helpers. The second half is the ACP-specific
//! payload vocabulary — `initialize`, session management, prompt, config
//! options, and the streaming `session/update` notifications — carried in the
//! `params`/`result` fields of those envelopes.
//!
//! These types are pure data: they derive serde (de)serialization and carry no
//! bridging logic beyond [`RpcMessage`]'s accessors.

use crate::error::AcpError;

// ---------------------------------------------------------------------------
// JSON-RPC 2.0 wire types
// ---------------------------------------------------------------------------

/// Correlation identifier for a JSON-RPC request/response pair.
///
/// ACP uses JSON-RPC 2.0's integer id space; the peer echoes this value on the
/// matching response so the bridge can route it back to the originating
/// editor call.
pub type RequestId = u64;

/// A JSON-RPC 2.0 request — always carries an `id` so the peer can match
/// the response.
#[derive(serde::Serialize, serde::Deserialize, Debug)]
pub struct JsonRpcRequest {
    /// Protocol version marker; always `"2.0"`.
    pub jsonrpc: String,
    /// The request id the peer echoes on the response.
    pub id: RequestId,
    /// The method name being invoked (e.g. `"session/new"`).
    pub method: String,
    /// Method-specific parameters; absent when the method takes none.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub params: Option<serde_json::Value>,
}

/// A JSON-RPC 2.0 response — carries either a `result` or an `error`.
#[derive(serde::Serialize, serde::Deserialize, Debug)]
pub struct JsonRpcResponse {
    /// Protocol version marker; always `"2.0"`.
    pub jsonrpc: String,
    /// The id of the request this response answers.
    pub id: RequestId,
    /// The success payload; present exactly when the call succeeded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<serde_json::Value>,
    /// The error payload; present exactly when the call failed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<JsonRpcError>,
}

/// Structured error object inside a JSON-RPC error response.
#[derive(serde::Serialize, serde::Deserialize, Debug)]
pub struct JsonRpcError {
    /// Numeric JSON-RPC error code (e.g. `-32601` for "method not found").
    pub code: i64,
    /// Human-readable error message.
    pub message: String,
    /// Optional structured error details for programmatic handling.
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
    /// Protocol version marker; always `"2.0"`.
    pub jsonrpc: String,
    /// The notification method name (e.g. `"session/cancel"`).
    pub method: String,
    /// Method-specific parameters; absent when the notification takes none.
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
    /// A request that expects a JSON-RPC response.
    Request(JsonRpcRequest),
    /// A fire-and-forget notification (no response is sent).
    Notification(JsonRpcNotification),
}

impl RpcMessage {
    /// Get the JSON-RPC request ID if this is a request, `None` if a notification.
    #[must_use]
    pub fn id(&self) -> Option<u64> {
        match self {
            RpcMessage::Request(req) => Some(req.id),
            RpcMessage::Notification(_) => None,
        }
    }

    /// Get the method name.
    #[must_use]
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
///
/// # Errors
///
/// Returns [`AcpError::Serde`] (with the underlying [`serde_json::Error`])
/// when the line is not valid JSON or does not match either `RpcMessage` shape.
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

/// Parameters of the editor's `initialize` request: the negotiated protocol
/// version, the editor's capabilities, and its self-identification.
#[derive(serde::Serialize, serde::Deserialize, Debug)]
pub struct InitializeParams {
    /// ACP protocol version the editor speaks.
    pub protocol_version: u32,
    /// Capabilities the editor advertises (FS, terminal, prompt features).
    #[serde(default)]
    pub capabilities: ClientCapabilities,
    /// The editor's name and version, for logging/telemetry.
    #[serde(default)]
    pub client_info: ClientInfo,
}

/// Capabilities declared by the editor on `initialize`.
///
/// In v2 these will drive FS/terminal proxy decisions; in v1 every tool call
/// is executed through the daemon regardless (see
/// [`ClientCapabilitiesStore`](crate::client_capabilities::ClientCapabilitiesStore)).
#[derive(serde::Serialize, serde::Deserialize, Debug, Default)]
pub struct ClientCapabilities {
    /// Session-related capability block, if the editor sent one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session: Option<serde_json::Value>,
    /// Prompt feature support (image/audio/embedded context).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt: Option<PromptCapabilities>,
    /// Filesystem proxy support (`fs.readTextFile` / `fs.writeTextFile`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fs: Option<FsCapabilities>,
    /// Terminal proxy support, if the editor sent one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub terminal: Option<serde_json::Value>,
}

/// Prompt feature support advertised by the editor.
#[derive(serde::Serialize, serde::Deserialize, Debug, Default)]
pub struct PromptCapabilities {
    /// Whether the editor can attach image content blocks.
    #[serde(default)]
    pub image: bool,
    /// Whether the editor can attach audio content blocks.
    #[serde(default)]
    pub audio: bool,
    /// Whether the editor can attach embedded-context content blocks.
    #[serde(default)]
    pub embedded_context: bool,
}

/// Filesystem proxy support advertised by the editor.
///
/// The presence of each field (not its value) is the capability signal: a
/// `Some` means the editor implements that request.
#[derive(serde::Serialize, serde::Deserialize, Debug, Default)]
pub struct FsCapabilities {
    /// Editor implements `fs.readTextFile`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub read_text_file: Option<serde_json::Value>,
    /// Editor implements `fs.writeTextFile`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub write_text_file: Option<serde_json::Value>,
}

/// The editor's self-identification (name and version).
#[derive(serde::Serialize, serde::Deserialize, Debug, Default)]
pub struct ClientInfo {
    /// Editor name (e.g. `"zed"`).
    pub name: String,
    /// Editor version string.
    pub version: String,
}

// --- Initialize response — what the agent advertises back ---

/// Result of the editor's `initialize` request: what the agent advertises
/// back — protocol version, capabilities, identity, and config options.
#[derive(serde::Serialize, serde::Deserialize, Debug)]
pub struct InitializeResult {
    /// ACP protocol version the agent speaks.
    pub protocol_version: u32,
    /// Agent capabilities (session management, prompt features, MCP).
    pub agent_capabilities: AgentCapabilities,
    /// The agent's self-identification.
    pub agent_info: AgentInfo,
    /// Advertised config options, if any were built (none on `initialize`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config_options: Option<Vec<ConfigOption>>,
}

/// Capabilities the agent advertises to the editor.
#[derive(serde::Serialize, serde::Deserialize, Debug)]
pub struct AgentCapabilities {
    /// Whether the agent supports loading an existing session.
    #[serde(default)]
    pub load_session: bool,
    /// Prompt feature support (image/audio/embedded context).
    #[serde(default)]
    pub prompt_capabilities: PromptCapabilities,
    /// Session lifecycle support (list/delete/close).
    #[serde(default)]
    pub session_capabilities: SessionCapabilities,
    /// MCP transport support (http/sse).
    #[serde(default)]
    pub mcp_capabilities: McpCapabilities,
}

/// Session lifecycle operations the agent supports.
///
/// Each field's presence signals support; the bridge advertises all three.
#[derive(serde::Serialize, serde::Deserialize, Debug, Default)]
pub struct SessionCapabilities {
    /// The agent implements `session/list`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub list: Option<serde_json::Value>,
    /// The agent implements `session/delete`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delete: Option<serde_json::Value>,
    /// The agent implements `session/close`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub close: Option<serde_json::Value>,
}

/// MCP transport support advertised by the agent.
#[derive(serde::Serialize, serde::Deserialize, Debug, Default)]
pub struct McpCapabilities {
    /// Whether the agent can connect to MCP servers over HTTP.
    #[serde(default)]
    pub http: bool,
    /// Whether the agent can connect to MCP servers over SSE.
    #[serde(default)]
    pub sse: bool,
}

/// The agent's self-identification (name and version).
#[derive(serde::Serialize, serde::Deserialize, Debug)]
pub struct AgentInfo {
    /// Agent name (`"Choreographr"`).
    pub name: String,
    /// Agent version string.
    pub version: String,
}

// --- Configuration options ---

/// A single configurable option advertised to the editor (model, reasoning
/// effort, or tool groups): its identity, presentation metadata, type, and
/// current value.
#[derive(serde::Serialize, serde::Deserialize, Debug)]
pub struct ConfigOption {
    /// Machine-readable option id (e.g. `"model"`).
    pub id: String,
    /// Human-readable option name for display.
    pub name: String,
    /// Optional longer description of what the option controls.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Optional grouping category for the editor's settings UI.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub category: Option<String>,
    /// The widget type used to render the option.
    pub option_type: ConfigOptionType,
    /// The option's current value.
    pub current_value: ConfigOptionValue,
    /// Selectable choices; present only for [`ConfigOptionType::Select`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub options: Option<Vec<SelectOption>>,
}

/// The input widget used to render a [`ConfigOption`].
#[derive(serde::Serialize, serde::Deserialize, Debug)]
#[serde(rename_all = "snake_case")]
pub enum ConfigOptionType {
    /// A dropdown of fixed [`SelectOption`]s.
    Select,
    /// A free-text input field.
    TextField,
    /// A boolean on/off switch.
    Switch,
}

/// A config option's value: either a string or a boolean, tagged by shape
/// (`untagged`), so the JSON is a bare string/bool rather than an object.
#[derive(serde::Serialize, serde::Deserialize, Debug)]
#[serde(untagged)]
pub enum ConfigOptionValue {
    /// A textual value (also carries select/`TextField` choices).
    String(String),
    /// A boolean value (used by `Switch` options).
    Bool(bool),
}

/// One choice within a [`ConfigOptionType::Select`] option.
#[derive(serde::Serialize, serde::Deserialize, Debug)]
pub struct SelectOption {
    /// The value sent back when this choice is selected.
    pub value: String,
    /// Optional display label; the value is shown when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

// --- Session management ---

/// Parameters of a `session/new` request: an optional caller-suggested session
/// id, initial config values, and opaque client metadata.
#[derive(serde::Serialize, serde::Deserialize, Debug)]
pub struct NewSessionRequest {
    /// Optional caller-suggested ACP session id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    /// Initial config option values to apply to the new session.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config_options: Option<Vec<ConfigOptionValue>>,
    /// Opaque client metadata; the bridge reads `account_name` from it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<serde_json::Value>,
}

/// Result of a `session/new` request: the assigned ACP session id and the
/// config options the editor may now display.
#[derive(serde::Serialize, serde::Deserialize, Debug)]
pub struct NewSessionResult {
    /// The ACP session id assigned to the new session.
    pub session_id: String,
    /// Config options to advertise for the new session.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config_options: Option<Vec<ConfigOption>>,
}

/// Parameters of a `session/load` request.
#[derive(serde::Serialize, serde::Deserialize, Debug)]
pub struct LoadSessionRequest {
    /// The ACP session id to load.
    pub session_id: String,
}

/// Result of a `session/load` request.
#[derive(serde::Serialize, serde::Deserialize, Debug)]
pub struct LoadSessionResult {
    /// Config options reflecting the loaded session's restored state.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config_options: Option<Vec<ConfigOption>>,
}

// --- Prompt ---

/// Parameters of a `session/prompt` request: the target session and the user's
/// message as a sequence of content blocks.
#[derive(serde::Serialize, serde::Deserialize, Debug)]
pub struct PromptRequest {
    /// The ACP session id the prompt targets.
    pub session_id: String,
    /// The user's message, split into text/resource/image blocks.
    pub prompt: Vec<ContentBlock>,
}

/// A single piece of prompt content, tagged by `type` so each variant maps to
/// one ACP content-block shape.
#[derive(serde::Serialize, serde::Deserialize, Debug)]
#[serde(tag = "type")]
pub enum ContentBlock {
    /// Plain text content.
    #[serde(rename = "text")]
    Text {
        /// The text of the block.
        text: String,
    },
    /// A referenced resource (file/URI) and its content.
    #[serde(rename = "resource")]
    Resource {
        /// The resource payload.
        resource: ResourceContent,
    },
    /// An inline image attachment.
    #[serde(rename = "image")]
    Image {
        /// The image payload.
        image: ImageContent,
    },
}

/// A resource content block: a URI plus its (untyped) content.
#[derive(serde::Serialize, serde::Deserialize, Debug)]
pub struct ResourceContent {
    /// Identifier of the referenced resource (e.g. a `file://` URI).
    pub uri: String,
    /// The resource's content; a string, object, or `null`.
    pub content: serde_json::Value,
}

/// An image content block: base64 data plus an optional MIME type.
#[derive(serde::Serialize, serde::Deserialize, Debug)]
pub struct ImageContent {
    /// Base64-encoded image bytes.
    pub data: String,
    /// Optional MIME type (e.g. `"image/png"`); `None` when unspecified.
    pub mime_type: Option<String>,
}

/// Result of a `session/prompt` request, sent once the turn reaches a terminal
/// state.
#[derive(serde::Serialize, serde::Deserialize, Debug)]
pub struct PromptResult {
    /// Why the turn ended (e.g. `"end_turn"`, `"refusal"`, `"cancelled"`).
    pub stop_reason: String,
    /// Token usage for the turn, when the daemon reported it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<UsageInfo>,
}

/// Token-usage counters reported for a completed turn.
#[derive(serde::Serialize, serde::Deserialize, Debug)]
pub struct UsageInfo {
    /// Input tokens consumed by the turn.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub used_input_tokens: Option<u32>,
    /// Output tokens produced by the turn.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub used_output_tokens: Option<u32>,
    /// Reasoning tokens consumed by the turn.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub used_reasoning_tokens: Option<u32>,
}

// --- Cancel ---

/// Parameters of a `session/cancel` notification.
#[derive(serde::Serialize, serde::Deserialize, Debug)]
pub struct CancelNotification {
    /// The ACP session id whose active prompt should be cancelled.
    pub session_id: String,
}

// --- List sessions ---

/// Result of a `session/list` request.
#[derive(serde::Serialize, serde::Deserialize, Debug)]
pub struct ListSessionsResult {
    /// The known sessions, one entry per daemon session.
    pub sessions: Vec<SessionInfo>,
}

/// A single entry in a [`ListSessionsResult`].
#[derive(serde::Serialize, serde::Deserialize, Debug)]
pub struct SessionInfo {
    /// The ACP session id (or a `daemon_<id>` fallback for unmapped sessions).
    pub session_id: String,
    /// The session title, if any.
    pub title: Option<String>,
    /// The session's currently selected model, if any.
    pub model: Option<String>,
    /// Creation time in Unix seconds.
    pub created_at: Option<i64>,
}

// --- Delete session ---

/// Parameters of a `session/delete` request.
#[derive(serde::Serialize, serde::Deserialize, Debug)]
pub struct DeleteSessionRequest {
    /// The ACP session id to delete.
    pub session_id: String,
}

// --- Close session ---

/// Parameters of a `session/close` request.
#[derive(serde::Serialize, serde::Deserialize, Debug)]
pub struct CloseSessionRequest {
    /// The ACP session id to close.
    pub session_id: String,
}

// --- Set config option ---

/// Parameters of a `session/set_config_option` request.
#[derive(serde::Serialize, serde::Deserialize, Debug)]
pub struct SetConfigOptionRequest {
    /// The ACP session id whose config is being changed.
    pub session_id: String,
    /// The id of the config option to change (`model`, `reasoning_effort`, or
    /// `tool_groups`).
    pub config_id: String,
    /// The new value for the option.
    pub value: ConfigOptionValue,
}

// --- Session update notifications (streaming) ---

/// Parameters of a `session/update` notification: the target session plus the
/// flattened [`SessionUpdateVariant`] payload.
#[derive(serde::Serialize, serde::Deserialize, Debug)]
pub struct SessionUpdateParams {
    /// The ACP session id the update applies to.
    pub session_id: String,
    /// The update payload, flattened into the params object.
    #[serde(flatten)]
    pub variant: SessionUpdateVariant,
}

/// The payload of a `session/update` notification, tagged by `type`.
#[derive(serde::Serialize, serde::Deserialize, Debug)]
#[serde(tag = "type")]
pub enum SessionUpdateVariant {
    /// A chunk of the assistant's streaming message.
    #[serde(rename = "agent_message_chunk")]
    AgentMessageChunk {
        /// Identifier grouping the chunks of one assistant message.
        message_id: String,
        /// The chunk's content.
        content: ContentBlock,
    },
    /// A tool call has started.
    #[serde(rename = "tool_call")]
    ToolCall {
        /// Identifier matching this call's later updates.
        tool_call_id: String,
        /// Display title for the tool call.
        title: String,
        /// Normalised tool kind (e.g. `"read"`, `"terminal"`).
        kind: String,
        /// Current status (e.g. `"running"`).
        status: String,
        /// Initial content for the tool call (e.g. its arguments).
        content: Vec<ContentBlock>,
        /// Optional file locations the call touches.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        locations: Option<Vec<serde_json::Value>>,
    },
    /// An update to an in-flight tool call (progress or terminal status).
    #[serde(rename = "tool_call_update")]
    ToolCallUpdate {
        /// Identifier of the tool call being updated.
        tool_call_id: String,
        /// The updated status (e.g. `"running"`, `"completed"`, `"failed"`).
        status: String,
        /// Optional new content for the tool call.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        content: Option<Vec<ContentBlock>>,
    },
    /// Token-usage counters for the session.
    #[serde(rename = "usage_update")]
    UsageUpdate {
        /// Input tokens consumed so far.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        used_input_tokens: Option<u32>,
        /// Output tokens produced so far.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        used_output_tokens: Option<u32>,
        /// Reasoning tokens consumed so far.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        used_reasoning_tokens: Option<u32>,
    },
    /// A session-level status change (e.g. `"completed"`, `"refusal"`).
    #[serde(rename = "status_update")]
    StatusUpdate {
        /// The new session status.
        status: String,
    },
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
            ConfigOptionValue::String(_) => panic!("expected Bool"),
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
                name: "Choreographr".into(),
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
        assert!(json.contains(r#""name":"Choreographr""#));
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
