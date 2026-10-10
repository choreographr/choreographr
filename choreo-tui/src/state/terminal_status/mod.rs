//! Terminal-status publishing: the OSC 7501 program-status records and the
//! OSC 2 window title derived from the app's session state.
//!
//! The `terminal` module is a leaf that only frames and writes sequences; the
//! orchestration that needs `App` — which sessions to report, the attached
//! session's live status, the persisted terminal outcome that survives a
//! finished turn — lives here as `impl App` methods. The connection loop calls
//! them and owns the diff against the emitted records plus the actual writes.
//!
//! The `App` fields this module reads (`term_status_override`,
//! `attached_status`, `session_mgr`, …) stay on the struct in `state/`.

use choreo_proto::SessionStatus;

use super::App;
use crate::terminal;

/// The tool NAME of a `ToolCall` status (never its arguments), used as an
/// OSC 7501 `msg`. [`crate::terminal::status::record`] strips control
/// characters and caps the length, so a hostile tool name can never make the
/// terminal discard the whole record.
fn tool_call_name(status: &SessionStatus) -> Option<&str> {
    match status {
        SessionStatus::ToolCall(name) => Some(name.as_str()),
        _ => None,
    }
}

impl App {
    /// The current OSC 2 window title: the plain program name when no titled
    /// session is attached, else `choreo-tui — <attached session title>`.
    pub(crate) fn window_title(&self) -> String {
        let title = self
            .attached_session_id
            .and_then(|id| self.session_title(id));
        terminal::title::window_title(title)
    }

    /// The OSC 7501 child records to publish right now, keyed by session id.
    ///
    /// Every record is a CHILD record (`id=<session_id>`); the OSC 9;4 root
    /// record is owned by the progress family. The attached session is always
    /// present, and every background session that is active — or that carries
    /// a persisted terminal outcome (`term_status_override`) — is added. The
    /// outcome case is what lets a background agent that just finished keep
    /// its `done` record after its live status drops to `Inactive` (the
    /// protocol's "which agent needs me" use case); an idle/sleeping
    /// background session with no outcome is omitted so a stale `working`
    /// record is cleared.
    pub(crate) fn desired_status_records(&self) -> Vec<(u64, String)> {
        let mut desired: Vec<(u64, String)> = Vec::new();
        if let Some(id) = self.attached_session_id {
            desired.push((id, self.status_record(id, self.attached_status.as_ref())));
        }
        for summary in &self.session_mgr.all {
            let id = summary.session_id;
            if Some(id) == self.attached_session_id {
                continue;
            }
            if !summary.status.is_active() && !self.term_status_override.contains_key(&id) {
                continue;
            }
            desired.push((id, self.status_record(id, Some(&summary.status))));
        }
        desired
    }

    /// Build the OSC 7501 child record for `id`, given its live status (the
    /// attached session passes `attached_status`; a background session passes
    /// its summary status). A persisted terminal outcome
    /// (`term_status_override`) wins over the live state and suppresses the
    /// tool `msg` — a finished/cancelled turn has no tool to name.
    fn status_record(&self, id: u64, live: Option<&SessionStatus>) -> String {
        let override_state = self.term_status_override.get(&id).copied();
        let state = override_state
            .or_else(|| live.map(terminal::status::state_for))
            .unwrap_or("idle");
        let msg = if override_state.is_some() {
            None
        } else {
            live.and_then(tool_call_name)
        };
        let title = self.session_title(id);
        terminal::status::record(id, terminal::status::APP, state, title, msg)
    }

    /// The raw title of the session `id`, if it has one.
    ///
    /// Borrowed rather than cloned — the caller only reads it. Sanitization
    /// and capping happen at each consumer's boundary — `terminal::title` (200
    /// chars) for OSC 2, `terminal::status` (192 bytes) for the record's
    /// `title` — because the two have different limits.
    fn session_title(&self, id: u64) -> Option<&str> {
        self.session_mgr
            .all
            .iter()
            .find(|s| s.session_id == id)
            .and_then(|s| s.title.as_deref())
    }
}

#[cfg(test)]
mod tests;
