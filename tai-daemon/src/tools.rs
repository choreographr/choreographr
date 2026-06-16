use crate::{SessionState, broadcast_to_session, git_tools};
use crate::openai::{ChatToolCall, ChatToolDefinition};
use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use image::GenericImageView;
use reqwest::{
    Method, StatusCode, Url,
    header::{CONTENT_TYPE, HeaderMap, HeaderName, HeaderValue},
};
use resvg::usvg;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    io,
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tai_proto::{DaemonMessage, ImageMetadata, MAX_IMAGE_CHUNK_SIZE};
use tokio::{fs::OpenOptions, io::AsyncWriteExt, sync::Mutex};

#[derive(Debug, Clone)]
pub(crate) struct ToolResult {
    pub(crate) content: String,
    pub(crate) is_error: bool,
}

#[derive(Debug)]
pub(crate) struct ToolExecutionOutput {
    pub(crate) result: ToolResult,
    pub(crate) image: Option<PreparedImage>,
}

#[derive(Debug)]
pub(crate) struct PreparedImage {
    pub(crate) mime_type: String,
    pub(crate) data: Vec<u8>,
    pub(crate) width: u32,
    pub(crate) height: u32,
    pub(crate) alt: Option<String>,
}

#[derive(Debug, Deserialize)]
struct HttpRequestArgs {
    method: String,
    url: String,
    #[serde(default)]
    headers: HashMap<String, String>,
    body: Option<String>,
    timeout_secs: Option<u64>,
}

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
struct DisplayImageArgs {
    mime_type: String,
    path: Option<String>,
    url: Option<String>,
    base64_data: Option<String>,
    svg_text: Option<String>,
    alt: Option<String>,
}

