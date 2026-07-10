use crate::state::App;

/// Create an `App` with an isolated temporary database so tests never collide
/// with each other or with a real user database.
pub fn test_app(socket_path: &str, picker_protocol: &str) -> App {
    let dir = Box::new(tempfile::tempdir().expect("tempdir for test"));
    let db_path = dir.path().join("state.redb");
    crate::db::set_test_db_path(db_path);
    // Leak the TempDir so the redb file stays alive for the test's lifetime.
    Box::leak(dir);
    App::new(socket_path.to_string(), picker_protocol.to_string())
}
