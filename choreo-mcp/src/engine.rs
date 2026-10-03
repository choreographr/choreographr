//! The `rmcp`-backed [`McpEngine`]: the only place in the crate that names an
//! `rmcp` type.
//!
//! [`connect`] spawns the server subprocess and performs the lifecycle
//! handshake (`server/discover` with an `initialize` fallback, or one pinned
//! era), then hands back an engine wrapping the live [`Peer`]. Every other
//! module works in terms of the crate's own [`McpTool`] / [`CallToolResult`],
//! so the daemon is never coupled to rmcp's API.

use crate::config::{McpProtocolMode, McpServerConfig};
use crate::error::McpError;
use crate::protocol::{CallToolResult, McpContent, McpTool, normalize_input_schema};
use crate::session::{BoxFuture, CallRequest, EngineCall, EngineFactory, McpEngine};
use rmcp::model::{
    CallToolRequest, CallToolRequestParams, CancelledNotificationParam, ClientCapabilities,
    ClientConfig, ClientRequest, ContentBlock, Implementation, ProtocolVersion, ResourceContents,
    ServerPeerInfo, ServerResult,
};
use rmcp::service::{Peer, PeerRequestOptions, RoleClient, RunningService, ServiceError};
use rmcp::transport::TokioChildProcess;
use rmcp::{ClientLifecycleMode, serve_client_with_lifecycle};
use std::sync::Arc;
use std::time::Duration;

/// The `clientInfo.name` this client advertises.
const CLIENT_NAME: &str = "choreographr";

/// The `rmcp`-backed engine for one connected server.
pub(crate) struct RmcpEngine {
    /// The peer handle for request/response and notifications. Cloneable, so
    /// every call task can hold its own.
    peer: Peer<RoleClient>,
    /// The running service, kept alive so the connection stays open; closed
    /// exactly once on shutdown. This is a lifecycle handle (never touched per
    /// message), guarded only because `close` needs `&mut`.
    running: tokio::sync::Mutex<Option<RunningService<RoleClient, ClientConfig>>>,
    name: String,
    version: String,
    /// Per-server request timeout for listings (calls carry their own).
    timeout: Duration,
}

/// Connect to the server described by `config`, returning a ready engine.
///
/// # Errors
///
/// Returns [`McpError::SpawnFailed`] when the subprocess cannot be spawned,
/// [`McpError::InitializeFailed`] when the lifecycle handshake fails, and
/// [`McpError::ProtocolError`] when the sidecar runtime is not initialized.
pub(crate) fn connect(config: &McpServerConfig) -> Result<Arc<dyn McpEngine>, McpError> {
    let client_info = client_config(config.protocol);
    let lifecycle = lifecycle_for(config.protocol);
    let timeout = config.request_timeout();

    // The child process is spawned and the handshake driven on the sidecar
    // runtime: `tokio::process` needs a runtime context, and `rmcp`'s serving
    // loop does too.
    let running = crate::runtime::block_on(async {
        let transport = build_transport(config)?;
        serve_client_with_lifecycle(client_info, transport, lifecycle)
            .await
            .map_err(|e| McpError::InitializeFailed(e.to_string()))
    })?;
    let running = running?;

    let peer = running.peer().clone();
    let (name, version) = server_identity(&running);
    Ok(Arc::new(RmcpEngine {
        peer,
        running: tokio::sync::Mutex::new(Some(running)),
        name,
        version,
        timeout,
    }))
}

/// Build the reconnect factory the dispatcher uses to rebuild a dead engine.
pub(crate) fn factory(config: McpServerConfig) -> EngineFactory {
    Box::new(move || connect(&config))
}

