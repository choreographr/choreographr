use choreo_daemon::{FindArgs, execute_find_tool};

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

#[test]
#[ignore]
fn find_shows_file_sizes() {
    let dir = tempfile::tempdir().unwrap();
    // A 4 KiB file — sizes come from the walker's metadata pass.
    std::fs::write(dir.path().join("blob.bin"), vec![0u8; 4096]).unwrap();

    let result = execute_find_tool(
        &FindArgs {
            pattern: "blob".to_string(),
            glob: false,
            path: Some(dir.path().to_str().unwrap().to_string()),
            max_results: None,
        },
        None,
    );
    let content = result.unwrap_or_default();
    assert!(
        content
            .lines()
            .any(|l| l.contains("blob.bin") && l.contains("4 KiB")),
        "expected size in output: {content}"
    );
}

#[cfg(unix)]
#[test]
#[ignore]
fn find_shows_symlink_targets() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("real.txt"), "hi\n").unwrap();
    std::os::unix::fs::symlink("real.txt", dir.path().join("link.txt")).unwrap();

    let result = execute_find_tool(
        &FindArgs {
            pattern: "link".to_string(),
            glob: false,
            path: Some(dir.path().to_str().unwrap().to_string()),
            max_results: None,
        },
        None,
    );
    let content = result.unwrap_or_default();
    assert!(content.contains("link.txt -> real.txt"), "{content}");
}

#[test]
#[ignore]
fn find_marks_truncation_at_max_results() {
    let dir = tempfile::tempdir().unwrap();
    for i in 0..5 {
        std::fs::write(dir.path().join(format!("file{i}.rs")), "").unwrap();
    }

    let result = execute_find_tool(
        &FindArgs {
            pattern: ".rs".to_string(),
            glob: false,
            path: Some(dir.path().to_str().unwrap().to_string()),
            max_results: Some(2),
        },
        None,
    );
    let content = result.unwrap_or_default();
    assert_eq!(content.lines().count(), 3, "2 matches + marker: {content}");
    assert!(
        content.contains("...[truncated at 2 results]"),
        "expected truncation marker: {content}"
    );
}

#[test]
#[ignore]
fn find_path_anchored_glob_matches_relative_paths() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join("src")).unwrap();
    std::fs::write(dir.path().join("src/main.rs"), "").unwrap();
    std::fs::write(dir.path().join("src/lib.rs"), "").unwrap();
    std::fs::write(dir.path().join("top.rs"), "").unwrap();

    // `src/*.rs` contains a '/', so it is matched natively by the walker
    // against root-relative paths (and prunes traversal outside src/).
    let result = execute_find_tool(
        &FindArgs {
            pattern: "src/*.rs".to_string(),
            glob: false,
            path: Some(dir.path().to_str().unwrap().to_string()),
            max_results: None,
        },
        None,
    );
    let content = result.unwrap_or_default();
    assert!(content.contains("src/main.rs"), "{content}");
    assert!(content.contains("src/lib.rs"), "{content}");
    assert!(
        !content.contains("top.rs"),
        "should not match root file: {content}"
    );
}

#[test]
#[ignore]
fn find_bare_glob_matches_basename_at_any_depth() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join("src")).unwrap();
    std::fs::write(dir.path().join("src/main.rs"), "").unwrap();
    std::fs::write(dir.path().join("top.rs"), "").unwrap();

    // A bare pattern (no '/') keeps basename matching at any depth.
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
    assert!(content.contains("src/main.rs"), "{content}");
    assert!(content.contains("top.rs"), "{content}");
}

#[test]
#[ignore]
fn find_leading_dot_slash_pattern_matches() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join("src")).unwrap();
    std::fs::write(dir.path().join("src/main.rs"), "").unwrap();

    // A leading `./` is a path prefix, not part of any file name — it must
    // not silently produce zero results.
    let result = execute_find_tool(
        &FindArgs {
            pattern: "./src/*.rs".to_string(),
            glob: false,
            path: Some(dir.path().to_str().unwrap().to_string()),
            max_results: None,
        },
        None,
    );
    let content = result.unwrap_or_default();
    assert!(content.contains("src/main.rs"), "{content}");
}

#[test]
#[ignore]
fn find_absolute_pattern_relative_to_root() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join("src")).unwrap();
    std::fs::write(dir.path().join("src/main.rs"), "").unwrap();

    // An absolute pattern under the search root is converted to the
    // equivalent root-relative pattern instead of silently matching nothing.
    let abs_pattern = format!("{}/src/*.rs", dir.path().display());
    let result = execute_find_tool(
        &FindArgs {
            pattern: abs_pattern,
            glob: false,
            path: Some(dir.path().to_str().unwrap().to_string()),
            max_results: None,
        },
        None,
    );
    let content = result.unwrap_or_default();
    assert!(content.contains("src/main.rs"), "{content}");
}

#[test]
#[ignore]
fn find_absolute_pattern_outside_root_errors() {
    let dir = tempfile::tempdir().unwrap();
    let other = tempfile::tempdir().unwrap();
    std::fs::write(other.path().join("x.rs"), "").unwrap();

    // An absolute pattern outside the search root cannot be expressed
    // relative to it — error rather than silently returning nothing.
    let abs_pattern = format!("{}/x.rs", other.path().display());
    let result = execute_find_tool(
        &FindArgs {
            pattern: abs_pattern,
            glob: false,
            path: Some(dir.path().to_str().unwrap().to_string()),
            max_results: None,
        },
        None,
    );
    assert!(result.is_err(), "expected an error, got {result:?}");
    assert!(result.unwrap_err().0.contains("outside the search root"));
}

#[cfg(unix)]
#[test]
#[ignore]
fn find_sanitizes_control_chars_in_symlink_target() {
    let dir = tempfile::tempdir().unwrap();
    // A symlink whose *target* name contains a literal newline — the target
    // must be escaped so the result stays on exactly one line.
    let target_name = "evil\ntarget.txt";
    std::fs::write(dir.path().join(target_name), "hi\n").unwrap();
    std::os::unix::fs::symlink(target_name, dir.path().join("link.txt")).unwrap();

    let result = execute_find_tool(
        &FindArgs {
            pattern: "link".to_string(),
            glob: false,
            path: Some(dir.path().to_str().unwrap().to_string()),
            max_results: None,
        },
        None,
    );
    let content = result.unwrap_or_default();
    assert!(
        content.contains("link.txt -> evil\\ntarget.txt"),
        "{content:?}"
    );
    assert_eq!(content.lines().count(), 1, "{content:?}");
}
