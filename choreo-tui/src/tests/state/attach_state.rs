//! The client-side attach state: what this client knows about its own view of
//! the attached session, which the daemon's `SessionStatus` vocabulary cannot
//! express (`SessionNote`, `history_loaded`, `attach_failed`).
//!
//! Attaching is optimistic — the view rebinds to the session and the status bar
//! primes from the summary before the daemon answers — and the transcript only
//! arrives afterwards in one `SessionState` snapshot. These tests pin what the
//! client reports and draws in that window, and what it does when the attach
//! fails outright.

use crate::connection::handle_daemon_message;
use crate::state::*;
use crate::tests::support::{make_session, test_app};
use choreo_proto::{DaemonMessage, DaemonMessageType, SessionEvent, SessionStatus, Turn};

/// The harness's display anchor: `test_app` seeds a display for session `0` and
/// makes it the active session, so the tests here attach to it rather than
/// introducing a second anchor the render path would not look at.
const SESSION: u64 = 0;

fn turn(user_text: &str) -> Turn {
    Turn {
        created_at: choreo_proto::TimestampMs::now(),
        undone: false,
        error: None,
        user_text: Some(user_text.to_string()),
        assistant_text: Some("answer".to_string()),
        assistant_reasoning: None,
        tool_calls: vec![],
        token_usage: None,
        tool_results: vec![],
        displayed_images: vec![],
        reasoning_artifact: None,
        reasoning_producer: None,
    }
}

/// Apply one daemon message through the real handler pipeline.
fn receive(app: &mut App, message: DaemonMessage) {
    let (tx, _rx) = crossbeam_channel::unbounded();
    handle_daemon_message(message, app, &tx).expect("the handler accepts the message");
}

/// The daemon's attach snapshot for `session_id`, carrying `turns`.
fn session_state(session_id: u64, turns: Vec<(u32, Turn)>) -> DaemonMessage {
    let mut map = std::collections::BTreeMap::new();
    for (turn_id, turn) in turns {
        map.insert(turn_id, turn);
    }
    DaemonMessage::broadcast(DaemonMessageType::Session {
        session_id: Some(session_id),
        event: SessionEvent::SessionState {
            title: None,
            selected_model: None,
            parent_session_id: None,
            working_dir: None,
            turns: map,
            active_tool_groups: vec![],
            token_usage: None,
            context_window: None,
            last_prompt_tokens: None,
            reasoning_effort: None,
            reasoning_capability: None,
            status: SessionStatus::Inactive,
        },
    })
}

/// An app attached to [`SESSION`] whose snapshot has not arrived yet.
fn loading_app() -> App {
    let mut app = test_app();
    app.attached_session_id = Some(SESSION);
    app
}

// ── show_welcome waits for the snapshot ──────────────────────────────

#[test]
fn no_welcome_without_an_attached_session() {
    // The harness has an active display but nothing attached: the panel belongs
    // to an attached session, so it is not drawn.
    assert!(!test_app().show_welcome());
}

#[test]
fn no_welcome_while_the_snapshot_is_in_flight() {
    // An attached session with zero turns is either genuinely empty or still
    // loading, and the panel must not invite a first message into a transcript
    // that has not arrived.
    let mut app = loading_app();
    app.compute_total_height_and_markers();
    assert!(app.attached_session_id.is_some());
    assert!(
        !app.show_welcome(),
        "an attached session must not show the welcome panel before its \
         transcript snapshot lands"
    );
}

#[test]
fn an_empty_snapshot_shows_the_welcome_panel() {
    let mut app = loading_app();
    receive(&mut app, session_state(SESSION, vec![]));
    app.compute_total_height_and_markers();
    assert!(app.show_welcome());
}

#[test]
fn a_snapshot_with_turns_hides_the_welcome_panel() {
    let mut app = loading_app();
    receive(&mut app, session_state(SESSION, vec![(1, turn("hello"))]));
    app.compute_total_height_and_markers();
    assert!(!app.show_welcome());
}

#[test]
fn a_failed_attach_hides_the_welcome_panel() {
    // The transcript is unavailable, not empty: the panel would claim the
    // session is ready for a first message.
    let mut app = loading_app();
    app.display_for(SESSION).attach_failed = true;
    app.compute_total_height_and_markers();
    assert!(!app.show_welcome());
}

