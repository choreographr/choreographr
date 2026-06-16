use super::{ToolResult, sha256_hex, truncate_tool_output};
use serde::Deserialize;
use std::{
    io,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};
use tokio::{fs::OpenOptions, io::AsyncWriteExt};

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

pub(crate) async fn execute_read_file_tool(arguments_json: &str) -> ToolResult {
    let path = match serde_json::from_str::<serde_json::Value>(arguments_json)
        .ok()
        .and_then(|value| {
            value
                .get("path")
                .and_then(|value| value.as_str())
                .map(str::to_string)
        }) {
        Some(path) if !path.trim().is_empty() => path,
        _ => {
            return ToolResult {
                content: "missing required string argument: path".to_string(),
                is_error: true,
            };
        }
    };
    match tokio::fs::read_to_string(&path).await {
        Ok(content) => ToolResult {
            content: truncate_tool_output(&content),
            is_error: false,
        },
        Err(error) => ToolResult {
            content: format!("failed to read {path}: {error}"),
            is_error: true,
        },
    }
}

pub(crate) async fn execute_read_file_range_tool(arguments_json: &str) -> ToolResult {
    const MAX_READ_FILE_RANGE_LINES: usize = 200;

    let args = match serde_json::from_str::<ReadFileRangeArgs>(arguments_json) {
        Ok(args) if !args.path.trim().is_empty() => args,
        Ok(_) => {
            return ToolResult {
                content: "missing required string argument: path".to_string(),
                is_error: true,
            };
        }
        Err(error) => {
            return ToolResult {
                content: format!("invalid arguments: {error}"),
                is_error: true,
            };
        }
    };

    if args.start_line == 0 {
        return ToolResult {
            content: "start_line must be >= 1".to_string(),
            is_error: true,
        };
    }

    if args.max_lines == 0 {
        return ToolResult {
            content: "max_lines must be >= 1".to_string(),
            is_error: true,
        };
    }

    if args.max_lines > MAX_READ_FILE_RANGE_LINES {
        return ToolResult {
            content: format!("max_lines must be <= {MAX_READ_FILE_RANGE_LINES}"),
            is_error: true,
        };
    }

    let content = match tokio::fs::read_to_string(&args.path).await {
        Ok(content) => content,
        Err(error) => {
            return ToolResult {
                content: format!("failed to read {}: {}", args.path, error),
                is_error: true,
            };
        }
    };

    let lines: Vec<&str> = content.lines().collect();
    let total_lines = lines.len();

    if args.start_line > total_lines {
        return ToolResult {
            content: format!(
                "start_line {} is past end of file; file has {} lines",
                args.start_line, total_lines
            ),
            is_error: true,
        };
    }

    let end_line = total_lines.min(args.start_line + args.max_lines - 1);
    let start_idx = args.start_line - 1;
    let end_idx = end_line;

    let mut output = format!(
        "path: {}\nlines: {}-{} of {}\n\n",
        args.path, args.start_line, end_line, total_lines
    );

    for (index, line) in lines[start_idx..end_idx].iter().enumerate() {
        let line_number = args.start_line + index;
        output.push_str(&format!("{line_number} | {line}\n"));
    }

    ToolResult {
        content: truncate_tool_output(&output),
        is_error: false,
    }
}

pub(crate) async fn execute_list_files_tool(arguments_json: &str) -> ToolResult {
    let path = serde_json::from_str::<serde_json::Value>(arguments_json)
        .ok()
        .and_then(|value| {
            value
                .get("path")
                .and_then(|value| value.as_str())
                .map(str::to_string)
        })
        .unwrap_or_else(|| ".".to_string());
    match tokio::fs::read_dir(&path).await {
        Ok(mut entries) => {
            let mut names = Vec::new();
            loop {
                match entries.next_entry().await {
                    Ok(Some(entry)) => {
                        let mut name = entry.file_name().to_string_lossy().to_string();
                        if entry
                            .file_type()
                            .await
                            .map(|kind| kind.is_dir())
                            .unwrap_or(false)
                        {
                            name.push('/');
                        }
                        names.push(name);
                    }
                    Ok(None) => break,
                    Err(error) => {
                        return ToolResult {
                            content: format!("failed to list {path}: {error}"),
                            is_error: true,
                        };
                    }
                }
            }
            names.sort();
            ToolResult {
                content: truncate_tool_output(&names.join("\n")),
                is_error: false,
            }
        }
        Err(error) => ToolResult {
            content: format!("failed to list {path}: {error}"),
            is_error: true,
        },
    }
}

