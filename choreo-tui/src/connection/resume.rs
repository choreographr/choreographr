//! POSIX suspend/resume coordination for the TUI.
//!
//! Owns the `ResumeCommand` control message the terminal-event thread sends the
//! main loop when it catches SIGCONT / SIGTSTP, the signal self-pipes and their
//! signal→command mapping, the shutdown-notify disconnect probe, and the
//! terminal re-init/teardown that a suspend/resume cycle requires. The UI loop
//! consumes these commands from its resume channel.

#[cfg(unix)]
use super::terminal_event::KITTY_KEYBOARD_FLAGS;
use crate::backend::TuiBackend;
use crate::state::App;
#[cfg(unix)]
use crate::terminal::{pointer, progress, title};
use crossbeam_channel as channel;
#[cfg(unix)]
use crossterm::event::{
    DisableBracketedPaste, EnableBracketedPaste, PopKeyboardEnhancementFlags,
    PushKeyboardEnhancementFlags,
};
#[cfg(unix)]
use mio::unix::SourceFd;
#[cfg(unix)]
use mio::{Interest, Poll, Token};
#[cfg(unix)]
use nix::fcntl::{FcntlArg, FdFlag, OFlag, fcntl};
#[cfg(unix)]
use nix::sys::signal::{Signal, raise};
use ratatui::Terminal;
#[cfg(unix)]
use signal_hook::low_level::pipe as signal_pipe;
#[cfg(unix)]
use std::fs::File;
use std::io;
#[cfg(unix)]
use std::io::Read;
#[cfg(unix)]
use std::os::fd::AsRawFd;

/// Commands sent from the terminal-event thread to the main loop for
/// coordinating terminal state around suspend/resume cycles.
///
/// On Windows the variants are never constructed (there is no job-control
/// suspend and no SIGCONT/SIGTSTP), but the type is still referenced by
/// `run_ui_loop`'s `select!` arm, so the dead-code lint is suppressed there.
/// A `Copy` enum keeps `handle_resume_command(cmd, …)` call sites
/// pass-by-value without triggering `needless_pass_by_value`.
#[derive(Debug, Clone, Copy)]
#[cfg_attr(windows, expect(dead_code))]
pub(super) enum ResumeCommand {
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
pub(super) fn signal_to_resume_command(signo: i32) -> Option<ResumeCommand> {
    match Signal::try_from(signo) {
        Ok(Signal::SIGCONT) => Some(ResumeCommand::ReinitTerminal),
        Ok(Signal::SIGTSTP) => Some(ResumeCommand::PrepareForSuspend),
        _ => None,
    }
}

/// First `mio` token owned by a signal self-pipe; each caught signal takes the
/// next one, in [`Signals::register`]'s order.
pub(super) const SIGNAL_TOKEN_BASE: usize = 2;

/// The signals the TUI catches, in `mio`-token order.  SIGWINCH maps to no
/// `ResumeCommand`: catching it only wakes the poll (see [`Signals::on_token`]).
#[cfg(unix)]
const HANDLED_SIGNALS: [Signal; 3] = [Signal::SIGCONT, Signal::SIGWINCH, Signal::SIGTSTP];

/// What one drain found in a signal pipe.
#[cfg(unix)]
#[derive(Debug, PartialEq, Eq)]
enum Wake {
    /// At least one delivery was waiting and has been consumed.  Repeated
    /// deliveries coalesce into this one wake, and the action they share is
    /// taken once — the signals are idempotent state transitions, not events to
    /// be counted.
    Signalled,
    /// Nothing was left to read: the wake had already been drained.
    Empty,
    /// The write end is gone, so this pipe can never deliver again.
    Closed,
}

/// One self-pipe per caught signal.
///
/// A signal handler may only write to a pipe, and `signal_hook`'s pipe handler
/// writes a single *unidentified* byte per delivery — it cannot encode which
/// signal fired without hand-rolling an async-signal-safe handler.  Suspend and
/// resume are opposite actions, so the identity is the whole message:
/// registering each signal on its own pipe makes the pipe the name of the
/// signal, and the read end's `mio` token then attributes a wake without
/// decoding anything.
#[cfg(unix)]
struct SignalPipe {
    /// The signal whose handler writes into this pipe.
    signal: Signal,
    /// The read end: non-blocking, close-on-exec, drained to empty on a wake.
    read: File,
}

#[cfg(unix)]
impl SignalPipe {
    /// Create the pipe and arm `signal`'s handler to write into it.
    fn register(signal: Signal) -> io::Result<Self> {
        let (read, write) = nix::unistd::pipe()?;
        // Both ends are close-on-exec so nothing this process spawns inherits
        // them; the read end is non-blocking so draining it can never stall the
        // terminal thread.
        fcntl(&read, FcntlArg::F_SETFD(FdFlag::FD_CLOEXEC))?;
        fcntl(&read, FcntlArg::F_SETFL(OFlag::O_NONBLOCK))?;
        fcntl(&write, FcntlArg::F_SETFD(FdFlag::FD_CLOEXEC))?;
        // The handler takes ownership of the write end: it must stay open for
        // the life of the process, and an async-signal-safe handler could never
        // close it.
        signal_pipe::register(signal as i32, write)?;
        Ok(Self {
            signal,
            read: read.into(),
        })
    }

