use std::sync::Arc;

/// Session-level context passed through tool execution.
///
/// Carries the session ID and database handle so tools (especially DB tools)
/// can perform session-scoped read/write operations without needing thread-local
/// state or separate dispatch paths.
#[derive(Clone)]
pub struct ToolContext {
    /// The session that initiated this tool call.
    pub session_id: u64,
    /// Handle to the daemon's shared redb database.
    pub db: Arc<redb::Database>,
}

impl ToolContext {
    pub fn new(session_id: u64, db: Arc<redb::Database>) -> Self {
        Self { session_id, db }
    }
}
