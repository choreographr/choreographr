//! The `ToolDyn` wrappers the daemon registers from an MCP server's tools.
//!
//! `McpToolWrapper` adapts one advertised server tool; the two resource
//! catalogue wrappers live in `resources`, and the MCP→`ToolOutput` content
//! mapping in `content`. The public wrapper types are re-exported here so every
//! `crate::mcp::tool::…` path is unchanged.

mod content;
mod resources;

use resources::{LIST_RESOURCES_DESC, READ_RESOURCE_DESC};
pub use resources::{McpListResourcesTool, McpReadResourceTool};

use crate::tools::context::ToolContext;
use crate::tools::{PreparedImage, ToolDyn, ToolError, ToolOutput, ToolOutputFormat, encode_outer};
use anyhow::{Context, Result};
use choreo_ai_protocols::openai::AllowedCaller;
use choreo_keystore::ServiceCredential;
use choreo_mcp::{CallToolResult, McpServerHandle, McpTool};
use content::{join_text_parts, map_mcp_result};
use crossbeam_channel;
use serde_json::Value;
use std::collections::HashSet;
use std::sync::atomic::Ordering;
use tracing::{debug, info};

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

/// The `[MCP <slug>]`-prefixed description every wrapper carries.
///
/// Centralized so the prefix convention is defined once. The tool name comes
/// from the per-server collision resolution ([`resolve_name`]) and the group
/// from [`choreo_mcp::group_name`]; only the description needs a shared helper
/// here, so the two never drift from the production registration path.
fn prefixed_description(server_slug: &str, description: &str) -> String {
    format!("[MCP {server_slug}] {description}")
}

/// Resolve the provider-safe name for `tool` on `slug`, appending a hash suffix
/// when another tool has already claimed the sanitized name.
///
/// Shared by the daemon-wide registration and the per-session overlay so their
/// first-come-first-served disambiguation is identical.
pub(super) fn resolve_name(slug: &str, tool: &str, used: &HashSet<String>) -> String {
    let base = choreo_mcp::build_tool_name(slug, tool);
    if !used.contains(&base) {
        return base;
    }
    choreo_mcp::build_tool_name_with_suffix(slug, tool, &format!("{slug}\u{0}{tool}"))
}

/// The wrappers one server contributes to a catalogue, with their resolved
/// names and the group they belong to.
pub(super) struct ServerWrappers {
    /// The `mcp/<slug>` group every wrapper belongs to.
    pub(super) group: String,
    /// The number of server TOOLS (excludes the resource-catalogue wrappers).
    pub(super) tool_count: usize,
    /// The wrappers in registration order (server tools first, then the
    /// resource-catalogue tools), each paired with its resolved name.
    pub(super) tools: Vec<(String, Box<dyn ToolDyn>)>,
}

/// Build the wrapper tools for one server's `tools`, resolving provider-safe
/// names against the shared `used` set.
///
/// Both the daemon-wide registration ([`crate::mcp::McpManager`]) and the
/// per-session overlay build through here, so their naming cannot drift. The
/// resource-catalogue tools receive the SAME names reserved in `used` (the
/// reservation is not discarded), which is what keeps a server tool named
/// `list_resources`/`read_resource` — or a same-segment collision with another
/// server — from colliding with the catalogue tools.
pub(super) fn build_server_wrappers(
    slug: &str,
    handle: &McpServerHandle,
    tools: &[McpTool],
    disabled: &[String],
    used: &mut HashSet<String>,
) -> ServerWrappers {
    let group = choreo_mcp::group_name(slug);
    let disabled: HashSet<&str> = disabled.iter().map(String::as_str).collect();
    let supports_resources = handle.supports_resources();

    // Reserve the resource-catalogue names FIRST and keep them, so a server
    // tool that sanitizes onto the same name takes the collision suffix and the
    // catalogue tool is registered under the reserved (possibly suffixed) name
    // rather than a recomputed base.
    let resource_names = supports_resources.then(|| {
        let list = resolve_name(slug, "list_resources", used);
        used.insert(list.clone());
        let read = resolve_name(slug, "read_resource", used);
        used.insert(read.clone());
        (list, read)
    });

    let mut wrappers: Vec<(String, Box<dyn ToolDyn>)> = Vec::new();
    let mut tool_count = 0usize;
    for mcp_tool in tools {
        if disabled.contains(mcp_tool.name.as_str()) {
            debug!(server = %slug, tool = %mcp_tool.name, "MCP tool disabled by config");
            continue;
        }
        let description = mcp_tool.description.clone().unwrap_or_default();
        let name = resolve_name(slug, &mcp_tool.name, used);
        used.insert(name.clone());
        let wrapper = McpToolWrapper::with_name(
            name.clone(),
            group.clone(),
            prefixed_description(slug, &description),
            mcp_tool.name.clone(),
            mcp_tool.input_schema.clone(),
            mcp_tool.output_schema.clone(),
            handle.clone(),
        );
        wrappers.push((name, Box::new(wrapper)));
        tool_count += 1;
    }

    if let Some((list_name, read_name)) = resource_names {
        let lister = McpListResourcesTool::with_name(
            list_name.clone(),
            group.clone(),
            prefixed_description(slug, LIST_RESOURCES_DESC),
            handle.clone(),
        );
        let reader = McpReadResourceTool::with_name(
            read_name.clone(),
            group.clone(),
            prefixed_description(slug, READ_RESOURCE_DESC),
            handle.clone(),
        );
        wrappers.push((list_name, Box::new(lister)));
        wrappers.push((read_name, Box::new(reader)));
        info!(server = %slug, "registered MCP resource tools");
    }

    ServerWrappers {
        group,
        tool_count,
        tools: wrappers,
    }
}

