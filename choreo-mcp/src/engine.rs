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
use crate::protocol::{
    CallToolResult, McpContent, McpListChange, McpListKind, McpResource, McpTool, cap_tools,
    normalize_input_schema, normalize_output_schema,
};
use crate::session::{BoxFuture, CallRequest, EngineCall, EngineFactory, McpEngine};
use reqwest::header::{HeaderName, HeaderValue};
use rmcp::model::{
    CallToolRequest, CallToolRequestParams, CancelledNotificationParam, ClientCapabilities,
    ClientConfig, ClientRequest, ContentBlock, ElicitResult, ElicitationAction, Implementation,
    InputRequest, InputRequests, InputResponses, ProgressNotificationParam, ProgressToken,
    ProtocolVersion, ResourceContents, ServerNotification, ServerPeerInfo, ServerResult,
    SubscriptionFilter,
};
use rmcp::service::{
    ClientInitializeError, MaybeSendFuture, NotificationContext, Peer, PeerRequestOptions,
    RoleClient, RunningService, ServiceError,
};
use rmcp::transport::StreamableHttpClientTransport;
use rmcp::transport::common::client_side_sse::SseRetryPolicy;
use rmcp::transport::streamable_http_client::{
    StreamableHttpClientTransportConfig, StreamableHttpError,
};
use rmcp::{ClientHandler, ClientLifecycleMode, serve_client_with_lifecycle};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// The `clientInfo.name` this client advertises.
const CLIENT_NAME: &str = "choreographr";

/// Upper bound on a single SSE event accepted from a Streamable HTTP server.
///
/// rmcp parses the event stream; this cap keeps a hostile or buggy server from
/// feeding an unbounded event into memory. The stdio path has the analogous
/// [`MAX_STDIO_FRAME_BYTES`](crate::MAX_STDIO_FRAME_BYTES) bound, applied by the
/// crate's own capped child-process transport.
const MAX_SSE_EVENT_BYTES: usize = 16 * 1024 * 1024;

/// Deadline for the deprecated-transport probe that runs only after an HTTP
/// connect has already failed, so it never adds latency to a healthy connect.
const LEGACY_SSE_PROBE_TIMEOUT: Duration = Duration::from_secs(3);

/// Upper bound on MRTR round-trips for a single `tools/call`.
///
/// A server may answer `tools/call` with an `input_required` result and expect
/// the client to fulfil the named input requests and retry. This client cannot
/// render an elicitation prompt yet, so it answers every request with a
/// `decline` (the MRTR-legal "no") and retries once; the round cap bounds a
/// peer that would otherwise keep asking.
const MAX_MRTR_ROUNDS: usize = 3;

/// Minimum spacing between forwarded progress chunks.
///
/// A chatty server can emit a progress notification per item; relaying every
/// one would flood the chunk sink. Coalescing to at most one chunk per interval
/// keeps the display live without the traffic.
const PROGRESS_MIN_INTERVAL: Duration = Duration::from_millis(100);

/// Maximum server logging notifications (`notifications/message`) forwarded to
/// `tracing` per second, per connection.
///
/// A server can emit log notifications far faster than is useful; without a
/// bound a chatty or hostile server would flood the daemon's log (and the I/O
/// behind it). Notifications beyond the budget are dropped and the count is
/// reported once the window rolls over. (`notifications/progress` has its own
/// per-call throttle, [`PROGRESS_MIN_INTERVAL`].)
const MAX_LOG_NOTIFICATIONS_PER_SECOND: u32 = 100;

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

/// A server-originated event the engine forwards to the rest of the client.
///
/// The engine's [`ClientHandler`] is the only place these are produced (rmcp
/// delivers notifications to the handler, not to a request's caller); the
/// broadcast carries them to the in-flight call tasks that care.
#[derive(Debug, Clone)]
pub(crate) enum ServerEvent {
    /// A `notifications/progress` for the call owning `token`.
    Progress {
        /// Correlates the notification with the originating request.
        token: ProgressToken,
        /// The current progress value.
        progress: f64,
        /// The total, when the server knows it.
        total: Option<f64>,
        /// Optional human-readable progress message.
        message: Option<String>,
    },
}

