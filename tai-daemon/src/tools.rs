use crate::git_tools;
use crate::openai::{ChatToolCall, ChatToolDefinition};
use sha2::{Digest, Sha256};

#[path = "tools_catalog.rs"]
mod catalog;
#[path = "tools_fs.rs"]
mod fs_tools;
#[path = "tools_http.rs"]
mod http_tools;
#[path = "tools_image.rs"]
mod image_tools;

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

pub(crate) use catalog::available_tools;
pub(crate) use image_tools::emit_prepared_image;

#[cfg(test)]
pub(crate) async fn execute_read_file_range_tool(arguments_json: &str) -> ToolResult {
    fs_tools::execute_read_file_range_tool(arguments_json).await
}

#[cfg(test)]
pub(crate) async fn execute_write_file_tool(arguments_json: &str) -> ToolResult {
    fs_tools::execute_write_file_tool(arguments_json).await
}

#[cfg(test)]
pub(crate) async fn execute_edit_file_tool(arguments_json: &str) -> ToolResult {
    fs_tools::execute_edit_file_tool(arguments_json).await
}

#[cfg(test)]
pub(crate) async fn execute_http_request_tool(arguments_json: &str) -> ToolResult {
    http_tools::execute_http_request_tool(arguments_json).await
}

pub(crate) async fn execute_tool_call(tool_call: &ChatToolCall) -> ToolExecutionOutput {
    match tool_call.name.as_str() {
        "read_file" => ToolExecutionOutput {
            result: fs_tools::execute_read_file_tool(&tool_call.arguments_json).await,
            image: None,
        },
        "read_file_range" => ToolExecutionOutput {
            result: fs_tools::execute_read_file_range_tool(&tool_call.arguments_json).await,
            image: None,
        },
        "list_files" => ToolExecutionOutput {
            result: fs_tools::execute_list_files_tool(&tool_call.arguments_json).await,
            image: None,
        },
        "http_request" => ToolExecutionOutput {
            result: http_tools::execute_http_request_tool(&tool_call.arguments_json).await,
            image: None,
        },
        "write_file" => ToolExecutionOutput {
            result: fs_tools::execute_write_file_tool(&tool_call.arguments_json).await,
            image: None,
        },
        "edit_file" => ToolExecutionOutput {
            result: fs_tools::execute_edit_file_tool(&tool_call.arguments_json).await,
            image: None,
        },
        "display_image" => image_tools::execute_display_image_tool(&tool_call.arguments_json).await,
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

pub(crate) fn sha256_hex(content: &str) -> String {
    let digest = Sha256::digest(content.as_bytes());
    format!("{digest:x}")
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
