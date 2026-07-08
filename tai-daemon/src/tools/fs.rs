use super::{ToolError, ToolResult, resolve_path, sha256_hex, tool_ok, truncate_tool_output};
use serde::Deserialize;
use std::{fs::OpenOptions, io::Write};
use std::{
    io,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

#[derive(Debug, Deserialize)]
struct WriteFileArgs {
    path: String,
    content: String,
    overwrite: Option<bool>,
    create_parents: Option<bool>,
}

#[derive(Debug, Deserialize)]
struct EditFileArgs {
    path: String,
    edits: Vec<TextEditArgs>,
    expected_sha256: Option<String>,
    dry_run: Option<bool>,
}

#[derive(Debug, Deserialize)]
struct TextEditArgs {
    old_text: String,
    new_text: String,
    replace_all: Option<bool>,
}

#[derive(Debug, Deserialize)]
struct ReadFileRangeArgs {
    path: String,
    start_line: usize,
    max_lines: usize,
}

#[derive(Debug, Deserialize)]
struct LineCountArgs {
    path: String,
}

pub(crate) fn execute_line_count_tool(
    arguments_json: &str,
    cwd: Option<&std::path::Path>,
) -> ToolResult {
    match execute_line_count_inner(arguments_json, cwd) {
        Ok(content) => tool_ok(content),
        Err(error) => error.into(),
    }
}

fn execute_line_count_inner(
    arguments_json: &str,
    cwd: Option<&std::path::Path>,
) -> Result<String, ToolError> {
    let args: LineCountArgs = serde_json::from_str(arguments_json)?;
    if args.path.trim().is_empty() {
        return Err(ToolError::Other(
            "missing required string argument: path".to_string(),
        ));
    }
    let resolved = resolve_path(&args.path, cwd);
    let content = std::fs::read_to_string(&resolved)?;
    let line_count = content.lines().count();
    Ok(format!("{}: {} lines", resolved.display(), line_count))
}

pub(crate) fn execute_read_file_tool(
    arguments_json: &str,
    cwd: Option<&std::path::Path>,
) -> ToolResult {
    match execute_read_file_inner(arguments_json, cwd) {
        Ok(content) => tool_ok(content),
        Err(error) => error.into(),
    }
}

fn execute_read_file_inner(
    arguments_json: &str,
    cwd: Option<&std::path::Path>,
) -> Result<String, ToolError> {
    let path = serde_json::from_str::<serde_json::Value>(arguments_json)
        .ok()
        .and_then(|value| {
            value
                .get("path")
                .and_then(|value| value.as_str())
                .map(str::to_string)
        })
        .filter(|p| !p.trim().is_empty())
        .ok_or_else(|| ToolError::Other("missing required string argument: path".to_string()))?;
    let resolved = resolve_path(&path, cwd);
    let content = std::fs::read_to_string(&resolved)?;
    Ok(truncate_tool_output(&content))
}

pub(crate) fn execute_read_file_range_tool(
    arguments_json: &str,
    cwd: Option<&std::path::Path>,
) -> ToolResult {
    match execute_read_file_range_inner(arguments_json, cwd) {
        Ok(content) => tool_ok(content),
        Err(error) => error.into(),
    }
}

fn execute_read_file_range_inner(
    arguments_json: &str,
    cwd: Option<&std::path::Path>,
) -> Result<String, ToolError> {
    const MAX_READ_FILE_RANGE_LINES: usize = 200;

    let args: ReadFileRangeArgs = serde_json::from_str(arguments_json)?;
    if args.path.trim().is_empty() {
        return Err(ToolError::Other(
            "missing required string argument: path".to_string(),
        ));
    }

    if args.start_line == 0 {
        return Err(ToolError::Other("start_line must be >= 1".to_string()));
    }

    if args.max_lines == 0 {
        return Err(ToolError::Other("max_lines must be >= 1".to_string()));
    }

    if args.max_lines > MAX_READ_FILE_RANGE_LINES {
        return Err(ToolError::Other(format!(
            "max_lines must be <= {MAX_READ_FILE_RANGE_LINES}"
        )));
    }

    let resolved = resolve_path(&args.path, cwd);
    let content = std::fs::read_to_string(&resolved)?;

    let lines: Vec<&str> = content.lines().collect();
    let total_lines = lines.len();

    if args.start_line > total_lines {
        return Err(ToolError::Other(format!(
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
    arguments_json: &str,
    cwd: Option<&std::path::Path>,
) -> ToolResult {
    match execute_list_files_inner(arguments_json, cwd) {
        Ok(content) => tool_ok(content),
        Err(error) => error.into(),
    }
}

fn execute_list_files_inner(
    arguments_json: &str,
    cwd: Option<&std::path::Path>,
) -> Result<String, ToolError> {
    let path = serde_json::from_str::<serde_json::Value>(arguments_json)
        .ok()
        .and_then(|value| {
            value
                .get("path")
                .and_then(|value| value.as_str())
                .map(str::to_string)
        })
        .unwrap_or_else(|| ".".to_string());
    let resolved = resolve_path(&path, cwd);
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
    arguments_json: &str,
    cwd: Option<&std::path::Path>,
) -> ToolResult {
    match execute_write_file_inner(arguments_json, cwd) {
        Ok(content) => tool_ok(content),
        Err(error) => error.into(),
    }
}

fn execute_write_file_inner(
    arguments_json: &str,
    cwd: Option<&std::path::Path>,
) -> Result<String, ToolError> {
    let args: WriteFileArgs = serde_json::from_str(arguments_json)?;

    let path = validate_nonempty_path(&args.path)?;
    let resolved = resolve_path(&path, cwd);
    ensure_parent_directories(&resolved, args.create_parents.unwrap_or(true))?;

    match write_text_file(&resolved, &args.content, args.overwrite.unwrap_or(true)) {
        Ok(()) => Ok(format!("wrote file: {}", resolved.display())),
        Err(error) => {
            let overwrite = args.overwrite.unwrap_or(true);
            if !overwrite && error.kind() == io::ErrorKind::AlreadyExists {
                Err(ToolError::Other(format!(
                    "refusing to overwrite existing file: {}",
                    resolved.display()
                )))
            } else {
                Err(ToolError::Io(error))
            }
        }
    }
}

pub(crate) fn execute_edit_file_tool(
    arguments_json: &str,
    cwd: Option<&std::path::Path>,
) -> ToolResult {
    match execute_edit_file_inner(arguments_json, cwd) {
        Ok(content) => tool_ok(content),
        Err(error) => error.into(),
    }
}

fn execute_edit_file_inner(
    arguments_json: &str,
    cwd: Option<&std::path::Path>,
) -> Result<String, ToolError> {
    let args: EditFileArgs = serde_json::from_str(arguments_json)?;

    let path = validate_nonempty_path(&args.path)?;

    if args.edits.is_empty() {
        return Err(ToolError::Other(
            "missing required array argument: edits".to_string(),
        ));
    }

    let resolved = resolve_path(&path, cwd);
    let original_content = std::fs::read_to_string(&resolved)?;

    if let Some(expected_sha256) = args.expected_sha256.as_deref() {
        let actual_sha256 = sha256_hex(&original_content);
        if actual_sha256 != expected_sha256.trim().to_ascii_lowercase() {
            return Err(ToolError::Other(format!(
                "expected_sha256 mismatch for {}: expected {}, got {}",
                resolved.display(),
                expected_sha256.trim(),
                actual_sha256
            )));
        }
    }

    let edit_summary =
        apply_text_edits(&original_content, &args.edits).map_err(ToolError::Other)?;

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
        Err(error) => Err(ToolError::Io(error)),
    }
}

fn validate_nonempty_path(path: &str) -> Result<String, ToolError> {
    let trimmed = path.trim();
    if trimmed.is_empty() {
        Err(ToolError::Other(
            "missing required string argument: path".to_string(),
        ))
    } else {
        Ok(trimmed.to_string())
    }
}

fn ensure_parent_directories(path: &Path, create_parents: bool) -> Result<(), ToolError> {
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

define_tool!(
    ReadFile,
    "read_file",
    "Read a UTF-8 text file from the local workspace.",
    execute_read_file_tool,
    serde_json::json!({
        "type": "object",
        "properties": {
            "path": {
                "type": "string",
                "description": "Relative or absolute path to a text file"
            }
        },
        "required": ["path"],
        "additionalProperties": false
    }),
    "core"
);

define_tool!(
    ReadFileRange,
    "read_file_range",
    "Read a line range from a UTF-8 text file in the local workspace.",
    execute_read_file_range_tool,
    serde_json::json!({
        "type": "object",
        "properties": {
            "path": {
                "type": "string",
                "description": "Relative or absolute path to a text file"
            },
            "start_line": {
                "type": "integer",
                "minimum": 1,
                "description": "1-based inclusive start line"
            },
            "max_lines": {
                "type": "integer",
                "minimum": 1,
                "maximum": 200,
                "description": "Maximum number of lines to return"
            }
        },
        "required": ["path", "start_line", "max_lines"],
        "additionalProperties": false
    }),
    "core"
);

define_tool!(
    ListFiles,
    "list_files",
    "List files in a local directory.",
    execute_list_files_tool,
    serde_json::json!({
        "type": "object",
        "properties": {
            "path": {
                "type": "string",
                "description": "Relative or absolute path to a directory",
                "default": "."
            }
        },
        "additionalProperties": false
    }),
    "core"
);

define_tool!(
    LineCount,
    "line_count",
    "Count the number of lines in a UTF-8 text file.",
    execute_line_count_tool,
    serde_json::json!({
        "type": "object",
        "properties": {
            "path": {
                "type": "string",
                "description": "Relative or absolute path to a text file"
            }
        },
        "required": ["path"],
        "additionalProperties": false
    }),
    "core"
);

define_tool!(
    WriteFile,
    "write_file",
    "Write a UTF-8 text file to the local workspace.",
    execute_write_file_tool,
    serde_json::json!({
        "type": "object",
        "properties": {
            "path": {
                "type": "string",
                "description": "Relative or absolute path to the file to write"
            },
            "content": {
                "type": "string",
                "description": "Full UTF-8 file contents to write"
            },
            "overwrite": {
                "type": "boolean",
                "description": "Whether to overwrite an existing file",
                "default": true
            },
            "create_parents": {
                "type": "boolean",
                "description": "Whether to create missing parent directories",
                "default": true
            }
        },
        "required": ["path", "content"],
        "additionalProperties": false
    }),
    "core"
);

define_tool!(
    EditFile,
    "edit_file",
    "Edit a UTF-8 text file by applying one or more exact text replacements. Each edit must match at least once; non-replace_all edits must match exactly once.",
    execute_edit_file_tool,
    serde_json::json!({
        "type": "object",
        "properties": {
            "path": {
                "type": "string",
                "description": "Relative or absolute path to the file to edit"
            },
            "edits": {
                "type": "array",
                "minItems": 1,
                "items": {
                    "type": "object",
                    "properties": {
                        "old_text": {
                            "type": "string",
                            "description": "Exact text to replace"
                        },
                        "new_text": {
                            "type": "string",
                            "description": "Replacement text"
                        },
                        "replace_all": {
                            "type": "boolean",
                            "description": "When true, replace all exact matches instead of requiring exactly one match",
                            "default": false
                        }
                    },
                    "required": ["old_text", "new_text"],
                    "additionalProperties": false
                }
            },
            "expected_sha256": {
                "type": "string",
                "description": "Optional lowercase hex SHA-256 of the file before editing"
            },
            "dry_run": {
                "type": "boolean",
                "description": "When true, validate and preview the edit without writing the file",
                "default": false
            }
        },
        "required": ["path", "edits"],
        "additionalProperties": false
    }),
    "core"
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
}