/// Spawn the server subprocess and wrap its stdio in an rmcp child transport.
///
/// On Unix the child is placed in its own process group so launcher chains
/// (`npx` → `node`) are killed together on shutdown rather than orphaning the
/// grandchild that holds the pipe.
fn build_transport(config: &McpServerConfig) -> Result<TokioChildProcess, McpError> {
    let mut cmd = tokio::process::Command::new(&config.command);
    cmd.args(&config.args);
    // An explicit executable + args only — never a shell string — so config
    // values cannot be reinterpreted as shell syntax.
    for (key, value) in &config.env {
        cmd.env(key, value);
    }
    let mut wrap = process_wrap::tokio::CommandWrap::from(cmd);
    #[cfg(unix)]
    wrap.wrap(process_wrap::tokio::ProcessGroup::leader());
    TokioChildProcess::new(wrap).map_err(|e| McpError::SpawnFailed(e.to_string()))
}

/// The `clientInfo` / capability object advertised to the server.
fn client_config(mode: McpProtocolMode) -> ClientConfig {
    let mut config = ClientConfig::new(
        // No elicitation/sampling/roots: this client cannot honor them yet, and
        // the spec forbids advertising a capability that is not served.
        ClientCapabilities::default(),
        Implementation::new(CLIENT_NAME, env!("CARGO_PKG_VERSION")),
    );
    if mode == McpProtocolMode::Legacy {
        // The newest era that still answers `initialize`.
        config = config.with_protocol_version(ProtocolVersion::LATEST_WITH_INITIALIZE);
    }
    config
}

/// Map the per-server protocol mode onto rmcp's lifecycle selection.
fn lifecycle_for(mode: McpProtocolMode) -> ClientLifecycleMode {
    match mode {
        McpProtocolMode::Legacy => ClientLifecycleMode::Initialize,
        McpProtocolMode::Modern => ClientLifecycleMode::Discover {
            preferred_versions: vec![ProtocolVersion::V_2026_07_28],
        },
        McpProtocolMode::Auto => ClientLifecycleMode::Auto {
            preferred_versions: vec![ProtocolVersion::V_2026_07_28],
            legacy_version: Some(ProtocolVersion::V_2025_11_25),
        },
    }
}

/// Extract the server's self-reported name/version from the negotiated peer.
fn server_identity(running: &RunningService<RoleClient, ClientConfig>) -> (String, String) {
    let info: Option<Arc<ServerPeerInfo>> = running.peer_info();
    let implementation = info.as_ref().and_then(|info| info.server_info.as_ref());
    let name = implementation.map_or_else(|| "unknown".to_string(), |i| i.name.clone());
    let version = implementation.map_or_else(|| "0.0.0".to_string(), |i| i.version.clone());
    (name, version)
}

impl McpEngine for RmcpEngine {
    fn list_tools(&self) -> BoxFuture<'_, Result<Vec<McpTool>, McpError>> {
        let peer = self.peer.clone();
        let timeout = self.timeout;
        Box::pin(async move {
            // `list_all_tools` follows `nextCursor` to completion; the total is
            // bounded by the server's configured timeout.
            match tokio::time::timeout(timeout, peer.list_all_tools()).await {
                Ok(Ok(tools)) => Ok(convert_tools(tools)),
                Ok(Err(e)) => Err(map_service_error(e)),
                Err(_) => Err(McpError::Timeout),
            }
        })
    }

    fn call_tool(&self, call: EngineCall) -> BoxFuture<'_, Result<CallToolResult, McpError>> {
        let peer = self.peer.clone();
        Box::pin(async move { call_tool_impl(&peer, call).await })
    }

    fn shutdown(&self) -> BoxFuture<'_, ()> {
        Box::pin(async move {
            let mut guard = self.running.lock().await;
            if let Some(mut running) = guard.take() {
                // Bounded close so a wedged peer cannot hang shutdown.
                let _ = running.close_with_timeout(Duration::from_secs(3)).await;
            }
        })
    }

    fn name(&self) -> &str {
        &self.name
    }

    fn version(&self) -> &str {
        &self.version
    }
}

