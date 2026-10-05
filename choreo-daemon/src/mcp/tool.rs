use crate::tools::context::ToolContext;
use crate::tools::image::prepare_image_from_bytes;
use crate::tools::{PreparedImage, ToolDyn, ToolError, ToolOutput, ToolOutputFormat, encode_outer};
use anyhow::{Context, Result};
use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use choreo_ai_protocols::openai::AllowedCaller;
use choreo_keystore::ServiceCredential;
use choreo_mcp::{CallToolResult, McpContent, McpServerHandle};
use crossbeam_channel;
use serde_json::Value;
use std::sync::atomic::Ordering;

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

/// The provider-safe `(name, group)` pair and `[MCP <slug>]`-prefixed
/// description for one server tool.
///
/// Centralized so the naming, grouping, and prefixing convention is defined
/// once and every wrapper constructor derives its three strings from it rather
/// than re-assembling them.
fn prefixed_identity(
    server_slug: &str,
    tool_name: &str,
    description: &str,
) -> (String, String, String) {
    (
        choreo_mcp::build_tool_name(server_slug, tool_name),
        choreo_mcp::group_name(server_slug),
        format!("[MCP {server_slug}] {description}"),
    )
}

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
    /// The server's `outputSchema`, when it advertised one.
    output_schema: Option<Value>,
    /// The original tool name as the MCP server knows it
    original_name: String,
    /// Cloneable handle to the server's dispatcher (one per server, shared
    /// across all tools from that server).
    handle: McpServerHandle,
}

impl McpToolWrapper {
    /// Build a wrapper, deriving the provider-safe name and group from the
    /// server slug and the server's own tool name.
    #[must_use]
    pub fn new(
        server_slug: &str,
        tool_name: &str,
        description: &str,
        input_schema: Value,
        output_schema: Option<Value>,
        handle: McpServerHandle,
    ) -> Self {
        let (name, group, description) = prefixed_identity(server_slug, tool_name, description);
        Self::with_name(
            name,
            group,
            description,
            tool_name.to_string(),
            input_schema,
            output_schema,
            handle,
        )
    }

    /// Build a wrapper with an already-resolved provider-safe `name` and
    /// catalogue `group` (used by the daemon when it must disambiguate a
    /// collision by appending a hash).
    #[must_use]
    pub fn with_name(
        name: String,
        group: String,
        description: String,
        original_name: String,
        input_schema: Value,
        output_schema: Option<Value>,
        handle: McpServerHandle,
    ) -> Self {
        Self {
            name,
            group,
            description,
            input_schema,
            output_schema,
            original_name,
            handle,
        }
    }

