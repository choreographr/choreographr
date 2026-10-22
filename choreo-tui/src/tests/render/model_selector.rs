use super::drawn_cells;

// ── Model selector popup rendering ──

#[test]
fn render_model_selector_shows_title_filter_and_models() {
    use crate::tests::support::test_app;

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
    use crate::tests::support::test_app;

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
