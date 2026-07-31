use super::{ToolExecError, confine_path, sha256_hex, truncate_tool_output};
use schemars::JsonSchema;
use serde::Deserialize;
use std::{fs::OpenOptions, io::Write};
use std::{io, path::Path};
use tracing::{error, info, warn};
use zlob::ZlobFlags;
use zlob::walk::{WalkBuilder, WalkFlags, WalkState};

use super::glob_util::GlobFilter;

#[derive(Debug, Deserialize, JsonSchema)]
pub struct WriteFileArgs {
    /// Relative or absolute path to the file to write
    pub path: String,
    /// Full UTF-8 file contents to write
    pub content: String,
    /// Whether to overwrite an existing file
    pub overwrite: Option<bool>,
    /// Whether to create missing parent directories
    pub create_parents: Option<bool>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct EditFileArgs {
    /// Relative or absolute path to the file to edit
    pub path: String,
    /// One or more exact text replacements to apply
    pub edits: Vec<TextEditArgs>,
    /// If set, the tool will verify the file matches this SHA-256 before editing (safety check)
    pub expected_sha256: Option<String>,
    /// When true, preview changes without applying them
    pub dry_run: Option<bool>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct TextEditArgs {
    /// Exact text to replace (must match at least once)
    pub old_text: String,
    /// Replacement text
    pub new_text: String,
    /// When true, replace all exact matches instead of requiring exactly one match
    pub replace_all: Option<bool>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct LineCountArgs {
    /// Relative or absolute path to a text file
    pub path: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ListFilesArgs {
    /// Relative or absolute path to a directory (defaults to working directory)
    pub path: Option<String>,
}

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

pub(crate) fn execute_line_count_tool(
    args: &LineCountArgs,
    working_dir: Option<&Path>,
) -> Result<String, ToolExecError> {
    if args.path.trim().is_empty() {
        return Err(ToolExecError(
            "missing required string argument: path".to_string(),
        ));
    }
    let resolved = confine_path(&args.path, working_dir)?;
    let content = std::fs::read_to_string(&resolved)?;
    let line_count = content.lines().count();
    Ok(format!("{}: {} lines", resolved.display(), line_count))
}

pub(crate) fn execute_list_files_tool(
    args: &ListFilesArgs,
    working_dir: Option<&Path>,
) -> Result<String, ToolExecError> {
    let path = args.path.as_deref().unwrap_or(".");
    let resolved = confine_path(path, working_dir)?;
    let entries = std::fs::read_dir(&resolved)?;
    let mut names = Vec::new();
    for entry in entries {
        let entry = entry?;
        let mut name = entry.file_name().to_string_lossy().to_string();
        if entry.file_type().map(|kind| kind.is_dir()).unwrap_or(false) {
            name.push('/');
        }
        names.push(name);
    }
    names.sort();
    Ok(truncate_tool_output(&names.join("\n")))
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
            // Literal paths are confined immediately, rejecting directory
            // traversal attempts before any I/O occurs.
            targets.push(confine_path(trimmed, working_dir)?);
        }
    }

    // Expand glob patterns via a single directory walk anchored at the working dir.
    if !glob_patterns.is_empty() {
        // Use confine_path for the walk root (rather than resolve_path) so the
        // anchor itself is validated — defense-in-depth against misconfiguration.
        let wd = confine_path(".", working_dir)?;

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

        // Confine each glob-expanded path — defense-in-depth. Although the walk
        // is rooted in the working dir, a symlink-to-parent or bind-mount could
        // cause the walker to visit paths outside it. We filter them here so a
        // malicious or accidental symlink cannot escalate a glob beyond the
        // session boundary.
        for path in matched {
            match confine_path(&path.to_string_lossy(), working_dir) {
                Ok(confined) => targets.push(confined),
                Err(e) => warn!(
                    "skipping glob match '{}' — outside working directory: {e}",
                    path.display()
                ),
            }
        }
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

pub(crate) fn execute_write_file_tool(
    args: &WriteFileArgs,
    working_dir: Option<&Path>,
) -> Result<String, ToolExecError> {
    let path = validate_nonempty_path(&args.path)?;
    let resolved = confine_path(&path, working_dir)?;
    ensure_parent_directories(&resolved, args.create_parents.unwrap_or(true))?;

    match write_text_file(&resolved, &args.content, args.overwrite.unwrap_or(true)) {
        Ok(()) => {
            let lang = ext_to_lang(&resolved.display().to_string());
            let fenced = fence_content(&args.content, lang);
            Ok(format!("wrote file: {}\n\n{}", resolved.display(), fenced))
        }
        Err(error) => {
            let overwrite = args.overwrite.unwrap_or(true);
            if !overwrite && error.kind() == io::ErrorKind::AlreadyExists {
                Err(ToolExecError(format!(
                    "refusing to overwrite existing file: {}",
                    resolved.display()
                )))
            } else {
                Err(ToolExecError(format!("{error}")))
            }
        }
    }
}

pub(crate) fn execute_edit_file_tool(
    args: &EditFileArgs,
    working_dir: Option<&Path>,
) -> Result<String, ToolExecError> {
    let path = validate_nonempty_path(&args.path)?;

    if args.edits.is_empty() {
        return Err(ToolExecError(
            "missing required array argument: edits".to_string(),
        ));
    }

    let resolved = confine_path(&path, working_dir)?;
    let original_content = std::fs::read_to_string(&resolved)?;

    if let Some(expected_sha256) = args.expected_sha256.as_deref() {
        let actual_sha256 = sha256_hex(&original_content);
        if actual_sha256 != expected_sha256.trim().to_ascii_lowercase() {
            return Err(ToolExecError(format!(
                "expected_sha256 mismatch for {}: expected {}, got {}",
                resolved.display(),
                expected_sha256.trim(),
                actual_sha256
            )));
        }
    }

    let edit_summary = apply_text_edits(&original_content, &args.edits).map_err(ToolExecError)?;

    if args.dry_run.unwrap_or(false) {
        return Ok(format_edit_result(
            "would edit",
            &resolved.display().to_string(),
            &edit_summary,
        ));
    }

    match write_text_file(&resolved, &edit_summary.content, true) {
        Ok(()) => Ok(format_edit_result(
            "edited",
            &resolved.display().to_string(),
            &edit_summary,
        )),
        Err(error) => Err(ToolExecError(format!("{error}"))),
    }
}

fn validate_nonempty_path(path: &str) -> Result<String, ToolExecError> {
    let trimmed = path.trim();
    if trimmed.is_empty() {
        Err(ToolExecError(
            "missing required string argument: path".to_string(),
        ))
    } else {
        Ok(trimmed.to_string())
    }
}

fn ensure_parent_directories(path: &Path, create_parents: bool) -> Result<(), ToolExecError> {
    if !create_parents {
        return Ok(());
    }
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent)?;
    }
    Ok(())
}

fn write_text_file(path: &Path, content: &str, overwrite: bool) -> io::Result<()> {
    if !overwrite {
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        let mut file = options.open(path)?;
        file.write_all(content.as_bytes())?;
        file.flush()?;
        return Ok(());
    }

    atomic_write_text_file(path, content)
}

fn atomic_write_text_file(path: &Path, content: &str) -> io::Result<()> {
    let dir = path.parent().unwrap_or(Path::new("."));
    let mut tmp = tempfile::NamedTempFile::new_in(dir)?;
    tmp.write_all(content.as_bytes())?;
    tmp.flush()?;
    tmp.persist(path).map_err(|e| e.error)?;
    Ok(())
}

struct AppliedEditSummary {
    content: String,
    original: Option<String>,
    replacement_count: usize,
    char_delta: isize,
}

fn apply_text_edits(
    original_content: &str,
    edits: &[TextEditArgs],
) -> Result<AppliedEditSummary, String> {
    let original = original_content.to_string();
    let mut content = original.clone();
    let mut replacement_count = 0usize;
    let mut char_delta = 0isize;

    for (index, edit) in edits.iter().enumerate() {
        if edit.old_text.is_empty() {
            return Err(format!("edit {}: old_text must not be empty", index + 1));
        }

        let matches = content
            .match_indices(&edit.old_text)
            .map(|(offset, _)| offset)
            .collect::<Vec<_>>();
        if matches.is_empty() {
            return Err(format!("edit {}: old_text not found", index + 1));
        }

        let replace_all = edit.replace_all.unwrap_or(false);
        let replacements_for_edit = if replace_all {
            matches.len()
        } else {
            if matches.len() != 1 {
                return Err(format!(
                    "edit {}: old_text matched {} locations; edit is ambiguous",
                    index + 1,
                    matches.len()
                ));
            }
            1
        };

        content = if replace_all {
            content.replace(&edit.old_text, &edit.new_text)
        } else {
            content.replacen(&edit.old_text, &edit.new_text, 1)
        };
        replacement_count += replacements_for_edit;
        char_delta += (edit.new_text.chars().count() as isize
            - edit.old_text.chars().count() as isize)
            * replacements_for_edit as isize;
    }

    Ok(AppliedEditSummary {
        content,
        original: Some(original),
        replacement_count,
        char_delta,
    })
}

fn ext_to_lang(path: &str) -> &'static str {
    let p = std::path::Path::new(path);
    // Filename-based detection for files without extensions (e.g., Dockerfile).
    if let Some(fname) = p.file_name().and_then(|n| n.to_str())
        && fname.eq_ignore_ascii_case("dockerfile")
    {
        return "dockerfile";
    }
    let ext = p.extension().and_then(|e| e.to_str()).unwrap_or("");
    match ext.to_lowercase().as_str() {
        "rs" => "rust",
        "py" => "python",
        "js" => "javascript",
        "ts" | "tsx" | "mts" | "cts" => "javascript",
        "json" => "json",
        "toml" => "toml",
        "yaml" | "yml" => "yaml",
        "md" | "mdown" | "markdown" => "markdown",
        "html" | "htm" => "html",
        "css" => "css",
        "scss" | "sass" => "scss",
        "sh" | "bash" | "zsh" => "bash",
        "c" => "c",
        "h" => "c",
        "cpp" | "cc" | "cxx" | "hpp" => "cpp",
        "go" => "go",
        "rb" => "ruby",
        "java" => "java",
        "sql" => "sql",
        "xml" => "xml",
        "dockerfile" => "dockerfile",
        "makefile" | "mk" => "makefile",
        "lua" => "lua",
        "zig" => "zig",
        "swift" => "swift",
        "kt" | "kts" => "kotlin",
        "pl" | "pm" => "perl",
        "tex" => "latex",
        "proto" => "protobuf",
        _ => "",
    }
}

fn fence_content(content: &str, lang: &str) -> String {
    // Strip trailing newlines so the closing fence sits directly after the
    // last line of content, avoiding a blank line before the fence.
    let trimmed = content.trim_end_matches('\n');
    let max_run = trimmed
        .chars()
        .fold((0usize, 0usize), |(max_run, current), c| {
            if c == '`' {
                (max_run.max(current + 1), current + 1)
            } else {
                (max_run, 0)
            }
        })
        .0;
    let fence_len = (max_run + 1).max(3);
    let fence = "`".repeat(fence_len);
    format!("{fence}{lang}\n{trimmed}\n{fence}")
}

fn format_edit_result(action: &str, path: &str, summary: &AppliedEditSummary) -> String {
    let mut out = format!(
        "{action} file: {path} ({} replacement{}, {:+} chars)",
        summary.replacement_count,
        if summary.replacement_count == 1 {
            ""
        } else {
            "s"
        },
        summary.char_delta,
    );

    // Append diff if we have original content
    if let Some(ref original) = summary.original {
        let diff = crate::diff_util::generate_diff(original, &summary.content, path, path);
        if !diff.is_empty() {
            out.push_str("\n\n```diff\n");
            out.push_str(&diff);
            out.push_str("\n```");
        }
    }

    out
}

pub fn describe_list_files_invocation(args: &ListFilesArgs) -> String {
    match &args.path {
        Some(p) => format!("Listing files in `{}`.", p),
        None => "Listing files in the working directory.".to_string(),
    }
}

pub(crate) struct ListFiles;

define_tool!(
    ListFiles,
    "list_files",
    "List files in a local directory.",
    ListFilesArgs,
    execute_list_files_tool,
    "core",
    describe_list_files_invocation
);

pub fn describe_line_count_invocation(args: &LineCountArgs) -> String {
    format!("Counting lines in `{}`.", args.path)
}

pub(crate) struct LineCount;

define_tool!(
    LineCount,
    "line_count",
    "Count the number of lines in a UTF-8 text file.",
    LineCountArgs,
    execute_line_count_tool,
    "core",
    describe_line_count_invocation
);

pub fn describe_write_file_invocation(args: &WriteFileArgs) -> String {
    format!(
        "Writing {} bytes to file `{}` (overwrite: {}, create_parents: {}).",
        args.content.len(),
        args.path,
        args.overwrite.unwrap_or(false),
        args.create_parents.unwrap_or(false)
    )
}

pub(crate) struct WriteFile;

define_tool!(
    WriteFile,
    "write_file",
    "Write a UTF-8 text file to the local workspace.",
    WriteFileArgs,
    execute_write_file_tool,
    "core",
    describe_write_file_invocation
);

pub fn describe_edit_file_invocation(args: &EditFileArgs) -> String {
    let mut parts = vec![format!(
        "Editing file `{}` with {} edit(s).",
        args.path,
        args.edits.len()
    )];
    if let Some(ref sha) = args.expected_sha256 {
        parts.push(format!(" Expecting SHA-256: {}.", sha));
    }
    if args.dry_run.unwrap_or(false) {
        parts.push(" Dry run (no changes will be applied).".to_string());
    }
    parts.concat()
}

pub(crate) struct EditFile;

define_tool!(
    EditFile,
    "edit_file",
    "Edit a UTF-8 text file by applying one or more exact text replacements. Each edit must match at least once; non-replace_all edits must match exactly once.",
    EditFileArgs,
    execute_edit_file_tool,
    "core",
    describe_edit_file_invocation
);

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

