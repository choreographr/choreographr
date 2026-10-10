use crate::state::App;
use choreo_client_core::ClientError;
use crossterm::event::{KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};

// The connection module is split into per-page submodules so the monolithic
// `connection.rs` is navigable; the entry points (`run_app`,
// `handle_terminal_event`, `handle_ui_event`) dispatch into them via the
// siblings below. Alongside the per-page handlers the split also carves out
// three support siblings: `resume` (suspend/resume signal coordination plus
// the shutdown-notify probe), `terminal_event` (crossterm-event normalisation
// and routing — kitty flags, paste, fullscreen, `UiEvent`), and `ui_loop`
// (`run_app` and the `select!` event loop). The re-exports below keep the
// existing `connection::*` paths resolving, and the per-page handlers are
// `pub(super)` — visible to this module only — because nothing outside the
// connection glue calls them.
mod ai_providers;
mod chat;
mod command;
mod daemon;
mod model_selector;
mod resume;
mod session_manager;
mod terminal_event;
mod ui_loop;

// `run_app` is the crate's own entry point (`lib.rs` calls it); keep the path
// `crate::connection::run_app` resolving.
pub(crate) use ui_loop::run_app;

// `handle_daemon_message` and `handle_terminal_event` are referenced from
// outside the module only by the crate's `#[cfg(test)]` test modules
// (app_tests.rs, input_tests.rs, …), so the re-exports are gated to the test
// build — the lib build reaches these through the sibling modules directly,
// and an ungated re-export would be flagged unused there.
#[cfg(test)]
pub(crate) use daemon::handle_daemon_message;
#[cfg(test)]
pub(crate) use terminal_event::handle_terminal_event;

