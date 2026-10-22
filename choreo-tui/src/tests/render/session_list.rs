use super::drawn_cells;
use crate::render::*;
use choreo_proto::{SessionStatus, SessionSummary};

// ── Session list table ────────────────────────────────────────────────

#[test]
fn render_session_list_shows_ids_parents_and_titles() {
    use crate::tests::support::test_app;
    use choreo_proto::SessionStatus;

    let mut app = test_app();
    app.page = crate::state::Page::SessionManager;
    app.session_mgr.set_sessions(vec![
        SessionSummary {
            session_id: 9001,
            title: Some("alpha".into()),
            selected_model: Some("gpt-4".into()),
            reasoning_effort: None,
            parent_session_id: None,
            working_dir: None,
            created_at: 1_705_314_000_000,
            last_modified: 1_705_314_000_000,
            turn_count: 3,
            status: SessionStatus::Inactive,
            active_tool_groups: vec![],
            account_name: None,
            token_usage: None,
            context_window: None,
            last_prompt_tokens: None,
            pinned: false,
            archived_at: None,
        },
        SessionSummary {
            session_id: 9002,
            title: Some("beta".into()),
            selected_model: None,
            reasoning_effort: None,
            parent_session_id: Some(9001),
            working_dir: None,
            created_at: 1_705_314_000_000,
            last_modified: 1_705_314_000_001,
            turn_count: 12,
            status: SessionStatus::Responding,
            active_tool_groups: vec![],
            account_name: None,
            token_usage: None,
            context_window: None,
            last_prompt_tokens: None,
            pinned: false,
            archived_at: None,
        },
    ]);

    // 100 columns (not the usual 80) so every fixed column plus the 4-wide
    // marker column leaves room for the full "Title" header — the pushpin
    // pin marker costs one column over the old single-char "P".
    let content = drawn_cells(&mut app, 100, 24);

    // Table headers are drawn, including the new id columns.
    assert!(content.contains("Session"), "session header drawn");
    assert!(content.contains("Parent"), "parent header drawn");
    assert!(content.contains("Status"), "status header drawn");
    assert!(content.contains("Model"), "model header drawn");
    assert!(content.contains("Turns"), "turns header drawn");
    assert!(content.contains("Modified"), "modified header drawn");
    assert!(content.contains("Title"), "title header drawn");

    // Session ids render as a column, and the parent column shows both a
    // top-level dash and a child's parent id.
    assert!(content.contains("9001"), "session_id 9001 displayed");
    assert!(content.contains("9002"), "session_id 9002 displayed");

    // Session titles and a status label are present.
    assert!(content.contains("alpha"));
    assert!(content.contains("beta"));
    assert!(
        content.contains("responding"),
        "status text from status_display"
    );

    // The most recently modified session sorts to the top (earlier row = lower
    // flat-buffer offset), with session 9002's parent column pointing at 9001.
    let beta_at = content.find("beta").expect("beta rendered");
    let alpha_at = content.find("alpha").expect("alpha rendered");
    assert!(beta_at < alpha_at, "newest session appears first");
}

// ── Selected-row highlight spans the full content width ────────────────

