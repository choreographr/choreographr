//! Terminal-event normalisation and dispatch for the UI.
//!
//! Owns the crossterm-event entry point the UI loop feeds: the kitty
//! keyboard-protocol handling (the enhancement flags requested at startup and
//! the SHIFT normalisation that restores legacy-equivalent events), the
//! bracketed-paste routing into whichever input buffer is active, the
//! fullscreen-overlay guard, and the daemon→UI `UiEvent` handler. The per-page
//! key/mouse handlers live in the sibling page modules this dispatches to; the
//! UI loop itself lives in the sibling `ui_loop` module.

use crate::state::{AccountWizardStep, App, Page, UiEvent};
use choreo_client_core::ClientError;
use choreo_proto::ClientMessage;
use crossterm::event::{Event, KeyCode, KeyEventKind, KeyModifiers, KeyboardEnhancementFlags};

// The terminal-event dispatcher below calls the per-page handlers
// unqualified; bring them into scope from their sibling modules so the call
// sites read the same as they did when the dispatcher shared their module.
use super::ai_providers::{
    handle_account_wizard_event, handle_ai_providers_event, handle_credential_modal_event,
    handle_polkadot_import_event,
};
use super::chat::handle_chat_event;
use super::daemon::handle_daemon_message;
use super::model_selector::handle_model_selector_event;
use super::session_manager::handle_session_manager_event;

/// Keyboard enhancements requested from the terminal via the kitty keyboard
/// protocol (`CSI > flags u`), pushed at startup and re-pushed after resume.
///
/// `DISAMBIGUATE_ESCAPE_CODES` makes `Ctrl+letter` arrive as an unambiguous
/// CSI-u sequence (e.g. `CSI 109;5 u` for Ctrl+M) instead of the legacy
/// control byte (0x0D — identical to Enter), while plain text keys stay as
/// legacy UTF-8 bytes.
///
/// `REPORT_ALL_KEYS_AS_ESCAPE_CODES` is deliberately **not** requested.  With
/// it enabled, kitty-protocol terminals report *every* key as a CSI-u event,
/// and text produced by an input method (IME) — e.g. Vietnamese composed by
/// `OpenKey` — arrives as a pure "text event" with key number 0
/// (`CSI 0;;<codepoints>u`, the third field carrying the composed text).
/// crossterm 0.29 has no `KeyEvent` text field and silently drops that third
/// field, turning the event into `KeyCode::Char('\0')`; the composed text is
/// lost and the chat input receives a NUL.  Keeping text keys in legacy
/// encoding means IME-composed text arrives as plain UTF-8 bytes, which
/// crossterm parses into the correct `Char` events.
///
/// Trade-off: with `DISAMBIGUATE_ESCAPE_CODES` alone, the *plain* Enter/Tab/
/// Backspace keys stay in their legacy encodings (per the protocol), so a
/// `Ctrl+letter` editing chord stays distinct from its bare key while those
/// keys remain shell-friendly.  Key combinations with no legacy byte encoding
/// — e.g. Shift+Enter — are still reported as CSI-u (`CSI 13;2 u`), so modifier
/// variants like Shift+Enter (newline) remain distinguishable.
///
/// Terminals that do not implement the kitty protocol simply ignore the push
/// and keep legacy encodings (there `Ctrl+letter` arrives as its control byte,
/// which crossterm maps back to `Char(letter)+CONTROL` — so the readline
/// editing chords still work, and `Ctrl+M`/`Ctrl+I`/`Ctrl+H` arrive as Enter/
/// Tab/Backspace).  The push is what keeps Shift+Enter distinct from Enter; the
/// app's command shortcuts are all `Alt+` chords and need no rebinding, so
/// nothing here is consulted for key dispatch any more.
pub(super) const KITTY_KEYBOARD_FLAGS: KeyboardEnhancementFlags =
    KeyboardEnhancementFlags::from_bits_retain(
        KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES.bits(),
    );

