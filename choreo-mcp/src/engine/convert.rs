//! Value mapping between `rmcp` types and the crate's own daemon-facing types.
//!
//! Every `rmcp` value that crosses the engine boundary is translated here:
//! tools (with schema validation and the per-server cap), listed resources,
//! resource contents, `tools/call` results and their content blocks, and the
//! `rmcp` service errors that surface as this crate's [`McpError`].

use super::http::status_from_chain;
use crate::error::McpError;
use crate::protocol::{
    CallToolResult, McpContent, McpResource, McpTool, cap_tools, normalize_input_schema,
    normalize_output_schema,
};
use rmcp::model::{ContentBlock, ResourceContents};
use rmcp::service::ServiceError;

/// Convert rmcp tools, dropping any whose `inputSchema` is unusable and
/// truncating the catalogue at [`MAX_TOOLS_PER_SERVER`](crate::MAX_TOOLS_PER_SERVER).
pub(super) fn convert_tools(tools: Vec<rmcp::model::Tool>) -> Vec<McpTool> {
    let converted: Vec<McpTool> = tools.into_iter().filter_map(convert_tool).collect();
    let (kept, dropped) = cap_tools(converted);
    if dropped > 0 {
        tracing::warn!(
            dropped,
            cap = crate::MAX_TOOLS_PER_SERVER,
            "MCP server advertised more tools than the per-server cap; extra tools dropped"
        );
    }
    kept
}

/// Convert one rmcp tool, returning `None` when its schema must be rejected.
///
/// A tool with a non-object, oversized, or over-deep `inputSchema` is dropped
/// (the rest are kept), per the spec's "exclude the offending tool" rule. An
/// out-of-bounds `outputSchema` is dropped while the tool is kept, since it is
/// advisory.
fn convert_tool(tool: rmcp::model::Tool) -> Option<McpTool> {
    let input_schema = normalize_input_schema(tool.schema_as_json_value())?;
    let output_schema = tool
        .output_schema
        .map(|schema| serde_json::Value::Object(schema.as_ref().clone()))
        .and_then(normalize_output_schema);
    Some(McpTool {
        name: tool.name.into_owned(),
        description: tool.description.map(std::borrow::Cow::into_owned),
        input_schema,
        output_schema,
    })
}

/// Convert a listed rmcp resource into this crate's value type.
pub(super) fn convert_listed_resource(resource: rmcp::model::Resource) -> McpResource {
    McpResource {
        uri: resource.uri,
        name: Some(resource.name),
        description: resource.description,
        mime_type: resource.mime_type,
    }
}

/// Convert one resource's contents from a `resources/read` result.
pub(super) fn convert_resource_contents(contents: ResourceContents) -> McpContent {
    convert_resource(contents)
}

/// Convert an rmcp `tools/call` result into this crate's value type.
pub(super) fn convert_call_result(result: rmcp::model::CallToolResult) -> CallToolResult {
    CallToolResult {
        content: result.content.into_iter().map(convert_content).collect(),
        is_error: result.is_error.unwrap_or(false),
        structured_content: result.structured_content,
    }
}

/// Convert one rmcp content block; unknown future variants degrade to a text
/// placeholder rather than vanishing.
fn convert_content(block: ContentBlock) -> McpContent {
    match block {
        ContentBlock::Text(text) => McpContent::Text { text: text.text },
        ContentBlock::Image(image) => McpContent::Image {
            data: image.data,
            mime_type: image.mime_type,
        },
        ContentBlock::Audio(audio) => McpContent::Audio {
            data: audio.data,
            mime_type: audio.mime_type,
        },
        ContentBlock::Resource(resource) => convert_resource(resource.resource),
        ContentBlock::ResourceLink(link) => McpContent::ResourceLink {
            uri: link.uri,
            name: Some(link.name),
            mime_type: link.mime_type,
        },
        _ => McpContent::Text {
            text: "[unsupported content block]".to_string(),
        },
    }
}

