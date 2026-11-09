//! Terminal-event normalisation and dispatch for the UI.
//!
//! Owns the crossterm-event entry point the UI loop feeds: the kitty
//! keyboard-protocol handling (the enhancement flags requested at startup and
//! the SHIFT normalisation that restores legacy-equivalent events), the
//! bracketed-paste routing into whichever input buffer is active, the
//! pointer-hover sampling every mouse event feeds (see the block in
//! `handle_terminal_event`), the fullscreen-overlay guard, and the daemon→UI
//! `UiEvent` handler. The per-page key/mouse handlers live in the sibling page
//! modules this dispatches to; the UI loop itself lives in the sibling
//! `ui_loop` module.

use crate::state::{AccountWizardStep, App, Page, UiEvent};
use choreo_client_core::ClientError;
use choreo_proto::ClientMessage;
use crossterm::event::{
    Event, KeyCode, KeyEventKind, KeyModifiers, KeyboardEnhancementFlags, MouseEventKind,
};

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

/// Rows the read-only reference overlays (the Help modal and the Terminal page)
/// scroll per `PgUp`/`PgDn`: a page-sized jump so a full reference page can be
/// paged through quickly, larger than one transcript line.
const OVERLAY_SCROLL_PAGE: u16 = 10;

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
/// app's command chords are `Alt+` chords, and a shifted one carries SHIFT as a
/// modifier here and as the glyph's case on a legacy terminal (see
/// `state/keymap.rs::chord_from_event`), so both encodings bind the same
/// command.
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