    #[test]
    fn apply_text_edits_sets_original() {
        let summary = apply_text_edits(
            "hello world",
            &[TextEditArgs {
                old_text: "world".into(),
                new_text: "there".into(),
                replace_all: None,
            }],
        )
        .unwrap();
        assert_eq!(summary.original, Some("hello world".into()));
    }

    #[test]
    fn apply_text_edits_replaces_single_occurrence() {
        let summary = apply_text_edits(
            "a b c",
            &[TextEditArgs {
                old_text: "b".into(),
                new_text: "B".into(),
                replace_all: None,
            }],
        )
        .unwrap();
        assert_eq!(summary.content, "a B c");
        assert_eq!(summary.replacement_count, 1);
    }

    #[test]
    fn describe_list_files_invocation_with_path() {
        let args = ListFilesArgs {
            path: Some("src".into()),
        };
        let desc = super::describe_list_files_invocation(&args);
        assert_eq!(desc, "Listing files in `src`.");
    }

    #[test]
    fn describe_list_files_invocation_without_path() {
        let args = ListFilesArgs { path: None };
        let desc = super::describe_list_files_invocation(&args);
        assert_eq!(desc, "Listing files in the working directory.");
    }

    #[test]
    fn describe_line_count_invocation() {
        let args = LineCountArgs {
            path: "Cargo.toml".into(),
        };
        let desc = super::describe_line_count_invocation(&args);
        assert_eq!(desc, "Counting lines in `Cargo.toml`.");
    }

