use choreo_daemon::tools::Tool;
use choreo_daemon::tools::context::ToolContext;
use choreo_daemon::tools::subsession::{SpawnSubsession, SpawnSubsessionArgs};
use choreo_daemon::{ChildResult, DaemonCommand, SessionCommand};
use std::collections::HashSet;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::mpsc;
use std::thread;

mod common;

/// Verify that SpawnSubsession::execute correctly communicates with the
/// daemon to create a child session, sends the prompt via RunChildInput
/// user_text, and returns the child's output as a tool result.
#[ignore]
#[test]
fn spawn_subsession_happy_path() {
    let db = Arc::new(common::test_db());
    let (daemon_tx, daemon_rx) = mpsc::channel::<DaemonCommand>();

    // ── Daemon handler thread ────────────────────────────────────────
    //
    // Intercepts DaemonCommand::CreateSession, sets up a mock child
    // session, and sends back the child session command channel so the
    // tool can interact with it.
    let daemon_handle = thread::spawn(move || {
        match daemon_rx.recv().unwrap() {
            DaemonCommand::CreateSession {
                title,
                parent_session_id,
                working_dir,
                reasoning_effort,
                selected_model,
                context_config: _,
                account_name,
                active_tool_groups,
                reply,
            } => {
                // Verify the tool forwarded the right config.
                assert_eq!(title.as_deref(), Some("test-sub"));
                assert_eq!(parent_session_id, Some(1));
                assert_eq!(working_dir, None);
                assert_eq!(reasoning_effort, None);
                assert_eq!(selected_model, None);
                assert_eq!(account_name, None);
                // With no explicit categories, the tool inherits from
                // ToolContext.active_tool_groups (empty in this test).
                assert!(active_tool_groups.is_empty());

                // Create a mock child session channel.
                let (child_tx, child_rx) = mpsc::channel::<SessionCommand>();
                let child_id = 42u64;

                // Reply to the tool with the child session sender.
                reply.send(Ok((child_id, child_tx))).unwrap();

                // ── Receive and verify RunChildInput with user_text ──
                match child_rx.recv().unwrap() {
                    SessionCommand::RunChildInput {
                        request_id,
                        user_text,
                        reply,
                    } => {
                        assert_eq!(request_id, 1);
                        assert_eq!(user_text.as_deref(), Some("work on this task"));
                        reply
                            .send(Ok(ChildResult {
                                output: "task output here".into(),
                                is_error: false,
                            }))
                            .unwrap();
                    }
                    _ => panic!("expected RunChildInput"),
                }
            }
            _ => panic!("expected CreateSession"),
        }
    });

    // ── Build ToolContext with a daemon channel ──────────────────────
    let tool_ctx = ToolContext {
        session_id: 1,
        db,
        daemon_tx,
        active_tool_groups: HashSet::new(),
        reasoning_effort: None,
        selected_model: None,
        working_dir: None,
        cancelled: Arc::new(AtomicBool::new(false)),
        account_name: None,
    };

    // ── Execute the tool ─────────────────────────────────────────────
    let result = SpawnSubsession.execute(
        SpawnSubsessionArgs {
            prompt: "work on this task".into(),
            title: Some("test-sub".into()),
            categories: None,
        },
        None, // x_credentials
        None, // working_dir
        Some(&tool_ctx),
    );

    match result {
        Ok(output) => {
            assert!(
                output.contains("sub-session 42 result:"),
                "output should mention child session id: {output}",
            );
            assert!(
                output.contains("task output here"),
                "output should contain child result: {output}",
            );
        }
        Err(e) => panic!("SpawnSubsession::execute failed: {e}"),
    }

    daemon_handle.join().unwrap();
}

