pub(crate) fn list_sessions_schema() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "properties": {},
        "additionalProperties": false
    })
}

pub(crate) fn list_sessions_definition() -> crate::openai::ChatToolDefinition {
    crate::openai::ChatToolDefinition::function(
        "list_sessions",
        "List all sessions known to the daemon. Returns session ID, title, model, message count, parent session ID, and working directory for each session. Use this to discover what other sessions are doing before reading one with get_session.",
        list_sessions_schema(),
    )
}

pub(crate) fn get_session_schema() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "properties": {
            "session_id": {
                "type": "integer",
                "description": "The ID of the session to read messages from"
            }
        },
        "required": ["session_id"],
        "additionalProperties": false
    })
}

pub(crate) fn get_session_definition() -> crate::openai::ChatToolDefinition {
    crate::openai::ChatToolDefinition::function(
        "get_session",
        "Read the full message history of a session by its ID. Returns all messages (system, user, assistant, tool calls, tool results) with role labels. Use this after list_sessions to inspect the conversation in a specific session.",
        get_session_schema(),
    )
}