    #[test]
    fn describe_write_file_invocation() {
        let args = WriteFileArgs {
            path: "output.txt".into(),
            content: "hello world".into(),
            overwrite: Some(true),
            create_parents: Some(false),
        };
        let desc = super::describe_write_file_invocation(&args);
        assert_eq!(
            desc,
            "Writing 11 bytes to file `output.txt` (overwrite: true, create_parents: false)."
        );
    }

    #[test]
    fn describe_edit_file_invocation_with_sha_dry_run() {
        let args = EditFileArgs {
            path: "src/main.rs".into(),
            edits: vec![TextEditArgs {
                old_text: "foo".into(),
                new_text: "bar".into(),
                replace_all: Some(false),
            }],
            expected_sha256: Some("abc123".into()),
            dry_run: Some(true),
        };
        let desc = super::describe_edit_file_invocation(&args);
        assert!(desc.contains("Editing file `src/main.rs`"));
        assert!(desc.contains("1 edit(s)"));
        assert!(desc.contains("Expecting SHA-256: abc123"));
        assert!(desc.contains("Dry run"));
    }

    // ── ext_to_lang tests ────────────────────────────────────────────

    #[test]
    fn ext_to_lang_rust() {
        assert_eq!(super::ext_to_lang("src/main.rs"), "rust");
    }

