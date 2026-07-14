use crate::openai::ChatToolDefinition;
use crate::tools::ToolRegistry;

/// Tool definition for `load_tools`: activates one or more tool groups.
pub(crate) fn load_tools_definition(registry: &ToolRegistry) -> ChatToolDefinition {
    let names = registry.group_names();
    ChatToolDefinition::function(
        "load_tools",
        "Activate one or more tool groups for use in this session. \
         Tools belonging to inactive groups will not be available. \
         The 'core' group is always active and cannot be unloaded.",
        serde_json::json!({
            "type": "object",
            "properties": {
                "groups": {
                    "type": "array",
                    "items": {
                        "type": "string",
                        "enum": names
                    },
                    "description": "Tool groups to activate"
                }
            },
            "required": ["groups"]
        }),
    )
}

/// Tool definition for `unload_tools`: deactivates one or more tool groups.
pub(crate) fn unload_tools_definition(registry: &ToolRegistry) -> ChatToolDefinition {
    let names = registry.group_names();
    ChatToolDefinition::function(
        "unload_tools",
        "Deactivate one or more tool groups. Tools in deactivated \
         groups will no longer be available to call in this session. \
         The 'core' group cannot be unloaded.",
        serde_json::json!({
            "type": "object",
            "properties": {
                "groups": {
                    "type": "array",
                    "items": {
                        "type": "string",
                        "enum": names
                    },
                    "description": "Tool groups to deactivate"
                }
            },
            "required": ["groups"]
        }),
    )
}

/// Execute `load_tools` by adding groups to the session's active set.
pub(crate) fn execute_load_tools(
    active_tool_groups: &mut std::collections::HashSet<String>,
    arguments_json: &str,
) -> String {
    let args: serde_json::Value = match serde_json::from_str(arguments_json) {
        Ok(a) => a,
        Err(e) => return format!("invalid arguments: {e}"),
    };
    let groups: Vec<String> = match args.get("groups").and_then(|v| v.as_array()) {
        Some(arr) => arr
            .iter()
            .filter_map(|v| v.as_str().map(String::from))
            .collect(),
        None => return "missing required argument: groups".to_string(),
    };

    let mut loaded = Vec::new();
    for g in &groups {
        if active_tool_groups.insert(g.clone()) {
            loaded.push(g.clone());
        }
    }

    if loaded.is_empty() {
        "All specified groups were already active.".to_string()
    } else {
        format!("Activated tool groups: {}", humfmt::list(&loaded))
    }
}

/// Tool definition for `set_working_dir`: changes the session's working directory.
pub(crate) fn set_working_dir_definition() -> ChatToolDefinition {
    ChatToolDefinition::function(
        "set_working_dir",
        "Change the working directory for this session. All subsequent file operations, \
         shell commands, and context discovery (AGENTS.md, CLAUDE.md, skills) will \
         resolve relative to this new directory. The change takes effect on the next turn.",
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Absolute path or path relative to the current session working directory"
                }
            },
            "required": ["path"],
            "additionalProperties": false
        }),
    )
}

