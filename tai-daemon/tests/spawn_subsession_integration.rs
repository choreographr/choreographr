use std::collections::HashSet;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use tai_daemon::tools::Tool;
use tai_daemon::tools::subsession::{SpawnSubsession, SpawnSubsessionArgs};

mod common;

/// SpawnSubsession is currently stubbed out during the turn-based refactor.
/// All executions should return a "not yet implemented" error.
#[ignore]
#[test]
fn spawn_subsession_returns_not_yet_implemented() {
    let db = Arc::new(common::test_db());
    let (daemon_tx, _daemon_rx) = std::sync::mpsc::channel::<tai_daemon::DaemonCommand>();

    let tool_ctx = tai_daemon::tools::context::ToolContext {
        session_id: 1,
        db,
        daemon_tx,
        active_tool_groups: HashSet::new(),
        reasoning_effort: None,
        selected_model: None,
        working_dir: None,
        cancelled: Arc::new(AtomicBool::new(false)),
    };

    let result = SpawnSubsession.execute(
        SpawnSubsessionArgs {
            prompt: "work on this task".into(),
            title: None,
            max_turns: None,
            categories: None,
        },
        None,
        None,
        Some(&tool_ctx),
    );

    match result {
        Err(e) => {
            let msg = e.to_string();
            assert!(
                msg.contains("not yet implemented"),
                "error should mention not yet implemented: {msg}",
            );
        }
        Ok(output) => panic!("expected error, got success: {output}"),
    }
}

/// SpawnSubsession returns error when no context is provided.
#[ignore]
#[test]
fn spawn_subsession_no_context() {
    let result = SpawnSubsession.execute(
        SpawnSubsessionArgs {
            prompt: "irrelevant".into(),
            title: None,
            max_turns: None,
            categories: None,
        },
        None,
        None,
        None,
    );

    match result {
        Err(e) => {
            let msg = e.to_string();
            assert!(
                msg.contains("not yet implemented"),
                "error should mention not yet implemented: {msg}",
            );
        }
        Ok(output) => panic!("expected error, got success: {output}"),
    }
}
