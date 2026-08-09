use crate::tools::glob_util::GlobFilter;
use crate::tools::{ToolExecError, resolve_path, truncate_tool_output};
use schemars::JsonSchema;
use serde::Deserialize;
use std::path::Path;
use tracing::{error, info, warn};
use zlob::ZlobFlags;
use zlob::walk::{WalkBuilder, WalkFlags, WalkState};

#[derive(Debug, Deserialize, JsonSchema)]
pub struct DeleteFilesArgs {
    /// Paths or glob patterns to delete. Strings containing glob metacharacters
    /// (`*`, `?`, `[`) are treated as glob patterns. Metacharacters can be
    /// escaped with a backslash (e.g. `file\[.txt` for a literal `[`).
    ///
    /// Glob patterns without `/` are matched against the file's basename
    /// (filename only), so `*.log` matches `.log` files at any directory
    /// depth. Patterns with `/` are matched against the full path starting
    /// from the working directory.
    #[serde(default)]
    pub targets: Vec<String>,
    /// Whether to recursively delete directories (required for directories).
    pub recursive: Option<bool>,
}

pub(crate) fn execute_delete_files_tool(
    args: &DeleteFilesArgs,
    working_dir: Option<&Path>,
) -> Result<String, ToolExecError> {
    if args.targets.is_empty() {
        return Err(ToolExecError(
            "must provide at least one target (path or glob pattern)".to_string(),
        ));
    }

    let mut targets: Vec<std::path::PathBuf> = Vec::new();
    // Each glob entry determines match_basename automatically
    // (gitignore-style: no `/` → basename match, has `/` → full path match).
    let mut glob_patterns: Vec<GlobFilter> = Vec::new();

    // Pre-classify each target as either a glob pattern or a literal path.
    // Auto-detecting via has_wildcards avoids burdening the caller with an
    // explicit flag, at the cost of requiring backslash-escaped `*`, `?`, `[`
    // in filenames that should be treated literally.
    for raw in &args.targets {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            continue;
        }
        if zlob::has_wildcards(trimmed, ZlobFlags::RECOMMENDED) {
            glob_patterns
                .push(GlobFilter::compile(trimmed).map_err(|e| {
                    ToolExecError(format!("invalid glob pattern '{trimmed}': {e}"))
                })?);
        } else {
            // Literal paths are resolved against the session working directory.
            // Boundary enforcement is the OS-level sandbox's job (Landlock on
            // Linux, Seatbelt on macOS) — no in-process confinement here.
            targets.push(resolve_path(trimmed, working_dir));
        }
    }

    // Expand glob patterns via a single directory walk anchored at the working dir.
    if !glob_patterns.is_empty() {
        // Resolve the walk root against the session working directory.
        let wd = resolve_path(".", working_dir);

        // WalkFlags::RECOMMENDED skips hidden files and respects .gitignore rules,
        // matching the behavior of find and grep tools. This prevents accidental
        // deletion of dotfiles or git-ignored artifacts via broad globs like `*`.
        // To delete such files, target them with a literal path instead.
        let mut matched: Vec<std::path::PathBuf> = Vec::new();
        WalkBuilder::new(&wd)
            .map_err(|e| ToolExecError(format!("failed to start walk: {e}")))?
            .options(WalkFlags::RECOMMENDED)
            .run_serial(|entry| {
                let path = entry.path();
                for filter in &glob_patterns {
                    if filter.matches(path) {
                        matched.push(path.to_path_buf());
                        break;
                    }
                }
                WalkState::Continue
            })
            .map_err(|e| {
                // zlob's walker silently skips per-entry I/O errors (permission
                // denied, broken symlinks) — only fatal errors surface here.
                warn!(error = %e, "delete_files walk aborted due to fatal error");
                ToolExecError(format!("walk error: {e}"))
            })?;

        // Push each glob-expanded path directly.  Previously these were
        // re-checked against the working directory boundary; with the move to
        // OS-level sandboxing the walk result is taken as-is (the kernel
        // enforces the boundary at access time).
        targets.extend(matched);
    }

    // Deduplicate — a path matched by multiple patterns should appear once.
    targets.sort();
    targets.dedup();

    if targets.is_empty() {
        return Ok("No matching files found to delete.".to_string());
    }

    info!(
        "delete_files: deleting {} item(s) via {} target(s) and {} glob pattern(s)",
        targets.len(),
        args.targets.len(),
        glob_patterns.len(),
    );

    // Collect partial results rather than failing fast. When deleting multiple
    // files, a single permission error should not prevent the rest from being
    // cleaned up. The caller receives both success and failure lists.
    let mut deleted: Vec<String> = Vec::new();
    let mut errors: Vec<String> = Vec::new();

    for target in &targets {
        // Use symlink_metadata (not metadata) to avoid following symlinks.
        // If the target is a symlink to a directory outside the working dir,
        // we only remove the symlink itself, not the remote directory.
        let metadata = match std::fs::symlink_metadata(target) {
            Ok(m) => m,
            Err(e) => {
                warn!("delete_files: failed to stat '{}': {e}", target.display());
                errors.push(format!("{}: {e}", target.display()));
                continue;
            }
        };

        if metadata.is_dir() {
            if args.recursive.unwrap_or(false) {
                match std::fs::remove_dir_all(target) {
                    Ok(()) => deleted.push(format!("{} (directory)", target.display())),
                    Err(e) => {
                        error!(
                            "delete_files: failed to remove directory '{}': {e}",
                            target.display()
                        );
                        errors.push(format!("{}: {e}", target.display()));
                    }
                }
            } else {
                warn!(
                    "delete_files: '{}' is a directory, recursive not set",
                    target.display()
                );
                errors.push(format!(
                    "{} is a directory; set recursive=true to delete it",
                    target.display()
                ));
            }
        } else {
            match std::fs::remove_file(target) {
                Ok(()) => deleted.push(target.display().to_string()),
                Err(e) => {
                    warn!(
                        "delete_files: failed to remove file '{}': {e}",
                        target.display()
                    );
                    errors.push(format!("{}: {e}", target.display()));
                }
            }
        }
    }

    info!(
        "delete_files: completed — {} deleted, {} errors",
        deleted.len(),
        errors.len(),
    );

    // Build the output string with results grouped by success/failure.
    let mut output = String::new();
    if !deleted.is_empty() {
        output.push_str(&format!("Deleted {} item(s):\n", deleted.len()));
        for item in &deleted {
            output.push_str(&format!("  - {item}\n"));
        }
    }
    if !errors.is_empty() {
        if !output.is_empty() {
            output.push('\n');
        }
        output.push_str(&format!("Failed to delete {} item(s):\n", errors.len()));
        for error in &errors {
            output.push_str(&format!("  - {error}\n"));
        }
    }

    Ok(truncate_tool_output(&output))
}

