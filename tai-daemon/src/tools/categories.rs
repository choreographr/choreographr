use crate::openai::ChatToolDefinition;
use crate::tools::ToolRegistry;

/// Tool definition for `load_tools`: activates one or more tool categories.
pub(crate) fn load_tools_definition(registry: &ToolRegistry) -> ChatToolDefinition {
    let names = registry.category_names();
    ChatToolDefinition::function(
        "load_tools",
        "Activate one or more tool categories for use in this session. \
         Tools belonging to inactive categories will not be available. \
         The 'core' category is always active and cannot be unloaded.",
        serde_json::json!({
            "type": "object",
            "properties": {
                "categories": {
                    "type": "array",
                    "items": {
                        "type": "string",
                        "enum": names
                    },
                    "description": "Tool categories to activate"
                }
            },
            "required": ["categories"]
        }),
    )
}

/// Tool definition for `unload_tools`: deactivates one or more tool categories.
pub(crate) fn unload_tools_definition(registry: &ToolRegistry) -> ChatToolDefinition {
    let names = registry.category_names();
    ChatToolDefinition::function(
        "unload_tools",
        "Deactivate one or more tool categories. Tools in deactivated \
         categories will no longer be available to call in this session. \
         The 'core' category cannot be unloaded.",
        serde_json::json!({
            "type": "object",
            "properties": {
                "categories": {
                    "type": "array",
                    "items": {
                        "type": "string",
                        "enum": names
                    },
                    "description": "Tool categories to deactivate"
                }
            },
            "required": ["categories"]
        }),
    )
}

/// Execute `load_tools` by adding categories to the session's active set.
pub(crate) fn execute_load_tools(
    active_categories: &mut std::collections::HashSet<String>,
    arguments_json: &str,
) -> String {
    let args: serde_json::Value = match serde_json::from_str(arguments_json) {
        Ok(a) => a,
        Err(e) => return format!("invalid arguments: {e}"),
    };
    let categories: Vec<String> = match args.get("categories").and_then(|v| v.as_array()) {
        Some(arr) => arr.iter().filter_map(|v| v.as_str().map(String::from)).collect(),
        None => return "missing required argument: categories".to_string(),
    };

    let mut loaded = Vec::new();
    for cat in &categories {
        if active_categories.insert(cat.clone()) {
            loaded.push(cat.clone());
        }
    }

    if loaded.is_empty() {
        "All specified categories were already active.".to_string()
    } else {
        format!("Activated tool categories: {}", loaded.join(", "))
    }
}

/// Execute `unload_tools` by removing categories from the session's active set.
/// The "core" category is protected and cannot be removed.
pub(crate) fn execute_unload_tools(
    active_categories: &mut std::collections::HashSet<String>,
    arguments_json: &str,
) -> String {
    let args: serde_json::Value = match serde_json::from_str(arguments_json) {
        Ok(a) => a,
        Err(e) => return format!("invalid arguments: {e}"),
    };
    let categories: Vec<String> = match args.get("categories").and_then(|v| v.as_array()) {
        Some(arr) => arr.iter().filter_map(|v| v.as_str().map(String::from)).collect(),
        None => return "missing required argument: categories".to_string(),
    };

    let mut unloaded = Vec::new();
    let mut protected = Vec::new();
    for cat in &categories {
        if cat == "core" {
            protected.push(cat.clone());
        } else if active_categories.remove(cat) {
            unloaded.push(cat.clone());
        }
    }

    let mut parts = Vec::new();
    if !unloaded.is_empty() {
        parts.push(format!("Deactivated tool categories: {}", unloaded.join(", ")));
    }
    if !protected.is_empty() {
        parts.push("The 'core' category cannot be unloaded.".to_string());
    }
    if parts.is_empty() {
        parts.push("None of the specified categories were active.".to_string());
    }
    parts.join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_execute_load_tools_adds_new_categories() {
        let mut active: std::collections::HashSet<String> =
            ["core".into(), "git".into()].into_iter().collect();
        let result = execute_load_tools(&mut active, r#"{"categories": ["shell", "x"]}"#);
        assert_eq!(result, "Activated tool categories: shell, x");
        assert!(active.contains("shell"));
        assert!(active.contains("x"));
        assert!(active.contains("core"));
    }

    #[test]
    fn test_execute_load_tools_skips_already_active() {
        let mut active: std::collections::HashSet<String> =
            ["core".into(), "git".into(), "shell".into()].into_iter().collect();
        let result = execute_load_tools(&mut active, r#"{"categories": ["shell"]}"#);
        assert_eq!(result, "All specified categories were already active.");
    }

    #[test]
    fn test_execute_load_tools_invalid_json() {
        let mut active: std::collections::HashSet<String> =
            ["core".into()].into_iter().collect();
        let result = execute_load_tools(&mut active, "not json");
        assert!(result.starts_with("invalid arguments:"));
    }

    #[test]
    fn test_execute_load_tools_missing_categories() {
        let mut active: std::collections::HashSet<String> =
            ["core".into()].into_iter().collect();
        let result = execute_load_tools(&mut active, r#"{"wrong": []}"#);
        assert_eq!(result, "missing required argument: categories");
    }

    #[test]
    fn test_execute_unload_tools_removes_categories() {
        let mut active: std::collections::HashSet<String> =
            ["core".into(), "git".into(), "shell".into(), "x".into()].into_iter().collect();
        let result = execute_unload_tools(&mut active, r#"{"categories": ["x"]}"#);
        assert_eq!(result, "Deactivated tool categories: x");
        assert!(!active.contains("x"));
        assert!(active.contains("core"));
        assert!(active.contains("git"));
    }

    #[test]
    fn test_execute_unload_tools_protects_core() {
        let mut active: std::collections::HashSet<String> =
            ["core".into(), "git".into()].into_iter().collect();
        let result = execute_unload_tools(&mut active, r#"{"categories": ["core"]}"#);
        assert_eq!(result, "The 'core' category cannot be unloaded.");
        assert!(active.contains("core"));
    }

    #[test]
    fn test_execute_unload_tools_skips_inactive() {
        let mut active: std::collections::HashSet<String> =
            ["core".into()].into_iter().collect();
        let result = execute_unload_tools(&mut active, r#"{"categories": ["x", "vm"]}"#);
        assert_eq!(result, "None of the specified categories were active.");
    }

    #[test]
    fn test_execute_unload_tools_protected_and_unloaded() {
        let mut active: std::collections::HashSet<String> =
            ["core".into(), "shell".into()].into_iter().collect();
        let result = execute_unload_tools(&mut active, r#"{"categories": ["core", "shell"]}"#);
        assert!(result.contains("Deactivated tool categories: shell"));
        assert!(result.contains("The 'core' category cannot be unloaded."));
        assert!(active.contains("core"));
        assert!(!active.contains("shell"));
    }
}
