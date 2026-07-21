use super::{ToolExecError, confine_path, sha256_hex, truncate_tool_output};
use schemars::JsonSchema;
use serde::Deserialize;
use std::{fs::OpenOptions, io::Write};
use std::{
    io,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ReadFileArgs {
    /// Relative or absolute path to a text file
    pub path: String,
}

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
pub struct ReadFileRangeArgs {
    /// Relative or absolute path to a text file
    pub path: String,
    /// 1-based inclusive start line
    pub start_line: usize,
    /// Maximum number of lines to return
    pub max_lines: usize,
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

pub(crate) fn execute_read_file_tool(
    args: &ReadFileArgs,
    working_dir: Option<&Path>,
) -> Result<String, ToolExecError> {
    if args.path.trim().is_empty() {
        return Err(ToolExecError(
            "missing required string argument: path".to_string(),
        ));
    }
    let resolved = confine_path(&args.path, working_dir)?;
    let content = std::fs::read_to_string(&resolved)?;
    Ok(truncate_tool_output(&content))
}

pub(crate) fn execute_read_file_range_tool(
    args: &ReadFileRangeArgs,
    working_dir: Option<&Path>,
) -> Result<String, ToolExecError> {
    const MAX_READ_FILE_RANGE_LINES: usize = 200;

    if args.path.trim().is_empty() {
        return Err(ToolExecError(
            "missing required string argument: path".to_string(),
        ));
    }

    if args.start_line == 0 {
        return Err(ToolExecError("start_line must be >= 1".to_string()));
    }

    if args.max_lines == 0 {
        return Err(ToolExecError("max_lines must be >= 1".to_string()));
    }

    if args.max_lines > MAX_READ_FILE_RANGE_LINES {
        return Err(ToolExecError(format!(
            "max_lines must be <= {MAX_READ_FILE_RANGE_LINES}"
        )));
    }

    let resolved = confine_path(&args.path, working_dir)?;
    let content = std::fs::read_to_string(&resolved)?;

    let lines: Vec<&str> = content.lines().collect();
    let total_lines = lines.len();

    if args.start_line > total_lines {
        return Err(ToolExecError(format!(
            "start_line {} is past end of file; file has {} lines",
            args.start_line, total_lines
        )));
    }

    let end_line = total_lines.min(args.start_line + args.max_lines - 1);
    let start_idx = args.start_line - 1;
    let end_idx = end_line;

    let mut output = format!(
        "path: {}\nlines: {}-{} of {}\n\n",
        resolved.display(),
        args.start_line,
        end_line,
        total_lines
    );

    for (index, line) in lines[start_idx..end_idx].iter().enumerate() {
        let line_number = args.start_line + index;
        output.push_str(&format!("{line_number} | {line}\n"));
    }

    Ok(truncate_tool_output(&output))
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

pub(crate) fn execute_write_file_tool(
    args: &WriteFileArgs,
    working_dir: Option<&Path>,
) -> Result<String, ToolExecError> {
    let path = validate_nonempty_path(&args.path)?;
    let resolved = confine_path(&path, working_dir)?;
    ensure_parent_directories(&resolved, args.create_parents.unwrap_or(true))?;

    match write_text_file(&resolved, &args.content, args.overwrite.unwrap_or(true)) {
        Ok(()) => Ok(format!("wrote file: {}", resolved.display())),
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
    let temp_path = temporary_sibling_path(path);
    let write_result = (|| -> io::Result<()> {
        let mut temp_file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp_path)?;
        temp_file.write_all(content.as_bytes())?;
        temp_file.flush()?;
        drop(temp_file);
        std::fs::rename(&temp_path, path)
    })();

    match write_result {
        Ok(()) => Ok(()),
        Err(error) => {
            let _ = std::fs::remove_file(&temp_path);
            Err(error)
        }
    }
}

fn temporary_sibling_path(path: &Path) -> PathBuf {
    let file_name = path
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or("file");
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    path.with_file_name(format!(".{file_name}.tai-tmp-{unique}"))
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
            out.push_str("\n\n");
            out.push_str(&diff);
        }
    }

    out
}

pub fn describe_read_file_invocation(args: &ReadFileArgs) -> String {
    format!("Reading file `{}`.", args.path)
}

pub(crate) struct ReadFile;

define_tool!(
    ReadFile,
    "read_file",
    "Read a UTF-8 text file from the local workspace.",
    ReadFileArgs,
    execute_read_file_tool,
    "core",
    describe_read_file_invocation
);

pub fn describe_read_file_range_invocation(args: &ReadFileRangeArgs) -> String {
    format!(
        "Reading file `{}` from line {} (max {} lines).",
        args.path, args.start_line, args.max_lines
    )
}

pub(crate) struct ReadFileRange;

define_tool!(
    ReadFileRange,
    "read_file_range",
    "Read a line range from a UTF-8 text file in the local workspace.",
    ReadFileRangeArgs,
    execute_read_file_range_tool,
    "core",
    describe_read_file_range_invocation
);

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
    fn describe_read_file_invocation() {
        let args = ReadFileArgs { path: "src/main.rs".into() };
        let desc = super::describe_read_file_invocation(&args);
        assert_eq!(desc, "Reading file `src/main.rs`.");
    }

    #[test]
    fn describe_read_file_range_invocation() {
        let args = ReadFileRangeArgs { path: "src/lib.rs".into(), start_line: 10, max_lines: 50 };
        let desc = super::describe_read_file_range_invocation(&args);
        assert_eq!(desc, "Reading file `src/lib.rs` from line 10 (max 50 lines).");
    }

    #[test]
    fn describe_list_files_invocation_with_path() {
        let args = ListFilesArgs { path: Some("src".into()) };
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
        let args = LineCountArgs { path: "Cargo.toml".into() };
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
        assert_eq!(desc, "Writing 11 bytes to file `output.txt` (overwrite: true, create_parents: false).");
    }

    #[test]
    fn describe_edit_file_invocation_with_sha_dry_run() {
        let args = EditFileArgs {
            path: "src/main.rs".into(),
            edits: vec![TextEditArgs { old_text: "foo".into(), new_text: "bar".into(), replace_all: Some(false) }],
            expected_sha256: Some("abc123".into()),
            dry_run: Some(true),
        };
        let desc = super::describe_edit_file_invocation(&args);
        assert!(desc.contains("Editing file `src/main.rs`"));
        assert!(desc.contains("1 edit(s)"));
        assert!(desc.contains("Expecting SHA-256: abc123"));
        assert!(desc.contains("Dry run"));
    }
}
