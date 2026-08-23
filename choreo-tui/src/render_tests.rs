use super::render::*;
use choreo_proto::{SessionStatus, SessionSummary, TokenUsage};

// ── format_status tests ──

#[test]
fn format_status_retrying() {
    let status = SessionStatus::Retrying {
        attempt: 2,
        max_attempts: 5,
        delay_ms: 3000,
    };
    assert_eq!(format_status(&status), "retrying (2/5, 3s)");
}

#[test]
fn format_status_retrying_first_attempt() {
    let status = SessionStatus::Retrying {
        attempt: 1,
        max_attempts: 3,
        delay_ms: 1500,
    };
    assert_eq!(format_status(&status), "retrying (1/3, 1s 500ms)");
}

#[test]
fn format_status_retrying_second() {
    let status = SessionStatus::Retrying {
        attempt: 2,
        max_attempts: 3,
        delay_ms: 2000,
    };
    assert_eq!(format_status(&status), "retrying (2/3, 2s)");
}

// ── Cumulative token readouts (status bar + session detail) ──

#[test]
fn status_token_readout_small_counts_pass_through() {
    // Below the 1_000 compact threshold humfmt renders verbatim, so small
    // sessions read exactly like the raw counts used to.
    let usage = TokenUsage {
        input_tokens: 847,
        output_tokens: 23,
        total_tokens: 870,
    };
    assert_eq!(status_token_readout(&usage), "↑847 ↓23");
}

#[test]
fn status_token_readout_compacts_large_counts() {
    // Above the threshold humfmt compacts with K/M suffixes, matching the
    // context-fill readout beside it (e.g. "1.5K / 32K").  Values chosen
    // from humfmt's documented outputs: 15_320 -> "15.3K", 1_280 -> "1.3K"
    // (HalfUp at precision 1, trailing zero trimmed).
    let usage = TokenUsage {
        input_tokens: 15_320,
        output_tokens: 1_280,
        total_tokens: 16_600,
    };
    assert_eq!(status_token_readout(&usage), "↑15.3K ↓1.3K");
}

#[test]
fn status_token_readout_zero() {
    assert_eq!(status_token_readout(&TokenUsage::default()), "↑0 ↓0");
}

#[test]
fn session_detail_tokens_line_compacts_and_keeps_label_alignment() {
    let usage = TokenUsage {
        input_tokens: 15_320,
        output_tokens: 1_280,
        total_tokens: 16_600,
    };
    assert_eq!(
        session_detail_tokens_line(&usage),
        "Tokens:        15.3K in / 1.3K out (16.6K total)"
    );
}

#[test]
fn session_detail_tokens_line_small_counts_pass_through() {
    let usage = TokenUsage {
        input_tokens: 42,
        output_tokens: 7,
        total_tokens: 49,
    };
    assert_eq!(
        session_detail_tokens_line(&usage),
        "Tokens:        42 in / 7 out (49 total)"
    );
}

// ── Model selector popup rendering ──

#[test]
fn render_model_selector_shows_title_filter_and_models() {
    use crate::test_util::test_app;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    let mut app = test_app();
    app.model_selector.open();
    app.model_selector.apply_models(
        vec![
            "gpt-4o".to_string(),
            "gpt-4o-mini".to_string(),
            "claude-3".to_string(),
        ],
        Some("gpt-4o-mini".to_string()),
    );

    let backend = TestBackend::new(80, 24);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal
        .draw(|frame| render(frame, &mut app))
        .expect("render with model selector open");

    let content: String = terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(|c| c.symbol())
        .collect();
    assert!(content.contains("Select Model"), "popup title is drawn");
    assert!(content.contains("gpt-4o"), "all models are listed");
    assert!(content.contains("gpt-4o-mini"));
    assert!(content.contains("claude-3"));
    assert!(content.contains("esc close"), "footer hint is drawn");
}

