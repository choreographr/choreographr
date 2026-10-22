use crate::connection::handle_terminal_event;
use crate::state::*;
use crate::tests::support::test_app;
use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEventKind};
use ratatui::layout::Rect;

use super::send_mouse;

// ── New-account wizard mouse support ──────────────────────

#[test]
fn ai_providers_wizard_wheel_down_pins_at_middle_then_unpins() {
    let (tx, _rx) = crossbeam_channel::unbounded();
    let mut app = test_app();
    app.page = Page::AIProviders;
    app.ai_providers.wizard.open();
    app.ai_providers.wizard.viewport_height = 10;

    // 5 wheel-down notches walk the highlight to the middle row…
    for _ in 0..5 {
        send_mouse(&mut app, MouseEventKind::ScrollDown, 0, 0, &tx);
    }
    assert_eq!(
        app.ai_providers.wizard.focused, 5,
        "highlight reaches the middle row"
    );
    assert_eq!(app.ai_providers.wizard.scroll, 0);

    // …then further notches keep it pinned there while the list slides under
    // it (focused − scroll stays 5)…
    for _ in 0..5 {
        send_mouse(&mut app, MouseEventKind::ScrollDown, 0, 0, &tx);
    }
    assert_eq!(
        (
            app.ai_providers.wizard.focused,
            app.ai_providers.wizard.scroll
        ),
        (10, 5),
        "pinned at the middle while scrolling"
    );

    // …and at the bottom edge it un-pins and walks to the last provider
    // (generous notches: past the last item every press is a no-op).
    let last = app.providers.len() - 1;
    for _ in 0..(last + 5) {
        send_mouse(&mut app, MouseEventKind::ScrollDown, 0, 0, &tx);
    }
    assert_eq!(
        app.ai_providers.wizard.focused, last,
        "un-pinned at the bottom edge, walked to the last provider"
    );
    assert_eq!(
        app.ai_providers.wizard.scroll,
        app.providers.len().saturating_sub(10),
        "window pinned at max_scroll"
    );
}

#[test]
fn ai_providers_wizard_wheel_up_mirrors_wheel_down() {
    let (tx, _rx) = crossbeam_channel::unbounded();
    let mut app = test_app();
    app.page = Page::AIProviders;
    app.ai_providers.wizard.open();
    app.ai_providers.wizard.viewport_height = 10;
    let len = app.providers.len();

    // Park the highlight at the last provider with the window at max_scroll.
    app.ai_providers.wizard.focused = len - 1;
    app.ai_providers.wizard.scroll = len.saturating_sub(10);

    // Wheel-up walks the highlight up through the static window to the
    // middle row (scroll + 5)…
    for _ in 0..4 {
        send_mouse(&mut app, MouseEventKind::ScrollUp, 0, 0, &tx);
    }
    assert_eq!(
        app.ai_providers.wizard.focused,
        len - 5,
        "walked up to the middle row"
    );

    // …then pins at the middle while the list scrolls up until scroll
    // reaches 0 (one press per scroll row)…
    for _ in 0..(len.saturating_sub(10)) {
        send_mouse(&mut app, MouseEventKind::ScrollUp, 0, 0, &tx);
    }
    assert_eq!(
        (
            app.ai_providers.wizard.focused,
            app.ai_providers.wizard.scroll
        ),
        (5, 0),
        "pinned at the middle while scrolling up"
    );

    // …and finally un-pins at the top edge to walk to row 0.
    for _ in 0..5 {
        send_mouse(&mut app, MouseEventKind::ScrollUp, 0, 0, &tx);
    }
    assert_eq!(
        (
            app.ai_providers.wizard.focused,
            app.ai_providers.wizard.scroll
        ),
        (0, 0),
        "un-pinned at the top edge, walked to the first row"
    );
}

#[test]
fn ai_providers_wizard_click_row_selects_provider() {
    let (tx, _rx) = crossbeam_channel::unbounded();
    let mut app = test_app();
    app.page = Page::AIProviders;
    app.ai_providers.wizard.open();
    app.last_terminal_size = Some((100, 40));

    let layout = selector_list_layout(Rect {
        x: 0,
        y: 0,
        width: 100,
        height: 40,
    });
    // Click the first row of the list body (scroll is 0).
    send_mouse(
        &mut app,
        MouseEventKind::Down(MouseButton::Left),
        layout.body.x + 3,
        layout.body.y,
        &tx,
    );

    assert_eq!(
        app.ai_providers.wizard.step,
        AccountWizardStep::Slug,
        "a row click advances to the slug step exactly like Enter"
    );
    assert_eq!(
        app.ai_providers.wizard.picked_slug.as_deref(),
        Some(app.providers[0].slug.as_str()),
        "the clicked row is the picked provider"
    );
}