pub(crate) async fn execute_write_file_tool(arguments_json: &str) -> ToolResult {
    let args = match serde_json::from_str::<WriteFileArgs>(arguments_json) {
        Ok(args) => args,
        Err(error) => {
            return ToolResult {
                content: format!("invalid arguments: {error}"),
                is_error: true,
            };
        }
    };

    let path = match validate_nonempty_path(&args.path) {
        Ok(path) => path,
        Err(result) => return result,
    };

    let path_ref = Path::new(&path);
    if let Err(error) =
        ensure_parent_directories(path_ref, args.create_parents.unwrap_or(true)).await
    {
        return ToolResult {
            content: format!("failed to create parent directories for {path}: {error}"),
            is_error: true,
        };
    }

    match write_text_file(path_ref, &args.content, args.overwrite.unwrap_or(true)).await {
        Ok(()) => ToolResult {
            content: format!("wrote file: {path}"),
            is_error: false,
        },
        Err(error) => ToolResult {
            content: format_write_error(&path, error, args.overwrite.unwrap_or(true)),
            is_error: true,
        },
    }
}

pub(crate) async fn execute_edit_file_tool(arguments_json: &str) -> ToolResult {
    let args = match serde_json::from_str::<EditFileArgs>(arguments_json) {
        Ok(args) => args,
        Err(error) => {
            return ToolResult {
                content: format!("invalid arguments: {error}"),
                is_error: true,
            };
        }
    };

    let path = match validate_nonempty_path(&args.path) {
        Ok(path) => path,
        Err(result) => return result,
    };

    if args.edits.is_empty() {
        return ToolResult {
            content: "missing required array argument: edits".to_string(),
            is_error: true,
        };
    }

    let path_ref = Path::new(&path);
    let original_content = match tokio::fs::read_to_string(path_ref).await {
        Ok(content) => content,
        Err(error) => {
            return ToolResult {
                content: format!("failed to read {path}: {error}"),
                is_error: true,
            };
        }
    };

    if let Some(expected_sha256) = args.expected_sha256.as_deref() {
        let actual_sha256 = sha256_hex(&original_content);
        if actual_sha256 != expected_sha256.trim().to_ascii_lowercase() {
            return ToolResult {
                content: format!(
                    "expected_sha256 mismatch for {path}: expected {}, got {}",
                    expected_sha256.trim(),
                    actual_sha256
                ),
                is_error: true,
            };
        }
    }

    let edit_summary = match apply_text_edits(&original_content, &args.edits) {
        Ok(summary) => summary,
        Err(error) => {
            return ToolResult {
                content: error,
                is_error: true,
            };
        }
    };

    if args.dry_run.unwrap_or(false) {
        return ToolResult {
            content: format_edit_result("would edit", &path, &edit_summary),
            is_error: false,
        };
    }

    match write_text_file(path_ref, &edit_summary.content, true).await {
        Ok(()) => ToolResult {
            content: format_edit_result("edited", &path, &edit_summary),
            is_error: false,
        },
        Err(error) => ToolResult {
            content: format!("failed to write {path}: {error}"),
            is_error: true,
        },
    }
}

fn validate_nonempty_path(path: &str) -> Result<String, ToolResult> {
    let trimmed = path.trim();
    if trimmed.is_empty() {
        Err(ToolResult {
            content: "missing required string argument: path".to_string(),
            is_error: true,
        })
    } else {
        Ok(trimmed.to_string())
    }
}

async fn ensure_parent_directories(path: &Path, create_parents: bool) -> io::Result<()> {
    if !create_parents {
        return Ok(());
    }
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        tokio::fs::create_dir_all(parent).await?;
    }
    Ok(())
}

async fn write_text_file(path: &Path, content: &str, overwrite: bool) -> io::Result<()> {
    if !overwrite {
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        let mut file = options.open(path).await?;
        file.write_all(content.as_bytes()).await?;
        file.flush().await?;
        return Ok(());
    }

    atomic_write_text_file(path, content).await
}

async fn atomic_write_text_file(path: &Path, content: &str) -> io::Result<()> {
    let temp_path = temporary_sibling_path(path);
    let write_result = async {
        let mut temp_file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp_path)
            .await?;
        temp_file.write_all(content.as_bytes()).await?;
        temp_file.flush().await?;
        drop(temp_file);
        tokio::fs::rename(&temp_path, path).await
    }
    .await;

    match write_result {
        Ok(()) => Ok(()),
        Err(error) => {
            let _ = tokio::fs::remove_file(&temp_path).await;
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

fn format_write_error(path: &str, error: io::Error, overwrite: bool) -> String {
    if !overwrite && error.kind() == io::ErrorKind::AlreadyExists {
        format!("refusing to overwrite existing file: {path}")
    } else {
        format!("failed to write {path}: {error}")
    }
}

struct AppliedEditSummary {
    content: String,
    replacement_count: usize,
    char_delta: isize,
}

fn apply_text_edits(
    original_content: &str,
    edits: &[TextEditArgs],
) -> Result<AppliedEditSummary, String> {
    let mut content = original_content.to_string();
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
        replacement_count,
        char_delta,
    })
}

fn format_edit_result(action: &str, path: &str, summary: &AppliedEditSummary) -> String {
    format!(
        "{action} file: {path} ({} replacement{}, {:+} chars)",
        summary.replacement_count,
        if summary.replacement_count == 1 {
            ""
        } else {
            "s"
        },
        summary.char_delta,
    )
}
