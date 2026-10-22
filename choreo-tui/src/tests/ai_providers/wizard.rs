use crate::connection::handle_terminal_event;
use crate::state::*;
use crate::tests::support::test_app;
use choreo_proto::{ClientMessage, ClientMessageType};
use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
use tui_prompts::State;

// ── AI Providers new-account wizard (modal) ────────────────

fn setup_providers_new_account(app: &mut App) {
    app.page = Page::AIProviders;
    app.ai_providers.wizard.open();
}

/// Drive the wizard to step 2 (slug entry) with the given provider picked,
/// returning nothing.  `provider` is matched by slug.
fn advance_to_slug_phase(
    app: &mut App,
    tx: &crossbeam_channel::Sender<ClientMessage>,
    provider: &str,
) {
    setup_providers_new_account(app);
    let idx = app
        .providers
        .iter()
        .position(|p| p.slug == provider)
        .expect("provider in options");
    app.ai_providers.wizard.focused = idx;
    handle_terminal_event(
        Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
        app,
        tx,
    )
    .expect("enter selects provider");
    // The name step opens pre-filled with a suggestion; clear it so each test
    // types its own name from an empty field.
    app.ai_providers.wizard.prefill_slug(String::new());
}

#[test]
fn ai_providers_new_account_name_is_prefilled_from_the_provider() {
    let mut app = test_app();
    let (tx, _rx) = crossbeam_channel::unbounded();
    setup_providers_new_account(&mut app);
    app.ai_providers.wizard.focused = app
        .providers
        .iter()
        .position(|p| p.slug == "openai")
        .expect("openai in options");
    handle_terminal_event(
        Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
        &mut app,
        &tx,
    )
    .expect("enter selects provider");

    assert_eq!(app.ai_providers.wizard.step, AccountWizardStep::Slug);
    assert_eq!(app.ai_providers.wizard.slug.value(), "openai");
    // Typing extends the suggestion (the cursor sits at its end).
    handle_terminal_event(
        Event::Key(KeyEvent::new(KeyCode::Char('2'), KeyModifiers::NONE)),
        &mut app,
        &tx,
    )
    .expect("char 2 on name");
    assert_eq!(app.ai_providers.wizard.slug.value(), "openai2");
}

// fn-level #[allow]: clippy's assert_is_empty rewrite is worse than the
// deliberate `assert!(x.is_empty())` check below.
#[expect(clippy::assert_is_empty)]
#[test]
fn ai_providers_new_account_starts_at_provider_step() {
    let mut app = test_app();
    setup_providers_new_account(&mut app);
    assert!(app.ai_providers.wizard.is_open());
    assert_eq!(app.ai_providers.wizard.step, AccountWizardStep::Provider);
    assert_eq!(app.ai_providers.wizard.focused, 0);
    assert!(app.ai_providers.wizard.filter.text.is_empty());
    assert!(!app.ai_providers.wizard.slug.is_focused());
    assert!(app.ai_providers.wizard.error.get().is_none());
}

#[test]
fn ai_providers_new_account_enter_advances_to_slug() {
    let mut app = test_app();
    let (tx, _rx) = crossbeam_channel::unbounded();
    setup_providers_new_account(&mut app);

    handle_terminal_event(
        Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
        &mut app,
        &tx,
    )
    .expect("enter selects first provider");

    assert_eq!(app.ai_providers.wizard.step, AccountWizardStep::Slug);
    assert!(app.ai_providers.wizard.slug.is_focused());
    assert!(app.ai_providers.wizard.error.get().is_none());
    assert_eq!(
        app.ai_providers.wizard.picked_name.as_deref(),
        Some(app.providers[0].display_name.as_str()),
        "the first (alphabetical) provider is picked"
    );
}