    /// Consume everything this pipe accumulated and report what it held.
    ///
    /// The read never blocks (the fd is non-blocking) and never waits for a
    /// delivery, so a wake that `mio` reported but another reader already
    /// drained simply reports [`Wake::Empty`].
    fn drain(&mut self) -> io::Result<Wake> {
        let mut drained = false;
        let mut buf = [0u8; 64];
        loop {
            match self.read.read(&mut buf) {
                Ok(0) => return Ok(Wake::Closed),
                Ok(_) => drained = true,
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    return Ok(if drained {
                        Wake::Signalled
                    } else {
                        Wake::Empty
                    });
                }
                // EINTR: a signal interrupted the read, so try again.
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                Err(error) => return Err(error),
            }
        }
    }
}

/// Every caught signal's self-pipe, parked in the terminal thread's `mio` poll.
///
/// This is the input side of suspend/resume: the handlers installed here write a
/// byte, the terminal thread's poll wakes on it, and [`Signals::on_token`] turns
/// it back into the `ResumeCommand` the UI loop acts on.
#[cfg(unix)]
pub(super) struct Signals {
    /// One pipe per signal, indexed from [`SIGNAL_TOKEN_BASE`] by token.
    pipes: Vec<SignalPipe>,
}

#[cfg(unix)]
impl Signals {
    /// Install the handlers and create their pipes.
    pub(super) fn register() -> io::Result<Self> {
        let pipes = HANDLED_SIGNALS
            .into_iter()
            .map(SignalPipe::register)
            .collect::<io::Result<Vec<_>>>()?;
        Ok(Self { pipes })
    }

    /// Park every read end in `poll`, each under its own token.
    pub(super) fn register_with(&self, poll: &mut Poll) -> io::Result<()> {
        for (index, pipe) in self.pipes.iter().enumerate() {
            let fd = pipe.read.as_raw_fd();
            poll.registry().register(
                &mut SourceFd(&fd),
                Token(SIGNAL_TOKEN_BASE + index),
                Interest::READABLE,
            )?;
        }
        Ok(())
    }