pub(crate) fn available_tools() -> Vec<ChatToolDefinition> {
    vec![
        ChatToolDefinition::function(
            "read_file",
            "Read a UTF-8 text file from the local workspace.",
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
        ),
        ChatToolDefinition::function(
            "read_file_range",
            "Read a line range from a UTF-8 text file in the local workspace.",
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
        ),
        ChatToolDefinition::function(
            "list_files",
            "List files in a local directory.",
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
        ),
        ChatToolDefinition::function(
            "http_request",
            "Make an HTTP request to an absolute URL and return status, response headers, and response body text. Supports custom headers such as Range for partial content requests.",
            serde_json::json!({
                "type": "object",
                "properties": {
                    "method": {
                        "type": "string",
                        "enum": ["GET", "POST", "HEAD"]
                    },
                    "url": {
                        "type": "string",
                        "description": "Absolute http or https URL"
                    },
                    "headers": {
                        "type": "object",
                        "description": "Optional request headers, including Range",
                        "additionalProperties": {
                            "type": "string"
                        }
                    },
                    "body": {
                        "type": "string",
                        "description": "Optional UTF-8 request body"
                    },
                    "timeout_secs": {
                        "type": "integer",
                        "minimum": 1,
                        "maximum": 30,
                        "default": 10
                    }
                },
                "required": ["method", "url"],
                "additionalProperties": false
            }),
        ),
        ChatToolDefinition::function(
            "write_file",
            "Write a UTF-8 text file to the local workspace.",
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
        ),
        ChatToolDefinition::function(
            "edit_file",
            "Edit a UTF-8 text file by applying one or more exact text replacements. Each edit must match at least once; non-replace_all edits must match exactly once.",
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
        ),
        ChatToolDefinition::function(
            "display_image",
            "Display a PNG, JPEG, or SVG image on the client. Provide exactly one source: path, url, base64_data, or svg_text.",
            serde_json::json!({
                "type": "object",
                "properties": {
                    "mime_type": {
                        "type": "string",
                        "enum": ["image/png", "image/jpeg", "image/svg+xml"]
                    },
                    "path": {
                        "type": "string",
                        "description": "Relative or absolute path to an image file"
                    },
                    "url": {
                        "type": "string",
                        "description": "Absolute http or https URL for an image"
                    },
                    "base64_data": {
                        "type": "string",
                        "description": "Base64-encoded PNG or JPEG bytes"
                    },
                    "svg_text": {
                        "type": "string",
                        "description": "Raw SVG XML source when mime_type is image/svg+xml"
                    },
                    "alt": {
                        "type": "string",
                        "description": "Short alt text for the image"
                    }
                },
                "required": ["mime_type"],
                "additionalProperties": false
            }),
        ),
        ChatToolDefinition::function(
            "git_status",
            "Summarize the current Git repository status, including branch, staged, unstaged, and untracked changes.",
            serde_json::json!({
                "type": "object",
                "properties": {
                    "repo_path": {
                        "type": "string",
                        "description": "Relative or absolute path inside a Git repository",
                        "default": "."
                    }
                },
                "additionalProperties": false
            }),
        ),
        ChatToolDefinition::function(
            "git_diff",
            "Summarize Git changes in the working tree or index. Set cached=true to show staged changes.",
            serde_json::json!({
                "type": "object",
                "properties": {
                    "repo_path": {
                        "type": "string",
                        "description": "Relative or absolute path inside a Git repository",
                        "default": "."
                    },
                    "cached": {
                        "type": "boolean",
                        "description": "When true, summarize staged changes instead of working tree changes",
                        "default": false
                    },
                    "pathspec": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "Optional Git-style pathspec filters"
                    }
                },
                "additionalProperties": false
            }),
        ),
        ChatToolDefinition::function(
            "git_log",
            "Show recent Git commits for the current repository.",
            serde_json::json!({
                "type": "object",
                "properties": {
                    "repo_path": {
                        "type": "string",
                        "description": "Relative or absolute path inside a Git repository",
                        "default": "."
                    },
                    "limit": {
                        "type": "integer",
                        "minimum": 1,
                        "maximum": 100,
                        "default": 10
                    }
                },
                "additionalProperties": false
            }),
        ),
        ChatToolDefinition::function(
            "git_add",
            "Stage one or more Git pathspecs in the current repository index.",
            serde_json::json!({
                "type": "object",
                "properties": {
                    "repo_path": {
                        "type": "string",
                        "description": "Relative or absolute path inside a Git repository",
                        "default": "."
                    },
                    "pathspec": {
                        "type": "array",
                        "items": { "type": "string" },
                        "minItems": 1,
                        "description": "One or more Git-style pathspecs to stage"
                    }
                },
                "required": ["pathspec"],
                "additionalProperties": false
            }),
        ),
        ChatToolDefinition::function(
            "git_commit",
            "Create a Git commit from the currently staged index.",
            serde_json::json!({
                "type": "object",
                "properties": {
                    "repo_path": {
                        "type": "string",
                        "description": "Relative or absolute path inside a Git repository",
                        "default": "."
                    },
                    "message": {
                        "type": "string",
                        "description": "Commit message"
                    },
                    "allow_empty": {
                        "type": "boolean",
                        "description": "Whether to allow a commit when no staged changes are present",
                        "default": false
                    }
                },
                "required": ["message"],
                "additionalProperties": false
            }),
        ),
        ChatToolDefinition::function(
            "git_push",
            "Push the current or specified branch to a named Git remote using the external git command.",
            serde_json::json!({
                "type": "object",
                "properties": {
                    "repo_path": {
                        "type": "string",
                        "description": "Relative or absolute path inside a Git repository",
                        "default": "."
                    },
                    "remote": {
                        "type": "string",
                        "description": "Remote name to push to, such as origin"
                    },
                    "branch": {
                        "type": "string",
                        "description": "Branch name to push. Defaults to the current branch when HEAD is attached."
                    },
                    "set_upstream": {
                        "type": "boolean",
                        "description": "Whether to pass --set-upstream",
                        "default": false
                    },
                    "force_with_lease": {
                        "type": "boolean",
                        "description": "Whether to pass --force-with-lease",
                        "default": false
                    },
                    "dry_run": {
                        "type": "boolean",
                        "description": "Whether to pass --dry-run",
                        "default": false
                    }
                },
                "required": ["remote"],
                "additionalProperties": false
            }),
        ),
    ]
}

