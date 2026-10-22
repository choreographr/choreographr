use super::drawn_cells;

// ── Inline command palette rendering ───────────────────────────────────

#[test]
fn command_palette_lists_commands_for_a_command_line() {
    use crate::tests::support::test_app;

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
    use crate::tests::support::test_app;

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
    use crate::tests::support::test_app;

    let mut app = test_app();
    app.input.text = "hi".to_string();
    assert!(!app.command_palette_active(), "plain text opens nothing");

    let content = drawn_cells(&mut app, 80, 24);
    assert!(!content.contains("/model"), "no palette row is drawn");
    assert!(!content.contains("/session"), "no palette row is drawn");
}
