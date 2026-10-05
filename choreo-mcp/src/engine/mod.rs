//! The `rmcp`-backed [`McpEngine`]: the only place in the crate that names an
//! `rmcp` type.
//!
//! [`connect`] spawns the server subprocess and performs the lifecycle
//! handshake (`server/discover` with an `initialize` fallback, or one pinned
//! era), then hands back an engine wrapping the live [`Peer`]. Every other
//! module works in terms of the crate's own [`McpTool`] / [`CallToolResult`],
//! so the daemon is never coupled to rmcp's API.
//!
//! The tree is split by concern: this module owns the engine, its transport
//! selection, and the [`McpEngine`] implementation; the `http` submodule owns
//! the Streamable HTTP transport (client, connect retry, status recovery, and
//! the legacy-SSE probe), `handler` the client handler, `convert` the value
//! mapping onto the crate's own types, and `call` the `tools/call` path
//! (deadline, cancellation, progress forwarding, and the MRTR loop).

mod call;
mod convert;
mod handler;
mod http;

use self::call::call_tool_impl;
use self::convert::{
    auth_hint, convert_listed_resource, convert_resource_contents, convert_tools, map_service_error,
};
use self::handler::ServerHandler;
use self::http::{
    build_http_transport, connect_error_status, http_client, looks_like_legacy_sse,
    retryable_connect,
};
use crate::config::{McpProtocolMode, McpServerConfig, McpTransport};
use crate::error::McpError;
use crate::protocol::{
    CallToolResult, McpContent, McpListChange, McpListKind, McpResource, McpTool,
};
use crate::session::{BoxFuture, EngineCall, EngineFactory, McpEngine};
use rmcp::model::{
    ClientCapabilities, ClientConfig, Implementation, ProtocolVersion, ServerNotification,
    ServerPeerInfo, SubscriptionFilter,
};
use rmcp::service::{Peer, RoleClient, RunningService};
use rmcp::{ClientLifecycleMode, serve_client_with_lifecycle};
use std::sync::Arc;
use std::time::Duration;

/// The `clientInfo.name` this client advertises.
const CLIENT_NAME: &str = "choreographr";

/// Upper bound on a single SSE event accepted from a Streamable HTTP server.
///
/// rmcp parses the event stream; this cap keeps a hostile or buggy server from
/// feeding an unbounded event into memory. The stdio path has the analogous
/// [`MAX_STDIO_FRAME_BYTES`](crate::MAX_STDIO_FRAME_BYTES) bound, applied by the
/// crate's own capped child-process transport.
const MAX_SSE_EVENT_BYTES: usize = 16 * 1024 * 1024;

/// Maximum SSE reconnect attempts before the transport gives up on an ended
/// server stream.
///
/// rmcp's default SSE retry policy reconnects forever. This bounds the attempts
/// (with the same exponential backoff and ceiling as [`crate::retry`]) so a
/// permanently dead endpoint is not hammered and the failure surfaces instead.
const SSE_MAX_RECONNECTS: usize = 3;

/// Buffer depth of the per-connection server-event broadcast.
///
/// Progress and list-change notifications are best-effort: a call that falls
/// behind the buffer skips the missed events rather than stalling the reader.
const SERVER_EVENT_BUFFER: usize = 64;

