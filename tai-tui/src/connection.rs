use crate::render::{mouse_in_history_box, render};
use crate::state::{App, InputBuffer, PAGE_SCROLL_LINES, Page, SessionManagerView, UiEvent};
use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers, MouseEventKind};
use ratatui::{Terminal, backend::CrosstermBackend};
use signal_hook::consts::SIGINT;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
    mpsc,
};
use std::{io, time::Duration};
use tai_client_core::{
    ClientError, broken_pipe, build_add_credential_message, dispatch_daemon_message,
    resolve_private_key, run_daemon_connection, shell_command_echo,
};
use tai_proto::{ClientMessage, DaemonMessage, socket_path};
use tai_tui::{ShellCommand, build_picker, parse_input_line};

const UI_EVENT_CHANNEL_SIZE: usize = 4096;
const UI_FRAME_POLL_MS: u64 = 16;

pub(crate) fn run_app() -> io::Result<()> {
    let socket_path = socket_path();
    let app_socket_path = socket_path.clone();
    let (client_tx, client_rx) = std::sync::mpsc::channel::<ClientMessage>();
    let (shutdown_tx, shutdown_rx) = std::sync::mpsc::channel::<()>();
    let (ui_tx, mut ui_rx) = mpsc::sync_channel::<UiEvent>(UI_EVENT_CHANNEL_SIZE);

    let picker = build_picker();
    let picker_protocol = format!("{:?}", picker.protocol_type());

    let connection_ui_tx = ui_tx.clone();
    let connection_task = std::thread::spawn(move || {
        let result = run_daemon_connection(
            &socket_path,
            |message| {
                let _ = connection_ui_tx.send(UiEvent::Daemon(message));
            },
            client_rx,
            Some(shutdown_rx),
        );
        if result.is_ok() {
            let _ = connection_ui_tx.send(UiEvent::ReaderClosed);
        }
        result
    });

    let interrupted = Arc::new(AtomicBool::new(false));
    signal_hook::flag::register(SIGINT, Arc::clone(&interrupted)).map_err(io::Error::other)?;

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
    let result = run_ui_loop(
        &mut terminal,
        &mut app,
        &picker,
        &client_tx,
        &mut ui_rx,
        &interrupted,
    )
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

    match connection_task.join() {
        Ok(Ok(())) => {}
        Ok(Err(error)) => return Err(error.into()),
        Err(_) => {
            return Err(io::Error::other("daemon connection thread panicked"));
        }
    }

    result
}

pub(crate) fn run_ui_loop(
    terminal: &mut Terminal<CrosstermBackend<std::io::Stdout>>,
    app: &mut App,
    picker: &ratatui_image::picker::Picker,
    client_tx: &std::sync::mpsc::Sender<ClientMessage>,
    ui_rx: &mut mpsc::Receiver<UiEvent>,
    interrupted: &AtomicBool,
) -> Result<(), ClientError> {
    while !app.should_quit {
        if interrupted.load(Ordering::Relaxed) {
            app.push_text("interrupt received");
            app.should_quit = true;
            break;
        }

        // Drain *all* pending crossterm events before processing UI messages
        // and rendering.  If we only handled one per iteration, a fast
        // trackpad scroll could fall behind, and scrolling would continue
        // after the finger lifts because unprocessed events would still be
        // applied on subsequent frames.  The `while let Ok(true)` pattern
        // keeps polling with a zero timeout as long as events are ready.
        while let Ok(true) = event::poll(Duration::from_millis(0)) {
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
            }
        }

        // Consume the frame's accumulated scroll delta in one batch
        // (read-then-reset so no momentum carries forward).
        app.apply_scroll_delta();

        // Update viewport dimensions and clamp scroll *outside* the
        // terminal.draw closure so that render never mutates app state.
        app.update_viewport_from_terminal_size();
        app.clamp_scroll_state();

        terminal.draw(|frame| render(frame, app))?;

        // Block for up to ~60 Hz to pace the frame rate without
        // busy-waiting.  Any event that arrives during this interval
        // will be handled at the top of the next iteration's drain
        // loop — ensuring all events are processed in a single batch
        // before the next render.
        let _ = event::poll(Duration::from_millis(UI_FRAME_POLL_MS))?;
    }

    Ok(())
}

pub(crate) fn handle_terminal_event(
    event: Event,
    app: &mut App,
    client_tx: &std::sync::mpsc::Sender<ClientMessage>,
) -> Result<(), ClientError> {
    match app.page {
        Page::SessionManager => handle_session_manager_event(event, app, client_tx),
        Page::Chat => handle_chat_event(event, app, client_tx),
    }
}

