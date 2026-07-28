use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};

use tracing::{debug, warn};

use crate::error::AcpError;

/// Internal state for an ACP session, tracking the mapping between the ACP
/// session ID (a string like `"sess_<uuid>"`) and the daemon's numeric
/// session ID, plus current configuration and active-request guard.
#[derive(Debug)]
pub struct AcpSession {
    /// Numeric session ID used by Choreographr.
    pub daemon_id: u64,
    /// ACP protocol session ID string (e.g. `"sess_<uuid>"`).
    pub acp_id: String,
    /// Active prompt request ID, if any.  The ACP spec forbids concurrent
    /// prompts on a single session, so this acts as a guard.
    pub active_request: Option<u32>,
    /// Currently selected model for this session.
    pub model: Option<String>,
    /// Reasoning effort setting.
    pub reasoning_effort: Option<String>,
    /// Enabled tool groups (e.g. `["read", "edit", "terminal"]`).
    pub tool_groups: Vec<String>,
}

/// Monotonically incrementing counter for ACP session IDs.
/// Replaces UUID generation — unique per-process and sufficient for
/// session identification.
static NEXT_SESSION_ID: AtomicU64 = AtomicU64::new(1);

/// Manages the mapping between ACP session IDs and daemon session IDs, plus
/// active-prompt tracking per session.
///
/// The daemon uses numeric `u64` session IDs internally; the ACP protocol
/// uses string IDs (`"sess_<counter>"`).  This manager maintains a bidirectional
/// map so the event handler can look up either direction.
#[derive(Debug)]
pub struct SessionManager {
    /// ACP session ID → `AcpSession`.
    sessions: HashMap<String, AcpSession>,
    /// Daemon numeric session ID → ACP session ID.
    by_daemon_id: HashMap<u64, String>,
    /// Monotonically increasing request ID counter.  Each outgoing
    /// `ClientMessage` that expects a streaming response gets a unique ID.
    next_request_id: u32,
}

impl Default for SessionManager {
    fn default() -> Self {
        Self::new()
    }
}

impl SessionManager {
    pub fn new() -> Self {
        Self {
            sessions: HashMap::new(),
            by_daemon_id: HashMap::new(),
            next_request_id: 1,
        }
    }

    /// Create a new session mapping with an auto-generated ACP session ID.
    /// Returns the generated ACP ID.
    pub fn create(&mut self, daemon_id: u64) -> String {
        let id = NEXT_SESSION_ID.fetch_add(1, Ordering::Relaxed);
        let acp_id = format!("sess_{id}");
        debug!(daemon_id, acp_id, "creating session mapping");
        self.insert_session(daemon_id, &acp_id);
        acp_id
    }

    /// Create a session mapping with a caller-provided ACP ID (used when
    /// the editor requests a session load with a specific ID).
    pub fn create_with_id(&mut self, daemon_id: u64, acp_id: &str) {
        debug!(
            daemon_id,
            acp_id, "creating session mapping with provided ID"
        );
        self.insert_session(daemon_id, acp_id);
    }

    /// Shared path for `create` and `create_with_id`: builds the `AcpSession`,
    /// inserts into both maps.
    fn insert_session(&mut self, daemon_id: u64, acp_id: &str) {
        let owned: String = acp_id.into();
        let session = AcpSession {
            daemon_id,
            acp_id: owned.clone(),
            active_request: None,
            model: None,
            reasoning_effort: None,
            tool_groups: Vec::new(),
        };
        self.sessions.insert(owned.clone(), session);
        self.by_daemon_id.insert(daemon_id, owned);
    }

    pub fn get(&self, acp_id: &str) -> Option<&AcpSession> {
        self.sessions.get(acp_id)
    }

    pub fn get_mut(&mut self, acp_id: &str) -> Option<&mut AcpSession> {
        self.sessions.get_mut(acp_id)
    }

    pub fn get_by_daemon_id(&self, daemon_id: u64) -> Option<&str> {
        self.by_daemon_id.get(&daemon_id).map(|s| s.as_str())
    }

    /// Remove a session from the manager.  Returns `true` if the session
    /// existed and was removed.
    pub fn remove(&mut self, acp_id: &str) -> bool {
        if let Some(session) = self.sessions.remove(acp_id) {
            self.by_daemon_id.remove(&session.daemon_id);
            debug!(acp_id, "removed session mapping");
            true
        } else {
            warn!(acp_id, "attempted to remove non-existent session");
            false
        }
    }

    /// Allocate the next monotonically-increasing request ID.  Wraps on
    /// overflow (u32::MAX → 0) since request IDs only need to be unique
    /// per-daemon-connection-lifetime.
    pub fn next_request_id(&mut self) -> u32 {
        let id = self.next_request_id;
        self.next_request_id = self.next_request_id.wrapping_add(1);
        id
    }

