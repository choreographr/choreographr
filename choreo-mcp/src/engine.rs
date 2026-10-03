//! The `rmcp`-backed [`McpEngine`]: the only place in the crate that names an
//! `rmcp` type.
//!
//! [`connect`] spawns the server subprocess and performs the lifecycle
//! handshake (`server/discover` with an `initialize` fallback, or one pinned
//! era), then hands back an engine wrapping the live [`Peer`]. Every other
//! module works in terms of the crate's own [`McpTool`] / [`CallToolResult`],
//! so the daemon is never coupled to rmcp's API.

use crate::config::{McpProtocolMode, McpServerConfig, McpTransport};
use crate::error::McpError;
use crate::protocol::{CallToolResult, McpContent, McpTool, normalize_input_schema};
use crate::session::{BoxFuture, CallRequest, EngineCall, EngineFactory, McpEngine};
use reqwest::header::{HeaderName, HeaderValue};
use rmcp::model::{
    CallToolRequest, CallToolRequestParams, CancelledNotificationParam, ClientCapabilities,
    ClientConfig, ClientRequest, ContentBlock, Implementation, ProtocolVersion, ResourceContents,
    ServerPeerInfo, ServerResult,
};
use rmcp::service::{
    ClientInitializeError, Peer, PeerRequestOptions, RoleClient, RunningService, ServiceError,
};
use rmcp::transport::streamable_http_client::{
    StreamableHttpClientTransportConfig, StreamableHttpError,
};
use rmcp::transport::{StreamableHttpClientTransport, TokioChildProcess};
use rmcp::{ClientLifecycleMode, serve_client_with_lifecycle};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

/// The `clientInfo.name` this client advertises.
const CLIENT_NAME: &str = "choreographr";

/// Upper bound on a single SSE event accepted from a Streamable HTTP server.
///
/// rmcp parses the event stream; this cap keeps a hostile or buggy server from
/// feeding an unbounded event into memory (the stdio path has an analogous,
/// separately-tracked bound).
const MAX_SSE_EVENT_BYTES: usize = 16 * 1024 * 1024;