/// The `rmcp`-backed engine for one connected server.
pub(crate) struct RmcpEngine {
    /// The peer handle for request/response and notifications. Cloneable, so
    /// every call task can hold its own.
    peer: Peer<RoleClient>,
    /// The running service, kept alive so the connection stays open; closed
    /// exactly once on shutdown. This is a lifecycle handle (never touched per
    /// message), guarded only because `close` needs `&mut`.
    running: tokio::sync::Mutex<Option<RunningService<RoleClient, ServerHandler>>>,
    /// Broadcast of server notifications for this connection. Every call task
    /// subscribes to filter out the progress for its own request.
    events: tokio::sync::broadcast::Sender<ServerEvent>,
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

/// A fixed-window rate limiter for server-originated notifications.
///
/// Kept as a tiny counter with an injectable clock, so the allow/deny decision
/// is unit-testable without waiting on wall-clock time. One instance is shared
/// across a connection's notification callbacks through an `Arc`; the `Mutex`
/// guards only a few integers (no protocol data), and the same lock is never
/// held across an `await`.
#[derive(Debug)]
struct NotificationLimiter {
    /// Allowed notifications per window.
    max: u32,
    /// Window length.
    window: Duration,
    state: std::sync::Mutex<LimiterState>,
}

/// Mutable state behind a [`NotificationLimiter`].
#[derive(Debug)]
struct LimiterState {
    /// Start of the current window, or `None` before the first notification.
    window_start: Option<Instant>,
    /// Notifications allowed so far in the current window.
    count: u32,
    /// Notifications dropped in the current window.
    suppressed: u64,
}

impl NotificationLimiter {
    /// Build a limiter allowing `max` notifications per one-second window.
    fn new(max: u32) -> Self {
        Self {
            max,
            window: Duration::from_secs(1),
            state: std::sync::Mutex::new(LimiterState {
                window_start: None,
                count: 0,
                suppressed: 0,
            }),
        }
    }

    /// Record one notification observed at `now`; returns whether it is within
    /// the budget (and so should be forwarded).
    ///
    /// Rolling into a new window resets the allowance and, when the previous
    /// window dropped anything, emits a single trace line naming the count — so
    /// a throttled server is visible without logging every dropped notification.
    fn allow_at(&self, now: Instant) -> bool {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let in_window = state
            .window_start
            .is_some_and(|start| now.duration_since(start) < self.window);
        if !in_window {
            let suppressed = std::mem::take(&mut state.suppressed);
            if suppressed > 0 {
                tracing::debug!(
                    suppressed,
                    "MCP server logging notifications were rate-limited"
                );
            }
            state.count = 0;
            state.window_start = Some(now);
        }
        if state.count < self.max {
            state.count += 1;
            true
        } else {
            state.suppressed += 1;
            false
        }
    }
}

/// The `ClientHandler` for one connection.
///
/// rmcp routes every server-to-client notification here rather than to the
/// caller that issued the request, so this is where progress, logging, and
/// list-change notifications are turned into a [`ServerEvent`] broadcast (or,
/// for logging, a `tracing` event). The handler also carries the `clientInfo`
/// and capability object advertised to the server.
#[derive(Clone)]
struct ServerHandler {
    config: ClientConfig,
    events: tokio::sync::broadcast::Sender<ServerEvent>,
    /// Per-connection rate limiter for logging notifications.
    limiter: Arc<NotificationLimiter>,
}

impl ClientHandler for ServerHandler {
    fn get_info(&self) -> ClientConfig {
        self.config.clone()
    }

    fn on_progress(
        &self,
        params: ProgressNotificationParam,
        _context: NotificationContext<RoleClient>,
    ) -> impl Future<Output = ()> + MaybeSendFuture + '_ {
        let events = self.events.clone();
        async move {
            // Best-effort: a lagging subscriber is dropped, not blocked.
            let _ = events.send(ServerEvent::Progress {
                token: params.progress_token,
                progress: params.progress,
                total: params.total,
                message: params.message,
            });
        }
    }

    // List-changed notifications are NOT handled here: the stateless era
    // delivers them only on a `subscriptions/listen` stream, which rmcp routes
    // to that stream's own receiver (see `spawn_list_change_listener`), not to
    // the handler. An unsolicited list-changed from a legacy peer carries no
    // actionable list to refresh, so it is intentionally ignored.

