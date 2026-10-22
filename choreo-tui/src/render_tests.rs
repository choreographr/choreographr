use super::render::*;
use choreo_proto::{SessionStatus, SessionSummary, TokenUsage};
use ratatui::buffer::Cell;

/// Draw one whole frame through `render` into a `TestBackend` and return the
/// drawn cells as a flat string.
///
/// A test reads back the cells the backend actually holds, so the assertion is
/// on what reaches the screen rather than on what a widget was asked to draw.
/// The returned string preserves the buffer's row-major order, so `contains`
/// and `find` splice it exactly as a reader sees the frame; a test that needs
/// cell *styling* rather than symbols (a background-colour span) reads the
/// buffer directly instead.
fn drawn_cells(app: &mut crate::state::App, w: u16, h: u16) -> String {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    let mut terminal = Terminal::new(TestBackend::new(w, h)).unwrap();
    terminal
        .draw(|frame| render(frame, app))
        .expect("render a frame");
    terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(Cell::symbol)
        .collect()
}

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
        cached_tokens: 0,
        cache_write_tokens: 0,
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
        cached_tokens: 0,
        cache_write_tokens: 0,
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
        cached_tokens: 0,
        cache_write_tokens: 0,
    };
    assert_eq!(
        session_detail_tokens_line(&usage),
        "Tokens:        15.3K in / 1.3K out (16.6K total)"
    );
}

#[test]
fn session_detail_tokens_line_annotates_cached_tokens_when_reported() {
    // z.ai reports cached prompt tokens; the detail line annotates them as a
    // subset of the input count. Zero cached stays silent (see the test above).
    let usage = TokenUsage {
        input_tokens: 15_320,
        output_tokens: 1_280,
        total_tokens: 16_600,
        cached_tokens: 10_000,
        cache_write_tokens: 0,
    };
    assert_eq!(
        session_detail_tokens_line(&usage),
        "Tokens:        15.3K in / 1.3K out (16.6K total) (10K cached)"
    );
}

