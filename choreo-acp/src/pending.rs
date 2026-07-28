use std::collections::HashMap;

/// Identifies the kind of synchronous daemon response we're waiting for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PendingKind {
    CreateSession,
    ListSessions,
    DeleteSession(u64),
    SetModel,
    SetReasoningEffort,
}

/// A pending synchronous request — the event loop sent a `ClientMessage`
/// and is waiting for the matching `DaemonMessage` before it can write the
/// JSON-RPC response to the editor.
#[derive(Debug)]
pub struct PendingEntry {
    pub jsonrpc_id: u64,
    pub kind: PendingKind,
}

/// Tracks an active streaming prompt (`session/prompt`).
#[derive(Debug)]
pub struct ActivePrompt {
    pub jsonrpc_id: u64,
    pub daemon_request_id: u32,
    pub session_acp_id: String,
}

/// What a pending `Models` response from the daemon is expected for.
///
/// The daemon broadcasts `Models` after both `ListModels` and `SetModel`.
/// A dedicated enum ensures the response is routed to the correct handler
/// without relying on implicit ordering or side effects.
#[derive(Debug)]
pub enum ModelsPending {
    CreateSession {
        jsonrpc_id: u64,
        account_name: Option<String>,
    },
}

impl ModelsPending {
    pub fn jsonrpc_id(&self) -> u64 {
        match self {
            ModelsPending::CreateSession { jsonrpc_id, .. } => *jsonrpc_id,
        }
    }
}

/// Manages all in-flight requests for the main event loop.
///
/// Because the event loop is single-threaded and all I/O is blocking, we
/// can use a simple HashMap keyed by `PendingKind` for synchronous requests
/// and another HashMap keyed by session ID for streaming prompts.
#[derive(Debug)]
pub struct PendingRequests {
    pub sync: HashMap<PendingKind, PendingEntry>,
    pub prompts: HashMap<String, ActivePrompt>,
    /// Tracks what a pending `Models` response is expected for.
    /// `ListModels` (for `session/new`) and `SetModel` both produce a
    /// `Models` response from the daemon.  This slot routes it to the
    /// correct continuation without relying on the sync HashMap (which
    /// `SetModel` also uses for `ModelSelectionFailed`).
    pub models_pending: Option<ModelsPending>,
    /// Maps `PendingKind::SetModel` / `SetReasoningEffort` to the ACP session
    /// ID so that state is only applied when the daemon confirms the change.
    pending_sessions: HashMap<PendingKind, String>,
}

impl Default for PendingRequests {
    fn default() -> Self {
        Self::new()
    }
}

impl PendingRequests {
    pub fn new() -> Self {
        Self {
            sync: HashMap::new(),
            prompts: HashMap::new(),
            models_pending: None,
            pending_sessions: HashMap::new(),
        }
    }

    /// Record that we are waiting for a `Models` response for the
    /// `session/new` handshake (the next step is `CreateSession`).
    pub fn set_models_pending(&mut self, pending: ModelsPending) {
        self.models_pending = Some(pending);
    }

    /// Take the pending `Models` routing state.
    pub fn take_models_pending(&mut self) -> Option<ModelsPending> {
        self.models_pending.take()
    }

    /// Record which session a config change is for so it can be applied
    /// when the daemon confirms (via `Models` / `ReasoningEffortSet`).
    pub fn store_pending_session(&mut self, kind: PendingKind, session_id: String) {
        self.pending_sessions.insert(kind, session_id);
    }

    /// Take and return the session ID for a pending config change.
    pub fn take_pending_session(&mut self, kind: &PendingKind) -> Option<String> {
        self.pending_sessions.remove(kind)
    }

    pub fn insert_sync(&mut self, kind: PendingKind, jsonrpc_id: u64) {
        tracing::debug!(?kind, jsonrpc_id, "registering pending sync request");
        self.sync.insert(kind, PendingEntry { jsonrpc_id, kind });
    }

    pub fn take_sync(&mut self, kind: &PendingKind) -> Option<PendingEntry> {
        let entry = self.sync.remove(kind);
        if let Some(ref e) = entry {
            tracing::debug!(
                ?kind,
                jsonrpc_id = e.jsonrpc_id,
                "resolved pending sync request"
            );
        }
        entry
    }

