//! The `ToolDyn` wrappers the daemon registers from an MCP server's tools.
//!
//! `McpToolWrapper` adapts one advertised server tool; the two resource
//! catalogue wrappers live in `resources`, and the MCP→`ToolOutput` content
//! mapping in `content`. The public wrapper types are re-exported here so every
//! `crate::mcp::tool::…` path is unchanged.

mod content;
mod resources;

pub use resources::{McpListResourcesTool, McpReadResourceTool};

use crate::tools::context::ToolContext;
use crate::tools::{PreparedImage, ToolDyn, ToolError, ToolOutput, ToolOutputFormat, encode_outer};
use anyhow::{Context, Result};
use choreo_ai_protocols::openai::AllowedCaller;
use choreo_keystore::ServiceCredential;
use choreo_mcp::{CallToolResult, McpServerHandle};
use content::{join_text_parts, map_mcp_result};
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

#[cfg(test)]
mod tests {
    use super::*;
    use choreo_mcp::McpTool;

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