#[test]
fn render_model_selector_loading_and_error_states() {
    use crate::test_util::test_app;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    // Loading state (selector just opened, reply not yet arrived).
    let mut app = test_app();
    app.model_selector.open();
    let backend = TestBackend::new(80, 24);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal
        .draw(|frame| render(frame, &mut app))
        .expect("render loading state");
    let content: String = terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(|c| c.symbol())
        .collect();
    assert!(content.contains("Loading models"), "loading row is drawn");

    // Error state (the daemon rejected the model list request).
    let mut app = test_app();
    app.model_selector.open();
    app.model_selector.apply_error("no credential".to_string());
    let backend = TestBackend::new(80, 24);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal
        .draw(|frame| render(frame, &mut app))
        .expect("render error state");
    let content: String = terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(|c| c.symbol())
        .collect();
    assert!(content.contains("no credential"), "error row is drawn");
}

// ── Account wizard + credential modal rendering ─────────────────────────

#[test]
fn render_wizard_provider_shows_filter_and_providers() {
    use crate::test_util::test_app;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    let mut app = test_app();
    app.page = crate::state::Page::AIProviders;
    app.ai_providers.wizard.open();

    let backend = TestBackend::new(80, 24);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal
        .draw(|frame| render(frame, &mut app))
        .expect("render wizard provider picker");

    let content: String = terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(|c| c.symbol())
        .collect();
    assert!(content.contains("Select Provider"), "popup title is drawn");
    // The picker is alphabetical by display name, so the first provider is
    // shown.  The canonical slug must NOT appear (it is easily confused with
    // the account slug entered in step 2).
    assert!(
        content.contains(&app.providers[0].display_name),
        "provider display names are listed"
    );
    assert!(
        !content.contains(&app.providers[0].slug),
        "the provider slug is deliberately not shown"
    );
    assert!(content.contains("type to filter"), "footer hint is drawn");

    // A filter with no matches renders the empty state (not a panic).
    app.ai_providers.wizard.filter.text = "zzzz-no-such-provider".to_string();
    terminal
        .draw(|frame| render(frame, &mut app))
        .expect("render empty filter state");
    let content: String = terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(|c| c.symbol())
        .collect();
    assert!(
        content.contains("No providers match the filter."),
        "empty filter message is drawn"
    );
}

#[test]
fn render_wizard_slug_shows_picked_provider_and_slug_prompt() {
    use crate::state::AccountWizardStep;
    use crate::test_util::test_app;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    let mut app = test_app();
    app.page = crate::state::Page::AIProviders;
    app.ai_providers.wizard.open();
    // Drive to step 2 with a picked provider (mirrors confirm_provider).
    app.ai_providers.wizard.picked_slug = Some("openai".to_string());
    app.ai_providers.wizard.picked_name = Some("OpenAI".to_string());
    app.ai_providers.wizard.step = AccountWizardStep::Slug;

    let backend = TestBackend::new(80, 24);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal
        .draw(|frame| render(frame, &mut app))
        .expect("render wizard slug modal");

    let content: String = terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(|c| c.symbol())
        .collect();
    assert!(content.contains("Add Account"), "popup title is drawn");
    assert!(
        content.contains("OpenAI"),
        "picked provider is shown for context"
    );
    assert!(content.contains("Slug:"), "slug prompt is drawn");
    assert!(content.contains("create account"), "footer hint is drawn");
}

#[test]
fn render_credential_modal_shows_title_and_masked_key() {
    use crate::test_util::test_app;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    let mut app = test_app();
    app.page = crate::state::Page::AIProviders;
    app.ai_providers.credential.open("my-account".to_string());
    app.ai_providers.credential.input.text = "sk-abcdefghijklmnop".to_string();
    app.ai_providers.credential.input.cursor = app.ai_providers.credential.input.text.len();

    let backend = TestBackend::new(80, 24);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal
        .draw(|frame| render(frame, &mut app))
        .expect("render credential modal");

    let content: String = terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(|c| c.symbol())
        .collect();
    assert!(
        content.contains("API Key for \"my-account\""),
        "popup title names the account"
    );
    // The key is masked: the full plaintext never reaches the buffer.
    assert!(
        !content.contains("sk-abcdefghijklmnop"),
        "the plaintext API key is never drawn"
    );
    assert!(content.contains("save"), "footer hint is drawn");
}

