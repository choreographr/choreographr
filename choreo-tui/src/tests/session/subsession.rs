use crate::connection::handle_daemon_message;
use crate::state::*;
use crate::tests::support::{make_session_full, test_app};
use choreo_client_core::TurnEventHandler;
use choreo_proto::{
    ClientMessageType, DaemonMessage, DaemonMessageType, SessionEvent, SessionStatus, TokenUsage,
};

// ── Sub-session finish auto-switches back to the parent ─────────────

/// Build a session summary for a running sub-session: same fields as
/// `make_session_full`, plus a parent id and an active (streaming) status.
fn make_subsession(id: u64, title: &str, parent_id: u64) -> choreo_proto::SessionSummary {
    let mut s = make_session_full(id, title, "m1", 0);
    s.parent_session_id = Some(parent_id);
    s.status = choreo_proto::SessionStatus::Responding;
    s
}

#[test]
fn attached_subsession_finished_detects_active_to_idle_only() {
    let mut app = test_app();
    app.attached_session_id = Some(99);
    app.active_session_id = Some(99);
    app.session_mgr.set_sessions(vec![
        make_session_full(42, "parent session", "m1", 3),
        make_subsession(99, "child task", 42),
    ]);

    // Viewing the running sub-session on the Chat page: an active → idle
    // transition means it finished — report the parent id.
    assert_eq!(
        app.attached_subsession_finished(99, &SessionStatus::Inactive),
        Some(42)
    );
    assert_eq!(
        app.attached_subsession_finished(99, &SessionStatus::Sleeping),
        Some(42)
    );
    // The new status must be idle: a same-status re-broadcast of an active
    // status is not a finish.
    assert_eq!(
        app.attached_subsession_finished(99, &SessionStatus::Responding),
        None
    );

    // Idle → idle (the child already finished; this is a duplicate or a
    // summary refresh) must not re-fire.
    app.handle_session_status_changed(99, &SessionStatus::Inactive, 1_705_315_000_000);
    assert_eq!(
        app.attached_subsession_finished(99, &SessionStatus::Inactive),
        None,
        "idle → idle must not be treated as a finish"
    );

    // Not attached / different attached session: no switch.
    app.attached_session_id = Some(7);
    assert_eq!(
        app.attached_subsession_finished(99, &SessionStatus::Inactive),
        None
    );
    app.attached_session_id = Some(99);

    // Browsing the Session Manager page: no auto-jump away from it.
    app.page = Page::SessionManager;
    assert_eq!(
        app.attached_subsession_finished(99, &SessionStatus::Inactive),
        None
    );
    app.page = Page::Chat;

    // A top-level session (no parent) is not a sub-session: no switch.
    app.attached_session_id = Some(42);
    assert_eq!(
        app.attached_subsession_finished(42, &SessionStatus::Inactive),
        None
    );

    // The parent no longer exists in the summary (deleted while the child
    // ran): switching would attach to a dead session id — no switch.
    app.attached_session_id = Some(99);
    app.session_mgr
        .set_sessions(vec![make_subsession(99, "orphan task", 42)]);
    assert_eq!(
        app.attached_subsession_finished(99, &SessionStatus::Inactive),
        None
    );
}