#[test]
fn ai_providers_wizard_click_row_maps_scroll_offset() {
    let (tx, _rx) = crossbeam_channel::unbounded();
    let mut app = test_app();
    app.page = Page::AIProviders;
    app.ai_providers.wizard.open();
    app.last_terminal_size = Some((100, 40));
    // Cache the LIST-popup body height the way the event loop does, so the
    // rendered window start (what `window()` reports) matches the drawn rows.
    app.update_viewport_from_terminal_size();
    // The visible window starts at row 5, so the click maps to 5 + local row.
    app.ai_providers.wizard.scroll = 5;
    app.ai_providers.wizard.focused = 5;

    let layout = selector_list_layout(Rect {
        x: 0,
        y: 0,
        width: 100,
        height: 40,
    });
    // Click the third visible row → filtered index 5 + 2 = 7 (resolved
    // through the rendered window, not the stored `scroll`).
    send_mouse(
        &mut app,
        MouseEventKind::Down(MouseButton::Left),
        layout.body.x + 3,
        layout.body.y + 2,
        &tx,
    );

    assert_eq!(app.ai_providers.wizard.step, AccountWizardStep::Slug);
    assert_eq!(
        app.ai_providers.wizard.picked_slug.as_deref(),
        Some(app.providers[7].slug.as_str()),
        "scroll offset is added to the local row"
    );
}

#[test]
fn ai_providers_wizard_click_filter_row_positions_cursor() {
    let (tx, _rx) = crossbeam_channel::unbounded();
    let mut app = test_app();
    app.page = Page::AIProviders;
    app.ai_providers.wizard.open();
    app.last_terminal_size = Some((100, 40));
    app.ai_providers.wizard.filter.text = "abc".to_string();
    app.ai_providers.wizard.filter.cursor = 0;

    let layout = selector_list_layout(Rect {
        x: 0,
        y: 0,
        width: 100,
        height: 40,
    });
    // Click two columns past the "> " prefix → cursor at byte 2.
    send_mouse(
        &mut app,
        MouseEventKind::Down(MouseButton::Left),
        layout.filter_row.x + 2 + 2,
        layout.filter_row.y,
        &tx,
    );

    assert_eq!(
        app.ai_providers.wizard.filter.cursor, 2,
        "the click column minus the prefix maps to the cursor position"
    );
    assert_eq!(
        app.ai_providers.wizard.step,
        AccountWizardStep::Provider,
        "a filter-row click must not select anything"
    );
}

#[test]
fn ai_providers_wizard_click_outside_popup_is_noop() {
    let (tx, _rx) = crossbeam_channel::unbounded();
    let mut app = test_app();
    app.page = Page::AIProviders;
    app.ai_providers.wizard.open();
    app.last_terminal_size = Some((100, 40));

    // (0, 0) is the dimmed area well outside the centered popup.
    send_mouse(&mut app, MouseEventKind::Down(MouseButton::Left), 0, 0, &tx);

    assert!(app.ai_providers.wizard.is_open());
    assert_eq!(app.ai_providers.wizard.step, AccountWizardStep::Provider);
    assert_eq!(app.ai_providers.wizard.focused, 0);
}

#[test]
fn ai_providers_wizard_click_footer_is_noop() {
    let (tx, _rx) = crossbeam_channel::unbounded();
    let mut app = test_app();
    app.page = Page::AIProviders;
    app.ai_providers.wizard.open();
    app.last_terminal_size = Some((100, 40));

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

    assert_eq!(app.ai_providers.wizard.step, AccountWizardStep::Provider);
    assert_eq!(app.ai_providers.wizard.focused, 0);
}

#[test]
fn ai_providers_wizard_click_below_visible_tail_is_noop() {
    let (tx, _rx) = crossbeam_channel::unbounded();
    let mut app = test_app();
    app.page = Page::AIProviders;
    app.ai_providers.wizard.open();
    app.last_terminal_size = Some((100, 40));

    // Filter to exactly one provider, so the list's visible tail is a single
    // row and the body extends past it.
    for c in "friendli".chars() {
        handle_terminal_event(
            Event::Key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE)),
            &mut app,
            &tx,
        )
        .expect("filter char");
    }
    assert_eq!(app.ai_providers.wizard.filtered(&app.providers).len(), 1);

    let layout = selector_list_layout(Rect {
        x: 0,
        y: 0,
        width: 100,
        height: 40,
    });
    // Click the third body row — past the single visible row's tail.
    send_mouse(
        &mut app,
        MouseEventKind::Down(MouseButton::Left),
        layout.body.x + 3,
        layout.body.y + 2,
        &tx,
    );

    assert_eq!(
        app.ai_providers.wizard.step,
        AccountWizardStep::Provider,
        "below-tail click must not pick anything"
    );
    assert_eq!(app.ai_providers.wizard.focused, 0);
}
