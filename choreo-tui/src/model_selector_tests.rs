use crate::connection::{handle_daemon_message, handle_terminal_event};
use crate::state::{App, PROVIDER_PAGE_LINES, selector_list_layout};
use crate::test_util::test_app;
use choreo_proto::{ClientMessage, DaemonMessage};
use crossterm::event::{
    Event, KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use ratatui::layout::Rect;

/// Drive one mouse event through the full `handle_terminal_event` path
/// (dispatch to the model-selector overlay handler).
fn send_mouse(
    app: &mut App,
    kind: MouseEventKind,
    column: u16,
    row: u16,
    tx: &std::sync::mpsc::Sender<ClientMessage>,
) {
    handle_terminal_event(
        Event::Mouse(MouseEvent {
            kind,
            column,
            row,
            modifiers: KeyModifiers::NONE,
        }),
        app,
        tx,
    )
    .expect("handle mouse event");
}

// ── Model selector (Ctrl+M) ──

#[test]
fn chat_ctrl_m_opens_selector_and_requests_models() {
    let (tx, rx) = std::sync::mpsc::channel();
    let mut app = test_app();

    handle_terminal_event(
        Event::Key(KeyEvent::new(KeyCode::Char('m'), KeyModifiers::CONTROL)),
        &mut app,
        &tx,
    )
    .expect("handle ctrl+m");

    assert!(app.model_selector.is_open(), "ctrl+m opens the selector");
    assert!(app.model_selector.loading, "selector waits for the reply");
    let msg = rx.recv().expect("sent message");
    assert_eq!(msg, ClientMessage::ListModels);
}

#[test]
fn model_selector_populates_from_models_reply() {
    let (tx, _rx) = std::sync::mpsc::channel();
    let mut app = test_app();
    app.model_selector.open();

    handle_daemon_message(
        DaemonMessage::Models {
            models: vec![
                "gpt-4o".to_string(),
                "gpt-4o-mini".to_string(),
                "claude-3".to_string(),
            ],
            selected_model: Some("gpt-4o-mini".to_string()),
        },
        &mut app,
        &tx,
    )
    .expect("handle Models");

    assert!(!app.model_selector.loading);
    assert_eq!(app.model_selector.all_models.len(), 3);
    assert_eq!(
        app.model_selector.highlighted().as_deref(),
        Some("gpt-4o-mini"),
        "the active model is preselected"
    );
    // The reply must not leak into the chat status line.
    assert!(app.status.is_none());
}

#[test]
fn model_selector_enter_sends_set_model_and_closes() {
    let (tx, rx) = std::sync::mpsc::channel();
    let mut app = test_app();
    app.model_selector.open();
    app.model_selector.apply_models(
        vec!["gpt-4o".to_string(), "claude-3".to_string()],
        Some("gpt-4o".to_string()),
    );

    handle_terminal_event(
        Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
        &mut app,
        &tx,
    )
    .expect("handle enter");

    assert!(!app.model_selector.is_open(), "enter closes the selector");
    let msg = rx.recv().expect("sent message");
    assert_eq!(
        msg,
        ClientMessage::SetModel {
            model: "gpt-4o".to_string()
        }
    );
}

#[test]
fn model_selector_esc_closes_without_sending() {
    let (tx, rx) = std::sync::mpsc::channel();
    let mut app = test_app();
    app.model_selector.open();

    handle_terminal_event(
        Event::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
        &mut app,
        &tx,
    )
    .expect("handle esc");

    assert!(!app.model_selector.is_open(), "esc closes the selector");
    assert!(
        rx.try_recv().is_err(),
        "no message should be sent on dismiss"
    );
}

#[test]
fn model_selector_filter_narrows_and_submits_highlighted() {
    let (tx, rx) = std::sync::mpsc::channel();
    let mut app = test_app();
    app.model_selector.open();
    app.model_selector.apply_models(
        vec![
            "gpt-4o".to_string(),
            "gpt-4o-mini".to_string(),
            "claude-3".to_string(),
        ],
        Some("claude-3".to_string()),
    );

    // Type "mini" into the filter.
    for c in "mini".chars() {
        handle_terminal_event(
            Event::Key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE)),
            &mut app,
            &tx,
        )
        .expect("handle filter char");
    }

    assert_eq!(app.model_selector.filtered(), vec!["gpt-4o-mini"]);
    assert_eq!(
        app.model_selector.highlighted().as_deref(),
        Some("gpt-4o-mini")
    );

    handle_terminal_event(
        Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
        &mut app,
        &tx,
    )
    .expect("handle enter");

    assert!(!app.model_selector.is_open());
    let msg = rx.recv().expect("sent message");
    assert_eq!(
        msg,
        ClientMessage::SetModel {
            model: "gpt-4o-mini".to_string()
        }
    );
}

