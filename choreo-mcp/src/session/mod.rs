//! Per-server dispatcher thread and the blocking client facade over it.
//!
//! One dispatcher (a plain OS thread) owns a single engine — the live
//! connection to one server — and serves every caller through a command
//! channel. Calls no longer serialize behind a per-server `Mutex`: the
//! dispatcher spawns each call onto the sidecar runtime, so many calls to one
//! server run concurrently while the dispatcher keeps draining commands
//! (cancellation included). Because the protocol is stateless, a crashed
//! connection is simply rebuilt and the next request re-issued.
//!
//! Cross-thread control flows over crossbeam channels:
//!
//! - callers → dispatcher: a command (list / call / cancel / shutdown);
//! - dispatcher → callers: a per-request reply channel;
//! - call tasks → dispatcher: a completion notice (used to drop finished calls
//!   from the in-flight registry and to trigger a restart on a dead transport —
//!   a whole batch of calls lost on one transport is coalesced into a single
//!   rebuild).
//!
//! The only shared-mutable state in the crate is the in-flight registry, and it
//! is owned solely by the dispatcher thread (no lock); the per-call
//! cancellation token is the sanctioned "cooperative flag" used to un-block an
//! in-flight request.
//!
//! The tree is split by concern: this module owns the blocking facade
//! ([`McpServer`] / [`McpServerHandle`]), the channel protocol (`McpCommand`,
//! `CallRequest`, `EngineCall`), and the `McpEngine` backend trait; `dispatch`
//! the dispatcher loop and its call-slot accounting; `gate` the concurrent-call
//! admission arithmetic; `restart` the bounded transport-reconnect policy;
//! `cancel` the cooperative cancellation token; and `util` the bounded
//! thread-join helper.

mod cancel;
mod dispatch;
mod gate;
mod restart;
mod util;

#[cfg(test)]
mod tests;

#[cfg(test)]
use self::dispatch::DispatcherStats;
use self::dispatch::run_dispatcher;
use self::util::join_bounded;

use crate::config::McpServerConfig;
use crate::error::McpError;
use crate::protocol::{CallToolResult, McpContent, McpListChange, McpResource, McpTool};
use crossbeam_channel::Sender;
use std::sync::Arc;
use std::time::Duration;

/// A boxed, `Send` future — the engine trait's async-method return type.
///
/// Borrows the engine for `'a`, so a method call can capture the `Arc`-shared
/// engine without cloning; the returned future is driven within that borrow
/// (the dispatcher blocks on listings, and a spawned call task owns its engine
/// `Arc` for the whole `await`).
pub(crate) type BoxFuture<'a, T> =
    std::pin::Pin<Box<dyn std::future::Future<Output = T> + Send + 'a>>;

/// The blocking connection object returned by [`McpServer::connect`].
///
/// Owns the dispatcher thread's join handle so shutdown can join it with a
/// bounded wait; draw a cloneable [`McpServerHandle`] from it for the tool
/// wrappers. Dropping the last `McpServer` shuts the connection down.
pub struct McpServer {
    handle: McpServerHandle,
    join: Option<std::thread::JoinHandle<()>>,
}

impl McpServer {
    /// Connect to the configured server and start its dispatcher thread.
    ///
    /// A convenience wrapper over [`connect_with_list_changes`] for callers
    /// that do not consume the server's list-changed events (tests, and any
    /// embedder that only needs one-shot tool discovery).
    ///
    /// # Errors
    ///
    /// As [`connect_with_list_changes`].
    ///
    /// [`connect_with_list_changes`]: Self::connect_with_list_changes
    pub fn connect(config: &McpServerConfig) -> Result<Self, McpError> {
        Self::connect_with_list_changes(config, None)
    }

