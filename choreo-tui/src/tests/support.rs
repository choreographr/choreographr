use crate::state::App;
use choreo_proto::{SessionStatus, SessionSummary, Turn};

/// Create an `App` for testing.
pub fn test_app() -> App {
    let mut app = App::new();
    // The test harness represents a connected TUI whose daemon has confirmed
    // the keystore is UNLOCKED (the real app latches this from the daemon's
    // lock-state broadcasts / subscribe-time push; a fresh `App::new` assumes
    // locked). Setting it unlocked here keeps the many prompt-submission,
    // draft-clear and scroll-to-bottom tests behaving as before, while the
    // dedicated lock-state tests set `keystore_locked` explicitly.
    app.keystore_locked = false;
    // Set up a default active session so tests can access SessionDisplayState.
    // `0` is a HARNESS-ONLY display anchor: the daemon never assigns session
    // id 0 (ids start at 1, and `None` — not `0` — marks "no origin session"
    // on the envelope). Tests that construct a session-scoped `DaemonMessage`
    // should pass a nonzero id (e.g. 42) instead, to keep fixtures
    // wire-plausible; this baked-in display is for unit-level state access.
    app.active_session_id = Some(0);
    app.session_displays.entry(0).or_default();
    app
}

/// Add a `UserText` turn to the session, mimicking what the daemon sends after
/// processing a `RunInput`.
pub fn add_user_text(app: &mut App, content: &str) {
    // The test harness mints its own turn ids; in production the daemon assigns
    // them. Derive the next id from the display's existing turns so repeated
    // calls stay increasing (no client-side counter exists any more).
    let turn_id = app
        .display_for(0)
        .view
        .turns
        .keys()
        .next_back()
        .map_or(0, |k| k + 1);
    let turn = Turn {
        created_at: choreo_proto::TimestampMs::now(),
        undone: false,
        error: None,
        user_text: Some(content.to_string()),
        assistant_text: None,
        assistant_reasoning: None,
        tool_calls: vec![],
        token_usage: None,
        tool_results: vec![],
        displayed_images: vec![],
        reasoning_artifact: None,
        reasoning_producer: None,
    };
    app.display_for(0).view.insert_or_replace(turn_id, turn);
    app.rebuild_height_prefix();
}

/// Build a minimal session-summary fixture: the shape tests that only need a
/// session's identity (id, title, status) seed the session manager with.
///
/// `active_tool_groups` is `["core"]` because a real session always has at
/// least the core group; a test asserting on the group set therefore starts
/// from a populated default rather than an artificial empty one.
pub fn make_session(id: u64, title: &str) -> SessionSummary {
    SessionSummary {
        session_id: id,
        title: Some(title.into()),
        selected_model: None,
        reasoning_effort: None,
        parent_session_id: None,
        working_dir: None,
        created_at: 1000,
        // Decreasing with id so the session manager's sort keeps the
        // fixtures in ascending-id order (the order these tests assume);
        // the value stays small so an explicit `handle_session_status_changed`
        // timestamp still overrides it in the monotonicity test.
        last_modified: 1000 - id.cast_signed(),
        turn_count: 0,
        status: SessionStatus::Inactive,
        active_tool_groups: vec!["core".into()],
        account_name: None,
        token_usage: None,
        context_window: None,
        last_prompt_tokens: None,
        pinned: false,
        archived_at: None,
    }
}

/// Build a fully-populated session-summary fixture for tests that seed the
/// session manager (or drive `set_sessions` / auto-attach logic).
///
/// Unlike [`make_session`] this seeds a selected model, a turn count and
/// realistic epoch timestamps, which the session-list and render suites assert
/// on; the two shapes exist on purpose rather than by accident, so pick the
/// one whose fields the test actually reads.
pub fn make_session_full(id: u64, title: &str, model: &str, count: u32) -> SessionSummary {
    SessionSummary {
        session_id: id,
        title: Some(title.to_string()),
        selected_model: Some(model.to_string()),
        reasoning_effort: None,
        parent_session_id: None,
        working_dir: None,
        created_at: 1_705_314_000_000,
        // Decreasing with id so the session manager's
        // `(pinned, last_modified desc, session_id desc)` sort lands the
        // fixtures in ascending-id order — the order these tests assume.
        last_modified: 1_705_314_000_000 - id.cast_signed(),
        turn_count: count,
        status: SessionStatus::Inactive,
        active_tool_groups: Vec::new(),
        account_name: None,
        token_usage: None,
        context_window: None,
        last_prompt_tokens: None,
        pinned: false,
        archived_at: None,
    }
}

/// Insert a minimal turn — a user prompt and an assistant reply — into the
/// active display under an explicit id.
///
/// The id is passed rather than derived so a test can lay out a sparse or
/// out-of-order sequence on purpose (the height and marker suites rely on it).
pub fn insert_turn(app: &mut App, id: u32, user_text: &str, assistant_text: &str) {
    let turn = Turn {
        created_at: choreo_proto::TimestampMs::now(),
        undone: false,
        error: None,
        user_text: Some(user_text.into()),
        assistant_text: Some(assistant_text.into()),
        assistant_reasoning: None,
        tool_calls: vec![],
        token_usage: None,
        tool_results: vec![],
        displayed_images: vec![],
        reasoning_artifact: None,
        reasoning_producer: None,
    };
    app.display_for(0).view.insert_or_replace(id, turn);
}