// ── session_note: Loading / Error / None ──────────────────────────────

#[test]
fn no_note_without_an_attached_session() {
    assert_eq!(test_app().session_note(), None);
}

#[test]
fn a_session_without_a_snapshot_is_loading() {
    assert_eq!(loading_app().session_note(), Some(SessionNote::Loading));
}

#[test]
fn a_session_with_a_snapshot_has_no_note() {
    let mut app = loading_app();
    receive(&mut app, session_state(SESSION, vec![]));
    assert_eq!(app.session_note(), None);
}

#[test]
fn a_failed_attach_is_an_error_note() {
    let mut app = loading_app();
    app.display_for(SESSION).attach_failed = true;
    assert_eq!(app.session_note(), Some(SessionNote::Error));
}

#[test]
fn an_error_wins_over_loading() {
    // A failed attach leaves both "no snapshot" and "the attempt failed" true;
    // the failure is the actionable half, so it is the note that is reported.
    let mut app = loading_app();
    let display = app.display_for(SESSION);
    assert!(!display.history_loaded);
    display.attach_failed = true;
    assert_eq!(app.session_note(), Some(SessionNote::Error));
}

// ── Error implies idle ───────────────────────────────────────────────

#[test]
fn a_failed_attach_is_not_busy_and_does_not_reject_the_prompt() {
    // A failed attach leaves no running work behind, but the summary's last
    // known status can still be `Sleeping` (which `is_idle` counts as busy).
    // Treating it as busy would refuse every prompt forever, with nothing able
    // to clear the status.
    let mut app = loading_app();
    app.attached_status = Some(SessionStatus::Sleeping);
    app.display_for(SESSION).attach_failed = true;
    assert_eq!(app.session_note(), Some(SessionNote::Error));
    assert_eq!(app.new_turn_rejection(), None);
    assert_eq!(app.idle_rejection(), None);
}

#[test]
fn a_loading_session_is_still_busy_when_the_daemon_says_so() {
    // `Loading` deliberately does NOT imply idle: the session is not ready yet,
    // and "please wait" is the right answer for the frame it lasts.
    let mut app = loading_app();
    app.attached_status = Some(SessionStatus::Sleeping);
    assert_eq!(app.session_note(), Some(SessionNote::Loading));
    assert_eq!(
        app.new_turn_rejection(),
        Some("Session is not idle, please wait before prompting.")
    );
}

// ── history_loaded is per session ────────────────────────────────────

#[test]
fn a_snapshot_sets_history_loaded_for_its_own_session_only() {
    let mut app = loading_app();
    // A background session this client also holds a display for.
    app.session_displays.entry(7).or_default();

    receive(&mut app, session_state(SESSION, vec![]));

    assert!(app.display_for(SESSION).history_loaded);
    assert!(
        !app.display_for(7).history_loaded,
        "another session's snapshot must not mark this session's transcript loaded"
    );
}

// ── A turn can outrun its snapshot ───────────────────────────────────

#[test]
fn a_turn_appended_before_the_snapshot_renders_as_a_transcript() {
    // The all-activity subscription can deliver `TurnAppended` for the session
    // before its attach snapshot. The transcript must then draw immediately —
    // it must not wait on `history_loaded`, and it must not show the panel.
    let mut app = loading_app();
    receive(
        &mut app,
        DaemonMessage::broadcast(DaemonMessageType::Session {
            session_id: Some(SESSION),
            event: SessionEvent::TurnAppended {
                turn_id: 1,
                turn: turn("earlier"),
            },
        }),
    );
    app.compute_total_height_and_markers();

    assert_eq!(app.session_note(), Some(SessionNote::Loading));
    assert_eq!(app.display_for(SESSION).visible_turn_ids, vec![1]);
    assert!(
        !app.show_welcome(),
        "turns present means the transcript is drawn, whatever the flag says"
    );
}

// ── Setting and clearing the states ──────────────────────────────────

