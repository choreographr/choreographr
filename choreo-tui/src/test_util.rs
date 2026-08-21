use crate::state::App;

/// Create an `App` for testing.
pub fn test_app() -> App {
    let mut app = App::new();
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
