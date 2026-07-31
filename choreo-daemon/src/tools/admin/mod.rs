mod get_session;
mod list_sessions;
mod load_skill;

pub(crate) use get_session::GetSession;
pub(crate) use list_sessions::ListSessions;
pub(crate) use load_skill::LoadSkill;

#[cfg(test)]
pub(crate) mod tests {
    use crate::daemon::DaemonCommand;
    use crate::tools::context::ToolContext;
    use choreo_proto::{SessionStatus, SessionSummary, TokenUsage};
    use std::sync::Arc;

    /// Build a ToolContext with a mock daemon channel.
    pub(crate) fn test_context() -> (ToolContext, std::sync::mpsc::Sender<DaemonCommand>) {
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
                            working_dir: Some("/tmp".into()),
                            created_at: 1000,
                            turn_count: 5,
                            max_turns: None,
                            status: SessionStatus::Inactive,
                            active_tool_groups: vec!["core".into()],
                            account_name: None,
                            token_usage: Some(TokenUsage::default()),
                            context_window: None,
                            last_prompt_tokens: None,
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
}