#[test]
fn ai_providers_new_account_jk_types_into_provider_filter() {
    let mut app = test_app();
    let (tx, _rx) = crossbeam_channel::unbounded();
    setup_providers_new_account(&mut app);

    // j/k are NOT navigation keys in the provider picker: they must type into
    // the search filter, or providers whose names contain 'j'/'k' (Kimi,
    // Jiekou.AI, Sakana AI, Amazon Bedrock, …) could never be searched for.
    // Park the highlight at the very bottom first so the filter's re-clamp is
    // observable: after narrowing, focus must land back inside the filtered
    // list.
    app.ai_providers.wizard.focused = app.providers.len() - 1;
    handle_terminal_event(
        Event::Key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE)),
        &mut app,
        &tx,
    )
    .expect("j types into filter");
    assert_eq!(app.ai_providers.wizard.filter.text, "j");
    let expected = app
        .providers
        .iter()
        .filter(|p| p.display_name.to_lowercase().contains('j'))
        .count();
    assert!(expected > 0, "test data must include 'j' providers");
    assert_eq!(
        app.ai_providers.wizard.filtered(&app.providers).len(),
        expected,
        "typing 'j' narrows the list to names containing 'j'"
    );
    assert_eq!(
        app.ai_providers.wizard.focused,
        expected - 1,
        "highlight re-clamped to the bottom of the narrowed list"
    );

    // 'k' appends to the same filter text instead of navigating up.
    handle_terminal_event(
        Event::Key(KeyEvent::new(KeyCode::Char('k'), KeyModifiers::NONE)),
        &mut app,
        &tx,
    )
    .expect("k types into filter");
    assert_eq!(app.ai_providers.wizard.filter.text, "jk");
    assert_eq!(
        app.ai_providers.wizard.filtered(&app.providers).len(),
        app.providers
            .iter()
            .filter(|p| p.display_name.to_lowercase().contains("jk"))
            .count()
    );
    // Focus stayed clamped inside the (possibly empty) narrowed list.
    let narrowed = app.ai_providers.wizard.filtered(&app.providers);
    let max_focus = narrowed.len().saturating_sub(1);
    assert!(
        app.ai_providers.wizard.focused <= max_focus,
        "focus never points past the narrowed list"
    );
    assert_eq!(app.ai_providers.wizard.slug.value(), "");
}

#[expect(clippy::assert_is_empty)] // deliberate empty-check; clippy's rewrite is worse
#[test]
fn ai_providers_new_account_arrows_navigate_provider_list() {
    let mut app = test_app();
    let (tx, _rx) = crossbeam_channel::unbounded();
    setup_providers_new_account(&mut app);

    // Arrow keys are the dedicated navigation in the provider picker (j/k are
    // filter keys, see `ai_providers_new_account_jk_types_into_provider_filter`).
    handle_terminal_event(
        Event::Key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE)),
        &mut app,
        &tx,
    )
    .expect("down");
    assert_eq!(app.ai_providers.wizard.focused, 1);
    handle_terminal_event(
        Event::Key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE)),
        &mut app,
        &tx,
    )
    .expect("down again");
    assert_eq!(app.ai_providers.wizard.focused, 2);
    handle_terminal_event(
        Event::Key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE)),
        &mut app,
        &tx,
    )
    .expect("up");
    assert_eq!(app.ai_providers.wizard.focused, 1);

    // Arrow navigation must not leak into the filter text buffer.
    assert!(app.ai_providers.wizard.filter.text.is_empty());
    assert_eq!(app.ai_providers.wizard.slug.value(), "");
}

#[test]
fn ai_providers_new_account_provider_focus_clamps_at_edges() {
    let mut app = test_app();
    let (tx, _rx) = crossbeam_channel::unbounded();
    setup_providers_new_account(&mut app);

    // Up at the top stays at 0.
    handle_terminal_event(
        Event::Key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE)),
        &mut app,
        &tx,
    )
    .expect("up at top");
    assert_eq!(app.ai_providers.wizard.focused, 0);

    // Jump to the last provider and try to go past it.
    app.ai_providers.wizard.focused = app.providers.len() - 1;
    handle_terminal_event(
        Event::Key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE)),
        &mut app,
        &tx,
    )
    .expect("down at bottom");
    assert_eq!(app.ai_providers.wizard.focused, app.providers.len() - 1);
}

