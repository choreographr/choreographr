use crate::render::{mouse_in_history_box, mouse_in_scrollbar_column, render};
use crate::state::PROVIDER_OPTIONS;
use crate::state::{
    AIProvidersView, App, InputBuffer, PAGE_SCROLL_LINES, Page, SessionManagerView, UiEvent,
    find_turn_at_row, input_inner_width,
};
use choreo_client_core::{
    ClientError, ConnectionMode, broken_pipe, build_add_credential_message,
    dispatch_daemon_message, is_valid_account_name, resolve_private_key,
    run_daemon_connection_with_mode, shell_command_echo,
};
use choreo_keystore::ensure_keypair;
use choreo_proto::{ClientMessage, DaemonMessage};
use choreo_tui::image_worker::{ImageResult, ImageWorker};
use choreo_tui::terminal_progress;
use choreo_tui::{ShellCommand, build_picker, parse_input_line};
use crossbeam::channel;
use crossbeam::select;
use crossterm::event::{
    self, DisableBracketedPaste, EnableBracketedPaste, Event, KeyCode, KeyEvent, KeyEventKind,
    KeyModifiers, KeyboardEnhancementFlags, MouseButton, MouseEventKind,
    PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
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
use tui_prompts::State;

const UI_EVENT_CHANNEL_SIZE: usize = 4096;

/// Keyboard enhancements requested from the terminal via the kitty keyboard
/// protocol (`CSI > flags u`), pushed at startup and re-pushed after resume.
///
/// `DISAMBIGUATE_ESCAPE_CODES` makes `Ctrl+letter` (e.g. Ctrl+M) arrive as an
/// unambiguous CSI-u sequence (`CSI 109;5 u`) instead of the legacy control
/// byte (0x0D — identical to Enter), while plain text keys stay as legacy
/// UTF-8 bytes.
///
/// `REPORT_ALL_KEYS_AS_ESCAPE_CODES` is deliberately **not** requested.  With
/// it enabled, kitty-protocol terminals report *every* key as a CSI-u event,
/// and text produced by an input method (IME) — e.g. Vietnamese composed by
/// OpenKey — arrives as a pure "text event" with key number 0
/// (`CSI 0;;<codepoints>u`, the third field carrying the composed text).
/// crossterm 0.29 has no `KeyEvent` text field and silently drops that third
/// field, turning the event into `KeyCode::Char('\0')`; the composed text is
/// lost and the chat input receives a NUL.  Keeping text keys in legacy
/// encoding means IME-composed text arrives as plain UTF-8 bytes, which
/// crossterm parses into the correct `Char` events.
///
/// Trade-off: with `DISAMBIGUATE_ESCAPE_CODES` alone, the *plain* Enter/Tab/
/// Backspace keys stay in their legacy encodings (per the protocol), so Ctrl+M
/// stays distinct from Enter while those keys remain shell-friendly.  Key
/// combinations with no legacy byte encoding — e.g. Shift+Enter — are still
/// reported as CSI-u (`CSI 13;2 u`), so modifier variants like Shift+Enter
/// (newline) remain distinguishable.
///
/// Terminals that do not implement the kitty protocol simply ignore the push
/// and keep legacy encodings (there Ctrl+M arrives as Enter); supporting
/// those terminals is out of scope for now.
const KITTY_KEYBOARD_FLAGS: KeyboardEnhancementFlags = KeyboardEnhancementFlags::from_bits_retain(
    KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES.bits(),
);

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
    tracing::info!("[choreo-tui] run_app starting");
    // Ensure the keystore keypair exists before we try to connect to the
    // daemon.  If no keypair has been generated yet, this creates one on the
    // fly so the client can unlock the daemon without requiring a manual
    // setup step.
    if let Err(e) = ensure_keypair() {
        tracing::error!("[choreo-tui] failed to ensure keystore keypair: {e}");
    }

    let (client_tx, client_rx) = std::sync::mpsc::channel::<ClientMessage>();
    let (shutdown_tx, shutdown_rx) = std::sync::mpsc::channel::<()>();
    let (ui_tx, ui_rx) = channel::bounded::<UiEvent>(UI_EVENT_CHANNEL_SIZE);

    let picker = build_picker();

    // Spawn the background image worker that handles SVG rasterisation and
    // terminal protocol encoding without blocking the UI thread.
    let worker = ImageWorker::spawn(picker);

    // Use the self-pipe trick to catch SIGCONT, SIGTSTP, and SIGWINCH on any
    // POSIX platform (the signalfd approach used here previously was Linux-only).
    // Compatible with Linux and macOS.
    //
    // signal_hook installs signal handlers that atomically write a byte to a
    // pipe; the terminal-event thread monitors the pipe's read end via
    // mio::Poll alongside stdin and the notification pipe.
    //
    // SIGWINCH is essential here: crossterm 0.29 only reports terminal resizes
    // as `Event::Resize` from a SIGWINCH handler it installs internally, and that
    // event is only produced while draining crossterm events (inside
    // `event::poll`/`event::read`).  Without registering SIGWINCH on our own
    // pipe, a resize (e.g. toggling fullscreen in Ghostty with Ctrl+Enter) never
    // wakes the mio poll, so the resize stays undetected until the next keypress
    // and the viewport keeps the stale size — breaking the layout.  Registering
    // SIGWINCH here makes the poll wake so the drain loop below picks up the
    // queued `Event::Resize` and the app reflows immediately.
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
    signal_pipe::register(Signal::SIGWINCH as i32, signal_tx.try_clone()?)?;
    signal_pipe::register(Signal::SIGTSTP as i32, signal_tx)?;
    let mut signal_rx_file: std::fs::File = signal_rx.into();
    let signal_rx_fd = signal_rx_file.as_raw_fd();

    let connection_ui_tx = ui_tx.clone();
    let connection_task = thread::spawn(move || {
        let result = run_daemon_connection_with_mode(
            mode,
            |message| {
                // Use try_send so the daemon reader thread never blocks when
                // the UI event loop is temporarily backed up (e.g. during a
                // slow render).  Dropping a streaming chunk is acceptable
                // because the next chunk carries cumulative content and the
                // final Done/SessionMessageAppended delivers the complete
                // text; a blocking send would stall the reader thread, fill
                // the socket buffer, and cascade back to the daemon's
                // session thread — effectively killing all streaming.
                //
                // A Disconnected error (receiver dropped) is silently
                // ignored — the terminal thread has already begun tearing
                // down the UI, so there is no consumer left to process this
                // event and no point in propagating the error.
                if let Err(e) = connection_ui_tx.try_send(UiEvent::Daemon(Box::new(message)))
                    && e.is_full()
                {
                    tracing::warn!(
                        "[choreo-tui] daemon reader channel full, dropping event ({} queued)",
                        connection_ui_tx.len(),
                    );
                }
            },
            client_rx,
            Some(shutdown_rx),
        );
        if result.is_ok() {
            // ReaderClosed must always be delivered — blocking is safe here
            // because no more daemon messages are coming after this.
            let _ = connection_ui_tx.send(UiEvent::ReaderClosed);
        }
        result
    });

    crossterm::terminal::enable_raw_mode()?;
    let mut stdout = io::stdout();
    crossterm::execute!(
        stdout,
        EnableBracketedPaste,
        crossterm::terminal::EnterAlternateScreen,
        crossterm::event::EnableMouseCapture,
        PushKeyboardEnhancementFlags(KITTY_KEYBOARD_FLAGS),
    )?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    // Prime crossterm's internal event reader before the terminal thread starts
    // blocking in mio::Poll.  crossterm installs its SIGWINCH handler lazily on
    // the first `event::poll`/`event::read` call, and that handler is what turns
    // a resize into `Event::Resize`.  Without this priming, a resize that arrives
    // before the first drain (e.g. the user toggling fullscreen immediately at
    // startup) would be missed: nothing would wake the thread, the event reader
    // would never even be initialised, and the layout would stay stale until the
    // next keypress.  A zero-timeout poll is non-blocking and consumes nothing.
    let _ = event::poll(Duration::ZERO);

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
                tracing::warn!("[choreo-tui] terminal mio poll error: {e}");
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
                        //
                        // SIGWINCH is intentionally not mapped to a
                        // ResumeCommand: the point of catching it here is
                        // purely to wake the mio poll so the drain loop
                        // below runs `event::poll`/`event::read`, which is
                        // when crossterm converts its internal SIGWINCH
                        // notification into the `Event::Resize` that
                        // reflows the UI.
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
                                    tracing::warn!("[choreo-tui] signal pipe read error: {e}");
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

    let mut app = App::new();
    app.image_job_tx = Some(worker.job_tx);

    // ── Auto-unlock the daemon on connect ──────────────────────────
    //
    // Resolve the private key (raw key file, or encrypted key with
    // TAI_PASSPHRASE env var) and send an Unlock message immediately.
    // The daemon starts locked; this transparently unlocks it so the
    // user never needs to think about lock state.  If no key is
    // available the daemon stays locked — session operations (create,
    // browse, delete) still work; only inference requires unlocking.
    if let Some(private_key) = choreo_client_core::try_auto_unlock_key() {
        tracing::info!("[choreo-tui] auto-unlocking daemon on connect");
        let _ = client_tx.send(ClientMessage::Unlock { private_key });
    } else {
        tracing::info!("[choreo-tui] no private key available — daemon starts locked");
    }

    client_tx
        .send(ClientMessage::ListSessions)
        .map_err(|e| io::Error::new(io::ErrorKind::BrokenPipe, e.to_string()))?;
    client_tx
        .send(ClientMessage::ListAccounts)
        .map_err(|e| io::Error::new(io::ErrorKind::BrokenPipe, e.to_string()))?;
    client_tx
        .send(ClientMessage::SubscribeAllActivity)
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
        DisableBracketedPaste,
        crossterm::event::DisableMouseCapture,
        PopKeyboardEnhancementFlags,
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
    if app.fullscreen_image_target.is_none() {
        terminal.show_cursor()?;
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
        if app.fullscreen_image_target.is_some() {
            terminal.hide_cursor()?;
        }

        terminal.draw(|frame| render(frame, app))?;

        // Re-show the cursor once the overlay is dismissed.
        if app.fullscreen_image_target.is_none() {
            terminal.show_cursor()?;
        }

        // Clear the terminal-native progress bar when leaving Chat.
        // Updates are driven directly by the event handlers (Done,
        // SessionState) rather than through progress_dirty.
        if app
            .active_display_ref()
            .map(|d| d.progress_dirty)
            .unwrap_or(false)
        {
            if let Some(d) = app.active_display() {
                d.progress_dirty = false;
            }
            if app.page != Page::Chat {
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
            tracing::info!("[choreo-tui] reinitialising terminal after resume");
            crossterm::terminal::enable_raw_mode()?;
            crossterm::execute!(
                terminal.backend_mut(),
                EnableBracketedPaste,
                crossterm::terminal::EnterAlternateScreen,
                crossterm::event::EnableMouseCapture,
                PushKeyboardEnhancementFlags(KITTY_KEYBOARD_FLAGS),
            )?;
            terminal.clear()?;
            Ok(true)
        }
        ResumeCommand::PrepareForSuspend => {
            tracing::info!("[choreo-tui] restoring terminal for suspend");
            crossterm::terminal::disable_raw_mode()?;
            crossterm::execute!(
                terminal.backend_mut(),
                crossterm::event::DisableMouseCapture,
                crossterm::terminal::LeaveAlternateScreen,
                DisableBracketedPaste,
                PopKeyboardEnhancementFlags,
            )?;
            // Suspend the process.  When SIGCONT resumes us the
            // terminal-event thread will send ReinitTerminal.
            raise(Signal::SIGSTOP)?;
            Ok(false)
        }
    }
}

/// With the kitty protocol's `REPORT_ALL_KEYS_AS_ESCAPE_CODES` enhancement, a
/// terminal reports text keys as *unshifted* codepoints with an explicit
/// SHIFT modifier (e.g. `CSI 97;2 u` for Shift+A) instead of sending the
/// shifted glyph (`'A'`) as plain text.  Reconstruct the legacy view of the
/// event — apply the US-layout shift mapping and clear the SHIFT bit — so
/// every downstream handler (chat input, filter boxes, other pages) sees
/// exactly the `Char` it would have received from a legacy terminal.
///
/// When Ctrl is held the modifier is dropped without remapping: legacy
/// terminals masked Ctrl+letter to a control byte and lost the shift
/// distinction anyway (Ctrl+Shift+M was byte 0x0D, same as Ctrl+M).
/// Non-Char keys (e.g. Shift+Enter, Shift+Tab) pass through untouched.
fn normalize_kitty_shift(event: Event) -> Event {
    let Event::Key(mut key) = event else {
        return event;
    };
    if !key.modifiers.contains(KeyModifiers::SHIFT) {
        return Event::Key(key);
    }
    let KeyCode::Char(c) = key.code else {
        return Event::Key(key);
    };
    key.modifiers.remove(KeyModifiers::SHIFT);
    if !key.modifiers.contains(KeyModifiers::CONTROL) {
        key.code = KeyCode::Char(shift_char(c));
    }
    Event::Key(key)
}

/// US-layout shift mapping for ASCII keys — the layout terminals assume when
/// producing legacy shifted text.  Non-ASCII characters pass through
/// unchanged (the terminal's layout-specific shift result cannot be
/// reconstructed from the unshifted codepoint alone).
fn shift_char(c: char) -> char {
    match c {
        'a'..='z' => (c as u8 - b'a' + b'A') as char,
        '1' => '!',
        '2' => '@',
        '3' => '#',
        '4' => '$',
        '5' => '%',
        '6' => '^',
        '7' => '&',
        '8' => '*',
        '9' => '(',
        '0' => ')',
        '-' => '_',
        '=' => '+',
        '[' => '{',
        ']' => '}',
        '\\' => '|',
        ';' => ':',
        '\'' => '"',
        ',' => '<',
        '.' => '>',
        '/' => '?',
        '`' => '~',
        other => other,
    }
}

pub(crate) fn handle_terminal_event(
    event: Event,
    app: &mut App,
    client_tx: &std::sync::mpsc::Sender<ClientMessage>,
) -> Result<(), ClientError> {
    // Normalise kitty-protocol SHIFT reporting before anything else so the
    // paste guard and all page handlers see legacy-equivalent events.
    let event = normalize_kitty_shift(event);
    // Handle bracketed-paste events before anything else.  Pasted text
    // (including embedded newlines) arrives as a single Paste(String)
    // event rather than individual key events, so it must be inserted
    // directly into whichever input buffer is active.  Without this,
    // the newlines in pasted text arrive as bare KeyCode::Enter events
    // and trigger submission of the partial text instead of inserting
    // the newline.
    if let Event::Paste(data) = &event {
        if app.fullscreen_image_target.is_some() {
            tracing::debug!("[choreo-tui] ignoring paste while fullscreen overlay is active");
            return Ok(());
        }
        tracing::debug!("[choreo-tui] handling paste event");
        return handle_paste_event(data, app);
    }

    // Terminal-resize events are handled irrespective of page or fullscreen
    // state so the viewport is refreshed on the next frame.
    if let Event::Resize(cols, rows) = &event {
        tracing::trace!("[choreo-tui] terminal resize: {cols}x{rows}");
        app.mark_terminal_resized();
    }

    // Global Ctrl+Q quits from any page before page-specific dispatch and
    // fullscreen overlay so the user can always quit.
    if let Event::Key(key) = &event
        && key.kind == KeyEventKind::Press
        && key.code == KeyCode::Char('q')
        && key.modifiers.contains(KeyModifiers::CONTROL)
    {
        tracing::info!("Ctrl+Q requested quit");
        app.should_quit = true;
        return Ok(());
    }
    // Fullscreen image overlay takes priority over page content.
    if app.fullscreen_image_target.is_some() {
        return handle_fullscreen_event(event, app, client_tx);
    }
    // The model selector overlay (Chat page) also takes priority over page
    // content, mirroring the fullscreen guard above.  Ctrl+Q is handled
    // before this point so the user can always quit while it is open.
    if app.model_selector.is_open() {
        return handle_model_selector_event(event, app, client_tx);
    }
    match app.page {
        Page::SessionManager => handle_session_manager_event(event, app, client_tx),
        Page::AIProviders => handle_ai_providers_event(event, app, client_tx),
        Page::Chat => handle_chat_event(event, app, client_tx),
    }
}

/// Insert pasted text into whichever input buffer is currently active.
///
/// Routes the paste to the appropriate field based on the current page
/// and sub-view.  On the Chat page the command input receives the paste;
/// on the AI Providers page either the credential input or a form field
/// (account name / API key) receives it.
fn handle_paste_event(data: &str, app: &mut App) -> Result<(), ClientError> {
    match app.page {
        Page::Chat => {
            // While the model selector is open, pasted text goes into its
            // filter box rather than the main command input.
            if app.model_selector.is_open() {
                tracing::debug!("[choreo-tui] pasting into model selector filter");
                app.model_selector.filter.insert_str_at_cursor(data);
                return Ok(());
            }
            tracing::debug!("[choreo-tui] pasting into chat input buffer");
            app.input.insert_str_at_cursor(data);
            app.ensure_input_cursor_visible();
        }
        Page::AIProviders => {
            // The credential page takes priority over the wizard views (it is
            // reached right after account creation, with `view` already reset
            // to List), so route pastes there first.
            if app.ai_providers.credential_target.is_some() {
                tracing::debug!("[choreo-tui] pasting into credential input");
                app.ai_providers.credential_input.insert_str_at_cursor(data);
            } else if app.ai_providers.view == AIProvidersView::SetSlug {
                // Phase 2: bulk-insert into the slug field.
                tracing::debug!("[choreo-tui] pasting into new-account slug field");
                paste_into_text_state(&mut app.ai_providers.slug_state, data);
            }
        }
        _ => {}
    }
    Ok(())
}

/// Efficiently insert a string at the cursor position of a `tui_prompts::State`.
///
/// The trait's `push(char)` method rebuilds the entire string on every call,
/// making a char-by-char loop O(n*m).  This helper does the same thing in a
/// single pass, which matters for large pastes (e.g. API keys, base64 data).
fn paste_into_text_state(state: &mut impl tui_prompts::State, data: &str) {
    let pos = state.position();
    let suffix = state.value().chars().skip(pos).collect::<String>();
    // Truncate the value to the cursor position (char-indexed)…
    let truncated: String = state.value().chars().take(pos).collect();
    // …then append the pasted text and the original suffix.
    let new_value = if pos == state.len() {
        // Fast path: cursor at end — just append.
        let mut v = truncated;
        v.push_str(data);
        v
    } else {
        // Cursor in the middle — build in one allocation.
        let mut v = String::with_capacity(truncated.len() + data.len() + suffix.len());
        v.push_str(&truncated);
        v.push_str(data);
        v.push_str(&suffix);
        v
    };
    *state.value_mut() = new_value;
    *state.position_mut() = pos + data.chars().count();
}

/// Handle events while the fullscreen image overlay is active.
///
/// Only `Esc` (dismiss) is accepted; all other events are silently
/// consumed.  Quit is handled via Ctrl+Q on the Chat page.
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
    if key.code == KeyCode::Esc {
        app.fullscreen_image_target = None;
    }
    Ok(())
}

