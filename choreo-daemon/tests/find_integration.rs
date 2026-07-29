use choreographr::{FindArgs, execute_find_tool};

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
fn find_glob_auto_detect() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("foo.rs"), "").unwrap();
    std::fs::write(dir.path().join("bar.rs"), "").unwrap();
    std::fs::write(dir.path().join("readme.txt"), "").unwrap();

    // `glob: false` with wildcard pattern `*.rs` → auto-detected as glob.
    let result = execute_find_tool(
        &FindArgs {
            pattern: "*.rs".to_string(),
            glob: false,
            path: Some(dir.path().to_str().unwrap().to_string()),
            max_results: None,
        },
        None,
    );
    let content = result.unwrap_or_default();
    assert!(content.contains("foo.rs"), "{}", content);
    assert!(content.contains("bar.rs"), "{}", content);
    assert!(
        !content.contains("readme.txt"),
        "should not match .txt: {}",
        content
    );
}

#[test]
#[ignore]
fn find_glob_auto_detect_question_mark() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("foo.rs"), "").unwrap();
    std::fs::write(dir.path().join("fox.rs"), "").unwrap();
    std::fs::write(dir.path().join("bar.rs"), "").unwrap();

    // `f?x.rs` has a `?` wildcard → auto-detected as glob.
    let result = execute_find_tool(
        &FindArgs {
            pattern: "f?x.rs".to_string(),
            glob: false,
            path: Some(dir.path().to_str().unwrap().to_string()),
            max_results: None,
        },
        None,
    );
    let content = result.unwrap_or_default();
    assert!(content.contains("fox.rs"), "{}", content);
    assert!(
        !content.contains("bar.rs"),
        "should not match bar.rs: {}",
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
