use crate::tools::{Tool, ToolError};
use schemars::JsonSchema;
use serde::Deserialize;
use std::path::Path;
use tai_keystore::ServiceCredential;

#[derive(Debug, Deserialize, JsonSchema)]
pub struct SpawnSubsessionArgs {
    /// Task description for the sub-session to work on autonomously
    pub prompt: String,
    /// Optional title for the sub-session
    pub title: Option<String>,
    /// Optional maximum tool-calling iterations for this sub-session
    pub max_turns: Option<u32>,
    /// Optional tool categories to activate. Inherits from parent session if not set.
    pub categories: Option<Vec<String>>,
}

pub struct SpawnSubsession;

impl Tool for SpawnSubsession {
    type Args = SpawnSubsessionArgs;
    type Return = String;

    fn name(&self) -> &'static str {
        "spawn_subsession"
    }

    fn group(&self) -> &'static str {
        "core"
    }

    fn description(&self) -> &'static str {
        "Spawn a sub-session to autonomously work on a task. The sub-session inherits the parent session's working directory and runs its own tool-calling loop."
    }

    fn execute(
        &self,
        _args: Self::Args,
        _x_credentials: Option<&ServiceCredential>,
        _working_dir: Option<&Path>,
        _ctx: Option<&crate::tools::context::ToolContext>,
    ) -> Result<String, ToolError> {
        Err(ToolError::Other(
            "spawn_subsession is not yet implemented in the turn-based refactor".into(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spawn_subsession_schema_has_required_prompt() {
        let schema = SpawnSubsession.schema();
        let obj = schema.as_object().expect("schema should be an object");
        let required = obj
            .get("required")
            .and_then(|v| v.as_array())
            .expect("schema should have required array");
        assert!(
            required.iter().any(|v| v == "prompt"),
            "prompt should be in required: {required:?}",
        );
        let props = obj
            .get("properties")
            .and_then(|v| v.as_object())
            .expect("schema should have properties");
        assert!(
            props.contains_key("prompt"),
            "prompt should be in properties",
        );
        assert!(
            props["prompt"]["type"] == "string",
            "prompt should be string type",
        );
    }

    #[test]
    fn spawn_subsession_args_deserializes() {
        let json = r#"{"prompt": "do something"}"#;
        let args: SpawnSubsessionArgs = serde_json::from_str(json).expect("should deserialize");
        assert_eq!(args.prompt, "do something");
        assert!(args.title.is_none());
        assert!(args.max_turns.is_none());
        assert!(args.categories.is_none());
    }

    #[test]
    fn spawn_subsession_args_all_fields() {
        let json = r#"{
            "prompt": "work",
            "title": "my sub",
            "max_turns": 10,
            "categories": ["core", "shell"]
        }"#;
        let args: SpawnSubsessionArgs =
            serde_json::from_str(json).expect("should deserialize full payload");
        assert_eq!(args.prompt, "work");
        assert_eq!(args.title.as_deref(), Some("my sub"));
        assert_eq!(args.max_turns, Some(10));
        assert_eq!(args.categories, Some(vec!["core".into(), "shell".into()]));
    }

    #[test]
    fn spawn_subsession_args_missing_prompt_fails() {
        let json = r#"{"title": "no prompt"}"#;
        let result: Result<SpawnSubsessionArgs, _> = serde_json::from_str(json);
        assert!(result.is_err(), "missing prompt should fail: {result:?}",);
    }
}
