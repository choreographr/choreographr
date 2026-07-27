use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::mpsc;

use crate::daemon::DaemonCommand;

/// Session-level context passed through tool execution.
///
/// Carries the session ID, database handle, a channel to the daemon
/// command loop, and parent session config so tools (especially
/// spawn_subsession) can create child sessions with inherited settings.
#[derive(Clone)]
pub struct ToolContext {
    /// The session that initiated this tool call.
    pub session_id: u64,
    /// Handle to the daemon's shared redb database.
    pub db: Arc<redb::Database>,
    /// Channel to the daemon command loop for daemon-level operations.
    pub daemon_tx: mpsc::Sender<DaemonCommand>,
    /// Tool groups active in the parent session (inherited by sub-sessions).
    pub active_tool_groups: HashSet<String>,
    /// Reasoning effort configured for the parent session.
    pub reasoning_effort: Option<String>,
    /// Model selected for the parent session (inherited by sub-sessions).
    pub selected_model: Option<String>,
    /// Working directory for the parent session (used as fallback working_dir).
    pub working_dir: Option<PathBuf>,
    /// Cancellation flag: set to `true` when the parent session is cancelled.
    /// Tools that block indefinitely (e.g. `spawn_subsession`) should poll this
    /// and abort when it becomes `true`.
    pub cancelled: Arc<AtomicBool>,
}

impl ToolContext {
    /// Convenience constructor for tests and simple usage where only the
    /// session ID, database, and daemon channel are needed.
    /// New config fields (`active_tool_groups`, `reasoning_effort`, `selected_model`, `working_dir`)
    /// default to empty/None.
    pub fn new(
        session_id: u64,
        db: Arc<redb::Database>,
        daemon_tx: mpsc::Sender<DaemonCommand>,
    ) -> Self {
        Self {
            session_id,
            db,
            daemon_tx,
            active_tool_groups: HashSet::new(),
            reasoning_effort: None,
            selected_model: None,
            working_dir: None,
            cancelled: Arc::new(AtomicBool::new(false)),
        }
    }
}
