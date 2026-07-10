use crate::context;
use crate::daemon::DaemonCommand;
use crate::tools::context::ToolContext;
use crate::tools::{ToolError, truncate_tool_output};
use serde::Deserialize;
use std::path::Path;

// ── Args structs ────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub(crate) struct ListSessionsArgs {}

#[derive(Debug, Deserialize)]
pub(crate) struct GetSessionArgs {
    session_id: u64,
}

#[derive(Debug, Deserialize)]
pub(crate) struct LoadSkillArgs {
    name: String,
}

// ── list_sessions ──────────────────────────────────────────────────────────

fn execute_list_sessions(
    _args: &ListSessionsArgs,
    _cwd: Option<&Path>,
    ctx: Option<&ToolContext>,
) -> Result<String, ToolError> {
    let ctx = ctx.ok_or_else(|| ToolError::Other("no session context".into()))?;
    let (reply, rx) = std::sync::mpsc::channel();
    ctx.daemon_tx
        .send(DaemonCommand::ListSessions { reply })
        .map_err(|e| ToolError::Other(format!("daemon communication failed: {e}")))?;
    let sessions = rx
        .recv()
        .map_err(|_| ToolError::Other("failed to list sessions".into()))?;
    if sessions.is_empty() {
        return Ok("No sessions found.".to_string());
    }
    let lines: Vec<String> = sessions
        .iter()
        .map(|s| {
            let title = s.title.as_deref().unwrap_or("(untitled)");
            let model = s.selected_model.as_deref().unwrap_or("(no model)");
            let parent = s
                .parent_session_id
                .map(|id| id.to_string())
                .unwrap_or_else(|| "none".to_string());
            let cwd = s.cwd.as_deref().unwrap_or("(none)");
            format!(
                "Session {}: \"{}\" | model: {} | messages: {} | parent: {} | cwd: {}",
                s.session_id, title, model, s.message_count, parent, cwd
            )
        })
        .collect();
    Ok(truncate_tool_output(&lines.join("\n")))
}

pub(crate) struct ListSessions;

crate::define_tool!(
    ListSessions,
    "list_sessions",
    "List all sessions known to the daemon. Returns session ID, title, model, message count, parent session ID, and working directory for each session.",
    ListSessionsArgs,
    String,
    execute_list_sessions,
    serde_json::json!({
        "type": "object",
        "properties": {},
        "additionalProperties": false
    }),
    "core",
    use_context
);

// ── get_session ────────────────────────────────────────────────────────────

fn execute_get_session(
    args: &GetSessionArgs,
    _cwd: Option<&Path>,
    ctx: Option<&ToolContext>,
) -> Result<String, ToolError> {
    let ctx = ctx.ok_or_else(|| ToolError::Other("no session context".into()))?;
    let (reply, rx) = std::sync::mpsc::channel();
    ctx.daemon_tx
        .send(DaemonCommand::GetSession {
            session_id: args.session_id,
            reply,
        })
        .map_err(|e| ToolError::Other(format!("daemon communication failed: {e}")))?;
    match rx
        .recv()
        .map_err(|_| ToolError::Other("failed to get session".into()))?
    {
        Some(summary) => Ok(format!(
            "Session {} ({}) has {} messages.",
            args.session_id,
            summary.title.as_deref().unwrap_or("untitled"),
            summary.message_count
        )),
        None => Err(ToolError::Other(format!(
            "Session {} not found.",
            args.session_id
        ))),
    }
}

pub(crate) struct GetSession;

crate::define_tool!(
    GetSession,
    "get_session",
    "Read the full message history of a session by its ID. Returns all messages (system, user, assistant, tool calls, tool results) with role labels.",
    GetSessionArgs,
    String,
    execute_get_session,
    serde_json::json!({
        "type": "object",
        "properties": {
            "session_id": {
                "type": "integer",
                "description": "The ID of the session to inspect"
            }
        },
        "required": ["session_id"],
        "additionalProperties": false
    }),
    "core",
    use_context
);

// ── load_skill ─────────────────────────────────────────────────────────────

fn execute_load_skill(args: &LoadSkillArgs, cwd: Option<&Path>) -> Result<String, ToolError> {
    let effective_cwd = cwd.unwrap_or_else(|| Path::new("."));
    let body = context::load_skill_body(&args.name, effective_cwd)
        .ok_or_else(|| ToolError::Other(format!("skill not found: {}", args.name)))?;
    let skill_message = format!(
        "The following skill instructions are now active:\n\n<skill name=\"{name}\">\n{body}\n</skill>",
        name = args.name,
    );
    Ok(format!(
        "Loaded skill: {}\n\n---\n{}",
        args.name, skill_message
    ))
}

pub(crate) struct LoadSkill;

