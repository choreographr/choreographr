use crate::render::{mouse_in_history_box, render};
use crate::state::{App, UiEvent};
use crossterm::event::{self, Event, KeyCode, KeyEventKind, MouseEventKind};
use ratatui::{Terminal, backend::CrosstermBackend};
use std::{collections::HashSet, io, sync::Arc, time::Duration};
use tai_proto::{ClientMessage, DaemonMessage, read_message, socket_path, write_message};
use tai_sh::{
    ImageAssembler, ShellCommand, build_picker, build_rendered_image, channel_closed,
    parse_input_line,
};
use tokio::{
    io::AsyncWriteExt,
    net::UnixStream,
    sync::{Mutex, mpsc},
};

pub(crate) async fn run_app() -> io::Result<()> {
    let socket_path = socket_path();
    let stream = UnixStream::connect(&socket_path).await?;
    let (mut reader, mut writer) = stream.into_split();
    let (client_tx, mut client_rx) = mpsc::channel::<ClientMessage>(128);
    let (ui_tx, mut ui_rx) = mpsc::channel::<UiEvent>(128);
    let active = Arc::new(Mutex::new(HashSet::<u32>::new()));

    let picker = build_picker();
    let picker_protocol = format!("{:?}", picker.protocol_type());

    let writer_task = tokio::spawn(async move {
        while let Some(message) = client_rx.recv().await {
            write_message(&mut writer, &message).await?;
        }
        writer.shutdown().await
    });

    let reader_ui_tx = ui_tx.clone();
    let reader_task = tokio::spawn(async move {
        loop {
            match read_message::<_, DaemonMessage>(&mut reader).await {
                Ok(message) => {
                    if reader_ui_tx.send(UiEvent::Daemon(message)).await.is_err() {
                        break;
                    }
                }
                Err(error)
                    if matches!(
                        error.kind(),
                        io::ErrorKind::UnexpectedEof | io::ErrorKind::ConnectionReset
                    ) =>
                {
                    let _ = reader_ui_tx.send(UiEvent::ReaderClosed).await;
                    break;
                }
                Err(error) => return Err(error),
            }
        }
        Ok::<(), io::Error>(())
    });

    let signal_ui_tx = ui_tx.clone();
    let signal_task = tokio::spawn(async move {
        tokio::signal::ctrl_c().await.map_err(io::Error::other)?;
        let _ = signal_ui_tx.send(UiEvent::Interrupt).await;
        Ok::<(), io::Error>(())
    });

    crossterm::terminal::enable_raw_mode()?;
    let mut stdout = io::stdout();
    crossterm::execute!(
        stdout,
        crossterm::terminal::EnterAlternateScreen,
        crossterm::event::EnableMouseCapture
    )?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let mut app = App::new(socket_path.clone(), picker_protocol);
    let mut assembler = ImageAssembler::new();
    client_tx
        .send(ClientMessage::ListSessions)
        .await
        .map_err(channel_closed)?;

    let result = run_ui_loop(
        &mut terminal,
        &mut app,
        &picker,
        &mut assembler,
        &client_tx,
        &mut ui_rx,
        &active,
    )
    .await;

    crossterm::terminal::disable_raw_mode()?;
    crossterm::execute!(
        terminal.backend_mut(),
        crossterm::terminal::LeaveAlternateScreen,
        crossterm::event::DisableMouseCapture
    )?;
    terminal.show_cursor()?;

    drop(client_tx);
    writer_task.await.map_err(io::Error::other)??;
    signal_task.abort();
    match signal_task.await {
        Ok(Ok(())) => {}
        Ok(Err(error)) => return Err(error),
        Err(error) if error.is_cancelled() => {}
        Err(error) => return Err(io::Error::other(error)),
    }

    match reader_task.await {
        Ok(Ok(())) => {}
        Ok(Err(error)) => return Err(error),
        Err(error) => return Err(io::Error::other(error)),
    }

    result
}

