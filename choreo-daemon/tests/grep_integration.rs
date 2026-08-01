use choreographr::{GrepArgs, execute_grep_tool};

#[test]
#[ignore]
fn grep_plain_text_finds_content() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("test.txt"), "hello world\nfoo bar").unwrap();

    let result = execute_grep_tool(
        &GrepArgs {
            pattern: "hello".to_string(),
            regex: false,
            include: None,
            path: Some(dir.path().to_str().unwrap().to_string()),
            max_results: None,
        },
        None,
    );
    let content = result.unwrap_or_default();
    assert!(content.contains("hello world"), "{}", content);
}

#[test]
#[ignore]
fn grep_with_include_filter() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("test.rs"), "hello").unwrap();
    std::fs::write(dir.path().join("test.txt"), "hello").unwrap();

    let result = execute_grep_tool(
        &GrepArgs {
            pattern: "hello".to_string(),
            regex: false,
            include: Some("*.rs".to_string()),
            path: Some(dir.path().to_str().unwrap().to_string()),
            max_results: None,
        },
        None,
    );
    let content = result.unwrap_or_default();
    assert!(content.contains("test.rs"), "{}", content);
    assert!(
        !content.contains("test.txt"),
        "should not match .txt files: {}",
        content
    );
}

#[test]
#[ignore]
fn grep_regex_mode() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("test.rs"), "fn hello() {}\nfn world() {}").unwrap();

    let result = execute_grep_tool(
        &GrepArgs {
            pattern: r"fn \w+".to_string(),
            regex: true,
            include: None,
            path: Some(dir.path().to_str().unwrap().to_string()),
            max_results: None,
        },
        None,
    );
    let content = result.unwrap_or_default();
    assert!(content.contains("fn hello()"), "{}", content);
    assert!(content.contains("fn world()"), "{}", content);
}

#[test]
#[ignore]
fn grep_no_match_returns_empty() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("test.txt"), "hello world").unwrap();

    let result = execute_grep_tool(
        &GrepArgs {
            pattern: "nonexistent".to_string(),
            regex: false,
            include: None,
            path: Some(dir.path().to_str().unwrap().to_string()),
            max_results: None,
        },
        None,
    );
    assert!(result.unwrap_or_default().is_empty());
}

#[test]
#[ignore]
fn grep_marks_truncation_at_max_results() {
    let dir = tempfile::tempdir().unwrap();
    for i in 0..5 {
        std::fs::write(dir.path().join(format!("file{i}.rs")), "hello\n").unwrap();
    }

    let result = execute_grep_tool(
        &GrepArgs {
            pattern: "hello".to_string(),
            regex: false,
            include: None,
            path: Some(dir.path().to_str().unwrap().to_string()),
            max_results: Some(2),
        },
        None,
    );
    let content = result.unwrap_or_default();
    assert_eq!(content.lines().count(), 3, "2 matches + marker: {content}");
    assert!(
        content.contains("...[truncated at 2 matches]"),
        "expected truncation marker: {content}"
    );
}

#[cfg(unix)]
#[test]
#[ignore]
fn grep_sanitizes_control_chars_in_filename() {
    let dir = tempfile::tempdir().unwrap();
    // A filename containing a literal newline would split the match across
    // two lines without sanitization.
    std::fs::write(dir.path().join("evil\nname.rs"), "hello\n").unwrap();

    let result = execute_grep_tool(
        &GrepArgs {
            pattern: "hello".to_string(),
            regex: false,
            include: None,
            path: Some(dir.path().to_str().unwrap().to_string()),
            max_results: None,
        },
        None,
    );
    let content = result.unwrap_or_default();
    // The escaped form keeps the match on a single line.
    assert!(
        content.contains("evil\\nname.rs:1:hello"),
        "expected sanitized path: {content:?}"
    );
    assert_eq!(
        content.lines().count(),
        1,
        "one line per match: {content:?}"
    );
}
