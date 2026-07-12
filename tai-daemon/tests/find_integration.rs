use tai_daemon::{FindArgs, execute_find_tool};

#[test]
#[ignore]
fn find_substring_match() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("foo.rs"), "").unwrap();
    std::fs::write(dir.path().join("bar.rs"), "").unwrap();

    let result = execute_find_tool(
        &FindArgs {
            pattern: "foo".to_string(),
            glob: false,
            path: Some(dir.path().to_str().unwrap().to_string()),
            max_results: None,
        },
        None,
    );
    let content = result.unwrap_or_default();
    assert!(content.contains("foo.rs"), "{}", content);
    assert!(
        !content.contains("bar.rs"),
        "should not match bar: {}",
        content
    );
}

#[test]
#[ignore]
fn find_glob_match() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("foo.rs"), "").unwrap();
    std::fs::write(dir.path().join("bar.py"), "").unwrap();

    let result = execute_find_tool(
        &FindArgs {
            pattern: "*.rs".to_string(),
            glob: true,
            path: Some(dir.path().to_str().unwrap().to_string()),
            max_results: None,
        },
        None,
    );
    let content = result.unwrap_or_default();
    assert!(content.contains("foo.rs"), "{}", content);
    assert!(
        !content.contains("bar.py"),
        "should not match .py: {}",
        content
    );
}

#[test]
#[ignore]
fn find_directory_gets_trailing_slash() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join("srcdir")).unwrap();

    let result = execute_find_tool(
        &FindArgs {
            pattern: "srcdir".to_string(),
            glob: false,
            path: Some(dir.path().to_str().unwrap().to_string()),
            max_results: None,
        },
        None,
    );
    let content = result.unwrap_or_default();
    assert!(
        content.contains("srcdir/"),
        "expected trailing slash: {}",
        content
    );
}

#[test]
#[ignore]
fn find_no_match_returns_empty() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("foo.rs"), "").unwrap();

    let result = execute_find_tool(
        &FindArgs {
            pattern: "nonexistent".to_string(),
            glob: false,
            path: Some(dir.path().to_str().unwrap().to_string()),
            max_results: None,
        },
        None,
    );
    assert!(result.unwrap_or_default().is_empty());
}
