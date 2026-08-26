use crate::build_picker;
use crate::image_worker::{ImageResult, ImageWorker};
use crate::render::render;
use crate::state::{AccountWizardStep, App, Page, UiEvent};
use crate::terminal_progress;
use choreo_client_core::{ClientError, ConnectionMode, run_daemon_connection_with_mode};
use choreo_keystore::ensure_keypair;
use choreo_proto::ClientMessage;
use crossbeam::channel;
use crossbeam::select;
use crossterm::event::{
    self, DisableBracketedPaste, EnableBracketedPaste, Event, KeyCode, KeyEventKind, KeyModifiers,
    KeyboardEnhancementFlags, MouseButton, MouseEvent, MouseEventKind, PopKeyboardEnhancementFlags,
    PushKeyboardEnhancementFlags,
};
#[cfg(unix)]
use mio::unix::pipe;
#[cfg(unix)]
use mio::{Events, Interest, Poll, Token};
#[cfg(unix)]
use nix::fcntl::{F_SETFD, F_SETFL, FdFlag, OFlag, fcntl};
#[cfg(unix)]
use nix::sys::signal::{Signal, raise};
use ratatui::{Terminal, backend::CrosstermBackend};
#[cfg(unix)]
use signal_hook::low_level::pipe as signal_pipe;
use std::io;
#[cfg(unix)]
use std::io::Read;
#[cfg(unix)]
use std::os::unix::io::AsRawFd;
use std::{thread, time::Duration};

// The connection module is split into per-page submodules so the monolithic
// `connection.rs` (3.5k lines) is navigable; `handle_terminal_event` /
// `handle_ui_event` (here) dispatch into them, and the daemon-message
// dispatcher is re-exported so callers keep using `connection::*` paths
// unchanged.  The per-page handlers are `pub(super)` — visible to this
// module only — because nothing outside the connection glue calls them.
mod ai_providers;
mod chat;
mod daemon;
mod model_selector;
mod session_manager;

// The terminal-event dispatcher below calls the per-page handlers
// unqualified; bring them into scope without changing the call sites.
use ai_providers::{
    handle_account_wizard_event, handle_ai_providers_event, handle_credential_modal_event,
    handle_polkadot_import_event,
};
use chat::handle_chat_event;
use model_selector::handle_model_selector_event;
use session_manager::handle_session_manager_event;

// Entry points the rest of the crate (lib.rs, app_tests.rs, render_tests.rs)
// imports via `crate::connection::…`; the re-export keeps those paths
// resolving exactly as they did before the split.
pub(crate) use daemon::handle_daemon_message;

// Test-only imports: the in-file `#[cfg(test)] mod tests` unit tests build
// `Event`s and messages by hand and `use super::*` to reach the names below,
// which no non-test code in this module touches after the split (mouse
// construction and `handle_daemon_message` round-trips are exercised only
// there).  Gated so the lib build stays clippy-clean.
#[cfg(test)]
use crate::selection;
#[cfg(test)]
use choreo_proto::DaemonMessage;
#[cfg(test)]
use crossterm::event::KeyEvent;

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

/// High-water mark for the daemon→UI event queue.  The queue is unbounded by
/// design (see the reader-thread closure in `run_app`), so a stalled UI event
/// loop could otherwise accumulate events silently; above this many pending
/// events the reader thread warns once per episode so the backlog stays
/// observable.  Chosen well above the normal steady-state backlog (a handful
/// of events per frame).
const UI_EVENT_QUEUE_HIGH_WATER_MARK: usize = 16_384;

/// Commands sent from the terminal-event thread to the main loop for
/// coordinating terminal state around suspend/resume cycles.
///
/// On Windows the variants are never constructed (there is no job-control
/// suspend and no SIGCONT/SIGTSTP), but the type is still referenced by
/// `run_ui_loop`'s `select!` arm, so the dead-code lint is suppressed there.
#[derive(Debug)]
#[cfg_attr(windows, allow(dead_code))]
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
#[cfg(unix)]
fn signal_to_resume_command(signo: i32) -> Option<ResumeCommand> {
    match Signal::try_from(signo) {
        Ok(Signal::SIGCONT) => Some(ResumeCommand::ReinitTerminal),
        Ok(Signal::SIGTSTP) => Some(ResumeCommand::PrepareForSuspend),
        _ => None,
    }
}

