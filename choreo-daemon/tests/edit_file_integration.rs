//! Integration tests for the `edit_file` tool.
//!
//! These exercise real filesystem I/O (TempDir + writes + permission bits) and
//! therefore live in `tests/` per the project's Test Discipline policy rather
//! than in `src/` unit-test modules. They are marked `#[ignore]` so `cargo
//! test` runs only unit tests; run with `cargo test-integration`.

use choreo_daemon::{EditFileArgs, TextEditArgs, execute_edit_file_tool};
use std::path::Path;

fn edit_args(path: &Path, old: &str, new: &str) -> EditFileArgs {
    EditFileArgs {
        path: path.to_str().unwrap().into(),
        edits: vec![TextEditArgs {
            old_text: old.into(),
            new_text: new.into(),
            replace_all: None,
        }],
        expected_sha256: None,
        dry_run: None,
    }
}

// The regression this guards against: edit_file replaces the target via a
// NamedTempFile persist, which is created 0600 — without preserving the
// original mode, editing a script would strip its +x bit and leave a 0600
// copy behind (a `Permission denied` for `./script.sh`).
#[cfg(unix)]
#[test]
#[ignore]
fn edit_preserves_execute_permissions() {
    use std::os::unix::fs::PermissionsExt;

    let dir = tempfile::tempdir().unwrap();
    let script = dir.path().join("release.sh");
    std::fs::write(&script, "#!/usr/bin/env bash\necho hi\n").unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();

    execute_edit_file_tool(&edit_args(&script, "echo hi", "echo bye"), None).unwrap();

    let content = std::fs::read_to_string(&script).unwrap();
    assert!(
        content.contains("echo bye"),
        "content must be edited: {content}"
    );
    let mode = std::fs::metadata(&script).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o755, "edit must preserve the executable bit");
}

// The inverse of the above: the buggy code could also *narrow* permissions —
// a 0644 file silently became 0600, losing group/other readability. This
// guards against the persist dropping permission bits (the tempfile default
// is 0600), rather than widening them.
#[cfg(unix)]
#[test]
#[ignore]
fn edit_preserves_group_readable_permissions() {
    use std::os::unix::fs::PermissionsExt;

    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("shared.txt");
    std::fs::write(&file, "shared\n").unwrap();
    std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o644)).unwrap();

    execute_edit_file_tool(&edit_args(&file, "shared", "updated"), None).unwrap();

    let mode = std::fs::metadata(&file).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o644, "edit must preserve the original mode");
}

#[test]
#[ignore]
fn edit_replaces_content() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("note.txt");
    std::fs::write(&file, "hello world\n").unwrap();

    execute_edit_file_tool(&edit_args(&file, "hello world", "goodbye"), None).unwrap();

    let content = std::fs::read_to_string(&file).unwrap();
    assert_eq!(content, "goodbye\n");
}

// Editing through a symlink must update the real target and keep the link:
// the atomic replace resolves the target before swapping, so the persist
// lands on the file the link points to instead of replacing the link itself
// with a regular file.
#[cfg(unix)]
#[test]
#[ignore]
fn edit_through_symlink_updates_target_and_keeps_link() {
    use std::os::unix::fs::PermissionsExt;

    let dir = tempfile::tempdir().unwrap();
    let real = dir.path().join("real.txt");
    let link = dir.path().join("link.txt");
    std::fs::write(&real, "original\n").unwrap();
    std::os::unix::fs::symlink(&real, &link).unwrap();
    std::fs::set_permissions(&real, std::fs::Permissions::from_mode(0o644)).unwrap();

    execute_edit_file_tool(&edit_args(&link, "original", "edited"), None).unwrap();

    assert!(link.is_symlink(), "the symlink must survive the edit");
    assert_eq!(
        std::fs::read_to_string(&real).unwrap(),
        "edited\n",
        "the target file must hold the new content"
    );
    let mode = std::fs::metadata(&real).unwrap().permissions().mode() & 0o777;
    assert_eq!(
        mode, 0o644,
        "target mode must be preserved through the link"
    );
}
