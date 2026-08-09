use choreo_daemon::{GrepArgs, GrepOutputMode, execute_grep_tool};

/// Build args with defaults for a directory search so tests stay focused.
/// `regex: true` mirrors the production default (regex is on unless the
/// caller opts out).
fn dir_args(pattern: &str, dir: &tempfile::TempDir) -> GrepArgs {
    GrepArgs {
        pattern: pattern.to_string(),
        regex: true,
        ignore_case: false,
        context: 0,
        output_mode: GrepOutputMode::Content,
        include: None,
        path: Some(dir.path().to_str().unwrap().to_string()),
        max_results: None,
    }
}

#[test]
#[ignore]
fn grep_plain_text_finds_content() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("test.txt"), "hello world\nfoo bar").unwrap();

    let result = execute_grep_tool(&dir_args("hello", &dir), None);
    let content = result.unwrap_or_default();
    assert!(content.contains("hello world"), "{}", content);
}

#[test]
#[ignore]
fn grep_with_include_filter() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("test.rs"), "hello").unwrap();
    std::fs::write(dir.path().join("test.txt"), "hello").unwrap();

    let mut args = dir_args("hello", &dir);
    args.include = Some("*.rs".to_string());
    let result = execute_grep_tool(&args, None);
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

    // Regex is the default — no need to opt in for `fn \w+`.
    let args = dir_args(r"fn \w+", &dir);
    let result = execute_grep_tool(&args, None);
    let content = result.unwrap_or_default();
    assert!(content.contains("fn hello()"), "{}", content);
    assert!(content.contains("fn world()"), "{}", content);
}

#[test]
#[ignore]
fn grep_no_match_returns_message() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("test.txt"), "hello world").unwrap();

    let result = execute_grep_tool(&dir_args("nonexistent", &dir), None);
    let content = result.unwrap_or_default();
    assert!(
        content.contains("No matches found."),
        "expected explicit no-match message: {content:?}"
    );
}

#[test]
#[ignore]
fn grep_ignore_case() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("test.txt"), "Hello World\nfoo").unwrap();

    let mut args = dir_args("hello", &dir);
    args.ignore_case = true;
    let result = execute_grep_tool(&args, None);
    let content = result.unwrap_or_default();
    assert!(content.contains("Hello World"), "{}", content);
}

#[test]
#[ignore]
fn grep_context_lines() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("test.txt"), "a\nb\nhello\nc\nd").unwrap();

    let mut args = dir_args("hello", &dir);
    args.context = 1;
    let result = execute_grep_tool(&args, None);
    let content = result.unwrap_or_default();
    assert!(content.contains("test.txt-2-b"), "{}", content);
    assert!(content.contains("test.txt:3:hello"), "{}", content);
    assert!(content.contains("test.txt-4-c"), "{}", content);
}

#[test]
#[ignore]
fn grep_files_with_matches() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.rs"), "hello\n").unwrap();
    std::fs::write(dir.path().join("b.txt"), "hello world\nhello again\n").unwrap();
    std::fs::write(dir.path().join("c.py"), "nope\n").unwrap();

    let mut args = dir_args("hello", &dir);
    args.output_mode = GrepOutputMode::FilesWithMatches;
    let result = execute_grep_tool(&args, None);
    let content = result.unwrap_or_default();
    // One line per hit file, sorted, deduplicated — b.txt has two matches
    // but appears once.
    assert_eq!(content, "a.rs\nb.txt", "{}", content);
}

#[test]
#[ignore]
fn grep_count_mode() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.txt"), "world\n").unwrap();
    std::fs::write(dir.path().join("b.txt"), "world\nworld\n").unwrap();
    std::fs::write(dir.path().join("c.txt"), "nope\n").unwrap();

    let mut args = dir_args("world", &dir);
    args.output_mode = GrepOutputMode::Count;
    let result = execute_grep_tool(&args, None);
    let content = result.unwrap_or_default();
    assert_eq!(content, "a.txt: 1\nb.txt: 2", "{}", content);
}

#[test]
#[ignore]
fn grep_marks_truncation_at_max_results() {
    let dir = tempfile::tempdir().unwrap();
    for i in 0..5 {
        std::fs::write(dir.path().join(format!("file{i}.rs")), "hello\n").unwrap();
    }

    let mut args = dir_args("hello", &dir);
    args.max_results = Some(2);
    let result = execute_grep_tool(&args, None);
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

    let result = execute_grep_tool(&dir_args("hello", &dir), None);
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

#[test]
#[ignore]
fn grep_src_anchored_include_matches_relative_to_root() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join("src")).unwrap();
    std::fs::write(dir.path().join("src/main.rs"), "hello\n").unwrap();

    // Regression: `src/*.rs` used to be matched against the absolute path
    // and silently returned nothing. It must match `src/main.rs` relative
    // to the search root, wherever that root happens to live.
    let mut args = dir_args("hello", &dir);
    args.include = Some("src/*.rs".to_string());
    let result = execute_grep_tool(&args, None);
    let content = result.unwrap_or_default();
    assert!(
        content.contains("src/main.rs:1:hello"),
        "expected src/main.rs match: {content:?}"
    );
}

#[test]
#[ignore]
fn grep_single_file_include_filters_by_basename() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("main.rs");
    std::fs::write(&file, "hello\n").unwrap();

    let file_arg = |include: &str| GrepArgs {
        pattern: "hello".to_string(),
        regex: true,
        ignore_case: false,
        context: 0,
        output_mode: GrepOutputMode::Content,
        include: Some(include.to_string()),
        path: Some(file.to_str().unwrap().to_string()),
        max_results: None,
    };

    // A directly-named file has no directory context: the include glob is
    // matched against the file name.
    let matching = execute_grep_tool(&file_arg("*.rs"), None).unwrap_or_default();
    assert!(matching.contains("main.rs:1:hello"), "{matching:?}");

    // A non-matching glob filters the explicitly-named file out entirely.
    let filtered = execute_grep_tool(&file_arg("*.py"), None).unwrap_or_default();
    assert!(filtered.contains("No matches found."), "{filtered:?}");
}
