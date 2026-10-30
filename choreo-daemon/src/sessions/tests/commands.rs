//! Session-config tool command tests (working directory, load / unload tool
//! groups) and the Cancel / Shutdown paths.

use super::*;
use crate::broadcast::test_sink;

// -- Session-config tool command tests --------------------------------

#[test]
fn set_working_dir_updates_config_and_broadcasts() {
    let (mut state, ctx) = broadcast_setup();
    let (tx, rx) = test_sink();
    state.subscribers.insert(cid(10), tx);
    let (reply_tx, reply_rx) = mpsc::channel();
    // Pre-populate the skill cache so we can verify it gets invalidated.
    state.discovered_skills = Some(Vec::new());
    let new_path = PathBuf::from("/tmp/new-wd");

    let mut shutdown = false;
    process_command(
        SessionCommand::SetWorkingDir {
            path: new_path.clone(),
            tool_reply: reply_tx,
            reply: None,
        },
        &mut state,
        &mut shutdown,
        &ctx,
    );

    assert_eq!(state.config.working_dir, Some(new_path));
    assert!(
        state.discovered_skills.is_none(),
        "skill cache must be invalidated on working-dir change"
    );
    assert!(!shutdown);
    // Subscribers should receive the SessionWorkingDirSet broadcast.
    match rx.recv().unwrap().inner {
        DaemonMessageType::Session {
            session_id: Some(session_id),
            event: SessionEvent::SessionWorkingDirSet { path },
        } => {
            assert_eq!(session_id, ctx.session_id);
            assert_eq!(path.as_deref(), Some("/tmp/new-wd"));
        }
        other => panic!("expected SessionWorkingDirSet, got {other:?}"),
    }
    // The handler replies synchronously with the applied path.
    match reply_rx.recv() {
        Ok(Ok(msg)) => assert_eq!(msg, "/tmp/new-wd"),
        Ok(Err(e)) => panic!("expected success reply, got error: {e}"),
        Err(e) => panic!("expected reply, got {e:?}"),
    }
}

#[test]
fn load_tools_updates_active_groups_and_replies() {
    let (mut state, ctx) = broadcast_setup();
    let (reply_tx, reply_rx) = mpsc::channel();

    let mut shutdown = false;
    process_command(
        SessionCommand::LoadTools {
            groups: vec!["x".into()],
            reply: reply_tx,
        },
        &mut state,
        &mut shutdown,
        &ctx,
    );

    assert!(state.config.active_tool_groups.contains("x"));
    assert!(!shutdown);
    // The reply is sent synchronously by the handler — deterministic.
    match reply_rx.recv() {
        Ok(Ok(msg)) => assert_eq!(msg, "Activated tool groups: x"),
        Ok(Err(e)) => panic!("expected success reply, got error: {e}"),
        Err(e) => panic!("expected reply, got {e:?}"),
    }
}

#[test]
fn load_tools_skips_already_active_in_reply() {
    let (mut state, ctx) = broadcast_setup();
    state.config.active_tool_groups.insert("shell".into());
    let (reply_tx, reply_rx) = mpsc::channel();

    let mut shutdown = false;
    process_command(
        SessionCommand::LoadTools {
            groups: vec!["shell".into()],
            reply: reply_tx,
        },
        &mut state,
        &mut shutdown,
        &ctx,
    );

    match reply_rx.recv() {
        Ok(Ok(msg)) => {
            assert_eq!(msg, "All specified groups were already active.");
        }
        Ok(Err(e)) => panic!("expected success reply, got error: {e}"),
        Err(e) => panic!("expected reply, got {e:?}"),
    }
}

#[test]
fn unload_tools_updates_active_groups_and_replies() {
    let (mut state, ctx) = broadcast_setup();
    state.config.active_tool_groups.insert("x".into());
    let (reply_tx, reply_rx) = mpsc::channel();

    let mut shutdown = false;
    process_command(
        SessionCommand::UnloadTools {
            groups: vec!["x".into()],
            reply: reply_tx,
        },
        &mut state,
        &mut shutdown,
        &ctx,
    );

    assert!(!state.config.active_tool_groups.contains("x"));
    assert!(!shutdown);
    match reply_rx.recv() {
        Ok(Ok(msg)) => assert_eq!(msg, "Deactivated tool groups: x"),
        Ok(Err(e)) => panic!("expected success reply, got error: {e}"),
        Err(e) => panic!("expected reply, got {e:?}"),
    }
}