// ── Session list table ────────────────────────────────────────────────

#[test]
fn render_session_list_shows_ids_parents_and_titles() {
    use crate::test_util::test_app;
    use choreo_proto::SessionStatus;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

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
            created_at: 1705314000000,
            last_modified: 1705314000000,
            turn_count: 3,
            status: SessionStatus::Inactive,
            active_tool_groups: vec![],
            account_name: None,
            token_usage: None,
            context_window: None,
            last_prompt_tokens: None,
        },
        SessionSummary {
            session_id: 9002,
            title: Some("beta".into()),
            selected_model: None,
            reasoning_effort: None,
            parent_session_id: Some(9001),
            working_dir: None,
            created_at: 1705314000000,
            last_modified: 1705314000001,
            turn_count: 12,
            status: SessionStatus::Inference,
            active_tool_groups: vec![],
            account_name: None,
            token_usage: None,
            context_window: None,
            last_prompt_tokens: None,
        },
    ]);

    let backend = TestBackend::new(80, 24);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal
        .draw(|frame| render(frame, &mut app))
        .expect("render session list");
    let content: String = terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(|c| c.symbol())
        .collect();

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
        content.contains("inferring"),
        "status text from status_display"
    );

    // The most recently modified session sorts to the top (earlier row = lower
    // flat-buffer offset), with session 9002's parent column pointing at 9001.
    let beta_at = content.find("beta").expect("beta rendered");
    let alpha_at = content.find("alpha").expect("alpha rendered");
    assert!(beta_at < alpha_at, "newest session appears first");
}

// ── Selected-row highlight spans the full content width ────────────────

#[test]
fn session_list_selected_row_highlight_is_solid_across_width() {
    use crate::test_util::test_app;
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
            created_at: 1705314000000 + i as i64,
            last_modified: 1705314000000 + i as i64,
            turn_count: i as u32,
            status: SessionStatus::Inactive,
            active_tool_groups: vec![],
            account_name: None,
            token_usage: None,
            context_window: None,
            last_prompt_tokens: None,
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

#[test]
fn session_list_scrolls_to_keep_selection_visible() {
    use crate::test_util::test_app;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    // 30 sessions on a 100x24 terminal: the bordered list block leaves
    // 21 content rows, one of which is the table header, so 20 session
    // rows fit.  With the selection on row 25 the window must start at
    // 6 — the top six rows scroll off and the highlighted row stays
    // pinned to the last visible row.  All timestamps are equal so the
    // stable sort in `set_sessions` keeps the ids in input order
    // (index i = session id i+1).
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
            created_at: 1705314000000,
            last_modified: 1705314000000,
            turn_count: i as u32,
            status: SessionStatus::Inactive,
            active_tool_groups: vec![],
            account_name: None,
            token_usage: None,
            context_window: None,
            last_prompt_tokens: None,
        })
        .collect();
    app.session_mgr.set_sessions(sessions);
    app.session_mgr.selection = Some(25);

    let backend = TestBackend::new(100, 24);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal
        .draw(|frame| render(frame, &mut app))
        .expect("render scrolled list");
    let content: String = terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(|c| c.symbol())
        .collect();

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

