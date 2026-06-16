use crate::state::{AppState, DisplayImage, UiEvent};
use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use dioxus::prelude::{Readable, Signal, Writable};
use std::io;
use tai_client_core::{ShellCommand, parse_input_line};
use tai_proto::{
    ClientMessage, DaemonMessage, read_message, socket_path, write_message,
};
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

    match &message {
        ClientMessage::RunInput { input, .. } => {
            state.push_text(format!("> {}", String::from_utf8_lossy(input)));
        }
        ClientMessage::TestImage { .. } => state.push_text("> /image"),
        _ => {}
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
    match message {
        DaemonMessage::SessionCreated { session_id, title } => {
            let label = title.unwrap_or_else(|| "untitled".to_string());
            state.push_text(format!("[daemon] created session {session_id}: {label}"));
        }
        DaemonMessage::Sessions { sessions } => {
            if let Some(sender) = daemon_tx {
                let message = if let Some(session) = sessions.first() {
                    ClientMessage::AttachSession {
                        session_id: session.session_id,
                    }
                } else {
                    ClientMessage::CreateSession {
                        title: Some("default".to_string()),
                    }
                };
                let _ = sender.send(message);
            }
        }
        DaemonMessage::SessionAttached { session_id } => {
            state.push_text(format!("[daemon] attached session: {session_id}"));
        }
        DaemonMessage::SessionState {
            session_id,
            title,
            selected_model,
            messages,
        } => {
            let title = title.unwrap_or_else(|| "untitled".to_string());
            state.push_text(format!("[daemon] session {session_id}: {title}"));
            if let Some(model) = selected_model {
                state.push_text(format!("[daemon] selected model: {model}"));
            }
            for message in messages {
                state.push_session_message(message);
            }
        }
        DaemonMessage::SessionFailed { operation, error } => {
            state.push_text(format!("[daemon] {operation} failed: {error}"));
        }
        DaemonMessage::SessionMessageAppended { message } => {
            state.push_session_message(message);
        }
        DaemonMessage::Started { request_id } => {
            state.begin_stream(request_id);
        }
        DaemonMessage::ToolCallStarted {
            request_id,
            call_id,
            tool_name,
            arguments_json,
        } => {
            state.push_text(format!(
                "[{request_id}] tool {tool_name}#{call_id} start {arguments_json}"
            ));
        }
        DaemonMessage::ToolCallFinished {
            request_id,
            call_id,
            tool_name,
            output,
        } => {
            state.push_text(format!("[{request_id}] tool {tool_name}#{call_id} ok: {output}"));
        }
        DaemonMessage::ToolCallFailed {
            request_id,
            call_id,
            tool_name,
            error,
        } => {
            state.push_text(format!(
                "[{request_id}] tool {tool_name}#{call_id} failed: {error}"
            ));
        }
        DaemonMessage::OutputChunk {
            request_id,
            stream,
            data,
        } => {
            let text = String::from_utf8(data)
                .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
            state.append_stream(request_id, stream, &text);
        }
        DaemonMessage::ImageStart {
            request_id,
            metadata,
        } => {
            state.pending_images.start(request_id, metadata)?;
        }
        DaemonMessage::ImageChunk {
            request_id,
            image_id,
            data,
        } => {
            state.pending_images.push_chunk(request_id, image_id, &data)?;
        }
        DaemonMessage::ImageEnd {
            request_id,
            image_id,
        } => {
            let (metadata, data) = state.pending_images.finish(request_id, image_id)?;
            state.push_image(DisplayImage {
                data_url: format!("data:{};base64,{}", metadata.mime_type, BASE64.encode(data)),
                metadata,
            });
        }
        DaemonMessage::Done { request_id } => {
            state.finalize_stream(request_id);
            state.push_text(format!("[{request_id}] done"));
        }
        DaemonMessage::Failed { request_id, error } => {
            state.finalize_stream(request_id);
            state.push_text(format!("[{request_id}] failed: {error}"));
        }
        DaemonMessage::Cancelled { request_id } => {
            state.finalize_stream(request_id);
            state.push_text(format!("[{request_id}] cancelled"));
        }
        DaemonMessage::Pong => state.push_text("[daemon] pong"),
        DaemonMessage::Models {
            models,
            selected_model,
        } => {
            if models.is_empty() {
                state.push_text("[daemon] no models available");
            } else {
                state.push_text(format!("[daemon] supported models ({})", models.len()));
                for model in models {
                    let prefix = if selected_model.as_deref() == Some(model.as_str()) {
                        "*"
                    } else {
                        "-"
                    };
                    state.push_text(format!("{prefix} {model}"));
                }
            }
        }
        DaemonMessage::ModelsFailed { error } => {
            state.push_text(format!("[daemon] models failed: {error}"));
        }
        DaemonMessage::ModelSelected { model } => {
            state.push_text(format!("[daemon] selected model: {model}"));
        }
        DaemonMessage::ModelSelectionFailed { model, error } => {
            state.push_text(format!("[daemon] failed to select model {model}: {error}"));
        }
    }
    Ok(())
}

pub(crate) fn initial_socket_path() -> String {
    socket_path()
}
