//! POSIX suspend/resume coordination for the TUI.
//!
//! Owns the `ResumeCommand` control message the terminal-event thread sends the
//! main loop when it catches SIGCONT / SIGTSTP, the signal→command mapping, the
//! shutdown-notify disconnect probe, and the terminal re-init/teardown that a
//! suspend/resume cycle requires. The UI loop consumes these commands from its
//! resume channel.

#[cfg(unix)]
use super::terminal_event::KITTY_KEYBOARD_FLAGS;
use crate::backend::TuiBackend;
use crate::state::App;
#[cfg(unix)]
use crate::terminal::{progress, title};
use crossbeam_channel as channel;
#[cfg(unix)]
use crossterm::event::{
    DisableBracketedPaste, EnableBracketedPaste, PopKeyboardEnhancementFlags,
    PushKeyboardEnhancementFlags,
};
#[cfg(unix)]
use nix::sys::signal::{Signal, raise};
use ratatui::Terminal;
use std::io;

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
/// OSC 2 window title), alongside the OSC 9;4 progress bar handled here.
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
            terminal.clear()?;
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
