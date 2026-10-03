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
//!   from the in-flight registry and to trigger a restart on a dead transport).
//!
//! The only shared-mutable state in the crate is the in-flight registry, and it
//! is owned solely by the dispatcher thread (no lock); the per-call
//! cancellation token is the sanctioned "cooperative flag" used to un-block an
//! in-flight request.

use crate::config::McpServerConfig;
use crate::error::McpError;
use crate::protocol::{CallToolResult, McpContent, McpListChange, McpResource, McpTool};
use crossbeam_channel::{Receiver, RecvTimeoutError, Sender};
use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
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
        let join = std::thread::Builder::new()
            .name(format!("mcp-{}", config.slug))
            .spawn(move || run_dispatcher(engine, factory, &cmd_rx, max_concurrent_calls))
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
        let (tx, rx) = crossbeam_channel::bounded(1);
        self.cmd_tx
            .send(McpCommand::ListTools(tx))
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
    pub(crate) cancel: CancelToken,
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
    /// List every advertised tool (engine follows pagination, bounded by the
    /// engine's own configured timeout).
    fn list_tools(&self) -> BoxFuture<'_, Result<Vec<McpTool>, McpError>>;

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
    /// List tools; the reply carries the result.
    ListTools(Sender<Result<Vec<McpTool>, McpError>>),
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

/// Completion notice a spawned call task sends back to the dispatcher.
struct Done {
    call_id: u64,
    transport_failed: bool,
}

/// A tool invocation waiting for a free concurrency slot.
///
/// The dispatcher admits a call straight away when a slot is free, otherwise it
/// parks the request here until an in-flight call completes. The queued entry
/// carries everything the eventual spawn needs, plus its cancellation token so a
/// session cancel can reach a call that has not started yet.
struct QueuedCall {
    call_id: u64,
    session_id: u64,
    request: CallRequest,
    reply: Sender<Result<CallToolResult, McpError>>,
    chunk_tx: Option<crossbeam_channel::Sender<Vec<u8>>>,
    cancel: CancelToken,
}

/// Per-server accounting for the concurrent-call cap.
///
/// Kept as pure arithmetic (no channels, no threads) so the admission decision —
/// admit up to `cap`, queue the rest, promote one per completion — is
/// unit-testable without spawning anything. The dispatcher owns the single
/// instance and the [`QueuedCall`] deque that mirrors `queued`.
struct CallGate {
    cap: usize,
    active: usize,
    queued: usize,
}

impl CallGate {
    /// Build a gate with `cap` slots, clamped to at least one so a misconfigured
    /// zero can never wedge the dispatcher.
    fn new(cap: usize) -> Self {
        Self {
            cap: cap.max(1),
            active: 0,
            queued: 0,
        }
    }

    /// Admit a call: `true` when a slot is free (and is now taken), `false` when
    /// the call must wait in the queue.
    fn admit(&mut self) -> bool {
        if self.active < self.cap {
            self.active += 1;
            true
        } else {
            self.queued += 1;
            false
        }
    }

    /// Whether a queued call can be promoted into a free slot.
    fn has_capacity(&self) -> bool {
        self.active < self.cap
    }

    /// Move one queued call into an active slot. Only call when
    /// [`has_capacity`](Self::has_capacity) is true and the deque is non-empty.
    fn promote(&mut self) {
        debug_assert!(self.has_capacity() && self.queued > 0);
        self.queued = self.queued.saturating_sub(1);
        self.active += 1;
    }

    /// Record that an active call finished, freeing a slot.
    fn complete(&mut self) {
        self.active = self.active.saturating_sub(1);
    }

    /// Record that a queued call was abandoned without ever running.
    fn abandon(&mut self) {
        self.queued = self.queued.saturating_sub(1);
    }
}

/// Test-only snapshot of the dispatcher's call accounting.
#[cfg(test)]
#[derive(Debug)]
pub(crate) struct DispatcherStats {
    active: usize,
    queued: usize,
}

/// Restart policy for a dispatcher whose engine's transport dies.
///
/// Bounded attempts with exponential backoff (capped at 60 s) so a flapping
/// server is retried a few times and then left alone until the next request.
struct RestartPolicy {
    max_attempts: u32,
    base_backoff: Duration,
    failures: u32,
}

