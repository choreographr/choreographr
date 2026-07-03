use crate::state::{AppState, UiEvent};
use dioxus::prelude::*;
use std::io;
use tai_client_core::{
    ClientError, ShellCommand, dispatch_daemon_message, parse_input_line, run_daemon_connection,
    shell_command_echo,
};
use tai_proto::{ClientMessage, DaemonMessage};
use tokio::sync::mpsc::UnboundedSender;

pub(crate) fn run_client(
    socket_path: String,
    client_rx: std::sync::mpsc::Receiver<ClientMessage>,
    ui_tx: UnboundedSender<UiEvent>,
) -> io::Result<()> {
    let result = run_daemon_connection(
        &socket_path,
        |message| {
            if let Err(e) = ui_tx.send(UiEvent::Daemon(message)) {
                eprintln!("[tai-dioxus] failed to send Daemon UI event: {e}");
            }
        },
        client_rx,
    );
    if result.is_ok() {
        if let Err(e) = ui_tx.send(UiEvent::ReaderClosed) {
            eprintln!("[tai-dioxus] failed to send ReaderClosed UI event: {e}");
        }
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
        ShellCommand::InvalidCancel(value) => {
            state.push_text(format!("invalid request id: {value}"))
        }
        ShellCommand::UnknownCommand(error) => {
            state.push_text(error)
        }
        ShellCommand::Send(message) => send_client_message(state, daemon_tx, message),
    }
}

pub(crate) fn send_client_message(
    state: &mut AppState,
    daemon_tx: Option<std::sync::mpsc::Sender<ClientMessage>>,
    message: ClientMessage,
) {
    let Some(sender) = daemon_tx else {
        state.push_text("[client] not connected");
        return;
    };

    if let Some(echo) = shell_command_echo(&ShellCommand::Send(message.clone())) {
        state.push_text(echo);
    }

    if let Err(error) = sender.send(message) {
        state.push_text(format!("[client] failed to send command: {error}"));
    }
}

pub(crate) fn apply_daemon_message(
    state: &mut AppState,
    message: DaemonMessage,
    daemon_tx: Option<std::sync::mpsc::Sender<ClientMessage>>,
) -> Result<(), ClientError> {
    match &message {
        DaemonMessage::SessionCreated { session_id, .. }
        | DaemonMessage::SessionAttached { session_id } => {
            state.attached_session_id = Some(*session_id);
        }
        DaemonMessage::Sessions { sessions } => {
            if sessions.is_empty() {
                state.push_text("[daemon] no sessions");
            } else {
                state.push_text(format!("[daemon] sessions ({})", sessions.len()));
                for session in sessions {
                    let prefix = if Some(session.session_id) == state.attached_session_id {
                        "*"
                    } else {
                        " "
                    };
                    let title = session.title.as_deref().unwrap_or("untitled");
                    let model = session.selected_model.as_deref().unwrap_or("-");
                    state.push_text(format!(
                        "{} {}: \"{title}\" ({model}) — {} messages",
                        prefix, session.session_id, session.message_count,
                    ));
                }
            }
            if state.attached_session_id.is_none() {
                if let Some(sender) = &daemon_tx {
                    if let Some(first) = sessions.first() {
                        let _ = sender.send(ClientMessage::AttachSession {
                            session_id: first.session_id,
                        });
                    } else {
                        let _ = sender.send(ClientMessage::CreateSession {
                            title: Some("default".to_string()),
                            parent_session_id: None,
                            cwd: None,
                            max_turns: None,
                        });
                    }
                }
            }
            return Ok(());
        }
        _ => {}
    }

    let response = dispatch_daemon_message(state, message)?;
    if let Some(msg) = response
        && let Some(sender) = daemon_tx
    {
        if let Err(e) = sender.send(msg) {
            eprintln!("[tai-dioxus] failed to send daemon response: {e}");
        }
    }
    Ok(())
}
