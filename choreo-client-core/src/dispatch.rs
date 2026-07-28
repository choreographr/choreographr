use choreo_proto::{
    DaemonMessage, OutputStream, ReasoningCapability, SessionStatus, TokenUsage, Turn,
};
use std::borrow::Cow;
use std::collections::BTreeMap;
use tracing::debug;

#[derive(Debug, Clone)]
pub enum ToolCallEvent {
    Started {
        call_id: String,
        tool_name: String,
        arguments_json: String,
    },
    Finished {
        call_id: String,
        tool_name: String,
    },
    Failed {
        call_id: String,
        tool_name: String,
        error: String,
    },
}

/// Grouped payload for [`TurnEventHandler::handle_session_state`].
#[derive(Debug, Clone)]
pub struct SessionStateData {
    pub session_id: u64,
    pub turns: BTreeMap<u32, Turn>,
    pub title: Option<String>,
    pub selected_model: Option<String>,
    pub active_tool_groups: Vec<String>,
    pub token_usage: Option<TokenUsage>,
    pub context_window: Option<u32>,
    pub last_prompt_tokens: Option<u32>,
    pub status: SessionStatus,
    pub reasoning_effort: Option<String>,
    pub reasoning_capability: Option<ReasoningCapability>,
}

pub trait TurnEventHandler {
    fn handle_turn_appended(&mut self, turn_id: u32, turn: Turn);
    fn handle_turn_finalized(&mut self, turn_id: u32, turn: Turn);
    fn handle_turns_undone(&mut self, turn_ids: &[u32]);
    fn handle_turns_redone(&mut self, turns: BTreeMap<u32, Turn>);
    fn handle_request_stream(&mut self, request_id: u32, stream: OutputStream, data: Cow<'_, str>);
    fn handle_started(&mut self, request_id: u32, turn_id: u32, estimated_prompt_tokens: u32);
    fn handle_done(
        &mut self,
        request_id: u32,
        token_usage: Option<TokenUsage>,
        last_prompt_tokens: Option<u32>,
    );
    fn handle_failed(&mut self, request_id: u32, error: String);
    fn handle_tool_call_event(&mut self, request_id: u32, event: ToolCallEvent);
    fn handle_tool_result_chunk(&mut self, request_id: u32, call_id: String, data: Vec<u8>);
    fn handle_session_state(&mut self, state: SessionStateData);
    fn handle_status_text(&mut self, text: String);
    fn handle_error(&mut self, error: String);
    fn handle_session_attached(&mut self, session_id: u64);
    fn handle_session_created(
        &mut self,
        session_id: u64,
        title: Option<String>,
        working_dir: Option<String>,
        max_turns: Option<u32>,
    );
    fn handle_session_status_changed(&mut self, session_id: u64, status: SessionStatus);
    fn handle_token_usage_update(
        &mut self,
        token_usage: TokenUsage,
        last_prompt_tokens: Option<u32>,
    );
}

pub fn dispatch_daemon_message(msg: &DaemonMessage, handler: &mut impl TurnEventHandler) {
    debug!("dispatching daemon message: {msg:?}");
    match msg {
        DaemonMessage::SessionCreated {
            session_id,
            title,
            working_dir,
            max_turns,
            ..
        } => {
            handler.handle_session_created(
                *session_id,
                title.clone(),
                working_dir.clone(),
                *max_turns,
            );
        }
        DaemonMessage::Sessions { .. } => {
            // Handled upstream by the caller before dispatch.
        }
        DaemonMessage::SessionAttached { session_id } => {
            handler.handle_session_attached(*session_id);
        }
        DaemonMessage::SessionState {
            session_id,
            title,
            selected_model,
            turns,
            active_tool_groups,
            token_usage,
            context_window,
            last_prompt_tokens,
            status,
            reasoning_effort,
            reasoning_capability,
            ..
        } => {
            handler.handle_session_state(SessionStateData {
                session_id: *session_id,
                turns: turns.clone(),
                title: title.clone(),
                selected_model: selected_model.clone(),
                active_tool_groups: active_tool_groups.clone(),
                token_usage: *token_usage,
                context_window: *context_window,
                last_prompt_tokens: *last_prompt_tokens,
                status: status.clone(),
                reasoning_effort: reasoning_effort.clone(),
                reasoning_capability: reasoning_capability.clone(),
            });
        }
        DaemonMessage::TurnAppended { turn_id, turn } => {
            handler.handle_turn_appended(*turn_id, turn.clone());
        }
        DaemonMessage::TurnFinalized { turn_id, turn } => {
            handler.handle_turn_finalized(*turn_id, turn.clone());
        }
        DaemonMessage::TurnsUndone { turn_ids } => {
            handler.handle_turns_undone(turn_ids);
        }
        DaemonMessage::TurnsRedone { turns } => {
            handler.handle_turns_redone(turns.clone());
        }
        DaemonMessage::Started {
            request_id,
            turn_id,
            estimated_prompt_tokens,
        } => {
            handler.handle_started(*request_id, *turn_id, *estimated_prompt_tokens);
        }
        DaemonMessage::OutputChunk {
            request_id,
            stream,
            data,
        } => {
            handler.handle_request_stream(
                *request_id,
                stream.clone(),
                String::from_utf8_lossy(data),
            );
        }
        DaemonMessage::ToolCallStarted {
            request_id,
            call_id,
            tool_name,
            arguments_json,
        } => {
            handler.handle_tool_call_event(
                *request_id,
                ToolCallEvent::Started {
                    call_id: call_id.clone(),
                    tool_name: tool_name.clone(),
                    arguments_json: arguments_json.clone(),
                },
            );
        }
        DaemonMessage::ToolCallFinished {
            request_id,
            call_id,
            tool_name,
        } => {
            handler.handle_tool_call_event(
                *request_id,
                ToolCallEvent::Finished {
                    call_id: call_id.clone(),
                    tool_name: tool_name.clone(),
                },
            );
        }
        DaemonMessage::ToolCallFailed {
            request_id,
            call_id,
            tool_name,
            error,
        } => {
            handler.handle_tool_call_event(
                *request_id,
                ToolCallEvent::Failed {
                    call_id: call_id.clone(),
                    tool_name: tool_name.clone(),
                    error: error.clone(),
                },
            );
        }
        DaemonMessage::ToolResultChunk {
            request_id,
            call_id,
            data,
        } => {
            handler.handle_tool_result_chunk(*request_id, call_id.clone(), data.clone());
        }
        DaemonMessage::Done {
            request_id,
            token_usage,
            last_prompt_tokens,
        } => {
            handler.handle_done(*request_id, *token_usage, *last_prompt_tokens);
        }
        DaemonMessage::Failed { request_id, error } => {
            handler.handle_failed(*request_id, error.clone());
        }
        DaemonMessage::Cancelled { request_id } => {
            handler.handle_failed(*request_id, "cancelled".to_string());
        }
        DaemonMessage::SessionStatusChanged { session_id, status } => {
            handler.handle_session_status_changed(*session_id, status.clone());
        }
        DaemonMessage::SessionFailed { error, .. } => {
            handler.handle_error(error.clone());
        }
        DaemonMessage::Pong => {
            handler.handle_status_text("[daemon] pong".to_string());
        }
        DaemonMessage::ShuttingDown => {
            handler.handle_status_text("[daemon] shutting down".to_string());
        }
        DaemonMessage::Models {
            models,
            selected_model,
        } => {
            if models.is_empty() {
                handler.handle_status_text("[daemon] no models available".to_string());
            } else {
                let mut lines = vec![format!("[daemon] supported models ({})", models.len())];
                for model in models {
                    let prefix = if selected_model.as_deref() == Some(model.as_str()) {
                        "*"
                    } else {
                        "-"
                    };
                    lines.push(format!("{prefix} {model}"));
                }
                handler.handle_status_text(lines.join("\n"));
            }
        }
        DaemonMessage::ModelsFailed { error } => {
            handler.handle_error(format!("[daemon] models failed: {error}"));
        }
        DaemonMessage::ModelSelected {
            model,
            reasoning_capability: _,
        } => {
            handler.handle_status_text(format!("[daemon] selected model: {model}"));
        }
        DaemonMessage::ModelSelectionFailed { model, error } => {
            handler.handle_error(format!("[daemon] failed to select model {model}: {error}"));
        }
        DaemonMessage::Unlocked => {
            handler.handle_status_text(
                "[daemon] keystore unlocked, credentials available".to_string(),
            );
        }
        DaemonMessage::Locked => {
            handler.handle_status_text("[daemon] keystore locked, credentials cleared".to_string());
        }
        DaemonMessage::LockedError { error } => {
            handler.handle_error(format!("[daemon] locked: {error}"));
        }
        DaemonMessage::CredentialAdded { service } => {
            handler.handle_status_text(format!("[daemon] credential added: {service}"));
        }
        DaemonMessage::CredentialAddFailed { service, error } => {
            handler.handle_error(format!(
                "[daemon] credential add failed ({service}): {error}"
            ));
        }
        DaemonMessage::CredentialRemoved { service } => {
            handler.handle_status_text(format!("[daemon] credential removed: {service}"));
        }
        DaemonMessage::CredentialRemoveFailed { service, error } => {
            handler.handle_error(format!(
                "[daemon] credential remove failed ({service}): {error}"
            ));
        }
        DaemonMessage::Credential { .. } => {}
        DaemonMessage::SessionDeleted { .. } => {}
        DaemonMessage::SessionDeleteFailed { .. } => {}
        DaemonMessage::AccountAdded { name } => {
            handler.handle_status_text(format!("[daemon] account added: {name}"));
        }
        DaemonMessage::AccountAddFailed { name, error } => {
            handler.handle_error(format!("[daemon] failed to add account {name}: {error}"));
        }
        DaemonMessage::AccountRemoved { name } => {
            handler.handle_status_text(format!("[daemon] account removed: {name}"));
        }
        DaemonMessage::AccountRemoveFailed { name, error } => {
            handler.handle_error(format!("[daemon] failed to remove account {name}: {error}"));
        }
        DaemonMessage::Accounts { accounts } => {
            if accounts.is_empty() {
                handler.handle_status_text("[daemon] no accounts configured".to_string());
            } else {
                let mut lines = vec![format!("[daemon] accounts ({})", accounts.len())];
                for a in accounts {
                    lines.push(format!("  {}: {}", a.name, a.provider));
                }
                handler.handle_status_text(lines.join("\n"));
            }
        }
        DaemonMessage::AccountListFailed { error } => {
            handler.handle_error(format!("[daemon] failed to list accounts: {error}"));
        }
        DaemonMessage::SessionAccountSet { account } => {
            handler.handle_status_text(format!("[daemon] session account set: {account}"));
        }
        DaemonMessage::SessionWorkingDirSet { .. } => {}
        DaemonMessage::ReasoningEffortSet { effort } => {
            handler.handle_status_text(format!("[daemon] reasoning effort: {effort}"));
        }
        DaemonMessage::ReasoningEffortSetFailed { effort, error } => {
            handler.handle_error(format!(
                "[daemon] failed to set reasoning effort {effort}: {error}"
            ));
        }
        DaemonMessage::TokenUsageUpdate {
            token_usage,
            last_prompt_tokens,
        } => {
            handler.handle_token_usage_update(*token_usage, *last_prompt_tokens);
        }
        DaemonMessage::LiveOutputTokenCount { .. } => {
            // Handled at the TUI layer in connection.rs — no generic dispatch needed.
        }
        _ => {
            debug!("unhandled daemon message variant");
        }
    }
}