    pub fn insert_prompt(&mut self, session_id: &str, prompt: ActivePrompt) {
        tracing::debug!(session_id, "registering active prompt");
        self.prompts.insert(session_id.to_string(), prompt);
    }

    pub fn take_prompt(&mut self, session_id: &str) -> Option<ActivePrompt> {
        let prompt = self.prompts.remove(session_id);
        if prompt.is_some() {
            tracing::debug!(session_id, "cleared active prompt");
        }
        prompt
    }

    pub fn get_prompt(&self, session_id: &str) -> Option<&ActivePrompt> {
        self.prompts.get(session_id)
    }

    /// Find an active prompt by daemon request ID.  This is needed because
    /// streaming `DaemonMessage` values carry `request_id` but not the
    /// session ID, and we need to map back to the ACP session.
    pub fn find_by_request_id(&self, daemon_request_id: u32) -> Option<&ActivePrompt> {
        self.prompts
            .values()
            .find(|p| p.daemon_request_id == daemon_request_id)
    }

    /// Drain all active prompts (called when daemon disconnects).
    /// Returns the list of (session_id, jsonrpc_id) that need error responses.
    pub fn drain_prompts(&mut self) -> Vec<(String, u64)> {
        self.prompts
            .drain()
            .map(|(sid, p)| (sid, p.jsonrpc_id))
            .collect()
    }

    /// Drain all pending sync requests.
    pub fn drain_sync(&mut self) -> Vec<PendingEntry> {
        self.sync.drain().map(|(_, v)| v).collect()
    }