/// High-water mark for the daemon→UI event queue.  The queue is unbounded by
/// design (see the reader-thread closure in `run_app`), so a stalled UI event
/// loop could otherwise accumulate events silently; above this many pending
/// events the reader thread warns once per episode so the backlog stays
/// observable.  Chosen well above the normal steady-state backlog (a handful
/// of events per frame).
pub(super) const UI_EVENT_QUEUE_HIGH_WATER_MARK: usize = 16_384;

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
pub(super) fn normalize_kitty_shift(event: Event) -> Event {
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
pub(super) fn shift_char(c: char) -> char {
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
    client_tx: &crossbeam_channel::Sender<ClientMessage>,
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
        handle_paste_event(data, app);
        return Ok(());
    }

    // Terminal-resize events are handled irrespective of page or fullscreen
    // state so the viewport is refreshed on the next frame.
    if let Event::Resize(cols, rows) = &event {
        tracing::trace!("[choreo-tui] terminal resize: {cols}x{rows}");
        app.mark_terminal_resized();
    }

    // Global Alt+Q quits from any page before page-specific dispatch and
    // fullscreen overlay so the user can always quit.  It is deliberately NOT
    // routed through the Chat-only shortcut table: quitting must work from the
    // session manager, the accounts page, and every open modal too.
    if let Event::Key(key) = &event
        && key.kind == KeyEventKind::Press
        && key.code == KeyCode::Char('q')
        && key.modifiers.contains(KeyModifiers::ALT)
    {
        tracing::info!("Alt+Q requested quit");
        app.should_quit = true;
        return Ok(());
    }
    // Fullscreen image overlay takes priority over page content.
    if app.fullscreen_image_target.is_some() {
        handle_fullscreen_event(&event, app, client_tx);
        return Ok(());
    }
    // The model selector overlay (Chat page) also takes priority over page
    // content, mirroring the fullscreen guard above.  Alt+Q is handled
    // before this point so the user can always quit while it is open.
    if app.model_selector.is_open() {
        return handle_model_selector_event(&event, app, client_tx);
    }
    // The account modals (new-account wizard + API-key entry) take the same
    // overlay priority over the AI providers page.  They can only be opened
    // from that page and swallow every key while open, so routing them here
    // (before the page match) keeps the list page unreachable underneath —
    // exactly like the model selector.  The credential modal wins when both
    // are open: the wizard closes before the credential modal auto-opens
    // after account creation.
    if app.ai_providers.credential.is_open() {
        handle_credential_modal_event(&event, app, client_tx);
        return Ok(());
    }
    if app.ai_providers.wizard.is_open() {
        return handle_account_wizard_event(&event, app, client_tx);
    }
    if app.ai_providers.polkadot_import.is_open() {
        handle_polkadot_import_event(&event, app, client_tx);
        return Ok(());
    }
    match app.page {
        Page::SessionManager => handle_session_manager_event(&event, app, client_tx),
        Page::AIProviders => handle_ai_providers_event(&event, app, client_tx),
        Page::Chat => handle_chat_event(&event, app, client_tx),
    }
}

/// Insert pasted text into whichever input buffer is currently active.
///
/// Routes the paste to the appropriate field based on the current page
/// and overlay state.  On the Chat page the command input (or the model
/// selector's filter, when it is open) receives the paste; on the AI
/// Providers page the credential modal's key input, or the wizard's
/// provider filter / slug field, receives it.
fn handle_paste_event(data: &str, app: &mut App) {
    match app.page {
        Page::Chat => {
            // While the model selector is open, pasted text goes into its
            // filter box rather than the main command input.
            if app.model_selector.is_open() {
                tracing::debug!("[choreo-tui] pasting into model selector filter");
                app.model_selector.filter.insert_str_at_cursor(data);
                return;
            }
            tracing::debug!("[choreo-tui] pasting into chat input buffer");
            // A non-empty paste mutates the buffer, so a recalled history entry
            // detaches into the draft eagerly; an empty paste edits nothing and
            // must leave browsing (and the recalled entry) intact.
            if !data.is_empty() {
                app.detach_history_on_edit();
                app.input.insert_str_at_cursor(data);
            }
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
        // Only three pages exist; `SessionManager` has no paste target, so
        // name it instead of a wildcard that would silently absorb any new
        // page added later.
        Page::SessionManager => {}
    }
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
/// consumed.  Quit is handled via Alt+Q at the terminal-event level.
fn handle_fullscreen_event(
    event: &Event,
    app: &mut App,
    _client_tx: &crossbeam_channel::Sender<ClientMessage>,
) {
    let Event::Key(key) = event else {
        return;
    };
    if key.kind != KeyEventKind::Press {
        return;
    }
    if key.code == KeyCode::Esc {
        app.fullscreen_image_target = None;
    }
}

/// Process a single `UiEvent` and return whether the event was meaningful
/// (i.e., not a control-flow event like `ReaderClosed`).
///
/// Returns `Ok(true)` when the event warrants a re-render, `Ok(false)` for
/// control flow events, or `Err` on error.
pub(super) fn handle_ui_event(
    event: UiEvent,
    app: &mut App,
    client_tx: &crossbeam_channel::Sender<ClientMessage>,
) -> Result<bool, ClientError> {
    match event {
        UiEvent::Daemon(message) => {
            // Real daemon traffic means the connection is now established, so
            // the autostart reassurance ("no daemon running — starting…" /
            // "daemon started") has served its purpose. Clear it ONLY if it is
            // still the transient status — a status the daemon itself set (a
            // handler below) is left untouched, and `Status` arriving later
            // would re-arm the flag. Without this the "daemon started" text
            // lingers on the status line until the first daemon handler
            // happens to write one.
            if app.status_is_transient {
                app.status = None;
                app.status_is_transient = false;
            }
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
        UiEvent::Status(message) => {
            // Connection-task feedback while the connection is still being
            // established (currently: the daemon-autostart wait). Unlike a
            // daemon message this never scrolls or mutates a view — it just
            // sets the status line, flagged transient so the first real
            // daemon message clears it (see the `Daemon` arm above).
            app.status = Some(message);
            app.status_is_transient = true;
            Ok(true)
        }
    }
}
