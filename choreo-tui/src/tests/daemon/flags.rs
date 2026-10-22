use crate::connection::handle_daemon_message;
use crate::state::*;
use crate::tests::support::{make_session_full, test_app};
use choreo_proto::{DaemonMessage, DaemonMessageType, SessionEvent};

// ── per-session pin/archive flags (SessionFlagsChanged) ──

#[test]
fn session_flags_changed_updates_list_and_archived_split() {
    let mut app = test_app();
    let (tx, _rx) = crossbeam_channel::unbounded();
    app.session_mgr.set_sessions(vec![
        make_session_full(1, "a", "m", 0),
        make_session_full(2, "b", "m", 0),
    ]);
    // The live view holds both sessions.
    assert_eq!(app.session_mgr.sessions.len(), 2);

    // The daemon broadcasts a pin for session 1.
    handle_daemon_message(
        DaemonMessage::broadcast(DaemonMessageType::Session {
            session_id: Some(1),
            event: SessionEvent::SessionFlagsChanged {
                pinned: true,
                archived_at: None,
            },
        }),
        &mut app,
        &tx,
    )
    .expect("handle SessionFlagsChanged");
    // Pinned first, flag applied.
    assert_eq!(app.session_mgr.sessions[0].session_id, 1);
    assert!(app.session_mgr.sessions[0].pinned);

    // The daemon broadcasts an archive for session 2.
    handle_daemon_message(
        DaemonMessage::broadcast(DaemonMessageType::Session {
            session_id: Some(2),
            event: SessionEvent::SessionFlagsChanged {
                pinned: false,
                archived_at: Some(1_705_314_000_500),
            },
        }),
        &mut app,
        &tx,
    )
    .expect("handle SessionFlagsChanged");
    // Session 2 left the live list…
    assert_eq!(
        app.session_mgr
            .sessions
            .iter()
            .map(|s| s.session_id)
            .collect::<Vec<_>>(),
        vec![1]
    );
    // …and appears in the archived list.
    app.session_mgr.toggle_view();
    assert_eq!(
        app.session_mgr
            .sessions
            .iter()
            .map(|s| s.session_id)
            .collect::<Vec<_>>(),
        vec![2]
    );
}

#[test]
fn session_failed_for_pin_and_archive_sets_page_error() {
    let mut app = test_app();
    app.page = Page::SessionManager;
    let (tx, _rx) = crossbeam_channel::unbounded();
    assert!(app.session_mgr.error.get().is_none());

    handle_daemon_message(
        DaemonMessage::broadcast(DaemonMessageType::Session {
            session_id: Some(1),
            event: SessionEvent::SessionFailed {
                operation: "set_session_pinned".into(),
                error: "boom".into(),
            },
        }),
        &mut app,
        &tx,
    )
    .expect("handle SessionFailed");
    assert_eq!(app.session_mgr.error.get(), Some("boom"));
    // The generic dispatch overwrites `app.error` with the raw daemon error
    // text after this arm; the page-local error is the useful signal here.
    assert_eq!(app.error(), Some("boom"));

    app.session_mgr.error.clear();
    handle_daemon_message(
        DaemonMessage::broadcast(DaemonMessageType::Session {
            session_id: Some(1),
            event: SessionEvent::SessionFailed {
                operation: "set_session_archived".into(),
                error: "nope".into(),
            },
        }),
        &mut app,
        &tx,
    )
    .expect("handle SessionFailed");
    assert_eq!(app.session_mgr.error.get(), Some("nope"));
}
