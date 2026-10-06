/// Strip `$schema`, `title`, and `$defs`/`$ref` patterns from a
/// schemars-generated JSON Schema so it is compatible with providers
/// that do not support JSON Schema Draft 2020-12 meta-schema features.
///
/// When `add_additional_properties` is true, inserts `additionalProperties: false`
/// at the root — suitable for tool `parameters` (object schemas), but not for
/// `output_schema` (which may be a non-object type).
fn sanitize_schema(
    mut schema: serde_json::Value,
    add_additional_properties: bool,
) -> serde_json::Value {
    let defs = schema.as_object_mut().and_then(|obj| {
        obj.remove("$schema");
        obj.remove("title");
        obj.remove("$defs")
    });
    if let Some(serde_json::Value::Object(defs_map)) = defs {
        resolve_refs(&mut schema, &defs_map);
    }
    if add_additional_properties && let Some(obj) = schema.as_object_mut() {
        obj.insert("additionalProperties".into(), false.into());
    }
    schema
}

pub(crate) fn sanitize_params_schema(schema: serde_json::Value) -> serde_json::Value {
    let mut s = sanitize_schema(schema, true);
    // Unit type () generates {"type": "null"} from schemars, but OpenAI
    // tool parameters must be a JSON Schema object. Convert to an empty
    // object schema which is the standard "no arguments" representation.
    if s.get("type") == Some(&serde_json::Value::String("null".into())) {
        s = serde_json::json!({
            "type": "object",
            "properties": {},
            "additionalProperties": false
        });
    }
    s
}

pub(crate) fn sanitize_output_schema(schema: serde_json::Value) -> serde_json::Value {
    sanitize_schema(schema, false)
}

/// Recursively walk `value` and replace `{"$ref": "#/$defs/Name"}` with
/// the corresponding definition from `defs`.
fn resolve_refs(value: &mut serde_json::Value, defs: &serde_json::Map<String, serde_json::Value>) {
    match value {
        serde_json::Value::Object(map) => {
            if let Some(ref_path) = map.get("$ref").and_then(|v| v.as_str())
                && let Some(def_key) = ref_path.strip_prefix("#/$defs/")
                && let Some(resolved) = defs.get(def_key)
            {
                let mut resolved = resolved.clone();
                // Preserve any description carried alongside the $ref.
                if let Some(desc) = map.remove("description")
                    && let Some(resolved_obj) = resolved.as_object_mut()
                {
                    resolved_obj.insert("description".into(), desc);
                }
                *value = resolved;
                return;
            }
            for v in map.values_mut() {
                resolve_refs(v, defs);
            }
        }
        serde_json::Value::Array(arr) => {
            for v in arr.iter_mut() {
                resolve_refs(v, defs);
            }
        }
        _ => {}
    }
}

pub(crate) const MAX_FUNCTION_NAME_LEN: usize = 64;

/// Whether `name` is a provider-safe function name.
///
/// A provider accepts a function name of at most [`MAX_FUNCTION_NAME_LEN`]
/// bytes drawn from `[A-Za-z0-9_-]`. One illegal name — e.g. a path-style `/`
/// that a naive `mcp/<slug>/<tool>` join would smuggle in — rejects the entire
/// request, so the request-assembly path drops any definition that fails this
/// predicate rather than sending it.
///
/// Iterates bytes (ASCII classes only), so it needs no regex dependency and no
/// Unicode handling.
pub(crate) fn is_provider_safe_function_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= MAX_FUNCTION_NAME_LEN
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

/// Whether `def` is a valid provider tool definition: a provider-safe function
/// name and an object `parameters` schema.
///
/// The `parameters` field must be a JSON Schema *object* — a bare `null`, a
/// string, or any non-object value is not a legal `parameters` schema and is
/// rejected by the provider together with the rest of the list.
pub(crate) fn is_valid_tool_definition(
    def: &choreo_ai_protocols::openai::ChatToolDefinition,
) -> bool {
    is_provider_safe_function_name(&def.function.name) && def.function.parameters.is_object()
}

