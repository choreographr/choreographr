//! The TUI main loop.
//!
//! Owns `run_app` — the process-wide TUI entry point that wires the
//! daemon-connection thread, the image worker, and the terminal-event thread,
//! then hands control to `run_ui_loop` — plus the event-driven `select!` loop
//! itself. The per-source event handlers it drives live in the sibling
//! `terminal_event` and `resume` modules.

use crate::backend::TuiBackend;
use crate::build_picker;
use crate::image_worker::{ImageResult, ImageWorker};
use crate::render::render;
use crate::state::{App, Page, UiEvent};
use crate::terminal::{progress, title};
use choreo_client_core::{
    ClientError, ConnectionMode, PendingContext, run_daemon_connection_with_autostart,
    run_daemon_connection_with_mode,
};
use choreo_proto::{ClientMessage, ClientMessageType};
use crossbeam_channel as channel;
use crossbeam_channel::select;
use crossterm::event::{
    self, DisableBracketedPaste, EnableBracketedPaste, Event, PopKeyboardEnhancementFlags,
    PushKeyboardEnhancementFlags,
};
#[cfg(unix)]
use mio::unix::pipe;
#[cfg(unix)]
use mio::{Events, Interest, Poll, Token};
#[cfg(unix)]
use nix::fcntl::{F_SETFD, F_SETFL, FdFlag, OFlag, fcntl};
#[cfg(unix)]
use nix::sys::signal::Signal;
use ratatui::Terminal;
#[cfg(unix)]
use signal_hook::low_level::pipe as signal_pipe;
use std::io;
#[cfg(unix)]
use std::io::Read;
#[cfg(unix)]
use std::os::unix::io::AsRawFd;
use std::{thread, time::Duration};

#[cfg(windows)]
use super::resume::notify_disconnected;
#[cfg(unix)]
use super::resume::signal_to_resume_command;
use super::resume::{ResumeCommand, handle_resume_command};
use super::terminal_event::{
    KITTY_KEYBOARD_FLAGS, UI_EVENT_QUEUE_HIGH_WATER_MARK, handle_terminal_event, handle_ui_event,
};

