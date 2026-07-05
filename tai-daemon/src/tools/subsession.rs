pub(crate) fn spawn_subsession_schema() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "properties": {
            "prompt": {
                "type": "string",
                "description": "Task description for the sub-session to work on autonomously"
            },
            "title": {
                "type": "string",
                "description": "Optional title for the sub-session"
            },
            "max_turns": {
                "type": "integer",
                "description": "Optional maximum tool-calling iterations for this sub-session. Inherits from parent if not set."
            },
            "categories": {
                "type": "array",
                "items": {
                    "type": "string"
                },
                "description": "Optional tool categories to activate. Inherits from parent session if not set."
            }
        },
        "required": ["prompt"],
        "additionalProperties": false
    })
}

pub(crate) fn spawn_subsession_definition() -> crate::openai::ChatToolDefinition {
    crate::openai::ChatToolDefinition::function(
        "spawn_subsession",
        "Spawn a sub-session to autonomously work on a task. The sub-session inherits the parent session's working directory and runs its own tool-calling loop.",
        spawn_subsession_schema(),
    )
}
