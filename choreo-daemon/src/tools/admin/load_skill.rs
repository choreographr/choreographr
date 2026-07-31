use crate::context;
use crate::tools::ToolExecError;
use schemars::JsonSchema;
use serde::Deserialize;
use std::path::Path;

// ── Args structs ────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize, JsonSchema)]
pub(crate) struct LoadSkillArgs {
    /// Name of the skill to load
    name: String,
}

// ── load_skill ─────────────────────────────────────────────────────────────

fn execute_load_skill(
    args: &LoadSkillArgs,
    working_dir: Option<&Path>,
) -> Result<String, ToolExecError> {
    let effective_working_dir = working_dir.unwrap_or_else(|| Path::new("."));
    let body = context::load_skill_body(&args.name, effective_working_dir)
        .ok_or_else(|| ToolExecError(format!("skill not found: {}", args.name)))?;
    let skill_message = format!(
        "The following skill instructions are now active:\n\n<skill name=\"{name}\">\n{body}\n</skill>",
        name = args.name,
    );
    Ok(format!(
        "Loaded skill: {}\n\n---\n{}",
        args.name, skill_message
    ))
}

pub fn describe_load_skill_invocation(args: &LoadSkillArgs) -> String {
    format!("Loading skill `{}`.", args.name)
}

pub(crate) struct LoadSkill;

define_tool!(
    LoadSkill,
    "load_skill",
    "Load the full instructions for a skill by name. Use this when a task matches one of the available skill descriptions.",
    LoadSkillArgs,
    execute_load_skill,
    "core",
    describe_load_skill_invocation
);

#[cfg(test)]
mod tests {
    use super::*;

    // -- load_skill -----------------------------------------------------------

    #[test]
    fn execute_load_skill_not_found() {
        let dir = tempfile::tempdir().unwrap();
        let result = execute_load_skill(
            &LoadSkillArgs {
                name: "nonexistent".into(),
            },
            Some(dir.path()),
        );
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("skill not found"));
    }

    #[test]
    fn execute_load_skill_found() {
        let dir = tempfile::tempdir().unwrap();
        let skill_dir = dir.path().join(".agents/skills/test-skill");
        std::fs::create_dir_all(&skill_dir).unwrap();
        let skill_content = "\
---
name: test-skill
description: A test skill
---
Hello, this is the skill body.
---
";
        std::fs::write(skill_dir.join("SKILL.md"), skill_content).unwrap();
        let result = execute_load_skill(
            &LoadSkillArgs {
                name: "test-skill".into(),
            },
            Some(dir.path()),
        );
        assert!(result.is_ok());
        let msg = result.unwrap();
        assert!(msg.contains("Loaded skill: test-skill"));
        assert!(msg.contains("Hello, this is the skill body."));
    }
}
