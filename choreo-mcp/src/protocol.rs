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

/// Upper bound on the nesting depth of a tool's schema.
///
/// JSON Schema permits unbounded nesting, so a malicious schema could recurse
/// arbitrarily deep — costly to validate and a stack-depth hazard for any
/// consumer that walks it. The depth cap (counting nested objects/arrays, where
/// a top-level container is depth 1) rejects such a schema before it reaches a
/// validator. The bound is far deeper than any real tool schema needs.
pub const MAX_SCHEMA_DEPTH: usize = 32;

/// Upper bound on how many tools one server's catalogue may contribute.
///
/// A server can advertise an unbounded number of tools; forwarding them all
/// would flood the model's tool array and the daemon's registry. The cap keeps
/// the first [`MAX_TOOLS_PER_SERVER`] (the spec recommends servers list
/// deterministically, so the kept prefix is stable) and reports the rest as
/// dropped.
pub const MAX_TOOLS_PER_SERVER: usize = 1024;

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

/// Which advertised list changed on the server.
///
/// A `subscriptions/listen` stream opts in to one or more of these categories;
/// each event the client observes is relabelled with this kind so the daemon
/// knows what to refresh (its own tool catalogue, in either case).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum McpListKind {
    /// `notifications/tools/list_changed`: the server's tool set changed.
    Tools,
    /// `notifications/resources/list_changed`: the server's resource set
    /// changed.
    Resources,
}

/// A list-changed event observed on a server's `subscriptions/listen` stream.
///
/// The client opens one subscription per server (when the negotiated protocol
/// era and the server's advertised capabilities allow it) and forwards each
/// event to the daemon, which rebuilds its tool catalogue so a live server can
/// add or withdraw tools without a daemon restart. The slug names the
/// originating server, since every server's subscription feeds one shared
/// channel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpListChange {
    /// The server slug whose list changed.
    pub slug: String,
    /// Which list changed.
    pub kind: McpListKind,
}

/// Parse [`EMPTY_INPUT_SCHEMA`] into a `Value`; infallible for this literal.
#[must_use]
pub fn empty_input_schema() -> serde_json::Value {
    serde_json::from_str(EMPTY_INPUT_SCHEMA).unwrap_or_else(|_| serde_json::json!({}))
}

/// Normalize a tool's raw `inputSchema` to a valid JSON Schema object.
///
/// Returns `None` when the schema is not an object (arrays, strings, numbers,
/// booleans) or exceeds [`MAX_SCHEMA_BYTES`] / [`MAX_SCHEMA_DEPTH`] — the caller
/// drops such tools while keeping the rest, per the spec's "exclude the tool,
/// keep the others" rule. A `null` schema is treated as absent and replaced
/// with [`EMPTY_INPUT_SCHEMA`].
#[must_use]
pub fn normalize_input_schema(value: serde_json::Value) -> Option<serde_json::Value> {
    match value {
        serde_json::Value::Null => Some(empty_input_schema()),
        serde_json::Value::Object(_) if schema_within_bounds(&value) => Some(value),
        _ => None,
    }
}

/// Normalize a tool's optional `outputSchema`, dropping it when it is not a
/// bounded object.
///
/// The output schema is advisory (a caller may validate a tool's
/// `structuredContent` against it), so an out-of-bounds schema is dropped while
/// the tool itself is kept — unlike an input schema, whose absence would change
/// how the tool is invoked.
#[must_use]
pub fn normalize_output_schema(value: serde_json::Value) -> Option<serde_json::Value> {
    match value {
        serde_json::Value::Object(_) if schema_within_bounds(&value) => Some(value),
        _ => None,
    }
}

/// Whether `schema` respects the byte and depth bounds (and is measurable).
fn schema_within_bounds(schema: &serde_json::Value) -> bool {
    // Depth first: the walk is iterative, so an over-deep schema is rejected
    // before reaching the recursive serializer below.
    if json_depth(schema) > MAX_SCHEMA_DEPTH {
        return false;
    }
    // `to_string` of a `Value` is infallible in practice; the unlikely failure is
    // treated as "cannot bound the schema", rejecting it so an unmeasurable
    // schema never reaches a validator.
    let Ok(size) = serde_json::to_string(schema).map(|s| s.len()) else {
        return false;
    };
    size <= MAX_SCHEMA_BYTES
}

/// The nesting depth of a JSON value: a scalar is depth 0, and each object or
/// array level adds one, so a top-level object is depth 1.
///
/// Iterative by way of an explicit stack so a hostile deeply-nested schema
/// cannot overflow the native stack while being measured (the recursion this
/// replaces would itself be the hazard the depth cap exists to prevent).
#[must_use]
pub fn json_depth(value: &serde_json::Value) -> usize {
    // Each stack entry is `(value, depth-at-this-value)`; children are pushed
    // with `depth + 1` and the running max is tracked as they pop.
    let mut max = 0usize;
    let mut stack: Vec<(&serde_json::Value, usize)> = vec![(value, 0)];
    while let Some((value, depth)) = stack.pop() {
        max = max.max(depth);
        match value {
            serde_json::Value::Object(map) => {
                stack.extend(map.values().map(|child| (child, depth + 1)));
            }
            serde_json::Value::Array(items) => {
                stack.extend(items.iter().map(|child| (child, depth + 1)));
            }
            _ => {}
        }
    }
    max
}