/// A kitty-protocol terminal reports a shifted key that carries another
/// modifier as an unshifted codepoint plus an explicit SHIFT bit: `Alt+Shift+R`
/// arrives as `CSI 114;4 u` (`Char('r') + ALT + SHIFT`), where a legacy terminal
/// sends the shifted glyph as plain text (`ESC R`, which crossterm reports as
/// `Char('R') + ALT`).  Restore the legacy *glyph* — apply the US-layout shift
/// mapping — so every downstream handler (chat input, filter boxes, other pages)
/// sees exactly the `Char` a legacy terminal would have produced.
///
/// A plain shifted text key never reaches this function: with
/// `REPORT_ALL_KEYS_AS_ESCAPE_CODES` deliberately not requested (see
/// [`KITTY_KEYBOARD_FLAGS`]), the terminal sends `Shift+A` as the legacy byte
/// `'A'`, and only a combination carrying another modifier (or a key that
/// produces no text) is escaped to CSI-u.
///
/// The SHIFT **modifier is preserved**: it is a real part of a chord, so
/// `Alt+Shift+R` must stay distinct from `Alt+R` (`state/keymap.rs` reads it,
/// along with the glyph's case, to decide which chord was pressed).  Dropping it
/// here would collapse the two on kitty terminals while a legacy terminal —
/// which carries the shift in the byte's case — kept them apart.
///
/// When Ctrl is held the glyph is left alone: a legacy terminal encodes
/// `Ctrl+letter` as a control byte with no case to reconstruct from
/// (`Ctrl+Shift+M` is byte 0x0D, identical to `Ctrl+M`), so there is no legacy
/// glyph to mirror — and for the same reason the editing kernel treats control
/// chords as shift-insensitive.  Non-Char keys (e.g. Shift+Enter, Shift+Tab)
/// pass through untouched.
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
        if app.fullscreen_image_target.is_some() || app.show_terminal_page || app.show_help_modal {
            tracing::debug!("[choreo-tui] ignoring paste while a full-screen overlay is active");
            return Ok(());
        }
        tracing::debug!("[choreo-tui] handling paste event");
        handle_paste_event(data, app);
        return Ok(());
    }

    // Every mouse event carries the pointer's cell, so every one of them is a
    // hover sample — not only `Moved`.  A terminal reports button-less motion as
    // `Moved` while its any-event tracking (DECSET 1003) is in force and no
    // button is held; while it believes a button is down it reports the same
    // motion as a `Drag`, and a surface whose tracking mode has lapsed reports
    // no motion at all.  Sampling every kind keeps the pointer shape following
    // the pointer in each case instead of freezing on the last target — a
    // frozen hover is otherwise unrecoverable, because the shape is emitted only
    // when it changes, so a hover that stops being refreshed stops emitting.
    if let Event::Mouse(mouse) = &event {
        // A discrete action (click or wheel notch) landing somewhere the motion
        // stream never reported means the terminal is delivering mouse input but
        // not motion: either its any-event mode lapsed or another program on the
        // same pty reset it.  Ask the UI loop to re-assert the mode (an
        // idempotent private-mode set) so the shape starts tracking again
        // instead of staying frozen until the surface is replaced.
        //
        // `Moved`/`Drag`/`Up` deliberately never ask: motion is the healthy
        // signal, a drag arrives per cell crossed (so asking there would write
        // the mode set dozens of times per selection), and a release belongs to
        // the gesture its press started.  A latched button — which makes motion
        // arrive as a drag — is terminal-side state that re-sending the mode set
        // cannot clear; sampling drags above is what covers it.
        let discrete = matches!(
            mouse.kind,
            MouseEventKind::Down(_)
                | MouseEventKind::ScrollUp
                | MouseEventKind::ScrollDown
                | MouseEventKind::ScrollLeft
                | MouseEventKind::ScrollRight
        );
        if discrete && app.pointer_hover != Some((mouse.column, mouse.row)) {
            app.mouse_tracking_reassert = true;
        }
        app.pointer_hover = Some((mouse.column, mouse.row));
        // Bare motion is not a click or a drag: no page handler wants it, and
        // the selection gesture must not treat it as one, so it stops here.
        if mouse.kind == MouseEventKind::Moved {
            return Ok(());
        }
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
    // The OAuth login modal takes overlay priority over page content: while a
    // sign-in is in progress it swallows every key, and Esc cancels the flow
    // (tripping the worker's cancel flag). Alt+Q is handled above so the user
    // can always quit.
    if app.login.is_some() {
        if let Event::Key(key) = &event
            && key.kind == KeyEventKind::Press
            && key.code == KeyCode::Esc
        {
            app.cancel_login();
        }
        return Ok(());
    }
    // The model selector overlay (Chat page) also takes priority over page
    // content, mirroring the fullscreen guard above.  Alt+Q is handled
    // before this point so the user can always quit while it is open.
    if app.model_selector.is_open() {
        return handle_model_selector_event(&event, app, client_tx);
    }
    // The Terminal page is a read-only modal over the Chat page and takes the
    // same overlay priority: while it is open every event stops here, so a
    // keystroke can never reach the composer underneath. `Esc` and its own
    // `Alt+T` toggle close it; `Up`/`Down`/`PgUp`/`PgDn` scroll its body (the
    // page can be taller than a short terminal, and the renderer clamps the
    // offset to the content); a mouse event is inert because the page has no
    // clickable rows (Alt+Q still quits from above).
    if app.show_terminal_page {
        if let Event::Key(key) = &event
            && key.kind == KeyEventKind::Press
        {
            let alt_t = key.modifiers.contains(KeyModifiers::ALT)
                && matches!(key.code, KeyCode::Char('t' | 'T'));
            match key.code {
                KeyCode::Up => {
                    app.terminal_page_scroll = app.terminal_page_scroll.saturating_sub(1);
                }
                KeyCode::Down => {
                    app.terminal_page_scroll = app.terminal_page_scroll.saturating_add(1);
                }
                KeyCode::PageUp => {
                    app.terminal_page_scroll =
                        app.terminal_page_scroll.saturating_sub(OVERLAY_SCROLL_PAGE);
                }
                KeyCode::PageDown => {
                    app.terminal_page_scroll =
                        app.terminal_page_scroll.saturating_add(OVERLAY_SCROLL_PAGE);
                }
                _ if key.code == KeyCode::Esc || alt_t => {
                    tracing::debug!("[choreo-tui] closing the terminal page");
                    app.show_terminal_page = false;
                }
                _ => {}
            }
        }
        return Ok(());
    }
    // The Help modal is another read-only overlay over the Chat page and takes
    // the same priority: while it is open every event stops here. `Esc` and its
    // own `Alt+H` toggle close it; `Up`/`Down`/`PgUp`/`PgDn` scroll its body
    // (the page is taller than most terminals, and the renderer clamps the
    // offset to the content); a mouse event is inert because the page has no
    // clickable rows (Alt+Q still quits from above).
    if app.show_help_modal {
        if let Event::Key(key) = &event
            && key.kind == KeyEventKind::Press
        {
            let alt_h = key.modifiers.contains(KeyModifiers::ALT)
                && matches!(key.code, KeyCode::Char('h' | 'H'));
            match key.code {
                KeyCode::Up => {
                    app.help_modal_scroll = app.help_modal_scroll.saturating_sub(1);
                }
                KeyCode::Down => {
                    app.help_modal_scroll = app.help_modal_scroll.saturating_add(1);
                }
                KeyCode::PageUp => {
                    app.help_modal_scroll =
                        app.help_modal_scroll.saturating_sub(OVERLAY_SCROLL_PAGE);
                }
                KeyCode::PageDown => {
                    app.help_modal_scroll =
                        app.help_modal_scroll.saturating_add(OVERLAY_SCROLL_PAGE);
                }
                _ if key.code == KeyCode::Esc || alt_h => {
                    tracing::debug!("[choreo-tui] closing the help modal");
                    app.show_help_modal = false;
                }
                _ => {}
            }
        }
        return Ok(());
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

/// Feed one terminal event through the dispatcher and report whether the frame
/// must be repainted.
///
/// Bare pointer motion is the one event that changes something *without*
/// changing the screen: it updates the terminal's pointer shape, which the UI
/// loop emits outside `terminal.draw`, so it must not dirty the frame.
/// Everything else may have changed what is drawn, so it dirties it (an event
/// that turns out to be a no-op only costs one redundant diff of identical
/// cells).
pub(super) fn dispatch_terminal_event(
    event: Event,
    app: &mut App,
    client_tx: &crossbeam_channel::Sender<ClientMessage>,
) -> Result<bool, ClientError> {
    let pointer_only = matches!(&event, Event::Mouse(mouse) if mouse.kind == MouseEventKind::Moved);
    handle_terminal_event(event, app, client_tx)?;
    Ok(!pointer_only)
}

/// Insert pasted text into whichever input buffer is currently active.
///
/// Routes the paste to the appropriate field based on the current page
/// and overlay state.  On the Chat page the command input (or the model
/// selector's filter, when it is open) receives the paste; on the AI
/// Providers page the credential modal's key input, or the wizard's
/// provider filter / slug field, receives it.
///
/// Every paste target sits behind this one function, so it is the single choke
/// point where untrusted clipboard bytes meet an editing buffer: the text is
/// sanitized once, here, before any branch sees it.
fn handle_paste_event(data: &str, app: &mut App) {
    // The buffer's bytes are what the daemon receives and the cursor is a byte
    // offset into them, so the paste is rewritten on the way in (control
    // characters dropped, tabs expanded, CR folded) rather than escaped at draw
    // time — see `text_filter::sanitize_pasted_text`.  A paste that sanitizes to
    // nothing edits nothing, so it must return before any branch runs: the Chat
    // branch, for one, detaches a recalled history entry on a real edit.
    let sanitized = crate::text_filter::sanitize_pasted_text(data);
    let data = sanitized.as_ref();
    if data.is_empty() {
        tracing::debug!("[choreo-tui] paste sanitized to nothing; ignoring");
        return;
    }
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
            // A recalled history entry detaches into the draft eagerly: the
            // paste is known non-empty here (an empty or fully sanitized-away
            // paste returned above), so this is always a real edit.
            app.detach_history_on_edit();
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
                app.clear_status();
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
            app.set_quit_message_if_absent("the connection to the daemon was closed");
            Ok(false)
        }
        UiEvent::Status(message) => {
            // Connection-task feedback while the connection is still being
            // established (currently: the daemon-autostart wait). Unlike a
            // daemon message this never scrolls or mutates a view — it just
            // sets the status line, flagged transient so the first real
            // daemon message clears it (see the `Daemon` arm above).
            app.set_status(message);
            app.status_is_transient = true;
            Ok(true)
        }
    }
}