#[test]
fn subsession_finish_switches_back_to_parent_with_notification() {
    let mut app = test_app();
    let (tx, rx) = crossbeam_channel::unbounded();
    // The user opened the sub-session 99 from the Session Manager and is now
    // reading it on the Chat page while it streams.
    app.attached_session_id = Some(99);
    app.active_session_id = Some(99);
    app.session_mgr.set_sessions(vec![
        make_session_full(42, "parent session", "m1", 3),
        make_subsession(99, "child task", 42),
    ]);

    handle_daemon_message(
        DaemonMessage::broadcast(DaemonMessageType::Session {
            session_id: Some(99),
            event: SessionEvent::SessionStatusChanged {
                status: SessionStatus::Inactive,
                last_modified: 1_705_315_000_000,
            },
        }),
        &mut app,
        &tx,
    )
    .expect("handle SessionStatusChanged");

    // The view jumped back to the parent session.
    assert_eq!(app.attached_session_id, Some(42));
    assert_eq!(app.active_session_id, Some(42));
    // The daemon was asked to attach to the parent, mirroring the Session
    // Manager Enter path (summary unsubscription first, then attach).
    let msgs: Vec<ClientMessageType> = rx.try_iter().map(|m| m.inner).collect();
    assert!(
        msgs.iter().any(|m| matches!(
            m,
            ClientMessageType::AttachSession { session_id } if *session_id == 42
        )),
        "must attach to the parent session"
    );
    assert!(
        msgs.iter()
            .any(|m| matches!(m, ClientMessageType::UnsubscribeSessionsSummary)),
        "must unsubscribe from the sessions summary like other attach paths"
    );
    // The notification names both sessions.
    assert_eq!(
        app.status(),
        Some("Subsession \"child task\" finished. Switched back to parent \"parent session\".")
    );
    // The status bar immediately reflects the parent's (idle) summary status
    // instead of holding the child's just-applied "inactive" until the
    // daemon's SessionAttached reply lands.
    assert_eq!(app.attached_status, Some(SessionStatus::Inactive));
}

#[test]
fn subsession_finish_does_not_fire_on_duplicate_idle_broadcast() {
    let mut app = test_app();
    let (tx, rx) = crossbeam_channel::unbounded();
    // The child already finished before the user opened it; its summary is
    // idle.  A re-broadcast of the idle status (summary refresh / re-attach)
    // must not yank the view back to the parent.
    app.attached_session_id = Some(99);
    app.active_session_id = Some(99);
    app.session_mgr.set_sessions(vec![
        make_session_full(42, "parent session", "m1", 3),
        make_subsession(99, "child task", 42),
    ]);
    app.handle_session_status_changed(99, &SessionStatus::Inactive, 1_705_315_000_000);

    handle_daemon_message(
        DaemonMessage::broadcast(DaemonMessageType::Session {
            session_id: Some(99),
            event: SessionEvent::SessionStatusChanged {
                status: SessionStatus::Inactive,
                last_modified: 1_705_315_000_000,
            },
        }),
        &mut app,
        &tx,
    )
    .expect("handle SessionStatusChanged");

    assert_eq!(app.attached_session_id, Some(99));
    assert_eq!(app.status(), None);
    assert!(
        rx.try_iter().next().is_none(),
        "no attach may be sent for an idle → idle broadcast"
    );
}

#[test]
fn top_level_session_finish_does_not_switch() {
    let mut app = test_app();
    let (tx, rx) = crossbeam_channel::unbounded();
    app.attached_session_id = Some(42);
    app.active_session_id = Some(42);
    app.session_mgr
        .set_sessions(vec![make_session_full(42, "my session", "m1", 3)]);

    handle_daemon_message(
        DaemonMessage::broadcast(DaemonMessageType::Session {
            session_id: Some(42),
            event: SessionEvent::SessionStatusChanged {
                status: SessionStatus::Inactive,
                last_modified: 1_705_315_000_000,
            },
        }),
        &mut app,
        &tx,
    )
    .expect("handle SessionStatusChanged");

    // A top-level session finishing is normal lifecycle — no switch, no
    // notification, no messages.
    assert_eq!(app.attached_session_id, Some(42));
    assert_eq!(app.status(), None);
    assert!(rx.try_iter().next().is_none());
}

#[test]
fn subsession_finish_with_missing_parent_does_not_switch() {
    let mut app = test_app();
    let (tx, rx) = crossbeam_channel::unbounded();
    // The parent was deleted while the child ran; the summary only holds the
    // orphaned child.  The switch-back must not fire — attaching to a dead
    // session id would strand the user on a session the daemon rejects.
    app.attached_session_id = Some(99);
    app.active_session_id = Some(99);
    app.session_mgr
        .set_sessions(vec![make_subsession(99, "orphan task", 42)]);

    handle_daemon_message(
        DaemonMessage::broadcast(DaemonMessageType::Session {
            session_id: Some(99),
            event: SessionEvent::SessionStatusChanged {
                status: SessionStatus::Inactive,
                last_modified: 1_705_315_000_000,
            },
        }),
        &mut app,
        &tx,
    )
    .expect("handle SessionStatusChanged");

    // The view stays on the finished child; no attach is attempted.
    assert_eq!(app.attached_session_id, Some(99));
    assert_eq!(app.active_session_id, Some(99));
    assert_eq!(app.status(), None);
    assert!(
        rx.try_iter().next().is_none(),
        "no attach may be sent when the parent is missing from the summary"
    );
}

