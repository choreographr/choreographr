use crate::openai::AllowedCaller;
use crate::tools::context::ToolContext;
use crate::tools::{PreparedImage, ToolDyn, ToolError, ToolOutput, ToolOutputFormat, encode_outer};
use anyhow::{Context, Result};
use serde_json::Value;
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use tai_keystore::ServiceCredential;
use tai_mcp::{CallToolResult, McpClient, McpContent};

/// Wraps an MCP server tool as a `ToolDyn` for tai's tool registry.
pub struct McpToolWrapper {
    /// Full prefixed name: "mcp/<server_slug>/<tool_name>"
    name: String,
    /// Tool group: "mcp/<server_slug>"
    group: String,
    /// Description with server prefix
    description: String,
    /// Original input schema from the MCP server
    input_schema: Value,
    /// The original tool name as the MCP server knows it
    original_name: String,
    /// Shared MCP client (one per server, shared across all tools from that server)
    client: Arc<Mutex<McpClient>>,
}

impl McpToolWrapper {
    pub fn new(
        server_slug: &str,
        tool_name: &str,
        description: &str,
        input_schema: Value,
        client: Arc<Mutex<McpClient>>,
    ) -> Self {
        Self {
            name: format!("mcp/{server_slug}/{tool_name}"),
            group: format!("mcp/{server_slug}"),
            description: format!("[MCP {server_slug}] {description}"),
            input_schema,
            original_name: tool_name.to_string(),
            client,
        }
    }

    fn call_with_args(&self, args: Value) -> Result<CallToolResult> {
        let mut client = self.client.lock().unwrap_or_else(|e| e.into_inner());
        client
            .call_tool(&self.original_name, Some(args), None)
            .with_context(|| format!("MCP tool call '{}' failed", self.original_name))
    }
}

fn parse_json_args(args_json: &str) -> Result<Value, ToolError> {
    serde_json::from_str(args_json).map_err(|e| ToolError::InvalidArguments(e.to_string()))
}

fn parse_binary_args(args_bytes: &[u8]) -> Result<Value, Vec<u8>> {
    postcard::from_bytes(args_bytes).map_err(|e| {
        encode_outer::<String, String>(Err(ToolError::Postcard(format!(
            "invalid binary arguments: {e}"
        ))))
    })
}

/// Convert an anyhow error to a `ToolError` for the ToolDyn boundary.
/// Uses `{:#}` formatting to include the full error chain (context added
/// by `.context()` / `.with_context()` upstream).
fn to_tool_error(e: anyhow::Error) -> ToolError {
    ToolError::Other(format!("{e:#}"))
}

fn mcp_result_to_text_parts(result: &CallToolResult) -> (Vec<String>, bool) {
    let mut text_parts = Vec::new();
    for content in &result.content {
        match content {
            McpContent::Text { text } => text_parts.push(text.clone()),
            McpContent::Image { data, mime_type } => {
                let mime = mime_type.clone().unwrap_or_else(|| "image/png".to_string());
                text_parts.push(format!(
                    "[Image: {} ({})]",
                    mime,
                    humfmt::bytes(data.len() as u64),
                ));
            }
            McpContent::Resource { resource } => {
                text_parts.push(format!("[Resource: {}]", resource));
            }
        }
    }
    (text_parts, result.is_error)
}

impl ToolDyn for McpToolWrapper {
    fn name(&self) -> &str {
        &self.name
    }

