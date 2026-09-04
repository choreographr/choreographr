use crate::state::App;
use choreo_proto::Turn;

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

/// Add a UserText turn to the session, mimicking what the daemon sends after
/// processing a RunInput.
pub fn add_user_text(app: &mut App, content: &str) {
    let turn_id = app.next_request_id;
    app.next_request_id += 1;
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

/// Build a session-summary fixture for tests that seed the session manager
/// (or drive `set_sessions` / auto-attach logic).
pub fn make_session(id: u64, title: &str, model: &str, count: u32) -> choreo_proto::SessionSummary {
    choreo_proto::SessionSummary {
        session_id: id,
        title: Some(title.to_string()),
        selected_model: Some(model.to_string()),
        reasoning_effort: None,
        parent_session_id: None,
        working_dir: None,
        created_at: 1705314000000,
        last_modified: 1705314000000,
        turn_count: count,
        status: choreo_proto::SessionStatus::Inactive,
        active_tool_groups: Vec::new(),
        account_name: None,
        token_usage: None,
        context_window: None,
        last_prompt_tokens: None,
    }
}
