//! `App`-level tests, one child module per subject.
//!
//! `text_layout` covers history-text measurement and markdown rendering;
//! `terminal_events` covers the basic key-to-input path and prompt submission;
//! `prompt_gating` pins the rule that a prompt is only accepted while the
//! session is idle; `drafts` covers the per-session unsent-prompt hand-off plus
//! the commands dispatched from a session's command line (`/refresh-models`,
//! `/mcp`, catalog and credential handling); `palette` covers the inline
//! command palette and unified command dispatch.

mod drafts;
mod palette;
mod prompt_gating;
mod terminal_events;
mod text_layout;

use crate::connection::handle_terminal_event;
use crate::state::*;
use choreo_proto::ClientMessage;
use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};

/// Drive a bare Enter keypress through the full terminal-event pipeline.
///
/// Shared by `prompt_gating` (does an Enter reach the daemon?) and `palette`
/// (`/quit` and the other command lines run off the same Enter path), so both
/// assert against one definition of "submit".
pub(super) fn press_enter(app: &mut App, tx: &crossbeam_channel::Sender<ClientMessage>) {
    handle_terminal_event(
        Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
        app,
        tx,
    )
    .expect("handle enter");
}