fn handle_chat_event(
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
                KeyCode::Char('s') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    app.page = Page::SessionManager;
                    client_tx
                        .send(ClientMessage::ListSessions)
                        .map_err(broken_pipe)?;
                    client_tx
                        .send(ClientMessage::SubscribeSessionsSummary)
                        .map_err(broken_pipe)?;
                }
                KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    app.should_quit = true;
                }
                KeyCode::Char('q') if app.input.is_empty() => app.should_quit = true,
                KeyCode::Esc => app.should_quit = true,
                KeyCode::Up => {
                    app.navigate_history_up();
                }
                KeyCode::Down => {
                    app.navigate_history_down();
                }
                KeyCode::Enter => {
                    let line = app.input.text.trim().to_string();
                    app.input.clear();
                    app.commit_to_history(line.clone());
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
                            client_tx.send(message).map_err(broken_pipe)?;
                        }
                        ShellCommand::Unlock { method } => match resolve_private_key(&method) {
                            Ok(private_key) => {
                                let _ = client_tx.send(ClientMessage::Unlock { private_key });
                            }
                            Err(e) => {
                                app.push_text(format!("[error] {e}"));
                                return Ok(());
                            }
                        },
                        ShellCommand::AddCredential {
                            service,
                            credential_type,
                            fields,
                            unlock,
                        } => {
                            match build_add_credential_message(
                                service,
                                credential_type,
                                fields,
                                unlock,
                            ) {
                                Ok(msg) => {
                                    let _ = client_tx.send(msg);
                                }
                                Err(e) => {
                                    app.push_text(format!("[error] {e}"));
                                    return Ok(());
                                }
                            }
                        }
                        ShellCommand::RemoveCredential { service } => {
                            let _ = client_tx.send(ClientMessage::RemoveCredential { service });
                        }
                    }
                }
                KeyCode::Backspace
                | KeyCode::Delete
                | KeyCode::Left
                | KeyCode::Right
                | KeyCode::Home
                | KeyCode::End => {
                    handle_input_key(key, &mut app.input);
                }
                KeyCode::Char(_) => {
                    handle_input_key(key, &mut app.input);
                }
                KeyCode::PageUp => {
                    app.scroll_up(PAGE_SCROLL_LINES);
                }
                KeyCode::PageDown => {
                    app.scroll_down(PAGE_SCROLL_LINES);
                }
                _ => {}
            }
        }
        Event::Mouse(mouse) if mouse_in_history_box(mouse.column, mouse.row) => {
            // Accumulate scroll events rather than scrolling immediately.
            // All accumulated deltas are applied in a single batch each
            // frame by `apply_scroll_delta`, which reads the accumulator
            // and resets it to zero — this prevents per-event re-renders
            // and ensures no momentum carries between frames.
            match mouse.kind {
                MouseEventKind::ScrollUp => {
                    app.scroll_accumulator = app.scroll_accumulator.saturating_add(1);
                }
                MouseEventKind::ScrollDown => {
                    app.scroll_accumulator = app.scroll_accumulator.saturating_sub(1);
                }
                _ => {}
            }
        }
        Event::Mouse(_) => {}
        _ => {}
    }
    Ok(())
}

fn handle_input_key(key: crossterm::event::KeyEvent, input: &mut InputBuffer) {
    // All editing logic moved into InputBuffer::handle_key.
    input.handle_key(key);
}

fn handle_session_manager_event(
    event: Event,
    app: &mut App,
    client_tx: &std::sync::mpsc::Sender<ClientMessage>,
) -> Result<(), ClientError> {
    let Event::Key(key) = event else {
        return Ok(());
    };
    if key.kind != KeyEventKind::Press {
        return Ok(());
    }

    match app.session_mgr.view {
        SessionManagerView::List => handle_session_list_key(key, app, client_tx),
        SessionManagerView::Detail => handle_session_detail_key(key, app, client_tx),
    }
}