    /// Connect to the configured server and start its dispatcher thread,
    /// forwarding the server's list-changed events to `list_changes`.
    ///
    /// Performs the lifecycle handshake (legacy `initialize` or the stateless
    /// `server/discover` probe, per the config's
    /// [`McpProtocolMode`](crate::McpProtocolMode)) before returning, so a
    /// best-effort caller can fail fast; a caller unwilling to wait (the
    /// daemon's bounded startup) runs this on its own thread and detaches on
    /// timeout.
    ///
    /// When `list_changes` is `Some`, and the negotiated era is stateless and
    /// the server advertises a list-changed capability, the connection opens a
    /// `subscriptions/listen` stream on the sidecar runtime and forwards each
    /// event as an [`McpListChange`]. The same sender is reused across
    /// reconnects, so a rebuilt transport re-establishes its subscription.
    ///
    /// # Errors
    ///
    /// Returns [`McpError::SpawnFailed`] when the subprocess cannot be spawned,
    /// [`McpError::InitializeFailed`] when the handshake fails,
    /// [`McpError::ProtocolError`] when the runtime is not initialized or the
    /// dispatcher thread cannot start, and transport/protocol errors surfaced
    /// by the handshake.
    pub fn connect_with_list_changes(
        config: &McpServerConfig,
        list_changes: Option<crossbeam_channel::Sender<McpListChange>>,
    ) -> Result<Self, McpError> {
        crate::runtime::init()?;
        let engine = crate::engine::connect(config, list_changes.clone())?;
        let name = engine.name().to_string();
        let version = engine.version().to_string();

        let (cmd_tx, cmd_rx) = crossbeam_channel::unbounded();
        let handle = McpServerHandle {
            cmd_tx,
            name: name.clone().into(),
            version: version.clone().into(),
            default_timeout: config.request_timeout(),
            resources: engine.supports_resources(),
        };

        let factory = crate::engine::factory(config.clone(), list_changes);
        let max_concurrent_calls = config.max_concurrent_calls();
        let max_restarts = config.max_restarts();
        let join = std::thread::Builder::new()
            .name(format!("mcp-{}", config.slug))
            .spawn(move || {
                run_dispatcher(engine, factory, &cmd_rx, max_concurrent_calls, max_restarts);
            })
            .map_err(|e| McpError::SpawnFailed(format!("failed to start dispatcher: {e}")))?;

        tracing::info!(server = %config.slug, %name, %version, "MCP server connected");
        Ok(Self {
            handle,
            join: Some(join),
        })
    }

    /// A cloneable command handle for invoking this server's tools.
    #[must_use]
    pub fn handle(&self) -> McpServerHandle {
        self.handle.clone()
    }

    /// The server's advertised implementation name.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.handle.name
    }

    /// The server's advertised implementation version.
    #[must_use]
    pub fn version(&self) -> &str {
        &self.handle.version
    }
}

impl Drop for McpServer {
    fn drop(&mut self) {
        // Best-effort: tell the dispatcher to close the connection and exit,
        // then join it with a bounded wait so a wedged server cannot hold up
        // daemon shutdown.
        let _ = self.handle.cmd_tx.send(McpCommand::Shutdown);
        if let Some(join) = self.join.take() {
            join_bounded(join, Duration::from_secs(5));
        }
    }
}

/// A cloneable, blocking command handle to one server's dispatcher.
///
/// Cheap to clone (a channel sender plus immutable metadata) and safe to share
/// across the daemon's tool wrappers: every method is a blocking round-trip to
/// the dispatcher thread, so no `rmcp` type (and no async runtime) leaks out of
/// this crate.
#[derive(Clone)]
pub struct McpServerHandle {
    cmd_tx: Sender<McpCommand>,
    name: Arc<str>,
    version: Arc<str>,
    default_timeout: Duration,
    resources: bool,
}

impl McpServerHandle {
    /// List every tool the server advertises, following pagination cursors.
    ///
    /// # Errors
    ///
    /// Returns [`McpError::NotConnected`] when the dispatcher has exited, and
    /// the handshake/transport/protocol errors surfaced by the engine.
    pub fn list_tools(&self) -> Result<Vec<McpTool>, McpError> {
        self.list_tools_inner(None)
    }

    /// Like [`list_tools`](Self::list_tools), but bounding the listing by
    /// `timeout` instead of the server's configured request timeout.
    ///
    /// Used by the daemon's catalogue-refresh sweeps, where one slow server
    /// must not stall the command loop for the full per-server timeout. The
    /// deadline is applied INSIDE the dispatcher (via the engine's listing
    /// timeout), so a server that never answers `tools/list` is cancelled
    /// rather than left busy.
    ///
    /// # Errors
    ///
    /// As [`list_tools`](Self::list_tools), plus [`McpError::Timeout`] when the
    /// listing outlives `timeout`.
    pub fn list_tools_with_deadline(&self, timeout: Duration) -> Result<Vec<McpTool>, McpError> {
        self.list_tools_inner(Some(timeout))
    }

    fn list_tools_inner(&self, timeout: Option<Duration>) -> Result<Vec<McpTool>, McpError> {
        let (tx, rx) = crossbeam_channel::bounded(1);
        self.cmd_tx
            .send(McpCommand::ListTools { timeout, reply: tx })
            .map_err(|_| McpError::NotConnected)?;
        rx.recv().map_err(|_| McpError::NotConnected)?
    }