#[test]
fn model_selector_down_moves_highlight() {
    let (tx, _rx) = std::sync::mpsc::channel();
    let mut app = test_app();
    app.model_selector.open();
    app.model_selector.apply_models(
        vec!["a".to_string(), "b".to_string(), "c".to_string()],
        None,
    );
    assert_eq!(app.model_selector.highlighted().as_deref(), Some("a"));

    handle_terminal_event(
        Event::Key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE)),
        &mut app,
        &tx,
    )
    .expect("handle down");

    assert_eq!(app.model_selector.highlighted().as_deref(), Some("b"));
}

#[test]
fn model_selector_models_reply_falls_through_when_closed() {
    let (tx, _rx) = std::sync::mpsc::channel();
    let mut app = test_app();

    // Selector closed: `/model` behavior — the reply prints into the status.
    handle_daemon_message(
        DaemonMessage::Models {
            models: vec!["gpt-4o".to_string()],
            selected_model: Some("gpt-4o".to_string()),
        },
        &mut app,
        &tx,
    )
    .expect("handle Models");

    assert!(
        app.status
            .as_deref()
            .is_some_and(|s| s.contains("supported models")),
        "closed selector keeps the chat-history behavior"
    );
}

#[test]
fn model_selector_failed_reply_shows_error_when_open() {
    let (tx, _rx) = std::sync::mpsc::channel();
    let mut app = test_app();
    app.model_selector.open();

    handle_daemon_message(
        DaemonMessage::ModelsFailed {
            error: "no credential".to_string(),
        },
        &mut app,
        &tx,
    )
    .expect("handle ModelsFailed");

    assert!(!app.model_selector.loading);
    assert_eq!(app.model_selector.error.as_deref(), Some("no credential"));
    assert!(app.status.is_none(), "error stays scoped to the popup");
}

#[test]
fn model_selector_paste_goes_to_filter() {
    let (tx, _rx) = std::sync::mpsc::channel();
    let mut app = test_app();
    app.model_selector.open();
    app.model_selector
        .apply_models(vec!["gpt-4o-mini".to_string()], None);

    handle_terminal_event(Event::Paste("mini".to_string()), &mut app, &tx).expect("handle paste");

    assert_eq!(app.model_selector.filter.text, "mini");
    assert!(
        app.input.text.is_empty(),
        "paste must not hit the main input"
    );
}

#[test]
fn model_selector_page_keys_jump_highlight() {
    let (tx, _rx) = std::sync::mpsc::channel();
    let mut app = test_app();
    app.model_selector.open();
    app.model_selector
        .apply_models((0..30).map(|i| format!("model-{i}")).collect(), None);

    handle_terminal_event(
        Event::Key(KeyEvent::new(KeyCode::PageDown, KeyModifiers::NONE)),
        &mut app,
        &tx,
    )
    .expect("handle page down");
    assert_eq!(app.model_selector.focused, PROVIDER_PAGE_LINES);

    handle_terminal_event(
        Event::Key(KeyEvent::new(KeyCode::PageUp, KeyModifiers::NONE)),
        &mut app,
        &tx,
    )
    .expect("handle page up");
    assert_eq!(app.model_selector.focused, 0);

    // PgUp at the top clamps to 0.
    handle_terminal_event(
        Event::Key(KeyEvent::new(KeyCode::PageUp, KeyModifiers::NONE)),
        &mut app,
        &tx,
    )
    .expect("handle page up at top");
    assert_eq!(app.model_selector.focused, 0);

    // PgDn past the last model clamps to it.
    app.model_selector.focused = 29;
    handle_terminal_event(
        Event::Key(KeyEvent::new(KeyCode::PageDown, KeyModifiers::NONE)),
        &mut app,
        &tx,
    )
    .expect("handle page down at bottom");
    assert_eq!(app.model_selector.focused, 29);

    // Paging must not leak into the filter buffer.
    assert!(app.model_selector.filter.text.is_empty());
}

