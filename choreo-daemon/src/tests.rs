use super::*;
use crate::tools::fs::{EditFileArgs, TextEditArgs, WriteFileArgs};

fn test_temp_path(prefix: &str) -> std::path::PathBuf {
    let unique = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("time")
        .as_nanos();
    std::env::temp_dir().join(format!("{prefix}-{unique}.txt"))
}

#[test]
fn write_file_tool_writes_new_file() {
    let path = test_temp_path("choreo-write-tool");

    execute_write_file_tool(
        &WriteFileArgs {
            path: path.display().to_string(),
            content: "hello from write tool\n".into(),
            overwrite: Some(true),
            create_parents: Some(true),
        },
        None,
    )
    .unwrap();

    assert_eq!(
        std::fs::read_to_string(&path).expect("read file"),
        "hello from write tool\n"
    );

    let _ = std::fs::remove_file(&path);
}

#[test]
fn write_file_tool_refuses_overwrite_when_disabled() {
    let path = test_temp_path("choreo-write-tool-existing");
    std::fs::write(&path, "original\n").expect("seed file");

    let result = execute_write_file_tool(
        &WriteFileArgs {
            path: path.display().to_string(),
            content: "replacement\n".into(),
            overwrite: Some(false),
            create_parents: Some(true),
        },
        None,
    );

    assert!(result.is_err(), "{}", result.unwrap_err());
    let err = result.unwrap_err().to_string();
    assert!(
        err.contains("refusing to overwrite existing file"),
        "{}",
        err
    );
    assert_eq!(
        std::fs::read_to_string(&path).expect("read file"),
        "original\n"
    );

    let _ = std::fs::remove_file(&path);
}

#[test]
fn write_file_tool_creates_parent_directories() {
    let dir = test_temp_path("choreo-write-tool-dir").with_extension("");
    let path = dir.join("nested/output.txt");

    execute_write_file_tool(
        &WriteFileArgs {
            path: path.display().to_string(),
            content: "nested hello\n".into(),
            overwrite: Some(true),
            create_parents: Some(true),
        },
        None,
    )
    .unwrap();

    assert_eq!(
        std::fs::read_to_string(&path).expect("read file"),
        "nested hello\n"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn edit_file_tool_replaces_single_unique_match() {
    let path = test_temp_path("choreo-edit-tool-single");
    std::fs::write(&path, "alpha\nbeta\ngamma\n").expect("seed file");

    let result = execute_edit_file_tool(
        &EditFileArgs {
            path: path.display().to_string(),
            edits: vec![TextEditArgs {
                old_text: "beta".into(),
                new_text: "delta".into(),
                replace_all: None,
            }],
            expected_sha256: None,
            dry_run: None,
        },
        None,
    );

    let content = result.unwrap_or_default();
    assert!(content.contains("edited file:"), "{}", content);
    assert_eq!(
        std::fs::read_to_string(&path).expect("read file"),
        "alpha\ndelta\ngamma\n"
    );

    let _ = std::fs::remove_file(&path);
}

#[test]
fn edit_file_tool_fails_when_old_text_is_missing() {
    let path = test_temp_path("choreo-edit-tool-missing");
    std::fs::write(&path, "hello\nworld\n").expect("seed file");

    let result = execute_edit_file_tool(
        &EditFileArgs {
            path: path.display().to_string(),
            edits: vec![TextEditArgs {
                old_text: "absent".into(),
                new_text: "present".into(),
                replace_all: None,
            }],
            expected_sha256: None,
            dry_run: None,
        },
        None,
    );

    assert!(result.is_err(), "{}", result.unwrap_err());
    let err = result.unwrap_err().to_string();
    assert!(err.contains("old_text not found"), "{}", err);
    assert_eq!(
        std::fs::read_to_string(&path).expect("read file"),
        "hello\nworld\n"
    );

    let _ = std::fs::remove_file(&path);
}

#[test]
fn edit_file_tool_fails_on_ambiguous_non_replace_all_edit() {
    let path = test_temp_path("choreo-edit-tool-ambiguous");
    std::fs::write(&path, "repeat\nrepeat\n").expect("seed file");

    let result = execute_edit_file_tool(
        &EditFileArgs {
            path: path.display().to_string(),
            edits: vec![TextEditArgs {
                old_text: "repeat".into(),
                new_text: "done".into(),
                replace_all: None,
            }],
            expected_sha256: None,
            dry_run: None,
        },
        None,
    );

    assert!(result.is_err(), "{}", result.unwrap_err());
    let err = result.unwrap_err().to_string();
    assert!(err.contains("matched 2 locations"), "{}", err);
    assert_eq!(
        std::fs::read_to_string(&path).expect("read file"),
        "repeat\nrepeat\n"
    );

    let _ = std::fs::remove_file(&path);
}

#[test]
fn edit_file_tool_supports_replace_all_and_dry_run() {
    let path = test_temp_path("choreo-edit-tool-replace-all");
    std::fs::write(&path, "foo\nfoo\n").expect("seed file");

    let result = execute_edit_file_tool(
        &EditFileArgs {
            path: path.display().to_string(),
            edits: vec![TextEditArgs {
                old_text: "foo".into(),
                new_text: "bar".into(),
                replace_all: Some(true),
            }],
            expected_sha256: None,
            dry_run: Some(true),
        },
        None,
    );

    let content = result.unwrap_or_default();
    assert!(content.contains("would edit file:"), "{}", content);
    assert!(content.contains("2 replacements"), "{}", content);
    assert_eq!(
        std::fs::read_to_string(&path).expect("read file"),
        "foo\nfoo\n"
    );

    let _ = std::fs::remove_file(&path);
}

#[test]
fn edit_file_tool_validates_expected_sha256() {
    let path = test_temp_path("choreo-edit-tool-sha");
    let original = "red\nblue\n";
    std::fs::write(&path, original).expect("seed file");
    let expected_sha256 = sha256_hex(original);

    let success = execute_edit_file_tool(
        &EditFileArgs {
            path: path.display().to_string(),
            edits: vec![TextEditArgs {
                old_text: "blue".into(),
                new_text: "green".into(),
                replace_all: None,
            }],
            expected_sha256: Some(expected_sha256.clone()),
            dry_run: None,
        },
        None,
    );
    assert!(success.is_ok(), "{}", success.unwrap_err());
    assert_eq!(
        std::fs::read_to_string(&path).expect("read file"),
        "red\ngreen\n"
    );

    let failure = execute_edit_file_tool(
        &EditFileArgs {
            path: path.display().to_string(),
            edits: vec![TextEditArgs {
                old_text: "green".into(),
                new_text: "purple".into(),
                replace_all: None,
            }],
            expected_sha256: Some(expected_sha256),
            dry_run: None,
        },
        None,
    );
    assert!(failure.is_err(), "{}", failure.unwrap_err());
    let err = failure.unwrap_err().to_string();
    assert!(err.contains("expected_sha256 mismatch"), "{}", err);
    assert_eq!(
        std::fs::read_to_string(&path).expect("read file"),
        "red\ngreen\n"
    );

    let _ = std::fs::remove_file(&path);
}
