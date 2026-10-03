//! MCP value types exchanged with the daemon, decoupled from `rmcp`.
//!
//! The daemon never sees an `rmcp` type: the engine layer converts rmcp's
//! decoded values into these plain shapes ([`McpTool`], [`CallToolResult`],
//! [`McpContent`]) at the crate boundary. Keeping the boundary typed on our own
//! structs isolates the daemon from rmcp API churn and lets the content mapping
//! be unit-tested without a live server.

/// The JSON Schema substituted for a tool whose `inputSchema` is missing or
/// `null`.
///
/// The MCP spec forbids a `null` schema (a client that forwards one to a
/// provider produces an invalid tool definition), so an absent schema is
/// normalized to an explicit empty-object schema instead: it accepts exactly
/// one argument shape — no arguments — and rejects everything else.
pub const EMPTY_INPUT_SCHEMA: &str = r#"{"type":"object","additionalProperties":false}"#;

/// Upper bound on the size of a tool's `inputSchema` in bytes.
///
/// A hostile or buggy server can return an arbitrarily large schema; the client
/// caps it so a single tool definition cannot balloon the memory used per
/// server or the prompt sent to a model.
pub const MAX_SCHEMA_BYTES: usize = 256 * 1024;

/// A tool advertised by an MCP server.
#[derive(Debug, Clone, PartialEq)]
pub struct McpTool {
    /// The tool name used to invoke it via `tools/call`.
    pub name: String,
    /// Human-readable description, if the server supplied one.
    pub description: Option<String>,
    /// JSON Schema for the tool's arguments, normalized to a valid object
    /// (never `null`).
    pub input_schema: serde_json::Value,
    /// The server's `outputSchema`, when it advertised one. Captured so a
    /// programmatic caller can learn the structured shape; not yet surfaced by
    /// the daemon's wrapper.
    pub output_schema: Option<serde_json::Value>,
}

/// A resource advertised by an MCP server via `resources/list`.
///
/// Resources are not tools: the daemon exposes them to the model through
/// generated wrapper tools (`read_resource`, `list_resources`) rather than one
/// registry entry per resource, because the catalogue can be large and change
/// while the daemon runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpResource {
    /// The resource URI, passed verbatim to `resources/read`.
    pub uri: String,
    /// Human-readable name, when the server supplied one.
    pub name: Option<String>,
    /// Description, when the server supplied one.
    pub description: Option<String>,
    /// The resource MIME type, when the server supplied one.
    pub mime_type: Option<String>,
}

/// The result of a `tools/call`.
#[derive(Debug, Clone, PartialEq)]
pub struct CallToolResult {
    /// The content blocks the tool returned, in server order.
    pub content: Vec<McpContent>,
    /// Whether the server flagged the call as an error (`isError`).
    pub is_error: bool,
    /// The server's `structuredContent`, when present (any JSON value).
    ///
    /// Captured for the programmatic call path; `None` when the server sent no
    /// structured content (an explicit JSON `null` is preserved as
    /// `Some(Value::Null)`).
    pub structured_content: Option<serde_json::Value>,
}

/// A single content block in a [`CallToolResult`].
#[derive(Debug, Clone, PartialEq)]
pub enum McpContent {
    /// Plain text content.
    Text {
        /// The text payload.
        text: String,
    },
    /// Base64-encoded image content.
    Image {
        /// Base64-encoded image data.
        data: String,
        /// The image MIME type.
        mime_type: String,
    },
    /// Base64-encoded audio content.
    Audio {
        /// Base64-encoded audio data.
        data: String,
        /// The audio MIME type.
        mime_type: String,
    },
    /// An embedded resource carried inline with the result.
    Resource {
        /// The resource URI.
        uri: String,
        /// The resource MIME type, if the server supplied one.
        mime_type: Option<String>,
        /// Inline text, present for a text resource (absent for a blob).
        text: Option<String>,
    },
    /// A link to a resource (URI/name) the client may fetch separately.
    ResourceLink {
        /// The resource URI.
        uri: String,
        /// Human-readable resource name, if the server supplied one.
        name: Option<String>,
        /// The resource MIME type, if the server supplied one.
        mime_type: Option<String>,
    },
}

/// Parse [`EMPTY_INPUT_SCHEMA`] into a `Value`; infallible for this literal.
#[must_use]
pub fn empty_input_schema() -> serde_json::Value {
    serde_json::from_str(EMPTY_INPUT_SCHEMA).unwrap_or_else(|_| serde_json::json!({}))
}

/// Normalize a tool's raw `inputSchema` to a valid JSON Schema object.
///
/// Returns `None` when the schema is not an object (arrays, strings, numbers,
/// booleans) or exceeds [`MAX_SCHEMA_BYTES`] — the caller drops such tools while
/// keeping the rest, per the spec's "exclude the tool, keep the others" rule.
/// A `null` schema is treated as absent and replaced with
/// [`EMPTY_INPUT_SCHEMA`].
#[must_use]
pub fn normalize_input_schema(value: serde_json::Value) -> Option<serde_json::Value> {
    match value {
        serde_json::Value::Null => Some(empty_input_schema()),
        serde_json::Value::Object(_) => {
            // `to_string` of a `Value` is infallible in practice; treat the
            // unlikely failure as "cannot bound the schema" and reject it so
            // an unmeasurable schema never reaches the model.
            let size = serde_json::to_string(&value).ok()?.len();
            (size <= MAX_SCHEMA_BYTES).then_some(value)
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_null_becomes_empty_object() {
        let normalized = normalize_input_schema(serde_json::Value::Null).expect("null normalized");
        assert_eq!(normalized["type"], "object");
        assert_eq!(normalized["additionalProperties"], false);
    }

    #[test]
    fn normalize_keeps_object() {
        let schema = serde_json::json!({"type": "object", "properties": {}});
        assert_eq!(normalize_input_schema(schema.clone()), Some(schema));
    }

    #[test]
    fn normalize_rejects_non_object() {
        assert!(normalize_input_schema(serde_json::json!(["not", "a", "schema"])).is_none());
        assert!(normalize_input_schema(serde_json::json!("string")).is_none());
        assert!(normalize_input_schema(serde_json::json!(42)).is_none());
    }

    #[test]
    fn normalize_rejects_oversized() {
        let filler = "x".repeat(MAX_SCHEMA_BYTES + 1);
        let schema = serde_json::json!({ "type": "object", "description": filler });
        assert!(normalize_input_schema(schema).is_none());
    }
}
