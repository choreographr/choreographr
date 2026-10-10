//! Selection/browsing lifecycle on session and page switches and terminal
//! resizes.

use crate::state::Page;
use crate::test_util::test_app;

// ── text selection lifecycle ──

#[test]
fn reset_for_session_switch_clears_text_selection() {
    // A selection is stored in screen coordinates keyed to the previous
    // session's rendered content; switching sessions must clear it so a
    // stale rectangle can never highlight another session's history.
    let mut app = test_app();
    app.text_selection = Some(crate::selection::TextSelection {
        anchor: (0, 0),
        head: (2, 3),
        cursor: (0, 0),
        active: true,
        head_sync: None,
    });
    app.reset_for_session_switch(1);
    assert!(
        app.text_selection.is_none(),
        "session switch must clear the in-progress selection"
    );
}

#[test]
fn reset_for_session_switch_ends_history_browsing() {
    // A history-recalled entry belongs to the session being left; switch
    // must end browsing so the field's "reset on session switch" contract
    // holds even when the input hand-off is skipped.
    let mut app = test_app();
    app.history_index = Some(0);
    app.reset_for_session_switch(1);
    assert!(
        app.history_index.is_none(),
        "session switch must end history browsing"
    );
}

#[test]
fn set_page_clears_text_selection() {
    // Leaving the Chat page invalidates the selection's screen-coordinate
    // context (it is keyed to the history it was drawn over); a stale
    // gesture must not survive a page switch and swallow the first click
    // on return.
    let mut app = test_app();
    app.text_selection = Some(crate::selection::TextSelection {
        anchor: (0, 0),
        head: (2, 3),
        cursor: (0, 0),
        active: true,
        head_sync: None,
    });
    app.set_page(Page::SessionManager);
    assert!(
        app.text_selection.is_none(),
        "page switch must clear the in-progress selection"
    );
}

#[test]
fn terminal_resize_clears_text_selection() {
    // A resize re-wraps every rendered line, so a stored (content line,
    // viewport column) anchor would point at different text afterwards.
    // The gesture must be dropped exactly like a suspend/page switch
    // drops it (the anchor is deliberately never re-resolved).
    let mut app = test_app();
    app.history_viewport.width = 80;
    app.history_viewport.height = 30;
    app.text_selection = Some(crate::selection::TextSelection {
        anchor: (0, 0),
        head: (2, 3),
        cursor: (0, 0),
        active: true,
        head_sync: None,
    });
    // A different cached terminal size drives a viewport change without
    // touching a real terminal (crossterm::size() is only queried when
    // `terminal_resized` is set).
    app.last_terminal_size = Some((60, 20));
    app.terminal_resized = false;
    app.update_viewport_from_terminal_size();
    assert!(
        app.text_selection.is_none(),
        "terminal resize must clear the in-progress selection"
    );
}
