use super::render::*;
use choreo_proto::{SessionStatus, SessionSummary};

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

// ── Session list table ────────────────────────────────────────────────

#[test]
fn render_session_list_is_a_table_sorted_newest_first_without_ids() {
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
            parent_session_id: None,
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

    // Table headers are drawn.
    assert!(content.contains("Status"), "status header drawn");
    assert!(content.contains("Model"), "model header drawn");
    assert!(content.contains("Turns"), "turns header drawn");
    assert!(content.contains("Modified"), "modified header drawn");
    assert!(content.contains("Title"), "title header drawn");

    // Session titles and a status label are present.
    assert!(content.contains("alpha"));
    assert!(content.contains("beta"));
    assert!(
        content.contains("inferring"),
        "status text from status_display"
    );

    // The most recently modified session sorts to the top (earlier row = lower
    // flat-buffer offset), and session ids are not rendered as a column.
    let beta_at = content.find("beta").expect("beta rendered");
    let alpha_at = content.find("alpha").expect("alpha rendered");
    assert!(beta_at < alpha_at, "newest session appears first");
    assert!(!content.contains("9001"), "session_id 9001 not displayed");
    assert!(!content.contains("9002"), "session_id 9002 not displayed");
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