/// An `attach_session` failure as the daemon reports it: no session id, just the
/// operation and its error.
fn attach_failure() -> DaemonMessage {
    DaemonMessage::broadcast(DaemonMessageType::Session {
        session_id: None,
        event: SessionEvent::SessionFailed {
            operation: "attach_session".to_string(),
            error: "session not found".to_string(),
        },
    })
}

#[test]
fn a_failed_attach_is_recorded_against_the_session_it_requested() {
    // `--attach 999` sends `AttachSession` without rebinding the view, so the
    // attached session is the healthy one the snapshot picked. The failure must
    // land on the session that was actually requested, or the healthy session
    // reports `error` until its next snapshot.
    let mut app = loading_app();
    app.session_displays.entry(7).or_default();
    app.pending_attach_id = Some(7);

    receive(&mut app, attach_failure());

    assert!(
        app.display_for(7).attach_failed,
        "the requested session carries the failure"
    );
    assert!(
        !app.display_for(SESSION).attach_failed,
        "the optimistically attached session is healthy and must stay so"
    );
    assert_eq!(app.session_note(), Some(SessionNote::Loading));
    assert_eq!(
        app.pending_attach_id, None,
        "the attempt is consumed by its own failure"
    );
}

#[test]
fn a_failed_attach_without_a_recorded_request_reports_the_attached_session() {
    // The fallback: an attach this client issued without the bookkeeping ends up
    // back on the optimistic attachment — and the view stays there, because
    // detaching would move the user somewhere they did not ask to be.
    let mut app = loading_app();
    assert_eq!(app.pending_attach_id, None);

    receive(&mut app, attach_failure());

    assert_eq!(app.attached_session_id, Some(SESSION));
    assert!(app.display_for(SESSION).attach_failed);
    assert_eq!(app.session_note(), Some(SessionNote::Error));
    assert!(app.error().is_some(), "the red band carries the message");
}

#[test]
fn a_confirmed_attach_clears_the_recorded_request() {
    let mut app = loading_app();
    app.session_displays.entry(7).or_default();
    app.display_for(7).attach_failed = true;
    app.pending_attach_id = Some(7);

    receive(
        &mut app,
        DaemonMessage::broadcast(DaemonMessageType::Session {
            session_id: Some(7),
            event: SessionEvent::SessionAttached,
        }),
    );

    assert_eq!(app.pending_attach_id, None, "the attach landed");
    assert!(
        !app.display_for(7).attach_failed,
        "a confirmed attach supersedes the previous attempt's failure"
    );
    assert_eq!(app.attached_session_id, Some(7));
}

#[test]
fn another_operations_failure_is_not_an_attach_error() {
    let mut app = loading_app();

    receive(
        &mut app,
        DaemonMessage::broadcast(DaemonMessageType::Session {
            session_id: None,
            event: SessionEvent::SessionFailed {
                operation: "create_session".to_string(),
                error: "boom".to_string(),
            },
        }),
    );

    assert!(!app.display_for(SESSION).attach_failed);
    assert_eq!(app.session_note(), Some(SessionNote::Loading));
}

#[test]
fn re_attaching_clears_a_previous_failure() {
    // Re-entering the session from the session list is the only retry there is,
    // so a fresh attempt must report `loading` rather than the old failure.
    let mut app = test_app();
    app.session_mgr
        .set_sessions(vec![make_session(SESSION, "s")]);
    app.display_for(SESSION).attach_failed = true;
    let (tx, _rx) = crossbeam_channel::unbounded();

    app.attach_to_session(SESSION, &tx)
        .expect("attach_to_session succeeds");

    assert!(!app.display_for(SESSION).attach_failed);
    assert_eq!(
        app.pending_attach_id,
        Some(SESSION),
        "the request is on the wire until the daemon confirms it"
    );
    assert_eq!(app.session_note(), Some(SessionNote::Loading));
}

#[test]
fn a_snapshot_after_a_failure_settles_the_note() {
    // A snapshot arriving anyway means the transcript is known: the failure
    // describes an attempt that is no longer the session's state.
    let mut app = loading_app();
    app.display_for(SESSION).attach_failed = true;

    receive(&mut app, session_state(SESSION, vec![]));

    assert_eq!(app.session_note(), None);
}
