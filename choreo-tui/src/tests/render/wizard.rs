use super::drawn_cells;

// ── Account wizard + credential modal rendering ─────────────────────────

#[test]
fn render_wizard_provider_shows_filter_and_providers() {
    use crate::tests::support::test_app;

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
    use crate::tests::support::test_app;

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
    use crate::tests::support::test_app;

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
    use crate::tests::support::test_app;

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
