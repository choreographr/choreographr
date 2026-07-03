pub(crate) fn load_skill_schema() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "properties": {
            "name": {
                "type": "string",
                "description": "Name of the skill to load (must match an available skill's name)"
            }
        },
        "required": ["name"],
        "additionalProperties": false
    })
}

pub(crate) fn load_skill_definition() -> crate::openai::ChatToolDefinition {
    crate::openai::ChatToolDefinition::function(
        "load_skill",
        "Load the full instructions for a skill by name. Use this when a task matches one of the available skill descriptions.",
        load_skill_schema(),
    )
}

#[cfg(test)]
mod tests {
    fn extract_skill_name(arguments_json: &str) -> Result<String, String> {
        let v: serde_json::Value =
            serde_json::from_str(arguments_json).map_err(|e| format!("invalid json: {e}"))?;
        v.get("name")
            .and_then(|n| n.as_str())
            .map(|s| s.to_string())
            .ok_or_else(|| "missing required parameter: name".to_string())
    }

    #[test]
    fn test_extract_skill_name_valid() {
        let name = extract_skill_name(r#"{"name": "my-skill"}"#).unwrap();
        assert_eq!(name, "my-skill");
    }

    #[test]
    fn test_extract_skill_name_missing() {
        assert!(extract_skill_name(r#"{}"#).is_err());
    }

    #[test]
    fn test_extract_skill_name_invalid_json() {
        assert!(extract_skill_name("not json").is_err());
    }
}
