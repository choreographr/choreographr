use choreo_client_core::{SessionStateData, SessionView, ToolCallEvent, TurnEventHandler};
use choreo_proto::{OutputStream, SessionStatus, TokenUsage, Turn};
use std::borrow::Cow;
use std::collections::BTreeMap;
use tracing::{debug, trace, warn};

#[expect(clippy::large_enum_variant)]
#[derive(Debug, Clone)]
pub(crate) enum UiEvent {
    Daemon(choreo_proto::DaemonMessage),
    ReaderClosed,
    ReaderFailed(String),
}

#[derive(Debug, Clone)]
pub(crate) struct AppState {
    pub(crate) input: String,
    pub(crate) next_request_id: u32,
    pub(crate) session_view: SessionView,
    pub(crate) status_texts: Vec<String>,
    pub(crate) pending_cancel: String,
    pub(crate) attached_session_id: Option<u64>,
    /// The strengthen-unlock key sent in the most recent `Unlock` or
    /// `AddCredential`, held until the daemon CONFIRMS it (an `Unlocked` or
    /// `CredentialAdded` reply) and then recorded via
    /// [`choreo_client_core::record_unlock_key`]. Never persisted on send —
    /// only on confirmed success. Cleared once recorded (or on any send).
    pub(crate) pending_unlock_key: Option<Vec<u8>>,
}

impl AppState {
    pub(crate) fn new(socket_path: String) -> Self {
        Self {
            input: String::new(),
            next_request_id: 1,
            session_view: SessionView::new(),
            status_texts: vec![format!("Connected to Choreographr at {socket_path}")],
            pending_cancel: String::new(),
            attached_session_id: None,
            pending_unlock_key: None,
        }
    }
}

impl TurnEventHandler for AppState {
    fn handle_turn_appended(&mut self, _session_id: u64, turn_id: u32, turn: Turn) {
        debug!(%turn_id, "turn appended");
        self.session_view.insert_or_replace(turn_id, turn);
    }

    fn handle_turns_undone(&mut self, _session_id: u64, turn_ids: &[u32]) {
        trace!(?turn_ids, "handle_turns_undone");
        for &id in turn_ids {
            if let Some(turn) = self.session_view.turns.get_mut(&id) {
                turn.undone = true;
            }
        }
    }

    fn handle_turns_redone(&mut self, _session_id: u64, turns: BTreeMap<u32, Turn>) {
        trace!(count = %turns.len(), "handle_turns_redone");
        for (id, turn) in turns {
            self.session_view.insert_or_replace(id, turn);
        }
    }

    fn handle_request_stream(
        &mut self,
        _session_id: u64,
        request_id: u32,
        stream: OutputStream,
        data: Cow<'_, str>,
    ) {
        trace!(%request_id, ?stream, len = %data.len(), "handle_request_stream");
        self.session_view.stream_chunk(request_id, stream, &data);
    }

    fn handle_started(
        &mut self,
        _session_id: u64,
        request_id: u32,
        turn_id: u32,
        _estimated_prompt_tokens: u32,
    ) {
        debug!(%request_id, %turn_id, "stream started");
        self.session_view
            .request_to_turn
            .insert(request_id, turn_id);
    }

    fn handle_done(
        &mut self,
        _session_id: u64,
        request_id: u32,
        _token_usage: Option<TokenUsage>,
        _last_prompt_tokens: Option<u32>,
    ) {
        trace!(%request_id, "handle_done");
        // The final TurnAppended usually cleaned description entries via
        // `insert_or_replace`; clear for this turn anyway (before the
        // request→turn mapping is removed) so a dropped final broadcast
        // can't leak them.
        if let Some(&turn_id) = self.session_view.request_to_turn.get(&request_id) {
            self.session_view.clear_tool_call_descriptions(turn_id);
        }
        self.session_view.request_to_turn.remove(&request_id);
    }

    fn handle_failed(&mut self, _session_id: Option<u64>, request_id: u32, error: String) {
        trace!(%request_id, %error, "handle_failed");
        // A failed request never re-broadcasts its turn, so `insert_or_replace`
        // won't clean the description map — clear it here (before the
        // request→turn mapping is removed) to keep the map bounded by
        // in-flight calls even on the failure path.
        if let Some(&turn_id) = self.session_view.request_to_turn.get(&request_id) {
            self.session_view.clear_tool_call_descriptions(turn_id);
        }
        self.session_view.request_to_turn.remove(&request_id);
        self.status_texts.push(format!("[error] {error}"));
    }

    fn handle_tool_call_event(&mut self, _session_id: u64, request_id: u32, event: ToolCallEvent) {
        trace!(%request_id, ?event, "handle_tool_call_event");
        match event {
            ToolCallEvent::Started {
                call_id,
                tool_name,
                arguments_json,
                invocation_description,
            } => {
                self.session_view.tool_call_started(
                    request_id,
                    call_id,
                    tool_name,
                    arguments_json,
                    invocation_description,
                );
            }
            ToolCallEvent::Finished { .. } | ToolCallEvent::Failed { .. } => {}
        }
    }

    fn handle_tool_result_chunk(
        &mut self,
        _session_id: u64,
        request_id: u32,
        call_id: String,
        data: Vec<u8>,
    ) {
        trace!(%request_id, %call_id, len = %data.len(), "handle_tool_result_chunk");
        match String::from_utf8(data) {
            Ok(text) => {
                self.session_view
                    .tool_result_chunk(request_id, &call_id, &text);
            }
            Err(e) => {
                warn!(%request_id, %call_id, error = %e, "non-UTF-8 tool result chunk");
            }
        }
    }

    fn handle_session_state(&mut self, state: SessionStateData) {
        debug!(session_id = %state.session_id, turn_count = %state.turns.len(), ?state.title, ?state.selected_model, ?state.status, "handle_session_state");
        self.session_view.turns = state.turns;
    }

    fn handle_status_text(&mut self, text: String) {
        self.status_texts.push(text);
    }

    fn handle_error(&mut self, error: String) {
        self.status_texts.push(format!("[error] {error}"));
    }

    fn handle_session_attached(&mut self, session_id: u64) {
        debug!(session_id, "handle_session_attached");
        self.attached_session_id = Some(session_id);
    }

    fn handle_session_created(
        &mut self,
        session_id: u64,
        title: Option<String>,
        working_dir: Option<String>,
        _account_name: Option<String>,
        _selected_model: Option<String>,
        _reasoning_effort: Option<String>,
    ) {
        debug!(session_id, ?title, ?working_dir, "handle_session_created");
        self.attached_session_id = Some(session_id);
    }

    fn handle_session_status_changed(
        &mut self,
        session_id: u64,
        status: SessionStatus,
        last_modified: i64,
    ) {
        debug!(
            session_id,
            ?status,
            last_modified,
            "handle_session_status_changed"
        );
    }

    fn handle_token_usage_update(
        &mut self,
        _session_id: u64,
        token_usage: TokenUsage,
        last_prompt_tokens: Option<u32>,
    ) {
        trace!(
            ?token_usage,
            ?last_prompt_tokens,
            "handle_token_usage_update"
        );
    }
}