/// Drive one `tools/call`, honouring the deadline and the cancellation token.
async fn call_tool_impl(
    peer: &Peer<RoleClient>,
    call: EngineCall,
) -> Result<CallToolResult, McpError> {
    let EngineCall { request, cancel } = call;
    let CallRequest {
        name,
        arguments,
        timeout,
    } = request;

    let mut params = CallToolRequestParams::new(name);
    if let serde_json::Value::Object(map) = arguments {
        params.arguments = Some(map);
    }

    // Request-scoped options: the deadline resets while progress notifications
    // arrive (a long tool that reports progress is not killed mid-work).
    let options = PeerRequestOptions::with_timeout(timeout).reset_timeout_on_progress();
    let handle = peer
        .send_cancellable_request(
            ClientRequest::CallToolRequest(CallToolRequest::new(params)),
            options,
        )
        .await
        .map_err(map_service_error)?;
    let request_id = handle.id.clone();

    tokio::select! {
        biased;
        // Cancellation arm first: a session cancel stops the call the instant
        // it is observed, and we tell the server to stop cooperatively.
        () = cancel.cancelled() => {
            let _ = peer
                .notify_cancelled(CancelledNotificationParam::new(
                    Some(request_id),
                    Some("client cancelled".to_string()),
                ))
                .await;
            Err(McpError::Cancelled)
        }
        response = handle.await_response() => match response.map_err(map_service_error)? {
            ServerResult::CallToolResult(result) => Ok(convert_call_result(result)),
            // SEP-2322 `input_required` and the tasks extension are not yet
            // driven here; surface what the server asked for instead of hanging.
            other => Err(McpError::ProtocolError(format!(
                "tools/call returned a result this client does not handle: {other:?}"
            ))),
        },
    }
}

/// Map an `rmcp` service error onto this crate's error type.
fn map_service_error(error: ServiceError) -> McpError {
    match error {
        ServiceError::McpError(data) => McpError::JsonRpcError {
            code: i64::from(data.code.0),
            message: data.message.into_owned(),
        },
        ServiceError::TransportClosed => McpError::ServerShutdown,
        ServiceError::Timeout { .. } => McpError::Timeout,
        ServiceError::Cancelled { .. } => McpError::Cancelled,
        other => McpError::ProtocolError(other.to_string()),
    }
}

/// Convert rmcp tools, dropping any whose `inputSchema` is unusable.
fn convert_tools(tools: Vec<rmcp::model::Tool>) -> Vec<McpTool> {
    tools.into_iter().filter_map(convert_tool).collect()
}

/// Convert one rmcp tool, returning `None` when its schema must be rejected.
///
/// A tool with a non-object or oversized schema is dropped (the rest are kept),
/// per the spec's "exclude the offending tool" rule.
fn convert_tool(tool: rmcp::model::Tool) -> Option<McpTool> {
    let input_schema = normalize_input_schema(tool.schema_as_json_value())?;
    let output_schema = tool
        .output_schema
        .map(|schema| serde_json::Value::Object(schema.as_ref().clone()));
    Some(McpTool {
        name: tool.name.into_owned(),
        description: tool.description.map(std::borrow::Cow::into_owned),
        input_schema,
        output_schema,
    })
}

/// Convert an rmcp `tools/call` result into this crate's value type.
fn convert_call_result(result: rmcp::model::CallToolResult) -> CallToolResult {
    CallToolResult {
        content: result.content.into_iter().map(convert_content).collect(),
        is_error: result.is_error.unwrap_or(false),
        structured_content: result.structured_content,
    }
}

