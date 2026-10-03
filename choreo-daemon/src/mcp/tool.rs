use crate::tools::context::ToolContext;
use crate::tools::image::prepare_image_from_bytes;
use crate::tools::{PreparedImage, ToolDyn, ToolError, ToolOutput, ToolOutputFormat, encode_outer};
use anyhow::{Context, Result};
use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use choreo_ai_protocols::openai::AllowedCaller;
use choreo_keystore::ServiceCredential;
use choreo_mcp::{CallToolResult, McpClient, McpContent};
use crossbeam_channel;
use serde_json::Value;
use std::sync::mpsc;
use std::sync::{Arc, Mutex};

/// Upper bound on the joined text an MCP result contributes to the model.
///
/// A server can return an arbitrarily large text block; capping it keeps one
/// call from flooding a request's context. Truncation is explicit (a trailing
/// marker names the cap) so the model can tell output was cut rather than
/// silently ending mid-thought.
const MAX_OUTPUT_TEXT_BYTES: usize = 256 * 1024;

/// The marker appended when a text block is truncated at
/// [`MAX_OUTPUT_TEXT_BYTES`].
const TRUNCATION_MARKER: &str = "\n… [output truncated]";

/// Wraps an MCP server tool as a `ToolDyn` for Choreographr's tool registry.
pub struct McpToolWrapper {
    /// Full prefixed name: "mcp/<`server_slug`>/<`tool_name`>"
    name: String,
    /// Tool group: "mcp/<`server_slug`>"
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
        let mut client = self
            .client
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
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
        encode_outer::<String, String>(&Err(ToolError::Postcard(format!(
            "invalid binary arguments: {e}"
        ))))
    })
}

/// Convert an anyhow error to a `ToolError` for the `ToolDyn` boundary.
/// Uses `{:#}` formatting to include the full error chain (context added
/// by `.context()` / `.with_context()` upstream).
fn to_tool_error(e: &anyhow::Error) -> ToolError {
    ToolError::Other(format!("{e:#}"))
}

/// The mapped form of an MCP call result.
struct MappedResult {
    /// Text blocks (and placeholders for content that has no textual form), in
    /// server order.
    text_parts: Vec<String>,
    /// Images decoded from `image` content blocks, ready for the daemon image
    /// pipeline. Empty when the caller cannot accept images.
    images: Vec<PreparedImage>,
    /// Whether the server flagged the call as an error (`isError`).
    is_error: bool,
}

/// Map an MCP [`CallToolResult`] onto the daemon's tool-output shape.
///
/// When `attach_images` is true, `image` blocks are base64-decoded and routed
/// to the daemon image pipeline; a decode or validation failure (or a caller
/// with no image sink) degrades to a text placeholder instead of dropping the
/// content silently.
fn map_mcp_result(result: &CallToolResult, attach_images: bool) -> MappedResult {
    let mut text_parts = Vec::new();
    let mut images = Vec::new();
    for content in &result.content {
        match content {
            McpContent::Text { text } => text_parts.push(text.clone()),
            McpContent::Image { data, mime_type } => {
                let mime = mime_type.clone().unwrap_or_else(|| "image/png".to_string());
                let placeholder =
                    format!("[Image: {} ({})]", mime, humfmt::bytes(data.len() as u64));
                if attach_images {
                    let prepared =
                        BASE64
                            .decode(data)
                            .map_err(|e| e.to_string())
                            .and_then(|bytes| {
                                prepare_image_from_bytes(&mime, &bytes)
                                    .map(|(m, w, h)| (m, bytes, w, h))
                                    .map_err(|e| e.to_string())
                            });
                    match prepared {
                        Ok((mime_type, data, width, height)) => images.push(PreparedImage {
                            mime_type,
                            data,
                            width,
                            height,
                            alt: None,
                        }),
                        Err(e) => {
                            tracing::warn!(error = %e, "MCP image content could not be attached");
                            text_parts.push(placeholder);
                        }
                    }
                } else {
                    text_parts.push(placeholder);
                }
            }
            McpContent::Resource { resource } => {
                text_parts.push(format!("[Resource: {resource}]"));
            }
        }
    }
    MappedResult {
        text_parts,
        images,
        is_error: result.is_error,
    }
}