#[test]
fn session_attached_does_not_regress_accumulated_live_state() {
    let mut app = test_app();
    let (tx, _rx) = crossbeam_channel::unbounded();
    // The user is viewing session 42 and its display already accumulated live
    // state via the all-activity subscription while it was in the background:
    // fresher per-turn token usage plus a live streaming count.
    app.attached_session_id = Some(42);
    app.active_session_id = Some(42);
    let display = app.display_for(42);
    display.token_usage = Some(TokenUsage {
        input_tokens: 50,
        output_tokens: 60,
        total_tokens: 110,
        cached_tokens: 0,
        cache_write_tokens: 0,
    });
    display.live_output_tokens = 5;
    display.selected_model = Some("gpt-live".to_string());

    // The session summary (refreshed on ListSessions, potentially stale) has
    // older values for the same fields.
    app.session_mgr
        .set_sessions(vec![make_session_full(42, "a", "gpt-stale", 0)]);
    {
        let s = app.session_mgr.all.first_mut().unwrap();
        s.token_usage = Some(TokenUsage {
            input_tokens: 1,
            output_tokens: 2,
            total_tokens: 3,
            cached_tokens: 0,
            cache_write_tokens: 0,
        });
        s.context_window = Some(4096);
    }

    handle_daemon_message(
        DaemonMessage::broadcast(DaemonMessageType::Session {
            session_id: Some(42),
            event: SessionEvent::SessionAttached,
        }),
        &mut app,
        &tx,
    )
    .expect("handle SessionAttached");

    // The accumulated (fresher) display state wins — the stale summary must
    // not regress the status bar's token readout or model right after switch.
    assert_eq!(
        app.display_for(42).token_usage,
        Some(TokenUsage {
            input_tokens: 50,
            output_tokens: 60,
            total_tokens: 110,
            cached_tokens: 0,
            cache_write_tokens: 0,
        })
    );
    assert_eq!(app.display_for(42).live_output_tokens, 5);
    assert_eq!(
        app.display_for(42).selected_model.as_deref(),
        Some("gpt-live")
    );
    // Fields the display never accumulated come from the summary.
    assert_eq!(app.display_for(42).context_window, Some(4096));
}

#[test]
fn handle_sessions_auto_attach_prefers_top_level_session() {
    let (tx, rx) = crossbeam_channel::unbounded();
    // Fresh App: nothing attached yet, on the chat page.
    let mut app = App::new();
    app.image_job_tx = None;

    // Session 7 is a sub-session (parent_session_id = Some) and is the most
    // recently modified (its requests completed while the parent's agent
    // worked, each bumping last_modified).
    // Session 3 is the most recently modified top-level session.
    let mut child = make_session_full(7, "sub", "m", 0);
    child.parent_session_id = Some(3);
    child.last_modified = 3000;
    let mut top = make_session_full(3, "main", "m", 0);
    top.last_modified = 2000;
    let mut older = make_session_full(1, "old", "m", 0);
    older.last_modified = 1000;

    app.handle_sessions(&[child.clone(), top.clone(), older.clone()], &tx)
        .expect("handle_sessions should succeed");

    // The auto-attach must pick the top-level session, not the sub-session
    // that happens to be the most recently modified.
    let msg = rx.recv().expect("auto-attach message");
    assert_eq!(
        msg.inner,
        ClientMessageType::AttachSession { session_id: 3 }
    );
    assert_eq!(app.attached_session_id, Some(3));
    assert_eq!(app.active_session_id, Some(3));

    // A second Sessions reply in the same tick must not re-fire: attachment
    // state was set locally, so the guard skips the auto-attach.
    let (tx2, rx2) = crossbeam_channel::unbounded();
    app.handle_sessions(&[child.clone(), top.clone(), older.clone()], &tx2)
        .expect("handle_sessions should succeed");
    assert!(
        rx2.try_recv().is_err(),
        "auto-attach must not fire twice for the same startup tick"
    );
}