/// Convert an embedded resource's contents (text or blob).
fn convert_resource(contents: ResourceContents) -> McpContent {
    match contents {
        ResourceContents::TextResourceContents {
            uri,
            mime_type,
            text,
            ..
        } => McpContent::Resource {
            uri,
            mime_type,
            text: Some(text),
        },
        ResourceContents::BlobResourceContents { uri, mime_type, .. } => McpContent::Resource {
            uri,
            mime_type,
            text: None,
        },
        _ => McpContent::Text {
            text: "[unsupported resource contents]".to_string(),
        },
    }
}

/// Map an `rmcp` service error onto this crate's error type.
///
/// An error carrying an HTTP 401/403 status is surfaced as an actionable
/// [`McpError::AuthRequired`] naming `slug`, so a mid-session authorization
/// failure (not just a connect-time one) explains itself rather than appearing
/// as an opaque transport error.
///
/// A send that fails at the transport layer becomes [`McpError::Transport`] —
/// the reconnect trigger — rather than an opaque protocol error, so a dropped
/// or refused connection reaches the dispatcher's restart policy instead of
/// leaving it serving against a dead transport. The two exceptions are the
/// settled answers: an authorization challenge is actionable, and a genuinely
/// malformed exchange (an unexpected response type, an over-run MRTR loop) is a
/// protocol error a rebuild cannot fix.
pub(super) fn map_service_error(error: ServiceError, slug: &str) -> McpError {
    match error {
        ServiceError::McpError(data) => McpError::JsonRpcError {
            code: i64::from(data.code.0),
            message: data.message.into_owned(),
        },
        ServiceError::TransportClosed => McpError::ServerShutdown,
        ServiceError::Timeout { .. } => McpError::Timeout,
        ServiceError::Cancelled { .. } => McpError::Cancelled,
        // A rejected send is a transport failure, not a protocol error: the
        // connection is (or may be) dead, so it must reach the reconnect
        // policy. rmcp delivers it as `TransportSend`, whose inner error is
        // where an HTTP status lives — `TransportSend`'s boxed payload is not
        // exposed through `source()`, so the walk starts there directly. An
        // authorization challenge (HTTP 401/403) is surfaced as the actionable
        // `AuthRequired`; any other send failure is a plain transport error.
        ServiceError::TransportSend(dynamic) => match status_from_chain(dynamic.error.as_ref()) {
            Some(status @ (401 | 403)) => McpError::AuthRequired {
                server: slug.to_string(),
                hint: auth_hint(status),
            },
            _ => McpError::Transport(dynamic.to_string()),
        },
        other => McpError::ProtocolError(other.to_string()),
    }
}

