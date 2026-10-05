//! The manager's shared slot value types and per-operation budgets, plus the
//! bounded connect helper every slot is built through.
//!
//! [`ServerSlot`] is one live connection (the dispatcher-owning [`McpServer`]
//! plus the cloneable handle its tool wrappers share); [`SharedSlot`] wraps a
//! `ServerSlot` with the ref-count of sessions referencing a project-shared
//! connection. The budget constants bound each of the manager's
//! command-loop-thread sweeps so one slow server cannot compound into an
//! unbounded stall, and [`connect_and_list`] / [`join_with_budget`] are the
//! connect-worker plumbing shared by startup, reload, reconnect, and the
//! per-session overlay resolution.
//!
//! These items live here rather than in the manager's root so `mod.rs` stays
//! focused on construction, registration, and the reconciliation orchestration,
//! and so the reconcile modules (`pool`, `overlay`, `query`) that consume them
//! sit next to types they share. The `pub(super)` items are re-imported by the
//! root, so a child module's `super::…` path is unchanged.

use choreo_mcp::{McpListChange, McpServer, McpServerConfig, McpServerHandle, McpTool};
use std::collections::HashSet;
use std::time::Duration;

/// A background connect's join handle: the connected server plus its listed
/// tools, or the connect/list error.
///
/// The listing runs inside the same bounded worker as the connect (see
/// [`connect_and_list`]), so the caller's budget covers connect AND discovery.
pub(super) type PendingConnect = (
    String,
    std::thread::JoinHandle<anyhow::Result<(McpServer, Vec<McpTool>)>>,
);

/// Default budget for connecting all MCP servers during `from_config`.
///
/// A hung server must never stall daemon startup: the manager gives the whole
/// batch this long to connect, then registers whatever is ready and leaves the
/// rest (their threads detach and the server is dropped, so nothing leaks).
pub(super) const STARTUP_BUDGET: Duration = Duration::from_secs(2);

/// Budget for a single manual reconnect attempt (the `/mcp` surface's
/// reconnect action). Longer than the per-server startup share because it is
/// user-initiated and can afford to wait for one server.
pub(super) const RECONNECT_BUDGET: Duration = Duration::from_secs(10);

/// Per-server budget for a config reload (the `/mcp reload` action). Reload
/// reconciles every configured server, so a (re)connect attempt gets the same
/// generous per-server share as a manual reconnect; servers are attempted one
/// at a time.
pub(super) const RELOAD_BUDGET: Duration = Duration::from_secs(10);

/// Aggregate budget for ONE reload sweep, capping the WHOLE reconcile rather
/// than each server.
///
/// `reload` (re)connects every added or changed server serially on the
/// command-loop thread, each bounded by [`RELOAD_BUDGET`]; without an aggregate
/// cap a config with many slow servers would stall the command loop for up to
/// one budget PER server. This deadline bounds the total (both the `/mcp`
/// reload path and the `mcp.json` watcher path run it), so a large or
/// partially-unreachable server set cannot compound into an unbounded stall; a
/// server reached after the deadline is recorded as failed for this sweep and
/// retried on the next reload.
pub(super) const RELOAD_TOTAL_BUDGET: Duration = Duration::from_secs(15);

/// Aggregate budget for ONE reconnect call, capping the WHOLE rebuild rather
/// than each connection.
///
/// A slug can name a daemon shared server plus one project-shared and/or
/// per-session connection per referencing project/session; each is rebuilt
/// serially, bounded by [`RECONNECT_BUDGET`]. This deadline bounds the total so
/// a slug referenced from many projects cannot stall the command loop (the
/// `/mcp reconnect` path runs it) for one budget per connection; a connection
/// reached after the deadline is reported as a failed reconnect.
pub(super) const RECONNECT_TOTAL_BUDGET: Duration = Duration::from_secs(15);