#[expect(clippy::assert_is_empty)] // deliberate empty-check; clippy's rewrite is worse
#[test]
fn ai_providers_new_account_provider_page_keys_move_selection_by_page() {
    let mut app = test_app();
    let (tx, _rx) = crossbeam_channel::unbounded();
    setup_providers_new_account(&mut app);

    // PgDn moves the highlight by a page…
    handle_terminal_event(
        Event::Key(KeyEvent::new(KeyCode::PageDown, KeyModifiers::NONE)),
        &mut app,
        &tx,
    )
    .expect("page down");
    assert_eq!(app.ai_providers.wizard.focused, PROVIDER_PAGE_LINES);

    // …and PgUp moves it back.
    handle_terminal_event(
        Event::Key(KeyEvent::new(KeyCode::PageUp, KeyModifiers::NONE)),
        &mut app,
        &tx,
    )
    .expect("page up");
    assert_eq!(app.ai_providers.wizard.focused, 0);

    // PgUp at the top clamps to 0.
    handle_terminal_event(
        Event::Key(KeyEvent::new(KeyCode::PageUp, KeyModifiers::NONE)),
        &mut app,
        &tx,
    )
    .expect("page up at top");
    assert_eq!(app.ai_providers.wizard.focused, 0);

    // PgDn past the last provider clamps to it.
    app.ai_providers.wizard.focused = app.providers.len() - 1;
    handle_terminal_event(
        Event::Key(KeyEvent::new(KeyCode::PageDown, KeyModifiers::NONE)),
        &mut app,
        &tx,
    )
    .expect("page down at bottom");
    assert_eq!(app.ai_providers.wizard.focused, app.providers.len() - 1);

    // Paging must not leak into the filter or slug buffers.
    assert!(app.ai_providers.wizard.filter.text.is_empty());
    assert_eq!(app.ai_providers.wizard.slug.value(), "");
}

#[test]
fn ai_providers_new_account_provider_window_keeps_selection_visible() {
    let mut app = test_app();
    setup_providers_new_account(&mut app);

    // The render window (pure) always contains the focus, anchoring it at the
    // bottom row once it passes the fold.
    let window = |app: &App| {
        let filtered = app.ai_providers.wizard.filtered(&app.providers);
        app.ai_providers.wizard.window(&filtered, 10)
    };
    app.ai_providers.wizard.focused = 50;
    let (start, count) = window(&app);
    assert!(
        (start..start + count).contains(&50),
        "window {start}..{} must contain focus 50",
        start + count
    );

    // A focus near the top anchors the window at row 0.
    app.ai_providers.wizard.focused = 1;
    let (start, _count) = window(&app);
    assert_eq!(start, 0, "window should start at the top for row 1");

    // The window never exceeds the list bounds at the bottom edge.
    app.ai_providers.wizard.focused = app.providers.len() - 1;
    let (start, count) = window(&app);
    assert_eq!(
        start + count,
        app.providers.len(),
        "window must end at the last provider"
    );
}

#[test]
fn ai_providers_new_account_filter_narrows_provider_list() {
    let mut app = test_app();
    let (tx, _rx) = crossbeam_channel::unbounded();
    setup_providers_new_account(&mut app);

    // Typing filters by case-insensitive substring over display names.
    for c in "open".chars() {
        handle_terminal_event(
            Event::Key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE)),
            &mut app,
            &tx,
        )
        .expect("filter char");
    }
    let filtered = app.ai_providers.wizard.filtered(&app.providers);
    assert!(
        !filtered.is_empty(),
        "'open' must match at least one provider"
    );
    assert!(
        filtered
            .iter()
            .all(|p| p.display_name.to_lowercase().contains("open")),
        "every match contains 'open' in its display name"
    );

    // Enter now picks the *highlighted filtered* provider, and the picked
    // slug is the one at the (clamped) focus position.
    handle_terminal_event(
        Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
        &mut app,
        &tx,
    )
    .expect("enter picks filtered provider");
    assert_eq!(app.ai_providers.wizard.step, AccountWizardStep::Slug);
    let filtered = app.ai_providers.wizard.filtered(&app.providers);
    let picked = filtered.get(app.ai_providers.wizard.focused);
    assert_eq!(
        app.ai_providers.wizard.picked_slug.as_deref(),
        picked.map(|p| p.slug.as_str()),
        "the picked provider matches the highlighted filtered row"
    );
}