// Test-only imports: the in-file `#[cfg(test)] mod tests` unit tests build
// `Event`s and messages by hand and `use super::*` to reach the names below,
// which no non-test code in this module touches after the split (mouse
// construction, the signal→resume-command mapping, kitty normalisation, and
// `handle_daemon_message` round-trips are exercised only there). Gated so the
// lib build stays clippy-clean.
#[cfg(test)]
use crate::selection;
#[cfg(test)]
use choreo_proto::{DaemonMessage, DaemonMessageType};
#[cfg(test)]
use resume::{ResumeCommand, notify_disconnected, signal_to_resume_command};
#[cfg(test)]
use terminal_event::{KITTY_KEYBOARD_FLAGS, handle_ui_event, normalize_kitty_shift, shift_char};

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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{Marker, UiEvent};
    use crate::test_util::test_app;
    use choreo_proto::Turn;
    // The crossterm types the page handlers used to pull in through
    // `super::*` are no longer imported by this module, so the tests name the
    // ones they construct directly.
    use crossbeam_channel as channel;
    use crossterm::event::{Event, KeyCode, KeyboardEnhancementFlags};
    #[cfg(unix)]
    use nix::sys::signal::Signal;

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
        // A Ctrl+letter editing chord must arrive as a distinct CSI-u key
        // event (e.g. CSI 109;5 u for Ctrl+M); DISAMBIGUATE_ESCAPE_CODES alone
        // gives us that because Ctrl+letter is a "disambiguated" key, while
        // plain text stays as legacy bytes.
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
            // `virtual_slot` is a u16 track coordinate, so halving it always
            // fits back into u16.
            #[expect(clippy::cast_possible_truncation)]
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
                "click on marker at content_line {cl} should make it the first visible line",
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
        let (tx, _rx) = crossbeam_channel::unbounded();
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
        let (tx, _rx) = crossbeam_channel::unbounded();
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
    fn version_mismatch_quit_message_is_actionable() {
        // A daemon built against an older PROTOCOL_VERSION must read as a
        // version incompatibility with a restart hint, never as a raw
        // codec error the user cannot act on.
        let error =
            ClientError::Proto(choreo_proto::ProtoError::UnsupportedVersion { version: 99 });
        let msg = crate::connection_quit_message(&error);
        assert!(
            msg.contains("protocol version is incompatible"),
            "must name the incompatibility, got: {msg}"
        );
        assert!(
            msg.contains("restart the daemon"),
            "must carry the restart hint, got: {msg}"
        );
        // The prefix stays stable for anything upstream that matches on it.
        assert!(msg.starts_with("connection to the daemon failed: "));
    }

    #[test]
    fn ordinary_connection_errors_keep_the_generic_wording() {
        // Only the version-mismatch case is remapped; everything else keeps
        // the historical "connection to the daemon failed: {error}" shape.
        let error = ClientError::Io(std::io::Error::new(
            std::io::ErrorKind::ConnectionRefused,
            "refused",
        ));
        assert_eq!(
            crate::connection_quit_message(&error),
            format!("connection to the daemon failed: {error}")
        );
    }

    #[test]
    fn reader_closed_quits_with_message() {
        let mut app = test_app();
        let (tx, _rx) = crossbeam_channel::unbounded();

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
    fn status_event_sets_the_status_line_and_requests_a_repaint() {
        // The autostart wait's "no daemon running — starting choreographr…"
        // feedback travels as a Status event: it must land on the status line
        // (and count as a re-render trigger) but never touch the views or
        // quit state.
        let mut app = test_app();
        let (tx, _rx) = crossbeam_channel::unbounded();

        let dirty = handle_ui_event(
            UiEvent::Status("no daemon running — starting choreographr…".to_string()),
            &mut app,
            &tx,
        )
        .expect("handle Status");

        assert!(dirty, "a status change must trigger a repaint");
        assert_eq!(
            app.status.as_deref(),
            Some("no daemon running — starting choreographr…")
        );
        assert!(
            app.status_is_transient,
            "a connection-task status must be flagged transient"
        );
        assert!(!app.should_quit, "a status event must never quit");
        assert!(app.quit_message.is_none());
    }

    #[test]
    fn first_daemon_message_clears_a_transient_status() {
        // The autostart reassurance must not linger once real daemon traffic
        // arrives: the "daemon started" text is cleared on the first daemon
        // message. `AccountListFailed` is used because it surfaces an ERROR
        // and returns early without touching the status line, so the cleared
        // (blank) status is observable rather than being overwritten by the
        // handler itself.
        let mut app = test_app();
        let (tx, _rx) = crossbeam_channel::unbounded();
        handle_ui_event(UiEvent::Status("daemon started".to_string()), &mut app, &tx)
            .expect("handle Status");
        assert_eq!(app.status.as_deref(), Some("daemon started"));

        handle_ui_event(
            UiEvent::Daemon(Box::new(DaemonMessage::broadcast(
                DaemonMessageType::AccountListFailed {
                    error: "boom".to_string(),
                },
            ))),
            &mut app,
            &tx,
        )
        .expect("handle Daemon");
        assert!(
            app.status.is_none(),
            "the transient status must be cleared by the first daemon message"
        );
        assert!(!app.status_is_transient);
        assert!(app.error.is_some(), "the daemon's error still lands");
    }

    #[test]
    fn daemon_message_does_not_clear_a_daemon_status() {
        // A status the daemon itself set must survive subsequent daemon
        // traffic — only the transient connection-task flag is cleared.
        let mut app = test_app();
        let (tx, _rx) = crossbeam_channel::unbounded();
        app.status = Some("a daemon-set status".to_string());
        app.status_is_transient = false;

        handle_ui_event(
            UiEvent::Daemon(Box::new(DaemonMessage::broadcast(
                DaemonMessageType::AccountListFailed {
                    error: "boom".to_string(),
                },
            ))),
            &mut app,
            &tx,
        )
        .expect("handle Daemon");
        assert_eq!(app.status.as_deref(), Some("a daemon-set status"));
    }

    #[test]
    fn reader_closed_keeps_existing_quit_message() {
        // The daemon flushes `ShuttingDown` before closing the socket, so the
        // TUI normally learns the reason from the message and only then sees
        // the EOF. ReaderClosed must not overwrite that reason with the
        // generic "connection closed" text.
        let mut app = test_app();
        let (tx, _rx) = crossbeam_channel::unbounded();
        handle_daemon_message(
            DaemonMessage::broadcast(DaemonMessageType::ShuttingDown),
            &mut app,
            &tx,
        )
        .expect("handle ShuttingDown");

        handle_ui_event(UiEvent::ReaderClosed, &mut app, &tx).expect("handle ReaderClosed");

        assert_eq!(
            app.quit_message.as_deref(),
            Some("the server is shutting down"),
            "the specific reason must survive the trailing EOF"
        );
    }
}
