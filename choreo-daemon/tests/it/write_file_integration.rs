//! Integration tests for the `write_file` tool's overwrite path.
//!
//! These exercise real filesystem I/O (`TempDir` + writes + permission bits) and
//! therefore live in `tests/` per the project's Test Discipline policy rather
//! than in `src/` unit-test modules. They are marked `#[ignore = "integration"]` so `cargo
//! test` runs only unit tests; run with `cargo test-integration`.
//!
//! `write_file` shares its atomic-replace helper (`atomic_write_text_file` in
//! `tools/fs/mod.rs`) with `edit_file`; these tests pin down the permission
//! behavior of that shared path from `write_file`'s side.

// AGENTS.md permits unwrap/expect/panic in tests/ files, but clippy's
// allow-*-in-tests config only recognizes #[test]-annotated functions —
// helper fns in this file need this file-level allowance.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::panic_in_result_fn,
    clippy::indexing_slicing
)]
use choreo_daemon::{WriteFileArgs, execute_write_file_tool};
use std::path::Path;

fn write_args(path: &Path, content: &str, overwrite: bool) -> WriteFileArgs {
    WriteFileArgs {
        path: path.to_str().unwrap().into(),
        content: content.into(),
        overwrite: Some(overwrite),
        create_parents: Some(true),
    }
}

// The regression the edit_file fix guards against applies here too: write_file
// with overwrite replaces the target via a NamedTempFile persist (created 0600
// on Unix), so without preserving the original mode an executable script would
// silently lose its +x bit.
#[cfg(unix)]
#[test]
#[ignore = "integration"]
fn overwrite_preserves_execute_permissions() {
    use std::os::unix::fs::PermissionsExt;

    let dir = tempfile::tempdir().unwrap();
    let script = dir.path().join("deploy.sh");
    std::fs::write(&script, "#!/usr/bin/env bash\necho deploy\n").unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();

    execute_write_file_tool(
        &write_args(&script, "#!/usr/bin/env bash\necho updated\n", true),
        None,
    )
    .unwrap();

    let content = std::fs::read_to_string(&script).unwrap();
    assert!(
        content.contains("echo updated"),
        "content must be overwritten: {content}"
    );
    let mode = std::fs::metadata(&script).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o755, "overwrite must preserve the executable bit");
}

// A brand-new file has no pre-existing permissions to honor: the atomic
// replace path deliberately keeps the tempfile default (0600 on Unix).
#[cfg(unix)]
#[test]
#[ignore = "integration"]
fn overwrite_new_file_keeps_tempfile_default_mode() {
    use std::os::unix::fs::PermissionsExt;

    let dir = tempfile::tempdir().unwrap();
    let fresh = dir.path().join("fresh.txt");

    execute_write_file_tool(&write_args(&fresh, "hello\n", true), None).unwrap();

    let mode = std::fs::metadata(&fresh).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600, "new files keep the 0600 tempfile default");
}
