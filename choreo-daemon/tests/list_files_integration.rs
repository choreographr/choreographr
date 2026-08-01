//! Integration tests for the `list_files` tool.
//!
//! These exercise real filesystem I/O (TempDir + writes) and therefore live
//! in `tests/` per the project's Test Discipline policy rather than in
//! `src/` unit-test modules. They are marked `#[ignore]` so `cargo test`
//! runs only unit tests; run with `cargo test -- --ignored`.

use choreographr::{ListFilesArgs, execute_list_files_tool};

#[test]
#[ignore]
fn lists_files_with_rich_metadata() {
    let dir = tempfile::tempdir().unwrap();
    // A small text file with a known size.
    std::fs::write(dir.path().join("main.rs"), "fn main() {}\n").unwrap();
    // A 4 KiB file.
    std::fs::write(dir.path().join("blob.bin"), vec![0u8; 4096]).unwrap();
    // Subdirectory with a couple of entries.
    std::fs::create_dir(dir.path().join("src")).unwrap();
    std::fs::write(dir.path().join("src/lib.rs"), "pub fn f() {}\n").unwrap();
    std::fs::write(dir.path().join("src/main.rs"), "fn main() {}\n").unwrap();

    let args = ListFilesArgs {
        path: Some(dir.path().to_str().unwrap().into()),
    };
    let out = execute_list_files_tool(&args, None).unwrap();

    assert!(out.contains("3 entries (2 files, 1 dir)"), "{out}");
    assert!(out.contains("main.rs"), "{out}");
    assert!(out.contains("4 KiB"), "{out}");
    assert!(out.contains("src/"), "{out}");
    assert!(out.contains("(2 entries)"), "{out}");
    // Pure metadata: sizes only, never content-derived annotations.
    assert!(!out.contains("lines"), "{out}");
    assert!(!out.contains("binary"), "{out}");
}

#[test]
#[ignore]
fn empty_directory_reports_zero_entries() {
    let dir = tempfile::tempdir().unwrap();
    let args = ListFilesArgs {
        path: Some(dir.path().to_str().unwrap().into()),
    };
    let out = execute_list_files_tool(&args, None).unwrap();
    assert!(out.contains("0 entries"), "{out}");
    assert_eq!(out.lines().count(), 1, "only the summary line: {out}");
}

#[cfg(unix)]
#[test]
#[ignore]
fn symlink_shows_target() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("real.txt"), "hi\n").unwrap();
    std::os::unix::fs::symlink("real.txt", dir.path().join("link.txt")).unwrap();

    let args = ListFilesArgs {
        path: Some(dir.path().to_str().unwrap().into()),
    };
    let out = execute_list_files_tool(&args, None).unwrap();

    assert!(out.contains("2 entries (1 file, 1 link)"), "{out}");
    assert!(out.contains("link.txt -> real.txt"), "{out}");
}

#[test]
#[ignore]
fn default_path_lists_working_directory() {
    // Execute with a temp working dir so the result is deterministic.
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.txt"), "x\n").unwrap();
    let args = ListFilesArgs { path: None };
    let out = execute_list_files_tool(&args, Some(dir.path())).unwrap();
    assert!(out.contains("1 entry (1 file)"), "{out}");
    assert!(out.contains("a.txt"), "{out}");
    assert!(
        out.starts_with(&format!("{}:", dir.path().display())),
        "{out}"
    );
}
