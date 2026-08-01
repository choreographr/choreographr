use super::render::*;
use choreo_proto::SessionStatus;

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
