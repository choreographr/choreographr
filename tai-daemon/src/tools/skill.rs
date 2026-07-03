use crate::context;
use crate::sessions::{SessionState, append_message_and_persist};
use crate::tools::ToolExecutionOutput;
use crate::tools::ToolResult;
use std::sync::Arc;
use tai_proto::SessionMessage;
use tokio::sync::Mutex;

pub(crate) async fn execute_load_skill(
    session: &Arc<Mutex<SessionState>>,
    session_id: u64,
    db: &Arc<redb::Database>,
    cwd: Option<&std::path::Path>,
    arguments_json: &str,
) -> ToolExecutionOutput {
    let name: String = match extract_skill_name(arguments_json) {
        Ok(n) => n,
        Err(e) => {
            return ToolExecutionOutput {
                result: ToolResult {
                    content: e,
                    is_error: true,
                },
                image: None,
            }
        }
    };

    let effective_cwd = cwd.unwrap_or_else(|| std::path::Path::new("."));

    let body = match context::load_skill_body(&name, effective_cwd) {
        Some(b) => b,
        None => {
            return ToolExecutionOutput {
                result: ToolResult {
                    content: format!("skill not found: {name}"),
                    is_error: true,
                },
                image: None,
            }
        }
    };

    let skill_message = format!(
        "The following skill instructions are now active:\n\n<skill name=\"{name}\">\n{body}\n</skill>"
    );

    append_message_and_persist(
        session,
        db,
        session_id,
        SessionMessage::SystemText {
            content: skill_message,
        },
    )
    .await;

    ToolExecutionOutput {
        result: ToolResult {
            content: format!("Loaded skill: {name}"),
            is_error: false,
        },
        image: None,
    }
}

fn extract_skill_name(arguments_json: &str) -> Result<String, String> {
    let v: serde_json::Value =
        serde_json::from_str(arguments_json).map_err(|e| format!("invalid json: {e}"))?;
    v.get("name")
        .and_then(|n| n.as_str())
        .map(|s| s.to_string())
        .ok_or_else(|| "missing required parameter: name".to_string())
}

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
    use super::*;

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
