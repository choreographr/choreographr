use crate::render::{mouse_in_history_box, mouse_in_scrollbar_column, render};
use crate::state::PROVIDER_OPTIONS;
use crate::state::{
    AIProvidersView, App, HOME_MENU_ITEMS, HistoryItem, HomeMenuItem, InputBuffer,
    PAGE_SCROLL_LINES, Page, SessionManagerView, UiEvent, find_history_item_at_row,
};
use crossbeam::channel;
use crossbeam::select;
use crossterm::event::{
    self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEventKind,
};
use mio::unix::pipe;
use mio::{Events, Interest, Poll, Token};
use nix::fcntl::{F_SETFD, F_SETFL, FdFlag, OFlag, fcntl};
use nix::sys::signal::{Signal, raise};
use ratatui::{Terminal, backend::CrosstermBackend};
use signal_hook::low_level::pipe as signal_pipe;
use std::io::{self, Read};
use std::os::unix::io::AsRawFd;
use std::{thread, time::Duration};
use tai_client_core::{
    ClientError, ConnectionMode, broken_pipe, build_add_credential_message,
    dispatch_daemon_message, is_valid_account_name, resolve_private_key,
    run_daemon_connection_with_mode, shell_command_echo,
};
use tai_keystore::ensure_keypair;
use tai_proto::{ClientMessage, DaemonMessage};
use tai_tui::image_worker::{ImageResult, ImageWorker};
use tai_tui::terminal_progress;
use tai_tui::{ShellCommand, build_picker, parse_input_line};
use tui_prompts::State;

const UI_EVENT_CHANNEL_SIZE: usize = 4096;

/// Commands sent from the terminal-event thread to the main loop for
/// coordinating terminal state around suspend/resume cycles.
#[derive(Debug)]
enum ResumeCommand {
    /// SIGCONT was received — re-initialise raw mode, alternate screen,
    /// and mouse capture after the terminal pty state was reset.
    ReinitTerminal,
    /// SIGTSTP was received — restore the terminal to normal (cooked)
    /// mode before the process is suspended.
    PrepareForSuspend,
}

/// Convert a raw signal number to the corresponding `ResumeCommand`.
/// Returns `None` for uninteresting signals (including invalid numbers).
fn signal_to_resume_command(signo: i32) -> Option<ResumeCommand> {
    match Signal::try_from(signo) {
        Ok(Signal::SIGCONT) => Some(ResumeCommand::ReinitTerminal),
        Ok(Signal::SIGTSTP) => Some(ResumeCommand::PrepareForSuspend),
        _ => None,
    }
}