#[test]
fn handle_sessions_auto_attach_falls_back_to_child_when_no_top_level() {
    let mut app = App::new();
    app.image_job_tx = None;
    let (tx, rx) = crossbeam_channel::unbounded();

    let mut child = make_session_full(7, "sub", "m", 0);
    child.parent_session_id = Some(3);
    app.handle_sessions(&[child], &tx)
        .expect("handle_sessions should succeed");

    // With no top-level session at all, fall back to the most recent session.
    let msg = rx.recv().expect("auto-attach message");
    assert_eq!(
        msg.inner,
        ClientMessageType::AttachSession { session_id: 7 }
    );
}

#[test]
fn handle_sessions_empty_creates_default_session_without_working_dir() {
    let mut app = App::new();
    app.image_job_tx = None;
    let (tx, rx) = crossbeam_channel::unbounded();

    // The daemon reports no sessions, so the bootstrap auto-creates a
    // "default" session with no working directory: the TUI's own cwd must
    // not be forwarded, because the daemon may be remote (serving a client
    // over TCP) and that cwd would be meaningless on the daemon host.
    app.handle_sessions(&[], &tx)
        .expect("handle_sessions should succeed");

    let msg = rx.recv().expect("CreateSession message");
    assert_eq!(
        msg.inner,
        ClientMessageType::CreateSession {
            title: Some("default".into()),
            parent_session_id: None,
            working_dir: None,
            context_config: None,
            account_name: app.ai_providers.accounts.first().map(|a| a.name.clone()),
            selected_model: None,
            reasoning_effort: None,
        }
    );
}

#[test]
fn handle_sessions_auto_attach_skips_archived_sessions() {
    // An archived session is hidden from the live list, so it must not be
    // auto-attached on the Chat page either. The archived session here is the
    // most recent (and pinned), so it sorts FIRST — yet the auto-attach must
    // still pick the live session.
    let mut app = App::new();
    app.image_job_tx = None;
    let (tx, rx) = crossbeam_channel::unbounded();

    let mut archived = make_session_full(5, "archived", "m", 0);
    archived.archived_at = Some(1_705_314_000_500);
    archived.pinned = true;
    archived.last_modified = 2_000_000_000_000; // newest → sorts first
    let live = make_session_full(3, "live", "m", 0);

    app.handle_sessions(&[archived, live], &tx)
        .expect("handle_sessions should succeed");

    let msg = rx.recv().expect("auto-attach message");
    assert_eq!(
        msg.inner,
        ClientMessageType::AttachSession { session_id: 3 }
    );
    assert_eq!(app.attached_session_id, Some(3));
}

#[test]
fn handle_sessions_only_archived_creates_default_instead_of_attaching() {
    // Every session is archived: there is no live session to open, so the
    // bootstrap creates a fresh default rather than silently attaching to the
    // archived one.
    let mut app = App::new();
    app.image_job_tx = None;
    let (tx, rx) = crossbeam_channel::unbounded();

    let mut archived = make_session_full(5, "archived", "m", 0);
    archived.archived_at = Some(1_705_314_000_500);

    app.handle_sessions(&[archived], &tx)
        .expect("handle_sessions should succeed");

    let msg = rx.recv().expect("CreateSession message");
    assert!(matches!(msg.inner, ClientMessageType::CreateSession { .. }));
    assert_eq!(app.attached_session_id, None);
}

// ── Auto-navigate into a spawned sub-session ───────────────────────

/// A broadcast `ToolCallStarted` for the given session and tool name.
fn tool_call_started(session_id: u64, call_id: &str, tool_name: &str) -> DaemonMessage {
    DaemonMessage::broadcast(DaemonMessageType::Session {
        session_id: Some(session_id),
        event: SessionEvent::ToolCallStarted {
            stream_id: 1,
            call_id: call_id.to_string(),
            tool_name: tool_name.to_string(),
            arguments_json: "{}".to_string(),
            invocation_description: String::new(),
        },
    })
}