pub fn describe_delete_files_invocation(args: &DeleteFilesArgs) -> String {
    let mut parts = Vec::new();
    let glob_count = args
        .targets
        .iter()
        .filter(|t| zlob::has_wildcards(t.trim(), ZlobFlags::RECOMMENDED))
        .count();
    let literal_count = args.targets.len() - glob_count;
    if literal_count == 1 && glob_count == 0 {
        parts.push(format!("Deleting file `{}`.", args.targets[0]));
    } else {
        if literal_count > 0 {
            parts.push(format!("Deleting {} path(s).", literal_count));
        }
        if glob_count > 0 {
            parts.push(format!("Expanding {} glob pattern(s).", glob_count));
        }
    }
    if args.recursive.unwrap_or(false) {
        parts.push("Recursive mode enabled.".to_string());
    }
    parts.concat()
}

pub(crate) struct DeleteFiles;

define_tool!(
    DeleteFiles,
    "delete_files",
    "Delete files or directories from the local workspace. Supports literal paths and glob patterns (auto-detected via presence of wildcard characters `*`, `?`, `[`). Glob patterns without `/` are matched against the file's basename (e.g. `*.log` matches `.log` files at any depth). Patterns with `/` are matched against the full path from the working directory.",
    DeleteFilesArgs,
    execute_delete_files_tool,
    "core",
    describe_delete_files_invocation
);

#[cfg(test)]
mod tests {
    use super::*;

    // ── delete_files tests ────────────────────────────────────────────

    #[test]
    fn delete_files_single_literal() {
        let dir = tempfile::TempDir::new().unwrap();
        std::fs::write(dir.path().join("test.txt"), "hello").unwrap();
        let args = DeleteFilesArgs {
            targets: vec!["test.txt".into()],
            recursive: None,
        };
        let result = super::execute_delete_files_tool(&args, Some(dir.path())).unwrap();
        assert!(result.contains("Deleted 1 item(s)"));
        assert!(result.contains("test.txt"));
        assert!(!dir.path().join("test.txt").exists());
    }

    #[test]
    fn delete_files_multiple_literals() {
        let dir = tempfile::TempDir::new().unwrap();
        std::fs::write(dir.path().join("a.txt"), "").unwrap();
        std::fs::write(dir.path().join("b.txt"), "").unwrap();
        let args = DeleteFilesArgs {
            targets: vec!["a.txt".into(), "b.txt".into()],
            recursive: None,
        };
        let result = super::execute_delete_files_tool(&args, Some(dir.path())).unwrap();
        assert!(result.contains("Deleted 2 item(s)"));
        assert!(!dir.path().join("a.txt").exists());
        assert!(!dir.path().join("b.txt").exists());
    }

