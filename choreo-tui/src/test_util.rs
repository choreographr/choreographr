use crate::state::App;

/// Create an `App` for testing.
pub fn test_app() -> App {
    let mut app = App::new();
    // Set up a default active session so tests can access SessionDisplayState.
    app.active_session_id = Some(0);
    app.session_displays.entry(0).or_default();
    app
}