pub(crate) async fn run_ui_loop(
    terminal: &mut Terminal<CrosstermBackend<std::io::Stdout>>,
    app: &mut App,
    picker: &ratatui_image::picker::Picker,
    assembler: &mut ImageAssembler,
    client_tx: &mpsc::Sender<ClientMessage>,
    ui_rx: &mut mpsc::Receiver<UiEvent>,
    active: &Arc<Mutex<HashSet<u32>>>,
) -> io::Result<()> {
    while !app.should_quit {
        while let Ok(event) = event::poll(Duration::from_millis(0)) {
            if !event {
                break;
            }
            handle_terminal_event(event::read()?, app, client_tx).await?;
        }

        while let Ok(message) = ui_rx.try_recv() {
            match message {
                UiEvent::Daemon(message) => {
                    handle_daemon_message(message, app, picker, assembler, active, client_tx)
                        .await?;
                }
                UiEvent::ReaderClosed => {
                    app.push_text("daemon connection closed");
                    app.should_quit = true;
                }
                UiEvent::Interrupt => {
                    app.push_text("interrupt received");
                    app.should_quit = true;
                }
            }
        }

        terminal.draw(|frame| render(frame, app))?;

        if event::poll(Duration::from_millis(16))? {
            handle_terminal_event(event::read()?, app, client_tx).await?;
        }
    }

    Ok(())
}

pub(crate) async fn handle_terminal_event(
    event: Event,
    app: &mut App,
    client_tx: &mpsc::Sender<ClientMessage>,
) -> io::Result<()> {
    match event {
        Event::Key(key) => {
            if key.kind != KeyEventKind::Press {
                return Ok(());
            }
            match key.code {
                KeyCode::Char('c')
                    if key
                        .modifiers
                        .contains(crossterm::event::KeyModifiers::CONTROL) =>
                {
                    app.should_quit = true;
                }
                KeyCode::Char('q') if app.input.is_empty() => app.should_quit = true,
                KeyCode::Esc => app.should_quit = true,
                KeyCode::Enter => {
                    let line = app.input.trim().to_string();
                    app.input.clear();
                    match parse_input_line(&line, &mut app.next_request_id) {
                        ShellCommand::Empty => {}
                        ShellCommand::InvalidCancel(value) => {
                            app.push_text(format!("invalid request id: {value}"))
                        }
                        ShellCommand::Send(message) => {
                            match &message {
                                ClientMessage::RunInput { request_id, input } => {
                                    app.active.insert(*request_id);
                                    app.push_text(format!("> {}", String::from_utf8_lossy(input)));
                                }
                                ClientMessage::TestImage { request_id } => {
                                    app.active.insert(*request_id);
                                    app.push_text("> /image".to_string());
                                }
                                _ => {}
                            }
                            client_tx.send(message).await.map_err(channel_closed)?;
                        }
                    }
                }
                KeyCode::Backspace => {
                    app.input.pop();
                }
                KeyCode::Char(c) => {
                    app.input.push(c);
                }
                KeyCode::PageUp => {
                    app.scroll_up(3);
                }
                KeyCode::PageDown => {
                    app.scroll_down(3);
                }
                _ => {}
            }
        }
        Event::Mouse(mouse) if mouse_in_history_box(mouse.column, mouse.row) => match mouse.kind {
            MouseEventKind::ScrollUp => app.scroll_up(1),
            MouseEventKind::ScrollDown => app.scroll_down(1),
            _ => {}
        },
        Event::Mouse(_) => {}
        _ => {}
    }
    Ok(())
}