/// The `rmcp`-backed engine for one connected server.
pub(crate) struct RmcpEngine {
    /// The peer handle for request/response and notifications. Cloneable, so
    /// every call task can hold its own.
    peer: Peer<RoleClient>,
    /// The running service, kept alive so the connection stays open; closed
    /// exactly once on shutdown. This is a lifecycle handle (never touched per
    /// message), guarded only because `close` needs `&mut`. This is the
    /// sanctioned shared-state exception #8 (see AGENTS.md).
    running: tokio::sync::Mutex<Option<RunningService<RoleClient, ServerHandler>>>,
    /// Broadcast of server notifications for this connection. Every call task
    /// subscribes to filter out the progress for its own request.
    events: tokio::sync::broadcast::Sender<handler::ServerEvent>,
    name: String,
    version: String,
    /// Per-server request timeout for listings (calls carry their own).
    timeout: Duration,
    /// Whether the server declared the `resources` capability.
    has_resources: bool,
    /// The server slug, used to name the server in an actionable
    /// authorization-required error.
    slug: String,
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
pub(crate) fn connect(
    config: &McpServerConfig,
    list_changes: Option<crossbeam_channel::Sender<McpListChange>>,
) -> Result<Arc<dyn McpEngine>, McpError> {
    let timeout = config.request_timeout();

    // The transport is built and the handshake driven on the sidecar runtime:
    // `tokio::process` needs a runtime context, and `rmcp`'s serving loop and
    // HTTP worker do too.
    // The per-connection event broadcast is created before the transport so
    // the `ClientHandler` (built inside the handshake) can hold its sender; the
    // engine keeps the same sender so call tasks can subscribe.
    let (events, _rx) = tokio::sync::broadcast::channel(SERVER_EVENT_BUFFER);
    let running = crate::runtime::block_on(connect_transport(config, &events))??;

    let peer = running.peer().clone();
    let (name, version) = server_identity(&running);
    let has_resources = server_has_resources(&running);

    // Open the list-change subscription against the freshly negotiated peer,
    // before the running service is moved into the engine. The subscription
    // task owns its own `Peer` clone, so it is independent of the engine's
    // lifecycle; a reconnect re-runs this and opens a fresh stream.
    if let Some(sender) = list_changes {
        spawn_list_change_listener(&peer, running.peer_info().as_deref(), &config.slug, sender);
    }

    Ok(Arc::new(RmcpEngine {
        peer,
        running: tokio::sync::Mutex::new(Some(running)),
        events,
        name,
        version,
        timeout,
        has_resources,
        slug: config.slug.clone(),
    }))
}

/// Build the reconnect factory the dispatcher uses to rebuild a dead engine.
///
/// The factory carries the list-change sender so a rebuilt transport
/// re-establishes its `subscriptions/listen` stream.
pub(crate) fn factory(
    config: McpServerConfig,
    list_changes: Option<crossbeam_channel::Sender<McpListChange>>,
) -> EngineFactory {
    Box::new(move || connect(&config, list_changes.clone()))
}

/// Spawn the `subscriptions/listen` reader for a server that supports it.
///
/// The stateless era delivers list changes only on a `subscriptions/listen`
/// stream, so a client that wants live tool catalogues must open one. The
/// request does not exist before that era, so this is gated on the negotiated
/// protocol version, and on the server actually advertising a list-changed
/// capability (an empty filter would subscribe to nothing).
fn spawn_list_change_listener(
    peer: &Peer<RoleClient>,
    peer_info: Option<&ServerPeerInfo>,
    slug: &str,
    sender: crossbeam_channel::Sender<McpListChange>,
) {
    let Some(info) = peer_info else {
        return;
    };
    // `subscriptions/listen` exists only from the stateless era onward.
    if info.protocol_version.has_initialize() {
        return;
    }
    let filter = SubscriptionFilter::builder()
        .tools_list_changed()
        .resources_list_changed()
        .build()
        .supported_by(&info.capabilities);
    if !filter_has_any(&filter) {
        return;
    }
    let peer = peer.clone();
    let slug = slug.to_string();
    // A missing runtime disables the subscription rather than failing the
    // connection: the server is still fully usable without live refresh.
    let Ok(handle) = crate::runtime::handle() else {
        tracing::warn!(
            server = %slug,
            "sidecar runtime unavailable; list-change subscription disabled"
        );
        return;
    };
    handle.spawn(async move {
        listen_for_changes(peer, filter, slug, sender).await;
    });
}

/// Whether a subscription filter opts in to at least one notification category.
fn filter_has_any(filter: &SubscriptionFilter) -> bool {
    filter.tools_list_changed == Some(true)
        || filter.prompts_list_changed == Some(true)
        || filter.resources_list_changed == Some(true)
        || filter
            .resource_subscriptions
            .as_ref()
            .is_some_and(|uris| !uris.is_empty())
}

/// Open one `subscriptions/listen` stream and forward each list-changed event.
///
/// Runs until the subscription ends or the transport closes; a reconnect
/// re-spawns a fresh listener (see [`connect`]), so an ended stream is not
/// retried here.
async fn listen_for_changes(
    peer: Peer<RoleClient>,
    filter: SubscriptionFilter,
    slug: String,
    sender: crossbeam_channel::Sender<McpListChange>,
) {
    let mut subscription = match peer.listen(filter).await {
        Ok(subscription) => subscription,
        Err(e) => {
            tracing::warn!(server = %slug, error = %e, "failed to open list-change subscription");
            return;
        }
    };
    tracing::debug!(server = %slug, "opened list-change subscription");
    loop {
        match subscription.next().await {
            Ok(Some(ServerNotification::ToolListChangedNotification(_))) => {
                let change = McpListChange {
                    slug: slug.clone(),
                    kind: McpListKind::Tools,
                };
                if sender.send(change).is_err() {
                    // The daemon dropped its receiver (shutdown); stop reading.
                    return;
                }
            }
            Ok(Some(ServerNotification::ResourceListChangedNotification(_))) => {
                let change = McpListChange {
                    slug: slug.clone(),
                    kind: McpListKind::Resources,
                };
                if sender.send(change).is_err() {
                    return;
                }
            }
            // Any other notification is outside the filter; ignore it.
            Ok(Some(_)) => {}
            Ok(None) => {
                tracing::debug!(server = %slug, "list-change subscription ended");
                return;
            }
            Err(e) => {
                tracing::warn!(server = %slug, error = %e, "list-change subscription error");
                return;
            }
        }
    }
}

/// Establish the transport and drive the lifecycle handshake for `config`.
async fn connect_transport(
    config: &McpServerConfig,
    events: &tokio::sync::broadcast::Sender<handler::ServerEvent>,
) -> Result<RunningService<RoleClient, ServerHandler>, McpError> {
    let handler = ServerHandler::new(client_config(config.protocol), events.clone());
    match &config.transport {
        McpTransport::Stdio { .. } => {
            let transport = crate::stdio::StdioTransport::spawn(config)?;
            serve_client_with_lifecycle(handler, transport, lifecycle_for(config.protocol))
                .await
                .map_err(|e| McpError::InitializeFailed(e.to_string()))
        }
        McpTransport::Http { url, .. } => connect_http(config, url, handler).await,
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
    handler: ServerHandler,
) -> Result<RunningService<RoleClient, ServerHandler>, McpError> {
    let client = http_client(config)?;
    let mut attempt = 0;
    loop {
        attempt += 1;
        let transport = build_http_transport(config, client.clone())?;
        match serve_client_with_lifecycle(
            handler.clone(),
            transport,
            lifecycle_for(config.protocol),
        )
        .await
        {
            Ok(running) => return Ok(running),
            Err(error) => {
                if attempt < crate::retry::MAX_ATTEMPTS && retryable_connect(&error) {
                    let backoff = crate::retry::connect_backoff(attempt);
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
                // An authorization failure is surfaced before anything else: the
                // legacy-SSE probe above only runs for a non-auth failure.
                if let Some(status) = connect_error_status(&error)
                    && matches!(status, 401 | 403)
                {
                    return Err(McpError::AuthRequired {
                        server: config.slug.clone(),
                        hint: auth_hint(status),
                    });
                }
                return Err(McpError::InitializeFailed(error.to_string()));
            }
        }
    }
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
fn server_identity(running: &RunningService<RoleClient, ServerHandler>) -> (String, String) {
    let info: Option<Arc<ServerPeerInfo>> = running.peer_info();
    let implementation = info.as_ref().and_then(|info| info.server_info.as_ref());
    let name = implementation.map_or_else(|| "unknown".to_string(), |i| i.name.clone());
    let version = implementation.map_or_else(|| "0.0.0".to_string(), |i| i.version.clone());
    (name, version)
}

/// Whether the server declared the `resources` capability.
///
/// The daemon registers its `read_resource`/`list_resources` wrapper tools only
/// for a server that advertises resources, so they are not offered to a model
/// for a server that cannot serve them.
fn server_has_resources(running: &RunningService<RoleClient, ServerHandler>) -> bool {
    running
        .peer_info()
        .is_some_and(|info| info.capabilities.resources.is_some())
}

impl McpEngine for RmcpEngine {
    fn list_tools(
        &self,
        timeout: Option<Duration>,
    ) -> BoxFuture<'_, Result<Vec<McpTool>, McpError>> {
        let peer = self.peer.clone();
        // A caller-supplied deadline (the daemon's catalogue-refresh budget)
        // overrides the server's configured listing timeout for this request.
        let timeout = timeout.unwrap_or(self.timeout);
        let slug = self.slug.clone();
        Box::pin(async move {
            // `list_all_tools` follows `nextCursor` to completion; the total is
            // bounded by the server's configured timeout.
            match tokio::time::timeout(timeout, peer.list_all_tools()).await {
                Ok(Ok(tools)) => Ok(convert_tools(tools)),
                Ok(Err(e)) => Err(map_service_error(e, &slug)),
                Err(_) => Err(McpError::Timeout),
            }
        })
    }

    fn call_tool(&self, call: EngineCall) -> BoxFuture<'_, Result<CallToolResult, McpError>> {
        let peer = self.peer.clone();
        let events = self.events.clone();
        let slug = self.slug.clone();
        Box::pin(async move { call_tool_impl(&peer, &events, call, &slug).await })
    }

    fn list_resources(&self) -> BoxFuture<'_, Result<Vec<McpResource>, McpError>> {
        let peer = self.peer.clone();
        let timeout = self.timeout;
        let slug = self.slug.clone();
        Box::pin(async move {
            match tokio::time::timeout(timeout, peer.list_all_resources()).await {
                Ok(Ok(resources)) => {
                    Ok(resources.into_iter().map(convert_listed_resource).collect())
                }
                Ok(Err(e)) => Err(map_service_error(e, &slug)),
                Err(_) => Err(McpError::Timeout),
            }
        })
    }

    fn read_resource(&self, uri: String) -> BoxFuture<'_, Result<Vec<McpContent>, McpError>> {
        let peer = self.peer.clone();
        let timeout = self.timeout;
        let slug = self.slug.clone();
        Box::pin(async move {
            let params = rmcp::model::ReadResourceRequestParams::new(uri);
            match tokio::time::timeout(timeout, peer.read_resource_once(params)).await {
                Ok(Ok(rmcp::model::ReadResourceResponse::Complete(result))) => Ok(result
                    .contents
                    .into_iter()
                    .map(convert_resource_contents)
                    .collect()),
                // MRTR on `resources/read` is not driven; report what was asked.
                Ok(Ok(rmcp::model::ReadResourceResponse::InputRequired(_))) => {
                    Err(McpError::ProtocolError(
                        "resources/read requested client input, which this client cannot provide"
                            .into(),
                    ))
                }
                Ok(Ok(_)) => Err(McpError::ProtocolError(
                    "resources/read returned an unsupported result type".into(),
                )),
                Ok(Err(e)) => Err(map_service_error(e, &slug)),
                Err(_) => Err(McpError::Timeout),
            }
        })
    }

    fn supports_resources(&self) -> bool {
        self.has_resources
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filter_has_any_detects_opted_in_categories() {
        assert!(!filter_has_any(&SubscriptionFilter::default()));
        assert!(filter_has_any(
            &SubscriptionFilter::builder().tools_list_changed().build()
        ));
        assert!(filter_has_any(
            &SubscriptionFilter::builder()
                .resources_list_changed()
                .build()
        ));
    }

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
}
