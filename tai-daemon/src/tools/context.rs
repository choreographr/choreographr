use std::sync::Arc;
use std::sync::mpsc;

use crate::daemon::DaemonCommand;

/// Session-level context passed through tool execution.
///
/// Carries the session ID, database handle, and a channel to the daemon
/// command loop so tools (especially admin tools) can perform daemon-level
/// operations without needing thread-local state or separate dispatch paths.
#[derive(Clone)]
pub struct ToolContext {
    /// The session that initiated this tool call.
    pub session_id: u64,
    /// Handle to the daemon's shared redb database.
    pub db: Arc<redb::Database>,
    /// Channel to the daemon command loop for daemon-level operations.
    pub daemon_tx: mpsc::Sender<DaemonCommand>,
}

impl ToolContext {
    pub fn new(
        session_id: u64,
        db: Arc<redb::Database>,
        daemon_tx: mpsc::Sender<DaemonCommand>,
    ) -> Self {
        Self {
            session_id,
            db,
            daemon_tx,
        }
    }
}