    /// Take one wake for `token`, which the caller has already established is a
    /// signal pipe's (anything from [`SIGNAL_TOKEN_BASE`] up); an index past the
    /// registered signals is ignored.
    ///
    /// A wake that maps to a command sends it on `resume_tx` (the UI loop's
    /// channel); SIGWINCH maps to nothing and only needs its pipe emptied, which
    /// is what lets the terminal thread then run crossterm's own drain — that is
    /// when crossterm turns its internal SIGWINCH notification into the
    /// `Event::Resize` that reflows the UI.
    pub(super) fn on_token(
        &mut self,
        token: Token,
        poll: &mut Poll,
        resume_tx: &channel::Sender<ResumeCommand>,
    ) {
        let Some(pipe) = token
            .0
            .checked_sub(SIGNAL_TOKEN_BASE)
            .and_then(|index| self.pipes.get_mut(index))
        else {
            return;
        };
        match pipe.drain() {
            Ok(Wake::Signalled) => {
                if let Some(command) = signal_to_resume_command(pipe.signal as i32) {
                    let _ = resume_tx.send(command);
                }
            }
            // Already emptied (a wake raced another drain): nothing to act on.
            Ok(Wake::Empty) => {}
            Ok(Wake::Closed) => {
                // Unreachable while the handler registration lives, but a
                // readable-at-EOF fd would make `poll` return instantly forever,
                // so drop the source instead of trusting that it cannot happen.
                tracing::warn!(
                    signal = ?pipe.signal,
                    "[choreo-tui] signal pipe closed; no longer watching it"
                );
                let fd = pipe.read.as_raw_fd();
                let _ = poll.registry().deregister(&mut SourceFd(&fd));
            }
            Err(error) => {
                tracing::warn!(
                    signal = ?pipe.signal,
                    %error,
                    "[choreo-tui] signal pipe read error"
                );
            }
        }
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
// Unix lib build: unused (dead). Unix test build: used by the unit test
// below. So `dead_code` fires in one target but not the other and cannot be
// an `expect`; kept as an `allow` with its `allow_attributes` exemption.
#[allow(clippy::allow_attributes)]
#[cfg_attr(unix, allow(dead_code))]
pub(super) fn notify_disconnected(rx: &channel::Receiver<()>) -> bool {
    matches!(
        rx.try_recv(),
        Err(crossbeam_channel::TryRecvError::Disconnected)
    )
}

/// React to a suspend/resume signal from the terminal-event thread.
///
/// Returns `true` when re-rendering is necessary (`ReinitTerminal`),
/// or `false` when the terminal was only torn down (`PrepareForSuspend`).
///
/// `app` carries the terminal-native records that must be cleared on suspend
/// and re-published on resume (the OSC 7501 program-status records and the
/// OSC 2 window title), alongside the OSC 9;4 progress bar handled here.  The
/// OSC 22 pointer shape is cleared on suspend (and its cached value forgotten)
/// but deliberately *not* re-published on resume: the next mouse motion
/// re-emits it, so no stale hand is restored.
#[cfg(unix)]
pub(super) fn handle_resume_command(
    cmd: ResumeCommand,
    terminal: &mut Terminal<TuiBackend>,
    app: &mut App,
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
            // Force a full repaint of the screen we just re-entered.  This is
            // `resize` on the current area, NOT `Terminal::clear`: `clear`
            // snapshots the cursor position to restore it afterwards, and reading
            // the cursor means writing a DSR query (`CSI 6 n`) and waiting on
            // stdin for the reply — which both races our own terminal-event
            // reader for those bytes and fails the whole resume on a terminal that
            // does not answer in time.  `resize` clears the region and resets the
            // back buffer without touching stdin (our viewport is fullscreen, so
            // it takes no cursor-reading path).
            terminal.resize(terminal.size()?.into())?;
            // The suspend cleared the program-status records and the window
            // title; re-apply the title now and flag the status records for
            // the UI loop to re-publish (it owns the diff against `term_status`).
            // The OSC 9;4 progress bar is re-emitted by the existing
            // progress path once a status/turn event supplies fresh data.
            let resumed_title = app.window_title();
            title::set(&resumed_title);
            app.term_title = Some(resumed_title);
            app.term_status_dirty = true;
            Ok(true)
        }
        ResumeCommand::PrepareForSuspend => {
            tracing::info!("[choreo-tui] restoring terminal for suspend");
            // Clear the terminal-native records BEFORE the process stops so
            // the shell the user lands in does not show a stale progress bar,
            // program-status record, or window title left by the TUI.
            progress::update(None, None);
            app.term_status.clear_all();
            title::clear();
            app.term_title = None;
            // Hand the pointer back to the terminal as well, and forget both
            // the hover position and the emitted shape: after the resume the
            // ordinary change detection re-emits only once the mouse actually
            // moves, so no stale hand is restored over the shell's content.
            pointer::set(pointer::PointerShape::Default);
            app.forget_pointer();
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
pub(super) fn handle_resume_command(
    _cmd: ResumeCommand,
    _terminal: &mut Terminal<TuiBackend>,
    _app: &mut App,
) -> io::Result<bool> {
    // Windows has no job-control suspend; ResumeCommand is never produced.
    Ok(false)
}