// Indices 1..=30 fit every narrower field they are cast into in the session
// fixtures below (closures are separate HIR bodies, so the allow lives on the
// test fn).
#[expect(clippy::cast_possible_wrap, clippy::cast_possible_truncation)]
#[test]
fn session_list_selected_row_highlight_is_solid_across_width() {
    use crate::tests::support::test_app;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::style::Color;

    let mut app = test_app();
    app.page = crate::state::Page::SessionManager;
    // 30 sessions so the scrollbar is rendered; newest first.
    let sessions: Vec<SessionSummary> = (1..=30)
        .map(|i| SessionSummary {
            session_id: i,
            title: Some(format!("session {i} with a fairly long title")),
            selected_model: Some("gpt-4".into()),
            reasoning_effort: None,
            parent_session_id: None,
            working_dir: None,
            created_at: 1_705_314_000_000 + i as i64,
            last_modified: 1_705_314_000_000 + i as i64,
            turn_count: i as u32,
            status: SessionStatus::Inactive,
            active_tool_groups: vec![],
            account_name: None,
            token_usage: None,
            context_window: None,
            last_prompt_tokens: None,
            pinned: false,
            archived_at: None,
        })
        .collect();
    app.session_mgr.set_sessions(sessions);
    // Start with the second row selected, then move the highlight up — the
    // same two-frame sequence the scratch repro used to chase a stale line.
    app.session_mgr.selection = Some(1);

    let backend = TestBackend::new(100, 24);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal
        .draw(|frame| render(frame, &mut app))
        .expect("render 1");

    app.session_mgr.selection = Some(0);
    terminal
        .draw(|frame| render(frame, &mut app))
        .expect("render 2");

    let buf = terminal.backend().buffer();
    // The table renders inside the bordered block: the content area spans
    // columns 1..=97 (column 98 is the scrollbar, 99 the right border).
    let content_left = 1u16;
    let content_right = 97u16;

    // Locate the selected row via the ">" marker in the first column.
    let selected_y = (0..24u16)
        .find(|&y| {
            buf.cell((content_left, y))
                .is_some_and(|c| c.symbol() == ">")
        })
        .expect("selected row marker rendered");

    // The highlight must be solid: every cell in the selected row carries the
    // background colour across the whole content width, with no
    // character-only gaps at column boundaries or trailing space.
    for x in content_left..=content_right {
        let cell = buf.cell((x, selected_y)).expect("cell in selected row");
        assert_eq!(
            cell.bg,
            Color::Blue,
            "selected row background must be solid at column {x}"
        );
    }

    // Rows that are not selected — including the one that was highlighted
    // in the previous frame — must not keep any highlight background.
    for y in 0..24u16 {
        if y == selected_y {
            continue;
        }
        for x in content_left..=content_right {
            let cell = buf.cell((x, y)).expect("cell in row");
            assert_ne!(
                cell.bg,
                Color::Blue,
                "row {y} is not selected but has a highlight background at column {x}"
            );
        }
    }
}

// ── Session list scrolls with selection ──────────────────────────────

// Indices 1..=30 fit u32, so the `turn_count` cast never truncates.
#[expect(clippy::cast_possible_truncation)]
#[test]
fn session_list_scrolls_to_keep_selection_visible() {
    use crate::tests::support::test_app;

    // 30 sessions on a 100x24 terminal: the bordered list block leaves
    // 21 content rows, one of which is the table header, so 20 session
    // rows fit.  With the selection on row 25 the window must start at
    // 6 — the top six rows scroll off and the highlighted row stays
    // pinned to the last visible row.  `last_modified` decreases with the
    // id, so the (pinned, last_modified desc, session_id desc) sort keeps
    // the ids in ascending order (index i = session id i+1).
    let mut app = test_app();
    app.page = crate::state::Page::SessionManager;
    let sessions: Vec<SessionSummary> = (1..=30)
        .map(|i| SessionSummary {
            session_id: i,
            title: Some(format!("session {i} with a fairly long title")),
            selected_model: Some("gpt-4".into()),
            reasoning_effort: None,
            parent_session_id: None,
            working_dir: None,
            created_at: 1_705_314_000_000,
            last_modified: 1_705_314_000_000 + (30 - i).cast_signed(),
            turn_count: i as u32,
            status: SessionStatus::Inactive,
            active_tool_groups: vec![],
            account_name: None,
            token_usage: None,
            context_window: None,
            last_prompt_tokens: None,
            pinned: false,
            archived_at: None,
        })
        .collect();
    app.session_mgr.set_sessions(sessions);
    app.session_mgr.selection = Some(25);

    let content = drawn_cells(&mut app, 100, 24);

    // First visible row is index 6 ("session 7"); the selected row 25
    // ("session 26") must be on screen.  A trailing space disambiguates
    // whole ids ("session 1 " vs "session 11 ").
    assert!(content.contains("session 7 "), "window start scrolled in");
    assert!(
        content.contains("session 26 "),
        "selected row stays on screen"
    );
    // Rows above the window must have scrolled off.
    assert!(
        !content.contains("session 1 ") && !content.contains("session 6 "),
        "top rows scrolled out of view"
    );
}

// ── Session list directional scrolling ──────────────────────────────