#[test]
fn session_detail_tokens_line_small_counts_pass_through() {
    let usage = TokenUsage {
        input_tokens: 42,
        output_tokens: 7,
        total_tokens: 49,
        cached_tokens: 0,
        cache_write_tokens: 0,
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

    let mut app = test_app();
    app.model_selector.open();
    app.model_selector.apply_models(
        vec![
            "gpt-4o".to_string(),
            "gpt-4o-mini".to_string(),
            "claude-3".to_string(),
        ],
        Some("gpt-4o-mini".to_string()),
        false,
    );

    let content = drawn_cells(&mut app, 80, 24);
    assert!(content.contains("Select Model"), "popup title is drawn");
    assert!(content.contains("gpt-4o"), "all models are listed");
    assert!(content.contains("gpt-4o-mini"));
    assert!(content.contains("claude-3"));
    assert!(content.contains("esc close"), "footer hint is drawn");
}

#[test]
fn render_model_selector_loading_and_error_states() {
    use crate::test_util::test_app;

    // Loading state (selector just opened, reply not yet arrived).
    let mut app = test_app();
    app.model_selector.open();
    let content = drawn_cells(&mut app, 80, 24);
    assert!(content.contains("Loading models"), "loading row is drawn");

    // Error state (the daemon rejected the model list request).
    let mut app = test_app();
    app.model_selector.open();
    app.model_selector.apply_error("no credential".to_string());
    let content = drawn_cells(&mut app, 80, 24);
    assert!(content.contains("no credential"), "error row is drawn");
}

// ── Account wizard + credential modal rendering ─────────────────────────

#[test]
fn render_wizard_provider_shows_filter_and_providers() {
    use crate::test_util::test_app;

    let mut app = test_app();
    app.page = crate::state::Page::AIProviders;
    app.ai_providers.wizard.open();

    let content = drawn_cells(&mut app, 80, 24);
    assert!(content.contains("Select Provider"), "popup title is drawn");
    // The picker is alphabetical by display name, so the first provider is
    // shown.  The canonical slug must NOT appear (it is easily confused with
    // the account slug entered in step 2).
    assert!(
        content.contains(&app.providers[0].display_name),
        "provider display names are listed"
    );
    // The canonical slug must not appear as a picker row.  A bare substring
    // check would be fragile: some display name could legitimately contain
    // another provider's slug verbatim, so match whole trimmed rows instead.
    assert!(
        !content.lines().any(|line| {
            let row = line.trim().trim_start_matches(['>', ' ']);
            row == app.providers[0].slug
        }),
        "the provider slug is deliberately not shown"
    );
    assert!(content.contains("type to filter"), "footer hint is drawn");

    // A filter with no matches renders the empty state (not a panic).
    app.ai_providers.wizard.filter.text = "zzzz-no-such-provider".to_string();
    let content = drawn_cells(&mut app, 80, 24);
    assert!(
        content.contains("No providers match the filter."),
        "empty filter message is drawn"
    );
}

#[test]
fn render_wizard_slug_shows_picked_provider_and_slug_prompt() {
    use crate::state::AccountWizardStep;
    use crate::test_util::test_app;

    let mut app = test_app();
    app.page = crate::state::Page::AIProviders;
    app.ai_providers.wizard.open();
    // Drive to step 2 with a picked provider (mirrors confirm_provider).
    app.ai_providers.wizard.picked_slug = Some("openai".to_string());
    app.ai_providers.wizard.picked_name = Some("OpenAI".to_string());
    app.ai_providers.wizard.step = AccountWizardStep::Slug;

    let content = drawn_cells(&mut app, 80, 24);
    assert!(content.contains("Add Account"), "popup title is drawn");
    assert!(
        content.contains("OpenAI"),
        "picked provider is shown for context"
    );
    assert!(
        content.contains("Account name:"),
        "account-name prompt is drawn"
    );
    assert!(content.contains("create account"), "footer hint is drawn");
}

#[test]
fn render_credential_modal_shows_title_and_masked_key() {
    use crate::test_util::test_app;

    let mut app = test_app();
    app.page = crate::state::Page::AIProviders;
    app.ai_providers.credential.open("my-account".to_string());
    app.ai_providers.credential.input.text = "sk-abcdefghijklmnop".to_string();
    app.ai_providers.credential.input.cursor = app.ai_providers.credential.input.text.len();

    let content = drawn_cells(&mut app, 80, 24);
    assert!(
        content.contains("API Key for \"my-account\""),
        "popup title names the account"
    );
    // The key is masked: neither the full plaintext nor the first-4/last-4
    // slices a proportional mask used to expose reach the buffer.
    assert!(
        !content.contains("sk-abcdefghijklmnop"),
        "the plaintext API key is never drawn"
    );
    assert!(
        !content.contains("sk-ab") && !content.contains("mnop"),
        "no 4-char prefix/suffix of the key leaks"
    );
    assert!(content.contains("save"), "footer hint is drawn");
}

#[test]
fn render_credential_modal_short_key_mid_cursor_does_not_panic() {
    use crate::test_util::test_app;

    let mut app = test_app();
    app.page = crate::state::Page::AIProviders;
    app.ai_providers.credential.open("my-account".to_string());
    // Bug 1 regression: a short ASCII key with the cursor mid-string used to
    // panic because the proportional byte-offset cursor landed inside a
    // multi-byte bullet when slicing the mask.  (2 is a byte offset into
    // "abcde" — safely between chars, but proportional math placed the cursor
    // on a bullet boundary.)
    app.ai_providers.credential.input.text = "abcde".to_string();
    app.ai_providers.credential.input.cursor = 2;

    let content = drawn_cells(&mut app, 80, 24);
    assert!(
        content.contains("API Key for \"my-account\""),
        "popup title names the account"
    );
    // No plaintext leaks, not even a 4-char prefix/suffix slice.
    assert!(!content.contains("abcde"), "plaintext key is never drawn");
    assert!(
        !content.contains("abcd") && !content.contains("bcde"),
        "no 4-char prefix/suffix of the short key leaks"
    );
}

// ── Session list table ────────────────────────────────────────────────

#[test]
fn render_session_list_shows_ids_parents_and_titles() {
    use crate::test_util::test_app;
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
    use crate::test_util::test_app;

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
    use crate::test_util::test_app;
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
    use crate::test_util::{make_session, test_app};

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
    use crate::test_util::{make_session, test_app};

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
    use crate::test_util::{make_session, test_app};

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

// ── Inline command palette rendering ───────────────────────────────────

#[test]
fn command_palette_lists_commands_for_a_command_line() {
    use crate::test_util::test_app;

    let mut app = test_app();
    app.input.text = "/".to_string();
    assert!(
        app.command_palette_active(),
        "a `/`-leading line shows the palette"
    );

    let content = drawn_cells(&mut app, 80, 24);
    // The palette lists the catalog alphabetically and windows to the first
    // rows on an empty line, so the leading commands are drawn (the tail is
    // summarized with a `+N more` row).
    assert!(content.contains("/account"), "the leading row is drawn");
    assert!(content.contains("/acl"), "alphabetical order is preserved");
    assert!(content.contains("more"), "the hidden tail is summarized");
    assert!(
        content.contains("command"),
        "the command-mode affordance (input-box title) is drawn"
    );
}

#[test]
fn command_palette_narrows_to_model_for_prefix_query() {
    use crate::test_util::test_app;

    let mut app = test_app();
    app.input.text = "/mo".to_string();

    let content = drawn_cells(&mut app, 80, 24);
    assert!(content.contains("/model"), "the matched command is drawn");
    assert!(
        !content.contains("/session"),
        "non-matching commands are hidden"
    );
}

#[test]
fn command_palette_draws_nothing_for_a_plain_prompt() {
    use crate::test_util::test_app;

    let mut app = test_app();
    app.input.text = "hi".to_string();
    assert!(!app.command_palette_active(), "plain text opens nothing");

    let content = drawn_cells(&mut app, 80, 24);
    assert!(!content.contains("/model"), "no palette row is drawn");
    assert!(!content.contains("/session"), "no palette row is drawn");
}

// ── Untrusted text is normalized at every widget sink ──────────────────

#[test]
fn status_bar_renders_daemon_errors_inert() {
    // `app.error` carries daemon-authored text (a provider's rejected request
    // body, say).  A control character must not reach a cell, and an escape
    // sequence must not ride along; the tab expands to spaces.
    use crate::test_util::test_app;

    let mut app = test_app();
    app.set_error("bad request\u{7}\tbody\x1b[31m".to_string());
    let content = drawn_cells(&mut app, 80, 24);
    assert!(
        !content.chars().any(char::is_control),
        "no control char may reach a cell: {content:?}"
    );
    assert!(content.contains("bad request"), "the message is shown");
    assert!(
        content.contains("\\u{7}") && content.contains("\\u{1b}[31m"),
        "hostile bytes render as inert text: {content:?}"
    );
}

#[test]
fn status_error_height_measures_the_stored_text_as_drawn() {
    // The reserved height is measured from the STORED text, which ingest already
    // normalized — the same bytes the renderer draws — so the two cannot
    // disagree about how many rows a taboo-laden message occupies.
    use crate::test_util::test_app;

    let mut app = test_app();
    app.set_error("aaaaaaaaaa\tbbbbbbbbbb\tcccc".to_string());
    let width = 24u16;
    let height = app.status_error_height(width);
    let text = app.error().expect("error stored");
    let drawn_rows = crate::markdown_render::plain_text_lines(text, width - 2).len();
    assert_eq!(
        usize::from(height),
        drawn_rows,
        "height must match the drawn rows for {text:?}"
    );
}

#[test]
fn session_manager_cells_render_untrusted_titles_inert() {
    // Session titles are model-chosen and model ids come from the catalog;
    // neither may carry a control character into a fixed-width column.
    use crate::test_util::test_app;

    let mut app = test_app();
    app.page = crate::state::Page::SessionManager;
    app.session_mgr.set_sessions(vec![SessionSummary {
        session_id: 1,
        title: Some("evil\u{202e}title\tx".to_string()),
        selected_model: Some("model\u{7}".to_string()),
        reasoning_effort: None,
        parent_session_id: None,
        working_dir: None,
        created_at: 1_705_314_000_000,
        last_modified: 1_705_314_000_000,
        turn_count: 0,
        status: SessionStatus::Inactive,
        active_tool_groups: vec![],
        account_name: None,
        token_usage: None,
        context_window: None,
        last_prompt_tokens: None,
        pinned: false,
        archived_at: None,
    }]);

    let content = drawn_cells(&mut app, 100, 24);
    assert!(
        !content.chars().any(|c| c.is_control() || c == '\u{202e}'),
        "no live control or bidi char may reach a cell: {content:?}"
    );
    assert!(
        content.contains("\\u{202e}") && content.contains("model\\u{7}"),
        "untrusted cell text renders inert: {content:?}"
    );
}

#[test]
fn login_modal_renders_untrusted_fields_inert() {
    // The authorization server supplies the status line, the authorize URL and
    // the device code, and the credential supplies the account name.  The three
    // display-only fields are normalized at INGEST (where `apply_login_event`
    // takes them from the event) while `provider`/`account` stay raw, so this
    // asserts both halves: the modal holds inert text, the drawn content is
    // inert end to end, and the identity fields still carry their exact bytes.
    use crate::state::LoginModal;
    use crate::test_util::test_app;
    use choreo_client_core::LoginEvent;

    let mut app = test_app();
    app.login = Some(LoginModal::test_modal("openai\u{7}", "work\tacct"));
    let (tx, _rx) = crossbeam_channel::unbounded();
    app.apply_login_event(
        LoginEvent::AuthUrl {
            url: "https://x/\u{202e}evil".to_string(),
        },
        &tx,
    );
    app.apply_login_event(
        LoginEvent::DeviceCode {
            code: "AB\u{7}CD".to_string(),
            uri: "https://x/dc\tx".to_string(),
        },
        &tx,
    );
    app.apply_login_event(
        LoginEvent::Prompt {
            message: "waiting\x1b[2J".to_string(),
        },
        &tx,
    );

    let modal = app.login.as_ref().expect("the flow is still running");
    let mut display_only = vec![
        modal.status.as_str(),
        modal.auth_url.as_deref().unwrap_or(""),
    ];
    if let Some((code, uri)) = modal.device_code.as_ref() {
        display_only.push(code.as_str());
        display_only.push(uri.as_str());
    }
    for text in display_only {
        assert!(
            !text.chars().any(char::is_control)
                && !text.contains('\u{202e}')
                && !text.contains('\t'),
            "display-only login fields are stored inert: {text:?}"
        );
    }
    // Deliberately NOT normalized: the account is the credential key the modal
    // hands back, and the provider selects the auth descriptor.
    assert_eq!(modal.provider, "openai\u{7}");
    assert_eq!(modal.account, "work\tacct");

    let content = drawn_cells(&mut app, 100, 30);
    assert!(
        !content.chars().any(|c| c.is_control() || c == '\u{202e}'),
        "no live control or bidi char may reach a cell: {content:?}"
    );
    for inert in ["openai\\u{7}", "waiting\\u{1b}[2J", "AB\\u{7}CD"] {
        assert!(
            content.contains(inert),
            "{inert:?} should render inert: {content:?}"
        );
    }
}

#[test]
fn status_display_renders_a_hostile_tool_name_inert() {
    // The status bar and the session table both show the session status, and a
    // tool name there is MCP-supplied arbitrary text.
    let (label, _color) = status_display(&SessionStatus::ToolCall("evil\u{7}tool".to_string()));
    assert_eq!(label, "tool call: evil\\u{7}tool");
    assert!(!label.chars().any(char::is_control), "{label:?}");
}

#[test]
fn status_bar_identity_fields_render_inert() {
    // Every value in the status bar's identity block is externally supplied —
    // the working directory from the filesystem, the account slug from the
    // keystore, the model id from the catalog, the reasoning effort and tool
    // groups echoed back by the daemon.  None may carry a live control or bidi
    // character into the single clipped line.
    use crate::test_util::test_app;

    let mut app = test_app();
    // The identity block draws only for an attached session.
    app.attached_session_id = Some(0);
    app.attached_account_slug = Some("acct\u{7}".to_string());
    app.attached_tool_groups = vec!["core\u{202e}".to_string(), "browser".to_string()];
    let display = app.display_for(0);
    display.working_dir = Some("/tmp/\u{7}evil".to_string());
    display.selected_model = Some("gpt\u{202e}4o".to_string());
    display.reasoning_effort = Some("hi\tgh".to_string());

    let content = drawn_cells(&mut app, 200, 24);
    assert!(
        !content.chars().any(|c| c.is_control() || c == '\u{202e}'),
        "no live control or bidi char may reach a cell: {content:?}"
    );
    for inert in [
        "/tmp/\\u{7}evil",
        "acct\\u{7}",
        "gpt\\u{202e}4o",
        // The reasoning tab expands at the 4-column stop instead of dropping.
        "hi  gh",
        "core\\u{202e}, browser",
    ] {
        assert!(
            content.contains(inert),
            "{inert:?} should render inert: {content:?}"
        );
    }
}

#[test]
fn session_manager_detail_renders_tool_groups_inert() {
    // The detail pane prints the session's active tool groups, which the daemon
    // echoes back from the turn — arbitrary text like every other line there.
    use crate::state::{Page, SessionDetailData, SessionManagerView};
    use crate::test_util::test_app;

    let mut app = test_app();
    app.page = Page::SessionManager;
    app.session_mgr.view = SessionManagerView::Detail;
    app.session_mgr.detail_data = Some(SessionDetailData {
        session_id: 1,
        title: "t".to_string(),
        selected_model: "m".to_string(),
        reasoning_effort: None,
        parent_session_id: None,
        working_dir: "/tmp".to_string(),
        created_at: 1_705_314_000_000,
        last_modified: 1_705_314_000_000,
        turn_count: 0,
        status: SessionStatus::Inactive,
        active_tool_groups: vec!["core\u{7}".to_string(), "browser\u{202e}".to_string()],
        account_name: None,
        accumulated_usage: None,
        context_window: None,
        last_prompt_tokens: None,
        pinned: false,
        archived_at: None,
    });

    let content = drawn_cells(&mut app, 100, 24);
    assert!(
        !content.chars().any(|c| c.is_control() || c == '\u{202e}'),
        "no live control or bidi char may reach a cell: {content:?}"
    );
    assert!(
        content.contains("Tool Groups:") && content.contains("core\\u{7}"),
        "the group list renders inert: {content:?}"
    );
    assert!(
        content.contains("browser\\u{202e}"),
        "every group in the list renders inert: {content:?}"
    );
}

#[test]
fn image_placeholder_label_renders_a_hostile_call_id_inert() {
    // A tool-result vision image's placeholder title embeds the model-chosen
    // tool-call id, so the label itself must be inert — asserting on the label
    // builder rather than drawing a whole image block keeps the test off the
    // image-protocol machinery.
    use crate::state::ImageSlot;

    let label = slot_label(&ImageSlot::ToolResult("call\u{7}\u{202e}".to_string()));
    assert_eq!(label, "[call\\u{7}\\u{202e}]");
    assert!(!label.chars().any(char::is_control), "{label:?}");
    // A positional slot carries no untrusted component.
    assert_eq!(slot_label(&ImageSlot::Displayed(2)), "[2]");
}