/// When the daemon rejects session creation, the tool should propagate the
/// error instead of panicking or hanging.
#[ignore]
#[test]
fn spawn_subsession_daemon_rejects_creation() {
    let db = Arc::new(common::test_db());
    let (daemon_tx, daemon_rx) = mpsc::channel::<DaemonCommand>();

    let daemon_handle = thread::spawn(move || match daemon_rx.recv().unwrap() {
        DaemonCommand::CreateSession { reply, .. } => {
            reply
                .send(Err(std::io::Error::other("daemon is busy")))
                .unwrap();
        }
        _ => panic!("expected CreateSession"),
    });

    let tool_ctx = ToolContext {
        session_id: 1,
        db,
        daemon_tx,
        active_tool_groups: HashSet::new(),
        reasoning_effort: None,
        selected_model: None,
        working_dir: None,
        cancelled: Arc::new(AtomicBool::new(false)),
        account_name: None,
    };

    let result = SpawnSubsession.execute(
        SpawnSubsessionArgs {
            prompt: "irrelevant".into(),
            title: None,
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
                msg.contains("daemon is busy"),
                "error should mention daemon rejection: {msg}",
            );
        }
        Ok(output) => panic!("expected error, got success: {output}"),
    }

    daemon_handle.join().unwrap();
}

/// When the daemon command channel is dropped before the tool sends its
/// command, the tool should surface a communication error.
#[ignore]
#[test]
fn spawn_subsession_daemon_disconnected() {
    let db = Arc::new(common::test_db());
    let (daemon_tx, daemon_rx) = mpsc::channel::<DaemonCommand>();
    drop(daemon_rx);

    let tool_ctx = ToolContext {
        session_id: 1,
        db,
        daemon_tx,
        active_tool_groups: HashSet::new(),
        reasoning_effort: None,
        selected_model: None,
        working_dir: None,
        cancelled: Arc::new(AtomicBool::new(false)),
        account_name: None,
    };

    let result = SpawnSubsession.execute(
        SpawnSubsessionArgs {
            prompt: "should not matter".into(),
            title: None,
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
                msg.contains("daemon communication failed"),
                "error should mention communication failure: {msg}",
            );
        }
        Ok(output) => panic!("expected error, got success: {output}"),
    }
}

/// When no ToolContext is provided, the tool should return an error rather
/// than panicking with unwrap/expect.
#[ignore]
#[test]
fn spawn_subsession_no_context() {
    let result = SpawnSubsession.execute(
        SpawnSubsessionArgs {
            prompt: "irrelevant".into(),
            title: None,
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
                msg.contains("no session context"),
                "error should mention missing context: {msg}",
            );
        }
        Ok(output) => panic!("expected error, got success: {output}"),
    }
}

/// Verify that categories are inherited from ToolContext when not specified
/// explicitly in the arguments.
#[ignore]
#[test]
fn spawn_subsession_inherits_categories() {
    let db = Arc::new(common::test_db());
    let (daemon_tx, daemon_rx) = mpsc::channel::<DaemonCommand>();

    let daemon_handle = thread::spawn(move || {
        match daemon_rx.recv().unwrap() {
            DaemonCommand::CreateSession {
                active_tool_groups,
                reply,
                ..
            } => {
                // Should have inherited from ToolContext.
                let mut expected: Vec<String> =
                    ["core", "shell"].into_iter().map(String::from).collect();
                let mut actual = active_tool_groups.clone();
                expected.sort();
                actual.sort();
                assert_eq!(actual, expected, "should inherit active_tool_groups");

                let (child_tx, child_rx) = mpsc::channel::<SessionCommand>();
                reply.send(Ok((1u64, child_tx))).unwrap();

                // Drain child messages so the test thread can join.
                match child_rx.recv().unwrap() {
                    SessionCommand::RunChildInput { reply, .. } => {
                        reply
                            .send(Ok(ChildResult {
                                output: "ok".into(),
                                is_error: false,
                            }))
                            .unwrap();
                    }
                    _ => panic!("expected RunChildInput"),
                }
            }
            _ => panic!("expected CreateSession"),
        }
    });

    let tool_ctx = ToolContext {
        session_id: 1,
        db,
        daemon_tx,
        active_tool_groups: ["core", "shell"].into_iter().map(String::from).collect(),
        reasoning_effort: None,
        selected_model: None,
        working_dir: None,
        cancelled: Arc::new(AtomicBool::new(false)),
        account_name: None,
    };

    let result = SpawnSubsession.execute(
        SpawnSubsessionArgs {
            prompt: "work".into(),
            title: None,
            categories: None, // inherit from ctx
        },
        None,
        None,
        Some(&tool_ctx),
    );

    assert!(result.is_ok(), "expected success: {result:?}");
    daemon_handle.join().unwrap();
}