    #[test]
    fn delete_files_glob_pattern() {
        let dir = tempfile::TempDir::new().unwrap();
        std::fs::write(dir.path().join("a.txt"), "").unwrap();
        std::fs::write(dir.path().join("b.txt"), "").unwrap();
        std::fs::write(dir.path().join("c.rs"), "").unwrap();
        let args = DeleteFilesArgs {
            targets: vec!["*.txt".into()],
            recursive: None,
        };
        let result = super::execute_delete_files_tool(&args, Some(dir.path())).unwrap();
        assert!(result.contains("Deleted 2 item(s)"));
        assert!(!dir.path().join("a.txt").exists());
        assert!(!dir.path().join("b.txt").exists());
        assert!(dir.path().join("c.rs").exists());
    }

    #[test]
    fn delete_files_recursive_directory() {
        let dir = tempfile::TempDir::new().unwrap();
        let sub = dir.path().join("subdir");
        std::fs::create_dir(&sub).unwrap();
        std::fs::write(sub.join("nested.txt"), "").unwrap();
        let args = DeleteFilesArgs {
            targets: vec!["subdir".into()],
            recursive: Some(true),
        };
        let result = super::execute_delete_files_tool(&args, Some(dir.path())).unwrap();
        assert!(result.contains("Deleted 1 item(s)"));
        assert!(!sub.exists());
    }

    #[test]
    fn delete_files_directory_without_recursive_errors() {
        let dir = tempfile::TempDir::new().unwrap();
        std::fs::create_dir(dir.path().join("subdir")).unwrap();
        let args = DeleteFilesArgs {
            targets: vec!["subdir".into()],
            recursive: None,
        };
        let result = super::execute_delete_files_tool(&args, Some(dir.path())).unwrap();
        assert!(result.contains("Failed to delete 1 item(s)"));
        assert!(result.contains("set recursive=true"));
        assert!(dir.path().join("subdir").exists());
    }

    #[test]
    fn delete_files_nonexistent_path() {
        let dir = tempfile::TempDir::new().unwrap();
        let args = DeleteFilesArgs {
            targets: vec!["nonexistent.txt".into()],
            recursive: None,
        };
        let result = super::execute_delete_files_tool(&args, Some(dir.path())).unwrap();
        assert!(result.contains("Failed to delete 1 item(s)"));
    }

    #[test]
    fn delete_files_relative_path_resolves_against_working_dir() {
        // In-process path confinement was removed (the OS-level sandbox is now
        // the boundary), so `..` paths resolve relative to the working dir and
        // are no longer rejected here.
        let base = tempfile::TempDir::new().unwrap();
        let workspace = base.path().join("workspace");
        std::fs::create_dir(&workspace).unwrap();
        std::fs::write(base.path().join("outside.txt"), "").unwrap();
        let args = DeleteFilesArgs {
            targets: vec!["../outside.txt".into()],
            recursive: None,
        };
        let result = super::execute_delete_files_tool(&args, Some(&workspace)).unwrap();
        assert!(result.contains("Deleted 1 item(s)"), "{}", result);
        assert!(!base.path().join("outside.txt").exists());
    }

    #[test]
    fn delete_files_empty_targets_errors() {
        let args = DeleteFilesArgs {
            targets: vec![],
            recursive: None,
        };
        let result = super::execute_delete_files_tool(&args, Some(std::path::Path::new("/tmp")));
        assert!(result.is_err());
        assert!(result.unwrap_err().0.contains("at least one target"));
    }

    #[test]
    fn delete_files_combined_literal_and_glob() {
        let dir = tempfile::TempDir::new().unwrap();
        std::fs::write(dir.path().join("keep.rs"), "").unwrap();
        std::fs::write(dir.path().join("remove.txt"), "").unwrap();
        std::fs::write(dir.path().join("also_delete.rs"), "").unwrap();
        let args = DeleteFilesArgs {
            targets: vec!["also_delete.rs".into(), "*.txt".into()],
            recursive: None,
        };
        let result = super::execute_delete_files_tool(&args, Some(dir.path())).unwrap();
        assert!(result.contains("Deleted 2 item(s)"));
        assert!(!dir.path().join("remove.txt").exists());
        assert!(!dir.path().join("also_delete.rs").exists());
        assert!(dir.path().join("keep.rs").exists());
    }