pub(crate) async fn execute_tool_call(tool_call: &ChatToolCall) -> ToolExecutionOutput {
    match tool_call.name.as_str() {
        "read_file" => ToolExecutionOutput {
            result: execute_read_file_tool(&tool_call.arguments_json).await,
            image: None,
        },
        "read_file_range" => ToolExecutionOutput {
            result: execute_read_file_range_tool(&tool_call.arguments_json).await,
            image: None,
        },
        "list_files" => ToolExecutionOutput {
            result: execute_list_files_tool(&tool_call.arguments_json).await,
            image: None,
        },
        "http_request" => ToolExecutionOutput {
            result: execute_http_request_tool(&tool_call.arguments_json).await,
            image: None,
        },
        "write_file" => ToolExecutionOutput {
            result: execute_write_file_tool(&tool_call.arguments_json).await,
            image: None,
        },
        "edit_file" => ToolExecutionOutput {
            result: execute_edit_file_tool(&tool_call.arguments_json).await,
            image: None,
        },
        "display_image" => execute_display_image_tool(&tool_call.arguments_json).await,
        "git_status" => ToolExecutionOutput {
            result: git_tools::execute_git_status_tool(&tool_call.arguments_json).await,
            image: None,
        },
        "git_diff" => ToolExecutionOutput {
            result: git_tools::execute_git_diff_tool(&tool_call.arguments_json).await,
            image: None,
        },
        "git_log" => ToolExecutionOutput {
            result: git_tools::execute_git_log_tool(&tool_call.arguments_json).await,
            image: None,
        },
        "git_add" => ToolExecutionOutput {
            result: git_tools::execute_git_add_tool(&tool_call.arguments_json).await,
            image: None,
        },
        "git_commit" => ToolExecutionOutput {
            result: git_tools::execute_git_commit_tool(&tool_call.arguments_json).await,
            image: None,
        },
        "git_push" => ToolExecutionOutput {
            result: git_tools::execute_git_push_tool(&tool_call.arguments_json).await,
            image: None,
        },
        _ => ToolExecutionOutput {
            result: ToolResult {
                content: format!("unknown tool: {}", tool_call.name),
                is_error: true,
            },
            image: None,
        },
    }
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

const MAX_DISPLAY_IMAGE_BYTES: usize = 8 * 1024 * 1024;
const SUPPORTED_IMAGE_MIME_TYPES: [&str; 3] = ["image/png", "image/jpeg", "image/svg+xml"];

pub(crate) async fn execute_display_image_tool(arguments_json: &str) -> ToolExecutionOutput {
    let args = match serde_json::from_str::<DisplayImageArgs>(arguments_json) {
        Ok(args) => args,
        Err(error) => {
            return ToolExecutionOutput {
                result: ToolResult {
                    content: format!("invalid arguments: {error}"),
                    is_error: true,
                },
                image: None,
            };
        }
    };

    match prepare_image(args).await {
        Ok(image) => {
            let mime_type = image.mime_type.clone();
            let width = image.width;
            let height = image.height;
            let byte_len = image.data.len();
            ToolExecutionOutput {
                result: ToolResult {
                    content: format!(
                        "displayed image ({mime_type}, {width}x{height}, {byte_len} bytes)"
                    ),
                    is_error: false,
                },
                image: Some(image),
            }
        }
        Err(error) => ToolExecutionOutput {
            result: ToolResult {
                content: error.to_string(),
                is_error: true,
            },
            image: None,
        },
    }
}

async fn prepare_image(args: DisplayImageArgs) -> io::Result<PreparedImage> {
    let mime_type = normalize_image_mime_type(&args.mime_type)?;
    let selected_sources = [
        args.path.as_ref().map(|_| "path"),
        args.url.as_ref().map(|_| "url"),
        args.base64_data.as_ref().map(|_| "base64_data"),
        args.svg_text.as_ref().map(|_| "svg_text"),
    ]
    .into_iter()
    .flatten()
    .count();
    if selected_sources != 1 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "provide exactly one image source: path, url, base64_data, or svg_text",
        ));
    }

    let data = if let Some(path) = args.path {
        tokio::fs::read(path.trim()).await?
    } else if let Some(url) = args.url {
        fetch_image_bytes(url.trim(), mime_type).await?
    } else if let Some(base64_data) = args.base64_data {
        BASE64.decode(base64_data.trim()).map_err(|error| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("invalid base64_data: {error}"),
            )
        })?
    } else if let Some(svg_text) = args.svg_text {
        svg_text.into_bytes()
    } else {
        unreachable!("source count validated")
    };

    if data.len() > MAX_DISPLAY_IMAGE_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "image exceeds maximum allowed size of {} bytes",
                MAX_DISPLAY_IMAGE_BYTES
            ),
        ));
    }

    let (width, height) = inspect_image_dimensions(mime_type, &data)?;
    Ok(PreparedImage {
        mime_type: mime_type.to_string(),
        data,
        width,
        height,
        alt: args.alt.filter(|alt| !alt.trim().is_empty()),
    })
}