pub(crate) fn run_app(mode: ConnectionMode) -> io::Result<()> {
    tracing::info!("[tai-tui] run_app starting");
    // Ensure the keystore keypair exists before we try to connect to the
    // daemon.  If no keypair has been generated yet, this creates one on the
    // fly so the client can unlock the daemon without requiring a manual
    // setup step.
    if let Err(e) = ensure_keypair() {
        tracing::error!("[tai-tui] failed to ensure keystore keypair: {e}");
    }

    let app_socket_path = match &mode {
        ConnectionMode::UnixSocket(path) => path.clone(),
        ConnectionMode::Tcp { addr, .. } => addr.clone(),
    };
    let (client_tx, client_rx) = std::sync::mpsc::channel::<ClientMessage>();
    let (shutdown_tx, shutdown_rx) = std::sync::mpsc::channel::<()>();
    let (ui_tx, ui_rx) = channel::bounded::<UiEvent>(UI_EVENT_CHANNEL_SIZE);

    let picker = build_picker();

    // Spawn the background image worker that handles SVG rasterisation and
    // terminal protocol encoding without blocking the UI thread.
    let worker = ImageWorker::spawn(picker);

    // Use the self-pipe trick to catch SIGCONT and SIGTSTP on any POSIX
    // platform (the signalfd approach used here previously was Linux-only).
    // Compatible with Linux and macOS.
    //
    // signal_hook installs signal handlers that atomically write the signal
    // number to a pipe; the terminal-event thread monitors the pipe's read
    // end via mio::Poll alongside stdin and the notification pipe.
    //
    // NOTE: In raw mode, termios ISIG is disabled, so pressing Ctrl+Z in the
    // terminal sends byte 0x1A to stdin as a regular character — it does NOT
    // generate SIGTSTP.  The pipe only catches external SIGTSTP (kill -TSTP,
    // shell job control).  For Ctrl+Z keyboard suspend, add an explicit
    // KeyCode::Char('z') + Ctrl match in the page event handlers that calls
    // handle_resume_command(PrepareForSuspend, ...) and returns early.
    //
    // O_NONBLOCK ensures the read-end drain loop never blocks (the pipe is
    // drained inside the Token(2) handler).  O_CLOEXEC prevents the pipe fds
    // from leaking to child processes on fork+exec.
    let (signal_rx, signal_tx) = nix::unistd::pipe()?;
    fcntl(&signal_rx, F_SETFD(FdFlag::FD_CLOEXEC))?;
    fcntl(&signal_rx, F_SETFL(OFlag::O_NONBLOCK))?;
    fcntl(&signal_tx, F_SETFD(FdFlag::FD_CLOEXEC))?;
    signal_pipe::register(Signal::SIGCONT as i32, signal_tx.try_clone()?)?;
    signal_pipe::register(Signal::SIGTSTP as i32, signal_tx)?;
    let mut signal_rx_file: std::fs::File = signal_rx.into();
    let signal_rx_fd = signal_rx_file.as_raw_fd();

    let connection_ui_tx = ui_tx.clone();
    let connection_task = thread::spawn(move || {
        let result = run_daemon_connection_with_mode(
            mode,
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

    crossterm::terminal::enable_raw_mode()?;
    let mut stdout = io::stdout();
    crossterm::execute!(
        stdout,
        crossterm::terminal::EnterAlternateScreen,
        crossterm::event::EnableMouseCapture
    )?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    // Spawn a background thread that reads terminal events via crossterm and
    // forwards them through a crossbeam channel so the main loop can block on
    // all event sources simultaneously via select!.
    //
    // The thread uses mio::Poll to wait on THREE sources:
    //   1. stdin (fd 0) — for crossterm events (keyboard, mouse, resize)
    //   2. a notification pipe — for clean shutdown signalling
    //   3. a signal pipe — for SIGCONT/SIGTSTP (suspend/resume)
    //
    // This is truly event-driven: the thread parks in poll with no
    // timeout and zero CPU usage while idle.
    let (terminal_tx, terminal_rx) = channel::unbounded::<Event>();
    let (notify_tx, mut notify_rx) = pipe::new()?;
    let (resume_tx, resume_rx) = channel::unbounded::<ResumeCommand>();

    let mut poll = Poll::new()?;
    poll.registry()
        .register(&mut notify_rx, Token(0), Interest::READABLE)?;

    let stdin_fd = io::stdin().as_raw_fd();
    let mut stdin_source = mio::unix::SourceFd(&stdin_fd);
    poll.registry()
        .register(&mut stdin_source, Token(1), Interest::READABLE)?;

    // Register the signal pipe with the mio poll instance so the terminal
    // thread can wait on it alongside stdin and the notification pipe.
    let mut sig_source = mio::unix::SourceFd(&signal_rx_fd);
    poll.registry()
        .register(&mut sig_source, Token(2), Interest::READABLE)?;

    let terminal_handle = thread::spawn(move || {
        let mut events = Events::with_capacity(3);
        loop {
            // Block in poll until stdin data, shutdown signal,
            // or a caught signal (SIGCONT / SIGTSTP).
            if let Err(e) = poll.poll(&mut events, None) {
                if e.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                tracing::warn!("[tai-tui] terminal mio poll error: {e}");
                break;
            }
            for event in &events {
                match event.token() {
                    Token(0) => {
                        // Shutdown via pipe — writer end was dropped
                        // or an error occurred.  Return unconditionally
                        // so the thread exits and the main loop can
                        // join it during cleanup.
                        return;
                    }
                    Token(1) => {
                        // stdin pty closed — terminal emulator was
                        // killed or the SSH session dropped.  Break
                        // out so the main loop sees the channel close
                        // and shuts down cleanly.
                        if event.is_read_closed() || event.is_error() {
                            return;
                        }
                    }
                    Token(2) => {
                        // Drain all pending signals from the self-pipe,
                        // logging and discarding read errors so a
                        // transient fd issue doesn't hang the thread.
                        loop {
                            let mut buf = [0u8; 4];
                            match signal_rx_file.read(&mut buf) {
                                Ok(4) => {
                                    let signo = i32::from_ne_bytes(buf);
                                    if let Some(cmd) = signal_to_resume_command(signo) {
                                        let _ = resume_tx.send(cmd);
                                    }
                                }
                                Ok(_) => break,
                                Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => break,
                                Err(e) => {
                                    tracing::warn!("[tai-tui] signal pipe read error: {e}");
                                    break;
                                }
                            }
                        }
                    }
                    _ => {}
                }
            }
            // Drain all pending crossterm events.  Our mio instance was woken
            // because stdin is readable; crossterm's own internal mio was also
            // woken and has already buffered parsed events, so read() will
            // return immediately without blocking.
            loop {
                match event::poll(Duration::ZERO) {
                    Ok(true) => match event::read() {
                        Ok(ev) => {
                            if terminal_tx.send(ev).is_err() {
                                return;
                            }
                        }
                        Err(_) => break,
                    },
                    Ok(false) => break,
                    Err(_) => break,
                }
            }
        }
    });

    let mut app = App::new(app_socket_path);
    app.image_job_tx = Some(worker.job_tx);
    client_tx
        .send(ClientMessage::ListSessions)
        .map_err(|e| io::Error::new(io::ErrorKind::BrokenPipe, e.to_string()))?;
    client_tx
        .send(ClientMessage::ListAccounts)
        .map_err(|e| io::Error::new(io::ErrorKind::BrokenPipe, e.to_string()))?;
    let result = run_ui_loop(
        &mut terminal,
        &mut app,
        &client_tx,
        &ui_rx,
        worker.result_rx,
        &terminal_rx,
        &resume_rx,
    )
    .map_err(io::Error::from);

    // Signal the image worker to shut down and wait for it to finish.
    app.image_job_tx = None;
    let _ = worker.handle.join();

    let _ = shutdown_tx.send(());
    drop(client_tx);

    // Signal the terminal thread to stop by closing the notification pipe,
    // then wait for it to exit.  This must happen *before* disable_raw_mode
    // so the thread isn't still blocked in crossterm when we restore the
    // terminal.
    drop(notify_tx);
    let _ = terminal_handle.join();

    crossterm::terminal::disable_raw_mode()?;
    crossterm::execute!(
        terminal.backend_mut(),
        crossterm::terminal::LeaveAlternateScreen,
        crossterm::event::DisableMouseCapture
    )?;
    terminal.show_cursor()?;
    // Clear the terminal-native progress bar now that the TUI is exiting.
    terminal_progress::update_terminal_progress(None, None);

    match connection_task.join() {
        Ok(Ok(())) => {}
        Ok(Err(error)) => return Err(error.into()),
        Err(_) => {
            return Err(io::Error::other("daemon connection thread panicked"));
        }
    }

    result
}

fn run_ui_loop(
    terminal: &mut Terminal<CrosstermBackend<std::io::Stdout>>,
    app: &mut App,
    client_tx: &std::sync::mpsc::Sender<ClientMessage>,
    ui_rx: &channel::Receiver<UiEvent>,
    image_result_rx: channel::Receiver<ImageResult>,
    terminal_rx: &channel::Receiver<Event>,
    resume_rx: &channel::Receiver<ResumeCommand>,
) -> Result<(), ClientError> {
    // Render the initial frame immediately so the user sees the UI before
    // any events arrive (the select! below would otherwise block forever).
    app.update_viewport_from_terminal_size();
    app.clamp_scroll_state();
    terminal.draw(|frame| render(frame, app))?;
    if app.fullscreen_image_idx.is_none() {
        terminal.show_cursor()?;
    }
    if app.page == Page::Chat && app.attached_session_id.is_some() {
        terminal_progress::update_terminal_progress(
            app.attached_last_prompt_tokens,
            app.attached_context_window,
        );
    }

    let mut dirty = false;

    while !app.should_quit {
        // Wait for an event from any source.  The thread is blocked in the
        // kernel here — zero CPU usage while idle.
        select! {
            recv(terminal_rx) -> msg => {
                if let Ok(event) = msg {
                    handle_terminal_event(event, app, client_tx)?;
                    dirty = true;
                }
            }
            recv(ui_rx) -> msg => {
                match msg {
                    Ok(event) => {
                        if handle_ui_event(event, app, client_tx)? {
                            dirty = true;
                        }
                    }
                    Err(_) => {
                        // Daemon channel disconnected — treat as closed.
                        app.should_quit = true;
                    }
                }
            }
            recv(image_result_rx) -> msg => {
                if let Ok(result) = msg {
                    app.apply_image_result(result);
                    dirty = true;
                }
            }
            recv(resume_rx) -> msg => {
                if let Ok(cmd) = msg {
                    dirty = handle_resume_command(cmd, terminal)?;
                }
            }
        }

        // Drain all remaining events from every channel before rendering
        // so that a burst (e.g. fast touchpad scrolling) is consumed in a
        // single batch and triggers only one repaint.
        loop {
            let mut progress = false;

            while let Ok(event) = terminal_rx.try_recv() {
                handle_terminal_event(event, app, client_tx)?;
                progress = true;
                dirty = true;
            }
            while let Ok(msg) = ui_rx.try_recv() {
                progress = true;
                if handle_ui_event(msg, app, client_tx)? {
                    dirty = true;
                }
            }
            while let Ok(result) = image_result_rx.try_recv() {
                app.apply_image_result(result);
                progress = true;
                dirty = true;
            }
            while let Ok(cmd) = resume_rx.try_recv() {
                progress = true;
                dirty = handle_resume_command(cmd, terminal)?;
            }

            // If none of the channels had anything new the drain is
            // complete and we can proceed to render.
            if !progress {
                break;
            }
        }

        // Skip rendering entirely when nothing has changed.
        if !dirty {
            continue;
        }
        dirty = false;

        // Consume the frame's accumulated scroll delta in one batch.
        app.apply_scroll_delta();

        // Update viewport dimensions and clamp scroll *outside* the
        // terminal.draw closure so that render never mutates app state.
        app.update_viewport_from_terminal_size();
        app.clamp_scroll_state();

        // Hide the cursor while the fullscreen overlay is active.
        if app.fullscreen_image_idx.is_some() {
            terminal.hide_cursor()?;
        }

        terminal.draw(|frame| render(frame, app))?;

        // Re-show the cursor once the overlay is dismissed.
        if app.fullscreen_image_idx.is_none() {
            terminal.show_cursor()?;
        }

        // Update the terminal-native progress bar only when the
        // underlying data or page has changed since the last frame.
        if app.progress_dirty {
            app.progress_dirty = false;
            if app.page == Page::Chat && app.attached_session_id.is_some() {
                terminal_progress::update_terminal_progress(
                    app.attached_last_prompt_tokens,
                    app.attached_context_window,
                );
            } else {
                terminal_progress::update_terminal_progress(None, None);
            }
        }
    }

    Ok(())
}

/// React to a suspend/resume signal from the terminal-event thread.
///
/// Returns `true` when re-rendering is necessary (`ReinitTerminal`),
/// or `false` when the terminal was only torn down (`PrepareForSuspend`).
fn handle_resume_command(
    cmd: ResumeCommand,
    terminal: &mut Terminal<CrosstermBackend<std::io::Stdout>>,
) -> io::Result<bool> {
    match cmd {
        ResumeCommand::ReinitTerminal => {
            tracing::info!("[tai-tui] reinitialising terminal after resume");
            crossterm::terminal::enable_raw_mode()?;
            crossterm::execute!(
                terminal.backend_mut(),
                crossterm::terminal::EnterAlternateScreen,
                crossterm::event::EnableMouseCapture,
            )?;
            terminal.clear()?;
            Ok(true)
        }
        ResumeCommand::PrepareForSuspend => {
            tracing::info!("[tai-tui] restoring terminal for suspend");
            crossterm::terminal::disable_raw_mode()?;
            crossterm::execute!(
                terminal.backend_mut(),
                crossterm::event::DisableMouseCapture,
                crossterm::terminal::LeaveAlternateScreen,
            )?;
            // Suspend the process.  When SIGCONT resumes us the
            // terminal-event thread will send ReinitTerminal.
            raise(Signal::SIGSTOP)?;
            Ok(false)
        }
    }
}

pub(crate) fn handle_terminal_event(
    event: Event,
    app: &mut App,
    client_tx: &std::sync::mpsc::Sender<ClientMessage>,
) -> Result<(), ClientError> {
    // Fullscreen image overlay takes priority over page content.
    if app.fullscreen_image_idx.is_some() {
        return handle_fullscreen_event(event, app, client_tx);
    }
    match app.page {
        Page::SessionManager => handle_session_manager_event(event, app, client_tx),
        Page::AIProviders => handle_ai_providers_event(event, app, client_tx),
        Page::Chat => handle_chat_event(event, app, client_tx),
        Page::Settings => handle_settings_event(event, app, client_tx),
        Page::Home => handle_home_event(event, app, client_tx),
    }
}

fn handle_home_event(
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
    match key.code {
        // Esc returns to the previous page the user was on.
        KeyCode::Esc => {
            app.set_page(app.previous_page);
        }
        // Navigate menu with j/k or Up/Down
        KeyCode::Up | KeyCode::Char('k') => {
            if app.home_selection > 0 {
                app.home_selection -= 1;
            }
        }
        KeyCode::Down | KeyCode::Char('j') => {
            if app.home_selection < HOME_MENU_ITEMS.len() - 1 {
                app.home_selection += 1;
            }
        }
        // Select a menu item
        KeyCode::Enter => match HOME_MENU_ITEMS[app.home_selection] {
            HomeMenuItem::Sessions => {
                app.set_page(Page::SessionManager);
                let _ = client_tx.send(ClientMessage::ListSessions);
                let _ = client_tx.send(ClientMessage::SubscribeSessionsSummary);
            }
            HomeMenuItem::AIProviders => {
                app.set_page(Page::AIProviders);
                let _ = client_tx.send(ClientMessage::ListAccounts);
            }
            HomeMenuItem::Settings => {
                app.set_page(Page::Settings);
            }
            HomeMenuItem::Exit => {
                app.should_quit = true;
            }
        },
        // Letter shortcuts for each menu item
        KeyCode::Char('s') => {
            app.set_page(Page::SessionManager);
            let _ = client_tx.send(ClientMessage::ListSessions);
            let _ = client_tx.send(ClientMessage::SubscribeSessionsSummary);
        }
        KeyCode::Char('t') => {
            app.set_page(Page::Settings);
        }
        KeyCode::Char('q') => {
            app.should_quit = true;
        }
        _ => {}
    }
    Ok(())
}

fn handle_settings_event(
    event: Event,
    app: &mut App,
    _client_tx: &std::sync::mpsc::Sender<ClientMessage>,
) -> Result<(), ClientError> {
    let Event::Key(key) = event else {
        return Ok(());
    };
    if key.kind != KeyEventKind::Press {
        return Ok(());
    }
    match key.code {
        // Ctrl+C quits from the settings page
        KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            app.should_quit = true;
        }
        // Esc returns to the home page
        KeyCode::Esc => {
            app.set_page(Page::Home);
        }
        _ => {}
    }
    Ok(())
}

/// Handle events while the fullscreen image overlay is active.
///
/// Only `Esc` (dismiss) and `Ctrl+C` (quit) are accepted; all other events
/// are silently consumed.
fn handle_fullscreen_event(
    event: Event,
    app: &mut App,
    _client_tx: &std::sync::mpsc::Sender<ClientMessage>,
) -> Result<(), ClientError> {
    let Event::Key(key) = event else {
        return Ok(());
    };
    if key.kind != KeyEventKind::Press {
        return Ok(());
    }
    match key.code {
        KeyCode::Esc => {
            app.fullscreen_image_idx = None;
        }
        KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            app.should_quit = true;
        }
        _ => {}
    }
    Ok(())
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
                    app.set_page(Page::SessionManager);
                    client_tx
                        .send(ClientMessage::ListSessions)
                        .map_err(broken_pipe)?;
                    client_tx
                        .send(ClientMessage::SubscribeSessionsSummary)
                        .map_err(broken_pipe)?;
                }
                KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    app.set_page(Page::Settings);
                }
                KeyCode::Esc => {
                    // Save where we came from so Home can return to Chat.
                    app.previous_page = Page::Chat;
                    app.home_selection = 0;
                    app.set_page(Page::Home);
                }
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
                        ShellCommand::Unlock { method } => {
                            if let Some(echo) = shell_command_echo(&ShellCommand::Unlock {
                                method: method.clone(),
                            }) {
                                app.push_text(echo);
                            }
                            match resolve_private_key(&method) {
                                Ok(private_key) => {
                                    let _ = client_tx.send(ClientMessage::Unlock { private_key });
                                }
                                Err(e) => {
                                    app.push_text(format!("[error] {e}"));
                                    return Ok(());
                                }
                            }
                        }
                        ShellCommand::AddCredential {
                            ref service,
                            ref credential_type,
                            ref fields,
                            unlock,
                        } => {
                            if let Some(echo) = shell_command_echo(&ShellCommand::AddCredential {
                                service: service.clone(),
                                credential_type: credential_type.clone(),
                                fields: fields.clone(),
                                unlock,
                            }) {
                                app.push_text(echo);
                            }
                            match build_add_credential_message(
                                service.clone(),
                                credential_type.clone(),
                                fields.clone(),
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
                        ShellCommand::RemoveCredential { ref service } => {
                            if let Some(echo) =
                                shell_command_echo(&ShellCommand::RemoveCredential {
                                    service: service.clone(),
                                })
                            {
                                app.push_text(echo);
                            }
                            let _ = client_tx.send(ClientMessage::RemoveCredential {
                                service: service.clone(),
                            });
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
        // While the user is dragging the scrollbar thumb, route all
        // mouse events through the drag handler regardless of whether
        // the cursor is inside or outside the narrow scrollbar column.
        Event::Mouse(mouse) if app.scrollbar_dragging => {
            match mouse.kind {
                MouseEventKind::Drag(MouseButton::Left) => {
                    app.scroll_to_track_row(mouse.row, app.history_viewport.height);
                }
                MouseEventKind::Up(MouseButton::Left) => {
                    app.scrollbar_dragging = false;
                }
                _ => {
                    // Any other mouse event (scroll, right-click, etc.)
                    // cancels the drag.
                    app.scrollbar_dragging = false;
                }
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
                // Left-click on an image opens it fullscreen.  We render the
                // existing `StatefulProtocol` directly by index (no re-decode
                // of the raw image bytes).
                MouseEventKind::Down(MouseButton::Left) => {
                    if let Some(idx) = find_history_item_at_row(app, mouse.row)
                        && matches!(&app.client.history[idx], HistoryItem::Image(_))
                    {
                        app.fullscreen_image_idx = Some(idx);
                    }
                }
                _ => {}
            }
        }
        // Left-click (and drag) in the scrollbar column.
        Event::Mouse(mouse) if mouse_in_scrollbar_column(mouse.column, mouse.row) => {
            if matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left)) {
                app.scrollbar_dragging = true;
                app.scroll_to_track_row(mouse.row, app.history_viewport.height);
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
        KeyCode::PageUp => {
            app.session_mgr.scroll_up_page();
        }
        KeyCode::PageDown => {
            app.session_mgr.scroll_down_page();
        }
        KeyCode::Enter => {
            if let Some(sel) = app.session_mgr.selection
                && let Some(session) = app.session_mgr.sessions.get(sel)
            {
                let session_id = session.session_id;
                app.set_page(Page::Chat);
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
            tracing::info!("[tai-tui] pressing n on session list -> CreateSession");
            client_tx
                .send(ClientMessage::CreateSession {
                    title: None,
                    parent_session_id: None,
                    working_dir: None,
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
            app.previous_page = Page::SessionManager;
            app.home_selection = 0;
            app.set_page(Page::Home);
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
                app.set_page(Page::Chat);
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

fn handle_ai_providers_event(
    event: Event,
    app: &mut App,
    client_tx: &std::sync::mpsc::Sender<ClientMessage>,
) -> Result<(), ClientError> {
    // If credential input is active, handle that first.
    if app.ai_providers.credential_target.is_some() {
        return handle_ai_providers_credential_key(event, app, client_tx);
    }
    match app.ai_providers.view {
        AIProvidersView::List => handle_ai_providers_list_key(event, app, client_tx),
        AIProvidersView::NewForm => handle_ai_providers_new_form_key(event, app, client_tx),
    }
}

fn handle_ai_providers_list_key(
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

    // If in delete-confirmation mode, handle y/n/Esc first
    if app.ai_providers.confirm_remove.is_some() {
        match key.code {
            KeyCode::Char('y') | KeyCode::Char('Y') => {
                if let Some(name) = app.ai_providers.confirm_remove.take() {
                    client_tx
                        .send(ClientMessage::RemoveAccount { name: name.clone() })
                        .map_err(broken_pipe)?;
                }
            }
            KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
                app.ai_providers.confirm_remove = None;
            }
            _ => {}
        }
        return Ok(());
    }

    match key.code {
        KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            app.should_quit = true;
        }
        KeyCode::Up | KeyCode::Char('k') => app.ai_providers.select_up(),
        KeyCode::Down | KeyCode::Char('j') => app.ai_providers.select_down(),
        KeyCode::PageUp => {
            app.ai_providers.scroll_up_page();
        }
        KeyCode::PageDown => {
            app.ai_providers.scroll_down_page();
        }

        // Remove account (with confirmation)
        KeyCode::Char('r') => {
            if let Some(sel) = app.ai_providers.selection
                && let Some(account) = app.ai_providers.accounts.get(sel)
            {
                app.ai_providers.confirm_remove = Some(account.name.clone());
            }
        }
        // New account
        KeyCode::Char('n') => {
            app.ai_providers.enter_new_form();
        }
        // Set credential (API key) for the selected account
        KeyCode::Char('c') => {
            if let Some(sel) = app.ai_providers.selection
                && let Some(account) = app.ai_providers.accounts.get(sel)
            {
                app.ai_providers.enter_credential(account.name.clone());
            }
        }
        KeyCode::Esc | KeyCode::Char('q') => {
            app.previous_page = Page::AIProviders;
            app.home_selection = 0;
            app.set_page(Page::Home);
        }
        _ => {}
    }
    Ok(())
}

/// Handle key events for the credential-input view (setting an API key
/// for an existing account).
fn handle_ai_providers_credential_key(
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

    match key.code {
        KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            app.should_quit = true;
        }
        // Enter saves the credential
        KeyCode::Enter => {
            let account_name = match app.ai_providers.credential_target.take() {
                Some(name) => name,
                None => return Ok(()),
            };
            let api_key = app.ai_providers.credential_input.text.trim().to_string();
            app.ai_providers.credential_input = InputBuffer::new();

            if api_key.is_empty() {
                app.ai_providers.add_error = Some("API key cannot be empty".to_string());
                app.ai_providers.credential_target = Some(account_name);
                return Ok(());
            }

            app.ai_providers.add_error = None;

            // Build and send the encrypted credential
            match tai_client_core::build_add_credential_message(
                account_name.clone(),
                "api_key".to_string(),
                vec![api_key],
                false,
            ) {
                Ok(msg) => {
                    let _ = client_tx.send(msg);
                    app.push_text(format!(
                        "[daemon] credential stored for account: {account_name}"
                    ));
                }
                Err(e) => {
                    app.push_text(format!(
                        "[warning] failed to encrypt API key for {account_name}: {e}"
                    ));
                }
            }
        }
        // Esc cancels
        KeyCode::Esc => {
            app.ai_providers.leave_credential();
        }
        // All other keys go to the credential input buffer
        _ => {
            app.ai_providers.credential_input.handle_key(key);
        }
    }
    Ok(())
}

fn handle_ai_providers_new_form_key(
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

    // Ctrl+C -> quit (overrides everything)
    if matches!(
        key.code,
        KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL)
    ) {
        app.should_quit = true;
        return Ok(());
    }

    // Esc -> cancel form back to list
    if key.code == KeyCode::Esc {
        app.ai_providers.leave_new_form();
        return Ok(());
    }

    // Enter -> validate, advance to next field, or submit
    if key.code == KeyCode::Enter {
        if app.ai_providers.new_name_state.is_focused() {
            let name = app.ai_providers.new_name_state.value().trim().to_string();
            if name.is_empty() {
                app.ai_providers.add_error = Some("Account name is required".to_string());
                return Ok(());
            }
            if !is_valid_account_name(&name) {
                app.ai_providers.add_error = Some(
                    "account name must be lowercase alphanumeric, hyphens, or underscores"
                        .to_string(),
                );
                return Ok(());
            }
            app.ai_providers.add_error = None;
            app.ai_providers.new_name_state.blur();
            app.ai_providers.new_provider_state.focus();
        } else if app.ai_providers.new_provider_state.is_focused() {
            app.ai_providers.new_provider_state.blur();
            app.ai_providers.new_api_key_state.focus();
        } else if app.ai_providers.new_api_key_state.is_focused() {
            submit_new_account(app, client_tx)?;
        }
        return Ok(());
    }

    // Remap j/k to Up/Down when the provider field is focused, so users
    // can navigate the SelectPrompt with the same keys as before.
    let key = match key.code {
        KeyCode::Char('j') if app.ai_providers.new_provider_state.is_focused() => KeyEvent {
            code: KeyCode::Down,
            ..key
        },
        KeyCode::Char('k') if app.ai_providers.new_provider_state.is_focused() => KeyEvent {
            code: KeyCode::Up,
            ..key
        },
        _ => key,
    };

    // Dispatch all other keys to the focused field's state
    if app.ai_providers.new_name_state.is_focused() {
        app.ai_providers.new_name_state.handle_key_event(key);
    } else if app.ai_providers.new_provider_state.is_focused() {
        app.ai_providers.new_provider_state.handle_key_event(key);
    } else if app.ai_providers.new_api_key_state.is_focused() {
        app.ai_providers.new_api_key_state.handle_key_event(key);
    }

    Ok(())
}

/// Validate the form, send AddAccount and (optionally) AddCredential
/// messages, then return to the provider list view.
fn submit_new_account(
    app: &mut App,
    client_tx: &std::sync::mpsc::Sender<ClientMessage>,
) -> Result<(), ClientError> {
    let name = app.ai_providers.new_name_state.value().trim().to_string();

    let provider_idx = app.ai_providers.new_provider_state.focused_index();
    let provider_str = PROVIDER_OPTIONS[provider_idx].slug;

    let api_key = app
        .ai_providers
        .new_api_key_state
        .value()
        .trim()
        .to_string();

    app.ai_providers.add_error = None;

    // Send AddAccount
    client_tx
        .send(ClientMessage::AddAccount {
            name: name.clone(),
            provider: provider_str.to_string(),
            base_url: None,
            streaming: None,
            retry_max_attempts: None,
            connect_timeout_secs: None,
            request_timeout_secs: None,
        })
        .map_err(broken_pipe)?;

    // If an API key was provided, encrypt and send the credential.
    if !api_key.is_empty() {
        match build_add_credential_message(
            name.clone(),
            "api_key".to_string(),
            vec![api_key],
            false,
        ) {
            Ok(msg) => {
                let _ = client_tx.send(msg);
            }
            Err(e) => {
                app.push_text(format!("[warning] failed to encrypt API key: {e}"));
            }
        }
    }

    app.ai_providers.leave_new_form();
    Ok(())
}

/// Process a single UiEvent and return whether the event was meaningful
/// (i.e., not a control-flow event like ReaderClosed).
///
/// Returns `Ok(true)` when the event warrants a re-render, `Ok(false)` for
/// control flow events, or `Err` on error.
fn handle_ui_event(
    event: UiEvent,
    app: &mut App,
    client_tx: &std::sync::mpsc::Sender<ClientMessage>,
) -> Result<bool, ClientError> {
    match event {
        UiEvent::Daemon(message) => {
            handle_daemon_message(message, app, client_tx)?;
            Ok(true)
        }
        UiEvent::ReaderClosed => {
            app.should_quit = true;
            Ok(false)
        }
    }
}

pub(crate) fn handle_daemon_message(
    message: DaemonMessage,
    app: &mut App,
    client_tx: &std::sync::mpsc::Sender<ClientMessage>,
) -> Result<(), ClientError> {
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
            // Early return so we don't fall through to dispatch_daemon_message,
            // which would push text to the chat history (duplicate / invisible
            // on the Session Manager page).
            return Ok(());
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
        DaemonMessage::SessionFailed { operation, error } => {
            // Push to chat history for the Chat page.
            app.push_text(format!("[daemon] {operation} failed: {error}"));
            // If we're on the Session Manager page, also show the error
            // right on that page so the user has immediate feedback.
            if app.page == Page::SessionManager && operation == "create_session" {
                app.session_mgr.set_error(error.clone());
            }
        }
        // ── AI Provider Accounts ──────────────────────────
        DaemonMessage::Accounts { accounts } => {
            app.handle_accounts(accounts);
            // Don't return early here — fall through to dispatch_daemon_message
            // which will push the account list to the chat history so the
            // user sees the response to their `/account` command.
        }
        DaemonMessage::AccountListFailed { error } => {
            app.push_text(format!("[daemon] failed to list accounts: {error}"));
            return Ok(());
        }
        DaemonMessage::AccountAdded { name } => {
            app.push_text(format!("[daemon] account added: {name}"));
            let _ = client_tx.send(ClientMessage::ListAccounts);
        }
        DaemonMessage::AccountAddFailed { name, error } => {
            app.ai_providers.add_error = Some(format!("{name}: {error}"));
            app.push_text(format!("[daemon] failed to add account {name}: {error}"));
            // Stay on the new-form page so the user can see the error
            // and fix it.
        }
        DaemonMessage::AccountRemoved { name } => {
            app.push_text(format!("[daemon] account removed: {name}"));
            app.ai_providers.remove_account(name);
            let _ = client_tx.send(ClientMessage::ListAccounts);
        }
        DaemonMessage::AccountRemoveFailed { name, error } => {
            app.push_text(format!("[daemon] failed to remove account {name}: {error}"));
        }

        DaemonMessage::SessionState {
            session_id,
            token_usage,
            context_window,
            last_prompt_tokens,
            working_dir,
            ..
        } => {
            // Only update progress data when the message is for the
            // currently-attached session; stale messages from a previous
            // session that the daemon is still draining should be ignored.
            if app.attached_session_id == Some(*session_id) {
                app.attached_token_usage = token_usage.clone();
                app.attached_context_window = *context_window;
                app.attached_last_prompt_tokens = *last_prompt_tokens;
                app.attached_working_dir = working_dir.clone();
                app.progress_dirty = true;
            }
            // Fall through to dispatch_daemon_message for message processing.
        }
        DaemonMessage::Done {
            token_usage: Some(usage),
            last_prompt_tokens,
            ..
        } => {
            // Capture per-request token usage (e.g. final streaming chunk).
            // This lacks a session_id, so we trust it belongs to the
            // attached session (the daemon only sends Done for active
            // requests on the session the client subscribed to).
            app.attached_token_usage = Some(usage.clone());
            app.attached_last_prompt_tokens = *last_prompt_tokens;
            app.progress_dirty = true;
            // Fall through to dispatch_daemon_message.
        }
        DaemonMessage::Done {
            token_usage: None, ..
        } => {
            // No token usage data — fall through.
        }
        DaemonMessage::ModelSelected { model } => {
            app.handle_model_selected(model);
        }
        DaemonMessage::ReasoningEffortSet { effort } => {
            app.handle_reasoning_effort_set(*effort);
        }
        DaemonMessage::SessionAccountSet { account } => {
            app.handle_session_account_set(account);
        }
        DaemonMessage::SessionWorkingDirSet { session_id, path } => {
            app.handle_session_working_dir_set(*session_id, path);
        }

        _ => {}
    }

    // Flag replay mode so `push_session_message` injects synthetic tool-call
    // lifecycle text entries (`[N] tool start …`, `[N] done`) that match
    // the live `push_tool_text` format but are not sent during replay.
    let was_replay = std::mem::replace(&mut app.replaying_history, false);
    if matches!(&message, DaemonMessage::SessionState { .. }) {
        tracing::debug!("replay: entering session-history replay mode");
        app.replaying_history = true;
    }
    let result = dispatch_daemon_message(app, message);
    app.replaying_history = was_replay;
    tracing::trace!(replaying = was_replay, "replay: restored prior replay flag");
    result?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sigcont_maps_to_reinit_terminal() {
        assert!(matches!(
            signal_to_resume_command(Signal::SIGCONT as i32),
            Some(ResumeCommand::ReinitTerminal),
        ));
    }

    #[test]
    fn sigtstp_maps_to_prepare_for_suspend() {
        assert!(matches!(
            signal_to_resume_command(Signal::SIGTSTP as i32),
            Some(ResumeCommand::PrepareForSuspend),
        ));
    }

    #[test]
    fn uninteresting_signal_returns_none() {
        assert!(signal_to_resume_command(Signal::SIGINT as i32).is_none());
        assert!(signal_to_resume_command(Signal::SIGTERM as i32).is_none());
    }

    #[test]
    fn invalid_signal_number_returns_none() {
        assert!(signal_to_resume_command(9999).is_none());
    }
}
