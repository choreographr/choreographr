use crate::connection::handle_daemon_message;
use crate::state::*;
use crate::tests::support::test_app;
use choreo_proto::{ClientMessageType, DaemonMessage, DaemonMessageType, SessionEvent};

// ── Connection-level `session_id: None` and ModelSelectionFailed gating ──
//
// The daemon replies to bare `/reasoning` (GetReasoningEffort) without an
// attachment, and sends other connection-level errors, with `session_id: None`
// meaning "resolve to the attached session".  Those must keep their
// fall-through feedback — they are the user's own command replies, not
// background noise.  ModelSelectionFailed is the failure counterpart of
// ModelSelected and must be gated the same way for background sessions.

#[test]
fn reasoning_effort_set_connection_level_writes_status_feedback_for_attached_session() {
    let mut app = test_app();
    let (tx, _rx) = crossbeam_channel::unbounded();
    app.attached_session_id = Some(42);
    app.active_session_id = Some(42);

    // A connection-level ReasoningEffortSet (bare `/reasoning` without an
    // attachment) arrives with `session_id: None` and resolves to the
    // attached session.  It must be treated as the user's own feedback, not
    // swallowed as background noise.
    handle_daemon_message(
        DaemonMessage::broadcast(DaemonMessageType::Session {
            session_id: None,
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
        "bare /reasoning feedback must be preserved for a connection-level reply"
    );
    assert_eq!(
        app.display_for(42).reasoning_effort.as_deref(),
        Some("high"),
        "the current effort must land in the attached session's display"
    );
    assert_eq!(
        app.display_for(0).reasoning_effort.as_deref(),
        None,
        "a connection-level reply must not write a phantom session-0 display"
    );
    assert_eq!(app.error(), None);
}

#[test]
fn reasoning_effort_set_failed_connection_level_writes_status_and_error() {
    let mut app = test_app();
    let (tx, _rx) = crossbeam_channel::unbounded();
    app.attached_session_id = Some(42);
    app.active_session_id = Some(42);

    // The daemon sends ReasoningEffortSetFailed with `session_id: None` for
    // connection-level rejections ("no session attached").  The user must see
    // the rejection of their own /reasoning command.
    handle_daemon_message(
        DaemonMessage::broadcast(DaemonMessageType::Session {
            session_id: None,
            event: SessionEvent::ReasoningEffortSetFailed {
                effort: "high".to_string(),
                error: "no session attached".to_string(),
            },
        }),
        &mut app,
        &tx,
    )
    .expect("handle ReasoningEffortSetFailed");

    assert_eq!(
        app.status(),
        Some("reasoning effort rejected: no session attached")
    );
    assert_eq!(
        app.error(),
        Some("[daemon] failed to set reasoning effort high: no session attached")
    );
    assert_eq!(
        app.display_for(42).reasoning_effort.as_deref(),
        Some("off"),
        "the attached session's effort is reset on rejection"
    );
}

#[test]
fn model_selection_failed_for_background_session_does_not_write_global_error() {
    let mut app = test_app();
    let (tx, _rx) = crossbeam_channel::unbounded();
    app.attached_session_id = Some(42);
    app.active_session_id = Some(42);
    assert!(app.status().is_none() && app.error().is_none());

    handle_daemon_message(
        DaemonMessage::broadcast(DaemonMessageType::Session {
            session_id: Some(99),
            event: SessionEvent::ModelSelectionFailed {
                model: "gpt-other".to_string(),
                error: "model not found".to_string(),
            },
        }),
        &mut app,
        &tx,
    )
    .expect("handle ModelSelectionFailed");

    assert_eq!(
        app.error(),
        None,
        "a background session's model rejection must not write the global error line"
    );
    assert_eq!(app.status(), None);
}

#[test]
fn model_selection_failed_for_attached_session_writes_global_error() {
    let mut app = test_app();
    let (tx, _rx) = crossbeam_channel::unbounded();
    app.attached_session_id = Some(42);
    app.active_session_id = Some(42);

    // The user's own /model command failed — the rejection must still be
    // surfaced via the generic dispatch's error write.
    handle_daemon_message(
        DaemonMessage::broadcast(DaemonMessageType::Session {
            session_id: Some(42),
            event: SessionEvent::ModelSelectionFailed {
                model: "gpt-x".to_string(),
                error: "model not found".to_string(),
            },
        }),
        &mut app,
        &tx,
    )
    .expect("handle ModelSelectionFailed");

    assert_eq!(
        app.error(),
        Some("[daemon] failed to select model gpt-x: model not found"),
        "attached-session /model rejection feedback must be preserved"
    );
}

#[test]
fn model_selection_failed_connection_level_writes_global_error() {
    let mut app = test_app();
    let (tx, _rx) = crossbeam_channel::unbounded();
    app.attached_session_id = Some(42);
    app.active_session_id = Some(42);

    // No session attached at the daemon connection level — the `None` reply
    // must keep its error feedback, not be swallowed as background noise.
    handle_daemon_message(
        DaemonMessage::broadcast(DaemonMessageType::Session {
            session_id: None,
            event: SessionEvent::ModelSelectionFailed {
                model: "gpt-x".to_string(),
                error: "no session attached".to_string(),
            },
        }),
        &mut app,
        &tx,
    )
    .expect("handle ModelSelectionFailed");

    assert_eq!(
        app.error(),
        Some("[daemon] failed to select model gpt-x: no session attached")
    );
}

// ── SessionCreated must not auto-attach to agent-spawned sub-sessions ──

#[test]
fn session_created_for_sub_session_does_not_hijack_chat_view() {
    let mut app = test_app();
    let (tx, rx) = crossbeam_channel::unbounded();
    // The user is viewing session 42 on the Chat page.
    app.attached_session_id = Some(42);
    app.active_session_id = Some(42);

    // spawn_subsession makes the daemon broadcast SessionCreated with
    // parent_session_id = Some(parent).  The TUI must not switch to it.
    handle_daemon_message(
        DaemonMessage::broadcast(DaemonMessageType::Session {
            session_id: Some(99),
            event: SessionEvent::SessionCreated {
                parent_session_id: Some(42),
                title: None,
                working_dir: None,
                account_name: None,
                selected_model: None,
                reasoning_effort: None,
            },
        }),
        &mut app,
        &tx,
    )
    .expect("handle SessionCreated");

    // The view must stay on the session the user was reading.
    assert_eq!(
        app.attached_session_id,
        Some(42),
        "sub-session creation must not change the attached session"
    );
    assert_eq!(
        app.active_session_id,
        Some(42),
        "sub-session creation must not change the active session"
    );
    // No AttachSession may be sent for the sub-session, and — because the
    // Chat page renders `Sessions` replies into the status line — no
    // unsolicited list refresh either (it would rewrite the status line and
    // reflow the viewed viewport).
    let msgs: Vec<ClientMessageType> = rx.try_iter().map(|m| m.inner).collect();
    assert!(
        !msgs.iter().any(
            |m| matches!(m, ClientMessageType::AttachSession { session_id } if *session_id == 99)
        ),
        "sub-session creation must not auto-attach"
    );
    assert!(
        !msgs
            .iter()
            .any(|m| matches!(m, ClientMessageType::ListSessions)),
        "on the Chat page a sub-session creation must not refresh the session list — \
         the Sessions reply would rewrite the status line"
    );
}

#[test]
fn session_created_for_sub_session_on_session_manager_refreshes_list() {
    let mut app = test_app();
    let (tx, rx) = crossbeam_channel::unbounded();
    app.attached_session_id = Some(42);
    app.active_session_id = Some(42);
    app.page = Page::SessionManager;

    handle_daemon_message(
        DaemonMessage::broadcast(DaemonMessageType::Session {
            session_id: Some(99),
            event: SessionEvent::SessionCreated {
                parent_session_id: Some(42),
                title: None,
                working_dir: None,
                account_name: None,
                selected_model: None,
                reasoning_effort: None,
            },
        }),
        &mut app,
        &tx,
    )
    .expect("handle SessionCreated");

    // Still no auto-attach, but the list refresh is harmless here: the reply
    // renders into the session list, not the status line.
    assert_eq!(app.attached_session_id, Some(42));
    assert_eq!(app.active_session_id, Some(42));
    let msgs: Vec<ClientMessageType> = rx.try_iter().map(|m| m.inner).collect();
    assert!(
        !msgs.iter().any(
            |m| matches!(m, ClientMessageType::AttachSession { session_id } if *session_id == 99)
        ),
        "sub-session creation must not auto-attach"
    );
    assert!(
        msgs.iter()
            .any(|m| matches!(m, ClientMessageType::ListSessions)),
        "on the Session Manager page the list must refresh so the sub-session is visible"
    );
}

#[test]
fn session_created_for_user_session_attaches_on_chat_page() {
    let mut app = test_app();
    let (tx, rx) = crossbeam_channel::unbounded();
    app.attached_session_id = Some(42);
    app.active_session_id = Some(42);

    // The direct reply to THIS client's CreateSession
    // (`SessionCreatedForRequester`, parent_session_id = None) keeps the old
    // behavior: switch the view and attach.
    handle_daemon_message(
        DaemonMessage::broadcast(DaemonMessageType::Session {
            session_id: Some(99),
            event: SessionEvent::SessionCreatedForRequester {
                parent_session_id: None,
                title: None,
                working_dir: None,
                account_name: Some("acct".to_string()),
                selected_model: Some("gpt-new".to_string()),
                reasoning_effort: Some("off".to_string()),
            },
        }),
        &mut app,
        &tx,
    )
    .expect("handle SessionCreatedForRequester");

    assert_eq!(app.attached_session_id, Some(99));
    assert_eq!(app.active_session_id, Some(99));
    // The new session's display fields are primed from the creation params.
    assert_eq!(app.display_for(99).account_name.as_deref(), Some("acct"));
    assert_eq!(
        app.display_for(99).selected_model.as_deref(),
        Some("gpt-new")
    );
    let msgs: Vec<ClientMessageType> = rx.try_iter().map(|m| m.inner).collect();
    assert!(
        msgs.iter().any(
            |m| matches!(m, ClientMessageType::AttachSession { session_id } if *session_id == 99)
        ),
        "a user-created session must still auto-attach on the Chat page"
    );
    assert!(
        msgs.iter()
            .any(|m| matches!(m, ClientMessageType::ListSessions))
    );
}

#[test]
fn session_created_for_user_session_on_session_manager_navigates() {
    // Pressing `n` (or `/new`, or `/session new`) on the Session Manager
    // creates a session; the requester reply must navigate the creator to it
    // (attach + switch to the Chat page), not leave them stranded on the list.
    let mut app = test_app();
    let (tx, rx) = crossbeam_channel::unbounded();
    app.attached_session_id = Some(42);
    app.active_session_id = Some(42);
    app.page = Page::SessionManager;

    handle_daemon_message(
        DaemonMessage::broadcast(DaemonMessageType::Session {
            session_id: Some(99),
            event: SessionEvent::SessionCreatedForRequester {
                parent_session_id: None,
                title: None,
                working_dir: None,
                account_name: Some("acct".to_string()),
                selected_model: Some("gpt-new".to_string()),
                reasoning_effort: Some("off".to_string()),
            },
        }),
        &mut app,
        &tx,
    )
    .expect("handle SessionCreatedForRequester");

    assert_eq!(
        app.page,
        Page::Chat,
        "the creator must land on the session it just made"
    );
    assert_eq!(app.attached_session_id, Some(99));
    assert_eq!(app.active_session_id, Some(99));
    assert_eq!(app.display_for(99).account_name.as_deref(), Some("acct"));
    let msgs: Vec<ClientMessageType> = rx.try_iter().map(|m| m.inner).collect();
    assert!(
        msgs.iter().any(
            |m| matches!(m, ClientMessageType::AttachSession { session_id } if *session_id == 99)
        ),
        "the creator must attach to the new session"
    );
    assert!(
        msgs.iter()
            .any(|m| matches!(m, ClientMessageType::UnsubscribeSessionsSummary)),
        "leaving the Session Manager must drop its summary subscription"
    );
    // Exactly-once refresh: the direct reply must NOT also fetch the list from
    // the Session Manager page — the broadcast `SessionCreated` owns that (and
    // races this reply), so fetching here too would double the round-trip.
    assert!(
        !msgs
            .iter()
            .any(|m| matches!(m, ClientMessageType::ListSessions)),
        "a create from the Session Manager must not send a second ListSessions"
    );
}

// ── A BROADCAST SessionCreated must never hijack the view ──

#[test]
fn session_created_broadcast_does_not_attach_on_chat_page() {
    // The phone scenario: ANOTHER client created the session, so this client
    // only sees the broadcast `SessionCreated` (parent_session_id = None).
    // It must not move the view.
    let mut app = test_app();
    let (tx, rx) = crossbeam_channel::unbounded();
    app.attached_session_id = Some(42);
    app.active_session_id = Some(42);

    handle_daemon_message(
        DaemonMessage::broadcast(DaemonMessageType::Session {
            session_id: Some(99),
            event: SessionEvent::SessionCreated {
                parent_session_id: None,
                title: None,
                working_dir: None,
                account_name: Some("acct".to_string()),
                selected_model: Some("gpt-new".to_string()),
                reasoning_effort: None,
            },
        }),
        &mut app,
        &tx,
    )
    .expect("handle SessionCreated broadcast");

    assert_eq!(
        app.attached_session_id,
        Some(42),
        "another client's creation must not change the attached session"
    );
    assert_eq!(app.active_session_id, Some(42));
    let msgs: Vec<ClientMessageType> = rx.try_iter().map(|m| m.inner).collect();
    assert!(
        !msgs
            .iter()
            .any(|m| matches!(m, ClientMessageType::AttachSession { .. })),
        "a broadcast create must not auto-attach"
    );
    assert!(
        !msgs
            .iter()
            .any(|m| matches!(m, ClientMessageType::ListSessions)),
        "on the Chat page a broadcast create must not refresh the list — \
         the Sessions reply would rewrite the status line"
    );
}

#[test]
fn session_created_broadcast_on_session_manager_refreshes_list_only() {
    let mut app = test_app();
    let (tx, rx) = crossbeam_channel::unbounded();
    app.attached_session_id = Some(42);
    app.active_session_id = Some(42);
    app.page = Page::SessionManager;

    handle_daemon_message(
        DaemonMessage::broadcast(DaemonMessageType::Session {
            session_id: Some(99),
            event: SessionEvent::SessionCreated {
                parent_session_id: None,
                title: None,
                working_dir: None,
                account_name: None,
                selected_model: None,
                reasoning_effort: None,
            },
        }),
        &mut app,
        &tx,
    )
    .expect("handle SessionCreated broadcast");

    assert_eq!(app.attached_session_id, Some(42));
    assert_eq!(app.active_session_id, Some(42));
    let msgs: Vec<ClientMessageType> = rx.try_iter().map(|m| m.inner).collect();
    assert!(
        !msgs
            .iter()
            .any(|m| matches!(m, ClientMessageType::AttachSession { .. })),
        "a broadcast create must not auto-attach"
    );
    assert!(
        msgs.iter()
            .any(|m| matches!(m, ClientMessageType::ListSessions)),
        "on the Session Manager page the list must refresh so the new session is visible"
    );
}