/// Execute `unload_tools` by removing groups from the session's active set.
/// The "core" group is protected and cannot be removed.
pub(crate) fn execute_unload_tools(
    active_tool_groups: &mut std::collections::HashSet<String>,
    arguments_json: &str,
) -> String {
    let args: serde_json::Value = match serde_json::from_str(arguments_json) {
        Ok(a) => a,
        Err(e) => return format!("invalid arguments: {e}"),
    };
    let groups: Vec<String> = match args.get("groups").and_then(|v| v.as_array()) {
        Some(arr) => arr
            .iter()
            .filter_map(|v| v.as_str().map(String::from))
            .collect(),
        None => return "missing required argument: groups".to_string(),
    };

    let mut unloaded = Vec::new();
    let mut protected = Vec::new();
    for g in &groups {
        if g == "core" {
            protected.push(g.clone());
        } else if active_tool_groups.remove(g) {
            unloaded.push(g.clone());
        }
    }

    let mut parts = Vec::new();
    if !unloaded.is_empty() {
        parts.push(format!(
            "Deactivated tool groups: {}",
            humfmt::list(&unloaded)
        ));
    }
    if !protected.is_empty() {
        parts.push("The 'core' group cannot be unloaded.".to_string());
    }
    if parts.is_empty() {
        parts.push("None of the specified groups were active.".to_string());
    }
    parts.join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::ToolRegistry;

    #[test]
    fn set_working_dir_schema_has_required_path() {
        let _registry = ToolRegistry::new().build();
        let def = set_working_dir_definition();
        assert_eq!(def.function.name, "set_working_dir");
        let params = def
            .function
            .parameters
            .as_object()
            .expect("params should be an object");
        let required = params
            .get("required")
            .and_then(|v| v.as_array())
            .expect("should have required array");
        assert!(
            required.iter().any(|v| v == "path"),
            "path should be in required: {required:?}",
        );
        let props = params
            .get("properties")
            .and_then(|v| v.as_object())
            .expect("schema should have properties");
        assert!(props.contains_key("path"), "path should be in properties",);
        assert!(
            props["path"]["type"] == "string",
            "path should be string type",
        );
    }

    #[test]
    fn test_execute_load_tools_adds_new_groups() {
        let mut active: std::collections::HashSet<String> =
            ["core".into(), "git".into()].into_iter().collect();
        let result = execute_load_tools(&mut active, r#"{"groups": ["shell", "x"]}"#);
        assert_eq!(result, "Activated tool groups: shell and x");
        assert!(active.contains("shell"));
        assert!(active.contains("x"));
        assert!(active.contains("core"));
    }

    #[test]
    fn test_execute_load_tools_skips_already_active() {
        let mut active: std::collections::HashSet<String> =
            ["core".into(), "git".into(), "shell".into()]
                .into_iter()
                .collect();
        let result = execute_load_tools(&mut active, r#"{"groups": ["shell"]}"#);
        assert_eq!(result, "All specified groups were already active.");
    }

    #[test]
    fn test_execute_load_tools_invalid_json() {
        let mut active: std::collections::HashSet<String> = ["core".into()].into_iter().collect();
        let result = execute_load_tools(&mut active, "not json");
        assert!(result.starts_with("invalid arguments:"));
    }

    #[test]
    fn test_execute_load_tools_missing_categories() {
        let mut active: std::collections::HashSet<String> = ["core".into()].into_iter().collect();
        let result = execute_load_tools(&mut active, r#"{"wrong": []}"#);
        assert_eq!(result, "missing required argument: groups");
    }

    #[test]
    fn test_execute_unload_tools_removes_groups() {
        let mut active: std::collections::HashSet<String> =
            ["core".into(), "git".into(), "shell".into(), "x".into()]
                .into_iter()
                .collect();
        let result = execute_unload_tools(&mut active, r#"{"groups": ["x"]}"#);
        assert_eq!(result, "Deactivated tool groups: x");
        assert!(!active.contains("x"));
        assert!(active.contains("core"));
        assert!(active.contains("git"));
    }

    #[test]
    fn test_execute_unload_tools_protects_core() {
        let mut active: std::collections::HashSet<String> =
            ["core".into(), "git".into()].into_iter().collect();
        let result = execute_unload_tools(&mut active, r#"{"groups": ["core"]}"#);
        assert_eq!(result, "The 'core' group cannot be unloaded.");
        assert!(active.contains("core"));
    }

    #[test]
    fn test_execute_unload_tools_skips_inactive() {
        let mut active: std::collections::HashSet<String> = ["core".into()].into_iter().collect();
        let result = execute_unload_tools(&mut active, r#"{"groups": ["x", "vm"]}"#);
        assert_eq!(result, "None of the specified groups were active.");
    }

    #[test]
    fn test_execute_unload_tools_protected_and_unloaded() {
        let mut active: std::collections::HashSet<String> =
            ["core".into(), "shell".into()].into_iter().collect();
        let result = execute_unload_tools(&mut active, r#"{"groups": ["core", "shell"]}"#);
        assert!(result.contains("Deactivated tool groups: shell"));
        assert!(result.contains("The 'core' group cannot be unloaded."));
        assert!(active.contains("core"));
        assert!(!active.contains("shell"));
    }
}