/// Truncate a tool list to [`MAX_TOOLS_PER_SERVER`], returning the kept tools
/// and how many were dropped.
///
/// Keeps the leading prefix (servers SHOULD list deterministically), so the
/// retained set is stable across calls.
#[must_use]
pub fn cap_tools(tools: Vec<McpTool>) -> (Vec<McpTool>, usize) {
    if tools.len() <= MAX_TOOLS_PER_SERVER {
        return (tools, 0);
    }
    let dropped = tools.len() - MAX_TOOLS_PER_SERVER;
    let kept = tools.into_iter().take(MAX_TOOLS_PER_SERVER).collect();
    (kept, dropped)
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

    #[test]
    fn json_depth_counts_container_nesting() {
        assert_eq!(json_depth(&serde_json::json!(42)), 0);
        assert_eq!(json_depth(&serde_json::json!({"a": 1})), 1);
        assert_eq!(json_depth(&serde_json::json!({"a": {"b": {"c": 1}}})), 3);
        // Arrays count too, and a mixed object/array nest accumulates.
        assert_eq!(json_depth(&serde_json::json!([[[1]]])), 3);
        assert_eq!(json_depth(&serde_json::json!({"a": [{"b": 1}]})), 3);
    }

    #[test]
    fn normalize_rejects_over_deep_input_schema() {
        // Build an object nested one level beyond the cap.
        let mut schema = serde_json::json!(1);
        for _ in 0..=MAX_SCHEMA_DEPTH {
            schema = serde_json::json!({"n": schema});
        }
        // A shallow schema of the same shape passes, so the rejection is the
        // depth, not the shape.
        let shallow = serde_json::json!({"type": "object"});
        assert!(normalize_input_schema(shallow).is_some());
        assert!(normalize_input_schema(schema).is_none());
    }

    #[test]
    fn normalize_output_schema_bounds_depth_and_shape() {
        let ok = serde_json::json!({"type": "object", "properties": {}});
        assert_eq!(normalize_output_schema(ok.clone()), Some(ok));
        // A non-object output schema is dropped.
        assert!(normalize_output_schema(serde_json::json!(["nope"])).is_none());
        let mut deep = serde_json::json!(1);
        for _ in 0..=MAX_SCHEMA_DEPTH {
            deep = serde_json::json!({"n": deep});
        }
        assert!(normalize_output_schema(deep).is_none());
    }

    #[test]
    fn json_depth_measures_deep_nesting_without_stack_overflow() {
        // `json_depth` is iterative, so a value far deeper than any real schema
        // is measured rather than recursing. (The value is built directly — a
        // serde_json parse would stop at its own recursion limit.)
        let mut deep = serde_json::json!(0);
        for _ in 0..2_000 {
            deep = serde_json::Value::Array(vec![deep]);
        }
        assert_eq!(json_depth(&deep), 2_000);
        // Such a value is rejected by the schema bound.
        assert!(normalize_input_schema(serde_json::json!({"type": "object"})).is_some());
        assert!(
            normalize_input_schema({
                let mut s = serde_json::json!(0);
                for _ in 0..=MAX_SCHEMA_DEPTH {
                    s = serde_json::Value::Array(vec![s]);
                }
                s
            })
            .is_none()
        );
    }

    #[test]
    fn cap_tools_keeps_prefix_and_reports_dropped() {
        let make = |n: usize| McpTool {
            name: format!("t{n}"),
            description: None,
            input_schema: serde_json::json!({"type": "object"}),
            output_schema: None,
        };
        // Under the cap: returned unchanged, nothing dropped.
        let (kept, dropped) = cap_tools((0..10).map(make).collect());
        assert_eq!(kept.len(), 10);
        assert_eq!(dropped, 0);
        // Over the cap: exactly MAX kept (the leading prefix), the rest dropped.
        let total = MAX_TOOLS_PER_SERVER + 7;
        let (kept, dropped) = cap_tools((0..total).map(make).collect());
        assert_eq!(kept.len(), MAX_TOOLS_PER_SERVER);
        assert_eq!(dropped, 7);
        assert_eq!(kept.first().map(|t| t.name.as_str()), Some("t0"));
        assert_eq!(
            kept.last().map(|t| t.name.as_str()),
            Some(format!("t{}", MAX_TOOLS_PER_SERVER - 1).as_str())
        );
    }
}