/// Handle events while the model selector overlay is open (Chat page).
///
/// Up/Down move the highlight, Enter selects the highlighted model and
/// closes (sending `SetModel`), Esc dismisses without changing anything,
/// and every other key feeds the filter box.  Non-key events are ignored;
/// quit is handled via Ctrl+Q at the terminal-event level.
fn handle_model_selector_event(
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
    // Esc dismisses the overlay without changing the model.
    if key.code == KeyCode::Esc {
        tracing::debug!("[choreo-tui] model selector dismissed");
        app.model_selector.close();
        return Ok(());
    }
    // Enter selects the highlighted model (if any) and closes.  An empty
    // filtered list (e.g. a filter with no matches) simply closes.
    if key.code == KeyCode::Enter {
        if let Some(model) = app.model_selector.submit() {
            tracing::info!(%model, "model selector: selecting model");
            client_tx
                .send(ClientMessage::SetModel { model })
                .map_err(broken_pipe)?;
        }
        return Ok(());
    }
    match key.code {
        KeyCode::Up => app.model_selector.move_up(),
        KeyCode::Down => app.model_selector.move_down(),
        // Everything else goes to the filter input (characters, backspace,
        // word deletes, cursor movement).  `filter_key` returns false for
        // Enter/Esc, which are handled above.
        _ => {
            app.model_selector.filter_key(key);
        }
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
            // Any keypress clears transient status/error messages.
            app.status = None;
            app.error = None;
            // Don't clear help on Ctrl+H itself — let the toggle arm handle it.
            if key.code != KeyCode::Char('h') || !key.modifiers.contains(KeyModifiers::CONTROL) {
                app.show_ctrl_help = false;
            }
            match key.code {
                // All Ctrl+ combinations delegated to a dedicated handler.
                _ if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    handle_chat_ctrl_key(key, app, client_tx)?;
                }
                // Alt+Enter → continue generation
                KeyCode::Enter if key.modifiers.contains(KeyModifiers::ALT) => {
                    if app.attached_session_id.is_some() {
                        tracing::debug!("Alt+Enter continuing generation");
                        let request_id = app.next_request_id;
                        app.next_request_id = app.next_request_id.wrapping_add(1);
                        app.active_display().unwrap().active.insert(request_id);
                        client_tx
                            .send(ClientMessage::ContinueGeneration { request_id })
                            .map_err(broken_pipe)?;
                        app.scroll_to(0);
                    } else {
                        tracing::debug!("Alt+Enter ignored — no session attached");
                        app.status = Some("no session attached".to_string());
                    }
                }
                KeyCode::Esc => {
                    if app.attached_session_id.is_some() {
                        tracing::debug!("Esc stopping generation");
                        client_tx
                            .send(ClientMessage::Cancel { request_id: 0 })
                            .map_err(broken_pipe)?;
                    } else {
                        tracing::debug!("Esc ignored — no session attached");
                        app.status = Some("no session attached".to_string());
                    }
                }
                KeyCode::Up => {
                    let inner = app
                        .last_terminal_size
                        .map(|(w, _)| input_inner_width(w))
                        .unwrap_or(78);
                    if app.input.is_on_first_visual_line(inner) {
                        app.navigate_history_up();
                    } else {
                        app.input.cursor_up(inner);
                        app.ensure_input_cursor_visible();
                    }
                }
                KeyCode::Down => {
                    let inner = app
                        .last_terminal_size
                        .map(|(w, _)| input_inner_width(w))
                        .unwrap_or(78);
                    if app.input.is_on_last_visual_line(inner) {
                        app.navigate_history_down();
                    } else {
                        app.input.cursor_down(inner);
                        app.ensure_input_cursor_visible();
                    }
                }
                KeyCode::Enter if key.modifiers.contains(KeyModifiers::SHIFT) => {
                    app.input.insert_char_at_cursor('\n');
                    app.ensure_input_cursor_visible();
                }
                KeyCode::Enter => {
                    let line = app.input.text.trim().to_string();
                    app.input.clear();
                    app.commit_to_history();
                    match parse_input_line(&line, &mut app.next_request_id, app.attached_session_id)
                    {
                        ShellCommand::Empty => {}
                        ShellCommand::InvalidCancel(value) => {
                            app.status = Some(format!("invalid request id: {value}"))
                        }
                        ShellCommand::UnknownCommand(error) => app.status = Some(error),
                        ShellCommand::Send(message) => {
                            // Client-side validation: reject reasoning slugs that
                            // the attached model's capability set does not include.
                            // This provides faster feedback than waiting for the
                            // daemon to reply with ReasoningEffortSetFailed.
                            if let ClientMessage::SetReasoningEffort { ref effort } = message
                                && effort != "off"
                            {
                                let valid = app
                                    .active_display_ref()
                                    .and_then(|d| d.reasoning_capability.as_ref())
                                    .map(|c| c.available_effort_levels.iter().any(|l| l == effort))
                                    .unwrap_or(true); // No capability cached → let daemon validate
                                if !valid {
                                    tracing::warn!(
                                        %effort,
                                        "TUI rejected reasoning slug not in capability set",
                                    );
                                    app.status = Some(format!(
                                        "model does not support reasoning '{effort}'"
                                    ));
                                    return Ok(());
                                }
                            }
                            let message = match message {
                                ClientMessage::CreateSession {
                                    title,
                                    parent_session_id,
                                    working_dir,
                                    context_config,
                                    account_name,
                                    selected_model,
                                    reasoning_effort,
                                } => ClientMessage::CreateSession {
                                    title,
                                    parent_session_id,
                                    // Inherit fields from the currently attached session
                                    // when not explicitly provided by the user.
                                    working_dir: working_dir.or_else(|| {
                                        app.active_display_ref().and_then(|d| d.working_dir.clone())
                                    }),
                                    context_config,
                                    account_name: account_name.or_else(|| {
                                        app.active_display_ref()
                                            .and_then(|d| d.account_name.clone())
                                    }),
                                    selected_model: selected_model.or_else(|| {
                                        app.active_display_ref()
                                            .and_then(|d| d.selected_model.clone())
                                    }),
                                    reasoning_effort: reasoning_effort.or_else(|| {
                                        app.active_display_ref()
                                            .and_then(|d| d.reasoning_effort.clone())
                                    }),
                                },
                                other => other,
                            };
                            if let Some(echo) =
                                shell_command_echo(&ShellCommand::Send(message.clone()))
                            {
                                app.status = Some(echo);
                            }
                            if let ClientMessage::RunInput { request_id, .. } = &message {
                                app.error = None;
                                app.active_display().unwrap().active.insert(*request_id);
                            }
                            client_tx.send(message).map_err(broken_pipe)?;

                            // Scroll the history view to the bottom so the user can
                            // see their submitted message appear as the daemon
                            // processes it.  Without this, a user who has scrolled
                            // up to read past conversation would remain scrolled up
                            // and miss the new content arriving at the bottom.
                            app.scroll_to(0);
                        }
                        ShellCommand::Unlock { method } => match resolve_private_key(&method) {
                            Ok(private_key) => {
                                let _ = client_tx.send(ClientMessage::Unlock { private_key });
                            }
                            Err(e) => {
                                tracing::warn!("[choreo-tui] unlock failed: {e}");
                            }
                        },
                        ShellCommand::AddCredential {
                            ref service,
                            ref credential_type,
                            ref fields,
                            unlock,
                        } => {
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
                                    tracing::warn!("[choreo-tui] add credential failed: {e}");
                                }
                            }
                        }
                        ShellCommand::RemoveCredential { ref service } => {
                            if let Some(echo) =
                                shell_command_echo(&ShellCommand::RemoveCredential {
                                    service: service.clone(),
                                })
                            {
                                app.status = Some(echo);
                            }
                            let _ = client_tx.send(ClientMessage::RemoveCredential {
                                service: service.clone(),
                            });
                        }
                        ShellCommand::Undo => {
                            if let Some(echo) = shell_command_echo(&ShellCommand::Undo) {
                                app.status = Some(echo);
                            }
                            let _ = client_tx.send(ClientMessage::Undo);
                        }
                        ShellCommand::Redo => {
                            if let Some(echo) = shell_command_echo(&ShellCommand::Redo) {
                                app.status = Some(echo);
                            }
                            let _ = client_tx.send(ClientMessage::Redo);
                        }
                        ShellCommand::Continue => {
                            if let Some(echo) = shell_command_echo(&ShellCommand::Continue) {
                                app.status = Some(echo);
                            }
                            if app.attached_session_id.is_some() {
                                let request_id = app.next_request_id;
                                app.next_request_id = app.next_request_id.wrapping_add(1);
                                app.active_display().unwrap().active.insert(request_id);
                                client_tx
                                    .send(ClientMessage::ContinueGeneration { request_id })
                                    .map_err(broken_pipe)?;
                                app.scroll_to(0);
                            } else {
                                app.status = Some("no session attached".to_string());
                            }
                        }
                        ShellCommand::Stop => {
                            if let Some(echo) = shell_command_echo(&ShellCommand::Stop) {
                                app.status = Some(echo);
                            }
                            // Send Cancel with request_id 0 (the CANCEL_ALL sentinel)
                            // to stop whatever request is currently active on the
                            // attached session and all its children.
                            if app.attached_session_id.is_some() {
                                client_tx
                                    .send(ClientMessage::Cancel { request_id: 0 })
                                    .map_err(broken_pipe)?;
                            } else {
                                app.status = Some("no session attached".to_string());
                            }
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
                    app.ensure_input_cursor_visible();
                }
                KeyCode::Char(_) => {
                    handle_input_key(key, &mut app.input);
                    app.ensure_input_cursor_visible();
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
        // Left-click (and drag) in the scrollbar column.
        // This must be checked BEFORE the drag handler so that a new click
        // on the scrollbar always reaches this handler, even when the drag
        // flag is still set from a previous click.  Only treated as a
        // scrollbar when one is actually rendered: on sessions whose history
        // fits the viewport the column is blank, and a click there must not
        // arm the drag state (which would swallow the next history click).
        Event::Mouse(mouse)
            if app.scrollbar_visible()
                && mouse_in_scrollbar_column(
                    mouse.column,
                    mouse.row,
                    app.history_viewport.width,
                    app.history_viewport.height,
                ) =>
        {
            match mouse.kind {
                MouseEventKind::Down(MouseButton::Left) => {
                    app.scrollbar_dragging = true;

                    // Check whether the click lands on a user-text marker.
                    let top_slot = 2 * mouse.row as usize;
                    let bot_slot = top_slot + 1;

                    let marker_hit = app
                        .active_display_ref()
                        .unwrap()
                        .markers
                        .iter()
                        .find(|m| m.virtual_slot == top_slot || m.virtual_slot == bot_slot);

                    if let Some(marker) = marker_hit {
                        app.scroll_to_content_line(marker.content_line);
                    } else {
                        app.scroll_to_track_row(mouse.row, app.history_viewport.height);
                    }
                }
                MouseEventKind::Drag(MouseButton::Left) => {
                    app.scroll_to_track_row(mouse.row, app.history_viewport.height);
                }
                MouseEventKind::ScrollUp => {
                    app.scrollbar_scroll_up();
                }
                MouseEventKind::ScrollDown => {
                    app.scrollbar_scroll_down();
                }
                _ => {}
            }
        }
        // While the user is dragging the scrollbar thumb, route all
        // mouse events through the drag handler regardless of whether
        // the cursor is inside or outside the narrow scrollbar column.
        // This arm catches drags that have exited the scrollbar column.
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
        Event::Mouse(mouse)
            if mouse_in_history_box(
                mouse.column,
                mouse.row,
                app.history_viewport.width,
                app.history_viewport.height,
            ) =>
        {
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
                // Left-click on an image opens it fullscreen.  Uses
                // `TurnImageLayout` (populated by `rebuild_height_prefix`) to
                // map the click's content-line offset within the turn to
                // the correct image index — no text-height recomputation
                // or cache dependency needed.
                MouseEventKind::Down(MouseButton::Left) => {
                    // A click on the reasoning header row toggles the
                    // collapsible reasoning section.  Checked before image
                    // hit-testing so the header wins when they overlap.
                    let reasoning_toggle =
                        find_turn_at_row(app, mouse.row).and_then(|(turn_idx, offset)| {
                            let display = app.active_display_ref()?;
                            let layout = display.turn_layouts.get(turn_idx)?;
                            let (start, end) = layout.reasoning_header_range?;
                            (offset >= start && offset < end)
                                .then(|| display.visible_turn_ids.get(turn_idx).copied())
                                .flatten()
                        });
                    if let Some(turn_id) = reasoning_toggle {
                        if let Some(display) = app.active_display() {
                            display.toggle_reasoning(turn_id);
                        }
                    } else if let Some((turn_idx, offset)) = find_turn_at_row(app, mouse.row)
                        && let Some(layout) =
                            app.active_display_ref().unwrap().turn_layouts.get(turn_idx)
                        && let Some(img_idx) = layout
                            .image_ranges
                            .iter()
                            .position(|&(start, end)| offset >= start && offset < end)
                        && let Some(turn_id) = app
                            .active_display_ref()
                            .unwrap()
                            .visible_turn_ids
                            .get(turn_idx)
                            .copied()
                        && let Some(session_id) = app.active_session_id
                    {
                        app.fullscreen_image_target = Some((session_id, turn_id, img_idx));
                    }
                }
                _ => {}
            }
        }
        Event::Mouse(_) => {}
        _ => {}
    }
    Ok(())
}

