use crate::connection::{handle_daemon_message, handle_terminal_event};
use crate::state::*;
use crate::tests::support::test_app;
use choreo_proto::{AccountInfo, ClientMessageType, DaemonMessage, DaemonMessageType};
use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEventKind};

use super::send_mouse;

#[test]
fn ai_providers_enter_selects_account_and_returns_to_chat() {
    let (tx, rx) = crossbeam_channel::unbounded();
    let mut app = test_app();
    app.page = Page::AIProviders;
    app.ai_providers.set_accounts(vec![
        AccountInfo {
            name: "work-account".to_string(),
            provider: "anthropic".to_string(),
            has_credential: true,
        },
        AccountInfo {
            name: "personal-account".to_string(),
            provider: "openai".to_string(),
            has_credential: true,
        },
    ]);
    // Highlight the second account.
    app.ai_providers.selection = Some(1);

    handle_terminal_event(
        Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
        &mut app,
        &tx,
    )
    .expect("handle enter");

    // The view returns to the chat page and the highlighted account is sent
    // as the account for the active (attached) session.
    assert_eq!(app.page, Page::Chat);
    let msg = rx.recv().expect("sent message");
    assert_eq!(
        msg.inner,
        ClientMessageType::SetSessionAccount {
            name: "personal-account".to_string(),
        }
    );
}

#[test]
fn ai_providers_enter_without_selection_stays_on_page() {
    let (tx, rx) = crossbeam_channel::unbounded();
    let mut app = test_app();
    app.page = Page::AIProviders;
    app.ai_providers.set_accounts(vec![AccountInfo {
        name: "only-account".to_string(),
        provider: "anthropic".to_string(),
        has_credential: true,
    }]);
    // No selection (the list was emptied or never highlighted): Enter must
    // not send a message or leave the page.
    app.ai_providers.selection = None;

    handle_terminal_event(
        Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
        &mut app,
        &tx,
    )
    .expect("handle enter");

    assert_eq!(app.page, Page::AIProviders);
    assert!(
        rx.try_recv().is_err(),
        "no message sent without a selection"
    );
}

// ── AI Providers list click support ──────────────────

#[test]
fn ai_providers_list_click_selects_account_and_returns_to_chat() {
    let (tx, rx) = crossbeam_channel::unbounded();
    let mut app = test_app();
    app.page = Page::AIProviders;
    app.ai_providers.set_accounts(vec![
        AccountInfo {
            name: "work-account".to_string(),
            provider: "anthropic".to_string(),
            has_credential: true,
        },
        AccountInfo {
            name: "personal-account".to_string(),
            provider: "openai".to_string(),
            has_credential: true,
        },
    ]);
    app.last_terminal_size = Some((100, 40));

    let content = page_list_content_rect(Some((100, 40))).expect("geometry");
    // Click the second account's row (index 1): each account is a
    // 4-row block, so the second occupies content row 4..7.
    send_mouse(
        &mut app,
        MouseEventKind::Down(MouseButton::Left),
        content.x + 3,
        content.y + 4,
        &tx,
    );

    assert_eq!(
        app.page,
        Page::Chat,
        "clicking an account is equivalent to selecting it and pressing Enter"
    );
    let msg = rx.recv().expect("sent message");
    assert_eq!(
        msg.inner,
        ClientMessageType::SetSessionAccount {
            name: "personal-account".to_string(),
        }
    );
}

#[test]
fn ai_providers_list_click_selects_the_highlighted_account() {
    let (tx, _rx) = crossbeam_channel::unbounded();
    let mut app = test_app();
    app.page = Page::AIProviders;
    app.ai_providers.set_accounts(vec![
        AccountInfo {
            name: "work-account".to_string(),
            provider: "anthropic".to_string(),
            has_credential: true,
        },
        AccountInfo {
            name: "personal-account".to_string(),
            provider: "openai".to_string(),
            has_credential: true,
        },
    ]);
    app.last_terminal_size = Some((100, 40));

    let content = page_list_content_rect(Some((100, 40))).expect("geometry");
    // A click on the second account's row must move the highlight there,
    // so a failed send (broken pipe) still leaves it selected on the page.
    send_mouse(
        &mut app,
        MouseEventKind::Down(MouseButton::Left),
        content.x + 3,
        content.y + 4,
        &tx,
    );

    assert_eq!(
        app.ai_providers.selection,
        Some(1),
        "the clicked account stays highlighted"
    );
}