    /// Call a tool, blocking until it returns, the deadline passes, or the
    /// session is cancelled.
    ///
    /// `session_id` tags the call so a later
    /// [`cancel_session`](Self::cancel_session) can stop exactly the calls a
    /// cancelled session started. `timeout` overrides the server's configured
    /// default for this call.
    ///
    /// # Errors
    ///
    /// Returns [`McpError::NotConnected`] when the dispatcher has exited,
    /// [`McpError::Cancelled`] when the call's session was cancelled,
    /// [`McpError::Timeout`] on deadline, and the engine's transport/protocol
    /// errors otherwise. A server-flagged tool error is *not* an `Err`: it is
    /// returned as [`CallToolResult::is_error`].
    pub fn call_tool(
        &self,
        session_id: u64,
        name: &str,
        arguments: serde_json::Value,
        timeout: Option<Duration>,
    ) -> Result<CallToolResult, McpError> {
        self.call_tool_streaming(session_id, name, arguments, timeout, None)
    }

    /// Like [`call_tool`](Self::call_tool), but forwards the server's progress
    /// notifications to `chunk_tx` as rate-limited `ToolResultChunk` bytes.
    ///
    /// Used by the daemon's streaming tool path so a long-running MCP tool's
    /// progress is visible live rather than only at completion. Progress is
    /// best-effort: a full sink drops the chunk rather than blocking the call.
    ///
    /// # Errors
    ///
    /// As [`call_tool`](Self::call_tool).
    pub fn call_tool_streaming(
        &self,
        session_id: u64,
        name: &str,
        arguments: serde_json::Value,
        timeout: Option<Duration>,
        chunk_tx: Option<crossbeam_channel::Sender<Vec<u8>>>,
    ) -> Result<CallToolResult, McpError> {
        let (tx, rx) = crossbeam_channel::bounded(1);
        let request = CallRequest {
            name: name.to_string(),
            arguments,
            timeout: timeout.unwrap_or(self.default_timeout),
        };
        self.cmd_tx
            .send(McpCommand::Call {
                session_id,
                request,
                reply: tx,
                chunk_tx,
            })
            .map_err(|_| McpError::NotConnected)?;
        rx.recv().map_err(|_| McpError::NotConnected)?
    }

    /// List every resource the server advertises, following pagination cursors.
    ///
    /// # Errors
    ///
    /// Returns [`McpError::NotConnected`] when the dispatcher has exited, and
    /// the transport/protocol errors surfaced by the engine.
    pub fn list_resources(&self) -> Result<Vec<McpResource>, McpError> {
        let (tx, rx) = crossbeam_channel::bounded(1);
        self.cmd_tx
            .send(McpCommand::ListResources(tx))
            .map_err(|_| McpError::NotConnected)?;
        rx.recv().map_err(|_| McpError::NotConnected)?
    }

    /// Read one resource's contents by URI.
    ///
    /// # Errors
    ///
    /// Returns [`McpError::NotConnected`] when the dispatcher has exited, and
    /// the transport/protocol errors surfaced by the engine.
    pub fn read_resource(&self, uri: &str) -> Result<Vec<McpContent>, McpError> {
        let (tx, rx) = crossbeam_channel::bounded(1);
        self.cmd_tx
            .send(McpCommand::ReadResource {
                uri: uri.to_string(),
                reply: tx,
            })
            .map_err(|_| McpError::NotConnected)?;
        rx.recv().map_err(|_| McpError::NotConnected)?
    }

    /// Whether the server declared the `resources` capability.
    #[must_use]
    pub fn supports_resources(&self) -> bool {
        self.resources
    }

    /// Cancel every in-flight call started by `session_id`.
    ///
    /// Best-effort (the dispatcher may already have finished them); never
    /// blocks and never fails.
    pub fn cancel_session(&self, session_id: u64) {
        let _ = self.cmd_tx.send(McpCommand::CancelSession { session_id });
    }

    /// The server's advertised implementation name.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The server's advertised implementation version.
    #[must_use]
    pub fn version(&self) -> &str {
        &self.version
    }

    /// Build a handle with no dispatcher behind it, for callers that only
    /// exercise the handle's metadata (`name`, `version`).
    ///
    /// Any command sent through it fails with [`McpError::NotConnected`].
    #[doc(hidden)]
    #[must_use]
    pub fn disconnected(name: &str, version: &str, default_timeout: Duration) -> Self {
        let (cmd_tx, _rx) = crossbeam_channel::unbounded();
        Self {
            cmd_tx,
            name: name.into(),
            version: version.into(),
            default_timeout,
            resources: false,
        }
    }