crate::define_tool!(
    LoadSkill,
    "load_skill",
    "Load the full instructions for a skill by name. Use this when a task matches one of the available skill descriptions.",
    LoadSkillArgs,
    String,
    execute_load_skill,
    serde_json::json!({
        "type": "object",
        "properties": {
            "name": {
                "type": "string",
                "description": "Name of the skill to load"
            }
        },
        "required": ["name"],
        "additionalProperties": false
    }),
    "core"
);

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::context::ToolContext;
    use std::sync::Arc;
    use tai_proto::{SessionStatus, SessionSummary, TokenUsage};

    /// Build a ToolContext with a mock daemon channel.
    fn test_context() -> (ToolContext, std::sync::mpsc::Sender<DaemonCommand>) {
        let (daemon_tx, daemon_rx) = std::sync::mpsc::channel::<DaemonCommand>();
        let dir = tempfile::tempdir().unwrap();
        let db = Arc::new(redb::Database::create(dir.path().join("test.redb")).unwrap());
        let ctx = ToolContext::new(42, db, daemon_tx.clone());

        // Spawn a mock daemon that handles ListSessions and GetSession.
        std::thread::spawn(move || {
            while let Ok(cmd) = daemon_rx.recv() {
                match cmd {
                    DaemonCommand::ListSessions { reply } => {
                        let _ = reply.send(Vec::new());
                    }
                    DaemonCommand::GetSession {
                        session_id: 1,
                        reply,
                    } => {
                        let _ = reply.send(Some(SessionSummary {
                            session_id: 1,
                            title: Some("test".into()),
                            selected_model: Some("gpt-4".into()),
                            reasoning_effort: None,
                            parent_session_id: None,
                            cwd: Some("/tmp".into()),
                            created_at: 1000,
                            message_count: 5,
                            max_turns: None,
                            status: SessionStatus::Inactive,
                            active_tool_groups: vec!["core".into()],
                            account_name: None,
                            token_usage: Some(TokenUsage::default()),
                        }));
                    }
                    DaemonCommand::GetSession {
                        session_id: 99,
                        reply,
                    } => {
                        let _ = reply.send(None);
                    }
                    _ => {}
                }
            }
        });

        (ctx, daemon_tx)
    }

    // -- list_sessions --------------------------------------------------------

    #[test]
    fn execute_list_sessions_empty() {
        let (ctx, _tx) = test_context();
        let result = execute_list_sessions(&ListSessionsArgs {}, None, Some(&ctx));
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), "No sessions found.");
    }

    #[test]
    fn execute_list_sessions_no_context() {
        let result = execute_list_sessions(&ListSessionsArgs {}, None, None);
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("no session context")
        );
    }

    #[test]
    fn execute_list_sessions_disconnected() {
        let (tx, _rx) = std::sync::mpsc::channel::<DaemonCommand>();
        // Drop the receiver so sends fail.
        drop(_rx);
        let dir = tempfile::tempdir().unwrap();
        let db = Arc::new(redb::Database::create(dir.path().join("test.redb")).unwrap());
        let ctx = ToolContext::new(42, db, tx);
        let result = execute_list_sessions(&ListSessionsArgs {}, None, Some(&ctx));
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("daemon communication failed")
        );
    }

    // -- get_session ----------------------------------------------------------

    #[test]
    fn execute_get_session_found() {
        let (ctx, _tx) = test_context();
        let args = GetSessionArgs { session_id: 1 };
        let result = execute_get_session(&args, None, Some(&ctx));
        assert!(result.is_ok());
        let msg = result.unwrap();
        assert!(msg.contains("Session 1"));
        assert!(msg.contains("test"));
        assert!(msg.contains("5 messages"));
    }

    #[test]
    fn execute_get_session_not_found() {
        let (ctx, _tx) = test_context();
        let args = GetSessionArgs { session_id: 99 };
        let result = execute_get_session(&args, None, Some(&ctx));
        assert!(result.is_err());
        assert_eq!(result.unwrap_err().to_string(), "Session 99 not found.");
    }

    #[test]
    fn execute_get_session_no_context() {
        let args = GetSessionArgs { session_id: 1 };
        let result = execute_get_session(&args, None, None);
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("no session context")
        );
    }

    #[test]
    fn execute_get_session_disconnected() {
        let (tx, _rx) = std::sync::mpsc::channel::<DaemonCommand>();
        drop(_rx);
        let dir = tempfile::tempdir().unwrap();
        let db = Arc::new(redb::Database::create(dir.path().join("test.redb")).unwrap());
        let ctx = ToolContext::new(42, db, tx);
        let args = GetSessionArgs { session_id: 1 };
        let result = execute_get_session(&args, None, Some(&ctx));
        assert!(result.is_err());
    }

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
