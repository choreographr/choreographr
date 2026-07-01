use crate::state::{AppState, UiEvent};
use dioxus::prelude::*;
use std::io;
use tai_client_core::{ShellCommand, dispatch_daemon_message, parse_input_line, shell_command_echo};
use tai_proto::{ClientMessage, DaemonMessage, read_message, write_message};
use tokio::{
    net::UnixStream,
    sync::mpsc::{UnboundedReceiver, UnboundedSender},
};

pub(crate) async fn run_client(
    socket_path: String,
    mut client_rx: UnboundedReceiver<ClientMessage>,
    ui_tx: UnboundedSender<UiEvent>,
) -> io::Result<()> {
    let stream = UnixStream::connect(&socket_path).await?;
    let (mut reader, mut writer) = stream.into_split();

    let writer_ui_tx = ui_tx.clone();
    let writer_task = tokio::spawn(async move {
        while let Some(message) = client_rx.recv().await {
            if let Err(error) = write_message(&mut writer, &message).await {
                let _ = writer_ui_tx.send(UiEvent::WriterFailed(error.to_string()));
                return Err(error);
            }
        }
        Ok::<(), io::Error>(())
    });

    loop {
        match read_message::<_, DaemonMessage>(&mut reader).await {
            Ok(message) => {
                if ui_tx.send(UiEvent::Daemon(message)).is_err() {
                    break;
                }
            }
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::UnexpectedEof | io::ErrorKind::ConnectionReset
                ) =>
            {
                let _ = ui_tx.send(UiEvent::ReaderClosed);
                break;
            }
            Err(error) => {
                writer_task.abort();
                match writer_task.await {
                    Ok(Ok(())) | Err(_) => {}
                    Ok(Err(writer_error)) => return Err(writer_error),
                }
                return Err(error);
            }
        }
    }

    match writer_task.await {
        Ok(Ok(())) | Err(_) => {}
        Ok(Err(error)) => return Err(error),
    }

    Ok(())
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
) -> io::Result<()> {
    let response = dispatch_daemon_message(state, message)?;
    if let Some(msg) = response
        && let Some(sender) = daemon_tx
    {
        let _ = sender.send(msg);
    }
    Ok(())
}
