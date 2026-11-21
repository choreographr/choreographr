use crate::connection::handle_daemon_message;
use crate::tests::support::test_app;
use choreo_proto::{ClientMessageType, DaemonMessage, DaemonMessageType};

// ── A fresh account's model list: waiting, then a push ──

#[test]
fn an_empty_provisional_reply_keeps_the_popup_waiting() {
    // The daemon has nothing cached or seedable for this account yet, so it
    // answers with an empty provisional list and fetches in the background.
    // The picker must show a waiting row rather than "no models", and it must
    // not report an error: the wait ends on a push, not on a failure.
    let mut app = test_app();
    app.attached_account_slug = Some("anthropic".to_string());
    let (tx, _rx) = crossbeam_channel::unbounded();
    app.model_selector.open();
    let id = app.pending.send(&tx, ClientMessageType::ListModels);

    handle_daemon_message(
        DaemonMessage::reply(
            id,
            DaemonMessageType::Models {
                models: Vec::new(),
                selected_model: None,
                provisional: true,
            },
        ),
        &mut app,
        &tx,
    )
    .expect("handle Models");

    assert!(
        app.model_selector.loading,
        "the picker keeps its waiting row while the daemon fetches"
    );
    assert!(
        app.model_selector.all_models.is_empty(),
        "there is nothing to paint yet"
    );
    assert_eq!(app.model_selector.error.get(), None);
}

#[test]
fn a_models_updated_push_paints_the_list_and_ends_the_wait() {
    // The fetch that the empty provisional reply announced settles with the
    // live list, painted in place and marked verified.
    let mut app = test_app();
    app.attached_account_slug = Some("anthropic".to_string());
    let (tx, _rx) = crossbeam_channel::unbounded();
    app.model_selector.open();
    app.model_selector.apply_models(Vec::new(), None, true);
    assert!(app.model_selector.loading);

    handle_daemon_message(
        DaemonMessage::broadcast(DaemonMessageType::ModelsUpdated {
            account: "anthropic".to_string(),
            models: vec!["claude-3".to_string(), "claude-4".to_string()],
        }),
        &mut app,
        &tx,
    )
    .expect("handle ModelsUpdated");

    assert!(!app.model_selector.loading, "the waiting row is gone");
    assert!(
        !app.model_selector.provisional,
        "the list is the verified one"
    );
    assert_eq!(app.model_selector.all_models, vec!["claude-3", "claude-4"]);
    assert_eq!(app.model_selector.error.get(), None);
    assert!(app.status().is_none(), "the push must not print to chat");
}

#[test]
fn a_models_unavailable_push_shows_the_error_inline() {
    // The fetch the picker was waiting on failed. The push ends the wait with
    // the provider's own message, inline in the popup — never a waiting row
    // that cannot end.
    let mut app = test_app();
    app.attached_account_slug = Some("anthropic".to_string());
    let (tx, _rx) = crossbeam_channel::unbounded();
    app.model_selector.open();
    app.model_selector.apply_models(Vec::new(), None, true);
    assert!(app.model_selector.loading);

    handle_daemon_message(
        DaemonMessage::broadcast(DaemonMessageType::ModelsUnavailable {
            account: "anthropic".to_string(),
            error: "provider unreachable".to_string(),
        }),
        &mut app,
        &tx,
    )
    .expect("handle ModelsUnavailable");

    assert!(!app.model_selector.loading, "the waiting row is gone");
    assert_eq!(app.model_selector.error.get(), Some("provider unreachable"));
}

#[test]
fn a_models_unavailable_push_is_ignored_for_a_different_account() {
    // The push is keyed by account, like `ModelsUpdated`: another account's
    // failed fetch must not put an error in this picker.
    let mut app = test_app();
    app.attached_account_slug = Some("anthropic".to_string());
    let (tx, _rx) = crossbeam_channel::unbounded();
    app.model_selector.open();
    app.model_selector.apply_models(Vec::new(), None, true);

    handle_daemon_message(
        DaemonMessage::broadcast(DaemonMessageType::ModelsUnavailable {
            account: "someone-else".to_string(),
            error: "provider unreachable".to_string(),
        }),
        &mut app,
        &tx,
    )
    .expect("handle ModelsUnavailable");

    assert!(
        app.model_selector.loading,
        "this account's fetch may still be running"
    );
    assert_eq!(app.model_selector.error.get(), None);
}

#[test]
fn a_models_unavailable_push_is_ignored_when_the_picker_is_closed() {
    let mut app = test_app();
    app.attached_account_slug = Some("anthropic".to_string());
    let (tx, _rx) = crossbeam_channel::unbounded();

    handle_daemon_message(
        DaemonMessage::broadcast(DaemonMessageType::ModelsUnavailable {
            account: "anthropic".to_string(),
            error: "provider unreachable".to_string(),
        }),
        &mut app,
        &tx,
    )
    .expect("handle ModelsUnavailable");

    assert_eq!(
        app.model_selector.error.get(),
        None,
        "a closed picker keeps no error for a later open to inherit"
    );
}

#[test]
fn a_models_failed_reply_is_shown_without_waiting() {
    // A genuine refusal has no fetch behind it, so it is terminal: painted
    // inline, no waiting row.
    let mut app = test_app();
    let (tx, _rx) = crossbeam_channel::unbounded();
    app.model_selector.open();
    let id = app.pending.send(&tx, ClientMessageType::ListModels);

    handle_daemon_message(
        DaemonMessage::reply(
            id,
            DaemonMessageType::ModelsFailed {
                error: "no accounts configured".to_string(),
            },
        ),
        &mut app,
        &tx,
    )
    .expect("handle ModelsFailed");

    assert_eq!(
        app.model_selector.error.get(),
        Some("no accounts configured")
    );
    assert!(!app.model_selector.loading);
}