#[expect(clippy::assert_is_empty)] // deliberate empty-check; clippy's rewrite is worse
#[test]
fn ai_providers_new_account_filter_no_match_blocks_enter() {
    let mut app = test_app();
    let (tx, _rx) = crossbeam_channel::unbounded();
    setup_providers_new_account(&mut app);

    // A filter with no matches empties the list; Enter must be a no-op (stay
    // on the provider step) rather than picking nothing.
    for c in "zzzz-no-such-provider".chars() {
        handle_terminal_event(
            Event::Key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE)),
            &mut app,
            &tx,
        )
        .expect("filter char");
    }
    assert!(app.ai_providers.wizard.filtered(&app.providers).is_empty());
    assert_eq!(app.ai_providers.wizard.focused, 0);

    handle_terminal_event(
        Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
        &mut app,
        &tx,
    )
    .expect("enter on empty filtered list");

    assert_eq!(app.ai_providers.wizard.step, AccountWizardStep::Provider);
    assert!(app.ai_providers.wizard.is_open());
    assert!(app.ai_providers.wizard.picked_slug.is_none());
}

#[test]
fn ai_providers_new_account_slug_validation_empty() {
    let mut app = test_app();
    let (tx, _rx) = crossbeam_channel::unbounded();
    advance_to_slug_phase(&mut app, &tx, "openai");

    // Enter on an empty slug should show an error and stay on the step.
    handle_terminal_event(
        Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
        &mut app,
        &tx,
    )
    .expect("enter empty slug");

    assert_eq!(app.ai_providers.wizard.step, AccountWizardStep::Slug);
    assert!(
        app.ai_providers.wizard.slug.is_focused(),
        "should stay on slug field when slug is empty"
    );
    assert_eq!(
        app.ai_providers.wizard.error.get(),
        Some("Account name is required"),
    );
}

#[test]
fn ai_providers_new_account_slug_validation_invalid() {
    let mut app = test_app();
    let (tx, _rx) = crossbeam_channel::unbounded();
    advance_to_slug_phase(&mut app, &tx, "openai");

    // Type uppercase (invalid — must be lowercase).
    handle_terminal_event(
        Event::Key(KeyEvent::new(KeyCode::Char('U'), KeyModifiers::NONE)),
        &mut app,
        &tx,
    )
    .expect("char uppercase");
    handle_terminal_event(
        Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
        &mut app,
        &tx,
    )
    .expect("enter invalid slug");

    assert_eq!(app.ai_providers.wizard.step, AccountWizardStep::Slug);
    assert!(
        app.ai_providers.wizard.slug.is_focused(),
        "should stay on slug field when slug is invalid"
    );
    assert_eq!(
        app.ai_providers.wizard.error.get(),
        Some("account name must be lowercase letters, digits, hyphens, or underscores"),
    );
}

#[test]
fn ai_providers_new_account_esc_aborts_wizard() {
    let mut app = test_app();
    let (tx, _rx) = crossbeam_channel::unbounded();
    setup_providers_new_account(&mut app);

    assert!(app.ai_providers.wizard.is_open());
    assert_eq!(app.ai_providers.wizard.step, AccountWizardStep::Provider);

    handle_terminal_event(
        Event::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
        &mut app,
        &tx,
    )
    .expect("esc cancels wizard");

    assert!(!app.ai_providers.wizard.is_open());
    assert_eq!(app.ai_providers.wizard.focused, 0);
}