/// Retain only the provider-valid definitions in `defs`, returning the names of
/// the dropped ones in encounter order.
///
/// A tool whose name or `parameters` schema a provider would reject invalidates
/// the WHOLE request — and the provider may answer with a bare, bodiless 400
/// that names no offending field, leaving nothing to diagnose. MCP names and
/// schemas are sanitized at registration, so a definition that fails
/// [`is_valid_tool_definition`] here is a regression: dropping just the
/// offending tool keeps the session usable instead of dispatching a request the
/// provider is guaranteed to reject.
pub(crate) fn retain_valid_tool_definitions(
    defs: &mut Vec<choreo_ai_protocols::openai::ChatToolDefinition>,
) -> Vec<String> {
    let mut dropped = Vec::new();
    defs.retain(|def| {
        if is_valid_tool_definition(def) {
            true
        } else {
            dropped.push(def.function.name.clone());
            false
        }
    });
    dropped
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_safe_function_name_accepts_provider_names() {
        for name in [
            "read_file",
            "mcp__filesystem__read_text_file",
            "a-b_c9",
            "x",
        ] {
            assert!(
                is_provider_safe_function_name(name),
                "{name} should be accepted"
            );
        }
        // Exactly at the cap is fine; one byte over is not.
        assert!(is_provider_safe_function_name(
            &"a".repeat(MAX_FUNCTION_NAME_LEN)
        ));
        assert!(!is_provider_safe_function_name(
            &"a".repeat(MAX_FUNCTION_NAME_LEN + 1)
        ));
    }

    #[test]
    fn provider_safe_function_name_rejects_bad_names() {
        // A path-style slash (the regression this guards), the empty name, and
        // any character outside the provider alphabet.
        assert!(!is_provider_safe_function_name(""));
        assert!(!is_provider_safe_function_name("mcp/x/y"));
        assert!(!is_provider_safe_function_name("has space"));
        assert!(!is_provider_safe_function_name("has.dot"));
    }

    #[test]
    fn valid_tool_definition_requires_safe_name_and_object_parameters() {
        let ok = choreo_ai_protocols::openai::ChatToolDefinition::function(
            "read_file",
            "Read a file.",
            serde_json::json!({ "type": "object" }),
        );
        assert!(is_valid_tool_definition(&ok));

        let bad_name = choreo_ai_protocols::openai::ChatToolDefinition::function(
            "bad/name",
            "desc",
            serde_json::json!({ "type": "object" }),
        );
        assert!(!is_valid_tool_definition(&bad_name));

        // A non-object `parameters` (here a bare string) is not a legal schema.
        let bad_params = choreo_ai_protocols::openai::ChatToolDefinition::function(
            "ok_name",
            "desc",
            serde_json::json!("not-an-object"),
        );
        assert!(!is_valid_tool_definition(&bad_params));
    }
    #[test]
    fn retain_valid_tool_definitions_drops_invalid_and_returns_their_names() {
        let valid = choreo_ai_protocols::openai::ChatToolDefinition::function(
            "read_file",
            "Read a file.",
            serde_json::json!({ "type": "object" }),
        );
        let bad_name = choreo_ai_protocols::openai::ChatToolDefinition::function(
            "bad/name",
            "desc",
            serde_json::json!({ "type": "object" }),
        );
        let bad_params = choreo_ai_protocols::openai::ChatToolDefinition::function(
            "ok_name",
            "desc",
            serde_json::json!("not-an-object"),
        );
        let mut defs = vec![valid, bad_name, bad_params];

        let dropped = retain_valid_tool_definitions(&mut defs);

        assert_eq!(dropped, vec!["bad/name".to_string(), "ok_name".to_string()]);
        assert_eq!(defs.len(), 1);
        assert_eq!(defs[0].function.name, "read_file");
    }

    #[test]
    fn retain_valid_tool_definitions_keeps_all_valid() {
        let mut defs = vec![choreo_ai_protocols::openai::ChatToolDefinition::function(
            "read_file",
            "Read a file.",
            serde_json::json!({ "type": "object" }),
        )];
        let dropped = retain_valid_tool_definitions(&mut defs);
        assert_eq!(dropped, Vec::<String>::new());
        assert_eq!(defs.len(), 1);
    }

    #[test]
    fn sanitize_schema_strips_metadata() {
        let input = serde_json::json!({
            "$schema": "https://json-schema.org/draft/2020-12/schema",
            "title": "MySchema",
            "$defs": { "Foo": { "type": "string" } },
            "type": "object"
        });
        let result = super::sanitize_schema(input, false);
        assert!(result.get("$schema").is_none(), "should strip $schema");
        assert!(result.get("title").is_none(), "should strip title");
        assert!(result.get("$defs").is_none(), "should strip $defs");
        assert_eq!(result["type"], "object");
    }

    #[test]
    fn sanitize_schema_inlines_refs() {
        let input = serde_json::json!({
            "$defs": { "Point": { "type": "object", "properties": { "x": {"type": "integer"} } } },
            "type": "object",
            "properties": {
                "location": { "$ref": "#/$defs/Point" }
            }
        });
        let result = super::sanitize_schema(input, false);
        // The $ref should have been replaced by the definition inlined
        let location = &result["properties"]["location"];
        assert!(location.get("$ref").is_none(), "$ref should be resolved");
        assert_eq!(location["type"], "object");
        assert_eq!(location["properties"]["x"]["type"], "integer");
    }

    #[test]
    fn sanitize_schema_preserves_description_across_ref() {
        let input = serde_json::json!({
            "$defs": { "Str": { "type": "string" } },
            "items": { "$ref": "#/$defs/Str", "description": "A string item" }
        });
        let result = super::sanitize_schema(input, false);
        assert_eq!(result["items"]["type"], "string");
        assert_eq!(result["items"]["description"], "A string item");
    }

    #[test]
    fn sanitize_schema_adds_additional_properties() {
        let input = serde_json::json!({ "type": "object", "properties": {} });
        let result = super::sanitize_schema(input, true);
        assert_eq!(result["additionalProperties"], false);
    }

    #[test]
    fn sanitize_schema_skips_additional_properties_when_false() {
        let input = serde_json::json!({ "type": "string" });
        let result = super::sanitize_schema(input, false);
        assert!(result.get("additionalProperties").is_none());
    }

    #[test]
    fn sanitize_schema_passthrough_clean_schema() {
        let input = serde_json::json!({ "type": "integer" });
        let result = super::sanitize_schema(input.clone(), false);
        assert_eq!(result, input);
    }

    #[test]
    fn sanitize_schema_resolves_refs_in_arrays() {
        let input = serde_json::json!({
            "$defs": { "Tag": { "type": "string" } },
            "type": "array",
            "prefixItems": [
                { "$ref": "#/$defs/Tag" },
                { "type": "integer" }
            ]
        });
        let result = super::sanitize_schema(input, false);
        assert!(result["prefixItems"][0].get("$ref").is_none());
        assert_eq!(result["prefixItems"][0]["type"], "string");
        assert_eq!(result["prefixItems"][1]["type"], "integer");
    }

    #[test]
    fn sanitize_params_schema_converts_null_to_object() {
        // Unit type () generates {"type": "null"} from schemars.
        let input = serde_json::json!({ "type": "null" });
        let result = super::sanitize_params_schema(input);
        assert_eq!(result["type"], "object");
        assert_eq!(result["properties"], serde_json::json!({}));
        assert_eq!(result["additionalProperties"], false);
    }

    #[test]
    fn sanitize_params_schema_preserves_normal_schema() {
        let input = serde_json::json!({
            "type": "object",
            "properties": {
                "name": { "type": "string" }
            }
        });
        let result = super::sanitize_params_schema(input);
        assert_eq!(result["type"], "object");
        assert_eq!(result["properties"]["name"]["type"], "string");
        assert_eq!(result["additionalProperties"], false);
    }

    #[test]
    fn sanitize_params_schema_strips_schema_title_defs() {
        let input = serde_json::json!({
            "$schema": "https://json-schema.org/draft/2020-12/schema",
            "title": "Args",
            "$defs": { "X": { "type": "string" } },
            "type": "object"
        });
        let result = super::sanitize_params_schema(input);
        assert!(result.get("$schema").is_none());
        assert!(result.get("title").is_none());
        assert!(result.get("$defs").is_none());
    }

    #[test]
    fn sanitize_output_schema_no_additional_properties() {
        let input = serde_json::json!({ "type": "string" });
        let result = super::sanitize_output_schema(input);
        assert_eq!(result["type"], "string");
        assert!(result.get("additionalProperties").is_none());
    }

    #[test]
    fn sanitize_output_schema_strips_metadata() {
        let input = serde_json::json!({
            "$schema": "https://json-schema.org/draft/2020-12/schema",
            "title": "Return",
            "type": "integer"
        });
        let result = super::sanitize_output_schema(input);
        assert!(result.get("$schema").is_none());
        assert!(result.get("title").is_none());
    }

    #[test]
    fn resolve_refs_basic() {
        let mut value = serde_json::json!({ "$ref": "#/$defs/MyType" });
        let defs = [("MyType".to_string(), serde_json::json!({"type": "string"}))]
            .into_iter()
            .collect();
        super::resolve_refs(&mut value, &defs);
        assert_eq!(value, serde_json::json!({"type": "string"}));
    }

    #[test]
    fn resolve_refs_no_match_unchanged() {
        let original = serde_json::json!({ "$ref": "#/$defs/Unknown" });
        let mut value = original.clone();
        let defs = serde_json::Map::new();
        super::resolve_refs(&mut value, &defs);
        // Unknown refs are left as-is (schemars shouldn't produce these).
        assert_eq!(value, original);
    }

    #[test]
    fn resolve_refs_no_ref_unchanged() {
        let original = serde_json::json!({ "type": "object", "properties": {} });
        let mut value = original.clone();
        let defs = serde_json::Map::new();
        super::resolve_refs(&mut value, &defs);
        assert_eq!(value, original);
    }

    #[test]
    fn resolve_refs_nested_skipped() {
        // Known limitation: resolve_refs does NOT recursively resolve
        // $refs inside the resolved definition. If $defs/B points to
        // $defs/A, only the first level is resolved.
        let mut value = serde_json::json!({ "$ref": "#/$defs/B" });
        let mut defs = serde_json::Map::new();
        defs.insert("A".into(), serde_json::json!({"type": "string"}));
        defs.insert("B".into(), serde_json::json!({"$ref": "#/$defs/A"}));
        super::resolve_refs(&mut value, &defs);
        // B resolves to {"$ref": "#/$defs/A"} — nested ref is NOT resolved.
        assert_eq!(value, serde_json::json!({"$ref": "#/$defs/A"}));
    }
}
