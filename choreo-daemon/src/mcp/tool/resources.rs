//! The two MCP resource-catalogue wrapper tools.
//!
//! A server that declares the `resources` capability is exposed to the model
//! as two catalogue tools (`McpListResourcesTool`/`McpReadResourceTool`) rather
//! than one registry entry per resource: the catalogue can be large and may
//! change while the daemon runs, so it is read on demand instead of snapshotted
//! into the tool registry.

use crate::tools::context::ToolContext;
use crate::tools::{PreparedImage, ToolDyn, ToolError, ToolOutput, ToolOutputFormat, encode_outer};
use choreo_ai_protocols::openai::AllowedCaller;
use choreo_keystore::ServiceCredential;
use choreo_mcp::{CallToolResult, McpServerHandle};
use crossbeam_channel;
use serde_json::Value;

use super::content::{join_text_parts, map_mcp_result};
use super::parse_json_args;

/// The empty-argument schema shared by the resource-catalogue tools.
fn empty_object_schema() -> Value {
    serde_json::json!({"type": "object", "additionalProperties": false})
}

/// Description for the `list_resources` catalogue tool.
pub(super) const LIST_RESOURCES_DESC: &str = "List the resources this server exposes.";

/// Description for the `read_resource` catalogue tool.
pub(super) const READ_RESOURCE_DESC: &str = "Read one resource by URI (see list_resources).";

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
mod tests {
    use super::*;

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
}
