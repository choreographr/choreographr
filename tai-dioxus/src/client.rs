use crate::state::{AppState, UiEvent};
use dioxus::prelude::*;
use std::io;
use tai_client_core::{
    ClientError, ShellCommand, dispatch_daemon_message, parse_input_line, run_daemon_connection,
    shell_command_echo,
};
use tai_proto::{ClientMessage, DaemonMessage};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};

pub(crate) async fn run_client(
    socket_path: String,
    client_rx: UnboundedReceiver<ClientMessage>,
    ui_tx: UnboundedSender<UiEvent>,
) -> io::Result<()> {
    let result = run_daemon_connection(
        &socket_path,
        |message| {
            let _ = ui_tx.send(UiEvent::Daemon(message));
        },
        client_rx,
    )
    .await;
    if result.is_ok() {
        let _ = ui_tx.send(UiEvent::ReaderClosed);
    }
    result.map_err(io::Error::from)
}

pub(crate) fn submit_input(
    state: &mut Signal<AppState>,
    daemon_tx: Option<UnboundedSender<ClientMessage>>,
) {
    let line = state.read().input.trim().to_string();
    state.write().input.clear();
    let command = {
        let mut guard = state.write();
        parse_input_line(&line, &mut guard.next_request_id)
    };
    handle_shell_command(&mut state.write(), daemon_tx, command);
}

pub(crate) fn handle_shell_command(
    state: &mut AppState,
    daemon_tx: Option<UnboundedSender<ClientMessage>>,
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
    daemon_tx: Option<UnboundedSender<ClientMessage>>,
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
    daemon_tx: Option<UnboundedSender<ClientMessage>>,
) -> Result<(), ClientError> {
    let response = dispatch_daemon_message(state, message)?;
    if let Some(msg) = response
        && let Some(sender) = daemon_tx
    {
        let _ = sender.send(msg);
    }
    Ok(())
}