#[test]
fn session_list_scrolls_down_then_up_directionally() {
    use crate::connection::handle_terminal_event;
    use crate::test_util::test_app;
    use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

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
            created_at: 1705314000000,
            last_modified: 1705314000000,
            turn_count: i as u32,
            status: SessionStatus::Inactive,
            active_tool_groups: vec![],
            account_name: None,
            token_usage: None,
            context_window: None,
            last_prompt_tokens: None,
        })
        .collect();
    app.session_mgr.set_sessions(sessions);
    // A 100x24 terminal fits 20 session rows below the table header (see
    // render_session_list_view); cache the height the way the event loop's
    // update_viewport_from_terminal_size does.
    app.session_mgr.viewport_height = 20;

    let (tx, _rx) = std::sync::mpsc::channel();
    let mut terminal = Terminal::new(TestBackend::new(100, 24)).unwrap();
    let key = |code| Event::Key(KeyEvent::new(code, KeyModifiers::NONE));

    // Scroll to the bottom: 29 × j.  Window rows 10..29 (sessions 11..30).
    for _ in 0..29 {
        handle_terminal_event(key(KeyCode::Char('j')), &mut app, &tx).expect("j");
    }
    terminal
        .draw(|frame| render(frame, &mut app))
        .expect("render");
    let content: String = terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(|c| c.symbol())
        .collect();
    assert!(content.contains("session 11 "), "scrolled past the fold");
    assert!(content.contains("session 30 "), "last session visible");
    assert!(!content.contains("session 1 "), "top row scrolled off");

    // Press up 19 times: the selection climbs 29 → 10 (the top edge of the
    // window), but the window must NOT scroll back — the bottom of the list
    // stays on screen.
    for _ in 0..19 {
        handle_terminal_event(key(KeyCode::Char('k')), &mut app, &tx).expect("k");
    }
    terminal
        .draw(|frame| render(frame, &mut app))
        .expect("render");
    let content: String = terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(|c| c.symbol())
        .collect();
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
    terminal
        .draw(|frame| render(frame, &mut app))
        .expect("render");
    let content: String = terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(|c| c.symbol())
        .collect();
    assert!(
        content.contains("session 10 "),
        "window scrolled up one row"
    );
    assert!(
        !content.contains("session 30 "),
        "bottom row scrolled off after window shift"
    );
}

// ── format_timestamp ──────────────────────────────────────────────────

#[test]
fn format_timestamp_zero_or_negative_is_dash() {
    assert_eq!(format_timestamp(0), "-");
    assert_eq!(format_timestamp(-5), "-");
}

#[test]
fn format_timestamp_today_shows_time_only() {
    use chrono::Local;
    let now = Local::now();
    let s = format_timestamp(now.timestamp_millis());
    assert!(
        s.len() == 5 && s.contains(':'),
        "today renders as HH:MM, got {s}"
    );
}

#[test]
fn format_timestamp_same_year_shows_month_and_day() {
    use chrono::{Datelike, Local, TimeZone};
    let now = Local::now();
    // A fixed mid-year date is always in the current calendar year; if it
    // happens to be today, nudge to a neighbouring date that still can't be
    // today and is guaranteed to stay in the same year.
    let mut dt = Local
        .with_ymd_and_hms(now.year(), 6, 15, 12, 0, 0)
        .single()
        .expect("valid date");
    if dt.date_naive() == now.date_naive() {
        dt = Local
            .with_ymd_and_hms(now.year(), 6, 16, 12, 0, 0)
            .single()
            .expect("valid date");
    }
    let s = format_timestamp(dt.timestamp_millis());
    assert_eq!(
        s,
        dt.format("%b %d").to_string(),
        "same-year date shows month+day"
    );
    assert!(!s.contains(':'), "no time for non-today same-year dates");
}

#[test]
fn format_timestamp_older_year_shows_full_date() {
    use chrono::{Datelike, Local, TimeZone};
    let year = Local::now().year() - 2;
    let dt = Local
        .with_ymd_and_hms(year, 6, 15, 12, 0, 0)
        .single()
        .expect("valid date");
    let s = format_timestamp(dt.timestamp_millis());
    assert_eq!(
        s,
        dt.format("%b %d %Y").to_string(),
        "older dates include the year"
    );
}
