use crate::connection::{handle_daemon_message, handle_terminal_event};
use crate::tests::support::{make_session, test_app};
use choreo_proto::{
    ClientMessageType, DaemonMessage, DaemonMessageType, ReasoningCapability, SessionEvent,
};
use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};

// ── Alt+R reasoning effort cycling ──────────────────────────────

#[test]
fn ctrl_r_no_session_shows_message() {
    let mut app = test_app();
    let (tx, _rx) = crossbeam_channel::unbounded();

    app.attached_session_id = None;
    app.display_for(0).reasoning_capability = Some(ReasoningCapability {
        available_effort_levels: vec![
            "off".to_string(),
            "low".to_string(),
            "medium".to_string(),
            "high".to_string(),
        ],
    });

    // Alt+R should show message even without session attached
    // (the handler checks capability, not session_id).
    handle_terminal_event(
        Event::Key(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::ALT)),
        &mut app,
        &tx,
    )
    .expect("handle alt+r");

    assert_eq!(app.display_for(0).reasoning_effort.as_deref(), Some("low"));
    assert_eq!(app.status(), Some("reasoning: low"));
}

#[test]
fn ctrl_r_no_active_display_shows_message() {
    let mut app = test_app();
    let (tx, rx) = crossbeam_channel::unbounded();

    // No session attached at all: there is no display whose capability
    // could be consulted, so fall back to the established no-session
    // wording used by the other session-bound shortcuts.
    app.active_session_id = None;
    app.attached_session_id = None;

    handle_terminal_event(
        Event::Key(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::ALT)),
        &mut app,
        &tx,
    )
    .expect("handle alt+r");

    assert_eq!(app.status(), Some("no session attached"));
    assert!(rx.try_iter().next().is_none(), "no client message expected");
}

#[test]
fn ctrl_r_cycles_through_valid_slugs() {
    let mut app = test_app();
    let (tx, rx) = crossbeam_channel::unbounded();

    app.display_for(0).reasoning_capability = Some(ReasoningCapability {
        available_effort_levels: vec![
            "off".to_string(),
            "low".to_string(),
            "medium".to_string(),
            "high".to_string(),
        ],
    });

    // First Alt+R: off -> low
    handle_terminal_event(
        Event::Key(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::ALT)),
        &mut app,
        &tx,
    )
    .expect("handle alt+r 1");
    assert_eq!(app.display_for(0).reasoning_effort.as_deref(), Some("low"));
    assert_eq!(app.status(), Some("reasoning: low"));
    let msg = rx.recv().expect("SetReasoningEffort 1");
    assert_eq!(
        msg.inner,
        ClientMessageType::SetReasoningEffort {
            effort: "low".to_string()
        }
    );

    // Second Alt+R: low -> medium
    handle_terminal_event(
        Event::Key(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::ALT)),
        &mut app,
        &tx,
    )
    .expect("handle alt+r 2");
    assert_eq!(
        app.display_for(0).reasoning_effort.as_deref(),
        Some("medium")
    );
    let msg = rx.recv().expect("SetReasoningEffort 2");
    assert_eq!(
        msg.inner,
        ClientMessageType::SetReasoningEffort {
            effort: "medium".to_string()
        }
    );

    // Third: medium -> high
    handle_terminal_event(
        Event::Key(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::ALT)),
        &mut app,
        &tx,
    )
    .expect("handle alt+r 3");
    assert_eq!(app.display_for(0).reasoning_effort.as_deref(), Some("high"));
    let msg = rx.recv().expect("SetReasoningEffort 3");
    assert_eq!(
        msg.inner,
        ClientMessageType::SetReasoningEffort {
            effort: "high".to_string()
        }
    );

    // Fourth: high -> off (wraps around)
    handle_terminal_event(
        Event::Key(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::ALT)),
        &mut app,
        &tx,
    )
    .expect("handle alt+r 4");
    assert_eq!(app.display_for(0).reasoning_effort.as_deref(), Some("off"));
    let msg = rx.recv().expect("SetReasoningEffort 4");
    assert_eq!(
        msg.inner,
        ClientMessageType::SetReasoningEffort {
            effort: "off".to_string()
        }
    );
}

