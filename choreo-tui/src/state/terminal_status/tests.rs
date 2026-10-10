//! Unit tests for `state::terminal_status` (OSC 7501 records + OSC 2 title).

use crate::state::tests::make_session;
use crate::test_util::test_app;
use choreo_client_core::TurnEventHandler;
use choreo_proto::SessionStatus;

#[test]
fn desired_records_include_attached_and_active_background_only() {
    let mut app = test_app();
    app.attached_session_id = Some(1);
    app.attached_status = Some(SessionStatus::Inference);
    app.session_mgr.all = vec![
        {
            let mut s = make_session(1, "attached");
            s.status = SessionStatus::Inference;
            s
        },
        {
            let mut s = make_session(2, "busy background");
            s.status = SessionStatus::ToolCall("shell".into());
            s
        },
        {
            let mut s = make_session(3, "idle background");
            s.status = SessionStatus::Inactive;
            s
        },
    ];

    let desired = app.desired_status_records();
    let ids: Vec<u64> = desired.iter().map(|(id, _)| *id).collect();
    assert!(ids.contains(&1), "the attached session is always present");
    assert!(ids.contains(&2), "an active background session is present");
    assert!(
        !ids.contains(&3),
        "an idle background session must be omitted so a stale record is cleared"
    );

    // The ToolCall msg carries the tool NAME only (base64 of "shell").
    let bg = desired
        .iter()
        .find(|(id, _)| *id == 2)
        .map(|(_, seq)| seq)
        .expect("background record");
    assert!(bg.contains("state=working"), "got {bg}");
    assert!(bg.contains(":msg=c2hlbGw="), "tool name as msg, got {bg}");
}

#[test]
fn a_finished_background_agent_keeps_its_done_record() {
    let mut app = test_app();
    // The attached session is unrelated; the background agent is the subject.
    app.attached_session_id = Some(1);
    app.attached_status = Some(SessionStatus::Inference);
    app.session_mgr.all = vec![
        {
            let mut s = make_session(1, "attached");
            s.status = SessionStatus::Inference;
            s
        },
        {
            let mut s = make_session(2, "background agent");
            s.status = SessionStatus::Inference;
            s
        },
    ];

    // The background agent finishes: `handle_done` records the outcome and the
    // daemon then drops its live status to `Inactive`.
    app.handle_done(2, 9, None, None);
    app.handle_session_status_changed(2, &SessionStatus::Inactive, 1);

    let desired = app.desired_status_records();
    let bg = desired
        .iter()
        .find(|(id, _)| *id == 2)
        .map(|(_, seq)| seq)
        .expect("a finished background agent still reports");
    assert!(
        bg.contains("state=done"),
        "the background completion survives the idle status, got {bg}"
    );
    assert!(
        !bg.contains(":msg="),
        "a terminal outcome carries no tool msg"
    );
}

#[test]
fn done_override_wins_and_survives_the_trailing_idle() {
    let mut app = test_app();
    app.attached_session_id = Some(1);
    app.attached_status = Some(SessionStatus::Inference);
    app.session_mgr.all = vec![{
        let mut s = make_session(1, "t");
        s.status = SessionStatus::Inference;
        s
    }];

    app.handle_done(1, 7, None, None);
    // The daemon broadcasts an idle status right after the turn finishes; the
    // done outcome must survive it.
    app.handle_session_status_changed(1, &SessionStatus::Inactive, 1);

    let seq = app
        .desired_status_records()
        .into_iter()
        .find(|(id, _)| *id == 1)
        .map(|(_, seq)| seq)
        .expect("attached record");
    assert!(
        seq.contains("state=done"),
        "done survives the idle, got {seq}"
    );
    assert!(
        !seq.contains(":msg="),
        "a terminal outcome carries no tool msg"
    );
}

#[test]
fn fresh_active_status_clears_the_override() {
    let mut app = test_app();
    app.attached_session_id = Some(1);
    app.session_mgr.all = vec![make_session(1, "t")];

    app.handle_done(1, 7, None, None);
    assert!(app.term_status_override.contains_key(&1));

    // A new turn began (status went active): the old outcome is dropped.
    app.handle_session_status_changed(1, &SessionStatus::Inference, 2);
    assert!(!app.term_status_override.contains_key(&1));
}

#[test]
fn failed_and_cancelled_records() {
    let mut app = test_app();
    app.attached_session_id = Some(1);
    app.attached_status = Some(SessionStatus::Inference);
    app.session_mgr.all = vec![{
        let mut s = make_session(1, "t");
        s.status = SessionStatus::Inference;
        s
    }];

    app.handle_cancelled(Some(1), 8);
    let seq = app
        .desired_status_records()
        .into_iter()
        .find(|(id, _)| *id == 1)
        .map(|(_, seq)| seq)
        .expect("attached record");
    assert!(seq.contains("state=idle"), "got {seq}");
    assert!(
        app.display_for(1).error.is_none(),
        "a user cancel must not record an error"
    );
    assert!(
        app.error.is_none(),
        "a user cancel must not write the global error bar"
    );

    // A real failure still reports `error` and records its message.
    app.handle_failed(Some(1), 7, "boom".into());
    let seq = app
        .desired_status_records()
        .into_iter()
        .find(|(id, _)| *id == 1)
        .map(|(_, seq)| seq)
        .expect("attached record");
    assert!(seq.contains("state=error"), "got {seq}");
    assert_eq!(app.display_for(1).error.as_deref(), Some("boom"));
}

#[test]
fn window_title_reflects_the_attached_session() {
    let mut app = test_app();
    assert_eq!(app.window_title(), "Choreographr");

    app.attached_session_id = Some(1);
    app.session_mgr.all = vec![make_session(1, "Fix the parser")];
    assert_eq!(app.window_title(), "Choreographr - Fix the parser");
}
