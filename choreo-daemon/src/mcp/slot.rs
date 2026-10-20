//! The manager's shared slot value types and per-operation budgets, plus the
//! bounded connect helper every slot is built through.
//!
//! [`ServerSlot`] is one live connection (the dispatcher-owning [`McpServer`]
//! plus the cloneable handle its tool wrappers share); [`SharedSlot`] wraps a
//! `ServerSlot` with the ref-count of sessions referencing a project-shared
//! connection. The budget constants bound each single connect and each
//! command-loop re-list, so one slow server cannot compound into an unbounded
//! stall, and [`connect_and_list`] / [`connect_entry`] / [`run_with_budget`] are
//! the connect-worker plumbing shared by startup, reload, reconnect, and the
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

/// Per-server budget for the daemon's BACKGROUND connect.
///
/// A background connect runs on a dedicated worker thread, never on the command
/// loop, so the socket is bound and clients are served long before any server
/// finishes handshaking. The budget therefore only bounds how long ONE server's
/// connect may outlive its worker: a cold `npx` start is routinely multi-second,
/// so this is deliberately generous. A server that overruns it is recorded as
/// failed and retried on the next `/mcp reload` or reconnect; the abandoned
/// worker's connection is dropped when the server eventually answers or exits.
pub(crate) const BACKGROUND_CONNECT_BUDGET: Duration = Duration::from_secs(60);

/// Budget for a single manual reconnect attempt (the `/mcp` surface's
/// reconnect action). Longer than the per-server startup share because it is
/// user-initiated and can afford to wait for one server.
pub(super) const RECONNECT_BUDGET: Duration = Duration::from_secs(10);

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

/// Budget for ONE server's tool re-listing on the command loop, after that
/// server reported a list change.
///
/// Deliberately distinct from, and far shorter than, the per-server request
/// timeout (60 s by default): the re-list runs on the command-loop thread, so a
/// single slow server must not freeze every session and client behind it. A
/// server that misses this deadline is skipped from that refresh with a warning
/// and keeps its previous registration, rather than stalling the loop. A few
/// seconds is ample for a healthy server, whose `tools/list` round-trip is
/// normally milliseconds.
pub(super) const CATALOGUE_REFRESH_BUDGET: Duration = Duration::from_secs(3);

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
    /// catalogue refresh that misses [`CATALOGUE_REFRESH_BUDGET`] can fall back
    /// to the previous registration instead of dropping the server's tools from
    /// the rebuilt catalogue, and so the daemon can rebuild the catalogue from
    /// cache without a single network round-trip.
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

/// Connect to a server and list its tools, both on the calling worker thread.
///
/// Bundling the initial listing into the connect worker lets the caller's
/// budget bound connect **and** discovery together: a server that handshakes
/// quickly but never answers `tools/list` is dropped at the caller's budget
/// rather than stalling it for the full per-server request timeout.
pub(crate) fn connect_and_list(
    config: &McpServerConfig,
    list_changes: Option<crossbeam_channel::Sender<McpListChange>>,
) -> Result<(McpServer, Vec<McpTool>), choreo_mcp::McpError> {
    // A remote server with an `oauth` block connects through the OAuth client
    // when the daemon's credential runtime is installed; otherwise (no oauth
    // block, or no running daemon) it connects unauthenticated.
    let auth = crate::mcp::oauth::wiring_for(config);
    let server = McpServer::connect_with_auth(config, list_changes, auth)?;
    let tools = server.handle().list_tools()?;
    Ok((server, tools))
}

/// Connect one server and list its tools off the calling thread, giving up
/// after `timeout`.
///
/// This is the ONE connect seam for every path that must not block: the
/// daemon-tier background connects, the per-session overlay workers, and the
/// bounded in-place rebuilds. A connect that overruns the budget is reported as
/// an error, so the caller records a failure and retries later instead of
/// waiting on a wedged server.
pub(crate) fn connect_entry(
    config: &McpServerConfig,
    list_changes: Option<crossbeam_channel::Sender<McpListChange>>,
    timeout: Duration,
) -> super::McpConnectOutcome {
    let cfg = config.clone();
    run_with_budget(
        move || connect_and_list(&cfg, list_changes).map_err(|e| e.to_string()),
        timeout,
    )
    .unwrap_or_else(|| Err(format!("did not connect within {}s", timeout.as_secs())))
}

/// Run `work` on a dedicated thread and wait at most `timeout` for its result.
///
/// Returns `None` when the budget expires, leaving the worker detached: a
/// wedged handshake is abandoned rather than joined, and whatever connection it
/// eventually builds is dropped on the abandoned thread (closing its
/// dispatcher). Exactly ONE thread is spawned per call — no joiner thread — so a
/// hung connect costs the single thread that is stuck in it.
pub(crate) fn run_with_budget<T: Send + 'static>(
    work: impl FnOnce() -> T + Send + 'static,
    timeout: Duration,
) -> Option<T> {
    let (done_tx, done_rx) = crossbeam_channel::bounded::<T>(1);
    std::thread::spawn(move || {
        // The receiver is gone once the budget expires (and on a panic in the
        // caller), which drops `work`'s result — and any connection in it.
        let _ = done_tx.send(work());
    });
    // A worker that panics never sends, so `Disconnected` is treated exactly
    // like a timeout: the caller sees no result and records the failure.
    done_rx.recv_timeout(timeout).ok()
}