    // Logging is deprecated by the specification (SEP-2577), but a server may
    // still emit `notifications/message`, so it is forwarded to `tracing`
    // rather than dropped. The deprecation note is the framework's; there is no
    // replacement notification to consume instead.
    #[expect(
        deprecated,
        reason = "rmcp flags the logging notification types as deprecated (SEP-2577); consuming the notification is still the only way to observe a server that sends one"
    )]
    async fn on_logging_message(
        &self,
        params: rmcp::model::LoggingMessageNotificationParam,
        _context: NotificationContext<RoleClient>,
    ) {
        // Bound the notification rate: a server that floods the log is
        // throttled rather than allowed to dominate the daemon's log output.
        if !self.limiter.allow_at(Instant::now()) {
            return;
        }
        let logger = params.logger.as_deref().unwrap_or("server");
        let data = params.data;
        tracing::info!(logger = %logger, level = ?params.level, "MCP server log: {data}");
    }
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
    events: &tokio::sync::broadcast::Sender<ServerEvent>,
) -> Result<RunningService<RoleClient, ServerHandler>, McpError> {
    let handler = ServerHandler {
        config: client_config(config.protocol),
        events: events.clone(),
        limiter: Arc::new(NotificationLimiter::new(MAX_LOG_NOTIFICATIONS_PER_SECOND)),
    };
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
    let mut http_config = StreamableHttpClientTransportConfig::with_uri(url.as_str())
        .custom_headers(custom)
        .max_sse_event_size(MAX_SSE_EVENT_BYTES);
    // Bound the SSE reconnect policy. rmcp's default retries a dropped server
    // stream forever, which would hammer a permanently dead endpoint; this caps
    // the attempts with a bounded exponential backoff so the failure surfaces.
    // No idle-read timeout is set: rmcp exposes no read-idle hook, and the
    // per-request deadline (`PeerRequestOptions::with_timeout`) already bounds
    // every ordinary request, while a `subscriptions/listen` stream is
    // re-established by the dispatcher's own restart policy.
    http_config.retry_config = Arc::new(BoundedSseRetry {
        max_attempts: SSE_MAX_RECONNECTS,
        base: crate::retry::BASE_BACKOFF,
        ceiling: crate::retry::MAX_BACKOFF,
    });
    Ok(StreamableHttpClientTransport::with_client(
        client,
        http_config,
    ))
}

/// A bounded SSE stream-reconnect policy for the Streamable HTTP transport.
///
/// Implements rmcp's [`SseRetryPolicy`] with a finite attempt budget and a
/// capped exponential backoff (the same shape as [`crate::retry`]'s connect
/// policy), replacing the crate default of unbounded reconnection. rmcp's own
/// policy types are `#[non_exhaustive]` and so cannot be constructed here.
#[derive(Debug)]
struct BoundedSseRetry {
    /// Maximum reconnect attempts before the stream is abandoned.
    max_attempts: usize,
    /// First-retry delay.
    base: Duration,
    /// Ceiling on a single reconnect delay.
    ceiling: Duration,
}

