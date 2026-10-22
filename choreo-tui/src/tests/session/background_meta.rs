use crate::connection::handle_daemon_message;
use crate::tests::support::test_app;
use choreo_proto::{DaemonMessage, DaemonMessageType, SessionEvent};

// ── Background-session metadata must not write the global status/error line ──
//
// The TUI subscribes to ALL activity (SubscribeAllActivity), so ModelSelected,
// ReasoningEffortSet, ReasoningEffortSetFailed and SessionAccountSet arrive for
// background sessions too.  Before the fix these fell through to
// dispatch_daemon_message, whose generic arms write `app.status` / `app.error`
// — overwriting the global status line while the user views another session,
// which changes `status_error_height` and reflows the viewed history viewport.
// The per-session display routing stays; only the global write must be gated
// on the message belonging to the attached session.

#[test]
fn model_selected_for_background_session_does_not_write_global_status() {
    let mut app = test_app();
    let (tx, _rx) = crossbeam_channel::unbounded();
    // The user is viewing session 42.
    app.attached_session_id = Some(42);
    app.active_session_id = Some(42);
    assert!(app.status().is_none() && app.error().is_none());

    // A background session (99) changes its model.  Its per-session display
    // is updated, but the global status/error line must stay untouched.
    handle_daemon_message(
        DaemonMessage::broadcast(DaemonMessageType::Session {
            session_id: Some(99),
            event: SessionEvent::ModelSelected {
                model: "gpt-other".to_string(),
                reasoning_capability: None,
            },
        }),
        &mut app,
        &tx,
    )
    .expect("handle ModelSelected");

    assert_eq!(
        app.status(),
        None,
        "a background session's model change must not write the global status line"
    );
    assert_eq!(
        app.error(),
        None,
        "a background session's model change must not write the global error line"
    );
    assert_eq!(
        app.display_for(99).selected_model.as_deref(),
        Some("gpt-other"),
        "per-session display routing must still happen"
    );
}

#[test]
fn model_selected_for_attached_session_writes_status_feedback() {
    let mut app = test_app();
    let (tx, _rx) = crossbeam_channel::unbounded();
    app.attached_session_id = Some(42);
    app.active_session_id = Some(42);

    // The user's own `/model` command must still print its confirmation via
    // the generic dispatch fall-through.
    handle_daemon_message(
        DaemonMessage::broadcast(DaemonMessageType::Session {
            session_id: Some(42),
            event: SessionEvent::ModelSelected {
                model: "gpt-new".to_string(),
                reasoning_capability: None,
            },
        }),
        &mut app,
        &tx,
    )
    .expect("handle ModelSelected");

    assert_eq!(
        app.status(),
        Some("[daemon] selected model: gpt-new"),
        "attached-session /model feedback must be preserved"
    );
    assert_eq!(
        app.display_for(42).selected_model.as_deref(),
        Some("gpt-new")
    );
}

#[test]
fn reasoning_effort_set_for_background_session_does_not_write_global_status() {
    let mut app = test_app();
    let (tx, _rx) = crossbeam_channel::unbounded();
    app.attached_session_id = Some(42);
    app.active_session_id = Some(42);
    assert!(app.status().is_none() && app.error().is_none());

    handle_daemon_message(
        DaemonMessage::broadcast(DaemonMessageType::Session {
            session_id: Some(99),
            event: SessionEvent::ReasoningEffortSet {
                effort: "high".to_string(),
            },
        }),
        &mut app,
        &tx,
    )
    .expect("handle ReasoningEffortSet");

    assert_eq!(app.status(), None);
    assert_eq!(app.error(), None);
    assert_eq!(
        app.display_for(99).reasoning_effort.as_deref(),
        Some("high")
    );
}

#[test]
fn reasoning_effort_set_for_attached_session_writes_status_feedback() {
    let mut app = test_app();
    let (tx, _rx) = crossbeam_channel::unbounded();
    app.attached_session_id = Some(42);
    app.active_session_id = Some(42);

    handle_daemon_message(
        DaemonMessage::broadcast(DaemonMessageType::Session {
            session_id: Some(42),
            event: SessionEvent::ReasoningEffortSet {
                effort: "high".to_string(),
            },
        }),
        &mut app,
        &tx,
    )
    .expect("handle ReasoningEffortSet");

    assert_eq!(
        app.status(),
        Some("[daemon] reasoning effort: high"),
        "attached-session /reasoning feedback must be preserved"
    );
    assert_eq!(
        app.display_for(42).reasoning_effort.as_deref(),
        Some("high")
    );
}

