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

#[cfg(test)]
mod tests {
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