    /// Like [`disconnected`](Self::disconnected), but reporting the `resources`
    /// capability, so a caller can exercise the resource-catalogue wrapping
    /// (names, reservation) without a live connection.
    ///
    /// Any command sent through it fails with [`McpError::NotConnected`].
    #[doc(hidden)]
    #[must_use]
    pub fn disconnected_with_resources(name: &str) -> Self {
        Self {
            resources: true,
            ..Self::disconnected(name, "0.0.0", Duration::from_secs(5))
        }
    }
}

/// A single tool invocation handed to the dispatcher.
pub(crate) struct CallRequest {
    pub(crate) name: String,
    pub(crate) arguments: serde_json::Value,
    pub(crate) timeout: Duration,
}

/// A tool invocation paired with its cancellation token and progress sink, as
/// seen by an [`McpEngine`].
///
/// `chunk_tx` is the daemon's streaming-output channel when the caller wants
/// live progress; the engine forwards rate-limited progress messages to it as
/// `notifications/progress` arrive. `None` means the caller is not streaming.
pub(crate) struct EngineCall {
    pub(crate) request: CallRequest,
    pub(crate) cancel: cancel::CancelToken,
    pub(crate) chunk_tx: Option<crossbeam_channel::Sender<Vec<u8>>>,
}

/// The backend that executes MCP operations for one server.
///
/// The production implementation is the `rmcp`-backed engine; tests supply a
/// mock so the dispatcher's command routing, cancellation, and restart logic
/// can be exercised without a subprocess or the network. Methods take `&self`
/// so the dispatcher can `Arc`-share one engine across concurrently spawned
/// call tasks.
pub(crate) trait McpEngine: Send + Sync + 'static {
    /// List every advertised tool (engine follows pagination, bounded by
    /// `timeout` when supplied, else the engine's own configured timeout).
    fn list_tools(
        &self,
        timeout: Option<Duration>,
    ) -> BoxFuture<'_, Result<Vec<McpTool>, McpError>>;

    /// Call one tool, honouring the call's deadline and cancellation token.
    fn call_tool(&self, call: EngineCall) -> BoxFuture<'_, Result<CallToolResult, McpError>>;

    /// List every advertised resource, following pagination cursors.
    fn list_resources(&self) -> BoxFuture<'_, Result<Vec<McpResource>, McpError>>;

    /// Read one resource's contents by URI.
    fn read_resource(&self, uri: String) -> BoxFuture<'_, Result<Vec<McpContent>, McpError>>;

    /// Whether the server declared the `resources` capability.
    fn supports_resources(&self) -> bool;

    /// Close the connection. Called once on shutdown.
    fn shutdown(&self) -> BoxFuture<'_, ()>;

    /// The server's advertised implementation name.
    fn name(&self) -> &str;

    /// The server's advertised implementation version.
    fn version(&self) -> &str;
}

/// A factory that builds a fresh engine (reconnect) for the restart policy.
pub(crate) type EngineFactory = Box<dyn Fn() -> Result<Arc<dyn McpEngine>, McpError> + Send + Sync>;

/// Commands sent to a server's dispatcher thread.
pub(crate) enum McpCommand {
    /// List tools; the reply carries the result. `timeout` overrides the
    /// engine's configured listing timeout for this request (a shorter
    /// catalogue-refresh deadline), or is `None` to use the default.
    ListTools {
        timeout: Option<Duration>,
        reply: Sender<Result<Vec<McpTool>, McpError>>,
    },
    /// Call a tool; the reply carries the result.
    Call {
        session_id: u64,
        request: CallRequest,
        reply: Sender<Result<CallToolResult, McpError>>,
        chunk_tx: Option<crossbeam_channel::Sender<Vec<u8>>>,
    },
    /// List resources; the reply carries the result.
    ListResources(Sender<Result<Vec<McpResource>, McpError>>),
    /// Read one resource; the reply carries the result.
    ReadResource {
        uri: String,
        reply: Sender<Result<Vec<McpContent>, McpError>>,
    },
    /// Cancel every in-flight call started by the session.
    CancelSession { session_id: u64 },
    /// Report the dispatcher's in-flight/queued call counts.
    ///
    /// Test-only observability: the cap test needs to assert that an excess call
    /// is queued rather than spawned, deterministically (no sleep-poll). The
    /// production paths never construct it.
    #[cfg(test)]
    InflightStats(Sender<DispatcherStats>),
    /// Close the connection and exit the dispatcher.
    Shutdown,
}
