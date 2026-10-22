use crate::connection::handle_terminal_event;
use crate::state::*;
use crate::tests::support::test_app;
use crossterm::event::Event;
use tui_prompts::State;

#[test]
fn paste_event_inserts_into_credential_modal() {
    let mut app = test_app();
    let (tx, _rx) = crossbeam_channel::unbounded();

    app.page = Page::AIProviders;
    app.ai_providers.credential.open("my-account".to_string());
    handle_terminal_event(Event::Paste("sk-abc123".to_string()), &mut app, &tx)
        .expect("handle paste into credential input");
    assert_eq!(app.ai_providers.credential.input.text, "sk-abc123");
    assert_eq!(app.ai_providers.credential.input.cursor, 9);
}

#[test]
fn paste_event_inserts_into_new_account_slug_field() {
    let mut app = test_app();
    let (tx, _rx) = crossbeam_channel::unbounded();

    app.page = Page::AIProviders;
    app.ai_providers.wizard.open();
    app.ai_providers.wizard.step = AccountWizardStep::Slug;
    app.ai_providers.wizard.slug.focus();
    handle_terminal_event(Event::Paste("my-account".to_string()), &mut app, &tx)
        .expect("handle paste into slug field");
    assert_eq!(app.ai_providers.wizard.slug.value(), "my-account");
    assert_eq!(app.ai_providers.wizard.slug.position(), 10);
}

// fn-level #[allow]: clippy's assert_is_empty rewrite for `assert!(x.is_empty())`
// is worse than the assertion itself; the empty check is deliberate here.
#[expect(clippy::assert_is_empty)]
#[test]
fn paste_event_goes_into_provider_filter() {
    let mut app = test_app();
    let (tx, _rx) = crossbeam_channel::unbounded();

    // Step 1 (provider picker) has a search filter — a paste lands there and
    // re-clamps the highlight against the narrowed list.
    app.page = Page::AIProviders;
    app.ai_providers.wizard.open();
    app.ai_providers.wizard.focused = app.providers.len() - 1;
    handle_terminal_event(Event::Paste("friendli".to_string()), &mut app, &tx)
        .expect("handle paste on provider filter");
    assert_eq!(app.ai_providers.wizard.filter.text, "friendli");
    assert_eq!(
        app.ai_providers.wizard.filtered(&app.providers).len(),
        1,
        "paste narrows the list to the matching provider"
    );
    assert_eq!(
        app.ai_providers.wizard.focused, 0,
        "highlight re-clamped to the sole match"
    );
    assert_eq!(app.ai_providers.wizard.slug.value(), "");
    assert!(app.ai_providers.credential.input.text.is_empty());
}