#[test]
fn model_selector_wheel_down_scrolls_with_pin_behavior() {
    let (tx, _rx) = std::sync::mpsc::channel();
    let mut app = test_app();
    app.model_selector.open();
    app.model_selector.viewport_height = 10;
    app.model_selector
        .apply_models((0..20).map(|i| format!("model-{i}")).collect(), None);

    // 5 wheel-down notches walk the highlight to the middle row…
    for _ in 0..5 {
        send_mouse(&mut app, MouseEventKind::ScrollDown, 0, 0, &tx);
    }
    assert_eq!(
        app.model_selector.focused, 5,
        "highlight reaches the middle row"
    );
    assert_eq!(app.model_selector.scroll, 0);

    // …then further notches keep it pinned there while the list slides under
    // it (focused − scroll stays 5)…
    for _ in 0..5 {
        send_mouse(&mut app, MouseEventKind::ScrollDown, 0, 0, &tx);
    }
    assert_eq!(
        (app.model_selector.focused, app.model_selector.scroll),
        (10, 5),
        "pinned at the middle while scrolling"
    );

    // …and at the bottom edge it un-pins and walks to the last model.
    for _ in 0..9 {
        send_mouse(&mut app, MouseEventKind::ScrollDown, 0, 0, &tx);
    }
    assert_eq!(
        (app.model_selector.focused, app.model_selector.scroll),
        (19, 10),
        "un-pinned at the bottom edge, walked to the last model"
    );

    // Further notches are no-ops at the bottom.
    send_mouse(&mut app, MouseEventKind::ScrollDown, 0, 0, &tx);
    assert_eq!(app.model_selector.focused, 19);
}

#[test]
fn model_selector_wheel_up_mirrors_wheel_down() {
    let (tx, _rx) = std::sync::mpsc::channel();
    let mut app = test_app();
    app.model_selector.open();
    app.model_selector.viewport_height = 10;
    app.model_selector
        .apply_models((0..20).map(|i| format!("model-{i}")).collect(), None);

    // Park the highlight at the last model with the window at max_scroll.
    app.model_selector.focused = 19;
    app.model_selector.scroll = 10;

    // Wheel-up walks the highlight up through the static window to the
    // middle, pins it while scrolling up, then un-pins at the top edge.
    for _ in 0..4 {
        send_mouse(&mut app, MouseEventKind::ScrollUp, 0, 0, &tx);
    }
    assert_eq!(
        (app.model_selector.focused, app.model_selector.scroll),
        (15, 10)
    );
    for _ in 0..10 {
        send_mouse(&mut app, MouseEventKind::ScrollUp, 0, 0, &tx);
    }
    assert_eq!(
        (app.model_selector.focused, app.model_selector.scroll),
        (5, 0)
    );
    for _ in 0..5 {
        send_mouse(&mut app, MouseEventKind::ScrollUp, 0, 0, &tx);
    }
    assert_eq!(
        (app.model_selector.focused, app.model_selector.scroll),
        (0, 0)
    );

    // Further notches are no-ops at the top.
    send_mouse(&mut app, MouseEventKind::ScrollUp, 0, 0, &tx);
    assert_eq!(app.model_selector.focused, 0);
}

#[test]
fn model_selector_click_row_selects_and_sends_set_model() {
    let (tx, rx) = std::sync::mpsc::channel();
    let mut app = test_app();
    app.model_selector.open();
    app.last_terminal_size = Some((100, 40));
    app.model_selector.apply_models(
        vec![
            "gpt-4o".to_string(),
            "gpt-4o-mini".to_string(),
            "claude-3".to_string(),
        ],
        None,
    );

    let layout = selector_list_layout(Rect {
        x: 0,
        y: 0,
        width: 100,
        height: 40,
    });
    // Click the second row of the list body (scroll is 0).
    send_mouse(
        &mut app,
        MouseEventKind::Down(MouseButton::Left),
        layout.body.x + 3,
        layout.body.y + 1,
        &tx,
    );

    assert!(
        !app.model_selector.is_open(),
        "a row click selects exactly like Enter"
    );
    let msg = rx.recv().expect("sent message");
    assert_eq!(
        msg,
        ClientMessage::SetModel {
            model: "gpt-4o-mini".to_string()
        }
    );
}

