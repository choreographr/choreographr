//! Mapping MCP call results onto the daemon's tool-output shape.
//!
//! [`map_mcp_result`] builds the [`MappedResult`] the wrappers render: text
//! blocks are kept, `image` blocks are base64-decoded and routed through the
//! daemon image pipeline when a caller can accept images (otherwise, and on a
//! decode failure, they degrade to a text placeholder), and content with no
//! textual form (audio, blob resources, resource links) is described rather
//! than dropped. [`join_text_parts`] joins the text and truncates the result.

use crate::tools::PreparedImage;
use crate::tools::image::prepare_image_from_bytes;
use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use choreo_mcp::{CallToolResult, McpContent};
use serde_json::Value;

use super::{MAX_OUTPUT_TEXT_BYTES, TRUNCATION_MARKER};

/// The mapped form of an MCP call result.
pub(super) struct MappedResult {
    /// Text blocks (and placeholders for content that has no textual form), in
    /// server order.
    pub(super) text_parts: Vec<String>,
    /// Images decoded from `image` content blocks, ready for the daemon image
    /// pipeline. Empty when the caller cannot accept images.
    pub(super) images: Vec<PreparedImage>,
    /// Whether the server flagged the call as an error (`isError`).
    pub(super) is_error: bool,
    /// The server's `structuredContent`, preserved for the programmatic path.
    pub(super) structured: Option<Value>,
}