// Indices 1..=30 fit u32, so the `turn_count` cast never truncates.
#[expect(clippy::cast_possible_truncation)]
#[test]
fn session_list_scrolls_down_then_up_directionally() {
    use crate::connection::handle_terminal_event;
    use crate::tests::support::test_app;
    use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};

    let mut app = test_app();
    app.page = crate::state::Page::SessionManager;
    let sessions: Vec<SessionSummary> = (1..=30)
        .map(|i| SessionSummary {
            session_id: i,
            title: Some(format!("session {i}")),
            selected_model: Some("gpt-4".into()),
            reasoning_effort: None,
            parent_session_id: None,
            working_dir: None,
            created_at: 1_705_314_000_000,
            // Decreasing with id keeps the sort in ascending-id order
            // (index i = session id i+1).
            last_modified: 1_705_314_000_000 + (30 - i).cast_signed(),
            turn_count: i as u32,
            status: SessionStatus::Inactive,
            active_tool_groups: vec![],
            account_name: None,
            token_usage: None,
            context_window: None,
            last_prompt_tokens: None,
            pinned: false,
            archived_at: None,
        })
        .collect();
    app.session_mgr.set_sessions(sessions);
    // A 100x24 terminal fits 20 session rows below the table header (see
    // render_session_list_view); cache the height the way the event loop's
    // update_viewport_from_terminal_size does.
    app.session_mgr.viewport_height = 20;

    let (tx, _rx) = crossbeam_channel::unbounded();
    let key = |code| Event::Key(KeyEvent::new(code, KeyModifiers::NONE));

    // Scroll to the bottom: 29 × j.  Window rows 10..29 (sessions 11..30).
    for _ in 0..29 {
        handle_terminal_event(key(KeyCode::Char('j')), &mut app, &tx).expect("j");
    }
    let content = drawn_cells(&mut app, 100, 24);
    assert!(content.contains("session 11 "), "scrolled past the fold");
    assert!(content.contains("session 30 "), "last session visible");
    assert!(!content.contains("session 1 "), "top row scrolled off");

    // Press up 19 times: the selection climbs 29 → 10 (the top edge of the
    // window), but the window must NOT scroll back — the bottom of the list
    // stays on screen.
    for _ in 0..19 {
        handle_terminal_event(key(KeyCode::Char('k')), &mut app, &tx).expect("k");
    }
    let content = drawn_cells(&mut app, 100, 24);
    assert!(
        content.contains("session 11 "),
        "window start unchanged while selection climbs"
    );
    assert!(content.contains("session 30 "), "bottom still visible");
    assert!(
        !content.contains("session 1 "),
        "top row still scrolled off"
    );

    // One more up: the selection (now 9) leaves the top edge, so the
    // window finally scrolls up one row (sessions 10..29).
    handle_terminal_event(key(KeyCode::Char('k')), &mut app, &tx).expect("k");
    let content = drawn_cells(&mut app, 100, 24);
    assert!(
        content.contains("session 10 "),
        "window scrolled up one row"
    );
    assert!(
        !content.contains("session 30 "),
        "bottom row scrolled off after window shift"
    );
}

// ── Archived view + pin marker ──────────────────────────────────────

#[test]
fn session_list_archived_view_renders_only_archived() {
    use crate::tests::support::{make_session, test_app};

    let mut app = test_app();
    app.page = crate::state::Page::SessionManager;
    let mut archived = make_session(2, "old-session", "gpt-4", 1);
    archived.archived_at = Some(1_705_314_000_500);
    app.session_mgr
        .set_sessions(vec![make_session(1, "live-session", "gpt-4", 1), archived]);
    // Tab into the archived view.
    app.session_mgr.toggle_view();

    let content = drawn_cells(&mut app, 120, 24);

    assert!(content.contains("Archived"), "archived block title drawn");
    assert!(
        content.contains("old-session"),
        "the archived session is shown"
    );
    assert!(
        !content.contains("live-session"),
        "live sessions are hidden in the archived view"
    );
}

#[test]
fn session_list_archived_view_empty_state_message() {
    use crate::tests::support::{make_session, test_app};

    let mut app = test_app();
    app.page = crate::state::Page::SessionManager;
    app.session_mgr
        .set_sessions(vec![make_session(1, "live", "gpt-4", 1)]);
    app.session_mgr.toggle_view();

    let content = drawn_cells(&mut app, 80, 24);
    assert!(content.contains("No archived sessions."));
}

#[test]
fn session_list_shows_pin_marker() {
    use crate::tests::support::{make_session, test_app};

    let mut app = test_app();
    app.page = crate::state::Page::SessionManager;
    let mut pinned = make_session(1, "pinned-session", "gpt-4", 1);
    pinned.pinned = true;
    app.session_mgr
        .set_sessions(vec![pinned, make_session(2, "plain", "gpt-4", 1)]);
    // Make the pinned top row both selected and attached so its marker column
    // renders the unique "sel + attached + pin" glyph sequence.
    app.session_mgr.selection = Some(0);
    app.attached_session_id = Some(1);

    let content = drawn_cells(&mut app, 80, 24);
    assert!(
        content.contains(">*📌"),
        "the pinned, selected, attached row draws the pushpin pin marker"
    );
}