/// Join text parts and truncate the result at [`MAX_OUTPUT_TEXT_BYTES`].
fn join_text_parts(text_parts: &[String]) -> String {
    let joined = text_parts.join("\n");
    if joined.len() <= MAX_OUTPUT_TEXT_BYTES {
        return joined;
    }
    let cut = MAX_OUTPUT_TEXT_BYTES;
    // `cut` may land mid-codepoint; back off to the nearest char boundary so
    // the truncated string stays valid UTF-8.
    let mut boundary = cut.min(joined.len());
    while boundary > 0 && !joined.is_char_boundary(boundary) {
        boundary -= 1;
    }
    let mut truncated = joined.get(..boundary).unwrap_or(&joined).to_string();
    truncated.push_str(TRUNCATION_MARKER);
    truncated
}

impl ToolDyn for McpToolWrapper {
    fn describe_invocation_json(&self, _args_json: &str) -> String {
        self.description().to_string()
    }
    fn supports_streaming_output(&self) -> bool {
        true
    }

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
        // We do not capture the server's `outputSchema`, so there is no real
        // return schema to advertise; `None` is honest (a hard-coded
        // `{"type":"string"}` would be wrong for any structured tool).
        None
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
        image_tx: Option<mpsc::Sender<PreparedImage>>,
    ) -> Result<ToolOutput, ToolError> {
        let args = parse_json_args(args_json)?;
        let result = self.call_with_args(args).map_err(|e| to_tool_error(&e))?;
        let mapped = map_mcp_result(&result, image_tx.is_some());
        // Images ride the dedicated sink so a vision-capable model receives
        // them as images, not as text; a caller without a sink got placeholders
        // from the mapping above instead.
        if let Some(tx) = &image_tx {
            for image in mapped.images {
                let _ = tx.send(image);
            }
        }
        let content = join_text_parts(&mapped.text_parts);
        Ok(ToolOutput {
            content: match format {
                ToolOutputFormat::Text => content,
                ToolOutputFormat::Json => serde_json::to_string(&content).unwrap_or(content),
            },
            is_error: mapped.is_error,
            invocation_description: String::new(),
            ..Default::default()
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
        // The postcard wire shape has no `is_error` bit, so a server-flagged
        // error is surfaced as the domain `Err` (the byte-level analogue of
        // `ToolOutput.is_error`) rather than being dropped.
        let result: Result<String, String> = match self.call_with_args(args) {
            Ok(call_result) => {
                let mapped = map_mcp_result(&call_result, false);
                let text = join_text_parts(&mapped.text_parts);
                if mapped.is_error { Err(text) } else { Ok(text) }
            }
            Err(e) => Err(format!("{e:#}")),
        };
        encode_outer::<String, String>(&Ok(result))
    }

    fn execute_streaming_json(
        &self,
        args_json: &str,
        format: ToolOutputFormat,
        _x_credentials: Option<&ServiceCredential>,
        _working_dir: Option<&std::path::Path>,
        output_tx: crossbeam_channel::Sender<Vec<u8>>,
        _ctx: Option<&ToolContext>,
        image_tx: Option<mpsc::Sender<PreparedImage>>,
    ) -> Result<ToolOutput, ToolError> {
        let args = parse_json_args(args_json)?;
        let result = self.call_with_args(args).map_err(|e| to_tool_error(&e))?;
        let mapped = map_mcp_result(&result, image_tx.is_some());
        if let Some(tx) = &image_tx {
            for image in mapped.images {
                let _ = tx.send(image);
            }
        }
        let text_content = join_text_parts(&mapped.text_parts);
        // Always stream text content for incremental display.
        let _ = output_tx.send(text_content.as_bytes().to_vec());
        Ok(ToolOutput {
            content: match format {
                ToolOutputFormat::Text => text_content,
                ToolOutputFormat::Json => {
                    serde_json::to_string(&text_content).unwrap_or(text_content)
                }
            },
            is_error: mapped.is_error,
            invocation_description: String::new(),
            ..Default::default()
        })
    }
}

#[cfg(test)]
fn mcp_result_to_string(result: &CallToolResult) -> String {
    let mapped = map_mcp_result(result, false);
    join_text_parts(&mapped.text_parts)
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── mcp_result_to_tool_output tests ──────────────────────────────

    fn mcp_result_to_tool_output(result: &CallToolResult) -> ToolOutput {
        let mapped = map_mcp_result(result, false);
        ToolOutput {
            content: join_text_parts(&mapped.text_parts),
            is_error: mapped.is_error,
            invocation_description: String::new(),
            ..Default::default()
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
        let output = mcp_result_to_tool_output(&result);
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
        let output = mcp_result_to_tool_output(&result);
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
        let output = mcp_result_to_tool_output(&result);
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
        let output = mcp_result_to_tool_output(&result);
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
        let output = mcp_result_to_tool_output(&result);
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
        let output = mcp_result_to_tool_output(&result);
        assert!(output.is_error);
        assert_eq!(output.content, "error msg");
    }

    #[test]
    fn mcp_result_to_tool_output_empty_content() {
        let result = CallToolResult {
            content: vec![],
            is_error: false,
        };
        let output = mcp_result_to_tool_output(&result);
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
        assert_eq!(mcp_result_to_string(&result), "hello");
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
        assert_eq!(mcp_result_to_string(&result), "a\nb");
    }

    // ── content mapping: image attach / truncation ───────────────────

    /// A 1x1 opaque PNG (correct CRCs), valid for the daemon image pipeline.
    const PNG_1X1: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR4nGP4z8DwHwAFAAH/iZk9HQAAAABJRU5ErkJggg==";

    #[test]
    fn map_mcp_result_attaches_valid_image() {
        let result = CallToolResult {
            content: vec![McpContent::Image {
                data: PNG_1X1.into(),
                mime_type: Some("image/png".into()),
            }],
            is_error: false,
        };
        let mapped = map_mcp_result(&result, true);
        assert_eq!(mapped.images.len(), 1, "valid image should be attached");
        assert_eq!(mapped.text_parts.len(), 0, "image-only result has no text");
        assert_eq!(mapped.images[0].mime_type(), "image/png");
    }

    #[test]
    fn map_mcp_result_invalid_image_falls_back_to_placeholder() {
        let result = CallToolResult {
            content: vec![McpContent::Image {
                data: "not-base64!!".into(),
                mime_type: Some("image/png".into()),
            }],
            is_error: false,
        };
        let mapped = map_mcp_result(&result, true);
        assert_eq!(mapped.images.len(), 0, "invalid image must not attach");
        assert_eq!(mapped.text_parts.len(), 1);
        assert!(mapped.text_parts[0].contains("[Image:"));
    }

    #[test]
    fn map_mcp_result_without_sink_uses_placeholder() {
        let result = CallToolResult {
            content: vec![McpContent::Image {
                data: PNG_1X1.into(),
                mime_type: Some("image/png".into()),
            }],
            is_error: false,
        };
        let mapped = map_mcp_result(&result, false);
        assert_eq!(mapped.images.len(), 0, "no sink means no image attaches");
        assert_eq!(mapped.text_parts.len(), 1);
        assert!(mapped.text_parts[0].contains("[Image:"));
    }

    #[test]
    fn join_text_parts_truncates_with_marker() {
        let big = "a".repeat(MAX_OUTPUT_TEXT_BYTES + 100);
        let joined = join_text_parts(&[big]);
        assert!(joined.len() <= MAX_OUTPUT_TEXT_BYTES + TRUNCATION_MARKER.len());
        assert!(joined.ends_with(TRUNCATION_MARKER));
    }

    #[test]
    fn join_text_parts_leaves_small_input_untouched() {
        assert_eq!(join_text_parts(&["a".into(), "b".into()]), "a\nb");
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
        assert_eq!(
            args.get("nested")
                .and_then(|v| v.get("a"))
                .and_then(serde_json::Value::as_i64),
            Some(1)
        );
    }

    // ── parse_binary_args tests ──────────────────────────────────────

    #[test]
    fn parse_binary_args_invalid_returns_error_bytes() {
        let bytes = parse_binary_args(b"not postcard").unwrap_err();
        assert_ne!(bytes, [] as [u8; 0]);
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