#[test]
fn model_selector_click_filter_row_positions_cursor() {
    let (tx, _rx) = std::sync::mpsc::channel();
    let mut app = test_app();
    app.model_selector.open();
    app.last_terminal_size = Some((100, 40));
    app.model_selector.filter.text = "gpt".to_string();
    app.model_selector.filter.cursor = 0;

    let layout = selector_list_layout(Rect {
        x: 0,
        y: 0,
        width: 100,
        height: 40,
    });
    // Click one column past the "> " prefix → cursor at byte 1.
    send_mouse(
        &mut app,
        MouseEventKind::Down(MouseButton::Left),
        layout.filter_row.x + 3,
        layout.filter_row.y,
        &tx,
    );

    assert_eq!(
        app.model_selector.filter.cursor, 1,
        "the click column minus the prefix maps to the cursor position"
    );
    assert!(
        app.model_selector.is_open(),
        "a filter-row click must not select anything"
    );
}

#[test]
fn model_selector_click_outside_popup_is_noop() {
    let (tx, rx) = std::sync::mpsc::channel();
    let mut app = test_app();
    app.model_selector.open();
    app.last_terminal_size = Some((100, 40));
    app.model_selector
        .apply_models(vec!["gpt-4o".to_string()], None);

    // (0, 0) is the dimmed area well outside the centered popup.
    send_mouse(&mut app, MouseEventKind::Down(MouseButton::Left), 0, 0, &tx);

    assert!(
        app.model_selector.is_open(),
        "outside click must not select"
    );
    assert_eq!(app.model_selector.focused, 0);
    assert!(rx.try_recv().is_err(), "no message sent");
}

#[test]
fn model_selector_click_footer_is_noop() {
    let (tx, rx) = std::sync::mpsc::channel();
    let mut app = test_app();
    app.model_selector.open();
    app.last_terminal_size = Some((100, 40));
    app.model_selector
        .apply_models(vec!["gpt-4o".to_string()], None);

    let layout = selector_list_layout(Rect {
        x: 0,
        y: 0,
        width: 100,
        height: 40,
    });
    // A click on the footer hint row is inside the popup but not a list row.
    send_mouse(
        &mut app,
        MouseEventKind::Down(MouseButton::Left),
        layout.footer.x + 2,
        layout.footer.y,
        &tx,
    );

    assert!(app.model_selector.is_open(), "footer click must not select");
    assert_eq!(app.model_selector.focused, 0);
    assert!(rx.try_recv().is_err(), "no message sent");
}

#[test]
fn model_selector_click_after_page_jump_maps_to_drawn_row() {
    let (tx, rx) = std::sync::mpsc::channel();
    let mut app = test_app();
    app.model_selector.open();
    app.last_terminal_size = Some((100, 40));
    app.model_selector.viewport_height = 10;
    app.model_selector
        .apply_models((0..30).map(|i| format!("model-{i}")).collect(), None);
    // PgDn jumps the highlight without touching `scroll`: `picker_window`
    // then pushes the drawn window down to keep the jumped focus visible, so
    // the stored `scroll` (still 0) no longer equals the rendered window
    // start — a click resolved against the raw `scroll` would select the row
    // above the one drawn.  (The filter-narrowing path used to leave `scroll`
    // stale the same way, but `clamp_focus` now clamps it to the true
    // max_scroll at every mutation; the page jump is the remaining path that
    // moves `focused` without touching `scroll`.)
    handle_terminal_event(
        Event::Key(KeyEvent::new(KeyCode::PageDown, KeyModifiers::NONE)),
        &mut app,
        &tx,
    )
    .expect("page down");

    // focused = 10 after the jump; the renderer draws rows 1..11, so the
    // first visible body row is filtered[1] = "model-1".
    let filtered = app.model_selector.filtered();
    let (start, _) = app.model_selector.window(&filtered, 10);
    assert_eq!(start, 1, "renderer shows rows 1..11");
    assert_eq!(
        app.model_selector.scroll, 0,
        "the stored anchor is untouched"
    );

    let layout = selector_list_layout(Rect {
        x: 0,
        y: 0,
        width: 100,
        height: 40,
    });
    // Click the FIRST visible body row — what the user sees at the top of
    // the list.  The pick must land on the drawn row (filtered[1]), not on a
    // stale-scroll offset (filtered[0]).
    send_mouse(
        &mut app,
        MouseEventKind::Down(MouseButton::Left),
        layout.body.x + 3,
        layout.body.y,
        &tx,
    );

    assert!(
        !app.model_selector.is_open(),
        "a row click selects exactly like Enter"
    );
    let msg = rx.recv().expect("sent message");
    assert_eq!(
        msg,
        ClientMessage::SetModel {
            model: "model-1".to_string()
        },
        "click maps to the row that was actually drawn (window start 1)"
    );
}