/// A broadcast `ToolCallFinished` for the given session and tool name.
fn tool_call_finished(session_id: u64, call_id: &str, tool_name: &str) -> DaemonMessage {
    DaemonMessage::broadcast(DaemonMessageType::Session {
        session_id: Some(session_id),
        event: SessionEvent::ToolCallFinished {
            stream_id: 1,
            call_id: call_id.to_string(),
            tool_name: tool_name.to_string(),
        },
    })
}

/// A broadcast `SessionCreated` for a (optionally sub-) session.
fn session_created(child: u64, parent: Option<u64>) -> DaemonMessage {
    DaemonMessage::broadcast(DaemonMessageType::Session {
        session_id: Some(child),
        event: SessionEvent::SessionCreated {
            title: Some("child task".into()),
            parent_session_id: parent,
            working_dir: None,
            account_name: None,
            selected_model: None,
            reasoning_effort: None,
        },
    })
}

/// A broadcast `SessionStatusChanged` for the given session.
fn status_changed(session_id: u64, status: SessionStatus) -> DaemonMessage {
    DaemonMessage::broadcast(DaemonMessageType::Session {
        session_id: Some(session_id),
        event: SessionEvent::SessionStatusChanged {
            status,
            last_modified: 1_705_315_000_000,
        },
    })
}

#[test]
fn single_inflight_spawn_navigates_into_child() {
    let mut app = test_app();
    let (tx, rx) = crossbeam_channel::unbounded();
    // The user is reading session 42 on the Chat page.
    app.attached_session_id = Some(42);
    app.active_session_id = Some(42);
    app.page = Page::Chat;

    // Session 42 starts one spawn_subsession call...
    handle_daemon_message(
        tool_call_started(42, "call-1", "spawn_subsession"),
        &mut app,
        &tx,
    )
    .expect("tool call started");
    assert_eq!(app.spawn_subsession_inflight_count(42), 1);

    // ...and the daemon broadcasts the child's creation with parent = 42.
    handle_daemon_message(session_created(99, Some(42)), &mut app, &tx).expect("session created");

    // The view entered the child, and the summary-independent round trip is
    // recorded from the creation event's authoritative ids.
    assert_eq!(app.attached_session_id, Some(99));
    assert_eq!(app.active_session_id, Some(99));
    let nav = app.spawned_child_nav.as_ref().expect("nav record recorded");
    assert_eq!((nav.child_id, nav.parent_id), (99, 42));
    assert!(!nav.seen_active, "no active status seen yet");

    let msgs: Vec<ClientMessageType> = rx.try_iter().map(|m| m.inner).collect();
    assert!(
        msgs.iter().any(
            |m| matches!(m, ClientMessageType::AttachSession { session_id } if *session_id == 99)
        ),
        "must attach to the spawned child"
    );
}

#[test]
fn parallel_spawns_do_not_navigate() {
    let mut app = test_app();
    let (tx, _rx) = crossbeam_channel::unbounded();
    app.attached_session_id = Some(42);
    app.active_session_id = Some(42);
    app.page = Page::Chat;

    // Two spawn_subsession calls are in flight in one turn.
    handle_daemon_message(
        tool_call_started(42, "c1", "spawn_subsession"),
        &mut app,
        &tx,
    )
    .unwrap();
    handle_daemon_message(
        tool_call_started(42, "c2", "spawn_subsession"),
        &mut app,
        &tx,
    )
    .unwrap();
    assert_eq!(app.spawn_subsession_inflight_count(42), 2);

    handle_daemon_message(session_created(99, Some(42)), &mut app, &tx).unwrap();

    // Ambiguous — do not thrash the view; the child just joins the list.
    assert_eq!(app.attached_session_id, Some(42));
    assert!(app.spawned_child_nav.is_none());
}

#[test]
fn spawn_from_background_session_does_not_navigate() {
    let mut app = test_app();
    let (tx, _rx) = crossbeam_channel::unbounded();
    // The user is reading session 7; a *different* (background) session 42
    // spawns a child.
    app.attached_session_id = Some(7);
    app.active_session_id = Some(7);
    app.page = Page::Chat;

    handle_daemon_message(
        tool_call_started(42, "c1", "spawn_subsession"),
        &mut app,
        &tx,
    )
    .unwrap();
    handle_daemon_message(session_created(99, Some(42)), &mut app, &tx).unwrap();

    // Parent 42 is not the attached session: no navigation.
    assert_eq!(app.attached_session_id, Some(7));
    assert!(app.spawned_child_nav.is_none());
}