/// Override categories via explicit argument — should take precedence over
/// ToolContext.active_tool_groups.
#[ignore]
#[test]
fn spawn_subsession_overrides_categories() {
    let db = Arc::new(common::test_db());
    let (daemon_tx, daemon_rx) = mpsc::channel::<DaemonCommand>();

    let daemon_handle = thread::spawn(move || {
        match daemon_rx.recv().unwrap() {
            DaemonCommand::CreateSession {
                active_tool_groups,
                reply,
                ..
            } => {
                // Should use the explicit list, not the ctx default.
                let mut expected: Vec<String> = ["db"].into_iter().map(String::from).collect();
                let mut actual = active_tool_groups.clone();
                expected.sort();
                actual.sort();
                assert_eq!(actual, expected, "should use explicit categories");

                let (child_tx, child_rx) = mpsc::channel::<SessionCommand>();
                reply.send(Ok((1u64, child_tx))).unwrap();

                match child_rx.recv().unwrap() {
                    SessionCommand::RunChildInput { reply, .. } => {
                        reply
                            .send(Ok(ChildResult {
                                output: "ok".into(),
                                is_error: false,
                            }))
                            .unwrap();
                    }
                    _ => panic!("expected RunChildInput"),
                }
            }
            _ => panic!("expected CreateSession"),
        }
    });

    let tool_ctx = ToolContext {
        session_id: 1,
        db,
        daemon_tx,
        active_tool_groups: ["core", "shell"].into_iter().map(String::from).collect(),
        reasoning_effort: None,
        selected_model: None,
        working_dir: None,
        cancelled: Arc::new(AtomicBool::new(false)),
        account_name: None,
    };

    let result = SpawnSubsession.execute(
        SpawnSubsessionArgs {
            prompt: "work".into(),
            title: None,
            categories: Some(vec!["db".into()]),
        },
        None,
        None,
        Some(&tool_ctx),
    );

    assert!(result.is_ok(), "expected success: {result:?}");
    daemon_handle.join().unwrap();
}

/// Verify that selected_model is inherited from ToolContext when creating a
/// child session.
#[ignore]
#[test]
fn spawn_subsession_inherits_selected_model() {
    let db = Arc::new(common::test_db());
    let (daemon_tx, daemon_rx) = mpsc::channel::<DaemonCommand>();

    let daemon_handle = thread::spawn(move || {
        match daemon_rx.recv().unwrap() {
            DaemonCommand::CreateSession {
                selected_model,
                reply,
                ..
            } => {
                // Should have inherited from ToolContext.
                assert_eq!(
                    selected_model.as_deref(),
                    Some("gpt-4o"),
                    "should inherit selected_model from ToolContext",
                );

                let (child_tx, child_rx) = mpsc::channel::<SessionCommand>();
                reply.send(Ok((1u64, child_tx))).unwrap();

                match child_rx.recv().unwrap() {
                    SessionCommand::RunChildInput { reply, .. } => {
                        reply
                            .send(Ok(ChildResult {
                                output: "ok".into(),
                                is_error: false,
                            }))
                            .unwrap();
                    }
                    _ => panic!("expected RunChildInput"),
                }
            }
            _ => panic!("expected CreateSession"),
        }
    });

    let tool_ctx = ToolContext {
        session_id: 1,
        db,
        daemon_tx,
        active_tool_groups: HashSet::new(),
        reasoning_effort: None,
        selected_model: Some("gpt-4o".into()),
        working_dir: None,
        cancelled: Arc::new(AtomicBool::new(false)),
        account_name: None,
    };

    let result = SpawnSubsession.execute(
        SpawnSubsessionArgs {
            prompt: "work".into(),
            title: None,
            categories: None,
        },
        None,
        None,
        Some(&tool_ctx),
    );

    assert!(result.is_ok(), "expected success: {result:?}");
    daemon_handle.join().unwrap();
}