    fn group(&self) -> &str {
        &self.group
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn schema(&self) -> Value {
        self.input_schema.clone()
    }

    fn output_schema(&self) -> Option<Value> {
        Some(serde_json::json!({"type": "string"}))
    }

    fn allowed_callers(&self) -> Vec<AllowedCaller> {
        vec![AllowedCaller::Direct, AllowedCaller::Programmatic]
    }

    fn execute_json(
        &self,
        args_json: &str,
        format: ToolOutputFormat,
        _x_credentials: Option<&ServiceCredential>,
        _working_dir: Option<&std::path::Path>,
        _ctx: Option<&ToolContext>,
        _image_tx: Option<mpsc::Sender<PreparedImage>>,
    ) -> Result<ToolOutput, ToolError> {
        let args = parse_json_args(args_json)?;
        let result = self.call_with_args(args).map_err(to_tool_error)?;
        let (text_parts, is_error) = mcp_result_to_text_parts(&result);
        let content = text_parts.join("\n");
        Ok(ToolOutput {
            content: match format {
                ToolOutputFormat::Text => content,
                ToolOutputFormat::Json => serde_json::to_string(&content).unwrap_or(content),
            },
            is_error,
        })
    }

    fn execute_postcard(
        &self,
        args_bytes: &[u8],
        _x_credentials: Option<&ServiceCredential>,
        _working_dir: Option<&std::path::Path>,
        _ctx: Option<&ToolContext>,
    ) -> Vec<u8> {
        let args = match parse_binary_args(args_bytes) {
            Ok(v) => v,
            Err(e) => return e,
        };
        let result: Result<String, String> = match self.call_with_args(args) {
            Ok(call_result) => Ok(mcp_result_to_string(call_result)),
            Err(e) => Err(format!("{e:#}")),
        };
        encode_outer::<String, String>(Ok(result))
    }

    fn execute_streaming_json(
        &self,
        args_json: &str,
        format: ToolOutputFormat,
        _x_credentials: Option<&ServiceCredential>,
        _working_dir: Option<&std::path::Path>,
        output_tx: mpsc::Sender<Vec<u8>>,
        _ctx: Option<&ToolContext>,
        _image_tx: Option<mpsc::Sender<PreparedImage>>,
    ) -> Result<ToolOutput, ToolError> {
        let args = parse_json_args(args_json)?;
        let result = self.call_with_args(args).map_err(to_tool_error)?;
        let (text_parts, is_error) = mcp_result_to_text_parts(&result);
        let text_content = text_parts.join("\n");
        // Always stream text content for incremental display.
        let _ = output_tx.send(text_content.as_bytes().to_vec());
        Ok(ToolOutput {
            content: match format {
                ToolOutputFormat::Text => text_content,
                ToolOutputFormat::Json => {
                    serde_json::to_string(&text_content).unwrap_or(text_content)
                }
            },
            is_error,
        })
    }
}

fn mcp_result_to_string(result: CallToolResult) -> String {
    let (text_parts, _) = mcp_result_to_text_parts(&result);
    text_parts.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── mcp_result_to_tool_output tests ──────────────────────────────

    fn mcp_result_to_tool_output(result: CallToolResult) -> ToolOutput {
        let (text_parts, is_error) = mcp_result_to_text_parts(&result);
        ToolOutput {
            content: text_parts.join("\n"),
            is_error,
        }
    }

    #[test]
    fn mcp_result_to_tool_output_text_only() {
        let result = CallToolResult {
            content: vec![McpContent::Text {
                text: "hello world".into(),
            }],
            is_error: false,
        };
        let output = mcp_result_to_tool_output(result);
        assert!(!output.is_error);
        assert_eq!(output.content, "hello world");
    }

    #[test]
    fn mcp_result_to_tool_output_multiple_texts() {
        let result = CallToolResult {
            content: vec![
                McpContent::Text {
                    text: "line 1".into(),
                },
                McpContent::Text {
                    text: "line 2".into(),
                },
            ],
            is_error: false,
        };
        let output = mcp_result_to_tool_output(result);
        assert_eq!(output.content, "line 1\nline 2");
    }

    #[test]
    fn mcp_result_to_tool_output_with_image() {
        let result = CallToolResult {
            content: vec![McpContent::Image {
                data: "abc123".into(),
                mime_type: Some("image/png".into()),
            }],
            is_error: false,
        };
        let output = mcp_result_to_tool_output(result);
        assert!(output.content.contains("[Image:"));
        assert!(output.content.contains("image/png"));
    }

    #[test]
    fn mcp_result_to_tool_output_image_no_mime_defaults_to_png() {
        let result = CallToolResult {
            content: vec![McpContent::Image {
                data: "data".into(),
                mime_type: None,
            }],
            is_error: false,
        };
        let output = mcp_result_to_tool_output(result);
        assert!(output.content.contains("image/png"));
    }

    #[test]
    fn mcp_result_to_tool_output_with_resource() {
        let result = CallToolResult {
            content: vec![McpContent::Resource {
                resource: serde_json::json!({"uri": "file:///tmp/test"}),
            }],
            is_error: false,
        };
        let output = mcp_result_to_tool_output(result);
        assert!(output.content.contains("[Resource:"));
    }

    #[test]
    fn mcp_result_to_tool_output_is_error() {
        let result = CallToolResult {
            content: vec![McpContent::Text {
                text: "error msg".into(),
            }],
            is_error: true,
        };
        let output = mcp_result_to_tool_output(result);
        assert!(output.is_error);
        assert_eq!(output.content, "error msg");
    }

    #[test]
    fn mcp_result_to_tool_output_empty_content() {
        let result = CallToolResult {
            content: vec![],
            is_error: false,
        };
        let output = mcp_result_to_tool_output(result);
        assert!(!output.is_error);
        assert_eq!(output.content, "");
    }

    // ── mcp_result_to_string tests ───────────────────────────────────

    #[test]
    fn mcp_result_to_string_text_only() {
        let result = CallToolResult {
            content: vec![McpContent::Text {
                text: "hello".into(),
            }],
            is_error: false,
        };
        assert_eq!(mcp_result_to_string(result), "hello");
    }

    #[test]
    fn mcp_result_to_string_joins_multiple() {
        let result = CallToolResult {
            content: vec![
                McpContent::Text { text: "a".into() },
                McpContent::Text { text: "b".into() },
            ],
            is_error: false,
        };
        assert_eq!(mcp_result_to_string(result), "a\nb");
    }

    // ── parse_json_args tests ────────────────────────────────────────

    #[test]
    fn parse_json_args_invalid_returns_error() {
        let err = parse_json_args("not json").unwrap_err();
        assert!(matches!(err, ToolError::InvalidArguments(_)));
        assert!(err.to_string().contains("invalid arguments"));
    }

    #[test]
    fn parse_json_args_valid_returns_value() {
        let args = parse_json_args(r#"{"key": "value"}"#).unwrap();
        assert_eq!(args.get("key").and_then(|v| v.as_str()), Some("value"));
    }

    #[test]
    fn parse_json_args_empty_object() {
        let args = parse_json_args("{}").unwrap();
        assert!(args.as_object().unwrap().is_empty());
    }

    #[test]
    fn parse_json_args_nested_value() {
        let args = parse_json_args(r#"{"nested": {"a": 1}}"#).unwrap();
        assert!(
            args.get("nested")
                .and_then(|v| v.get("a"))
                .and_then(|v| v.as_i64())
                == Some(1)
        );
    }

    // ── parse_binary_args tests ──────────────────────────────────────

    #[test]
    fn parse_binary_args_invalid_returns_error_bytes() {
        let bytes = parse_binary_args(b"not postcard").unwrap_err();
        assert!(!bytes.is_empty());
        let decoded: Result<Result<String, String>, ToolError> =
            postcard::from_bytes(&bytes).unwrap();
        assert!(decoded.is_err());
        assert!(
            decoded
                .unwrap_err()
                .to_string()
                .contains("invalid binary arguments")
        );
    }
}