    #[test]
    fn ext_to_lang_python() {
        assert_eq!(super::ext_to_lang("script.py"), "python");
    }

    #[test]
    fn ext_to_lang_javascript() {
        assert_eq!(super::ext_to_lang("app.js"), "javascript");
    }

    #[test]
    fn ext_to_lang_typescript_mapped_to_javascript() {
        assert_eq!(super::ext_to_lang("app.ts"), "javascript");
        assert_eq!(super::ext_to_lang("app.tsx"), "javascript");
        assert_eq!(super::ext_to_lang("app.mts"), "javascript");
        assert_eq!(super::ext_to_lang("app.cts"), "javascript");
    }

    #[test]
    fn ext_to_lang_dockerfile_detected_by_filename() {
        assert_eq!(super::ext_to_lang("Dockerfile"), "dockerfile");
        assert_eq!(super::ext_to_lang("path/to/Dockerfile"), "dockerfile");
    }

    #[test]
    fn ext_to_lang_dockerfile_extension() {
        assert_eq!(super::ext_to_lang("config.dockerfile"), "dockerfile");
    }

    #[test]
    fn ext_to_lang_unknown_extension() {
        assert_eq!(super::ext_to_lang("file.xyzzy"), "");
    }

    #[test]
    fn ext_to_lang_no_extension() {
        assert_eq!(super::ext_to_lang("Makefile"), "");
    }