#[test]
fn foreign_sub_session_creation_without_inflight_spawn_does_not_navigate() {
    // A sub-session created by another client (no local spawn_subsession call)
    // must never hijack the view, even though its parent is the attached
    // session.
    let mut app = test_app();
    let (tx, rx) = crossbeam_channel::unbounded();
    app.attached_session_id = Some(42);
    app.active_session_id = Some(42);
    app.page = Page::Chat;

    handle_daemon_message(session_created(99, Some(42)), &mut app, &tx).unwrap();

    assert_eq!(app.attached_session_id, Some(42));
    assert!(app.spawned_child_nav.is_none());
    let msgs: Vec<ClientMessageType> = rx.try_iter().map(|m| m.inner).collect();
    assert!(
        !msgs
            .iter()
            .any(|m| matches!(m, ClientMessageType::AttachSession { .. })),
        "no attach may be sent without an in-flight spawn"
    );
}

#[test]
fn spawned_child_finish_switches_back_to_parent() {
    let mut app = test_app();
    let (tx, _rx) = crossbeam_channel::unbounded();
    app.attached_session_id = Some(42);
    app.active_session_id = Some(42);
    app.page = Page::Chat;

    // Spawn a single child and enter it.
    handle_daemon_message(
        tool_call_started(42, "c1", "spawn_subsession"),
        &mut app,
        &tx,
    )
    .unwrap();
    handle_daemon_message(session_created(99, Some(42)), &mut app, &tx).unwrap();
    assert_eq!(app.attached_session_id, Some(99));

    // The creation-time Inactive broadcast arrives right after creation and
    // must NOT bounce the view back (the child has not run yet).
    handle_daemon_message(status_changed(99, SessionStatus::Inactive), &mut app, &tx).unwrap();
    assert_eq!(
        app.attached_session_id,
        Some(99),
        "creation-time idle must not switch back"
    );

    // The child runs, then finishes → switch back to the parent and consume the
    // round trip.
    handle_daemon_message(status_changed(99, SessionStatus::Responding), &mut app, &tx).unwrap();
    assert_eq!(app.attached_session_id, Some(99));
    assert!(
        app.spawned_child_nav
            .as_ref()
            .is_some_and(|n| n.seen_active)
    );

    handle_daemon_message(status_changed(99, SessionStatus::Inactive), &mut app, &tx).unwrap();
    assert_eq!(app.attached_session_id, Some(42));
    assert!(app.spawned_child_nav.is_none(), "record consumed on finish");
}

#[test]
fn tool_finish_clears_inflight_spawn_count() {
    let mut app = test_app();
    let (tx, _rx) = crossbeam_channel::unbounded();
    app.attached_session_id = Some(42);
    app.active_session_id = Some(42);
    app.page = Page::Chat;

    handle_daemon_message(
        tool_call_started(42, "c1", "spawn_subsession"),
        &mut app,
        &tx,
    )
    .unwrap();
    assert_eq!(app.spawn_subsession_inflight_count(42), 1);
    handle_daemon_message(
        tool_call_finished(42, "c1", "spawn_subsession"),
        &mut app,
        &tx,
    )
    .unwrap();
    assert_eq!(
        app.spawn_subsession_inflight_count(42),
        0,
        "a finished spawn call is no longer in flight"
    );
}

#[test]
fn non_spawn_tool_calls_do_not_affect_inflight_count() {
    let mut app = test_app();
    let (tx, _rx) = crossbeam_channel::unbounded();
    app.attached_session_id = Some(42);
    app.active_session_id = Some(42);
    app.page = Page::Chat;

    handle_daemon_message(tool_call_started(42, "c1", "read_file"), &mut app, &tx).unwrap();
    assert_eq!(app.spawn_subsession_inflight_count(42), 0);
}

