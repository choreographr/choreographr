use crate::state::App;
use choreo_client_core::ClientError;
use crossterm::event::{KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};

// The connection module is split into per-page submodules so the monolithic
// `connection.rs` is navigable; the entry points (`run_app`,
// `handle_terminal_event`, `handle_ui_event`) dispatch into them via the
// siblings below. Alongside the per-page handlers the split also carves out
// four support siblings: `resume` (suspend/resume signal coordination plus
// the shutdown-notify probe), `terminal_event` (crossterm-event normalisation
// and routing — kitty flags, paste, fullscreen, `UiEvent`), `setup_flow` (the
// I/O driver for the staged account/model/reasoning choice, whose decision
// core lives in `state/setup_flow.rs`), and `ui_loop` (`run_app` and the
// `select!` event loop). The re-exports below keep the existing
// `connection::*` paths resolving, and the per-page handlers are
// `pub(super)` — visible to this module only — because nothing outside the
// connection glue calls them.
mod ai_providers;
mod chat;
mod command;
mod daemon;
mod model_selector;
mod resume;
mod session_manager;
mod setup_flow;
mod terminal_event;
mod ui_loop;

// `run_app` is the crate's own entry point (`lib.rs` calls it); keep the path
// `crate::connection::run_app` resolving.
pub(crate) use ui_loop::run_app;

// `handle_daemon_message` and `handle_terminal_event` are referenced from
// outside the module only by the crate suite (`crate::tests` — its `app`,
// `daemon`, `session`, `input`, `render`, `ai_providers` and
// `model_selector` subjects), so the re-exports are gated to the test build —
// the lib build reaches these through the sibling modules directly, and an
// ungated re-export would be flagged unused there.
#[cfg(test)]
pub(crate) use daemon::handle_daemon_message;
#[cfg(test)]
pub(crate) use terminal_event::handle_terminal_event;

// Test-only imports: the `tests` child module (declared below, body in
// `connection/tests.rs`) builds `Event`s and messages by hand and `use
// super::*` to reach the names below, which no non-test code in this module
// touches (mouse construction, the fullscreen/pointer dispatch, and the
// daemon-message round-trips are exercised only there). Gated so the lib build
// stays clippy-clean.
//
// The signal→resume-command mapping and the kitty-keyboard normalisation live
// beside the tests that exercise them (`resume` and `terminal_event`), so this
// module imports neither.
#[cfg(test)]
use crate::selection;
#[cfg(test)]
use choreo_proto::{DaemonMessage, DaemonMessageType};
#[cfg(test)]
use terminal_event::{dispatch_terminal_event, handle_ui_event};

/// Whether `key` is a `Ctrl`/`Alt` *modifier chord*.
///
/// The full-page list handlers (Session Manager list + detail, AI-provider
/// accounts) match on `KeyCode` alone, so without this guard a modifier
/// combination would fire the bare-letter action of the same key — e.g.
/// `Ctrl+A` would archive a session and `Ctrl+N` / `Alt+A` would open a wizard.
/// Chat-page dispatch needs no such guard: it resolves modifiers through the
/// logical keymap (`crate::state::binding_for`), so only the exact `Alt+` chord
/// matches a command.
///
/// Shared here so the three call sites cannot drift apart.
pub(super) fn is_modifier_chord(key: &KeyEvent) -> bool {
    key.modifiers
        .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
}

/// Route a per-session display update for a daemon-reported session id and
/// report whether the message should also fall through to the generic
/// dispatch.
///
/// The display update lands on the session the message names
/// ([`App::resolve_daemon_session`]): a real id updates that session's display,
/// and a connection-level reply (`None`, no origin — e.g. a bare
/// `GetReasoningEffort` reply without an attachment, or a "no session
/// attached" failure) resolves to the attached session, never a phantom entry.
/// Background sessions (a real id that is not the attached one) still get
/// their display updated — so the per-session state is correct when the user
/// switches to it — but must not rewrite the global status/error line.
///
/// The fall-through decision is now structural rather than a UI-state guess: a
/// **targeted reply** (`reply_id: Some`, the answer to this client's own
/// request) always falls through so the user sees feedback for the command they
/// issued, while a **broadcast** (`reply_id: None`) from a background session is
/// suppressed. Returns `true` to fall through, `false` to suppress (the caller
/// logs and returns early).
pub(super) fn route_session_update(
    app: &mut App,
    reported: Option<u64>,
    reply_id: Option<u64>,
    update: impl FnOnce(&mut App, u64),
) -> bool {
    if let Some(session_id) = app.resolve_daemon_session(reported) {
        update(app, session_id);
    }
    !(reply_id.is_none() && app.is_background_session_message(reported))
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
// session_manager.rs also calls this with `&MouseEvent`; the signature is
// shared, so the lint is silenced at function level (param attributes are not
// honoured for this lint).
#[expect(clippy::trivially_copy_pass_by_ref)]
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

// The suite lives in its own file (the precedent is `render/tests.rs` and
// `selection/tests.rs`): the module keeps the three shared helpers below plus
// this declaration, so the file body stays navigable.
#[cfg(test)]
mod tests;
