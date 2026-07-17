use crate::state::App;

/// Create an `App` for testing.
pub fn test_app(socket_path: &str) -> App {
    App::new(socket_path.to_string())
}