#[test]
fn model_selector_click_while_loading_is_noop() {
    let (tx, rx) = std::sync::mpsc::channel();
    let mut app = test_app();
    app.model_selector.open();
    app.last_terminal_size = Some((100, 40));
    app.model_selector
        .apply_models(vec!["gpt-4o".to_string(), "gpt-4o-mini".to_string()], None);
    // Re-open: `open()` keeps the previous `all_models` (so a re-open shows
    // results immediately once the fresh reply lands) and sets `loading` back
    // to true — the popup draws "Loading models…", not the stale list, so a
    // click must not select a model that is not drawn.
    app.model_selector.open();
    assert!(app.model_selector.loading);

    let layout = selector_list_layout(Rect {
        x: 0,
        y: 0,
        width: 100,
        height: 40,
    });
    send_mouse(
        &mut app,
        MouseEventKind::Down(MouseButton::Left),
        layout.body.x + 3,
        layout.body.y,
        &tx,
    );

    assert!(
        app.model_selector.is_open(),
        "a click while loading must not select"
    );
    assert_eq!(app.model_selector.focused, 0);
    assert!(rx.try_recv().is_err(), "no message sent");
}

#[test]
fn model_selector_click_after_failed_refresh_is_noop() {
    let (tx, rx) = std::sync::mpsc::channel();
    let mut app = test_app();
    app.model_selector.open();
    app.last_terminal_size = Some((100, 40));
    app.model_selector
        .apply_models(vec!["gpt-4o".to_string()], None);
    // A failed refresh leaves the stale list in place but replaces the body
    // with the error text — no rows are drawn, so a click must not select one.
    app.model_selector.apply_error("daemon unreachable");

    let layout = selector_list_layout(Rect {
        x: 0,
        y: 0,
        width: 100,
        height: 40,
    });
    send_mouse(
        &mut app,
        MouseEventKind::Down(MouseButton::Left),
        layout.body.x + 3,
        layout.body.y,
        &tx,
    );

    assert!(
        app.model_selector.is_open(),
        "a click after a failed refresh must not select"
    );
    assert!(rx.try_recv().is_err(), "no message sent");
}

#[test]
fn model_selector_filter_row_click_while_loading_positions_cursor() {
    // The guard only skips *row* selection while the popup shows no list;
    // the filter row is still drawn, so clicking it must keep positioning
    // the cursor.
    let (tx, _rx) = std::sync::mpsc::channel();
    let mut app = test_app();
    app.model_selector.open();
    app.last_terminal_size = Some((100, 40));
    app.model_selector.filter.text = "gpt".to_string();
    app.model_selector.filter.cursor = 0;

    let layout = selector_list_layout(Rect {
        x: 0,
        y: 0,
        width: 100,
        height: 40,
    });
    send_mouse(
        &mut app,
        MouseEventKind::Down(MouseButton::Left),
        layout.filter_row.x + 3,
        layout.filter_row.y,
        &tx,
    );

    assert_eq!(
        app.model_selector.filter.cursor, 1,
        "filter-row clicks still position the cursor while loading"
    );
    assert!(app.model_selector.is_open());
}