/// Map an MCP [`CallToolResult`] onto the daemon's tool-output shape.
///
/// When `attach_images` is true, `image` blocks are base64-decoded and routed
/// to the daemon image pipeline; a decode or validation failure (or a caller
/// with no image sink) degrades to a text placeholder instead of dropping the
/// content silently. Content with no textual form (audio, blob resources,
/// resource links) is described rather than dropped.
pub(super) fn map_mcp_result(result: &CallToolResult, attach_images: bool) -> MappedResult {
    let mut text_parts = Vec::new();
    let mut images = Vec::new();
    for content in &result.content {
        match content {
            McpContent::Text { text } => text_parts.push(text.clone()),
            McpContent::Image { data, mime_type } => {
                let placeholder = format!(
                    "[Image: {} ({})]",
                    mime_type,
                    humfmt::bytes(data.len() as u64)
                );
                if attach_images {
                    let prepared =
                        BASE64
                            .decode(data)
                            .map_err(|e| e.to_string())
                            .and_then(|bytes| {
                                prepare_image_from_bytes(mime_type, &bytes)
                                    .map(|(m, w, h)| (m, bytes, w, h))
                                    .map_err(|e| e.to_string())
                            });
                    match prepared {
                        Ok((mime, data, width, height)) => images.push(PreparedImage {
                            mime_type: mime,
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
            McpContent::Audio { data, mime_type } => text_parts.push(format!(
                "[Audio: {}, {} — not attached]",
                mime_type,
                humfmt::bytes(data.len() as u64)
            )),
            McpContent::Resource {
                uri,
                mime_type,
                text,
            } => match text {
                Some(text) => text_parts.push(text.clone()),
                None => text_parts.push(format!(
                    "[Resource: {uri} ({})]",
                    mime_type.as_deref().unwrap_or("unknown")
                )),
            },
            McpContent::ResourceLink { uri, name, .. } => {
                let label = name.as_deref().unwrap_or(uri);
                text_parts.push(format!("[Resource link: {label} ({uri})]"));
            }
        }
    }
    MappedResult {
        text_parts,
        images,
        is_error: result.is_error,
        structured: result.structured_content.clone(),
    }
}

/// Join text parts and truncate the result at [`MAX_OUTPUT_TEXT_BYTES`].
pub(super) fn join_text_parts(text_parts: &[String]) -> String {
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

#[cfg(test)]
fn mcp_result_to_string(result: &CallToolResult) -> String {
    let mapped = map_mcp_result(result, false);
    join_text_parts(&mapped.text_parts)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::ToolOutput;

    fn empty_result() -> CallToolResult {
        CallToolResult {
            content: vec![],
            is_error: false,
            structured_content: None,
        }
    }

    fn text_result(text: &str) -> CallToolResult {
        CallToolResult {
            content: vec![McpContent::Text { text: text.into() }],
            is_error: false,
            structured_content: None,
        }
    }

    fn mcp_result_to_tool_output(result: &CallToolResult) -> ToolOutput {
        let mapped = map_mcp_result(result, false);
        ToolOutput {
            content: join_text_parts(&mapped.text_parts),
            is_error: mapped.is_error,
            result_json: mapped.structured,
            invocation_description: String::new(),
            ..Default::default()
        }
    }

    #[test]
    fn mcp_result_to_tool_output_text_only() {
        let output = mcp_result_to_tool_output(&text_result("hello world"));
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
            structured_content: None,
        };
        assert_eq!(mcp_result_to_tool_output(&result).content, "line 1\nline 2");
    }

    #[test]
    fn mcp_result_to_tool_output_with_image() {
        let result = CallToolResult {
            content: vec![McpContent::Image {
                data: "abc123".into(),
                mime_type: "image/png".into(),
            }],
            is_error: false,
            structured_content: None,
        };
        let output = mcp_result_to_tool_output(&result);
        assert!(output.content.contains("[Image:"));
        assert!(output.content.contains("image/png"));
    }

    #[test]
    fn mcp_result_to_tool_output_with_audio() {
        let result = CallToolResult {
            content: vec![McpContent::Audio {
                data: "abcd".into(),
                mime_type: "audio/wav".into(),
            }],
            is_error: false,
            structured_content: None,
        };
        let output = mcp_result_to_tool_output(&result);
        assert!(output.content.contains("[Audio:"));
        assert!(output.content.contains("audio/wav"));
    }

    #[test]
    fn mcp_result_to_tool_output_with_inline_resource() {
        let result = CallToolResult {
            content: vec![McpContent::Resource {
                uri: "file:///tmp/a".into(),
                mime_type: Some("text/plain".into()),
                text: Some("body".into()),
            }],
            is_error: false,
            structured_content: None,
        };
        assert_eq!(mcp_result_to_tool_output(&result).content, "body");
    }

    #[test]
    fn mcp_result_to_tool_output_with_resource_link() {
        let result = CallToolResult {
            content: vec![McpContent::ResourceLink {
                uri: "file:///tmp/a".into(),
                name: Some("a".into()),
                mime_type: None,
            }],
            is_error: false,
            structured_content: None,
        };
        assert!(
            mcp_result_to_tool_output(&result)
                .content
                .contains("Resource link")
        );
    }

    #[test]
    fn mcp_result_to_tool_output_is_error() {
        let result = CallToolResult {
            content: vec![McpContent::Text {
                text: "error msg".into(),
            }],
            is_error: true,
            structured_content: None,
        };
        let output = mcp_result_to_tool_output(&result);
        assert!(output.is_error);
        assert_eq!(output.content, "error msg");
    }

    #[test]
    fn mcp_result_carries_structured_content() {
        let result = CallToolResult {
            content: vec![],
            is_error: false,
            structured_content: Some(serde_json::json!({"value": 42})),
        };
        let output = mcp_result_to_tool_output(&result);
        assert_eq!(output.result_json, Some(serde_json::json!({"value": 42})));
    }

    #[test]
    fn mcp_result_to_tool_output_empty_content() {
        let output = mcp_result_to_tool_output(&empty_result());
        assert!(!output.is_error);
        assert_eq!(output.content, "");
    }

    #[test]
    fn mcp_result_to_string_text_only() {
        assert_eq!(mcp_result_to_string(&text_result("hello")), "hello");
    }

    #[test]
    fn mcp_result_to_string_joins_multiple() {
        let result = CallToolResult {
            content: vec![
                McpContent::Text { text: "a".into() },
                McpContent::Text { text: "b".into() },
            ],
            is_error: false,
            structured_content: None,
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
                mime_type: "image/png".into(),
            }],
            is_error: false,
            structured_content: None,
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
                mime_type: "image/png".into(),
            }],
            is_error: false,
            structured_content: None,
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
                mime_type: "image/png".into(),
            }],
            is_error: false,
            structured_content: None,
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
}
