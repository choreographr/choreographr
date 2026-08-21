use choreo_proto::{
    DaemonMessage, OutputStream, ReasoningCapability, SessionEvent, SessionStatus, TokenUsage, Turn,
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
        /// Human-readable invocation description from `ToolCallStarted` (e.g.
        /// "Running command: `cargo build`.") so clients can render the tool's
        /// context immediately, before any streaming output arrives.
        invocation_description: String,
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
    fn handle_turn_appended(&mut self, session_id: u64, turn_id: u32, turn: Turn);
    fn handle_turns_undone(&mut self, session_id: u64, turn_ids: &[u32]);
    fn handle_turns_redone(&mut self, session_id: u64, turns: BTreeMap<u32, Turn>);
    fn handle_request_stream(
        &mut self,
        session_id: u64,
        request_id: u32,
        stream: OutputStream,
        data: Cow<'_, str>,
    );
    fn handle_started(
        &mut self,
        session_id: u64,
        request_id: u32,
        turn_id: u32,
        estimated_prompt_tokens: u32,
    );
    fn handle_done(
        &mut self,
        session_id: u64,
        request_id: u32,
        token_usage: Option<TokenUsage>,
        last_prompt_tokens: Option<u32>,
    );
    fn handle_failed(&mut self, session_id: u64, request_id: u32, error: String);
    fn handle_tool_call_event(&mut self, session_id: u64, request_id: u32, event: ToolCallEvent);
    fn handle_tool_result_chunk(
        &mut self,
        session_id: u64,
        request_id: u32,
        call_id: String,
        data: Vec<u8>,
    );
    fn handle_session_state(&mut self, state: SessionStateData);
    fn handle_status_text(&mut self, text: String);
    fn handle_error(&mut self, error: String);
    fn handle_session_attached(&mut self, session_id: u64);
    // The parameter list mirrors the SessionCreated message fields 1:1; a
    // struct would just re-wrap fields the dispatcher already destructures.
    #[allow(clippy::too_many_arguments)]
    fn handle_session_created(
        &mut self,
        session_id: u64,
        title: Option<String>,
        working_dir: Option<String>,
        account_name: Option<String>,
        selected_model: Option<String>,
        reasoning_effort: Option<String>,
    );
    fn handle_session_status_changed(
        &mut self,
        session_id: u64,
        status: SessionStatus,
        last_modified: i64,
    );
    fn handle_token_usage_update(
        &mut self,
        session_id: u64,
        token_usage: TokenUsage,
        last_prompt_tokens: Option<u32>,
    );
}