/// Whether a shutdown-notify channel has been disconnected.
///
/// The Windows terminal thread's notify is a sender that is *dropped* (never
/// sent on) to signal shutdown — `try_recv` then reports `Disconnected`. A
/// plain `try_recv().is_ok()` check would never fire, because no message is
/// ever sent; this helper pins the Disconnected-detection contract.
///
/// Only the Windows terminal thread uses it in the lib build (the unit test
/// below exercises it on every platform); `allow(dead_code)` keeps the Unix
/// lib build warning-free.
#[cfg_attr(unix, allow(dead_code))]
fn notify_disconnected(rx: &channel::Receiver<()>) -> bool {
    matches!(
        rx.try_recv(),
        Err(crossbeam::channel::TryRecvError::Disconnected)
    )
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
        let result = run_daemon_connection_with_mode(
            mode,
            |message| {
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

    // Surface why the TUI exited (daemon eviction / graceful shutdown / a
    // dropped connection) once the alternate screen is gone and the message
    // is visible on the restored terminal. A normal user quit (Ctrl+Q)
    // leaves `quit_message` None and prints nothing.
    if let Some(message) = &app.quit_message {
        println!("{message}");
    }

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
                        // ReaderClosed normally carries the reason; this arm
                        // only fires if the connection thread dropped its
                        // sender without one (e.g. a panic mid-read).
                        app.quit_message.get_or_insert_with(|| {
                            "the connection to the daemon was closed".to_string()
                        });
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
                    // A stale selection must not survive a suspend/resume
                    // cycle: the viewport and scroll state are re-established
                    // on resume, so an old rectangle would highlight the
                    // wrong rows until the next mouse event cleared it.
                    if matches!(&cmd, ResumeCommand::PrepareForSuspend) {
                        app.text_selection = None;
                    }
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
                if matches!(&cmd, ResumeCommand::PrepareForSuspend) {
                    app.text_selection = None;
                }
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
#[cfg(unix)]
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

/// Windows twin of [`handle_resume_command`]: no-op.
///
/// Windows has no POSIX job-control signals, so `ResumeCommand` is never
/// produced on Windows (the resume channel never fires); the receiver arm in
/// `run_ui_loop` stays compiled on both platforms for symmetry.
#[cfg(windows)]
fn handle_resume_command(
    _cmd: ResumeCommand,
    _terminal: &mut Terminal<CrosstermBackend<std::io::Stdout>>,
) -> io::Result<bool> {
    // Windows has no job-control suspend; ResumeCommand is never produced.
    Ok(false)
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
    // The account modals (new-account wizard + API-key entry) take the same
    // overlay priority over the AI providers page.  They can only be opened
    // from that page and swallow every key while open, so routing them here
    // (before the page match) keeps the list page unreachable underneath —
    // exactly like the model selector.  The credential modal wins when both
    // are open: the wizard closes before the credential modal auto-opens
    // after account creation.
    if app.ai_providers.credential.is_open() {
        return handle_credential_modal_event(event, app, client_tx);
    }
    if app.ai_providers.wizard.is_open() {
        return handle_account_wizard_event(event, app, client_tx);
    }
    if app.ai_providers.polkadot_import.is_open() {
        return handle_polkadot_import_event(event, app, client_tx);
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
/// and overlay state.  On the Chat page the command input (or the model
/// selector's filter, when it is open) receives the paste; on the AI
/// Providers page the credential modal's key input, or the wizard's
/// provider filter / slug field, receives it.
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
            // The credential modal takes priority (it is also auto-opened
            // right after account creation, with the wizard already closed).
            if app.ai_providers.credential.is_open() {
                tracing::debug!("[choreo-tui] pasting into credential input");
                app.ai_providers.credential.input.insert_str_at_cursor(data);
            } else if app.ai_providers.wizard.is_open() {
                match app.ai_providers.wizard.step {
                    // Step 1: bulk-insert into the provider filter, then
                    // re-clamp the highlight against the narrowed list.
                    AccountWizardStep::Provider => {
                        tracing::debug!("[choreo-tui] pasting into provider filter");
                        app.ai_providers.wizard.filter.insert_str_at_cursor(data);
                        app.ai_providers.wizard.clamp_focus(&app.providers);
                    }
                    // Step 2: bulk-insert into the slug field.
                    AccountWizardStep::Slug => {
                        tracing::debug!("[choreo-tui] pasting into new-account slug field");
                        paste_into_text_state(&mut app.ai_providers.wizard.slug, data);
                    }
                }
            } else if app.ai_providers.polkadot_import.is_open() {
                tracing::debug!("[choreo-tui] pasting into polkadot import field");
                app.ai_providers.polkadot_import.handle_paste(data);
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
            // If the daemon already told us why (ShuttingDown / Evicted),
            // keep that message; a bare EOF means the daemon went away
            // without an advisory (crash, restart, or the socket closed).
            app.quit_message
                .get_or_insert_with(|| "the connection to the daemon was closed".to_string());
            Ok(false)
        }
    }
}

/// How a routed per-session daemon message should be handled by the caller
/// after `route_session_update` has applied the display update.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum SessionUpdateRouting {
    /// The message belongs to the attached session (or a connection-level
    /// `None` reply) — the caller should fall through to the generic dispatch
    /// so the user sees the status/error feedback for their own command.
    FallThrough,
    /// The message belongs to a background session — the per-session display
    /// was already updated, but the caller must not fall through: the generic
    /// dispatch would rewrite the global status/error line the user is
    /// looking at (reflowing the viewed viewport).
    Suppress,
}

/// Route a per-session display update for a daemon-reported session id and
/// report whether the message must also fall through to the generic dispatch.
///
/// Connection-level replies carry `session_id: None` (no origin session —
/// e.g. a "no session attached" failure or a bare `GetReasoningEffort` reply
/// without an attachment), so `resolve_daemon_session` maps them (and every
/// real id) to the session whose display `update` mutates — never a phantom
/// entry.  Background messages (a real session that is not the attached one)
/// still get their display updated, so the per-session state is already
/// correct when the user switches to it, but they must not fall through.
/// Returns [`SessionUpdateRouting::Suppress`] for background noise — the
/// caller should log and return early — or
/// [`SessionUpdateRouting::FallThrough`] for the attached session /
/// connection-level (`None`) replies.
pub(super) fn route_session_update(
    app: &mut App,
    reported: Option<u64>,
    update: impl FnOnce(&mut App, u64),
) -> SessionUpdateRouting {
    if let Some(session_id) = app.resolve_daemon_session(reported) {
        update(app, session_id);
    }
    if app.is_background_session_message(reported) {
        SessionUpdateRouting::Suppress
    } else {
        SessionUpdateRouting::FallThrough
    }
}

/// Shared skeleton for the two full-page list mouse handlers (the AI-providers
/// accounts list and the session-manager list).  Both behave identically at
/// this level and differ only in their list-specific details, which are
/// supplied as closures:
///
/// * `confirmed` — a remove/delete confirmation is armed, so every click (and
///   the wheel) is a no-op.
/// * `select_up` / `select_down` — move the list highlight by one row (the
///   wheel scrolls the highlight, exactly like the picker popups).
/// * `on_click` — a left-click: resolve the drawn row via the list's
///   `*_list_click_index` and, when it lands on a row, apply it as the
///   Enter-equivalent action on that selected row.  Returns `Ok(())` for a
///   click that misses a row.
///
/// Kept out of the per-page modules so the confirm guard, the wheel scroll,
/// and the left-click dispatch are written once instead of twice.
pub(super) fn handle_full_page_list_mouse(
    app: &mut App,
    mouse: &MouseEvent,
    confirmed: bool,
    select_up: impl FnOnce(&mut App),
    select_down: impl FnOnce(&mut App),
    on_click: impl FnOnce(&mut App) -> Result<(), ClientError>,
) -> Result<(), ClientError> {
    if confirmed {
        return Ok(());
    }
    match mouse.kind {
        MouseEventKind::ScrollDown => select_down(app),
        MouseEventKind::ScrollUp => select_up(app),
        MouseEventKind::Down(MouseButton::Left) => on_click(app)?,
        _ => {}
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::Marker;
    use crate::test_util::test_app;
    use choreo_proto::Turn;

    #[cfg(unix)]
    #[test]
    fn sigcont_maps_to_reinit_terminal() {
        assert!(matches!(
            signal_to_resume_command(Signal::SIGCONT as i32),
            Some(ResumeCommand::ReinitTerminal),
        ));
    }

    #[cfg(unix)]
    #[test]
    fn sigtstp_maps_to_prepare_for_suspend() {
        assert!(matches!(
            signal_to_resume_command(Signal::SIGTSTP as i32),
            Some(ResumeCommand::PrepareForSuspend),
        ));
    }

    #[cfg(unix)]
    #[test]
    fn sigwinch_is_not_a_resume_command() {
        // SIGWINCH is registered on the self-pipe purely to wake the terminal
        // thread's mio poll; the actual resize is then reported by crossterm's
        // `event::poll`/`event::read` drain as `Event::Resize`.  Mapping it to a
        // ResumeCommand would be wrong — resizing must never trigger a terminal
        // teardown/reinit.
        assert!(signal_to_resume_command(Signal::SIGWINCH as i32).is_none());
    }

    #[cfg(unix)]
    #[test]
    fn uninteresting_signal_returns_none() {
        assert!(signal_to_resume_command(Signal::SIGINT as i32).is_none());
        assert!(signal_to_resume_command(Signal::SIGTERM as i32).is_none());
    }

    #[cfg(unix)]
    #[test]
    fn invalid_signal_number_returns_none() {
        assert!(signal_to_resume_command(9999).is_none());
    }

    // ── Kitty keyboard protocol ──

    #[test]
    fn notify_disconnected_detects_dropped_sender() {
        // The Windows terminal thread's shutdown notify is a sender that is
        // dropped (never sent on); `try_recv` then reports Disconnected — the
        // `try_recv().is_ok()` check the old code used would never fire
        // because no message is ever sent, hanging the thread (and the join
        // at shutdown) forever. Pin the exact detection contract.
        let (tx, rx) = channel::unbounded::<()>();
        assert!(
            !notify_disconnected(&rx),
            "a live channel must not read as shut down"
        );
        drop(tx);
        assert!(
            notify_disconnected(&rx),
            "a dropped sender must read as shut down"
        );
        assert!(
            notify_disconnected(&rx),
            "detection must stay latched after disconnect"
        );
    }

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
            reasoning_artifact: None,
            reasoning_producer: None,
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
            reasoning_artifact: None,
            reasoning_producer: None,
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

    // ── Mouse text selection (select-to-copy) ───────────────────────────

    /// Drive one mouse event through the full `handle_terminal_event` path
    /// (kitty normalization → page dispatch → Chat page mouse arms).
    fn send_mouse(app: &mut App, kind: MouseEventKind, column: u16, row: u16) {
        let (tx, _rx) = std::sync::mpsc::channel();
        handle_terminal_event(
            Event::Mouse(crossterm::event::MouseEvent {
                kind,
                column,
                row,
                modifiers: KeyModifiers::NONE,
            }),
            app,
            &tx,
        )
        .expect("handle mouse event");
    }

    /// The first viewport row that maps to a *content* line (one whose
    /// rendered text is selectable per the renderer's content ranges — box
    /// chrome rows like separators and padding are excluded).  `None` when
    /// the history has no selectable content.
    fn first_content_row(app: &App) -> Option<u16> {
        let display = app.active_display_ref()?;
        for (turn_idx, _turn_id) in display.visible_turn_ids.iter().enumerate() {
            let Some(cached) = display.render_cache[turn_idx].as_ref() else {
                continue;
            };
            let turn_start = display
                .height_prefix
                .get(turn_idx.wrapping_sub(1))
                .copied()
                .unwrap_or(0);
            for (line_idx, content) in cached.rendered.content_ranges.iter().enumerate() {
                if !content.is_some_and(|(lo, hi)| lo < hi) {
                    continue;
                }
                let row_lo = cached
                    .rendered
                    .visual_offsets
                    .get(line_idx.wrapping_sub(1))
                    .copied()
                    .unwrap_or(0);
                // Reuse the selection module's content→screen mapping rather
                // than re-deriving the bottom-anchored formula by hand (and
                // keep scanning: an off-screen line is not the answer yet).
                if let Some(screen_row) =
                    crate::selection::content_to_screen_row(app, turn_start + row_lo)
                {
                    return Some(screen_row);
                }
            }
        }
        None
    }

    #[test]
    fn mouse_drag_selects_copies_and_reports_status() {
        let mut app = test_app();
        app.history_viewport.width = 80;
        app.history_viewport.height = 20;
        insert_turn(&mut app, 0, "user a", "assistant a");
        insert_turn(&mut app, 1, "user b", "assistant b");
        app.rebuild_height_prefix();

        let start_row = first_content_row(&app).expect("selectable content row");
        send_mouse(
            &mut app,
            MouseEventKind::Down(MouseButton::Left),
            0,
            start_row,
        );
        assert!(
            app.text_selection.is_some(),
            "mouse-down in the history box arms a selection"
        );
        assert!(
            !app.text_selection.unwrap().active,
            "armed but not active before any drag"
        );

        send_mouse(
            &mut app,
            MouseEventKind::Drag(MouseButton::Left),
            5,
            start_row + 1,
        );
        assert!(
            app.text_selection.unwrap().active,
            "drag activates the selection"
        );

        send_mouse(
            &mut app,
            MouseEventKind::Up(MouseButton::Left),
            5,
            start_row + 1,
        );
        assert!(
            app.text_selection.is_none(),
            "selection cleared after release"
        );
        let status = app.status.as_deref().expect("copy sets a status message");
        assert_eq!(status, "Selection copied to clipboard.");
    }

    #[test]
    fn mouse_click_without_drag_does_not_copy() {
        let mut app = test_app();
        app.history_viewport.width = 80;
        app.history_viewport.height = 20;
        insert_turn(&mut app, 0, "user a", "assistant a");
        app.rebuild_height_prefix();

        let start_row = first_content_row(&app).expect("selectable content row");
        send_mouse(
            &mut app,
            MouseEventKind::Down(MouseButton::Left),
            3,
            start_row,
        );
        send_mouse(
            &mut app,
            MouseEventKind::Up(MouseButton::Left),
            3,
            start_row,
        );
        assert!(
            app.text_selection.is_none(),
            "a plain click must not leave a selection armed"
        );
        assert!(
            app.status.is_none(),
            "a plain click must not trigger a copy status"
        );
    }

    #[test]
    fn mouse_scroll_during_selection_keeps_selection_and_scrolls() {
        let mut app = test_app();
        app.history_viewport.width = 80;
        app.history_viewport.height = 10;
        for i in 0..20 {
            insert_turn(&mut app, i, "user text", "assistant response");
        }
        app.rebuild_height_prefix();
        assert!(
            app.scrollbar_visible(),
            "history must overflow the viewport"
        );

        let start_row = first_content_row(&app).expect("selectable content row");
        send_mouse(
            &mut app,
            MouseEventKind::Down(MouseButton::Left),
            0,
            start_row,
        );
        send_mouse(
            &mut app,
            MouseEventKind::Drag(MouseButton::Left),
            3,
            start_row + 1,
        );
        // A scroll wheel mid-gesture keeps the selection (the anchor stays
        // pinned to the text while the head tracks the cursor) AND the wheel
        // input lands immediately: the scroll is applied synchronously, not
        // swallowed by the gesture.
        let scroll_before = app.effective_scroll();
        send_mouse(&mut app, MouseEventKind::ScrollUp, 0, start_row);
        assert!(
            app.text_selection.is_some_and(|s| s.active),
            "scrolling must keep the active selection"
        );
        assert!(
            app.effective_scroll() > scroll_before,
            "the wheel scroll must land during the gesture"
        );
    }

    #[test]
    fn mouse_scroll_during_selection_tracks_cursor_and_keeps_anchor() {
        // The anchor stays pinned to the text it was placed on (content
        // coordinates) while the live drag head re-resolves to the content
        // now under the cursor — so scrolling mid-gesture updates the
        // selection immediately, without waiting for the next drag event.
        let mut app = test_app();
        app.history_viewport.width = 80;
        app.history_viewport.height = 10;
        for i in 0..20 {
            insert_turn(&mut app, i, "user text", "assistant response");
        }
        app.rebuild_height_prefix();

        let start_row = first_content_row(&app).expect("selectable content row");
        send_mouse(
            &mut app,
            MouseEventKind::Down(MouseButton::Left),
            3,
            start_row,
        );
        send_mouse(
            &mut app,
            MouseEventKind::Drag(MouseButton::Left),
            80,
            start_row + 5,
        );
        let ((anchor0, _), (head0, _)) =
            selection::selection_range(&app).expect("active selection");

        // Scroll up mid-gesture (the wheel event is reported at the cursor
        // position): older content moves under the cursor, so the head moves
        // to an earlier content line while the anchor stays put.
        send_mouse(&mut app, MouseEventKind::ScrollUp, 80, start_row + 5);
        let ((anchor1, _), (head1, _)) =
            selection::selection_range(&app).expect("active selection");
        assert_eq!(anchor0, anchor1, "the anchor stays pinned to its text");
        assert!(
            head1 < head0,
            "the head tracks the content under the cursor"
        );
        assert!(
            app.text_selection.is_some_and(|s| s.active),
            "scrolling must keep the active selection"
        );
    }

    #[test]
    fn mouse_down_in_scrollbar_column_does_not_arm_selection() {
        let mut app = test_app();
        app.history_viewport.width = 80;
        app.history_viewport.height = 20;
        for i in 0..20 {
            insert_turn(&mut app, i, "user text", "assistant response");
        }
        app.rebuild_height_prefix();
        assert!(
            app.scrollbar_visible(),
            "content must overflow the viewport"
        );

        // Click in the scrollbar column (viewport width) — that is a
        // scrollbar interaction, never a text selection.
        let vp_width = app.history_viewport.width;
        send_mouse(
            &mut app,
            MouseEventKind::Down(MouseButton::Left),
            vp_width,
            0,
        );
        assert!(
            app.text_selection.is_none(),
            "scrollbar-column clicks must not arm a text selection"
        );
    }

    // ── Connection-level termination ────────────────────────────────────

    #[test]
    fn reader_closed_quits_with_message() {
        let mut app = test_app();
        let (tx, _rx) = std::sync::mpsc::channel();

        assert!(
            !handle_ui_event(UiEvent::ReaderClosed, &mut app, &tx).expect("handle ReaderClosed"),
            "ReaderClosed is a control-flow event, not a re-render"
        );
        assert!(app.should_quit);
        assert_eq!(
            app.quit_message.as_deref(),
            Some("the connection to the daemon was closed"),
            "a bare EOF must report the dropped connection"
        );
    }

    #[test]
    fn reader_closed_keeps_existing_quit_message() {
        // The daemon flushes `ShuttingDown` before closing the socket, so the
        // TUI normally learns the reason from the message and only then sees
        // the EOF. ReaderClosed must not overwrite that reason with the
        // generic "connection closed" text.
        let mut app = test_app();
        let (tx, _rx) = std::sync::mpsc::channel();
        handle_daemon_message(DaemonMessage::ShuttingDown, &mut app, &tx)
            .expect("handle ShuttingDown");

        handle_ui_event(UiEvent::ReaderClosed, &mut app, &tx).expect("handle ReaderClosed");

        assert_eq!(
            app.quit_message.as_deref(),
            Some("the server is shutting down"),
            "the specific reason must survive the trailing EOF"
        );
    }
}