    /// Try to begin a prompt for the given session.  Returns
    /// `Err(SessionBusy)` if the session already has an active prompt, or
    /// `Err(SessionNotFound)` if the session doesn't exist.
    pub fn try_begin_prompt(&mut self, acp_id: &str, request_id: u32) -> Result<(), AcpError> {
        let session = self
            .sessions
            .get_mut(acp_id)
            .ok_or_else(|| AcpError::SessionNotFound(acp_id.to_string()))?;

        if session.active_request.is_some() {
            warn!(acp_id, "session already has an active prompt");
            return Err(AcpError::SessionBusy(acp_id.to_string()));
        }

        session.active_request = Some(request_id);
        debug!(acp_id, request_id, "prompt started");
        Ok(())
    }

    /// End the active prompt for the given session (clears the guard).
    pub fn end_prompt(&mut self, acp_id: &str) {
        if let Some(session) = self.sessions.get_mut(acp_id)
            && let Some(request_id) = session.active_request.take()
        {
            debug!(acp_id, request_id, "prompt ended");
        }
    }

    /// Check whether the given session currently has an active prompt.
    pub fn is_prompt_active(&self, acp_id: &str) -> bool {
        self.sessions
            .get(acp_id)
            .is_some_and(|s| s.active_request.is_some())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn create_session_generates_unique_ids() {
        let mut mgr = SessionManager::new();
        let id1 = mgr.create(1);
        let id2 = mgr.create(2);
        assert_ne!(id1, id2);
        assert!(id1.starts_with("sess_"));
        assert!(id2.starts_with("sess_"));
    }

    #[test]
    fn get_session_by_acp_id() {
        let mut mgr = SessionManager::new();
        let acp_id = mgr.create(42);
        let session = mgr.get(&acp_id);
        assert!(session.is_some());
        assert_eq!(session.unwrap().daemon_id, 42);
    }

    #[test]
    fn get_session_by_daemon_id() {
        let mut mgr = SessionManager::new();
        let acp_id = mgr.create(42);
        let found = mgr.get_by_daemon_id(42);
        assert_eq!(found, Some(acp_id.as_str()));
    }

    #[test]
    fn create_with_id_uses_provided_id() {
        let mut mgr = SessionManager::new();
        mgr.create_with_id(10, "sess_custom");
        let session = mgr.get("sess_custom");
        assert!(session.is_some());
        assert_eq!(session.unwrap().daemon_id, 10);
    }

    #[test]
    fn remove_session() {
        let mut mgr = SessionManager::new();
        let acp_id = mgr.create(1);
        assert!(mgr.remove(&acp_id));
        assert!(!mgr.remove(&acp_id)); // second removal fails
        assert!(mgr.get(&acp_id).is_none());
        assert!(mgr.get_by_daemon_id(1).is_none());
    }

    #[test]
    fn try_begin_prompt_ok() {
        let mut mgr = SessionManager::new();
        let acp_id = mgr.create(1);
        assert!(mgr.try_begin_prompt(&acp_id, 100).is_ok());
        assert!(mgr.is_prompt_active(&acp_id));
    }

    #[test]
    fn try_begin_prompt_busy() {
        let mut mgr = SessionManager::new();
        let acp_id = mgr.create(1);
        mgr.try_begin_prompt(&acp_id, 100).unwrap();
        let result = mgr.try_begin_prompt(&acp_id, 101);
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), AcpError::SessionBusy(_)));
    }

    #[test]
    fn try_begin_prompt_not_found() {
        let mut mgr = SessionManager::new();
        let result = mgr.try_begin_prompt("nonexistent", 1);
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), AcpError::SessionNotFound(_)));
    }

    #[test]
    fn end_prompt_clears_active() {
        let mut mgr = SessionManager::new();
        let acp_id = mgr.create(1);
        mgr.try_begin_prompt(&acp_id, 100).unwrap();
        assert!(mgr.is_prompt_active(&acp_id));
        mgr.end_prompt(&acp_id);
        assert!(!mgr.is_prompt_active(&acp_id));
        // Ending a prompt with no active request is a no-op.
        mgr.end_prompt(&acp_id);
    }

    #[test]
    fn next_request_id_increments() {
        let mut mgr = SessionManager::new();
        let id1 = mgr.next_request_id();
        let id2 = mgr.next_request_id();
        assert_eq!(id2, id1 + 1);
    }

    #[test]
    fn next_request_id_wraps() {
        let mut mgr = SessionManager::new();
        mgr.next_request_id = u32::MAX;
        let id1 = mgr.next_request_id();
        let id2 = mgr.next_request_id();
        assert_eq!(id1, u32::MAX);
        assert_eq!(id2, 0);
    }
}
