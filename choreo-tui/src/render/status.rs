//! The session-status vocabulary: the label and colour for a daemon-reported
//! [`SessionStatus`], and the persistent keystore-lock marker that leads the
//! status bar while the daemon's credential store is locked.
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
