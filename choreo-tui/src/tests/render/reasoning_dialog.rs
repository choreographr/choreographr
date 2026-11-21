use super::drawn_cells;

// ── Reasoning-effort dialog rendering ──

#[test]
fn render_reasoning_dialog_shows_the_ladder_and_the_suggestion() {
    use crate::tests::support::test_app;

    let mut app = test_app();
    app.reasoning_dialog.open(
        ["off", "low", "medium", "high"]
            .iter()
            .map(|l| (*l).to_string())
            .collect(),
        Some("medium".to_string()),
    );

    let content = drawn_cells(&mut app, 80, 24);
    assert!(content.contains("Reasoning Effort"), "popup title is drawn");
    for level in ["off", "low", "medium", "high"] {
        assert!(content.contains(level), "{level} is listed");
    }
    assert!(
        content.contains("(suggested)"),
        "the pre-highlighted row is explained"
    );
    assert!(content.contains("enter select"), "footer hint is drawn");
}

#[test]
fn render_reasoning_dialog_is_closed_by_default() {
    use crate::tests::support::test_app;

    // A closed dialog draws nothing at all, so the chat page underneath is
    // never covered by an empty popup.
    let mut app = test_app();
    let content = drawn_cells(&mut app, 80, 24);
    assert!(!content.contains("Reasoning Effort"));
}

#[test]
fn render_reasoning_dialog_survives_an_empty_ladder() {
    use crate::tests::support::test_app;

    // An empty ladder cannot come from the flow (it commits "off" instead of
    // opening the dialog), but the renderer must still not panic on one.
    let mut app = test_app();
    app.reasoning_dialog.open(Vec::new(), None);
    let content = drawn_cells(&mut app, 80, 24);
    assert!(content.contains("Reasoning Effort"));
    assert!(content.contains("No effort levels"));
}

#[test]
fn render_reasoning_dialog_with_an_empty_ladder_draws_no_rows() {
    use crate::tests::support::test_app;

    // The empty-ladder branch is one explanatory line and nothing else: no
    // focus marker, no suggestion marker, and no footer hint advertising a
    // selection there is nothing to select.
    let mut app = test_app();
    app.reasoning_dialog.open(Vec::new(), None);
    let content = drawn_cells(&mut app, 80, 24);
    assert!(
        !content.contains('>'),
        "no row is marked as focused:\n{content}"
    );
    assert!(!content.contains("(suggested)"), "nothing to suggest");
    assert!(
        !content.contains("enter select"),
        "no footer hint for a ladder that is not drawn"
    );
}