    #[test]
    fn ext_to_lang_markdown() {
        assert_eq!(super::ext_to_lang("README.md"), "markdown");
    }

    #[test]
    fn ext_to_lang_shell() {
        assert_eq!(super::ext_to_lang("script.sh"), "bash");
        assert_eq!(super::ext_to_lang("script.bash"), "bash");
    }

    #[test]
    fn ext_to_lang_protobuf() {
        assert_eq!(super::ext_to_lang("message.proto"), "protobuf");
    }

    #[test]
    fn ext_to_lang_toml() {
        assert_eq!(super::ext_to_lang("Cargo.toml"), "toml");
    }

    #[test]
    fn ext_to_lang_yaml() {
        assert_eq!(super::ext_to_lang("config.yaml"), "yaml");
        assert_eq!(super::ext_to_lang("config.yml"), "yaml");
    }

    // ── fence_content tests ──────────────────────────────────────────

    #[test]
    fn fence_content_basic() {
        let result = super::fence_content("hello", "rust");
        assert_eq!(result, "```rust\nhello\n```");
    }

    #[test]
    fn fence_content_no_lang() {
        let result = super::fence_content("plain text", "");
        assert_eq!(result, "```\nplain text\n```");
    }

    #[test]
    fn fence_content_with_backticks() {
        let result = super::fence_content("`code`", "text");
        // Content contains a single backtick, so fence must be at least 2 wide.
        assert!(result.starts_with("``"));
        assert!(result.ends_with("``"));
        assert!(result.contains("`code`"));
    }

    #[test]
    fn fence_content_triple_backticks() {
        let result = super::fence_content("```\ncode\n```", "text");
        // Content contains 3 consecutive backticks, so fence must be at least 4 wide.
        assert!(result.starts_with("````"));
        assert!(result.ends_with("````"));
    }

    #[test]
    fn fence_content_empty_content() {
        let result = super::fence_content("", "json");
        assert_eq!(result, "```json\n\n```");
    }

    #[test]
    fn fence_content_trailing_newline_stripped() {
        let result = super::fence_content("hello\n", "text");
        // Should not have a blank line before the closing fence.
        assert_eq!(result, "```text\nhello\n```");
    }

    #[test]
    fn fence_content_multiple_trailing_newlines_stripped() {
        let result = super::fence_content("a\nb\n\n", "text");
        assert_eq!(result, "```text\na\nb\n```");
    }

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
    fn delete_files_outside_working_dir_rejected() {
        let dir = tempfile::TempDir::new().unwrap();
        let args = DeleteFilesArgs {
            targets: vec!["../outside.txt".into()],
            recursive: None,
        };
        let result = super::execute_delete_files_tool(&args, Some(dir.path()));
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.0.contains("outside the session working directory"));
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