#[test]
fn session_account_set_for_background_session_does_not_write_global_status() {
    let mut app = test_app();
    let (tx, _rx) = crossbeam_channel::unbounded();
    app.attached_session_id = Some(42);
    app.active_session_id = Some(42);
    assert!(app.status().is_none() && app.error().is_none());

    handle_daemon_message(
        DaemonMessage::broadcast(DaemonMessageType::Session {
            session_id: Some(99),
            event: SessionEvent::SessionAccountSet {
                account: "bg-account".to_string(),
            },
        }),
        &mut app,
        &tx,
    )
    .expect("handle SessionAccountSet");

    assert_eq!(app.status(), None);
    assert_eq!(app.error(), None);
    assert_eq!(
        app.display_for(99).account_name.as_deref(),
        Some("bg-account")
    );
}

#[test]
fn session_account_set_for_attached_session_writes_status_feedback() {
    let mut app = test_app();
    let (tx, _rx) = crossbeam_channel::unbounded();
    app.attached_session_id = Some(42);
    app.active_session_id = Some(42);

    handle_daemon_message(
        DaemonMessage::broadcast(DaemonMessageType::Session {
            session_id: Some(42),
            event: SessionEvent::SessionAccountSet {
                account: "main".to_string(),
            },
        }),
        &mut app,
        &tx,
    )
    .expect("handle SessionAccountSet");

    assert_eq!(
        app.status(),
        Some("[daemon] session account set: main"),
        "attached-session /account feedback must be preserved"
    );
    assert_eq!(app.display_for(42).account_name.as_deref(), Some("main"));
}

#[test]
fn reasoning_effort_set_failed_for_background_session_does_not_write_global_status_or_error() {
    let mut app = test_app();
    let (tx, _rx) = crossbeam_channel::unbounded();
    app.attached_session_id = Some(42);
    app.active_session_id = Some(42);
    assert!(app.status().is_none() && app.error().is_none());

    // A background session's rejection previously leaked through BOTH the
    // explicit `app.status` write and the generic dispatch's `app.error`.
    handle_daemon_message(
        DaemonMessage::broadcast(DaemonMessageType::Session {
            session_id: Some(99),
            event: SessionEvent::ReasoningEffortSetFailed {
                effort: "high".to_string(),
                error: "model does not support reasoning".to_string(),
            },
        }),
        &mut app,
        &tx,
    )
    .expect("handle ReasoningEffortSetFailed");

    assert_eq!(
        app.status(),
        None,
        "a background session's rejection must not write the global status line"
    );
    assert_eq!(
        app.error(),
        None,
        "a background session's rejection must not write the global error line"
    );
    assert_eq!(app.display_for(99).reasoning_effort.as_deref(), Some("off"));
}

#[test]
fn reasoning_effort_set_failed_for_attached_session_writes_status_and_error() {
    let mut app = test_app();
    let (tx, _rx) = crossbeam_channel::unbounded();
    app.attached_session_id = Some(42);
    app.active_session_id = Some(42);

    // The user's own `/reasoning` command failed — the rejection notice stays
    // on the status line and the generic dispatch records the error.
    handle_daemon_message(
        DaemonMessage::broadcast(DaemonMessageType::Session {
            session_id: Some(42),
            event: SessionEvent::ReasoningEffortSetFailed {
                effort: "high".to_string(),
                error: "model does not support reasoning".to_string(),
            },
        }),
        &mut app,
        &tx,
    )
    .expect("handle ReasoningEffortSetFailed");

    assert_eq!(
        app.status(),
        Some("reasoning effort rejected: model does not support reasoning")
    );
    assert_eq!(
        app.error(),
        Some("[daemon] failed to set reasoning effort high: model does not support reasoning")
    );
    assert_eq!(app.display_for(42).reasoning_effort.as_deref(), Some("off"));
}
