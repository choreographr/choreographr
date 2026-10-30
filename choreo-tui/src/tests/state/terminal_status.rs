//! Unit tests for `state::terminal_status` (OSC 7501 records + OSC 2 title).

use crate::state::LoginModal;
use crate::terminal::status::RecordKey;
use crate::tests::support::{make_session, test_app};
use choreo_client_core::TurnEventHandler;
use choreo_proto::SessionStatus;

/// The record published for `key`, if any.
fn record_for(app: &crate::state::App, key: RecordKey) -> Option<String> {
    app.desired_status_records()
        .into_iter()
        .find(|(k, _)| *k == key)
        .map(|(_, seq)| seq)
}

#[test]
fn desired_records_include_attached_and_active_background_only() {
    let mut app = test_app();
    app.attached_session_id = Some(1);
    app.attached_status = Some(SessionStatus::Responding);
    app.session_mgr.all = vec![
        {
            let mut s = make_session(1, "attached");
            s.status = SessionStatus::Responding;
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
    let keys: Vec<RecordKey> = desired.iter().map(|(key, _)| *key).collect();
    assert!(
        keys.contains(&RecordKey::Session(1)),
        "the attached session is always present"
    );
    assert!(
        keys.contains(&RecordKey::Session(2)),
        "an active background session is present"
    );
    assert!(
        !keys.contains(&RecordKey::Session(3)),
        "an idle background session must be omitted so a stale record is cleared"
    );
    assert!(
        !keys.contains(&RecordKey::Login),
        "no login flow is open, so there is no login record"
    );

    // The ToolCall msg carries the tool NAME only (base64 of "shell").
    let bg = record_for(&app, RecordKey::Session(2)).expect("background record");
    assert!(bg.contains("state=working"), "got {bg}");
    assert!(bg.contains(":msg=c2hlbGw="), "tool name as msg, got {bg}");
    // A tool call is not a retrying request, so it has no percentage.
    assert!(!bg.contains(":progress="), "got {bg}");
}

#[test]
fn a_finished_background_agent_keeps_its_done_record() {
    let mut app = test_app();
    // The attached session is unrelated; the background agent is the subject.
    app.attached_session_id = Some(1);
    app.attached_status = Some(SessionStatus::Responding);
    app.session_mgr.all = vec![
        {
            let mut s = make_session(1, "attached");
            s.status = SessionStatus::Responding;
            s
        },
        {
            let mut s = make_session(2, "background agent");
            s.status = SessionStatus::Responding;
            s
        },
    ];

    // The background agent finishes: `handle_done` records the outcome and the
    // daemon then drops its live status to `Inactive`.
    app.handle_done(2, 9, None, None);
    app.handle_session_status_changed(2, &SessionStatus::Inactive, 1);

    let bg =
        record_for(&app, RecordKey::Session(2)).expect("a finished background agent still reports");
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
    app.attached_status = Some(SessionStatus::Responding);
    app.session_mgr.all = vec![{
        let mut s = make_session(1, "t");
        s.status = SessionStatus::Responding;
        s
    }];

    app.handle_done(1, 7, None, None);
    // The daemon broadcasts an idle status right after the turn finishes; the
    // done outcome must survive it.
    app.handle_session_status_changed(1, &SessionStatus::Inactive, 1);

    let seq = record_for(&app, RecordKey::Session(1)).expect("attached record");
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
    app.handle_session_status_changed(1, &SessionStatus::Responding, 2);
    assert!(!app.term_status_override.contains_key(&1));
}

#[test]
fn failed_and_cancelled_records() {
    let mut app = test_app();
    app.attached_session_id = Some(1);
    app.attached_status = Some(SessionStatus::Responding);
    app.session_mgr.all = vec![{
        let mut s = make_session(1, "t");
        s.status = SessionStatus::Responding;
        s
    }];

    app.handle_cancelled(Some(1), 8);
    let seq = record_for(&app, RecordKey::Session(1)).expect("attached record");
    assert!(seq.contains("state=idle"), "got {seq}");
    assert!(
        app.display_for(1).error.is_none(),
        "a user cancel must not record an error"
    );
    assert!(
        app.error().is_none(),
        "a user cancel must not write the global error bar"
    );

    // A real failure still reports `error` and records its message.
    app.handle_failed(Some(1), 7, "boom".into());
    let seq = record_for(&app, RecordKey::Session(1)).expect("attached record");
    assert!(seq.contains("state=error"), "got {seq}");
    assert_eq!(app.display_for(1).error.as_deref(), Some("boom"));
}

#[test]
fn a_retrying_request_reports_progress() {
    let retrying = SessionStatus::Retrying {
        attempt: 2,
        max_attempts: 4,
        delay_ms: 500,
    };
    let mut app = test_app();
    app.attached_session_id = Some(1);
    app.attached_status = Some(retrying.clone());
    app.session_mgr.all = vec![{
        let mut s = make_session(1, "t");
        s.status = retrying;
        s
    }];

    let seq = record_for(&app, RecordKey::Session(1)).expect("attached record");
    assert!(seq.contains("state=working"), "got {seq}");
    assert!(
        seq.contains(":progress=50"),
        "attempt 2 of 4 is half the retries, got {seq}"
    );
    // `progress` is not a fill gauge: no context-window figure leaks into it,
    // and `kind` is meaningless while working.
    assert!(!seq.contains(":kind="), "got {seq}");
}

#[test]
fn an_open_login_modal_publishes_a_blocked_auth_record() {
    let mut app = test_app();
    let mut modal = LoginModal::test_modal("openai", "work");
    modal.status = "waiting for the browser".to_string();
    app.login = Some(modal);

    let seq = record_for(&app, RecordKey::Login).expect("login record");
    assert!(seq.contains("state=blocked"), "got {seq}");
    assert!(seq.contains(":kind=auth"), "got {seq}");
    assert!(seq.contains(":id=login"), "got {seq}");
    assert!(seq.contains(":app=choreo-tui"), "got {seq}");
    // `title` is the account (base64 of "work") and `msg` the status line.
    assert!(seq.contains(":title=d29yaw=="), "got {seq}");
    assert!(
        seq.contains(":msg=d2FpdGluZyBmb3IgdGhlIGJyb3dzZXI="),
        "got {seq}"
    );
    // It is a child record (a session id or the literal `login`), never the
    // terminal-wide root record, which OSC 9;4 owns.
    assert!(seq.contains(":id="), "got {seq}");
    // A sign-in is not a percentage, so `progress` stays absent.
    assert!(!seq.contains(":progress="), "got {seq}");
}

#[test]
fn closing_the_login_modal_clears_its_record() {
    let mut app = test_app();
    app.login = Some(LoginModal::test_modal("openai", "work"));
    assert!(
        record_for(&app, RecordKey::Login).is_some(),
        "an open modal publishes the login record"
    );
    app.term_status_dirty = false;

    // Cancelling drops the modal, which drops the record from the desired set —
    // the publisher's `sync` then emits `state=clear:id=login` for it.
    assert!(app.cancel_login());
    assert!(
        app.term_status_dirty,
        "closing the modal must re-publish the records so the record is cleared"
    );
    assert!(
        record_for(&app, RecordKey::Login).is_none(),
        "a closed modal publishes no login record"
    );
}

#[test]
fn window_title_reflects_the_attached_session() {
    let mut app = test_app();
    assert_eq!(app.window_title(), "Choreographr");

    app.attached_session_id = Some(1);
    app.session_mgr.all = vec![make_session(1, "Fix the parser")];
    assert_eq!(app.window_title(), "Choreographr - Fix the parser");
}
