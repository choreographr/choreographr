pub fn test_db() -> redb::Database {
    let dir = tempfile::tempdir().unwrap();
    redb::Database::create(dir.path().join("state.redb")).unwrap()
}