    fn call_with_args(
        &self,
        args: Value,
        session_id: u64,
        chunk_tx: Option<crossbeam_channel::Sender<Vec<u8>>>,
    ) -> Result<CallToolResult> {
        self.handle
            .call_tool_streaming(session_id, &self.original_name, args, None, chunk_tx)
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

/// The session a call belongs to (0 when no context is available, e.g. a direct
/// unit-test call): used so a later session cancel can stop the call.
fn session_of(ctx: Option<&ToolContext>) -> u64 {
    ctx.map_or(0, |ctx| ctx.session_id)
}

/// Whether the session has already been cancelled, checked at the call
/// boundary so a cancelled session does not start a fresh MCP call.
fn is_cancelled(ctx: Option<&ToolContext>) -> bool {
    ctx.is_some_and(|ctx| ctx.cancelled.load(Ordering::Relaxed))
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
    /// The server's `structuredContent`, preserved for the programmatic path.
    structured: Option<Value>,
}

/// Map an MCP [`CallToolResult`] onto the daemon's tool-output shape.
///
/// When `attach_images` is true, `image` blocks are base64-decoded and routed
/// to the daemon image pipeline; a decode or validation failure (or a caller
/// with no image sink) degrades to a text placeholder instead of dropping the
/// content silently. Content with no textual form (audio, blob resources,
/// resource links) is described rather than dropped.
fn map_mcp_result(result: &CallToolResult, attach_images: bool) -> MappedResult {
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
        self.description.clone()
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
        // The server's real `outputSchema`, when it advertised one; `None` is
        // honest for a tool that returns free-form content.
        self.output_schema.clone()
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
        ctx: Option<&ToolContext>,
        image_tx: Option<crossbeam_channel::Sender<PreparedImage>>,
    ) -> Result<ToolOutput, ToolError> {
        if is_cancelled(ctx) {
            return Ok(cancelled_output());
        }
        let args = parse_json_args(args_json)?;
        let result = self
            .call_with_args(args, session_of(ctx), None)
            .map_err(|e| to_tool_error(&e))?;
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
            result_json: mapped.structured,
            invocation_description: String::new(),
            ..Default::default()
        })
    }

    fn execute_postcard(
        &self,
        args_bytes: &[u8],
        _x_credentials: Option<&ServiceCredential>,
        _working_dir: Option<&std::path::Path>,
        ctx: Option<&ToolContext>,
    ) -> Vec<u8> {
        if is_cancelled(ctx) {
            return encode_outer::<String, String>(&Ok(Err("MCP call cancelled".to_string())));
        }
        let args = match parse_binary_args(args_bytes) {
            Ok(v) => v,
            Err(e) => return e,
        };
        // The postcard wire shape has no `is_error` bit, so a server-flagged
        // error is surfaced as the domain `Err` (the byte-level analogue of
        // `ToolOutput.is_error`) rather than being dropped.
        let result: Result<String, String> = match self.call_with_args(args, session_of(ctx), None)
        {
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
        ctx: Option<&ToolContext>,
        image_tx: Option<crossbeam_channel::Sender<PreparedImage>>,
    ) -> Result<ToolOutput, ToolError> {
        if is_cancelled(ctx) {
            return Ok(cancelled_output());
        }
        let args = parse_json_args(args_json)?;
        // Forward the server's progress notifications to the same live-output
        // channel the final text uses, so a long MCP tool reports progress as
        // it runs (the engine rate-limits and drops on a full sink).
        let result = self
            .call_with_args(args, session_of(ctx), Some(output_tx.clone()))
            .map_err(|e| to_tool_error(&e))?;
        let mapped = map_mcp_result(&result, image_tx.is_some());
        if let Some(tx) = &image_tx {
            for image in mapped.images {
                let _ = tx.send(image);
            }
        }
        let text_content = join_text_parts(&mapped.text_parts);
        // Stream the text content for incremental display, then return it as
        // the final output.
        let _ = output_tx.send(text_content.as_bytes().to_vec());
        Ok(ToolOutput {
            content: match format {
                ToolOutputFormat::Text => text_content,
                ToolOutputFormat::Json => {
                    serde_json::to_string(&text_content).unwrap_or(text_content)
                }
            },
            is_error: mapped.is_error,
            result_json: mapped.structured,
            invocation_description: String::new(),
            ..Default::default()
        })
    }
}

/// The output returned when a session cancel is observed at the call boundary.
fn cancelled_output() -> ToolOutput {
    ToolOutput {
        content: "MCP call cancelled".to_string(),
        is_error: true,
        invocation_description: String::new(),
        ..Default::default()
    }
}

/// The empty-argument schema shared by the resource-catalogue tools.
fn empty_object_schema() -> Value {
    serde_json::json!({"type": "object", "additionalProperties": false})
}

/// The argument schema for `read_resource`: a single required resource URI.
fn read_resource_schema() -> Value {
    serde_json::json!({
        "type": "object",
        "properties": {
            "uri": {
                "type": "string",
                "description": "The resource URI to read, as returned by list_resources."
            }
        },
        "required": ["uri"],
        "additionalProperties": false
    })
}

/// Wrapper tool listing an MCP server's resources (`mcp/<slug>/list_resources`).
///
/// Resources are exposed to the model as two catalogue tools rather than one
/// registry entry per resource: the catalogue can be large and may change
/// while the daemon runs, so it is read on demand instead of snapshotted into
/// the tool registry.
pub struct McpListResourcesTool {
    name: String,
    group: String,
    description: String,
    handle: McpServerHandle,
}

impl McpListResourcesTool {
    /// Build the listing tool for one server.
    #[must_use]
    pub fn new(server_slug: &str, handle: McpServerHandle) -> Self {
        let (name, group, description) = prefixed_identity(
            server_slug,
            "list_resources",
            "List the resources this server exposes.",
        );
        Self::with_name(name, group, description, handle)
    }

    /// Build the listing tool with an already-resolved name and group.
    #[must_use]
    pub fn with_name(
        name: String,
        group: String,
        description: String,
        handle: McpServerHandle,
    ) -> Self {
        Self {
            name,
            group,
            description,
            handle,
        }
    }
}