#[test]
fn ai_providers_new_account_esc_backs_to_provider_from_slug() {
    let mut app = test_app();
    let (tx, _rx) = crossbeam_channel::unbounded();
    advance_to_slug_phase(&mut app, &tx, "anthropic");

    let anthro_idx = app
        .providers
        .iter()
        .position(|p| p.slug == "anthropic")
        .expect("anthropic in options");
    assert_eq!(app.ai_providers.wizard.focused, anthro_idx);

    // Type a partial slug, then Esc should return to the provider picker
    // while keeping the previously chosen provider highlighted.
    handle_terminal_event(
        Event::Key(KeyEvent::new(KeyCode::Char('m'), KeyModifiers::NONE)),
        &mut app,
        &tx,
    )
    .expect("char m");
    handle_terminal_event(
        Event::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
        &mut app,
        &tx,
    )
    .expect("esc back to provider");

    assert_eq!(app.ai_providers.wizard.step, AccountWizardStep::Provider);
    assert_eq!(app.ai_providers.wizard.focused, anthro_idx);
    assert_eq!(app.ai_providers.wizard.slug.value(), "");
}

#[test]
fn ai_providers_new_account_submit_creates_account_and_redirects_to_credential() {
    let mut app = test_app();
    let (tx, rx) = crossbeam_channel::unbounded();
    advance_to_slug_phase(&mut app, &tx, "openai");

    // Type a valid slug.
    for c in "my-account".chars() {
        handle_terminal_event(
            Event::Key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE)),
            &mut app,
            &tx,
        )
        .expect("char");
    }

    // Submit — creates the account.
    handle_terminal_event(
        Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
        &mut app,
        &tx,
    )
    .expect("enter submits slug");

    // AddAccount was sent with the slug as the account name.
    let msg = rx.recv().expect("AddAccount message");
    assert_eq!(
        msg.inner,
        ClientMessageType::AddAccount {
            name: "my-account".to_string(),
            provider: "openai".to_string(),
            base_url: None,
            streaming: None,
            retry_max_attempts: None,
            connect_timeout_secs: None,
            request_timeout_secs: None,
            total_timeout_secs: None,
        }
    );

    // The wizard closes and the flow lands on the credential modal for the
    // freshly created account.
    assert!(!app.ai_providers.wizard.is_open());
    assert_eq!(
        app.ai_providers.credential.target.as_deref(),
        Some("my-account")
    );
    assert_eq!(app.ai_providers.wizard.focused, 0);
    assert_eq!(app.ai_providers.wizard.slug.value(), "");

    // A key now goes to the credential input, not the slug field.
    handle_terminal_event(
        Event::Key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::NONE)),
        &mut app,
        &tx,
    )
    .expect("char s on credential");
    assert_eq!(app.ai_providers.credential.input.text, "s");
    assert_eq!(app.ai_providers.wizard.slug.value(), "");
}

#[expect(clippy::assert_is_empty)] // deliberate empty-check; clippy's rewrite is worse
#[test]
fn ai_providers_new_account_typing_goes_to_slug_field() {
    let mut app = test_app();
    let (tx, _rx) = crossbeam_channel::unbounded();
    advance_to_slug_phase(&mut app, &tx, "openai");

    // Slug is focused by default; typing goes into the slug buffer.
    handle_terminal_event(
        Event::Key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE)),
        &mut app,
        &tx,
    )
    .expect("char x on slug");
    assert_eq!(app.ai_providers.wizard.slug.value(), "x");
    assert!(app.ai_providers.credential.input.text.is_empty());
}

#[test]
fn ai_providers_new_account_escaped_slug_input_not_leaked_to_credential() {
    let mut app = test_app();
    let (tx, _rx) = crossbeam_channel::unbounded();
    advance_to_slug_phase(&mut app, &tx, "openai");

    // 'k' on the slug page types into the slug field (it is not a nav key
    // in step 2).
    handle_terminal_event(
        Event::Key(KeyEvent::new(KeyCode::Char('k'), KeyModifiers::NONE)),
        &mut app,
        &tx,
    )
    .expect("k on slug");
    assert_eq!(
        app.ai_providers.wizard.slug.value(),
        "k",
        "k should type into slug field in step 2"
    );
}