/// The actionable guidance attached to an authorization-required error.
///
/// Names both supported paths: a static token in the server's `headers`, and
/// the OAuth support that is not yet shipped.
pub(super) fn auth_hint(status: u16) -> String {
    format!(
        "the server answered HTTP {status}; configure a static token in this server's \
         \"headers\" (for example \"Authorization\": \"Bearer ${{TOKEN}}\"), or wait \
         for OAuth support, which is not yet available"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use rmcp::transport::DynamicTransportError;
    use rmcp::transport::streamable_http_client::StreamableHttpError;

    #[test]
    fn content_blocks_convert() {
        let text = convert_content(ContentBlock::text("hello"));
        assert_eq!(
            text,
            McpContent::Text {
                text: "hello".into()
            }
        );
        let image = convert_content(ContentBlock::image("AAA", "image/png"));
        assert_eq!(
            image,
            McpContent::Image {
                data: "AAA".into(),
                mime_type: "image/png".into()
            }
        );
        let audio = convert_content(ContentBlock::audio("BBB", "audio/wav"));
        assert_eq!(
            audio,
            McpContent::Audio {
                data: "BBB".into(),
                mime_type: "audio/wav".into()
            }
        );
    }

    #[test]
    fn embedded_text_resource_converts() {
        let block = ContentBlock::embedded_text("file:///x", "body");
        assert_eq!(
            convert_content(block),
            McpContent::Resource {
                uri: "file:///x".into(),
                mime_type: Some("text/plain".into()),
                text: Some("body".into()),
            }
        );
    }

    #[test]
    fn service_error_maps_json_rpc() {
        let data = rmcp::model::ErrorData::new(
            rmcp::model::ErrorCode::METHOD_NOT_FOUND,
            "nope".to_string(),
            None,
        );
        match map_service_error(ServiceError::McpError(data), "test") {
            McpError::JsonRpcError { code, message } => {
                assert_eq!(code, -32601);
                assert_eq!(message, "nope");
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn service_error_maps_unauthorized_to_auth_required() {
        // A transport error whose source carries a 401 is surfaced as the
        // actionable AuthRequired naming the server, not an opaque error.
        let inner = StreamableHttpError::<reqwest::Error>::UnexpectedServerResponse(
            "HTTP 401 Unauthorized: token missing".into(),
        );
        let dynamic = DynamicTransportError::from_parts(
            "test",
            std::any::TypeId::of::<()>(),
            Box::new(inner),
        );
        match map_service_error(ServiceError::TransportSend(dynamic), "docs") {
            McpError::AuthRequired { server, hint } => {
                assert_eq!(server, "docs");
                assert!(hint.contains("headers"), "{hint}");
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn service_error_maps_status_less_send_failure_to_transport() {
        // A rejected send whose inner error carries no HTTP status (a dropped
        // connection, not an HTTP response) must map to the reconnect-triggering
        // `Transport`, not an opaque protocol error.
        let inner = StreamableHttpError::<reqwest::Error>::UnexpectedServerResponse(
            "connection reset by peer".into(),
        );
        let dynamic = DynamicTransportError::from_parts(
            "test",
            std::any::TypeId::of::<()>(),
            Box::new(inner),
        );
        match map_service_error(ServiceError::TransportSend(dynamic), "docs") {
            McpError::Transport(_) => {}
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn tool_conversion_keeps_object_schema() {
        let json = serde_json::json!({
            "name": "ok",
            "description": "d",
            "inputSchema": {"type": "object"}
        });
        let tool: rmcp::model::Tool = serde_json::from_value(json).expect("tool decodes");
        let converted = convert_tool(tool).expect("valid schema converts");
        assert_eq!(converted.name, "ok");
        assert_eq!(converted.input_schema["type"], "object");
    }

    #[test]
    fn tool_conversion_rejects_oversized_schema() {
        // rmcp itself enforces that `inputSchema` is a JSON object, so the
        // client-side guard that still matters is the byte cap.
        let filler = "x".repeat(crate::protocol::MAX_SCHEMA_BYTES + 1);
        let json = serde_json::json!({
            "name": "big",
            "inputSchema": {"type": "object", "description": filler}
        });
        let tool: rmcp::model::Tool = serde_json::from_value(json).expect("tool decodes");
        assert!(convert_tool(tool).is_none());
    }

    #[test]
    fn convert_tools_truncates_to_the_cap() {
        // A server advertising more tools than the cap keeps the leading prefix.
        let json: Vec<serde_json::Value> = (0..crate::MAX_TOOLS_PER_SERVER + 5)
            .map(|n| {
                serde_json::json!({
                    "name": format!("t{n}"),
                    "inputSchema": {"type": "object"}
                })
            })
            .collect();
        let tools: Vec<rmcp::model::Tool> = json
            .into_iter()
            .map(|v| serde_json::from_value(v).expect("tool decodes"))
            .collect();
        let converted = convert_tools(tools);
        assert_eq!(converted.len(), crate::MAX_TOOLS_PER_SERVER);
        assert_eq!(converted.first().map(|t| t.name.as_str()), Some("t0"));
    }
}