#[test]
fn ai_providers_list_click_outside_rows_is_noop() {
    let (tx, rx) = crossbeam_channel::unbounded();
    let mut app = test_app();
    app.page = Page::AIProviders;
    app.ai_providers.set_accounts(vec![AccountInfo {
        name: "only-account".to_string(),
        provider: "anthropic".to_string(),
        has_credential: true,
    }]);
    app.last_terminal_size = Some((100, 40));

    // Click the top block border (outside the content rows): a no-op.
    send_mouse(&mut app, MouseEventKind::Down(MouseButton::Left), 0, 0, &tx);

    assert_eq!(app.page, Page::AIProviders);
    assert!(
        rx.try_recv().is_err(),
        "no message sent for an outside click"
    );
}

#[test]
fn ai_providers_list_click_before_first_frame_is_noop() {
    let (tx, rx) = crossbeam_channel::unbounded();
    let mut app = test_app();
    app.page = Page::AIProviders;
    app.ai_providers.set_accounts(vec![AccountInfo {
        name: "only-account".to_string(),
        provider: "anthropic".to_string(),
        has_credential: true,
    }]);
    // last_terminal_size stays None (viewport unknown) -> click is a no-op.
    send_mouse(&mut app, MouseEventKind::Down(MouseButton::Left), 3, 3, &tx);

    assert_eq!(app.page, Page::AIProviders);
    assert!(rx.try_recv().is_err());
}

#[test]
fn ai_providers_list_wheel_scrolls_highlight() {
    let (tx, _rx) = crossbeam_channel::unbounded();
    let mut app = test_app();
    app.page = Page::AIProviders;
    app.ai_providers.set_accounts(vec![
        AccountInfo {
            name: "a".to_string(),
            provider: "p".to_string(),
            has_credential: true,
        },
        AccountInfo {
            name: "b".to_string(),
            provider: "q".to_string(),
            has_credential: true,
        },
    ]);
    app.last_terminal_size = Some((100, 40));

    send_mouse(&mut app, MouseEventKind::ScrollDown, 0, 0, &tx);
    assert_eq!(app.ai_providers.selection, Some(1));
    send_mouse(&mut app, MouseEventKind::ScrollUp, 0, 0, &tx);
    assert_eq!(app.ai_providers.selection, Some(0));
}

#[test]
fn ai_providers_credential_added_refreshes_account_list() {
    let mut app = test_app();
    let (tx, rx) = crossbeam_channel::unbounded();

    // Seed the accounts page with an account that has no credential yet.
    app.ai_providers.set_accounts(vec![AccountInfo {
        name: "my-account".to_string(),
        provider: "openai".to_string(),
        has_credential: false,
    }]);

    // The daemon confirms the credential was stored.  CredentialAdded does
    // not carry account data, so the accounts page must re-request the list
    // — otherwise `has_credential` stays stale (no) until the user leaves
    // and re-enters the page.
    handle_daemon_message(
        DaemonMessage::broadcast(DaemonMessageType::CredentialAdded {
            service: "my-account".to_string(),
        }),
        &mut app,
        &tx,
    )
    .expect("handle CredentialAdded");

    let msg = rx.recv().expect("ListAccounts sent after credential added");
    assert_eq!(msg.inner, ClientMessageType::ListAccounts);

    // Removal refreshes the list the same way.
    handle_daemon_message(
        DaemonMessage::broadcast(DaemonMessageType::CredentialRemoved {
            service: "my-account".to_string(),
        }),
        &mut app,
        &tx,
    )
    .expect("handle CredentialRemoved");

    let msg = rx
        .recv()
        .expect("ListAccounts sent after credential removed");
    assert_eq!(msg.inner, ClientMessageType::ListAccounts);
}