    #[test]
    fn delete_files_glob_recursive() {
        let dir = tempfile::TempDir::new().unwrap();
        std::fs::write(dir.path().join("a.log"), "").unwrap();
        let sub = dir.path().join("sub");
        std::fs::create_dir(&sub).unwrap();
        std::fs::write(sub.join("b.log"), "").unwrap();
        let args = DeleteFilesArgs {
            targets: vec!["**/*.log".into()],
            recursive: None,
        };
        let result = super::execute_delete_files_tool(&args, Some(dir.path())).unwrap();
        assert!(result.contains("Deleted 2 item(s)"));
        assert!(!dir.path().join("a.log").exists());
        assert!(!sub.join("b.log").exists());
    }

    #[test]
    fn delete_files_glob_nothing_matches() {
        let dir = tempfile::TempDir::new().unwrap();
        std::fs::write(dir.path().join("a.rs"), "").unwrap();
        let args = DeleteFilesArgs {
            targets: vec!["*.py".into()],
            recursive: None,
        };
        let result = super::execute_delete_files_tool(&args, Some(dir.path())).unwrap();
        assert_eq!(result, "No matching files found to delete.");
    }

    #[test]
    fn describe_delete_files_single_literal() {
        let args = DeleteFilesArgs {
            targets: vec!["old.rs".into()],
            recursive: None,
        };
        let desc = super::describe_delete_files_invocation(&args);
        assert_eq!(desc, "Deleting file `old.rs`.");
    }

    #[test]
    fn describe_delete_files_multiple_literals() {
        let args = DeleteFilesArgs {
            targets: vec!["a.rs".into(), "b.rs".into()],
            recursive: None,
        };
        let desc = super::describe_delete_files_invocation(&args);
        assert!(desc.contains("Deleting 2 path(s)."));
    }

    #[test]
    fn describe_delete_files_with_glob() {
        let args = DeleteFilesArgs {
            targets: vec!["specific.txt".into(), "*.bak".into()],
            recursive: Some(true),
        };
        let desc = super::describe_delete_files_invocation(&args);
        assert!(desc.contains("Deleting 1 path(s)."));
        assert!(desc.contains("Expanding 1 glob pattern(s)."));
        assert!(desc.contains("Recursive mode enabled."));
    }

    #[test]
    fn delete_files_dedup() {
        let dir = tempfile::TempDir::new().unwrap();
        std::fs::write(dir.path().join("dup.txt"), "").unwrap();
        let args = DeleteFilesArgs {
            targets: vec!["dup.txt".into(), "dup.txt".into()],
            recursive: None,
        };
        let result = super::execute_delete_files_tool(&args, Some(dir.path())).unwrap();
        assert!(result.contains("Deleted 1 item(s)"));
    }

    #[test]
    fn delete_files_glob_without_separator_matches_basename_at_any_depth() {
        let dir = tempfile::TempDir::new().unwrap();
        // Root-level file
        std::fs::write(dir.path().join("a.log"), "").unwrap();
        // File in a subdirectory
        let sub = dir.path().join("sub");
        std::fs::create_dir(&sub).unwrap();
        std::fs::write(sub.join("b.log"), "").unwrap();

        // Pattern has no `/` — matched by basename at any depth
        let args = DeleteFilesArgs {
            targets: vec!["*.log".into()],
            recursive: None,
        };
        let result = super::execute_delete_files_tool(&args, Some(dir.path())).unwrap();
        assert!(
            result.contains("Deleted 2 item(s)"),
            "expected 2 deletes, got:\n{result}"
        );
        assert!(!dir.path().join("a.log").exists());
        assert!(!sub.join("b.log").exists());
    }

    #[test]
    fn delete_files_glob_with_separator_matches_full_path() {
        let dir = tempfile::TempDir::new().unwrap();
        // Root-level file (should NOT match)
        std::fs::write(dir.path().join("data.txt"), "").unwrap();
        // File in sub/subdir (SHOULD match `*/sub/*.txt`)
        let inner = dir.path().join("sub").join("inner");
        std::fs::create_dir_all(&inner).unwrap();
        std::fs::write(inner.join("data.txt"), "").unwrap();

        // Pattern has `/` — matched against the full absolute path.
        // `*/sub/*.txt` matches files where `sub/` is between two segments.
        let args = DeleteFilesArgs {
            targets: vec!["*/sub/*.txt".into()],
            recursive: None,
        };
        let result = super::execute_delete_files_tool(&args, Some(dir.path())).unwrap();
        assert!(
            result.contains("Deleted 1 item(s)"),
            "expected 1 delete, got:\n{result}"
        );
        assert!(
            dir.path().join("data.txt").exists(),
            "root-level data.txt should NOT have been deleted"
        );
        assert!(!inner.join("data.txt").exists());
    }
}