/// Format a resource catalogue into the model-visible text listing.
fn format_resource_listing(resources: &[choreo_mcp::McpResource]) -> String {
    if resources.is_empty() {
        return "No resources.".to_string();
    }
    resources
        .iter()
        .map(|r| {
            let name = r.name.as_deref().unwrap_or(&r.uri);
            let mime = r.mime_type.as_deref().unwrap_or("unknown");
            match &r.description {
                Some(description) => {
                    format!("{name}\n  uri: {}\n  type: {mime}\n  {description}", r.uri)
                }
                None => format!("{name}\n  uri: {}\n  type: {mime}", r.uri),
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

impl ToolDyn for McpListResourcesTool {
    fn describe_invocation_json(&self, _args_json: &str) -> String {
        self.description.clone()
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
        empty_object_schema()
    }
    fn output_schema(&self) -> Option<Value> {
        None
    }
    fn allowed_callers(&self) -> Vec<AllowedCaller> {
        vec![AllowedCaller::Direct, AllowedCaller::Programmatic]
    }
    fn supports_streaming_output(&self) -> bool {
        false
    }
    fn execute_json(
        &self,
        _args_json: &str,
        format: ToolOutputFormat,
        _x_credentials: Option<&ServiceCredential>,
        _working_dir: Option<&std::path::Path>,
        _ctx: Option<&ToolContext>,
        _image_tx: Option<crossbeam_channel::Sender<PreparedImage>>,
    ) -> Result<ToolOutput, ToolError> {
        let listed = self
            .handle
            .list_resources()
            .map_err(|e| ToolError::Other(format!("failed to list MCP resources: {e:#}")))?;
        let content = format_resource_listing(&listed);
        Ok(ToolOutput {
            content: match format {
                ToolOutputFormat::Text => content,
                ToolOutputFormat::Json => serde_json::to_string(&content).unwrap_or(content),
            },
            is_error: false,
            invocation_description: String::new(),
            ..Default::default()
        })
    }

    fn execute_postcard(
        &self,
        _args_bytes: &[u8],
        _x_credentials: Option<&ServiceCredential>,
        _working_dir: Option<&std::path::Path>,
        _ctx: Option<&ToolContext>,
    ) -> Vec<u8> {
        let result: Result<String, String> = match self.handle.list_resources() {
            Ok(listed) => Ok(format_resource_listing(&listed)),
            Err(e) => Err(format!("failed to list MCP resources: {e:#}")),
        };
        encode_outer::<String, String>(&Ok(result))
    }

    fn execute_streaming_json(
        &self,
        args_json: &str,
        format: ToolOutputFormat,
        x_credentials: Option<&ServiceCredential>,
        working_dir: Option<&std::path::Path>,
        _output_tx: crossbeam_channel::Sender<Vec<u8>>,
        ctx: Option<&ToolContext>,
        image_tx: Option<crossbeam_channel::Sender<PreparedImage>>,
    ) -> Result<ToolOutput, ToolError> {
        // Not a streaming tool: the catalogue is one result.
        self.execute_json(args_json, format, x_credentials, working_dir, ctx, image_tx)
    }
}

/// Wrapper tool reading one MCP resource (`mcp/<slug>/read_resource`).
pub struct McpReadResourceTool {
    name: String,
    group: String,
    description: String,
    handle: McpServerHandle,
}

impl McpReadResourceTool {
    /// Build the read tool for one server.
    #[must_use]
    pub fn new(server_slug: &str, handle: McpServerHandle) -> Self {
        let (name, group, description) = prefixed_identity(
            server_slug,
            "read_resource",
            "Read one resource by URI (see list_resources).",
        );
        Self::with_name(name, group, description, handle)
    }

    /// Build the read tool with an already-resolved name and group.
    #[must_use]
    pub fn with_name(
        name: String,
        group: String,
        description: String,
        handle: McpServerHandle,
    ) -> Self {
        Self {
            name,
            group,
            description,
            handle,
        }
    }

    /// Extract the required `uri` argument.
    fn uri_arg(args: &Value) -> Result<String, ToolError> {
        args.get("uri")
            .and_then(Value::as_str)
            .map(ToString::to_string)
            .ok_or_else(|| {
                ToolError::InvalidArguments("missing required string argument 'uri'".to_string())
            })
    }

    /// Read `uri` and render its contents for the model.
    fn read_text(&self, uri: &str) -> Result<String, String> {
        let contents = self
            .handle
            .read_resource(uri)
            .map_err(|e| format!("failed to read MCP resource {uri:?}: {e:#}"))?;
        // Reuse the tool-result content mapping so a resource's text, blob, or
        // link content degrades exactly as a tool result would.
        let result = CallToolResult {
            content: contents,
            is_error: false,
            structured_content: None,
        };
        let mapped = map_mcp_result(&result, false);
        Ok(join_text_parts(&mapped.text_parts))
    }
}

impl ToolDyn for McpReadResourceTool {
    fn describe_invocation_json(&self, args_json: &str) -> String {
        match serde_json::from_str::<Value>(args_json) {
            Ok(args) => args.get("uri").and_then(Value::as_str).map_or_else(
                || self.description.clone(),
                |uri| format!("Read resource {uri}"),
            ),
            Err(_) => self.description.clone(),
        }
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
        read_resource_schema()
    }
    fn output_schema(&self) -> Option<Value> {
        None
    }
    fn allowed_callers(&self) -> Vec<AllowedCaller> {
        vec![AllowedCaller::Direct, AllowedCaller::Programmatic]
    }
    fn supports_streaming_output(&self) -> bool {
        false
    }
    fn execute_json(
        &self,
        args_json: &str,
        format: ToolOutputFormat,
        _x_credentials: Option<&ServiceCredential>,
        _working_dir: Option<&std::path::Path>,
        _ctx: Option<&ToolContext>,
        _image_tx: Option<crossbeam_channel::Sender<PreparedImage>>,
    ) -> Result<ToolOutput, ToolError> {
        let args = parse_json_args(args_json)?;
        let uri = Self::uri_arg(&args)?;
        let content = self.read_text(&uri).map_err(ToolError::Other)?;
        Ok(ToolOutput {
            content: match format {
                ToolOutputFormat::Text => content,
                ToolOutputFormat::Json => serde_json::to_string(&content).unwrap_or(content),
            },
            is_error: false,
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
        let result: Result<String, String> = match postcard::from_bytes::<String>(args_bytes) {
            Ok(uri) => self.read_text(&uri),
            Err(e) => Err(format!("invalid binary arguments: {e}")),
        };
        encode_outer::<String, String>(&Ok(result))
    }

    fn execute_streaming_json(
        &self,
        args_json: &str,
        format: ToolOutputFormat,
        x_credentials: Option<&ServiceCredential>,
        working_dir: Option<&std::path::Path>,
        _output_tx: crossbeam_channel::Sender<Vec<u8>>,
        ctx: Option<&ToolContext>,
        image_tx: Option<crossbeam_channel::Sender<PreparedImage>>,
    ) -> Result<ToolOutput, ToolError> {
        // Not a streaming tool: a resource read is one result.
        self.execute_json(args_json, format, x_credentials, working_dir, ctx, image_tx)
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
    use choreo_mcp::McpTool;

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

    // ── describe / output schema ─────────────────────────────────────

    /// A handle with no dispatcher behind it: the wrapper's metadata methods do
    /// not call the server, so a disconnected handle is sufficient here.
    fn unused_handle() -> McpServerHandle {
        McpServerHandle::disconnected("fixture", "0.1.0", std::time::Duration::from_secs(5))
    }

    #[test]
    fn wrapper_prefixes_name_and_description() {
        let wrapper = McpToolWrapper::new(
            "fixture",
            "echo",
            "Echo a message back.",
            serde_json::json!({"type": "object"}),
            Some(serde_json::json!({"type": "object"})),
            unused_handle(),
        );
        assert_eq!(wrapper.name(), "mcp/fixture/echo");
        assert_eq!(wrapper.group(), "mcp/fixture");
        assert_eq!(wrapper.description(), "[MCP fixture] Echo a message back.");
        assert_eq!(
            wrapper.output_schema(),
            Some(serde_json::json!({"type": "object"}))
        );
        // Sanity: the tool type still resolves.
        let _ = McpTool {
            name: "echo".into(),
            description: None,
            input_schema: serde_json::json!({}),
            output_schema: None,
        };
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
        assert!(
            parse_json_args("{}")
                .unwrap()
                .as_object()
                .unwrap()
                .is_empty()
        );
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

    // ── resource wrapper helpers ─────────────────────────────────────

    #[test]
    fn format_resource_listing_includes_uri_and_name() {
        let resources = vec![
            choreo_mcp::McpResource {
                uri: "file:///a.txt".into(),
                name: Some("a".into()),
                description: None,
                mime_type: Some("text/plain".into()),
            },
            choreo_mcp::McpResource {
                uri: "file:///b.bin".into(),
                name: None,
                description: Some("binary".into()),
                mime_type: None,
            },
        ];
        let listing = format_resource_listing(&resources);
        assert!(listing.contains("file:///a.txt"));
        assert!(listing.contains("text/plain"));
        assert!(listing.contains("binary"));
    }

    #[test]
    fn format_resource_listing_empty() {
        assert_eq!(format_resource_listing(&[]), "No resources.");
    }

    #[test]
    fn read_resource_uri_arg_requires_string() {
        assert_eq!(
            McpReadResourceTool::uri_arg(&serde_json::json!({"uri": "x"})).unwrap(),
            "x"
        );
        assert!(McpReadResourceTool::uri_arg(&serde_json::json!({})).is_err());
        assert!(McpReadResourceTool::uri_arg(&serde_json::json!({"uri": 7})).is_err());
    }

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
