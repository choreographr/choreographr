//! The session-status vocabulary: the label and colour for a daemon-reported
//! [`SessionStatus`], the label and colour for a client-side [`SessionNote`]
//! (see [`session_note_display`]), and the persistent keystore-lock marker that
//! leads the status bar while the daemon's credential store is locked.
//!
//! The two vocabularies are separate on purpose.  A `SessionStatus` describes
//! what the daemon is doing with the session; a `SessionNote` describes what
//! this client knows about its own view of the session (no transcript yet, or
//! the attach failed).  The status bar's slot prefers the note and falls back
//! to the daemon's status, so the two never render side by side.
//!
//! A tool name inside a status is MCP-supplied arbitrary text, so it is
//! normalized here rather than at each surface that draws a status: the status
//! bar and the session table both read this one function, and both get text
//! that measures exactly the columns it draws.
//!
//! The lock marker is driven straight from the latched `keystore_locked` flag
//! rather than from the transient status line, so a keystroke's status clear
//! cannot erase it: it stays on screen until the daemon reports the keystore
//! unlocked.

use choreo_proto::SessionStatus;
use ratatui::style::Color;

use super::display_text;
use crate::state::SessionNote;

/// The persistent status-bar banner shown while the daemon's credential
/// keystore is locked (see `App::keystore_locked`). Small and non-intrusive:
/// a lock marker that leads the status bar until the daemon reports unlocked
/// (or bound-and-unlocked). A fresh daemon starts unbound — it reports
/// `KeystoreUnbound` and the client auto-binds it (the marker clears on the
/// `Bound` reply); a bound-but-locked daemon is unlocked with `/unlock` or
/// `/unlock <base64 unlock-key>` (the fuller guidance also appears in the
/// startup status and the submit-time prompt rejection).
pub(crate) const KEYS_LOCKED_MARKER: &str = " 🔒 keystore locked";

pub(crate) fn status_display(status: &SessionStatus) -> (String, Color) {
    match status {
        SessionStatus::Sleeping => ("sleeping".into(), Color::DarkGray),
        SessionStatus::Inactive => ("idle".into(), Color::Green),
        SessionStatus::Sending => ("sending".into(), Color::Blue),
        SessionStatus::Waiting => ("waiting".into(), Color::LightBlue),
        SessionStatus::Reasoning => ("reasoning".into(), Color::Yellow),
        SessionStatus::Responding => ("responding".into(), Color::LightGreen),
        // A tool name is MCP-supplied (an arbitrary server string): normalize it
        // here, where every session-status surface reads it.
        SessionStatus::ToolCall(name) => {
            (format!("tool call: {}", display_text(name)), Color::Cyan)
        }
        SessionStatus::Retrying {
            attempt,
            max_attempts,
            delay_ms,
        } => (
            format!(
                "retrying ({attempt}/{max_attempts}, {})",
                humfmt::duration(std::time::Duration::from_millis(*delay_ms)),
            ),
            Color::Magenta,
        ),
        _ => ("unknown".into(), Color::White),
    }
}

pub(crate) fn format_status(status: &SessionStatus) -> String {
    status_display(status).0
}

/// The label and colour for a client-side [`SessionNote`] — the state the
/// daemon's status vocabulary cannot express, shown in the status bar's status
/// slot in place of (in preference to) the daemon's own status.
///
/// `loading` is blue, the same colour as the daemon's `sending` status: the
/// client is waiting on the daemon, not failing.  `error` is red, matching the
/// error line above the input box.  Neither is dim: a note that the transcript
/// is unavailable must read as state, not as an aside.
///
/// Kept a pure function of the note (no `App`) so the mapping is testable on
/// its own and stays part of this module's status vocabulary.  The label is
/// borrowed from a fixed pair of literals, so the caller builds a span from it
/// without allocating.
pub(crate) fn session_note_display(note: SessionNote) -> (&'static str, Color) {
    match note {
        SessionNote::Loading => ("loading", Color::Blue),
        SessionNote::Error => ("error", Color::Red),
    }
}