pub fn dispatch_daemon_message(msg: &DaemonMessage, handler: &mut impl TurnEventHandler) {
    debug!("dispatching daemon message: {msg:?}");

    // Session-scoped events all ride the `DaemonMessage::Session` envelope,
    // which hoists the origin session id, so `session_id` is destructured
    // exactly once here rather than per-arm. Non-session (flat) messages take
    // the else branch: the rest of the dispatch below handles them and
    // returns, so the session-event match below only ever sees the envelope.
    let DaemonMessage::Session { session_id, event } = msg else {
        match msg {
            DaemonMessage::Sessions { .. } => {
                // Handled upstream by the caller before dispatch.
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
            DaemonMessage::Unlocked => {
                handler.handle_status_text(
                    "[daemon] keystore unlocked, credentials available".to_string(),
                );
            }
            DaemonMessage::Locked => {
                handler.handle_status_text(
                    "[daemon] keystore locked, credentials cleared".to_string(),
                );
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
            _ => {
                debug!("unhandled daemon message variant");
            }
        }
        return;
    };

    match event {
        SessionEvent::SessionCreated {
            title,
            working_dir,
            account_name,
            selected_model,
            reasoning_effort,
            ..
        } => {
            handler.handle_session_created(
                *session_id,
                title.clone(),
                working_dir.clone(),
                account_name.clone(),
                selected_model.clone(),
                reasoning_effort.clone(),
            );
        }
        SessionEvent::SessionAttached => {
            handler.handle_session_attached(*session_id);
        }
        SessionEvent::SessionState {
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
        SessionEvent::TurnAppended { turn_id, turn } => {
            handler.handle_turn_appended(*session_id, *turn_id, turn.clone())
        }
        SessionEvent::TurnsUndone { turn_ids } => {
            handler.handle_turns_undone(*session_id, turn_ids)
        }
        SessionEvent::TurnsRedone { turns } => {
            handler.handle_turns_redone(*session_id, turns.clone())
        }
        SessionEvent::Started {
            request_id,
            turn_id,
            estimated_prompt_tokens,
        } => handler.handle_started(*session_id, *request_id, *turn_id, *estimated_prompt_tokens),
        SessionEvent::OutputChunk {
            request_id,
            stream,
            data,
        } => handler.handle_request_stream(
            *session_id,
            *request_id,
            stream.clone(),
            String::from_utf8_lossy(data),
        ),
        SessionEvent::ToolCallStarted {
            request_id,
            call_id,
            tool_name,
            arguments_json,
            invocation_description,
        } => handler.handle_tool_call_event(
            *session_id,
            *request_id,
            ToolCallEvent::Started {
                call_id: call_id.clone(),
                tool_name: tool_name.clone(),
                arguments_json: arguments_json.clone(),
                invocation_description: invocation_description.clone(),
            },
        ),
        SessionEvent::ToolCallFinished {
            request_id,
            call_id,
            tool_name,
        } => handler.handle_tool_call_event(
            *session_id,
            *request_id,
            ToolCallEvent::Finished {
                call_id: call_id.clone(),
                tool_name: tool_name.clone(),
            },
        ),
        SessionEvent::ToolCallFailed {
            request_id,
            call_id,
            tool_name,
            error,
        } => handler.handle_tool_call_event(
            *session_id,
            *request_id,
            ToolCallEvent::Failed {
                call_id: call_id.clone(),
                tool_name: tool_name.clone(),
                error: error.clone(),
            },
        ),
        SessionEvent::ToolResultChunk {
            request_id,
            call_id,
            data,
        } => handler.handle_tool_result_chunk(
            *session_id,
            *request_id,
            call_id.clone(),
            data.clone(),
        ),
        SessionEvent::Done {
            request_id,
            token_usage,
            last_prompt_tokens,
        } => handler.handle_done(*session_id, *request_id, *token_usage, *last_prompt_tokens),
        SessionEvent::Failed { request_id, error } => {
            handler.handle_failed(*session_id, *request_id, error.clone())
        }
        SessionEvent::Cancelled { request_id } => {
            handler.handle_failed(*session_id, *request_id, "cancelled".to_string())
        }
        SessionEvent::SessionStatusChanged {
            status,
            last_modified,
        } => handler.handle_session_status_changed(*session_id, status.clone(), *last_modified),
        SessionEvent::SessionFailed { error, .. } => {
            handler.handle_error(error.clone());
        }
        SessionEvent::ModelSelected { model, .. } => {
            handler.handle_status_text(format!("[daemon] selected model: {model}"));
        }
        SessionEvent::ModelSelectionFailed { model, error } => {
            handler.handle_error(format!("[daemon] failed to select model {model}: {error}"));
        }
        SessionEvent::SessionDeleted => {}
        SessionEvent::SessionDeleteFailed { .. } => {}
        SessionEvent::SessionAccountSet { account, .. } => {
            handler.handle_status_text(format!("[daemon] session account set: {account}"));
        }
        SessionEvent::SessionWorkingDirSet { .. } => {}
        SessionEvent::SessionTitleSet { .. } => {
            // Session title changes are metadata-only (no conversation
            // content) and are handled at the TUI layer directly via
            // the connection.rs routing — no generic dispatch needed.
        }
        SessionEvent::ReasoningEffortSet { effort, .. } => {
            handler.handle_status_text(format!("[daemon] reasoning effort: {effort}"));
        }
        SessionEvent::ReasoningEffortSetFailed { effort, error, .. } => {
            handler.handle_error(format!(
                "[daemon] failed to set reasoning effort {effort}: {error}"
            ));
        }
        SessionEvent::TokenUsageUpdate {
            token_usage,
            last_prompt_tokens,
        } => handler.handle_token_usage_update(*session_id, *token_usage, *last_prompt_tokens),
        SessionEvent::LiveOutputTokenCount { .. } => {
            // Handled at the TUI layer in connection.rs — no generic dispatch needed.
        }
        SessionEvent::ContextWindowResolved { .. } => {
            // Context-window resolution is metadata-only (no conversation
            // content) and is handled at the TUI layer in connection.rs — no
            // generic dispatch needed.
        }
    }
}