/// Handle Ctrl+key combinations on the Chat page.
///
/// Each arm logs a `tracing::debug!` event for observability. Unknown
/// Ctrl+combinations (e.g. Ctrl+Backspace, Ctrl+Home) are forwarded to
/// the input handler so standard text-editing shortcuts still work.
fn handle_chat_ctrl_key(
    key: KeyEvent,
    app: &mut App,
    client_tx: &std::sync::mpsc::Sender<ClientMessage>,
) -> Result<(), ClientError> {
    match key.code {
        KeyCode::Char('r') => {
            let (capability, current_effort) = app
                .active_display_ref()
                .map(|d| (d.reasoning_capability.clone(), d.reasoning_effort.clone()))
                .unwrap_or_default();
            match capability.as_ref().and_then(|c| {
                let current = current_effort.unwrap_or_else(|| "off".to_string());
                c.cycle_from(&current).map(|next| (current, next))
            }) {
                Some((current, next)) => {
                    if let Some(d) = app.active_display() {
                        d.reasoning_effort = Some(next.clone());
                    }
                    app.status = Some(format!("reasoning: {next}"));
                    tracing::info!(
                        session_id = ?app.attached_session_id,
                        current = %current,
                        next = %next,
                        "Ctrl+R cycling reasoning effort",
                    );
                    client_tx
                        .send(ClientMessage::SetReasoningEffort { effort: next })
                        .map_err(broken_pipe)?;
                }
                None => {
                    app.status = Some("model does not support reasoning".to_string());
                }
            }
        }
        KeyCode::Char('h') => {
            tracing::debug!("Ctrl+H toggling help overlay");
            app.show_ctrl_help = !app.show_ctrl_help;
        }
        KeyCode::Char('s') => {
            tracing::debug!("Ctrl+S navigating to session manager");
            // Highlight the session the user was just viewing so returning
            // to the session list lands on the session they came from (the
            // selection survives the ListSessions round-trip via
            // `pending_select`).
            if let Some(session_id) = app.attached_session_id {
                app.session_mgr.select_session(session_id);
            }
            app.set_page(Page::SessionManager);
            client_tx
                .send(ClientMessage::ListSessions)
                .map_err(broken_pipe)?;
            client_tx
                .send(ClientMessage::SubscribeSessionsSummary)
                .map_err(broken_pipe)?;
        }
        KeyCode::Char('a') => {
            tracing::debug!("Ctrl+A navigating to AI provider accounts");
            app.set_page(Page::AIProviders);
            client_tx
                .send(ClientMessage::ListAccounts)
                .map_err(broken_pipe)?;
        }
        KeyCode::Char('m') => {
            tracing::debug!("Ctrl+M opening model selector");
            app.model_selector.open();
            client_tx
                .send(ClientMessage::ListModels)
                .map_err(broken_pipe)?;
        }
        // Ctrl+C is a deliberate no-op on the chat page (no copy/sigint
        // in raw mode). Absorb it here so it doesn't fall through to the
        // input handler which would insert a literal 'c'.
        KeyCode::Char('c') => {
            tracing::debug!("Ctrl+C ignored on chat page");
        }
        KeyCode::Up => {
            tracing::debug!("Ctrl+Up undo");
            client_tx.send(ClientMessage::Undo).map_err(broken_pipe)?;
        }
        KeyCode::Down => {
            tracing::debug!("Ctrl+Down redo");
            client_tx.send(ClientMessage::Redo).map_err(broken_pipe)?;
        }
        // Ctrl+Left, Ctrl+Right, Ctrl+Backspace, Ctrl+Delete, Ctrl+Home,
        // Ctrl+End, etc. are text-editing shortcuts that should still work
        // in the input box.
        _ => {
            handle_input_key(key, &mut app.input);
            app.ensure_input_cursor_visible();
        }
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
                app.reset_for_session_switch(session_id);
                app.attached_session_id = Some(session_id);
                client_tx
                    .send(ClientMessage::AttachSession { session_id })
                    .map_err(broken_pipe)?;
            }
        }
        KeyCode::Char('i') => app.session_mgr.enter_detail(),
        KeyCode::Char('n') => {
            tracing::info!("[choreo-tui] pressing n on session list -> CreateSession");
            client_tx
                .send(ClientMessage::CreateSession {
                    title: None,
                    parent_session_id: None,
                    // Inherit fields from the currently attached session.
                    working_dir: app.active_display_ref().and_then(|d| d.working_dir.clone()),
                    context_config: None,
                    account_name: app
                        .active_display_ref()
                        .and_then(|d| d.account_name.clone()),
                    selected_model: app
                        .active_display_ref()
                        .and_then(|d| d.selected_model.clone()),
                    reasoning_effort: app
                        .active_display_ref()
                        .and_then(|d| d.reasoning_effort.clone()),
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
            app.set_page(Page::Chat);
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
        KeyCode::Char('b') | KeyCode::Esc => {
            app.session_mgr.leave_detail();
        }
        KeyCode::Enter => {
            if let Some(ref detail) = app.session_mgr.detail_data {
                let session_id = detail.session_id;
                app.set_page(Page::Chat);
                let _ = client_tx.send(ClientMessage::UnsubscribeSessionsSummary);
                app.reset_for_session_switch(session_id);
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
        AIProvidersView::SelectProvider => {
            handle_ai_providers_select_provider_key(event, app, client_tx)
        }
        AIProvidersView::SetSlug => handle_ai_providers_set_slug_key(event, app, client_tx),
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
        // New account: start the 2-phase wizard (provider → slug, then
        // the credential page)
        KeyCode::Char('n') => {
            app.ai_providers.enter_new_account();
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
            app.set_page(Page::Chat);
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

            // Build and send the encrypted credential, auto-unlocking
            // the daemon so the credential is immediately usable.
            match choreo_client_core::build_add_credential_message(
                account_name.clone(),
                "api_key".to_string(),
                vec![api_key],
                true,
            ) {
                Ok(msg) => {
                    let _ = client_tx.send(msg);
                    app.status = Some(format!(
                        "[daemon] credential stored for account: {account_name}"
                    ));
                }
                Err(e) => {
                    app.status = Some(format!(
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

/// Phase 1 of the new-account wizard: navigate the provider list and
/// confirm a selection, which moves the flow to phase 2 (slug entry).
fn handle_ai_providers_select_provider_key(
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
        KeyCode::Up | KeyCode::Char('k') => app.ai_providers.provider_up(),
        KeyCode::Down | KeyCode::Char('j') => app.ai_providers.provider_down(),
        // PgUp/PgDn page the selection (the render window follows it), so
        // browsing the ~90 providers takes a few keypresses, not a
        // row-by-row walk.
        KeyCode::PageUp => app.ai_providers.provider_page_up(),
        KeyCode::PageDown => app.ai_providers.provider_page_down(),
        // Enter confirms the highlighted provider and advances to the slug
        // entry page (phase 2).
        KeyCode::Enter => app.ai_providers.confirm_provider(),
        // Esc aborts the whole wizard back to the account list.
        KeyCode::Esc => app.ai_providers.leave_new_account(),
        _ => {}
    }
    Ok(())
}

/// Phase 2 of the new-account wizard: enter the account slug.  Enter
/// validates and submits `AddAccount`, then redirects to the credential
/// page for the freshly created account.
fn handle_ai_providers_set_slug_key(
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
        // Esc backs out to the provider picker (phase 1).
        KeyCode::Esc => app.ai_providers.back_to_provider(),
        // Enter validates the slug and creates the account.
        KeyCode::Enter => {
            let slug = app.ai_providers.slug_state.value().trim().to_string();
            if slug.is_empty() {
                app.ai_providers.add_error = Some("Account slug is required".to_string());
                return Ok(());
            }
            if !is_valid_account_name(&slug) {
                app.ai_providers.add_error = Some(
                    "slug must be lowercase alphanumeric, hyphens, or underscores".to_string(),
                );
                return Ok(());
            }
            app.ai_providers.add_error = None;
            submit_new_account(app, client_tx)?;
        }
        // All other keys go to the slug input buffer.
        _ => {
            app.ai_providers.slug_state.handle_key_event(key);
        }
    }
    Ok(())
}

/// Send AddAccount for the slug entered in phase 2, then redirect to the
/// credential page so the user can immediately paste an API key.
fn submit_new_account(
    app: &mut App,
    client_tx: &std::sync::mpsc::Sender<ClientMessage>,
) -> Result<(), ClientError> {
    let slug = app.ai_providers.slug_state.value().trim().to_string();
    let provider_str = app
        .ai_providers
        .selected_provider_slug()
        // The provider is always chosen before this page is reachable; if it
        // somehow is not, fall back to the first option rather than indexing
        // (the terminal `unwrap_or_default` is unreachable: PROVIDER_OPTIONS
        // is a non-empty compile-time table).
        .or_else(|| PROVIDER_OPTIONS.first().map(|p| p.slug))
        .unwrap_or_default()
        .to_string();

    app.ai_providers.add_error = None;

    // Create the account (no credential yet — that's the next page).
    client_tx
        .send(ClientMessage::AddAccount {
            name: slug.clone(),
            provider: provider_str,
            base_url: None,
            streaming: None,
            retry_max_attempts: None,
            connect_timeout_secs: None,
            request_timeout_secs: None,
            total_timeout_secs: None,
        })
        .map_err(broken_pipe)?;

    // Reset the wizard and immediately jump to the add-credential page for
    // the account we just created.
    app.ai_providers.finish_new_account();
    app.ai_providers.enter_credential(slug);
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
            handle_daemon_message(*message, app, client_tx)?;
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
    // dispatch in choreo_client_core handle the rest (text notifications,
    // stream appends, image assembly, etc.).
    match &message {
        DaemonMessage::SessionCreated {
            session_id,
            account_name,
            selected_model,
            reasoning_effort,
            ..
        } => {
            // Already known — nothing to do, and skip the generic dispatch too.
            if app
                .session_mgr
                .sessions
                .iter()
                .any(|s| s.session_id == *session_id)
            {
                return Ok(());
            }
            app.handle_session_created(
                *session_id,
                account_name.clone(),
                selected_model.clone(),
                reasoning_effort.clone(),
                client_tx,
            )?;
            // Early return so we don't fall through to dispatch_daemon_message,
            // which would push text to the chat history (duplicate / invisible
            // on the Session Manager page).
            return Ok(());
        }
        DaemonMessage::SessionAttached { session_id } => {
            app.handle_session_attached(*session_id);
        }
        DaemonMessage::SessionStatusChanged {
            session_id,
            status,
            last_modified,
        } => {
            app.handle_session_status_changed(*session_id, status, *last_modified);
            // Return early: the generic dispatch would call the same handler
            // again via the TurnEventHandler trait, and the sessions-page
            // re-sort must only run once.
            return Ok(());
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
        DaemonMessage::SessionFailed {
            operation, error, ..
        } => {
            app.error = Some(format!("[daemon] {operation} failed: {error}"));
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
            app.error = Some(format!("[daemon] failed to list accounts: {error}"));
            return Ok(());
        }
        DaemonMessage::AccountAdded { name } => {
            app.status = Some(format!("[daemon] account added: {name}"));
            let _ = client_tx.send(ClientMessage::ListAccounts);
        }
        DaemonMessage::AccountAddFailed { name, error } => {
            app.ai_providers.add_error = Some(format!("{name}: {error}"));
            app.error = Some(format!("[daemon] failed to add account {name}: {error}"));
            // Stay on the new-form page so the user can see the error
            // and fix it.
        }
        DaemonMessage::AccountRemoved { name } => {
            app.status = Some(format!("[daemon] account removed: {name}"));
            app.ai_providers.remove_account(name);
            let _ = client_tx.send(ClientMessage::ListAccounts);
        }
        DaemonMessage::AccountRemoveFailed { name, error } => {
            app.error = Some(format!("[daemon] failed to remove account {name}: {error}"));
        }
        // A credential mutation does not carry the updated account list, so
        // re-request it: the accounts page renders `has_credential` per
        // account, and without a refresh it would keep showing the stale
        // pre-credential state until the user leaves and re-enters the page.
        DaemonMessage::CredentialAdded { service } => {
            app.status = Some(format!("[daemon] credential added: {service}"));
            let _ = client_tx.send(ClientMessage::ListAccounts);
        }
        DaemonMessage::CredentialRemoved { service } => {
            app.status = Some(format!("[daemon] credential removed: {service}"));
            let _ = client_tx.send(ClientMessage::ListAccounts);
        }

        DaemonMessage::SessionState {
            session_id,
            token_usage,
            context_window,
            last_prompt_tokens,
            working_dir,
            status,
            ..
        } => {
            // Only update progress data when the message is for the
            // currently-attached session; stale messages from a previous
            // session that the daemon is still draining should be ignored.
            //
            // Only overwrite with Some values — a SessionState that arrives
            // after Done may not yet reflect the just-completed turn's
            // usage, and a blind `= *last_prompt_tokens` would wipe the
            // value Done just set.
            if app.attached_session_id == Some(*session_id) {
                {
                    let display = app.display_for(*session_id);
                    if let Some(usage) = token_usage {
                        display.token_usage = Some(*usage);
                    }
                    if let Some(cw) = context_window {
                        display.context_window = Some(*cw);
                    }
                    if let Some(tokens) = last_prompt_tokens {
                        display.last_prompt_tokens = Some(*tokens);
                    }
                    display.working_dir = working_dir.clone();
                    display.progress_dirty = true;
                }
                app.attached_status = Some(status.clone());
            }
            // Fall through to dispatch_daemon_message for message processing.
        }
        DaemonMessage::Done {
            session_id,
            token_usage,
            last_prompt_tokens,
            ..
        } => {
            // Progress-bar updates only apply to the currently-attached
            // session.  A Done for a background session (received via
            // SubscribeAllActivity) must not clobber the attached session's
            // token display — the generic dispatch below routes the
            // per-session bookkeeping (request cleanup, token_usage) to the
            // correct session display via handle_done.
            if app.attached_session_id == Some(*session_id) {
                // Capture per-request token usage at turn end.
                // Only set progress_dirty when we actually write data —
                // a Done message without token info doesn't change state.
                let has_data = token_usage.is_some() || last_prompt_tokens.is_some();

                if let Some(usage) = token_usage {
                    let display = app.display_for(*session_id);
                    display.token_usage = Some(*usage);
                    // Many providers only supply token_usage without the
                    // separate last_prompt_tokens field.  Fall back to
                    // input_tokens so the progress bar always updates.
                    if last_prompt_tokens.is_none() {
                        display.last_prompt_tokens = Some(usage.input_tokens);
                    }
                }
                if let Some(tokens) = last_prompt_tokens {
                    let display = app.display_for(*session_id);
                    display.last_prompt_tokens = Some(*tokens);
                }

                if has_data {
                    let display = app.display_for(*session_id);
                    display.progress_dirty = true;
                    // Push the update directly instead of waiting for the
                    // render loop — bypasses any timing issues with the
                    // progress_dirty flag getting consumed before render.
                    if let (Some(cw), Some(tokens)) =
                        (display.context_window, display.last_prompt_tokens)
                    {
                        terminal_progress::update_terminal_progress(Some(tokens), Some(cw));
                    }
                }
            }
            // Fall through to dispatch_daemon_message.
        }
        DaemonMessage::ModelSelected {
            session_id,
            model,
            reasoning_capability,
            ..
        } => {
            app.handle_model_selected(*session_id, model, reasoning_capability.clone());
        }
        DaemonMessage::ReasoningEffortSet {
            session_id, effort, ..
        } => {
            app.handle_reasoning_effort_set(*session_id, effort.clone());
        }
        DaemonMessage::ReasoningEffortSetFailed {
            session_id,
            effort,
            error,
            ..
        } => {
            tracing::warn!(%effort, %error, "reasoning effort rejected by daemon");
            // Reset only the session the rejection belongs to — a background
            // session's rejection must not flip the viewed session's effort.
            let display = app.display_for(*session_id);
            display.reasoning_effort = Some("off".to_string());
            app.status = Some(format!("reasoning effort rejected: {error}"));
        }
        DaemonMessage::SessionAccountSet {
            session_id,
            account,
            ..
        } => {
            app.handle_session_account_set(*session_id, account);
        }
        DaemonMessage::ContextWindowResolved {
            session_id,
            context_window,
        } => {
            if app.attached_session_id == Some(*session_id)
                && let Some(display) = app.active_display()
            {
                display.context_window = Some(*context_window);
            }
        }
        DaemonMessage::SessionWorkingDirSet { session_id, path } => {
            app.handle_session_working_dir_set(*session_id, path);
        }
        DaemonMessage::SessionTitleSet { session_id, title } => {
            app.handle_session_title_set(*session_id, title);
        }
        // TokenUsageUpdate is dispatched through the generic handler below.
        DaemonMessage::LiveOutputTokenCount {
            session_id,
            output_tokens,
            ..
        } => {
            // Route the live count to the session the message belongs to,
            // not the one the user happens to be viewing.  The TUI subscribes
            // to all session activity (SubscribeAllActivity), so these arrive
            // for every streaming session — writing to the active display
            // would let a background session's token count bleed into the
            // status bar of the session being viewed.  Each session keeps its
            // own live count; reset_for_session_switch preserves it, so the
            // count stays correct both while streaming in the background and
            // after the user switches to that session.
            let display = app.display_for(*session_id);
            display.live_output_tokens = *output_tokens;
        }

        DaemonMessage::Models {
            models,
            selected_model,
        } => {
            if app.model_selector.is_open() {
                // While the selector is open, the reply populates the popup
                // and must NOT fall through to the generic dispatch, which
                // would print the whole list into the chat history.  Prefer
                // the daemon's reported selection, falling back to the
                // display's cached model when it is absent.
                let selected = selected_model.clone().or_else(|| {
                    app.active_display_ref()
                        .and_then(|d| d.selected_model.clone())
                });
                tracing::debug!(
                    count = models.len(),
                    ?selected,
                    "model selector: received model list"
                );
                app.model_selector.apply_models(models.clone(), selected);
                return Ok(());
            }
            // Selector closed: fall through to dispatch_daemon_message so
            // `/model` keeps printing the list into the chat history.
        }
        DaemonMessage::ModelsFailed { error } if app.model_selector.is_open() => {
            tracing::warn!(%error, "model selector: failed to list models");
            app.model_selector.apply_error(error.clone());
            return Ok(());
        }
        // Selector closed: fall through to the generic error handling.
        _ => {}
    }

    // Dispatch remaining variants through the generic turn-event handler.
    dispatch_daemon_message(&message, app);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::Marker;
    use crate::test_util::test_app;
    use choreo_proto::Turn;

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
    fn sigwinch_is_not_a_resume_command() {
        // SIGWINCH is registered on the self-pipe purely to wake the terminal
        // thread's mio poll; the actual resize is then reported by crossterm's
        // `event::poll`/`event::read` drain as `Event::Resize`.  Mapping it to a
        // ResumeCommand would be wrong — resizing must never trigger a terminal
        // teardown/reinit.
        assert!(signal_to_resume_command(Signal::SIGWINCH as i32).is_none());
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

    // ── Kitty keyboard protocol ──

    #[test]
    fn kitty_flags_disambiguate_without_report_all_keys() {
        // Ctrl+M must arrive as a distinct CSI-u key event (CSI 109;5 u);
        // DISAMBIGUATE_ESCAPE_CODES alone gives us that because Ctrl+letter is
        // a "disambiguated" key, while plain text stays as legacy bytes.
        assert!(KITTY_KEYBOARD_FLAGS.contains(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES));
        // REPORT_ALL_KEYS_AS_ESCAPE_CODES must stay OFF: it makes IME-composed
        // text arrive as a `CSI 0;;<codepoints>u` text event, which crossterm
        // 0.29 mangles into `Char('\0')` (dropping the composed text) — so
        // Vietnamese/other IME input would type as nothing.
        assert!(
            !KITTY_KEYBOARD_FLAGS
                .contains(KeyboardEnhancementFlags::REPORT_ALL_KEYS_AS_ESCAPE_CODES),
            "REPORT_ALL_KEYS breaks IME text input (crossterm drops the associated-text field)"
        );
    }

    #[test]
    fn shift_char_maps_us_layout() {
        assert_eq!(shift_char('a'), 'A');
        assert_eq!(shift_char('z'), 'Z');
        assert_eq!(shift_char('1'), '!');
        assert_eq!(shift_char('0'), ')');
        assert_eq!(shift_char('-'), '_');
        assert_eq!(shift_char('='), '+');
        assert_eq!(shift_char('['), '{');
        assert_eq!(shift_char(']'), '}');
        assert_eq!(shift_char('\\'), '|');
        assert_eq!(shift_char(';'), ':');
        assert_eq!(shift_char('\''), '"');
        assert_eq!(shift_char(','), '<');
        assert_eq!(shift_char('.'), '>');
        assert_eq!(shift_char('/'), '?');
        assert_eq!(shift_char('`'), '~');
        // Non-ASCII and already-shifted chars pass through unchanged.
        assert_eq!(shift_char('é'), 'é');
        assert_eq!(shift_char('A'), 'A');
    }

    #[test]
    fn normalize_kitty_shift_applies_mapping_and_clears_shift() {
        // Shift+A arrives as Char('a') + SHIFT (kitty CSI 97;2 u); the
        // normaliser must produce the legacy-equivalent Char('A') with no
        // modifiers.
        let out = normalize_kitty_shift(Event::Key(KeyEvent::new(
            KeyCode::Char('a'),
            KeyModifiers::SHIFT,
        )));
        assert_eq!(
            out,
            Event::Key(KeyEvent::new(KeyCode::Char('A'), KeyModifiers::NONE))
        );

        // Shift+1 → '!'.
        let out = normalize_kitty_shift(Event::Key(KeyEvent::new(
            KeyCode::Char('1'),
            KeyModifiers::SHIFT,
        )));
        assert_eq!(
            out,
            Event::Key(KeyEvent::new(KeyCode::Char('!'), KeyModifiers::NONE))
        );
    }

    #[test]
    fn normalize_kitty_shift_drops_shift_when_ctrl_held() {
        // Ctrl+Shift+M arrives as Char('m') + CONTROL + SHIFT (CSI 109;6 u).
        // Legacy Ctrl+Shift+M was byte 0x0D — identical to Ctrl+M — so the
        // normaliser drops SHIFT without remapping the char.
        let out = normalize_kitty_shift(Event::Key(KeyEvent::new(
            KeyCode::Char('m'),
            KeyModifiers::CONTROL | KeyModifiers::SHIFT,
        )));
        assert_eq!(
            out,
            Event::Key(KeyEvent::new(KeyCode::Char('m'), KeyModifiers::CONTROL))
        );
    }

    #[test]
    fn normalize_kitty_shift_keeps_alt() {
        // Alt+Shift+A arrives as Char('a') + ALT + SHIFT; legacy sent ESC 'A'
        // (Char('A') + ALT).  The mapping must keep the ALT modifier.
        let out = normalize_kitty_shift(Event::Key(KeyEvent::new(
            KeyCode::Char('a'),
            KeyModifiers::ALT | KeyModifiers::SHIFT,
        )));
        assert_eq!(
            out,
            Event::Key(KeyEvent::new(KeyCode::Char('A'), KeyModifiers::ALT))
        );
    }

    #[test]
    fn normalize_kitty_shift_leaves_other_events_untouched() {
        // Non-Char keys (Shift+Enter keeps its modifier — it inserts a
        // newline in the chat input), unmodified keys, and non-key events
        // must pass through unchanged.
        let shift_enter = Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::SHIFT));
        assert_eq!(normalize_kitty_shift(shift_enter.clone()), shift_enter);

        let plain = Event::Key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE));
        assert_eq!(normalize_kitty_shift(plain.clone()), plain);

        let ctrl_q = Event::Key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::CONTROL));
        assert_eq!(normalize_kitty_shift(ctrl_q.clone()), ctrl_q);

        let paste = Event::Paste("Hi".to_string());
        assert_eq!(normalize_kitty_shift(paste.clone()), paste);
    }

    // ── Marker click logic ──
    //
    // The scrollbar click handler (line 935) maps a mouse row to virtual
    // half-slots and looks up matching markers in app.markers.  These tests
    // verify that the data flow — from rebuild_height_prefix through marker
    // creation and the lookup pattern — produces correct scroll positions.

    fn insert_turn(app: &mut App, id: u32, user_text: &str, assistant_text: &str) {
        let turn = Turn {
            created_at: choreo_proto::TimestampMs::now(),
            undone: false,
            error: None,
            user_text: Some(user_text.into()),
            assistant_text: Some(assistant_text.into()),
            assistant_reasoning: None,
            tool_calls: vec![],
            token_usage: None,
            tool_results: vec![],
            displayed_images: vec![],
        };
        app.display_for(0).view.insert_or_replace(id, turn);
    }

    /// Simulate the scrollbar click handler's marker lookup: compute
    /// half-slots for the given `mouse_row` and scan `app.markers` for
    /// a match.  Returns the matched marker if found.
    fn find_marker_by_row(app: &App, mouse_row: u16) -> Option<&Marker> {
        let top_slot = 2 * mouse_row as usize;
        let bot_slot = top_slot + 1;
        app.active_display_ref().and_then(|d| {
            d.markers
                .iter()
                .find(|m| m.virtual_slot == top_slot || m.virtual_slot == bot_slot)
        })
    }

    #[test]
    fn marker_lookup_finds_marker_at_mouse_row() {
        let mut app = test_app();
        app.history_viewport.width = 80;
        app.history_viewport.height = 20;

        // Two user-text turns → two markers.
        insert_turn(&mut app, 0, "user a", "assistant a");
        insert_turn(&mut app, 1, "user b", "assistant b");
        app.rebuild_height_prefix();

        assert_eq!(
            app.active_display_ref().unwrap().markers.len(),
            2,
            "should have 2 markers"
        );

        // Each marker's virtual_slot must be findable by the click handler's
        // row-to-slot mapping (slot = 2*row or slot = 2*row+1).
        let markers: Vec<Marker> = app.active_display_ref().unwrap().markers.clone();
        for marker in &markers {
            let row = marker.virtual_slot / 2;
            let found = find_marker_by_row(&app, row as u16);
            assert!(
                found.is_some(),
                "marker at virtual_slot {} should be findable at row {}",
                marker.virtual_slot,
                row,
            );
            if let Some(f) = found {
                assert_eq!(
                    f.content_line, marker.content_line,
                    "found marker should match content_line"
                );
            }
        }
    }

    #[test]
    fn marker_click_scrolls_to_content_line() {
        let mut app = test_app();
        app.history_viewport.width = 80;
        app.history_viewport.height = 10;

        // Three user-text turns.
        insert_turn(&mut app, 0, "first", "response a");
        insert_turn(&mut app, 1, "second", "response b");
        insert_turn(&mut app, 2, "third", "response c");
        app.rebuild_height_prefix();

        let total = app.total_history_height();
        let vh = app.history_viewport.height as usize;

        // Collect content_lines first to avoid borrow conflict with
        // scroll_to_content_line which takes &mut self.
        let content_lines: Vec<usize> = app
            .active_display_ref()
            .unwrap()
            .markers
            .iter()
            .map(|m| m.content_line)
            .collect();

        // Clicking on each marker should scroll so that the marker's
        // content_line is at the top of the viewport.
        for &cl in &content_lines {
            app.scroll_to_content_line(cl);

            let scroll = app.effective_scroll();
            // The first visible content line at the top of the viewport
            // should be the marker's content_line.
            let first_visible = total.saturating_sub(scroll + vh);
            assert_eq!(
                first_visible, cl,
                "click on marker at content_line {} should make it the first visible line",
                cl,
            );
        }
    }

    #[test]
    fn marker_click_after_content_change_still_correct() {
        let mut app = test_app();
        app.history_viewport.width = 80;
        app.history_viewport.height = 10;

        // Initial turns.
        insert_turn(&mut app, 0, "a", "resp a");
        insert_turn(&mut app, 1, "b", "resp b");
        app.rebuild_height_prefix();

        // Add more content — markers should be recomputed.
        insert_turn(&mut app, 2, "c", "resp c");
        app.rebuild_height_prefix();

        assert_eq!(
            app.active_display_ref().unwrap().markers.len(),
            3,
            "should have 3 markers after adding content"
        );

        // Collect content_lines first to avoid borrow conflict.
        let content_lines: Vec<usize> = app
            .active_display_ref()
            .unwrap()
            .markers
            .iter()
            .map(|m| m.content_line)
            .collect();

        // Each marker should scroll to the correct content_line.
        for &cl in &content_lines {
            app.scroll_to_content_line(cl);
            let scroll = app.effective_scroll();
            let total = app.total_history_height();
            let vh = app.history_viewport.height as usize;
            let first_visible = total.saturating_sub(scroll + vh);
            assert_eq!(
                first_visible, cl,
                "click on recomputed marker should scroll correctly"
            );
        }
    }

    #[test]
    fn marker_slot_uses_final_total_as_denominator() {
        let mut app = test_app();
        app.history_viewport.width = 80;
        app.history_viewport.height = 20;

        // Add turns of varying heights.
        insert_turn(&mut app, 0, "short", "short");
        insert_turn(&mut app, 1, "longer text here", "some response that wraps");
        app.rebuild_height_prefix();

        let total = app.total_history_height();
        let virtual_track = 2 * app.history_viewport.height as usize;

        let markers: Vec<Marker> = app.active_display_ref().unwrap().markers.clone();
        for marker in &markers {
            let expected_slot = marker.content_line * virtual_track / total.max(1);
            assert_eq!(
                marker.virtual_slot,
                expected_slot.min(virtual_track.saturating_sub(1)),
                "virtual_slot should be proportional to content_line using final total as denominator"
            );
        }
    }

    // ── Scrollbar-column click gating ──

    fn click_scrollbar_column(app: &mut App, row: u16) {
        let (tx, _rx) = std::sync::mpsc::channel();
        handle_terminal_event(
            Event::Mouse(crossterm::event::MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: app.history_viewport.width,
                row,
                modifiers: KeyModifiers::NONE,
            }),
            app,
            &tx,
        )
        .expect("handle click");
    }

    #[test]
    fn scrollbar_column_click_ignored_when_no_scrollbar_rendered() {
        // Short session: the history fits the viewport, so no scrollbar is
        // drawn.  Clicking the reserved rightmost column must not arm the
        // drag state (which would otherwise swallow the next history click,
        // e.g. on the reasoning header).
        let mut app = test_app();
        app.history_viewport.width = 80;
        app.history_viewport.height = 20;
        insert_turn(&mut app, 0, "short", "short");
        app.rebuild_height_prefix();
        assert!(
            !app.scrollbar_visible(),
            "content must fit the viewport for this test"
        );

        click_scrollbar_column(&mut app, 0);
        assert!(
            !app.scrollbar_dragging,
            "a hidden scrollbar must not arm the drag state"
        );
    }

    #[test]
    fn scrollbar_column_click_arms_drag_when_scrollbar_rendered() {
        // Tall session: the history overflows the viewport, so the scrollbar
        // is drawn and clicking its column must arm the drag as before.
        let mut app = test_app();
        app.history_viewport.width = 80;
        app.history_viewport.height = 10;
        for i in 0..20 {
            insert_turn(&mut app, i, "user text", "assistant response");
        }
        app.rebuild_height_prefix();
        assert!(
            app.scrollbar_visible(),
            "content must overflow the viewport for this test"
        );

        click_scrollbar_column(&mut app, 0);
        assert!(
            app.scrollbar_dragging,
            "a visible scrollbar should arm the drag state"
        );
    }

    #[test]
    fn marker_lookup_no_match_on_empty_track() {
        let mut app = test_app();
        app.history_viewport.width = 80;
        app.history_viewport.height = 10;

        // No markers (no user_text turns).
        app.display_for(0).view.turns.clear();
        let turn = Turn {
            created_at: choreo_proto::TimestampMs::now(),
            undone: false,
            error: None,
            user_text: None,
            assistant_text: Some("assistant only".into()),
            assistant_reasoning: None,
            tool_calls: vec![],
            token_usage: None,
            tool_results: vec![],
            displayed_images: vec![],
        };
        app.display_for(0).view.insert_or_replace(0, turn);
        app.rebuild_height_prefix();
        assert!(
            app.display_for(0).markers.is_empty(),
            "should have no markers"
        );

        // No marker should be found at any row.
        for row in 0..10 {
            assert!(
                find_marker_by_row(&app, row).is_none(),
                "row {row} should not match any marker"
            );
        }
    }
}
