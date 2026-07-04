use crate::render::{mouse_in_history_box, render};
use crate::state::{App, UiEvent};
use crossterm::event::{self, Event, KeyCode, KeyEventKind, MouseEventKind};
use ratatui::{Terminal, backend::CrosstermBackend};
use std::{io, time::Duration};
use tai_client_core::{
    ClientError, dispatch_daemon_message, run_daemon_connection, shell_command_echo,
};
use tai_proto::{ClientMessage, DaemonMessage, socket_path};
use tai_tui::{ShellCommand, build_picker, parse_input_line};
use tokio::sync::mpsc;

const UI_EVENT_CHANNEL_SIZE: usize = 128;
const UI_FRAME_POLL_MS: u64 = 16;

pub(crate) async fn run_app() -> io::Result<()> {
    let socket_path = socket_path();
    let app_socket_path = socket_path.clone();
    let (client_tx, client_rx) = std::sync::mpsc::channel::<ClientMessage>();
    let (shutdown_tx, shutdown_rx) = std::sync::mpsc::channel::<()>();
    let (ui_tx, mut ui_rx) = mpsc::channel::<UiEvent>(UI_EVENT_CHANNEL_SIZE);

    let picker = build_picker();
    let picker_protocol = format!("{:?}", picker.protocol_type());

    let connection_ui_tx = ui_tx.clone();
    let connection_task = tokio::task::spawn_blocking(move || {
        let result = run_daemon_connection(
            &socket_path,
            |message| {
                if let Err(e) = connection_ui_tx.try_send(UiEvent::Daemon(message)) {
                    eprintln!("[tai-tui] failed to send Daemon UI event: {e}");
                }
            },
            client_rx,
            Some(shutdown_rx),
        );
        if result.is_ok() {
            if let Err(e) = connection_ui_tx.try_send(UiEvent::ReaderClosed) {
                eprintln!("[tai-tui] failed to send ReaderClosed UI event: {e}");
            }
        }
        result
    });

    let signal_ui_tx = ui_tx.clone();
    let signal_task = tokio::spawn(async move {
        tokio::signal::ctrl_c().await.map_err(io::Error::other)?;
        if let Err(e) = signal_ui_tx.send(UiEvent::Interrupt).await {
            eprintln!("[tai-tui] failed to send Interrupt UI event: {e}");
        }
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

    let mut app = App::new(app_socket_path, picker_protocol);
    client_tx
        .send(ClientMessage::ListSessions)
        .map_err(|e| io::Error::new(io::ErrorKind::BrokenPipe, e.to_string()))?;
    let result = run_ui_loop(&mut terminal, &mut app, &picker, &client_tx, &mut ui_rx)
        .await
        .map_err(io::Error::from);

    let _ = shutdown_tx.send(());
    drop(client_tx);

    crossterm::terminal::disable_raw_mode()?;
    crossterm::execute!(
        terminal.backend_mut(),
        crossterm::terminal::LeaveAlternateScreen,
        crossterm::event::DisableMouseCapture
    )?;
    terminal.show_cursor()?;

    match connection_task.await {
        Ok(Ok(())) => {}
        Ok(Err(error)) => return Err(error.into()),
        Err(error) => return Err(io::Error::other(error)),
    }
    signal_task.abort();
    match signal_task.await {
        Ok(Ok(())) => {}
        Ok(Err(error)) => return Err(error),
        Err(error) if error.is_cancelled() => {}
        Err(error) => return Err(io::Error::other(error)),
    }

    result.map_err(io::Error::from)
}

pub(crate) async fn run_ui_loop(
    terminal: &mut Terminal<CrosstermBackend<std::io::Stdout>>,
    app: &mut App,
    picker: &ratatui_image::picker::Picker,
    client_tx: &std::sync::mpsc::Sender<ClientMessage>,
    ui_rx: &mut mpsc::Receiver<UiEvent>,
) -> Result<(), ClientError> {
    while !app.should_quit {
        while let Ok(event) = event::poll(Duration::from_millis(0)) {
            if !event {
                break;
            }
            handle_terminal_event(event::read()?, app, client_tx)?;
        }

        while let Ok(message) = ui_rx.try_recv() {
            match message {
                UiEvent::Daemon(message) => {
                    handle_daemon_message(message, app, picker, client_tx)?;
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

        if event::poll(Duration::from_millis(UI_FRAME_POLL_MS))? {
            handle_terminal_event(event::read()?, app, client_tx)?;
        }
    }

    Ok(())
}

pub(crate) fn handle_terminal_event(
    event: Event,
    app: &mut App,
    client_tx: &std::sync::mpsc::Sender<ClientMessage>,
) -> Result<(), ClientError> {
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
                    match parse_input_line(&line, &mut app.next_request_id, app.attached_session_id)
                    {
                        ShellCommand::Empty => {}
                        ShellCommand::InvalidCancel(value) => {
                            app.push_text(format!("invalid request id: {value}"))
                        }
                        ShellCommand::UnknownCommand(error) => app.push_text(error),
                        ShellCommand::Send(message) => {
                            if let Some(echo) =
                                shell_command_echo(&ShellCommand::Send(message.clone()))
                            {
                                app.push_text(echo);
                            }
                            match &message {
                                ClientMessage::RunInput { request_id, .. }
                                | ClientMessage::TestImage { request_id } => {
                                    app.active.insert(*request_id);
                                }
                                _ => {}
                            }
                            client_tx.send(message).map_err(|e| {
                                ClientError::Io(io::Error::new(
                                    io::ErrorKind::BrokenPipe,
                                    e.to_string(),
                                ))
                            })?;
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

pub(crate) fn handle_daemon_message(
    message: DaemonMessage,
    app: &mut App,
    picker: &ratatui_image::picker::Picker,
    client_tx: &std::sync::mpsc::Sender<ClientMessage>,
) -> Result<(), ClientError> {
    app.picker = Some(picker.clone());

    match &message {
        DaemonMessage::SessionCreated { session_id, .. }
        | DaemonMessage::SessionAttached { session_id } => {
            app.attached_session_id = Some(*session_id);
        }
        DaemonMessage::Sessions { sessions } => {
            if sessions.is_empty() {
                app.push_text("[daemon] no sessions");
            } else {
                app.push_text(format!("[daemon] sessions ({})", sessions.len()));
                for session in sessions {
                    let prefix = if Some(session.session_id) == app.attached_session_id {
                        "*"
                    } else {
                        " "
                    };
                    let title = session.title.as_deref().unwrap_or("untitled");
                    let model = session.selected_model.as_deref().unwrap_or("-");
                    app.push_text(format!(
                        "{} {}: \"{title}\" ({model}) — {} messages",
                        prefix, session.session_id, session.message_count,
                    ));
                }
            }
            if app.attached_session_id.is_none() {
                if let Some(first) = sessions.first() {
                    client_tx
                        .send(ClientMessage::AttachSession {
                            session_id: first.session_id,
                        })
                        .map_err(|e| {
                            ClientError::Io(io::Error::new(
                                io::ErrorKind::BrokenPipe,
                                e.to_string(),
                            ))
                        })?;
                } else {
                    client_tx
                        .send(ClientMessage::CreateSession {
                            title: Some("default".to_string()),
                            parent_session_id: None,
                            cwd: None,
                            max_turns: None,
                        })
                        .map_err(|e| {
                            ClientError::Io(io::Error::new(
                                io::ErrorKind::BrokenPipe,
                                e.to_string(),
                            ))
                        })?;
                }
            }
            return Ok(());
        }
        _ => {}
    }

    let response = dispatch_daemon_message(app, message)?;
    if let Some(msg) = response {
        client_tx.send(msg).map_err(|e| {
            ClientError::Io(io::Error::new(io::ErrorKind::BrokenPipe, e.to_string()))
        })?;
    }
    Ok(())
}