pub(crate) fn run_app(mode: ConnectionMode) -> io::Result<()> {
    tracing::info!("[choreo-tui] run_app starting");

    let (client_tx, client_rx) = crossbeam_channel::unbounded::<ClientMessage>();
    // The address that keys this daemon's unlock key in known_servers: the
    // actual dial address for TCP, the unix socket path otherwise. Derived
    // up front (by reference) because `mode` is moved into the connection
    // task below.
    let connection_addr = match &mode {
        ConnectionMode::UnixSocket(path) => path.clone(),
        ConnectionMode::Tcp { addr, .. } => addr.clone(),
        ConnectionMode::TcpPinned(addr) => addr.clone(),
        // In-process (embedded daemon): no dial address exists; key the
        // keystore records against the unix socket path — the same LOCAL
        // trust domain the daemon's embedded connection reports (`is_unix:
        // true`), so per-daemon keys stay consistent.
        ConnectionMode::InProcess { .. } => choreo_proto::socket_path(),
    };
    // The shutdown signal is a crossbeam control-plane channel (workspace
    // channel convention): a single one-shot send, so capacity 1 suffices.
    let (shutdown_tx, shutdown_rx) = crossbeam_channel::bounded::<()>(1);
    let (ui_tx, ui_rx) = channel::unbounded::<UiEvent>();

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
    #[cfg(unix)]
    let (signal_rx, signal_tx) = nix::unistd::pipe()?;
    #[cfg(unix)]
    fcntl(&signal_rx, F_SETFD(FdFlag::FD_CLOEXEC))?;
    #[cfg(unix)]
    fcntl(&signal_rx, F_SETFL(OFlag::O_NONBLOCK))?;
    #[cfg(unix)]
    fcntl(&signal_tx, F_SETFD(FdFlag::FD_CLOEXEC))?;
    #[cfg(unix)]
    signal_pipe::register(Signal::SIGCONT as i32, signal_tx.try_clone()?)?;
    #[cfg(unix)]
    signal_pipe::register(Signal::SIGWINCH as i32, signal_tx.try_clone()?)?;
    #[cfg(unix)]
    signal_pipe::register(Signal::SIGTSTP as i32, signal_tx)?;
    #[cfg(unix)]
    let mut signal_rx_file: std::fs::File = signal_rx.into();
    #[cfg(unix)]
    let signal_rx_fd = signal_rx_file.as_raw_fd();

    let connection_ui_tx = ui_tx.clone();
    let connection_task = thread::spawn(move || {
        // Warn once per backlog episode (reset when the queue drains below
        // half the high-water mark) so a wedged UI loop is observable without
        // spamming the log on every event while it is stalled.
        let mut queue_over_high_water_warned = false;
        // The per-message handler is hoisted into a binding so the unix-socket
        // arm below (autostart variant) and the other transports can share it
        // — only one arm ever runs, so the single move is fine.
        let handle_daemon_message = |message| {
            // The UI-event channel is unbounded, so this send can never
            // block the reader thread and can never fail on capacity: the
            // only failure is a Disconnected receiver, which means the UI
            // thread has already begun tearing down and there is no
            // consumer left to process this event.
            //
            // An unbounded channel is deliberate: with a bounded one, a
            // burst from another session (all activity is subscribed, and
            // a background session streams its own chunks/updates) could
            // fill the queue and DROP this session's streaming chunks —
            // and a dropped chunk is *not* recoverable from the next one
            // (chunks are deltas, appended by the client; only the final
            // `TurnAppended` resyncs the complete content).
            //
            // The cost is that a stalled UI event loop (slow render,
            // heavy paste, resize storm) lets this queue grow without a
            // hard cap: the daemon's drop-on-full bounds only the
            // daemon-side channel, which the reader drains immediately,
            // so it does NOT bound the queue here.  Correctness wins over
            // a hard cap (dropping the newest chunk is the exact bug this
            // replaced), so a high-water warning keeps a wedged loop
            // observable instead of silently accumulating memory.
            let _ = connection_ui_tx.send(UiEvent::Daemon(Box::new(message)));
            if connection_ui_tx.len() > UI_EVENT_QUEUE_HIGH_WATER_MARK
                && !queue_over_high_water_warned
            {
                queue_over_high_water_warned = true;
                tracing::warn!(
                    queued = connection_ui_tx.len(),
                    "ui event queue above high-water mark: a stalled render is accumulating events"
                );
            } else if connection_ui_tx.len() < UI_EVENT_QUEUE_HIGH_WATER_MARK / 2 {
                queue_over_high_water_warned = false;
            }
        };
        // Unix-socket mode connects DIRECTLY; only when the dial itself finds
        // nothing listening does client-core invoke the autostart hook (which
        // spawns the sibling daemon and waits for its socket), then retries
        // the connection. No probe, no pre-flight — the first dial is the real
        // connection attempt and is kept when it succeeds. TCP/embedded modes
        // connect through the plain dispatcher and never spawn anything.
        let result = match &mode {
            ConnectionMode::UnixSocket(socket_path) => {
                let ensure_path = socket_path.clone();
                // A separate clone for the autostart hook: `handle_daemon_message`
                // above borrows `connection_ui_tx` for the lifetime of the call,
                // so the hook cannot move it (crossbeam senders are cheap clones).
                let autostart_ui_tx = connection_ui_tx.clone();
                let mut ensure_daemon = move || {
                    // Simple status feedback while the daemon spawns and comes
                    // up (sub-second in practice): the UI loop paints this on
                    // the status line instead of the user staring at a silent
                    // screen for the whole autostart wait. Nothing is printed
                    // to the terminal directly — that would garble the
                    // alternate screen.
                    let _ = autostart_ui_tx.send(UiEvent::Status(
                        "no daemon running — starting choreographr…".to_string(),
                    ));
                    let result = crate::autostart::start_daemon(&ensure_path)
                        .map_err(|error| ClientError::DaemonStart(format!("{error:#}")));
                    // On success, replace the starting message; the first real
                    // daemon messages overwrite it in turn. On failure the
                    // connection error becomes the TUI's quit message.
                    if result.is_ok() {
                        let _ = autostart_ui_tx.send(UiEvent::Status("daemon started".to_string()));
                    }
                    result
                };
                run_daemon_connection_with_autostart(
                    socket_path,
                    &mut ensure_daemon,
                    handle_daemon_message,
                    client_rx,
                    Some(shutdown_rx),
                )
            }
            _ => run_daemon_connection_with_mode(
                mode,
                handle_daemon_message,
                client_rx,
                Some(shutdown_rx),
            ),
        };
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
    // `TuiBackend` wraps `CrosstermBackend` to work around ratatui's VS16
    // reserved-cell rendering bug; see the `backend` module docs.
    let backend = TuiBackend::new(stdout);
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
    // On Unix the thread uses mio::Poll to wait on THREE sources:
    //   1. stdin (fd 0) — for crossterm events (keyboard, mouse, resize)
    //   2. a notification pipe — for clean shutdown signalling
    //   3. a signal pipe — for SIGCONT/SIGTSTP (suspend/resume)
    //
    // This is truly event-driven: the thread parks in poll with no
    // timeout and zero CPU usage while idle.  On Windows there are no
    // signals to catch (crossterm reports resizes natively from console
    // events and there is no job-control suspend), so the thread instead
    // polls crossterm with a short timeout and watches the shutdown notify
    // between polls.
    let (terminal_tx, terminal_rx) = channel::unbounded::<Event>();
    // Shutdown notify: on Unix it is a mio pipe pair whose read end the
    // thread parks on in poll; on Windows it is a crossbeam channel.
    // Dropping the sender signals shutdown on both — the receiver observes
    // a Disconnected error.
    #[cfg(unix)]
    let (notify_tx, mut notify_rx) = pipe::new()?;
    #[cfg(windows)]
    let (notify_tx, notify_rx) = channel::unbounded::<()>();
    let (resume_tx, resume_rx) = channel::unbounded::<ResumeCommand>();
    // On Windows nothing ever sends on resume_tx (no SIGCONT/SIGTSTP), but
    // the sender must stay alive for the whole run: run_ui_loop's select!
    // blocks on resume_rx, and a dropped sender would make that arm
    // permanently ready with Disconnected, busy-spinning the main loop.
    #[cfg(windows)]
    let _resume_tx = resume_tx;

    #[cfg(unix)]
    let mut poll = Poll::new()?;
    #[cfg(unix)]
    poll.registry()
        .register(&mut notify_rx, Token(0), Interest::READABLE)?;

    #[cfg(unix)]
    let stdin_fd = io::stdin().as_raw_fd();
    #[cfg(unix)]
    let mut stdin_source = mio::unix::SourceFd(&stdin_fd);
    #[cfg(unix)]
    poll.registry()
        .register(&mut stdin_source, Token(1), Interest::READABLE)?;

    // Register the signal pipe with the mio poll instance so the terminal
    // thread can wait on it alongside stdin and the notification pipe.
    #[cfg(unix)]
    let mut sig_source = mio::unix::SourceFd(&signal_rx_fd);
    #[cfg(unix)]
    poll.registry()
        .register(&mut sig_source, Token(2), Interest::READABLE)?;

    let terminal_handle = {
        #[cfg(unix)]
        {
            thread::spawn(move || {
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
                                        Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => {
                                            break;
                                        }
                                        Err(e) => {
                                            tracing::warn!(
                                                "[choreo-tui] signal pipe read error: {e}"
                                            );
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
            })
        }
        #[cfg(windows)]
        {
            thread::spawn(move || {
                // No signal pipe on Windows: crossterm reports resize natively, and
                // there is no job-control suspend. Poll with a short timeout so the
                // shutdown notify (a dropped sender) is observed promptly.
                loop {
                    if notify_disconnected(&notify_rx) {
                        return;
                    }
                    match event::poll(Duration::from_millis(100)) {
                        Ok(true) => loop {
                            match event::read() {
                                Ok(ev) => {
                                    if terminal_tx.send(ev).is_err() {
                                        return;
                                    }
                                }
                                Err(_) => break,
                            }
                        },
                        Ok(false) => {}
                        Err(_) => {}
                    }
                }
            })
        }
    };

    let mut app = App::new();
    app.image_job_tx = Some(worker.job_tx);
    // The address that keys this daemon's unlock key in known_servers (see
    // the derivation up top — `mode` is moved by now).
    app.connection_addr = connection_addr;

    // ── Auto-unlock the daemon on connect ──────────────────────────
    //
    // Resolve the per-daemon unlock key (stored key, else the legacy local
    // key) and send an Unlock message immediately.  The daemon starts
    // locked; this transparently unlocks it so the user never needs to think
    // about lock state.  If no key resolves the daemon stays locked — session
    // operations (create, browse, delete) still work; only inference
    // requires unlocking. The key is held pending and recorded once the
    // daemon CONFIRMS (`Unlocked`), so a rejected key is never persisted.
    if let Some(private_key) = choreo_client_core::try_auto_unlock_key(&app.connection_addr) {
        tracing::info!("[choreo-tui] auto-unlocking daemon on connect");
        // The key rides the request's pending context so the daemon's reply
        // (`Unlocked`/`KeystoreUnbound`/`LockedError`) resolves it and records
        // or discards it — one confirm flow for every sender.
        let id = app.pending.send(
            &client_tx,
            ClientMessageType::Unlock {
                private_key: private_key.clone(),
            },
        );
        app.pending
            .set_context(id, PendingContext::UnlockKey(private_key));
    } else {
        tracing::info!("[choreo-tui] no unlock key available — awaiting keystore status");
        // Startup feedback while the daemon's authoritative keystore status
        // push is in flight (sent at subscribe time, a moment after connect).
        // A FRESH daemon (no binding) reports `Unbound` and the message handler
        // AUTO-BINDS it with a minted key — no user action. A daemon already
        // bound to ANOTHER client's key reports `Locked` and cannot be unlocked
        // without that key: /unlock <base64-key> supplies it (bare /unlock has
        // nothing stored to use).
        app.status = Some(
            "daemon is locked — if it is bound to another key, supply it with \
             /unlock <base64 unlock-key> (a fresh daemon binds automatically)"
                .to_string(),
        );
    }

    app.pending
        .send(&client_tx, ClientMessageType::ListSessions);
    app.pending
        .send(&client_tx, ClientMessageType::ListAccounts);
    app.pending
        .send(&client_tx, ClientMessageType::SubscribeAllActivity);
    // Emit the initial window title now that the alternate screen is active
    // and the app state can name the attached session. Seed `term_title` with
    // what was emitted so the UI loop does not re-send it unchanged.
    {
        let initial_title = app.window_title();
        title::set(&initial_title);
        app.term_title = Some(initial_title);
    }
    let result = run_ui_loop(
        &mut terminal,
        &mut app,
        &client_tx,
        &ui_rx,
        &worker.result_rx,
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
    // Clear every terminal-native record now that the TUI is exiting: the
    // native progress bar (OSC 9;4), the program-status records (OSC 7501),
    // and the window title (OSC 2). These must not outlive the TUI's
    // ownership of the display.
    progress::update(None, None);
    app.term_status.clear_all();
    title::clear();

    // Surface why the TUI exited (daemon eviction / graceful shutdown / a
    // dropped connection) once the alternate screen is gone and the message
    // is visible on the restored terminal. A normal user quit (Alt+Q)
    // leaves `quit_message` None and prints nothing.
    if let Some(message) = &app.quit_message {
        println!("{message}");
    }

    // The connection task's result is NEVER surfaced as an error: a failed
    // dial/handshake is a condition the user needs a readable explanation
    // for, not a crash. Returning it from run_app would escape main as a
    // raw anyhow 'Error: I/O error: ...' line (observed as a crash when a
    // daemon rejected an un-enrolled client). Instead the reason becomes
    // the exit message, printed on the restored terminal.
    match connection_task.join() {
        Ok(Ok(())) => {}
        Ok(Err(error)) => {
            // Overwrite (not get_or_insert) the quit message: the
            // ui_rx-disconnect arm has already inserted the generic
            // "the connection to the daemon was closed" text, and the
            // real error is strictly more specific. The version-mismatch
            // case gets actionable wording (see connection_quit_message).
            app.quit_message = Some(crate::connection_quit_message(&error));
        }
        Err(_) => {
            app.quit_message
                .get_or_insert_with(|| "daemon connection thread panicked".to_string());
        }
    }

    result
}

fn run_ui_loop(
    terminal: &mut Terminal<TuiBackend>,
    app: &mut App,
    client_tx: &crossbeam_channel::Sender<ClientMessage>,
    ui_rx: &channel::Receiver<UiEvent>,
    image_result_rx: &channel::Receiver<ImageResult>,
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
    // Flush any on-demand image fetches the initial frame queued for visible
    // images whose bytes were stripped from the turn snapshot.
    app.flush_image_fetches(client_tx);

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
                if let Ok(event) = msg {
                    if handle_ui_event(event, app, client_tx)? {
                        dirty = true;
                    }
                } else {
                    // Daemon channel disconnected — treat as closed.
                    app.should_quit = true;
                    // ReaderClosed normally carries the reason; this arm
                    // only fires if the connection thread dropped its
                    // sender without one (e.g. a panic mid-read).
                    app.quit_message.get_or_insert_with(|| {
                        "the connection to the daemon was closed".to_string()
                    });
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
                    // A stale selection must not survive a suspend/resume
                    // cycle: the viewport and scroll state are re-established
                    // on resume, so an old rectangle would highlight the
                    // wrong rows until the next mouse event cleared it.
                    if matches!(&cmd, ResumeCommand::PrepareForSuspend) {
                        app.text_selection = None;
                    }
                    dirty = handle_resume_command(cmd, terminal, app)?;
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
                if matches!(&cmd, ResumeCommand::PrepareForSuspend) {
                    app.text_selection = None;
                }
                dirty = handle_resume_command(cmd, terminal, app)?;
            }

            // If none of the channels had anything new the drain is
            // complete and we can proceed to render.
            if !progress {
                break;
            }
        }

        // Sweep the pending-request table for requests whose reply never
        // arrived within their per-kind budget. The sweep is driven by the UI
        // tick (an event just woke the loop) rather than a timer channel, per
        // the workspace's event-driven rule; a timeout surfaces on the status
        // line and drops the slot. Runs before the no-op short-circuit so a
        // control-flow-only event still lets a timeout through.
        for timeout in app.pending.expire(std::time::Instant::now()) {
            tracing::warn!(
                id = timeout.id,
                kind = ?timeout.kind,
                elapsed_secs = timeout.elapsed.as_secs_f64(),
                "request timed out without a daemon reply"
            );
            app.status = Some(format!(
                "[daemon] {:?} timed out after {:.0}s",
                timeout.kind,
                timeout.elapsed.as_secs_f64()
            ));
            dirty = true;
        }

        // Publish the terminal-visible program status and window title.
        // These write raw bytes to stdout, so they MUST run here — never
        // inside the `terminal.draw` render closure, which would interleave
        // with the frame. `term_status_dirty` is set by the event handlers
        // that change session status/title/attachment; when it is clear the
        // loop writes nothing. The desired records are computed into an
        // owned Vec first, so the immutable borrow of `app` ends before
        // `sync` takes a mutable one.
        if app.term_status_dirty {
            let desired = app.desired_status_records();
            app.term_status.sync(desired);
            app.term_status_dirty = false;

            // Dedupe the window title against the last one emitted; a title
            // change (attach/switch, SessionTitleSet, delete) also set
            // `term_status_dirty`.
            let desired_title = app.window_title();
            if app.term_title.as_deref() != Some(desired_title.as_str()) {
                title::set(&desired_title);
                app.term_title = Some(desired_title);
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

        // Flush any on-demand image fetches this frame queued for newly-visible
        // images (bytes are stripped from turn snapshots; the render path has
        // no client sender, so it queues and the UI loop sends).
        app.flush_image_fetches(client_tx);

        // Re-show the cursor once the overlay is dismissed.
        if app.fullscreen_image_target.is_none() {
            terminal.show_cursor()?;
        }

        // Clear the terminal-native progress bar when leaving Chat.
        // Updates are driven directly by the event handlers (Done,
        // SessionState) rather than through progress_dirty.
        if app.active_display_ref().is_some_and(|d| d.progress_dirty) {
            if let Some(d) = app.active_display() {
                d.progress_dirty = false;
            }
            if app.page != Page::Chat {
                progress::update(None, None);
            }
        }
    }

    Ok(())
}