/// Budget for a synchronous tool-listing sweep the command loop performs when it
/// refreshes the tool catalogue (a list-changed event, a reload, or a
/// reconnect) or (re)resolves a session overlay.
///
/// Deliberately distinct from, and far shorter than, the per-server request
/// timeout (60 s by default): these sweeps run on the command-loop thread, so a
/// single slow server must not freeze every session and client behind it. A
/// server that misses this deadline is skipped from that refresh with a warning
/// and keeps its previous registration, rather than stalling the loop. A few
/// seconds is ample for a healthy server, whose `tools/list` round-trip is
/// normally milliseconds.
pub(super) const CATALOGUE_REFRESH_BUDGET: Duration = Duration::from_secs(3);

/// Aggregate budget for ONE catalogue-refresh sweep, capping the WHOLE sweep
/// rather than each server.
///
/// `register_all` re-lists every connected server serially on the command-loop
/// thread, each bounded by [`CATALOGUE_REFRESH_BUDGET`]. A reload or reconnect
/// over many servers would otherwise stall the loop for up to one budget PER
/// server; this deadline bounds the total, so a large server set cannot
/// compound into an unbounded command-loop stall. A server reached after the
/// deadline is skipped for this sweep (keeping its cached tools) and re-listed
/// on the next one.
pub(super) const CATALOGUE_REFRESH_TOTAL_BUDGET: Duration = Duration::from_secs(10);

/// One connected server: the live connection (owns the dispatcher thread) plus
/// a cloneable handle shared with that server's tool wrappers.
pub(super) struct ServerSlot {
    pub(super) handle: McpServerHandle,
    /// Dropped on shutdown; its `Drop` sends the shutdown command and joins the
    /// dispatcher with a bounded wait.
    #[expect(
        dead_code,
        reason = "kept only for its Drop (bounded dispatcher shutdown)"
    )]
    pub(super) server: McpServer,
    /// The resolved config this server was connected from; kept so a manual
    /// reconnect can re-attempt the same server.
    pub(super) config: McpServerConfig,
    /// How many tools are registered for this server (excluding the resource
    /// catalogue tools).
    pub(super) tool_count: usize,
    /// The tools registered by the most recent successful listing. Kept so a
    /// catalogue-refresh sweep that misses [`CATALOGUE_REFRESH_BUDGET`] can fall
    /// back to the previous registration instead of dropping the server's tools
    /// from the rebuilt catalogue.
    pub(super) tools: Vec<McpTool>,
}

/// A pooled, ref-counted project-shared server: one connection shared by every
/// session that references the same `(project_root, slug)`.
pub(super) struct SharedSlot {
    pub(super) slot: ServerSlot,
    /// The sessions currently referencing this connection. The connection is
    /// dropped when the last one leaves the project.
    pub(super) sessions: HashSet<u64>,
}

/// Connect to a server and list its tools, both on the caller's worker thread.
///
/// Bundling the initial listing into the connect worker lets `join_with_budget`
/// bound connect **and** discovery together: a server that handshakes quickly
/// but never answers `tools/list` is dropped at the caller's budget rather than
/// stalling it for the full per-server request timeout.
pub(super) fn connect_and_list(
    config: &McpServerConfig,
    list_changes: Option<crossbeam_channel::Sender<McpListChange>>,
) -> Result<(McpServer, Vec<McpTool>), choreo_mcp::McpError> {
    let server = McpServer::connect_with_list_changes(config, list_changes)?;
    let tools = server.handle().list_tools()?;
    Ok((server, tools))
}

/// Join a thread, giving up after `timeout` and returning `None` (leaving it
/// detached) when it overruns.
pub(super) fn join_with_budget<T: Send + 'static>(
    handle: std::thread::JoinHandle<T>,
    timeout: Duration,
) -> Option<T> {
    let (done_tx, done_rx) = crossbeam_channel::bounded::<T>(1);
    std::thread::spawn(move || {
        if let Ok(value) = handle.join() {
            let _ = done_tx.send(value);
        }
    });
    match done_rx.recv_timeout(timeout) {
        Ok(value) => Some(value),
        Err(crossbeam_channel::RecvTimeoutError::Timeout) => None,
        Err(crossbeam_channel::RecvTimeoutError::Disconnected) => None,
    }
}