pub(crate) async fn handle_daemon_message(
    message: DaemonMessage,
    app: &mut App,
    picker: &ratatui_image::picker::Picker,
    assembler: &mut ImageAssembler,
    active: &Arc<Mutex<HashSet<u32>>>,
    client_tx: &mpsc::Sender<ClientMessage>,
) -> io::Result<()> {
    match message {
        DaemonMessage::SessionCreated { session_id, title } => {
            let label = title.unwrap_or_else(|| "untitled".to_string());
            app.push_text(format!("[daemon] created session {session_id}: {label}"));
        }
        DaemonMessage::Sessions { sessions } => {
            if let Some(session) = sessions.first() {
                client_tx
                    .send(ClientMessage::AttachSession {
                        session_id: session.session_id,
                    })
                    .await
                    .map_err(channel_closed)?;
            } else {
                client_tx
                    .send(ClientMessage::CreateSession {
                        title: Some("default".to_string()),
                    })
                    .await
                    .map_err(channel_closed)?;
            }
        }
        DaemonMessage::SessionAttached { session_id } => {
            app.push_text(format!("[daemon] attached session: {session_id}"))
        }
        DaemonMessage::SessionState {
            session_id,
            title,
            selected_model,
            messages,
        } => {
            let title = title.unwrap_or_else(|| "untitled".to_string());
            app.push_text(format!("[daemon] session {session_id}: {title}"));
            if let Some(model) = selected_model {
                app.push_text(format!("[daemon] selected model: {model}"));
            }
            for message in messages {
                app.push_session_message(message);
            }
        }
        DaemonMessage::SessionFailed { operation, error } => {
            app.push_text(format!("[daemon] {operation} failed: {error}"))
        }
        DaemonMessage::SessionMessageAppended { message } => app.push_session_message(message),
        DaemonMessage::Started { request_id } => app.begin_stream(request_id),
        DaemonMessage::ToolCallStarted {
            request_id,
            call_id,
            tool_name,
            arguments_json,
        } => app.push_text(format!(
            "[{request_id}] tool {tool_name}#{call_id} start {arguments_json}"
        )),
        DaemonMessage::ToolCallFinished {
            request_id,
            call_id,
            tool_name,
            output,
        } => app.push_text(format!(
            "[{request_id}] tool {tool_name}#{call_id} ok: {output}"
        )),
        DaemonMessage::ToolCallFailed {
            request_id,
            call_id,
            tool_name,
            error,
        } => app.push_text(format!(
            "[{request_id}] tool {tool_name}#{call_id} failed: {error}"
        )),
        DaemonMessage::OutputChunk {
            request_id,
            stream,
            data,
        } => {
            let text = String::from_utf8(data)
                .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
            app.append_stream_text(request_id, stream, &text);
        }
        DaemonMessage::ImageStart {
            request_id,
            metadata,
        } => assembler.start(request_id, metadata)?,
        DaemonMessage::ImageChunk {
            request_id,
            image_id,
            data,
        } => assembler.push_chunk(request_id, image_id, &data)?,
        DaemonMessage::ImageEnd {
            request_id,
            image_id,
        } => {
            let (metadata, data) = assembler.finish(request_id, image_id)?;
            let rendered = build_rendered_image(picker, metadata, data)?;
            app.push_image(rendered);
        }
        DaemonMessage::Done { request_id } => {
            app.finalize_stream(request_id);
            app.push_text(format!("[{request_id}] done"));
            assembler.drop_request(request_id);
            active.lock().await.remove(&request_id);
            app.drop_request(request_id);
        }
        DaemonMessage::Failed { request_id, error } => {
            app.finalize_stream(request_id);
            app.push_text(format!("[{request_id}] failed: {error}"));
            assembler.drop_request(request_id);
            active.lock().await.remove(&request_id);
            app.drop_request(request_id);
        }
        DaemonMessage::Cancelled { request_id } => {
            app.finalize_stream(request_id);
            app.push_text(format!("[{request_id}] cancelled"));
            assembler.drop_request(request_id);
            active.lock().await.remove(&request_id);
            app.drop_request(request_id);
        }
        DaemonMessage::Pong => app.push_text("[daemon] pong".to_string()),
        DaemonMessage::Models {
            models,
            selected_model,
        } => {
            if models.is_empty() {
                app.push_text("[daemon] no models available".to_string());
            } else {
                app.push_text(format!("[daemon] supported models ({})", models.len()));
                for model in models {
                    let prefix = if selected_model.as_deref() == Some(model.as_str()) {
                        "*"
                    } else {
                        "-"
                    };
                    app.push_text(format!("{prefix} {model}"));
                }
            }
        }
        DaemonMessage::ModelsFailed { error } => {
            app.push_text(format!("[daemon] models failed: {error}"))
        }
        DaemonMessage::ModelSelected { model } => {
            app.push_text(format!("[daemon] selected model: {model}"))
        }
        DaemonMessage::ModelSelectionFailed { model, error } => {
            app.push_text(format!("[daemon] failed to select model {model}: {error}"))
        }
    }
    Ok(())
}