#[test]
fn ctrl_r_no_model_selected_shows_message() {
    let mut app = test_app();
    let (tx, rx) = crossbeam_channel::unbounded();

    // No model selected yet and no capability reported.  `None` here must
    // NOT be reported as "model does not support reasoning" — the user
    // simply has nothing to cycle yet.
    app.display_for(0).reasoning_capability = None;
    app.display_for(0).selected_model = None;

    handle_terminal_event(
        Event::Key(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::ALT)),
        &mut app,
        &tx,
    )
    .expect("handle alt+r");

    assert_eq!(
        app.status(),
        Some("no model selected — pick one with Alt+M")
    );
    // Effort should remain unchanged (still None) and nothing is sent to
    // the daemon (no cycling, so no SetReasoningEffort).
    assert!(app.display_for(0).reasoning_effort.is_none());
    assert!(rx.try_iter().next().is_none(), "no client message expected");
}

#[test]
fn ctrl_r_model_selected_capability_pending_shows_message() {
    let mut app = test_app();
    let (tx, rx) = crossbeam_channel::unbounded();

    // A model is selected but the daemon has not reported its reasoning
    // capability yet — must not be reported as "does not support reasoning".
    app.display_for(0).reasoning_capability = None;
    app.display_for(0).selected_model = Some("gpt-4o".to_string());

    handle_terminal_event(
        Event::Key(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::ALT)),
        &mut app,
        &tx,
    )
    .expect("handle alt+r");

    assert_eq!(app.status(), Some("reasoning capability not yet available"));
    // Effort should remain unchanged (still None) and nothing is sent to
    // the daemon (no cycling, so no SetReasoningEffort).
    assert!(app.display_for(0).reasoning_effort.is_none());
    assert!(rx.try_iter().next().is_none(), "no client message expected");
}

#[test]
fn ctrl_r_google_off_on() {
    let mut app = test_app();
    let (tx, rx) = crossbeam_channel::unbounded();

    // Google Gemini style: only "off" and "on".
    app.display_for(0).reasoning_capability = Some(ReasoningCapability {
        available_effort_levels: vec!["off".to_string(), "on".to_string()],
    });

    // First Alt+R: off -> on
    handle_terminal_event(
        Event::Key(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::ALT)),
        &mut app,
        &tx,
    )
    .expect("handle alt+r 1");
    assert_eq!(app.display_for(0).reasoning_effort.as_deref(), Some("on"));
    let msg = rx.recv().expect("SetReasoningEffort 1");
    assert_eq!(
        msg.inner,
        ClientMessageType::SetReasoningEffort {
            effort: "on".to_string()
        }
    );

    // Second Alt+R: on -> off (wraps)
    handle_terminal_event(
        Event::Key(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::ALT)),
        &mut app,
        &tx,
    )
    .expect("handle alt+r 2");
    assert_eq!(app.display_for(0).reasoning_effort.as_deref(), Some("off"));
    let msg = rx.recv().expect("SetReasoningEffort 2");
    assert_eq!(
        msg.inner,
        ClientMessageType::SetReasoningEffort {
            effort: "off".to_string()
        }
    );
}

#[test]
fn reasoning_effort_set_updates_session_state() {
    let mut app = test_app();
    let (tx, _rx) = crossbeam_channel::unbounded();

    app.attached_session_id = Some(42);

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

    // A connection-level ReasoningEffortSet reply (bare `/reasoning` without
    // an attachment) arrives with `session_id: None` — it must resolve to the
    // attached session's display rather than a phantom entry.
    assert_eq!(
        app.display_for(42).reasoning_effort.as_deref(),
        Some("high")
    );
    assert_eq!(
        app.display_for(0).reasoning_effort.as_deref(),
        None,
        "a connection-level reply must resolve to the attached session, not a phantom display"
    );
}

#[test]
fn reasoning_effort_set_for_background_session_does_not_touch_attached_display() {
    let mut app = test_app();
    let (tx, _rx) = crossbeam_channel::unbounded();
    // The user is viewing session 42.
    app.attached_session_id = Some(42);
    app.active_session_id = Some(42);
    // The display exists because the user attached to it (reset_for_session_switch
    // creates the entry).
    app.display_for(42).reasoning_effort = Some("off".to_string());

    // A background session (99, streamed via SubscribeAllActivity) reports a
    // reasoning-effort change.  The handler must route it to session 99's own
    // display — writing it to the active display would let a background
    // session's settings bleed into the status bar of the session being viewed.
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

    // The attached display must NOT pick up the background session's effort.
    assert_eq!(
        app.display_for(42).reasoning_effort.as_deref(),
        Some("off"),
        "attached display must not pick up a background session's reasoning effort"
    );
    // The background session's own display received the update.
    assert_eq!(
        app.display_for(99).reasoning_effort.as_deref(),
        Some("high")
    );
}