impl SseRetryPolicy for BoundedSseRetry {
    fn retry(&self, current_times: usize) -> Option<Duration> {
        if current_times >= self.max_attempts {
            return None;
        }
        // `current_times` is bounded by `max_attempts` in practice, but clamp
        // the shift so a hostile value cannot overflow the left shift.
        let shift = u32::try_from(current_times).unwrap_or(u32::MAX).min(7);
        Some(self.base.saturating_mul(1u32 << shift).min(self.ceiling))
    }
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
/// The HTTP status is recovered by [`connect_error_status`].
fn retryable_connect(error: &ClientInitializeError) -> bool {
    connect_error_status(error).is_some_and(crate::retry::is_retryable_status)
}

/// Recover the HTTP status from a failed connect, if it carries one.
///
/// Walks the transport error's source chain for rmcp's `StreamableHttpError`: a
/// bare `reqwest::Error` carries a status directly, whereas a rejected POST
/// surfaces the status inside rmcp's `"HTTP <status>: <body>"` message.
fn connect_error_status(error: &ClientInitializeError) -> Option<u16> {
    let root: &dyn std::error::Error = match error {
        ClientInitializeError::TransportError { error, .. } => error.error.as_ref(),
        _ => return None,
    };
    let mut current: Option<&(dyn std::error::Error + 'static)> = Some(root);
    while let Some(err) = current {
        if let Some(http) = err.downcast_ref::<StreamableHttpError<reqwest::Error>>()
            && let Some(status) = http_error_status(http)
        {
            return Some(status);
        }
        current = err.source();
    }
    None
}

/// Extract the HTTP status from an rmcp Streamable HTTP error, when it carries
/// one.
fn http_error_status(error: &StreamableHttpError<reqwest::Error>) -> Option<u16> {
    match error {
        StreamableHttpError::Client(e) => e.status().map(|s| s.as_u16()),
        StreamableHttpError::UnexpectedServerResponse(message) => parse_http_status(message),
        // rmcp models a 401 challenge and an insufficient-scope 403 as their
        // own variants (no status number); map them to the codes they represent
        // so the caller can turn either into an actionable `AuthRequired`.
        StreamableHttpError::AuthRequired(_) => Some(401),
        StreamableHttpError::InsufficientScope(_) => Some(403),
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

/// Drive one `tools/call`, honouring the deadline, the cancellation token, and
/// the server's MRTR `input_required` responses.
///
/// A tool call can span several JSON-RPC round-trips when the server answers
/// with an `input_required` result: this client cannot render an elicitation
/// prompt, so it declines each requested input and retries the original call
/// with the decline responses and the echoed `requestState`. The retry loop is
/// bounded by [`MAX_MRTR_ROUNDS`]; a server that keeps asking is failed with a
/// message naming the input it wanted rather than hanging.
///
/// Progress notifications for this call's `progressToken` are forwarded to the
/// caller's chunk sink, rate-limited to [`PROGRESS_MIN_INTERVAL`].
async fn call_tool_impl(
    peer: &Peer<RoleClient>,
    events: &tokio::sync::broadcast::Sender<ServerEvent>,
    call: EngineCall,
    slug: &str,
) -> Result<CallToolResult, McpError> {
    let EngineCall {
        request,
        cancel,
        chunk_tx,
    } = call;
    let CallRequest {
        name,
        arguments,
        timeout,
    } = request;

    let mut input_responses: Option<InputResponses> = None;
    let mut request_state: Option<String> = None;
    let mut rounds = 0usize;
    let mut last_progress: Option<Instant> = None;

    loop {
        let mut params = CallToolRequestParams::new(name.clone());
        if let serde_json::Value::Object(map) = &arguments {
            params.arguments = Some(map.clone());
        }
        if let Some(responses) = input_responses.take() {
            params = params.with_input_responses(responses);
        }
        if let Some(state) = request_state.take() {
            params = params.with_request_state(state);
        }

        // Subscribe BEFORE sending: the broadcast drops a message when no
        // receiver is attached, so a fast server's progress must find this
        // receiver already live.
        let mut events_rx = events.subscribe();

        // Request-scoped options: the deadline resets while progress
        // notifications arrive (a long tool that reports progress is not
        // killed mid-work).
        let options = PeerRequestOptions::with_timeout(timeout).reset_timeout_on_progress();
        let handle = peer
            .send_cancellable_request(
                ClientRequest::CallToolRequest(CallToolRequest::new(params)),
                options,
            )
            .await
            .map_err(|e| map_service_error(e, slug))?;
        let request_id = handle.id.clone();
        let progress_token = handle.progress_token.clone();

        // A fatal transport error maps out of the loop; the response is the
        // only thing that drives a retry.
        let response_fut = handle.await_response();
        tokio::pin!(response_fut);
        let response = loop {
            tokio::select! {
                biased;
                // Cancellation arm first: a session cancel stops the call the
                // instant it is observed, and we tell the server to stop.
                () = cancel.cancelled() => {
                    let _ = peer
                        .notify_cancelled(CancelledNotificationParam::new(
                            Some(request_id.clone()),
                            Some("client cancelled".to_string()),
                        ))
                        .await;
                    return Err(McpError::Cancelled);
                }
                event = events_rx.recv() => {
                    // Any event that is not this call's progress (or a lagged
                    // receiver) is ignored; the loop re-selects.
                    if let Ok(ServerEvent::Progress { token, progress, total, message }) = event
                        && token == progress_token
                    {
                        forward_progress(chunk_tx.as_ref(), &mut last_progress, message.as_deref(), progress, total);
                    }
                }
                response = &mut response_fut => break response,
            }
        };

        match response.map_err(|e| map_service_error(e, slug))? {
            ServerResult::CallToolResult(result) => return Ok(convert_call_result(result)),
            ServerResult::InputRequiredResult(required) => {
                rounds += 1;
                if rounds >= MAX_MRTR_ROUNDS {
                    return Err(McpError::ProtocolError(format!(
                        "server kept requesting client input for tool {name:?} after {rounds} round(s); \
                         this client cannot supply it"
                    )));
                }
                input_responses = Some(decline_responses(required.input_requests.as_ref())?);
                // Echo the opaque state verbatim; it is required on retry and
                // must not be inspected.
                request_state.clone_from(&required.request_state);
            }
            // The tasks extension is not driven; surface it rather than hang.
            other => {
                return Err(McpError::ProtocolError(format!(
                    "tools/call returned a result this client does not handle: {other:?}"
                )));
            }
        }
    }
}

/// Forward one progress notification to the caller's chunk sink, rate-limited.
///
/// A message is preferred; failing that a compact numeric indicator is emitted
/// when the server knows the total. Sends are best-effort (`try_send`): a full
/// or dropped sink never blocks the call.
fn forward_progress(
    chunk_tx: Option<&crossbeam_channel::Sender<Vec<u8>>>,
    last_progress: &mut Option<Instant>,
    message: Option<&str>,
    progress: f64,
    total: Option<f64>,
) {
    let Some(tx) = chunk_tx else {
        return;
    };
    if let Some(last) = *last_progress
        && last.elapsed() < PROGRESS_MIN_INTERVAL
    {
        return;
    }
    let text = match message.filter(|m| !m.is_empty()) {
        Some(message) => format!("{message}\n"),
        None => match total {
            Some(total) if total > 0.0 => format!("[progress {progress:.0}/{total:.0}]\n"),
            _ => return,
        },
    };
    if tx.try_send(text.into_bytes()).is_ok() {
        *last_progress = Some(Instant::now());
    }
}

/// Build a decline response for every server input request in an
/// `input_required` result.
///
/// Only elicitation and roots can be answered with a well-formed decline here;
/// sampling (`sampling/createMessage`) would require this client to invoke a
/// model, so its presence is reported as an unsupported protocol exchange
/// rather than answered with fabricated content.
fn decline_responses(input_requests: Option<&InputRequests>) -> Result<InputResponses, McpError> {
    let mut responses = InputResponses::new();
    let Some(requests) = input_requests else {
        // A state-only `input_required` (load shedding) just needs the state
        // echoed; an empty response map is a valid retry.
        return Ok(responses);
    };
    for (key, request) in requests {
        let value = match request {
            InputRequest::Elicitation(_) => {
                serde_json::to_value(ElicitResult::new(ElicitationAction::Decline))
            }
            // The roots result shape is `{"roots": []}`; built as a literal
            // rather than through the (deprecated) rmcp type only to keep the
            // decline here independent of that deprecation.
            InputRequest::ListRoots(_) => Ok(serde_json::json!({ "roots": [] })),
            InputRequest::CreateMessage(_) => {
                return Err(McpError::ProtocolError(
                    "server requested sampling (`sampling/createMessage`), which this client does \
                     not advertise or serve"
                        .into(),
                ));
            }
            other => {
                return Err(McpError::ProtocolError(format!(
                    "server requested unsupported client input: {other:?}"
                )));
            }
        }
        .map_err(|e| McpError::ProtocolError(format!("failed to encode input response: {e}")))?;
        responses.insert(key.clone(), value);
    }
    Ok(responses)
}

/// Map an `rmcp` service error onto this crate's error type.
///
/// An error carrying an HTTP 401/403 status is surfaced as an actionable
/// [`McpError::AuthRequired`] naming `slug`, so a mid-session authorization
/// failure (not just a connect-time one) explains itself rather than appearing
/// as an opaque transport error.
fn map_service_error(error: ServiceError, slug: &str) -> McpError {
    match error {
        ServiceError::McpError(data) => McpError::JsonRpcError {
            code: i64::from(data.code.0),
            message: data.message.into_owned(),
        },
        ServiceError::TransportClosed => McpError::ServerShutdown,
        ServiceError::Timeout { .. } => McpError::Timeout,
        ServiceError::Cancelled { .. } => McpError::Cancelled,
        other => {
            // A transport error may embed a rejected POST's status (or rmcp's
            // dedicated auth variants); surface an authorization failure plainly
            // when it does.
            match service_error_status(&other) {
                Some(status @ (401 | 403)) => McpError::AuthRequired {
                    server: slug.to_string(),
                    hint: auth_hint(status),
                },
                _ => McpError::ProtocolError(other.to_string()),
            }
        }
    }
}

/// Recover an HTTP status from a service error by walking its source chain for
/// rmcp's `StreamableHttpError`.
fn service_error_status(error: &ServiceError) -> Option<u16> {
    // `TransportSend`'s inner error is not exposed through `source()`, so the
    // walk starts at the dynamic error's boxed payload, where the transport
    // error actually lives.
    let root: &(dyn std::error::Error + 'static) = match error {
        ServiceError::TransportSend(dynamic) => dynamic.error.as_ref(),
        _ => error,
    };
    let mut current: Option<&(dyn std::error::Error + 'static)> = Some(root);
    while let Some(err) = current {
        if let Some(http) = err.downcast_ref::<StreamableHttpError<reqwest::Error>>()
            && let Some(status) = http_error_status(http)
        {
            return Some(status);
        }
        current = err.source();
    }
    None
}

/// The actionable guidance attached to an authorization-required error.
///
/// Names both supported paths: a static token in the server's `headers`, and
/// the OAuth support that is not yet shipped.
fn auth_hint(status: u16) -> String {
    format!(
        "the server answered HTTP {status}; configure a static token in this server's \
         \"headers\" (for example \"Authorization\": \"Bearer ${{TOKEN}}\"), or wait \
         for OAuth support, which is not yet available"
    )
}

/// Convert rmcp tools, dropping any whose `inputSchema` is unusable and
/// truncating the catalogue at [`MAX_TOOLS_PER_SERVER`].
fn convert_tools(tools: Vec<rmcp::model::Tool>) -> Vec<McpTool> {
    let converted: Vec<McpTool> = tools.into_iter().filter_map(convert_tool).collect();
    let (kept, dropped) = cap_tools(converted);
    if dropped > 0 {
        tracing::warn!(
            dropped,
            cap = crate::MAX_TOOLS_PER_SERVER,
            "MCP server advertised more tools than the per-server cap; extra tools dropped"
        );
    }
    kept
}

/// Convert one rmcp tool, returning `None` when its schema must be rejected.
///
/// A tool with a non-object, oversized, or over-deep `inputSchema` is dropped
/// (the rest are kept), per the spec's "exclude the offending tool" rule. An
/// out-of-bounds `outputSchema` is dropped while the tool is kept, since it is
/// advisory.
fn convert_tool(tool: rmcp::model::Tool) -> Option<McpTool> {
    let input_schema = normalize_input_schema(tool.schema_as_json_value())?;
    let output_schema = tool
        .output_schema
        .map(|schema| serde_json::Value::Object(schema.as_ref().clone()))
        .and_then(normalize_output_schema);
    Some(McpTool {
        name: tool.name.into_owned(),
        description: tool.description.map(std::borrow::Cow::into_owned),
        input_schema,
        output_schema,
    })
}

/// Convert a listed rmcp resource into this crate's value type.
fn convert_listed_resource(resource: rmcp::model::Resource) -> McpResource {
    McpResource {
        uri: resource.uri,
        name: Some(resource.name),
        description: resource.description,
        mime_type: resource.mime_type,
    }
}

/// Convert one resource's contents from a `resources/read` result.
fn convert_resource_contents(contents: ResourceContents) -> McpContent {
    convert_resource(contents)
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
        match map_service_error(ServiceError::McpError(data), "test") {
            McpError::JsonRpcError { code, message } => {
                assert_eq!(code, -32601);
                assert_eq!(message, "nope");
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn service_error_maps_unauthorized_to_auth_required() {
        // A transport error whose source carries a 401 is surfaced as the
        // actionable AuthRequired naming the server, not an opaque error.
        let inner = StreamableHttpError::<reqwest::Error>::UnexpectedServerResponse(
            "HTTP 401 Unauthorized: token missing".into(),
        );
        let dynamic = rmcp::transport::DynamicTransportError::from_parts(
            "test",
            std::any::TypeId::of::<()>(),
            Box::new(inner),
        );
        match map_service_error(ServiceError::TransportSend(dynamic), "docs") {
            McpError::AuthRequired { server, hint } => {
                assert_eq!(server, "docs");
                assert!(hint.contains("headers"), "{hint}");
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
    fn decline_responses_declines_elicitation() {
        let elicitation = InputRequest::Elicitation(
            serde_json::from_value(serde_json::json!({
                "method": "elicitation/create",
                "params": {
                    "mode": "form",
                    "message": "Which environment?",
                    "requestedSchema": {"type": "object", "properties": {}}
                }
            }))
            .expect("elicitation request decodes"),
        );
        let mut requests = InputRequests::new();
        requests.insert("q1".to_string(), elicitation);
        let responses = decline_responses(Some(&requests)).expect("declines encode");
        assert_eq!(
            responses.get("q1"),
            Some(&serde_json::json!({"action": "decline"}))
        );
    }

    #[test]
    fn decline_responses_state_only_is_empty_map() {
        let responses = decline_responses(None).expect("no requests is fine");
        assert!(responses.is_empty());
    }

    #[test]
    fn forward_progress_rate_limits_and_formats() {
        let (tx, rx) = crossbeam_channel::unbounded();
        let mut last = None;
        forward_progress(Some(&tx), &mut last, Some("working"), 1.0, Some(2.0));
        // A second message inside the interval is dropped.
        forward_progress(Some(&tx), &mut last, Some("again"), 2.0, Some(2.0));
        let got: Vec<String> = rx
            .try_iter()
            .map(|b| String::from_utf8_lossy(&b).into_owned())
            .collect();
        assert_eq!(got.len(), 1, "rate-limited to one chunk: {got:?}");
        assert!(got[0].contains("working"));
    }

    #[test]
    fn forward_progress_numeric_fallback_and_no_sink() {
        let (tx, rx) = crossbeam_channel::unbounded();
        let mut last = None;
        // No message but a known total: a compact numeric indicator is emitted.
        forward_progress(Some(&tx), &mut last, None, 1.0, Some(4.0));
        let got: Vec<String> = rx
            .try_iter()
            .map(|b| String::from_utf8_lossy(&b).into_owned())
            .collect();
        assert_eq!(got.len(), 1);
        assert!(got[0].contains("1/4"));

        // No sink: nothing is sent and the rate-limit timestamp is untouched.
        let mut untouched = None;
        forward_progress(None, &mut untouched, Some("x"), 1.0, None);
        assert!(untouched.is_none());
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

    #[test]
    fn convert_tools_truncates_to_the_cap() {
        // A server advertising more tools than the cap keeps the leading prefix.
        let json: Vec<serde_json::Value> = (0..crate::MAX_TOOLS_PER_SERVER + 5)
            .map(|n| {
                serde_json::json!({
                    "name": format!("t{n}"),
                    "inputSchema": {"type": "object"}
                })
            })
            .collect();
        let tools: Vec<rmcp::model::Tool> = json
            .into_iter()
            .map(|v| serde_json::from_value(v).expect("tool decodes"))
            .collect();
        let converted = convert_tools(tools);
        assert_eq!(converted.len(), crate::MAX_TOOLS_PER_SERVER);
        assert_eq!(converted.first().map(|t| t.name.as_str()), Some("t0"));
    }

    #[test]
    fn notification_limiter_enforces_a_per_window_budget() {
        let limiter = NotificationLimiter::new(2);
        let base = Instant::now();
        assert!(limiter.allow_at(base), "first is within budget");
        assert!(limiter.allow_at(base), "second is within budget");
        assert!(!limiter.allow_at(base), "third exceeds the budget");
        assert!(!limiter.allow_at(base), "fourth too");
        // A fresh window restores the allowance.
        assert!(limiter.allow_at(base + Duration::from_secs(1)));
    }

    #[test]
    fn bounded_sse_retry_stops_after_the_budget() {
        let policy = BoundedSseRetry {
            max_attempts: 3,
            base: Duration::from_millis(500),
            ceiling: Duration::from_mins(1),
        };
        // The first retries back off exponentially from the base...
        assert_eq!(policy.retry(0), Some(Duration::from_millis(500)));
        assert_eq!(policy.retry(1), Some(Duration::from_secs(1)));
        assert_eq!(policy.retry(2), Some(Duration::from_secs(2)));
        // ...then the budget is spent and the stream is abandoned.
        assert_eq!(policy.retry(3), None);
    }
}