impl Default for RestartPolicy {
    fn default() -> Self {
        Self {
            max_attempts: 3,
            base_backoff: Duration::from_millis(500),
            failures: 0,
        }
    }
}

impl RestartPolicy {
    /// Backoff for the current failure count: `base * 2^(n-1)`, capped at 60 s.
    fn backoff(&self) -> Duration {
        let shift = self.failures.saturating_sub(1).min(7);
        self.base_backoff
            .saturating_mul(1u32 << shift)
            .min(Duration::from_mins(1))
    }

    /// Rebuild the engine after a transport failure, subject to the budget.
    ///
    /// Resets the failure count on success. The sleep is skipped entirely when
    /// the backoff is zero, which is how the unit tests keep the restart path
    /// free of any time-based wait.
    fn on_transport_failure(&mut self, factory: &EngineFactory, engine: &mut Arc<dyn McpEngine>) {
        self.failures = self.failures.saturating_add(1);
        if self.failures > self.max_attempts {
            tracing::warn!(
                failures = self.failures,
                "MCP server exceeded restart budget; not reconnecting"
            );
            return;
        }
        let backoff = self.backoff();
        if !backoff.is_zero() {
            std::thread::sleep(backoff);
        }
        match factory() {
            Ok(fresh) => {
                *engine = fresh;
                self.failures = 0;
                tracing::info!("MCP server reconnected after transport failure");
            }
            Err(e) => tracing::warn!(error = %e, "MCP reconnect failed"),
        }
    }
}

/// Whether an error indicates the transport (not the request) failed, which is
/// the trigger for a reconnect.
fn is_transport_error(error: &McpError) -> bool {
    matches!(
        error,
        McpError::ServerShutdown | McpError::Io(_) | McpError::NotConnected
    )
}

/// The dispatcher thread body: serve commands until shutdown or disconnect.
///
/// `max_concurrent_calls` bounds how many tool calls to this server run at once;
/// calls beyond the cap wait in a queue and are started as slots free. Listings
/// and resource reads run synchronously on this thread and so are naturally
/// serialized and not subject to the cap.
fn run_dispatcher(
    initial: Arc<dyn McpEngine>,
    factory: EngineFactory,
    cmd_rx: &Receiver<McpCommand>,
    max_concurrent_calls: usize,
) {
    let rt = match crate::runtime::handle() {
        Ok(handle) => handle,
        Err(e) => {
            tracing::error!(error = %e, "MCP dispatcher cannot start without a runtime");
            return;
        }
    };

    let mut engine = initial;
    let mut policy = RestartPolicy::default();
    let mut inflight: HashMap<u64, (CancelToken, u64)> = HashMap::new();
    let mut queued: VecDeque<QueuedCall> = VecDeque::new();
    let mut gate = CallGate::new(max_concurrent_calls);
    let mut next_call_id: u64 = 1;

    // Completion notices from spawned call tasks; drained between commands.
    let (done_tx, done_rx) = crossbeam_channel::unbounded::<Done>();

    loop {
        // Drain completion notices before waiting: a finished call frees a slot
        // and may have killed the transport (triggering a reconnect).
        while let Ok(done) = done_rx.try_recv() {
            apply_done(
                &done,
                &mut inflight,
                &mut gate,
                &mut policy,
                &factory,
                &mut engine,
            );
        }
        // Start queued calls into any slots the completions freed.
        pump_queued(
            &mut queued,
            &mut gate,
            &mut inflight,
            &rt,
            &engine,
            &done_tx,
        );

        // Wait for the next command or completion. Both are event sources, so no
        // polling: a finished call wakes the loop to free its slot even when no
        // command has arrived.
        crossbeam_channel::select! {
            recv(cmd_rx) -> msg => {
                if let Ok(cmd) = msg {
                    let keep_running = handle_command(
                        cmd,
                        &rt,
                        &mut engine,
                        &factory,
                        &mut policy,
                        &mut inflight,
                        &mut queued,
                        &mut gate,
                        &done_tx,
                        &mut next_call_id,
                    );
                    if !keep_running {
                        break;
                    }
                } else {
                    tracing::debug!("MCP command channel closed; dispatcher exiting");
                    break;
                }
            }
            recv(done_rx) -> msg => {
                if let Ok(done) = msg {
                    apply_done(
                        &done,
                        &mut inflight,
                        &mut gate,
                        &mut policy,
                        &factory,
                        &mut engine,
                    );
                }
            }
        }
    }
    tracing::debug!("MCP dispatcher thread exiting");
}

