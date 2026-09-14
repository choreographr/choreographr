use crate::context;
use crate::tools::context::ToolContext;
use crate::tools::{Tool, ToolExecError};
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
    skills: Option<&[context::SkillMeta]>,
) -> Result<String, ToolExecError> {
    // Resolve against the session's cached snapshot when provided (the
    // production path — one resolution shared with `persist_loaded_skill`);
    // otherwise fall back to a fresh ambient walk (direct/unit-test calls).
    let body = match skills {
        Some(skills) => context::load_skill_body_from(skills, &args.name),
        None => context::load_skill_body(&args.name, working_dir),
    }
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

impl Tool for LoadSkill {
    type Args = LoadSkillArgs;
    type Return = String;
    type Error = ToolExecError;

    fn name(&self) -> &'static str {
        "load_skill"
    }

    fn group(&self) -> &'static str {
        "core"
    }

    fn description(&self) -> &'static str {
        "Load the full instructions for a skill by name. Use this when a task matches one of the available skill descriptions."
    }

    fn describe_invocation(&self, args: &Self::Args) -> String {
        describe_load_skill_invocation(args)
    }

    fn return_string(ret: &Self::Return) -> String {
        ret.clone()
    }

    fn execute(
        &self,
        args: Self::Args,
        _x_credentials: Option<&crate::tools::ServiceCredential>,
        working_dir: Option<&std::path::Path>,
        ctx: Option<&ToolContext>,
    ) -> Result<Self::Return, Self::Error> {
        // Prefer the session's cached skill snapshot (the SAME list the
        // system-prompt listing was built from), so the body returned here can
        // never diverge from the body `persist_loaded_skill` reads. When no
        // context/snapshot is available (a direct unit-test call), pass `None`
        // so the free function falls back to a fresh ambient walk.
        let skills = ctx
            .and_then(|c| c.discovered_skills.as_deref())
            .map(|v| v.as_slice());
        execute_load_skill(&args, working_dir, skills)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // -- load_skill -----------------------------------------------------------

    #[test]
    fn execute_load_skill_not_found() {
        // Hermetic: a temp-dir working dir plus an EMPTY snapshot means the
        // lookup never consults the developer's ambient ~/.agents/skills.
        let dir = tempfile::tempdir().unwrap();
        let result = execute_load_skill(
            &LoadSkillArgs {
                name: "nonexistent".into(),
            },
            Some(dir.path()),
            Some(&[]),
        );
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("skill not found"));
    }

    #[test]
    fn execute_load_skill_none_working_dir_not_found() {
        // A dir-less session can still load global skills, but an obviously
        // absent name must error rather than panic. The empty snapshot means
        // there is no ambient lookup at all — the test is hermetic and does
        // NOT depend on the developer's real global skills.
        let result = execute_load_skill(
            &LoadSkillArgs {
                name: "definitely-no-such-skill-xyz".into(),
            },
            None,
            Some(&[]),
        );
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("skill not found"));
    }

    #[test]
    fn execute_load_skill_found() {
        // Hermetic: build the skill snapshot directly from a temp-dir SKILL.md
        // so discovery never touches ambient state.
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
        let skill_md = skill_dir.join("SKILL.md");
        std::fs::write(&skill_md, skill_content).unwrap();
        let skills = [context::SkillMeta {
            name: "test-skill".into(),
            description: "A test skill".into(),
            path: skill_md,
        }];
        let result = execute_load_skill(
            &LoadSkillArgs {
                name: "test-skill".into(),
            },
            Some(dir.path()),
            Some(&skills),
        );
        assert!(result.is_ok());
        let msg = result.unwrap();
        assert!(msg.contains("Loaded skill: test-skill"));
        assert!(msg.contains("Hello, this is the skill body."));
    }
}