fn normalize_image_mime_type(mime_type: &str) -> io::Result<&str> {
    let normalized = mime_type.trim();
    if SUPPORTED_IMAGE_MIME_TYPES.contains(&normalized) {
        Ok(normalized)
    } else {
        Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("unsupported image mime type: {normalized}"),
        ))
    }
}

async fn fetch_image_bytes(url: &str, expected_mime_type: &str) -> io::Result<Vec<u8>> {
    let url =
        Url::parse(url).map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;
    match url.scheme() {
        "http" | "https" => {}
        _ => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "image url must use http or https",
            ));
        }
    }

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .map_err(io::Error::other)?;
    let response = client.get(url).send().await.map_err(io::Error::other)?;
    let status = response.status();
    if !status.is_success() {
        return Err(io::Error::other(format!(
            "image request failed with status {status}"
        )));
    }
    if let Some(content_type) = response.headers().get(CONTENT_TYPE)
        && let Ok(content_type) = content_type.to_str()
        && !content_type.starts_with(expected_mime_type)
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "image response content-type {content_type} does not match {expected_mime_type}"
            ),
        ));
    }
    let bytes = response.bytes().await.map_err(io::Error::other)?;
    Ok(bytes.to_vec())
}

fn inspect_image_dimensions(mime_type: &str, data: &[u8]) -> io::Result<(u32, u32)> {
    match mime_type {
        "image/png" | "image/jpeg" => {
            let image = image::load_from_memory(data).map_err(io::Error::other)?;
            Ok(image.dimensions())
        }
        "image/svg+xml" => {
            let options = usvg::Options::default();
            let tree = usvg::Tree::from_data(data, &options).map_err(io::Error::other)?;
            let size = tree.size().to_int_size();
            Ok((size.width(), size.height()))
        }
        _ => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("unsupported image mime type: {mime_type}"),
        )),
    }
}