/// Apply one completion notice: drop the call from the in-flight registry, free
/// its slot, and rebuild the engine if the call died on the transport.
fn apply_done(
    done: &Done,
    inflight: &mut HashMap<u64, (CancelToken, u64)>,
    gate: &mut CallGate,
    policy: &mut RestartPolicy,
    factory: &EngineFactory,
    engine: &mut Arc<dyn McpEngine>,
) {
    inflight.remove(&done.call_id);
    gate.complete();
    if done.transport_failed {
        policy.on_transport_failure(factory, engine);
    }
}

/// Start queued calls while the gate has free slots.
///
/// A promoted call is registered in `inflight` exactly like an immediately
/// admitted one, so a later session cancel can still reach it.
fn pump_queued(
    queued: &mut VecDeque<QueuedCall>,
    gate: &mut CallGate,
    inflight: &mut HashMap<u64, (CancelToken, u64)>,
    rt: &tokio::runtime::Handle,
    engine: &Arc<dyn McpEngine>,
    done_tx: &Sender<Done>,
) {
    while gate.has_capacity() {
        let Some(call) = queued.pop_front() else {
            break;
        };
        gate.promote();
        inflight.insert(call.call_id, (call.cancel.clone(), call.session_id));
        spawn_call(rt, engine, done_tx, call);
    }
}

/// Spawn one call onto the sidecar runtime.
///
/// The task sends a [`Done`] notice (so the dispatcher frees the slot and can
/// reconnect a dead transport) followed by the reply; both are best-effort — a
/// dropped receiver means the caller already went away.
fn spawn_call(
    rt: &tokio::runtime::Handle,
    engine: &Arc<dyn McpEngine>,
    done_tx: &Sender<Done>,
    call: QueuedCall,
) {
    let QueuedCall {
        call_id,
        request,
        reply,
        chunk_tx,
        cancel,
        ..
    } = call;
    let engine = Arc::clone(engine);
    let done_tx = done_tx.clone();
    rt.spawn(async move {
        let result = engine
            .call_tool(EngineCall {
                request,
                cancel,
                chunk_tx,
            })
            .await;
        let transport_failed = is_transport_error_ref(&result);
        let _ = done_tx.send(Done {
            call_id,
            transport_failed,
        });
        let _ = reply.send(result);
    });
}