#[test]
fn spawned_child_tool_finish_switches_back_without_an_active_status() {
    // A child that finishes before the TUI ever observes it active emits no
    // active→idle status, so the status path alone would never switch back. The
    // parent's blocking `spawn_subsession` tool returning is the definitive
    // completion signal and closes that gap.
    let mut app = test_app();
    let (tx, _rx) = crossbeam_channel::unbounded();
    app.attached_session_id = Some(42);
    app.active_session_id = Some(42);
    app.page = Page::Chat;

    handle_daemon_message(
        tool_call_started(42, "c1", "spawn_subsession"),
        &mut app,
        &tx,
    )
    .unwrap();
    handle_daemon_message(session_created(99, Some(42)), &mut app, &tx).unwrap();
    assert_eq!(app.attached_session_id, Some(99));

    handle_daemon_message(
        tool_call_finished(42, "c1", "spawn_subsession"),
        &mut app,
        &tx,
    )
    .unwrap();

    assert_eq!(
        app.attached_session_id,
        Some(42),
        "a blocking spawn completing switches back even with no active status"
    );
    assert!(app.spawned_child_nav.is_none(), "the record is consumed");
    assert_eq!(
        app.spawn_subsession_inflight_count(42),
        0,
        "the terminal event decrements the tally"
    );
}

#[test]
fn status_finish_then_tool_finish_does_not_double_switch() {
    let mut app = test_app();
    let (tx, _rx) = crossbeam_channel::unbounded();
    app.attached_session_id = Some(42);
    app.active_session_id = Some(42);
    app.page = Page::Chat;

    handle_daemon_message(
        tool_call_started(42, "c1", "spawn_subsession"),
        &mut app,
        &tx,
    )
    .unwrap();
    handle_daemon_message(session_created(99, Some(42)), &mut app, &tx).unwrap();
    // The normal path: the child runs, then its idle status switches back and
    // consumes the record.
    handle_daemon_message(status_changed(99, SessionStatus::Responding), &mut app, &tx).unwrap();
    handle_daemon_message(status_changed(99, SessionStatus::Inactive), &mut app, &tx).unwrap();
    assert_eq!(app.attached_session_id, Some(42));

    // The tool finishing afterwards must not move the view a second time.
    handle_daemon_message(
        tool_call_finished(42, "c1", "spawn_subsession"),
        &mut app,
        &tx,
    )
    .unwrap();
    assert_eq!(app.attached_session_id, Some(42));
    assert!(app.spawned_child_nav.is_none());
}

#[test]
fn deleting_the_parent_clears_the_nav_record_and_tally() {
    // A deleted session cannot still be running a blocking spawn, and a round
    // trip through it can never complete: both must be dropped, not linger to
    // poison a later spawn's "exactly one in flight" gate.
    let mut app = test_app();
    let (tx, _rx) = crossbeam_channel::unbounded();
    app.attached_session_id = Some(42);
    app.active_session_id = Some(42);
    app.page = Page::Chat;

    handle_daemon_message(
        tool_call_started(42, "c1", "spawn_subsession"),
        &mut app,
        &tx,
    )
    .unwrap();
    handle_daemon_message(session_created(99, Some(42)), &mut app, &tx).unwrap();
    assert_eq!(app.spawn_subsession_inflight_count(42), 1);
    assert!(app.spawned_child_nav.is_some());

    app.handle_session_deleted(42);

    assert_eq!(
        app.spawn_subsession_inflight_count(42),
        0,
        "the tally is reconciled on delete"
    );
    assert!(
        app.spawned_child_nav.is_none(),
        "the nav record is dropped on delete"
    );
}

#[test]
fn a_request_ending_clears_a_stale_spawn_tally() {
    let mut app = test_app();
    let (tx, _rx) = crossbeam_channel::unbounded();
    app.attached_session_id = Some(42);
    app.active_session_id = Some(42);
    app.page = Page::Chat;

    // A spawn starts but its terminal event is lost; the request then ends. The
    // request-end teardown must clear the stale tally so a later genuine spawn
    // still reads as exactly one in flight.
    handle_daemon_message(
        tool_call_started(42, "c1", "spawn_subsession"),
        &mut app,
        &tx,
    )
    .unwrap();
    assert_eq!(app.spawn_subsession_inflight_count(42), 1);

    app.handle_cancelled(Some(42), 1);
    assert_eq!(
        app.spawn_subsession_inflight_count(42),
        0,
        "a request ending clears the tally"
    );
}