pub(crate) async fn emit_prepared_image(
    session: &Arc<Mutex<SessionState>>,
    request_id: u32,
    image_id: u32,
    image: PreparedImage,
) {
    let metadata = ImageMetadata {
        image_id,
        mime_type: image.mime_type,
        width: image.width,
        height: image.height,
        byte_len: image.data.len() as u64,
        alt: image.alt,
    };
    broadcast_to_session(
        session,
        DaemonMessage::ImageStart {
            request_id,
            metadata,
        },
        None,
    )
    .await;
    for data in image.data.chunks(MAX_IMAGE_CHUNK_SIZE) {
        broadcast_to_session(
            session,
            DaemonMessage::ImageChunk {
                request_id,
                image_id,
                data: data.to_vec(),
            },
            None,
        )
        .await;
    }
    broadcast_to_session(
        session,
        DaemonMessage::ImageEnd {
            request_id,
            image_id,
        },
        None,
    )
    .await;
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

pub(crate) fn sha256_hex(content: &str) -> String {
    let digest = Sha256::digest(content.as_bytes());
    format!("{digest:x}")
}

pub(crate) async fn execute_http_request_tool(arguments_json: &str) -> ToolResult {
    let args = match serde_json::from_str::<HttpRequestArgs>(arguments_json) {
        Ok(args) => args,
        Err(error) => {
            return ToolResult {
                content: format!("invalid arguments: {error}"),
                is_error: true,
            };
        }
    };

    let method = match args.method.as_str() {
        "GET" => Method::GET,
        "POST" => Method::POST,
        "HEAD" => Method::HEAD,
        other => {
            return ToolResult {
                content: format!("unsupported method: {other}"),
                is_error: true,
            };
        }
    };

    let url = match Url::parse(&args.url) {
        Ok(url) => url,
        Err(error) => {
            return ToolResult {
                content: format!("invalid url: {error}"),
                is_error: true,
            };
        }
    };
    match url.scheme() {
        "http" | "https" => {}
        other => {
            return ToolResult {
                content: format!("unsupported URL scheme: {other}"),
                is_error: true,
            };
        }
    }

    let timeout_secs = args.timeout_secs.unwrap_or(10).clamp(1, 30);
    let client = match reqwest::Client::builder()
        .timeout(Duration::from_secs(timeout_secs))
        .build()
    {
        Ok(client) => client,
        Err(error) => {
            return ToolResult {
                content: format!("failed to build http client: {error}"),
                is_error: true,
            };
        }
    };

    let headers = match build_http_request_headers(args.headers) {
        Ok(headers) => headers,
        Err(error) => {
            return ToolResult {
                content: error,
                is_error: true,
            };
        }
    };

    let mut request = client.request(method.clone(), url).headers(headers);
    if method != Method::GET && method != Method::HEAD
        && let Some(body) = args.body
    {
        request = request.body(body);
    }

    let response = match request.send().await {
        Ok(response) => response,
        Err(error) => {
            return ToolResult {
                content: format!("http request failed: {error}"),
                is_error: true,
            };
        }
    };

    let status = response.status();
    let headers = response.headers().clone();
    let content_type = headers
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .to_string();
    let body = if method == Method::HEAD {
        String::new()
    } else if is_text_content_type(&content_type) {
        match response.text().await {
            Ok(text) => truncate_tool_output(&text),
            Err(error) => format!("body omitted: failed to decode response text: {error}"),
        }
    } else {
        "body omitted: non-text response".to_string()
    };

    ToolResult {
        content: format_http_response(status, &headers, &body),
        is_error: false,
    }
}

fn build_http_request_headers(headers: HashMap<String, String>) -> Result<HeaderMap, String> {
    let mut request_headers = HeaderMap::new();
    for (name, value) in headers {
        let header_name = HeaderName::try_from(name.as_str())
            .map_err(|error| format!("invalid header name: {name}: {error}"))?;
        let header_value = HeaderValue::from_str(&value)
            .map_err(|error| format!("invalid header value for {name}: {error}"))?;
        request_headers.insert(header_name, header_value);
    }
    Ok(request_headers)
}

fn is_text_content_type(content_type: &str) -> bool {
    let mime = content_type
        .split(';')
        .next()
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase();
    mime.starts_with("text/")
        || matches!(
            mime.as_str(),
            "application/json"
                | "application/xml"
                | "application/javascript"
                | "application/x-javascript"
                | "application/x-ndjson"
                | "application/graphql-response+json"
        )
        || mime.ends_with("+json")
        || mime.ends_with("+xml")
}

fn format_http_response(status: StatusCode, headers: &HeaderMap, body: &str) -> String {
    let mut output = format!("status: {}", status);

    let mut entries = headers
        .iter()
        .map(|(name, value)| {
            (
                name.as_str().to_ascii_lowercase(),
                value.to_str().unwrap_or("<non-utf8>").to_string(),
            )
        })
        .collect::<Vec<_>>();
    entries.sort_by(|a, b| a.0.cmp(&b.0));

    for (name, value) in entries {
        output.push('\n');
        output.push_str(&name);
        output.push_str(": ");
        output.push_str(&value);
    }

    output.push_str("\n\n");
    output.push_str(body);
    output
}

pub(crate) fn truncate_tool_output(content: &str) -> String {
    const MAX_TOOL_OUTPUT_CHARS: usize = 16 * 1024;
    if content.chars().count() <= MAX_TOOL_OUTPUT_CHARS {
        return content.to_string();
    }
    let truncated = content
        .chars()
        .take(MAX_TOOL_OUTPUT_CHARS)
        .collect::<String>();
    format!("{truncated}\n...[truncated]")
}