/// Dispatch one command, returning `false` when the dispatcher should stop
/// (i.e. the command was a shutdown). Split out of [`run_dispatcher`] so the
/// (long) match does not have to be nested inside the `select!` arm.
#[expect(
    clippy::too_many_arguments,
    reason = "the dispatcher's mutable state is all owned on the thread; bundling it into a struct would add indirection without clarifying anything"
)]
fn handle_command(
    cmd: McpCommand,
    rt: &tokio::runtime::Handle,
    engine: &mut Arc<dyn McpEngine>,
    factory: &EngineFactory,
    policy: &mut RestartPolicy,
    inflight: &mut HashMap<u64, (CancelToken, u64)>,
    queued: &mut VecDeque<QueuedCall>,
    gate: &mut CallGate,
    done_tx: &Sender<Done>,
    next_call_id: &mut u64,
) -> bool {
    match cmd {
        McpCommand::ListTools(reply) => {
            let mut result = rt.block_on(engine.list_tools());
            if let Err(e) = &result
                && is_transport_error(e)
            {
                policy.on_transport_failure(factory, engine);
                result = rt.block_on(engine.list_tools());
            }
            let _ = reply.send(result);
        }
        McpCommand::Call {
            session_id,
            request,
            reply,
            chunk_tx,
        } => {
            let call_id = *next_call_id;
            *next_call_id = next_call_id.wrapping_add(1);
            let cancel = CancelToken::new();
            let call = QueuedCall {
                call_id,
                session_id,
                request,
                reply,
                chunk_tx,
                cancel: cancel.clone(),
            };
            if gate.admit() {
                inflight.insert(call_id, (cancel, session_id));
                spawn_call(rt, engine, done_tx, call);
            } else {
                queued.push_back(call);
            }
        }
        McpCommand::ListResources(reply) => {
            let mut result = rt.block_on(engine.list_resources());
            if let Err(e) = &result
                && is_transport_error(e)
            {
                policy.on_transport_failure(factory, engine);
                result = rt.block_on(engine.list_resources());
            }
            let _ = reply.send(result);
        }
        McpCommand::ReadResource { uri, reply } => {
            let mut result = rt.block_on(engine.read_resource(uri.clone()));
            if let Err(e) = &result
                && is_transport_error(e)
            {
                policy.on_transport_failure(factory, engine);
                result = rt.block_on(engine.read_resource(uri));
            }
            let _ = reply.send(result);
        }
        McpCommand::CancelSession { session_id } => {
            for (token, call_session) in inflight.values() {
                if *call_session == session_id {
                    token.cancel();
                }
            }
            // Cancel queued calls too, replying immediately and freeing their
            // slots — a cancelled call must not linger behind the cap.
            let mut index = 0;
            while index < queued.len() {
                if queued[index].session_id == session_id {
                    if let Some(call) = queued.remove(index) {
                        gate.abandon();
                        call.cancel.cancel();
                        let _ = call.reply.send(Err(McpError::Cancelled));
                    }
                } else {
                    index += 1;
                }
            }
        }
        #[cfg(test)]
        McpCommand::InflightStats(reply) => {
            let _ = reply.send(DispatcherStats {
                active: gate.active,
                queued: gate.queued,
            });
        }
        McpCommand::Shutdown => {
            rt.block_on(engine.shutdown());
            // Leaving returns `false`; `run_dispatcher` then drops `queued`,
            // whose reply senders close, so a waiting caller observes
            // `NotConnected` rather than blocking.
            return false;
        }
    }
    true
}

/// `is_transport_error` over a `Result` reference (used by spawned call tasks).
fn is_transport_error_ref(result: &Result<CallToolResult, McpError>) -> bool {
    result.as_ref().err().is_some_and(is_transport_error)
}

/// A cloneable, single-bit cooperative cancellation token.
///
/// This is the sanctioned cancellation-flag exception (AGENTS.md): a channel
/// message cannot interrupt a request already in flight inside `rmcp`, so a
/// data-free flag relays "stop" to the waiting call task. `tokio::sync::Notify`
/// makes the wait event-driven — the task parks on the token and wakes the
/// instant a session cancel fires, never polling or sleeping.
#[derive(Clone)]
pub(crate) struct CancelToken(Arc<CancelInner>);

struct CancelInner {
    flag: AtomicBool,
    notify: tokio::sync::Notify,
}

impl CancelToken {
    /// Create an un-cancelled token.
    pub(crate) fn new() -> Self {
        Self(Arc::new(CancelInner {
            flag: AtomicBool::new(false),
            notify: tokio::sync::Notify::new(),
        }))
    }

    /// Set the flag and wake any waiter. Idempotent.
    pub(crate) fn cancel(&self) {
        self.0.flag.store(true, Ordering::SeqCst);
        self.0.notify.notify_waiters();
    }

    /// Resolve once [`cancel`](Self::cancel) has been called.
    ///
    /// Registers the waiter before re-checking the flag, so a cancel that races
    /// this call is never lost.
    pub(crate) async fn cancelled(&self) {
        if self.0.flag.load(Ordering::SeqCst) {
            return;
        }
        let notified = self.0.notify.notified();
        if self.0.flag.load(Ordering::SeqCst) {
            return;
        }
        notified.await;
    }
}