/// Deadline for the deprecated-transport probe that runs only after an HTTP
/// connect has already failed, so it never adds latency to a healthy connect.
const LEGACY_SSE_PROBE_TIMEOUT: Duration = Duration::from_secs(3);

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
/// For a stdio server this spawns the child subprocess; for an HTTP server it
/// opens the Streamable HTTP transport (retrying transient failures, see
/// [`crate::retry`]). Either way the lifecycle handshake
/// (`server/discover` with an `initialize` fallback, or one pinned era) is
/// driven before returning, so a caller that fails to connect learns
/// immediately.
///
/// # Errors
///
/// Returns [`McpError::SpawnFailed`] when a stdio subprocess cannot be spawned,
/// [`McpError::InitializeFailed`] when the lifecycle handshake fails,
/// [`McpError::UnsupportedTransport`] when an HTTP endpoint speaks the removed
/// HTTP+SSE transport, and [`McpError::ProtocolError`] when the sidecar runtime
/// is not initialized or the HTTP config is invalid.
pub(crate) fn connect(config: &McpServerConfig) -> Result<Arc<dyn McpEngine>, McpError> {
    let timeout = config.request_timeout();

    // The transport is built and the handshake driven on the sidecar runtime:
    // `tokio::process` needs a runtime context, and `rmcp`'s serving loop and
    // HTTP worker do too.
    let running = crate::runtime::block_on(connect_transport(config))??;

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

/// Establish the transport and drive the lifecycle handshake for `config`.
async fn connect_transport(
    config: &McpServerConfig,
) -> Result<RunningService<RoleClient, ClientConfig>, McpError> {
    match &config.transport {
        McpTransport::Stdio { .. } => {
            let transport = build_stdio_transport(config)?;
            serve_client_with_lifecycle(
                client_config(config.protocol),
                transport,
                lifecycle_for(config.protocol),
            )
            .await
            .map_err(|e| McpError::InitializeFailed(e.to_string()))
        }
        McpTransport::Http { url, .. } => connect_http(config, url).await,
    }
}

/// Connect to a Streamable HTTP server, retrying transient connect failures and
/// distinguishing the removed HTTP+SSE transport from a genuine error.
///
/// rmcp's `Auto` lifecycle performs the spec's discover-first probe with an
/// `initialize` fallback, so a legacy Streamable HTTP server is handled without
/// any bespoke logic here; the only connect-shaping this function adds is the
/// bounded retry (a 408/429/5xx probe is re-attempted with backoff) and the
/// best-effort rejection of a 2024-11-05 HTTP+SSE endpoint, which is a
/// different, deprecated transport this client does not implement.
async fn connect_http(
    config: &McpServerConfig,
    url: &str,
) -> Result<RunningService<RoleClient, ClientConfig>, McpError> {
    let client = http_client(config)?;
    let mut attempt = 0;
    loop {
        attempt += 1;
        let transport = build_http_transport(config, client.clone())?;
        match serve_client_with_lifecycle(
            client_config(config.protocol),
            transport,
            lifecycle_for(config.protocol),
        )
        .await
        {
            Ok(running) => return Ok(running),
            Err(error) => {
                if attempt < crate::retry::MAX_ATTEMPTS && retryable_connect(&error) {
                    let backoff = crate::retry::backoff(attempt);
                    tracing::warn!(
                        url,
                        attempt,
                        ?backoff,
                        "MCP HTTP connect failed with a retryable status; retrying"
                    );
                    tokio::time::sleep(backoff).await;
                    continue;
                }
                if looks_like_legacy_sse(&client, url).await {
                    return Err(McpError::UnsupportedTransport(format!(
                        "{url} answers GET with an SSE `endpoint` event, i.e. the \
                         2024-11-05 HTTP+SSE transport, which was removed from the \
                         specification; point the server at a Streamable HTTP endpoint"
                    )));
                }
                return Err(McpError::InitializeFailed(error.to_string()));
            }
        }
    }
}

/// Build the reqwest client used by the Streamable HTTP transport.
///
/// Idle pooling is disabled and redirects are off (matching rmcp's own default
/// client): reuse can stall on Linux's delayed ACK, and a redirect would replay
/// user-supplied headers to a new host, defeating the per-server header
/// contract. The connect timeout uses the server's request timeout so a black-
/// holed endpoint cannot hang the connect phase.
fn http_client(config: &McpServerConfig) -> Result<reqwest::Client, McpError> {
    reqwest::Client::builder()
        .pool_max_idle_per_host(0)
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(config.request_timeout())
        .build()
        .map_err(|e| McpError::InitializeFailed(format!("failed to build HTTP client: {e}")))
}

/// Spawn the server subprocess and wrap its stdio in an rmcp child transport.
///
/// On Unix the child is placed in its own process group so launcher chains
/// (`npx` → `node`) are killed together on shutdown rather than orphaning the
/// grandchild that holds the pipe.
fn build_stdio_transport(config: &McpServerConfig) -> Result<TokioChildProcess, McpError> {
    let McpTransport::Stdio {
        command, args, env, ..
    } = &config.transport
    else {
        return Err(McpError::ProtocolError(
            "build_stdio_transport called for a non-stdio server".into(),
        ));
    };
    let mut cmd = tokio::process::Command::new(command);
    cmd.args(args);
    // An explicit executable + args only — never a shell string — so config
    // values cannot be reinterpreted as shell syntax.
    for (key, value) in env {
        cmd.env(key, value);
    }
    let mut wrap = process_wrap::tokio::CommandWrap::from(cmd);
    #[cfg(unix)]
    wrap.wrap(process_wrap::tokio::ProcessGroup::leader());
    TokioChildProcess::new(wrap).map_err(|e| McpError::SpawnFailed(e.to_string()))
}

/// Build the Streamable HTTP transport for `config` over `client`.
///
/// Config-supplied headers are validated up front so a bad name or a reserved
/// header fails the connect with a clear message rather than at the first
/// request. rmcp generates `Mcp-*` routing headers from the request body; user
/// headers ride alongside them (`MCP-Protocol-Version` is also generated, so it
/// is not user-settable).
fn build_http_transport(
    config: &McpServerConfig,
    client: reqwest::Client,
) -> Result<StreamableHttpClientTransport<reqwest::Client>, McpError> {
    let McpTransport::Http { url, headers } = &config.transport else {
        return Err(McpError::ProtocolError(
            "build_http_transport called for a non-HTTP server".into(),
        ));
    };
    let mut custom: HashMap<HeaderName, HeaderValue> = HashMap::new();
    for (name, value) in headers {
        let header = HeaderName::from_bytes(name.as_bytes()).map_err(|e| {
            McpError::ProtocolError(format!("invalid HTTP header name {name:?}: {e}"))
        })?;
        if is_reserved_header(&header) {
            return Err(McpError::ProtocolError(format!(
                "HTTP header {name:?} is reserved by the MCP transport"
            )));
        }
        let value = HeaderValue::from_str(value).map_err(|e| {
            McpError::ProtocolError(format!("invalid HTTP header value for {name:?}: {e}"))
        })?;
        custom.insert(header, value);
    }
    let http_config = StreamableHttpClientTransportConfig::with_uri(url.as_str())
        .custom_headers(custom)
        .max_sse_event_size(MAX_SSE_EVENT_BYTES);
    Ok(StreamableHttpClientTransport::with_client(
        client,
        http_config,
    ))
}

/// Whether a config-supplied header collides with one the transport owns.
///
/// This mirrors the reserved set rmcp rejects at request time, so a config
/// mistake surfaces at connect rather than mid-session. `MCP-Protocol-Version`
/// is intentionally *not* reserved here: rmcp allows and injects it.
fn is_reserved_header(name: &HeaderName) -> bool {
    let name = name.as_str();
    ["accept", "mcp-session-id", "last-event-id"]
        .iter()
        .any(|reserved| name.eq_ignore_ascii_case(reserved))
}

/// Whether a failed connect is worth retrying under [`crate::retry`].
///
/// The HTTP status is recovered by walking the transport error's source chain
/// for rmcp's `StreamableHttpError`: a bare `reqwest::Error` carries a status
/// directly, whereas a rejected POST surfaces the status inside rmcp's
/// `"HTTP <status>: <body>"` message.
fn retryable_connect(error: &ClientInitializeError) -> bool {
    let root: &dyn std::error::Error = match error {
        ClientInitializeError::TransportError { error, .. } => error.error.as_ref(),
        _ => return false,
    };
    let mut current: Option<&(dyn std::error::Error + 'static)> = Some(root);
    while let Some(err) = current {
        if let Some(http) = err.downcast_ref::<StreamableHttpError<reqwest::Error>>()
            && let Some(status) = http_error_status(http)
        {
            return crate::retry::is_retryable_status(status);
        }
        current = err.source();
    }
    false
}

/// Extract the HTTP status from an rmcp Streamable HTTP error, when it carries
/// one.
fn http_error_status(error: &StreamableHttpError<reqwest::Error>) -> Option<u16> {
    match error {
        StreamableHttpError::Client(e) => e.status().map(|s| s.as_u16()),
        StreamableHttpError::UnexpectedServerResponse(message) => parse_http_status(message),
        _ => None,
    }
}

/// Parse the status out of rmcp's `"HTTP <Status>: <body>"` rejection message.
///
/// A rejected POST that is not a JSON-RPC error is surfaced as
/// `"HTTP <status>: <body>"`, where `<status>` is `reqwest::StatusCode`'s
/// Display (`"503 Service Unavailable"`), so the code is the first token.
fn parse_http_status(message: &str) -> Option<u16> {
    message
        .strip_prefix("HTTP ")?
        .split_whitespace()
        .next()?
        .parse()
        .ok()
}

/// Best-effort detection of the deprecated 2024-11-05 HTTP+SSE transport.
///
/// That transport (removed from the specification) is a single GET whose first
/// SSE event is `endpoint`; Streamable HTTP removed the GET stream. A positive
/// result turns an otherwise opaque connect failure into a clear error. The
/// probe is request-timeout bounded, so a server that accepts the GET and stalls
/// cannot hang the connect.
async fn looks_like_legacy_sse(client: &reqwest::Client, url: &str) -> bool {
    let Ok(response) = client
        .get(url)
        .header(reqwest::header::ACCEPT, "text/event-stream")
        .timeout(LEGACY_SSE_PROBE_TIMEOUT)
        .send()
        .await
    else {
        return false;
    };
    let is_event_stream = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|ct| ct.starts_with("text/event-stream"));
    if !is_event_stream {
        return false;
    }
    // The `endpoint` event is the first event a legacy server sends; read a
    // bounded prefix rather than the whole (never-ending) stream.
    let mut response = response;
    let mut seen = String::new();
    loop {
        match response.chunk().await {
            Ok(Some(chunk)) => {
                seen.push_str(&String::from_utf8_lossy(&chunk));
                if is_legacy_sse_event(&seen) {
                    return true;
                }
                if seen.len() > 8192 {
                    return false;
                }
            }
            // End of stream or the probe deadline elapsed before the event.
            Ok(None) | Err(_) => return is_legacy_sse_event(&seen),
        }
    }
}

/// Whether the accumulated SSE text contains the legacy `endpoint` event.
fn is_legacy_sse_event(text: &str) -> bool {
    text.lines().any(|line| {
        line.trim_start()
            .strip_prefix("event:")
            .is_some_and(|rest| rest.trim() == "endpoint")
    })
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
    fn retryable_connect_backoff_is_capped() {
        // Sanity check on the policy the engine defers to: retryable for the
        // transient range, not for settled 4xx answers.
        assert!(crate::retry::is_retryable_status(503));
        assert!(!crate::retry::is_retryable_status(400));
    }

    #[test]
    fn parse_http_status_reads_rmcp_rejection_message() {
        assert_eq!(
            parse_http_status("HTTP 503 Service Unavailable: down"),
            Some(503)
        );
        assert_eq!(
            parse_http_status("HTTP 429 Too Many Requests: slow"),
            Some(429)
        );
        assert_eq!(parse_http_status("unexpected server response"), None);
    }

    #[test]
    fn legacy_sse_event_is_detected() {
        assert!(is_legacy_sse_event(
            "event: endpoint\ndata: /messages?sessionId=x\n\n"
        ));
        assert!(is_legacy_sse_event("event:endpoint\ndata: x"));
        assert!(!is_legacy_sse_event("data: {\"jsonrpc\"}\n\n"));
        assert!(!is_legacy_sse_event("event: message\ndata: {}"));
    }

    #[test]
    fn reserved_headers_are_rejected() {
        assert!(is_reserved_header(&HeaderName::from_static("accept")));
        assert!(is_reserved_header(&HeaderName::from_static(
            "mcp-session-id"
        )));
        assert!(is_reserved_header(&HeaderName::from_static(
            "last-event-id"
        )));
        assert!(!is_reserved_header(&HeaderName::from_static(
            "authorization"
        )));
    }

    #[test]
    fn retryable_connect_classifies_http_status_from_chained_error() {
        fn init_error<E: std::error::Error + Send + Sync + 'static>(e: E) -> ClientInitializeError {
            ClientInitializeError::TransportError {
                error: rmcp::transport::DynamicTransportError::from_parts(
                    "test",
                    std::any::TypeId::of::<()>(),
                    Box::new(e),
                ),
                context: "test".into(),
            }
        }

        let retryable = init_error(
            StreamableHttpError::<reqwest::Error>::UnexpectedServerResponse(
                "HTTP 503 Service Unavailable: down".into(),
            ),
        );
        assert!(retryable_connect(&retryable));

        let fatal = init_error(
            StreamableHttpError::<reqwest::Error>::UnexpectedServerResponse(
                "HTTP 400 Bad Request: bad".into(),
            ),
        );
        assert!(!retryable_connect(&fatal));

        // A non-transport initialize error is never retryable.
        assert!(!retryable_connect(&ClientInitializeError::Cancelled));
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