/// Wraps an MCP server tool as a `ToolDyn` for Choreographr's tool registry.
pub struct McpToolWrapper {
    /// Full prefixed name: "mcp__<`server_slug`>__<`tool_name`>"
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
    use std::collections::HashSet;

    /// A handle with no dispatcher behind it: the wrapper's metadata methods do
    /// not call the server, so a disconnected handle is sufficient here.
    fn unused_handle() -> McpServerHandle {
        McpServerHandle::disconnected("fixture", "0.1.0", std::time::Duration::from_secs(5))
    }

    #[test]
    fn resolve_name_disambiguates_a_collision() {
        // Two names that sanitize onto the same segment must be pulled apart by
        // the hash suffix, and the result must stay within the provider cap.
        let mut used = HashSet::new();
        let first = resolve_name("s", "a.b", &used);
        used.insert(first.clone());
        let second = resolve_name("s", "a_b", &used);
        assert_ne!(first, second);
        assert!(second.len() <= choreo_mcp::MAX_TOOL_NAME_LEN);
        assert_eq!(second, resolve_name("s", "a_b", &used));
    }

    #[test]
    fn build_server_wrappers_reserves_catalogue_names_for_colliding_tools() {
        // A server that advertises its OWN `list_resources` tool AND the
        // `resources` capability: the catalogue tool must keep the reserved
        // name while the server tool takes the collision suffix, so neither is
        // registered under a name the other already claimed.
        let handle = McpServerHandle::disconnected_with_resources("srv");
        let tools = vec![McpTool {
            name: "list_resources".into(),
            description: None,
            input_schema: serde_json::json!({"type": "object"}),
            output_schema: None,
        }];
        let mut used = HashSet::new();
        let built = build_server_wrappers("srv", &handle, &tools, &[], &mut used);
        let names: Vec<&str> = built.tools.iter().map(|(n, _)| n.as_str()).collect();
        // The catalogue tools keep the reserved names...
        assert!(names.contains(&"mcp__srv__list_resources"), "{names:?}");
        assert!(names.contains(&"mcp__srv__read_resource"), "{names:?}");
        // ...and the server's own `list_resources` tool is disambiguated.
        let server_tool = names
            .iter()
            .find(|n| n.starts_with("mcp__srv__list_resources-"))
            .expect("the colliding server tool takes a hash suffix");
        assert_ne!(*server_tool, "mcp__srv__list_resources");
        assert_eq!(built.tool_count, 1, "only server tools count");
    }

    #[test]
    fn build_server_wrappers_prefixes_name_and_description() {
        let tools = vec![McpTool {
            name: "echo".into(),
            description: Some("Echo a message back.".into()),
            input_schema: serde_json::json!({"type": "object"}),
            output_schema: Some(serde_json::json!({"type": "object"})),
        }];
        let mut used = HashSet::new();
        let built = build_server_wrappers("fixture", &unused_handle(), &tools, &[], &mut used);
        assert_eq!(built.group, "mcp/fixture");
        assert_eq!(built.tool_count, 1);
        let (name, wrapper) = &built.tools[0];
        assert_eq!(name, "mcp__fixture__echo");
        assert_eq!(wrapper.group(), "mcp/fixture");
        assert_eq!(wrapper.description(), "[MCP fixture] Echo a message back.");
        assert_eq!(
            wrapper.output_schema(),
            Some(serde_json::json!({"type": "object"}))
        );
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