/// Join a thread, giving up after `timeout` and leaving it detached.
///
/// The handle is moved into a waiter thread that joins and signals over a
/// crossbeam channel; this thread waits on that channel with `recv_timeout`, so
/// a wedged dispatcher cannot hang shutdown.
fn join_bounded(join: std::thread::JoinHandle<()>, timeout: Duration) {
    let (done_tx, done_rx) = crossbeam_channel::bounded::<()>(1);
    std::thread::spawn(move || {
        let _ = join.join();
        let _ = done_tx.send(());
    });
    match done_rx.recv_timeout(timeout) {
        Ok(()) | Err(RecvTimeoutError::Disconnected) => {}
        Err(RecvTimeoutError::Timeout) => {
            tracing::warn!(
                ?timeout,
                "MCP dispatcher did not exit within timeout; detaching"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::CallToolResult;
    use std::sync::Mutex;

    /// A scripted mock engine: `list_tools` returns a fixed vector; `call_tool`
    /// waits for the cancellation token or returns a fixed result, whichever
    /// comes first.
    struct MockEngine {
        tools: Vec<McpTool>,
        result: CallToolResult,
        calls: Mutex<Vec<String>>,
        /// Signalled the instant a call reaches the mock, so a test can cancel
        /// deterministically after the call is registered (never by sleeping).
        started_tx: Sender<()>,
        /// When true, `call_tool` blocks until cancelled and never returns the
        /// scripted result (the analogue of a long-running server tool).
        blocks: bool,
    }

    impl MockEngine {
        fn new(tools: Vec<McpTool>, result: CallToolResult) -> Self {
            let (started_tx, _) = crossbeam_channel::unbounded();
            Self {
                tools,
                result,
                calls: Mutex::new(Vec::new()),
                started_tx,
                blocks: false,
            }
        }

        fn blocking(tools: Vec<McpTool>, result: CallToolResult, started_tx: Sender<()>) -> Self {
            Self {
                tools,
                result,
                calls: Mutex::new(Vec::new()),
                started_tx,
                blocks: true,
            }
        }
        fn call_count(&self) -> usize {
            self.calls
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .len()
        }
    }

    impl McpEngine for MockEngine {
        fn list_tools(&self) -> BoxFuture<'_, Result<Vec<McpTool>, McpError>> {
            let tools = self.tools.clone();
            Box::pin(async move { Ok(tools) })
        }

        fn call_tool(&self, call: EngineCall) -> BoxFuture<'_, Result<CallToolResult, McpError>> {
            let EngineCall {
                request, cancel, ..
            } = call;
            self.calls
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(request.name.clone());
            let started_tx = self.started_tx.clone();
            let result = self.result.clone();
            let blocks = self.blocks;
            Box::pin(async move {
                // Signal that the call is in flight (and thus registered in the
                // dispatcher's in-flight map) before awaiting.
                let _ = started_tx.send(());
                if blocks {
                    // Park until cancelled; a cancel is the only way out.
                    cancel.cancelled().await;
                    Err(McpError::Cancelled)
                } else {
                    Ok(result)
                }
            })
        }

        fn list_resources(&self) -> BoxFuture<'_, Result<Vec<McpResource>, McpError>> {
            Box::pin(async { Ok(Vec::new()) })
        }

        fn read_resource(&self, uri: String) -> BoxFuture<'_, Result<Vec<McpContent>, McpError>> {
            let _ = uri;
            Box::pin(async { Ok(Vec::new()) })
        }

        fn supports_resources(&self) -> bool {
            false
        }

        fn shutdown(&self) -> BoxFuture<'_, ()> {
            Box::pin(async {})
        }

        fn name(&self) -> &'static str {
            "mock"
        }

        fn version(&self) -> &'static str {
            "0.0.1"
        }
    }

    /// Spawn a dispatcher over `engine` with a factory that rebuilds it, using
    /// the default concurrency cap.
    fn spawn(engine: Arc<MockEngine>) -> (McpServerHandle, std::thread::JoinHandle<()>) {
        spawn_with_cap(engine, crate::DEFAULT_MAX_CONCURRENT_CALLS)
    }

    /// Spawn a dispatcher over `engine` with an explicit concurrency cap.
    fn spawn_with_cap(
        engine: Arc<MockEngine>,
        cap: usize,
    ) -> (McpServerHandle, std::thread::JoinHandle<()>) {
        crate::runtime::init().expect("runtime init");
        let (cmd_tx, cmd_rx) = crossbeam_channel::unbounded();
        let handle = McpServerHandle {
            cmd_tx,
            name: "mock".into(),
            version: "0.0.1".into(),
            default_timeout: Duration::from_secs(5),
            resources: false,
        };
        let factory_engine = Arc::clone(&engine);
        let factory: EngineFactory =
            Box::new(move || Ok(Arc::clone(&factory_engine) as Arc<dyn McpEngine>));
        let join = std::thread::spawn(move || {
            run_dispatcher(engine as Arc<dyn McpEngine>, factory, &cmd_rx, cap);
        });
        (handle, join)
    }

    /// Test-only accessor for the dispatcher's call accounting.
    impl McpServerHandle {
        fn stats(&self) -> DispatcherStats {
            let (tx, rx) = crossbeam_channel::bounded(1);
            let _ = self.cmd_tx.send(McpCommand::InflightStats(tx));
            rx.recv().expect("dispatcher alive for a stats query")
        }
    }

    fn tool(name: &str) -> McpTool {
        McpTool {
            name: name.into(),
            description: Some(format!("the {name} tool")),
            input_schema: serde_json::json!({"type": "object"}),
            output_schema: None,
        }
    }

    fn ok_result() -> CallToolResult {
        CallToolResult {
            content: vec![crate::protocol::McpContent::Text { text: "ok".into() }],
            is_error: false,
            structured_content: None,
        }
    }

    #[test]
    fn list_tools_round_trips() {
        let engine = Arc::new(MockEngine::new(vec![tool("echo")], ok_result()));
        let (handle, join) = spawn(engine);
        let tools = handle.list_tools().expect("list tools");
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].name, "echo");
        let _ = handle.cmd_tx.send(McpCommand::Shutdown);
        join.join().expect("dispatcher joins");
    }

    #[test]
    fn call_tool_round_trips() {
        let engine = Arc::new(MockEngine::new(vec![], ok_result()));
        let (handle, join) = spawn(engine);
        let result = handle
            .call_tool(7, "echo", serde_json::json!({}), None)
            .expect("call tool");
        assert!(!result.is_error);
        let _ = handle.cmd_tx.send(McpCommand::Shutdown);
        join.join().expect("dispatcher joins");
    }

    #[test]
    fn cancel_session_stops_inflight_call() {
        // The mock blocks on the cancellation token, so a `CancelSession` for
        // the call's session resolves it as cancelled — deterministically: the
        // mock signals over a channel when the call is in flight, and only then
        // does the test cancel, so the call is always registered.
        let (started_tx, started_rx) = crossbeam_channel::unbounded();
        let engine = Arc::new(MockEngine::blocking(vec![], ok_result(), started_tx));
        let (handle, join) = spawn(engine);
        let caller = handle.clone();
        let call =
            std::thread::spawn(move || caller.call_tool(42, "slow", serde_json::json!({}), None));
        // Wait until the call is in flight, then cancel its session.
        started_rx.recv().expect("call started");
        handle.cancel_session(42);
        let result = call.join().expect("call thread joins");
        assert!(matches!(result, Err(McpError::Cancelled)), "got {result:?}");
        let _ = handle.cmd_tx.send(McpCommand::Shutdown);
        join.join().expect("dispatcher joins");
    }

    #[test]
    fn cancel_session_leaves_other_sessions() {
        let engine = Arc::new(MockEngine::new(vec![], ok_result()));
        let (handle, join) = spawn(engine);
        // Cancel a session with no in-flight call: a later call for a different
        // session must still succeed.
        handle.cancel_session(1);
        let result = handle
            .call_tool(2, "echo", serde_json::json!({}), None)
            .expect("unrelated session call");
        assert!(!result.is_error);
        let _ = handle.cmd_tx.send(McpCommand::Shutdown);
        join.join().expect("dispatcher joins");
    }

    #[test]
    fn call_after_shutdown_reports_not_connected() {
        let engine = Arc::new(MockEngine::new(vec![], ok_result()));
        let (handle, join) = spawn(engine);
        let _ = handle.cmd_tx.send(McpCommand::Shutdown);
        join.join().expect("dispatcher joins");
        let err = handle
            .call_tool(0, "echo", serde_json::json!({}), None)
            .expect_err("dispatcher gone");
        assert!(matches!(err, McpError::NotConnected), "got {err:?}");
    }

    #[test]
    fn restart_policy_backoff_is_capped() {
        let mut policy = RestartPolicy {
            max_attempts: 10,
            base_backoff: Duration::from_secs(1),
            failures: 0,
        };
        policy.failures = 1;
        assert_eq!(policy.backoff(), Duration::from_secs(1));
        policy.failures = 3;
        assert_eq!(policy.backoff(), Duration::from_secs(4));
        policy.failures = 20;
        assert_eq!(policy.backoff(), Duration::from_mins(1));
    }

    #[test]
    fn mock_engine_records_calls() {
        let engine = Arc::new(MockEngine::new(vec![], ok_result()));
        let (handle, join) = spawn(Arc::clone(&engine));
        handle
            .call_tool(0, "echo", serde_json::json!({}), None)
            .expect("call");
        assert_eq!(engine.call_count(), 1);
        let _ = handle.cmd_tx.send(McpCommand::Shutdown);
        join.join().expect("dispatcher joins");
    }

    #[test]
    fn call_gate_admits_up_to_cap_then_queues() {
        let mut gate = CallGate::new(2);
        assert!(gate.admit(), "first call takes a slot");
        assert!(gate.admit(), "second call takes the other slot");
        assert!(!gate.admit(), "third call is queued");
        assert!(!gate.admit(), "fourth call is queued too");
        assert_eq!((gate.active, gate.queued), (2, 2));

        // A completion frees a slot; exactly one queued call can then be
        // promoted, and its queue depth drops by one.
        gate.complete();
        assert!(gate.has_capacity());
        gate.promote();
        assert_eq!((gate.active, gate.queued), (2, 1));
        assert!(!gate.has_capacity());

        // Abandoning a queued call (a session cancel) drops it from the queue.
        gate.abandon();
        assert_eq!((gate.active, gate.queued), (2, 0));
    }

    #[test]
    fn call_gate_clamps_zero_cap_to_one() {
        let mut gate = CallGate::new(0);
        assert!(gate.admit());
        assert!(!gate.admit());
        assert_eq!((gate.active, gate.queued), (1, 1));
    }

    #[test]
    fn concurrency_cap_queues_excess_calls() {
        // A blocking mock: a call that is admitted parks until cancelled, so the
        // slot it holds stays busy. With a cap of one, the second call must wait
        // in the queue rather than reach the engine.
        let (started_tx, started_rx) = crossbeam_channel::unbounded();
        let engine = Arc::new(MockEngine::blocking(vec![], ok_result(), started_tx));
        let (handle, join) = spawn_with_cap(engine, 1);

        // First call: admitted immediately and reaches the engine.
        let (tx1, rx1) = crossbeam_channel::bounded(1);
        handle
            .cmd_tx
            .send(McpCommand::Call {
                session_id: 1,
                request: CallRequest {
                    name: "slow".into(),
                    arguments: serde_json::json!({}),
                    timeout: Duration::from_secs(5),
                },
                reply: tx1,
                chunk_tx: None,
            })
            .expect("send call 1");
        started_rx.recv().expect("call 1 reached the engine");

        // Second call: the only slot is taken, so it queues. Sending both
        // commands and the stats query from this thread pins their order, so the
        // counts are deterministic (no sleep-poll).
        let (tx2, rx2) = crossbeam_channel::bounded(1);
        handle
            .cmd_tx
            .send(McpCommand::Call {
                session_id: 2,
                request: CallRequest {
                    name: "slow".into(),
                    arguments: serde_json::json!({}),
                    timeout: Duration::from_secs(5),
                },
                reply: tx2,
                chunk_tx: None,
            })
            .expect("send call 2");
        let stats = handle.stats();
        assert_eq!(stats.active, 1, "one call in flight");
        assert_eq!(stats.queued, 1, "the second call waits in the queue");

        // Cancelling session 1 frees the slot; the queued call is promoted and
        // reaches the engine.
        handle.cancel_session(1);
        assert!(
            matches!(rx1.recv().expect("call 1 reply"), Err(McpError::Cancelled)),
            "call 1 should be cancelled"
        );
        started_rx
            .recv()
            .expect("call 2 promoted and reaches the engine after a slot frees");
        let stats = handle.stats();
        assert_eq!(stats.active, 1);
        assert_eq!(stats.queued, 0, "the queue drained once the slot freed");

        handle.cancel_session(2);
        assert!(
            matches!(rx2.recv().expect("call 2 reply"), Err(McpError::Cancelled)),
            "call 2 should be cancelled"
        );

        let _ = handle.cmd_tx.send(McpCommand::Shutdown);
        join.join().expect("dispatcher joins");
    }
}