/// Convert one rmcp content block; unknown future variants degrade to a text
/// placeholder rather than vanishing.
fn convert_content(block: ContentBlock) -> McpContent {
    match block {
        ContentBlock::Text(text) => McpContent::Text { text: text.text },
        ContentBlock::Image(image) => McpContent::Image {
            data: image.data,
            mime_type: image.mime_type,
        },
        ContentBlock::Audio(audio) => McpContent::Audio {
            data: audio.data,
            mime_type: audio.mime_type,
        },
        ContentBlock::Resource(resource) => convert_resource(resource.resource),
        ContentBlock::ResourceLink(link) => McpContent::ResourceLink {
            uri: link.uri,
            name: Some(link.name),
            mime_type: link.mime_type,
        },
        _ => McpContent::Text {
            text: "[unsupported content block]".to_string(),
        },
    }
}

/// Convert an embedded resource's contents (text or blob).
fn convert_resource(contents: ResourceContents) -> McpContent {
    match contents {
        ResourceContents::TextResourceContents {
            uri,
            mime_type,
            text,
            ..
        } => McpContent::Resource {
            uri,
            mime_type,
            text: Some(text),
        },
        ResourceContents::BlobResourceContents { uri, mime_type, .. } => McpContent::Resource {
            uri,
            mime_type,
            text: None,
        },
        _ => McpContent::Text {
            text: "[unsupported resource contents]".to_string(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lifecycle_maps_modes() {
        assert!(matches!(
            lifecycle_for(McpProtocolMode::Legacy),
            ClientLifecycleMode::Initialize
        ));
        assert!(matches!(
            lifecycle_for(McpProtocolMode::Modern),
            ClientLifecycleMode::Discover { .. }
        ));
        assert!(matches!(
            lifecycle_for(McpProtocolMode::Auto),
            ClientLifecycleMode::Auto { .. }
        ));
    }

    #[test]
    fn content_blocks_convert() {
        let text = convert_content(ContentBlock::text("hello"));
        assert_eq!(
            text,
            McpContent::Text {
                text: "hello".into()
            }
        );
        let image = convert_content(ContentBlock::image("AAA", "image/png"));
        assert_eq!(
            image,
            McpContent::Image {
                data: "AAA".into(),
                mime_type: "image/png".into()
            }
        );
        let audio = convert_content(ContentBlock::audio("BBB", "audio/wav"));
        assert_eq!(
            audio,
            McpContent::Audio {
                data: "BBB".into(),
                mime_type: "audio/wav".into()
            }
        );
    }

    #[test]
    fn embedded_text_resource_converts() {
        let block = ContentBlock::embedded_text("file:///x", "body");
        assert_eq!(
            convert_content(block),
            McpContent::Resource {
                uri: "file:///x".into(),
                mime_type: Some("text/plain".into()),
                text: Some("body".into()),
            }
        );
    }

    #[test]
    fn service_error_maps_json_rpc() {
        let data = rmcp::model::ErrorData::new(
            rmcp::model::ErrorCode::METHOD_NOT_FOUND,
            "nope".to_string(),
            None,
        );
        match map_service_error(ServiceError::McpError(data)) {
            McpError::JsonRpcError { code, message } => {
                assert_eq!(code, -32601);
                assert_eq!(message, "nope");
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn tool_conversion_keeps_object_schema() {
        let json = serde_json::json!({
            "name": "ok",
            "description": "d",
            "inputSchema": {"type": "object"}
        });
        let tool: rmcp::model::Tool = serde_json::from_value(json).expect("tool decodes");
        let converted = convert_tool(tool).expect("valid schema converts");
        assert_eq!(converted.name, "ok");
        assert_eq!(converted.input_schema["type"], "object");
    }

    #[test]
    fn tool_conversion_rejects_oversized_schema() {
        // rmcp itself enforces that `inputSchema` is a JSON object, so the
        // client-side guard that still matters is the byte cap.
        let filler = "x".repeat(crate::protocol::MAX_SCHEMA_BYTES + 1);
        let json = serde_json::json!({
            "name": "big",
            "inputSchema": {"type": "object", "description": filler}
        });
        let tool: rmcp::model::Tool = serde_json::from_value(json).expect("tool decodes");
        assert!(convert_tool(tool).is_none());
    }
}