    /// Returns true if no synchronous or streaming request is in flight,
    /// and no `Models` response is pending.
    pub fn is_idle(&self) -> bool {
        self.sync.is_empty() && self.prompts.is_empty() && self.models_pending.is_none()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_is_idle() {
        let p = PendingRequests::new();
        assert!(p.is_idle());
        assert!(p.sync.is_empty());
        assert!(p.prompts.is_empty());
        assert!(p.models_pending.is_none());
    }

    #[test]
    fn set_and_take_models_pending_create_session() {
        let mut p = PendingRequests::new();
        p.set_models_pending(ModelsPending::CreateSession {
            jsonrpc_id: 42,
            account_name: Some("alice".into()),
        });
        assert!(!p.is_idle());
        let taken = p.take_models_pending().unwrap();
        assert_eq!(taken.jsonrpc_id(), 42);
        match taken {
            ModelsPending::CreateSession { account_name, .. } => {
                assert_eq!(account_name, Some("alice".into()));
            }
        }
        assert!(p.is_idle());
    }

    #[test]
    fn set_and_take_models_pending_no_account() {
        let mut p = PendingRequests::new();
        p.set_models_pending(ModelsPending::CreateSession {
            jsonrpc_id: 7,
            account_name: None,
        });
        let taken = p.take_models_pending().unwrap();
        assert_eq!(taken.jsonrpc_id(), 7);
        match taken {
            ModelsPending::CreateSession { account_name, .. } => {
                assert!(account_name.is_none());
            }
        }
    }

    #[test]
    fn take_models_pending_none_when_empty() {
        let mut p = PendingRequests::new();
        assert!(p.take_models_pending().is_none());
    }

    #[test]
    fn insert_and_take_sync() {
        let mut p = PendingRequests::new();
        p.insert_sync(PendingKind::CreateSession, 10);
        assert!(!p.is_idle());
        let entry = p.take_sync(&PendingKind::CreateSession).unwrap();
        assert_eq!(entry.jsonrpc_id, 10);
        assert!(matches!(entry.kind, PendingKind::CreateSession));
        assert!(p.is_idle());
    }

    #[test]
    fn take_sync_wrong_kind_returns_none() {
        let mut p = PendingRequests::new();
        p.insert_sync(PendingKind::ListSessions, 1);
        assert!(p.take_sync(&PendingKind::CreateSession).is_none());
        // Original still present
        let entry = p.take_sync(&PendingKind::ListSessions).unwrap();
        assert_eq!(entry.jsonrpc_id, 1);
    }

    #[test]
    fn take_sync_none_when_empty() {
        let mut p = PendingRequests::new();
        assert!(p.take_sync(&PendingKind::SetModel).is_none());
    }

    #[test]
    fn insert_and_take_prompt() {
        let mut p = PendingRequests::new();
        let prompt = ActivePrompt {
            jsonrpc_id: 100,
            daemon_request_id: 5,
            session_acp_id: "sess_test".into(),
        };
        p.insert_prompt("sess_test", prompt);
        assert!(!p.is_idle());
        let taken = p.take_prompt("sess_test").unwrap();
        assert_eq!(taken.jsonrpc_id, 100);
        assert_eq!(taken.daemon_request_id, 5);
        assert_eq!(taken.session_acp_id, "sess_test");
        assert!(p.is_idle());
    }

    #[test]
    fn get_prompt_borrow() {
        let mut p = PendingRequests::new();
        p.insert_prompt(
            "sess_a",
            ActivePrompt {
                jsonrpc_id: 1,
                daemon_request_id: 10,
                session_acp_id: "sess_a".into(),
            },
        );
        let borrowed = p.get_prompt("sess_a").unwrap();
        assert_eq!(borrowed.jsonrpc_id, 1);
        // Still present after borrow
        let taken = p.take_prompt("sess_a").unwrap();
        assert_eq!(taken.daemon_request_id, 10);
    }

    #[test]
    fn get_prompt_nonexistent() {
        let p = PendingRequests::new();
        assert!(p.get_prompt("nobody").is_none());
    }

    #[test]
    fn take_prompt_nonexistent() {
        let mut p = PendingRequests::new();
        assert!(p.take_prompt("nobody").is_none());
    }

    #[test]
    fn find_by_request_id() {
        let mut p = PendingRequests::new();
        p.insert_prompt(
            "sess_1",
            ActivePrompt {
                jsonrpc_id: 10,
                daemon_request_id: 1,
                session_acp_id: "sess_1".into(),
            },
        );
        p.insert_prompt(
            "sess_2",
            ActivePrompt {
                jsonrpc_id: 20,
                daemon_request_id: 2,
                session_acp_id: "sess_2".into(),
            },
        );
        let found = p.find_by_request_id(2).unwrap();
        assert_eq!(found.jsonrpc_id, 20);
        assert_eq!(found.session_acp_id, "sess_2");
    }

    #[test]
    fn find_by_request_id_not_found() {
        let p = PendingRequests::new();
        assert!(p.find_by_request_id(999).is_none());
    }

    #[test]
    fn drain_prompts_returns_all_and_clears() {
        let mut p = PendingRequests::new();
        p.insert_prompt(
            "sess_a",
            ActivePrompt {
                jsonrpc_id: 1,
                daemon_request_id: 10,
                session_acp_id: "sess_a".into(),
            },
        );
        p.insert_prompt(
            "sess_b",
            ActivePrompt {
                jsonrpc_id: 2,
                daemon_request_id: 20,
                session_acp_id: "sess_b".into(),
            },
        );
        let drained = p.drain_prompts();
        assert_eq!(drained.len(), 2);
        assert!(drained.contains(&("sess_a".into(), 1)));
        assert!(drained.contains(&("sess_b".into(), 2)));
        assert!(p.prompts.is_empty());
    }

    #[test]
    fn drain_prompts_empty() {
        let mut p = PendingRequests::new();
        let drained = p.drain_prompts();
        assert!(drained.is_empty());
    }

    #[test]
    fn delete_session_matches_by_daemon_id() {
        assert!(matches!(
            PendingKind::DeleteSession(5),
            PendingKind::DeleteSession(5)
        ));
        assert!(!matches!(
            PendingKind::DeleteSession(5),
            PendingKind::DeleteSession(6)
        ));
    }

    #[test]
    fn is_idle_with_sync_pending() {
        let mut p = PendingRequests::new();
        p.insert_sync(PendingKind::CreateSession, 1);
        assert!(!p.is_idle());
    }

    #[test]
    fn is_idle_with_prompt_pending() {
        let mut p = PendingRequests::new();
        p.insert_prompt(
            "sess_x",
            ActivePrompt {
                jsonrpc_id: 1,
                daemon_request_id: 1,
                session_acp_id: "sess_x".into(),
            },
        );
        assert!(!p.is_idle());
    }

    #[test]
    fn is_idle_with_models_pending() {
        let mut p = PendingRequests::new();
        p.set_models_pending(ModelsPending::CreateSession {
            jsonrpc_id: 1,
            account_name: None,
        });
        assert!(!p.is_idle());
    }
}
