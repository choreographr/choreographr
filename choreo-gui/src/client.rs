use crate::state::{AppState, UiEvent};
use choreo_client_core::{
    ClientError, ConnectionMode, ShellCommand, build_add_credential_message,
    dispatch_daemon_message, parse_input_line, resolve_private_key,
    run_daemon_connection_with_mode, shell_command_echo,
};
use choreo_proto::{ClientMessage, DaemonMessage};
use dioxus::prelude::*;
use futures_channel::mpsc::UnboundedSender;

pub(crate) fn run_client(
    mode: ConnectionMode,
    client_rx: std::sync::mpsc::Receiver<ClientMessage>,
    ui_tx: UnboundedSender<UiEvent>,
) -> Result<(), ClientError> {
    let result = run_daemon_connection_with_mode(
        mode,
        |message| {
            if let Err(e) = ui_tx.unbounded_send(UiEvent::Daemon(message)) {
                tracing::error!("failed to send Daemon UI event: {e}");
            }
        },
        client_rx,
        None,
    );
    if result.is_ok()
        && let Err(e) = ui_tx.unbounded_send(UiEvent::ReaderClosed)
    {
        tracing::warn!("failed to send ReaderClosed UI event: {e}");
    }
    result
}

pub(crate) fn submit_input(
    state: &mut Signal<AppState>,
    daemon_tx: Option<std::sync::mpsc::Sender<ClientMessage>>,
) {
    let line = state.read().input.trim().to_string();
    state.write().input.clear();
    let command = {
        let mut guard = state.write();
        let attached = guard.attached_session_id;
        parse_input_line(&line, &mut guard.next_request_id, attached)
    };
    handle_shell_command(&mut state.write(), daemon_tx, command);
}

pub(crate) fn handle_shell_command(
    state: &mut AppState,
    daemon_tx: Option<std::sync::mpsc::Sender<ClientMessage>>,
    command: ShellCommand,
) {
    match command {
        ShellCommand::Empty => {}
        ShellCommand::InvalidCancel(value) => state
            .status_texts
            .push(format!("invalid request id: {value}")),
        ShellCommand::UnknownCommand(error) => state.status_texts.push(error),
        ShellCommand::Send(message) => send_client_message(state, daemon_tx, message),
        ShellCommand::Unlock { method } => match resolve_private_key(&method) {
            Ok(private_key) => {
                send_client_message(state, daemon_tx, ClientMessage::Unlock { private_key });
            }
            Err(e) => {
                state.status_texts.push(format!("[error] {e}"));
            }
        },
        ShellCommand::AddCredential {
            service,
            credential_type,
            fields,
            unlock,
        } => match build_add_credential_message(service, credential_type, fields, unlock) {
            Ok(msg) => send_client_message(state, daemon_tx, msg),
            Err(e) => state.status_texts.push(format!("[error] {e}")),
        },
        ShellCommand::RemoveCredential { service } => {
            send_client_message(
                state,
                daemon_tx,
                ClientMessage::RemoveCredential { service },
            );
        }
        ShellCommand::Undo => {
            send_client_message(state, daemon_tx, ClientMessage::Undo);
        }
        ShellCommand::Redo => {
            send_client_message(state, daemon_tx, ClientMessage::Redo);
        }
        ShellCommand::Continue => {
            if state.attached_session_id.is_some() {
                let request_id = state.next_request_id;
                state.next_request_id = state.next_request_id.wrapping_add(1);
                send_client_message(
                    state,
                    daemon_tx,
                    ClientMessage::ContinueGeneration { request_id },
                );
            } else {
                state.status_texts.push("no session attached".to_string());
            }
        }
        ShellCommand::Stop => {
            // Send Cancel with request_id 0 (CANCEL_ALL sentinel) to stop
            // whatever request is currently active on the attached session.
            if state.attached_session_id.is_some() {
                send_client_message(state, daemon_tx, ClientMessage::Cancel { request_id: 0 });
            } else {
                state.status_texts.push("no session attached".to_string());
            }
        }
        ShellCommand::RefreshModels { force } => {
            state.status_texts.push(if force {
                "refreshing models… (forced)".to_string()
            } else {
                "refreshing models…".to_string()
            });
            send_client_message(state, daemon_tx, ClientMessage::RefreshModels { force });
        }
    }
}

pub(crate) fn send_client_message(
    state: &mut AppState,
    daemon_tx: Option<std::sync::mpsc::Sender<ClientMessage>>,
    message: ClientMessage,
) {
    let Some(sender) = daemon_tx else {
        state
            .status_texts
            .push("[client] not connected".to_string());
        return;
    };

    if let Some(echo) = shell_command_echo(&ShellCommand::Send(message.clone())) {
        state.status_texts.push(echo);
    }

    if let Err(error) = sender.send(message) {
        state
            .status_texts
            .push(format!("[client] failed to send command: {error}"));
    }
}

/// Handles session lifecycle messages (auto-attach, session creation, attaching).
///
/// Returns `true` if the message was fully handled and should skip
/// [`dispatch_daemon_message`]. Returns `false` if dispatch should run after
/// this function.
fn handle_session_message(
    state: &mut AppState,
    daemon_tx: &Option<std::sync::mpsc::Sender<ClientMessage>>,
    message: &DaemonMessage,
) -> bool {
    match message {
        DaemonMessage::SessionCreated { session_id, .. }
        | DaemonMessage::SessionAttached { session_id } => {
            // Record the new attached session id, but let dispatch emit the
            // informational text message.
            state.attached_session_id = Some(*session_id);
            false
        }
        DaemonMessage::Sessions { sessions } => {
            if sessions.is_empty() {
                state.status_texts.push("[daemon] no sessions".to_string());
            } else {
                state
                    .status_texts
                    .push(format!("[daemon] sessions ({})", sessions.len()));
                for session in sessions {
                    let prefix = if Some(session.session_id) == state.attached_session_id {
                        "*"
                    } else {
                        " "
                    };
                    let title = session.title.as_deref().unwrap_or("untitled");
                    let model = session.selected_model.as_deref().unwrap_or("-");
                    state.status_texts.push(format!(
                        "{} {}: \"{title}\" ({model}) — {} turns",
                        prefix, session.session_id, session.turn_count,
                    ));
                }
            }
            if state.attached_session_id.is_none()
                && let Some(sender) = daemon_tx
            {
                if let Some(first) = sessions.first() {
                    if let Err(e) = sender.send(ClientMessage::AttachSession {
                        session_id: first.session_id,
                    }) {
                        tracing::error!("failed to send AttachSession: {e}");
                    }
                } else {
                    if let Err(e) = sender.send(ClientMessage::CreateSession {
                        title: Some("default".to_string()),
                        parent_session_id: None,
                        working_dir: None,
                        context_config: None,
                        account_name: None,
                        selected_model: None,
                        reasoning_effort: None,
                    }) {
                        tracing::error!("failed to send CreateSession: {e}");
                    }
                }
            }
            true
        }
        _ => false,
    }
}

pub(crate) fn apply_daemon_message(
    state: &mut AppState,
    message: DaemonMessage,
    daemon_tx: Option<std::sync::mpsc::Sender<ClientMessage>>,
) -> Result<(), ClientError> {
    if handle_session_message(state, &daemon_tx, &message) {
        return Ok(());
    }

    dispatch_daemon_message(&message, state);
    Ok(())
}