#[test]
fn model_selected_for_background_session_does_not_touch_attached_display() {
    let mut app = test_app();
    let (tx, _rx) = crossbeam_channel::unbounded();
    // The user is viewing session 42.
    app.attached_session_id = Some(42);
    app.active_session_id = Some(42);
    // The display exists because the user attached to it.
    app.display_for(42).selected_model = Some("gpt-main".to_string());

    // A background session (99) changes its model.  The active display must
    // not show the background session's model in its status bar.
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
        app.display_for(42).selected_model.as_deref(),
        Some("gpt-main"),
        "attached display must not pick up a background session's model"
    );
    assert_eq!(
        app.display_for(99).selected_model.as_deref(),
        Some("gpt-other")
    );
}

#[test]
fn model_selected_for_attached_session_updates_display_and_summary() {
    let mut app = test_app();
    let (tx, _rx) = crossbeam_channel::unbounded();
    // The user is viewing session 42, which is also the attached session.
    app.attached_session_id = Some(42);
    app.active_session_id = Some(42);
    app.display_for(42).selected_model = Some("gpt-old".to_string());
    // The summary the status bar's session-identity fields read from.
    app.session_mgr
        .set_sessions(vec![make_session(42, "a", "gpt-old", 0)]);

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

    // The attached session's own model change still updates the status bar's
    // identity fields (display + summary).
    assert_eq!(
        app.display_for(42).selected_model.as_deref(),
        Some("gpt-new")
    );
    assert_eq!(
        app.attached_session_mut()
            .unwrap()
            .selected_model
            .as_deref(),
        Some("gpt-new")
    );
}

#[test]
fn reasoning_effort_set_for_attached_session_updates_display_and_summary() {
    let mut app = test_app();
    let (tx, _rx) = crossbeam_channel::unbounded();
    app.attached_session_id = Some(42);
    app.active_session_id = Some(42);
    app.display_for(42).reasoning_effort = Some("off".to_string());
    app.session_mgr
        .set_sessions(vec![make_session(42, "a", "m", 0)]);

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
        app.display_for(42).reasoning_effort.as_deref(),
        Some("high")
    );
    assert_eq!(
        app.attached_session_mut()
            .unwrap()
            .reasoning_effort
            .as_deref(),
        Some("high")
    );
}

#[test]
fn session_account_set_for_background_session_does_not_touch_attached_display() {
    let mut app = test_app();
    let (tx, _rx) = crossbeam_channel::unbounded();
    app.attached_session_id = Some(42);
    app.active_session_id = Some(42);
    app.display_for(42).account_name = Some("main-account".to_string());

    // A background session (99) changes its account — the viewed session's
    // identity fields (account, provider slug) must not change.
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

    assert_eq!(
        app.display_for(42).account_name.as_deref(),
        Some("main-account"),
        "attached display must not pick up a background session's account"
    );
    assert_eq!(
        app.display_for(99).account_name.as_deref(),
        Some("bg-account")
    );
}

#[test]
fn reasoning_effort_set_failed_for_background_session_does_not_touch_attached_display() {
    let mut app = test_app();
    let (tx, _rx) = crossbeam_channel::unbounded();
    app.attached_session_id = Some(42);
    app.active_session_id = Some(42);
    app.display_for(42).reasoning_effort = Some("high".to_string());

    // A background session's rejection must not flip the viewed session's
    // reasoning-effort display back to "off".
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
        app.display_for(42).reasoning_effort.as_deref(),
        Some("high"),
        "attached display must not be reset by a background session's rejection"
    );
    assert_eq!(app.display_for(99).reasoning_effort.as_deref(), Some("off"));
}

#[test]
fn ctrl_r_with_empty_capability_shows_message() {
    let mut app = test_app();
    let (tx, _rx) = crossbeam_channel::unbounded();

    // Capability exists but has empty available_effort_levels.
    app.display_for(0).reasoning_capability = Some(ReasoningCapability {
        available_effort_levels: vec![],
    });

    handle_terminal_event(
        Event::Key(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::ALT)),
        &mut app,
        &tx,
    )
    .expect("handle alt+r");

    assert_eq!(app.status(), Some("model does not support reasoning"));
}
