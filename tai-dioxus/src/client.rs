use crate::state::{AppState, UiEvent};
use dioxus::prelude::*;
use futures_channel::mpsc::UnboundedSender;
use std::io;
use tai_client_core::{
    ClientError, ShellCommand, build_add_credential_message, dispatch_daemon_message,
    parse_input_line, resolve_private_key, run_daemon_connection, shell_command_echo,
};
use tai_proto::{ClientMessage, DaemonMessage};

pub(crate) fn run_client(
    socket_path: String,
    client_rx: std::sync::mpsc::Receiver<ClientMessage>,
    ui_tx: UnboundedSender<UiEvent>,
) -> io::Result<()> {
    let result = run_daemon_connection(
        &socket_path,
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
    result.map_err(io::Error::from)
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
            .client
            .push_text(format!("invalid request id: {value}")),
        ShellCommand::UnknownCommand(error) => state.client.push_text(error),
        ShellCommand::Send(message) => send_client_message(state, daemon_tx, message),
        ShellCommand::Unlock { method } => match resolve_private_key(&method) {
            Ok(private_key) => {
                send_client_message(state, daemon_tx, ClientMessage::Unlock { private_key });
            }
            Err(e) => {
                state.client.push_text(format!("[error] {e}"));
            }
        },
        ShellCommand::AddCredential {
            service,
            credential_type,
            fields,
            unlock,
        } => match build_add_credential_message(service, credential_type, fields, unlock) {
            Ok(msg) => send_client_message(state, daemon_tx, msg),
            Err(e) => state.client.push_text(format!("[error] {e}")),
        },
        ShellCommand::RemoveCredential { service } => {
            send_client_message(
                state,
                daemon_tx,
                ClientMessage::RemoveCredential { service },
            );
        }
    }
}

pub(crate) fn send_client_message(
    state: &mut AppState,
    daemon_tx: Option<std::sync::mpsc::Sender<ClientMessage>>,
    message: ClientMessage,
) {
    let Some(sender) = daemon_tx else {
        state.client.push_text("[client] not connected");
        return;
    };

    if let Some(echo) = shell_command_echo(&ShellCommand::Send(message.clone())) {
        state.client.push_text(echo);
    }

    if let Err(error) = sender.send(message) {
        state
            .client
            .push_text(format!("[client] failed to send command: {error}"));
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
                state.client.push_text("[daemon] no sessions");
            } else {
                state
                    .client
                    .push_text(format!("[daemon] sessions ({})", sessions.len()));
                for session in sessions {
                    let prefix = if Some(session.session_id) == state.attached_session_id {
                        "*"
                    } else {
                        " "
                    };
                    let title = session.title.as_deref().unwrap_or("untitled");
                    let model = session.selected_model.as_deref().unwrap_or("-");
                    state.client.push_text(format!(
                        "{} {}: \"{title}\" ({model}) — {} messages",
                        prefix, session.session_id, session.message_count,
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
                        cwd: None,
                        max_turns: None,
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

    let response = dispatch_daemon_message(state, message)?;
    if let Some(msg) = response
        && let Some(sender) = daemon_tx
        && let Err(e) = sender.send(msg)
    {
        tracing::error!("failed to send daemon response: {e}");
    }
    Ok(())
}