fn handle_session_list_key(
    key: crossterm::event::KeyEvent,
    app: &mut App,
    client_tx: &std::sync::mpsc::Sender<ClientMessage>,
) -> Result<(), ClientError> {
    // If in delete-confirmation mode, handle y/n/Esc first
    if app.session_mgr.confirm_delete.is_some() {
        match key.code {
            KeyCode::Char('y') | KeyCode::Char('Y') => {
                if let Some((session_id, _title)) = app.session_mgr.confirm_delete.take() {
                    client_tx
                        .send(ClientMessage::DeleteSession { session_id })
                        .map_err(broken_pipe)?;
                }
            }
            KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
                app.session_mgr.confirm_delete = None;
            }
            _ => {}
        }
        return Ok(());
    }

    match key.code {
        KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            app.should_quit = true;
        }
        KeyCode::Up | KeyCode::Char('k') => app.session_mgr.select_up(),
        KeyCode::Down | KeyCode::Char('j') => app.session_mgr.select_down(),
        KeyCode::Enter => {
            if let Some(sel) = app.session_mgr.selection
                && let Some(session) = app.session_mgr.sessions.get(sel)
            {
                let session_id = session.session_id;
                app.page = Page::Chat;
                let _ = client_tx.send(ClientMessage::UnsubscribeSessionsSummary);
                app.reset_for_session_switch();
                app.attached_session_id = Some(session_id);
                client_tx
                    .send(ClientMessage::AttachSession { session_id })
                    .map_err(broken_pipe)?;
            }
        }
        KeyCode::Char('i') => app.session_mgr.enter_detail(),
        KeyCode::Char('n') => {
            client_tx
                .send(ClientMessage::CreateSession {
                    title: None,
                    parent_session_id: None,
                    cwd: None,
                    max_turns: None,
                    context_config: None,
                    account_name: None,
                })
                .map_err(broken_pipe)?;
        }
        KeyCode::Char('d') => {
            // Enter delete-confirmation mode for the selected session
            if let Some(sel) = app.session_mgr.selection
                && let Some(session) = app.session_mgr.sessions.get(sel)
            {
                let title = session.title.clone().unwrap_or_else(|| "untitled".into());
                app.session_mgr.confirm_delete = Some((session.session_id, title));
            }
        }
        KeyCode::Esc | KeyCode::Char('q') => {
            app.page = Page::Chat;
            let _ = client_tx.send(ClientMessage::UnsubscribeSessionsSummary);
        }
        _ => {}
    }
    Ok(())
}

fn handle_session_detail_key(
    key: crossterm::event::KeyEvent,
    app: &mut App,
    client_tx: &std::sync::mpsc::Sender<ClientMessage>,
) -> Result<(), ClientError> {
    match key.code {
        KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            app.should_quit = true;
        }
        KeyCode::Char('b') | KeyCode::Esc => {
            app.session_mgr.leave_detail();
        }
        KeyCode::Enter => {
            if let Some(ref detail) = app.session_mgr.detail_data {
                let session_id = detail.session_id;
                app.page = Page::Chat;
                let _ = client_tx.send(ClientMessage::UnsubscribeSessionsSummary);
                app.reset_for_session_switch();
                app.attached_session_id = Some(session_id);
                client_tx
                    .send(ClientMessage::AttachSession { session_id })
                    .map_err(broken_pipe)?;
            }
        }
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

    // Dispatch per-variant handlers first, then let the generic
    // dispatch in tai_client_core handle the rest (text notifications,
    // stream appends, image assembly, etc.).
    match &message {
        DaemonMessage::SessionCreated { session_id, .. } => {
            // Already known — nothing to do, and skip the generic dispatch too.
            if app
                .session_mgr
                .sessions
                .iter()
                .any(|s| s.session_id == *session_id)
            {
                return Ok(());
            }
            app.handle_session_created(*session_id, client_tx)?;
        }
        DaemonMessage::SessionAttached { session_id } => {
            app.handle_session_attached(*session_id);
        }
        DaemonMessage::SessionStatusChanged { session_id, status } => {
            app.handle_session_status_changed(*session_id, status);
        }
        DaemonMessage::Sessions { sessions } => {
            // The Sessions handler manages the full lifecycle and should not
            // fall through to the generic dispatch (which would duplicate
            // the summary output).
            return app.handle_sessions(sessions, client_tx);
        }
        DaemonMessage::SessionDeleted { session_id } => {
            app.handle_session_deleted(*session_id);
        }
        DaemonMessage::SessionDeleteFailed { session_id, error } => {
            app.handle_session_delete_failed(*session_id, error);
        }
        _ => {}
    }

    let response = dispatch_daemon_message(app, message)?;
    if let Some(msg) = response {
        client_tx.send(msg).map_err(broken_pipe)?;
    }
    Ok(())
}