#[test]
fn unload_tools_protects_core() {
    let (mut state, ctx) = broadcast_setup();
    let (reply_tx, reply_rx) = mpsc::channel();

    let mut shutdown = false;
    process_command(
        SessionCommand::UnloadTools {
            groups: vec!["core".into()],
            reply: reply_tx,
        },
        &mut state,
        &mut shutdown,
        &ctx,
    );

    assert!(state.config.active_tool_groups.contains("core"));
    match reply_rx.recv() {
        Ok(Ok(msg)) => assert_eq!(msg, "The core group cannot be unloaded."),
        Ok(Err(e)) => panic!("expected success reply, got error: {e}"),
        Err(e) => panic!("expected reply, got {e:?}"),
    }
}

#[test]
fn load_tools_rejects_unknown_group() {
    let (mut state, ctx) = broadcast_setup();
    let (reply_tx, reply_rx) = mpsc::channel();

    let mut shutdown = false;
    process_command(
        SessionCommand::LoadTools {
            groups: vec!["not-a-real-group".into()],
            reply: reply_tx,
        },
        &mut state,
        &mut shutdown,
        &ctx,
    );

    // The unknown group must not be persisted into the authoritative set.
    assert!(!state.config.active_tool_groups.contains("not-a-real-group"));
    match reply_rx.recv() {
        Ok(Err(msg)) => {
            assert!(msg.contains("Unknown tool group(s): not-a-real-group"));
        }
        Ok(Ok(msg)) => panic!("expected error reply, got success: {msg}"),
        Err(e) => panic!("expected reply, got {e:?}"),
    }
}

#[test]
fn unload_tools_rejects_unknown_group() {
    let (mut state, ctx) = broadcast_setup();
    state.config.active_tool_groups.insert("git".into());
    let (reply_tx, reply_rx) = mpsc::channel();

    let mut shutdown = false;
    process_command(
        SessionCommand::UnloadTools {
            groups: vec!["not-a-real-group".into()],
            reply: reply_tx,
        },
        &mut state,
        &mut shutdown,
        &ctx,
    );

    // No group (valid or not) was touched — the whole request is rejected.
    assert!(state.config.active_tool_groups.contains("git"));
    match reply_rx.recv() {
        Ok(Err(msg)) => {
            assert!(msg.contains("Unknown tool group(s): not-a-real-group"));
        }
        Ok(Ok(msg)) => panic!("expected error reply, got success: {msg}"),
        Err(e) => panic!("expected reply, got {e:?}"),
    }
}

// -- Cancel / Shutdown tests -------------------------------------------

#[test]
fn cancel_sends_through_channel() {
    let (cancel_tx, cancel_rx) = crossbeam_channel::unbounded::<()>();
    let (mut state, ctx) = broadcast_setup();
    state
        .active_requests
        .insert(1, active_request(cancel_tx, 1));

    let mut shutdown = false;
    process_command(
        SessionCommand::Cancel { stream_id: 1 },
        &mut state,
        &mut shutdown,
        &ctx,
    );

    assert!(cancel_rx.try_recv().is_ok());
    assert!(!shutdown);
}

#[test]
fn shutdown_cancels_all_active_requests() {
    let (cancel_tx1, cancel_rx1) = crossbeam_channel::unbounded::<()>();
    let (cancel_tx2, cancel_rx2) = crossbeam_channel::unbounded::<()>();
    let (mut state, ctx) = broadcast_setup();
    state
        .active_requests
        .insert(1, active_request(cancel_tx1, 1));
    state
        .active_requests
        .insert(2, active_request(cancel_tx2, 2));

    let mut shutdown = false;
    process_command(SessionCommand::Shutdown, &mut state, &mut shutdown, &ctx);

    assert!(shutdown);
    assert!(cancel_rx1.try_recv().is_ok());
    assert!(cancel_rx2.try_recv().is_ok());
}

#[test]
fn shutdown_with_empty_active_requests_returns_true() {
    let (mut state, ctx) = broadcast_setup();
    let mut shutdown = false;
    let should_exit = process_command(SessionCommand::Shutdown, &mut state, &mut shutdown, &ctx);
    assert!(shutdown);
    assert!(should_exit);
}
