//! config broadcast tests (see the parent `tests` module).

use super::*;

#[test]
fn concurrent_status_label_single_tool_uses_real_name() {
    // A lone non-config tool call still dispatches through the concurrent
    // bucket, so the status must show its real name, not "(parallel)".
    let label = concurrent_tool_status_label(&[config_change_call("sh", "{}")]);
    assert_eq!(label, "sh");
}

#[test]
fn concurrent_status_label_multi_tool_batch_is_parallel() {
    let label = concurrent_tool_status_label(&[
        config_change_call("sh", "{}"),
        config_change_call("grep", "{}"),
    ]);
    assert_eq!(label, "(parallel)");
}

#[test]
fn concurrent_status_label_empty_batch_is_parallel() {
    // Defensive: the caller guards with `!concurrent.is_empty()` before
    // sending, but the label should still be well-defined if reached.
    let label = concurrent_tool_status_label(&[]);
    assert_eq!(label, "(parallel)");
}

// -- Cancellation helper tests -----------------------------------------

#[test]
fn is_cancelled_once_no_signal() {
    let (_tx, rx) = crossbeam_channel::unbounded::<()>();
    assert!(!is_cancelled_once(&rx));
}

#[test]
fn is_cancelled_once_with_signal() {
    let (tx, rx) = crossbeam_channel::unbounded::<()>();
    tx.send(()).unwrap();
    assert!(is_cancelled_once(&rx));
}

// -- pending/apply config-change tests ---------------------------------

#[test]
fn pending_load_tools_captures_groups_and_applies() {
    let tool_call = config_change_call("load_tools", r#"{"groups": ["shell", "x"]}"#);
    let change = pending_config_change(&tool_call, &ok_output(None), None)
        .expect("load_tools should produce a change");
    assert!(matches!(change, PendingConfigChange::LoadTools(ref g) if g == &["shell", "x"]));

    let mut session = SessionState::empty();
    session.config.active_tool_groups = ["core".into(), "git".into()].into_iter().collect();
    apply_pending_config_change(&mut session, &change, &core_protected());
    assert!(session.config.active_tool_groups.contains("shell"));
    assert!(session.config.active_tool_groups.contains("x"));
    assert!(session.config.active_tool_groups.contains("core"));
}

#[test]
fn pending_unload_tools_captures_groups_and_applies() {
    let tool_call = config_change_call("unload_tools", r#"{"groups": ["shell"]}"#);
    let change = pending_config_change(&tool_call, &ok_output(None), None)
        .expect("unload_tools should produce a change");
    assert!(matches!(change, PendingConfigChange::UnloadTools(ref g) if g == &["shell"]));

    let mut session = SessionState::empty();
    session.config.active_tool_groups = ["core".into(), "shell".into()].into_iter().collect();
    apply_pending_config_change(&mut session, &change, &core_protected());
    assert!(!session.config.active_tool_groups.contains("shell"));
    assert!(session.config.active_tool_groups.contains("core"));
}

#[test]
fn pending_set_working_dir_mirrors_executed_result() {
    // The tool executed against a path that has since been deleted.  The
    // mirror must use the EXECUTED result (the canonical path the tool
    // computed and the main loop applied) — no re-resolution, so the
    // deleted directory cannot break the mirror (no TOCTOU).
    let tool_call = config_change_call("set_working_dir", r#"{"path": "sub"}"#);
    let output = ok_output(Some(serde_json::json!({ "path": "/real/canonical/sub" })));
    let change = pending_config_change(&tool_call, &output, None)
        .expect("set_working_dir should produce a change");
    assert!(matches!(
        change,
        PendingConfigChange::SetWorkingDir(Some(ref p)) if p == &PathBuf::from("/real/canonical/sub")
    ));

    let mut session = SessionState::empty();
    session.discovered_skills = Some(Vec::new());
    apply_pending_config_change(&mut session, &change, &core_protected());
    assert_eq!(
        session.config.working_dir.as_deref(),
        Some(PathBuf::from("/real/canonical/sub").as_path())
    );
    assert!(
        session.discovered_skills.is_none(),
        "skill cache must be invalidated by the mirror"
    );
}

#[test]
fn pending_set_working_dir_falls_back_to_shared_resolution() {
    // result_json missing (shouldn't happen on success) — the mirror
    // falls back to the SAME resolution helper the tool uses.
    let base = tempfile::tempdir().unwrap();
    let sub = base.path().join("sub");
    std::fs::create_dir(&sub).unwrap();
    let tool_call = config_change_call("set_working_dir", r#"{"path": "sub"}"#);

    let change = pending_config_change(&tool_call, &ok_output(None), Some(base.path()))
        .expect("set_working_dir should produce a change");

    let mut session = SessionState::empty();
    apply_pending_config_change(&mut session, &change, &core_protected());
    assert_eq!(
        session.config.working_dir.as_deref(),
        Some(sub.canonicalize().unwrap().as_path())
    );
}

#[test]
fn pending_set_working_dir_nonexistent_path_still_invalidates_skills() {
    // The tool succeeded (result_json present) but the path is now gone.
    // The mirror still applies the executed path verbatim — no TOCTOU.
    let tool_call = config_change_call("set_working_dir", r#"{"path": "gone"}"#);
    let output = ok_output(Some(serde_json::json!({ "path": "/gone/dir" })));
    let change = pending_config_change(&tool_call, &output, None)
        .expect("set_working_dir should produce a change");

    let mut session = SessionState::empty();
    session.discovered_skills = Some(Vec::new());
    apply_pending_config_change(&mut session, &change, &core_protected());
    assert_eq!(
        session.config.working_dir.as_deref(),
        Some(PathBuf::from("/gone/dir").as_path())
    );
    assert!(
        session.discovered_skills.is_none(),
        "skill cache must be invalidated even when the path is gone"
    );
}

#[test]
fn pending_set_working_dir_unresolvable_fallback_still_invalidates_skills() {
    // result_json missing AND the fallback resolution fails (path does
    // not exist) — the worker skips the path update but MUST still
    // invalidate its skill cache so stale skills never leak across the
    // request boundary (RequestFinished merges discovered_skills over the
    // main loop's invalidated None).
    let base = tempfile::tempdir().unwrap();
    let tool_call = config_change_call("set_working_dir", r#"{"path": "does-not-exist"}"#);

    let change = pending_config_change(&tool_call, &ok_output(None), Some(base.path()))
        .expect("set_working_dir should still produce a change");
    assert!(matches!(change, PendingConfigChange::SetWorkingDir(None)));

    let mut session = SessionState::empty();
    session.discovered_skills = Some(Vec::new());
    apply_pending_config_change(&mut session, &change, &core_protected());
    assert!(session.config.working_dir.is_none());
    assert!(
        session.discovered_skills.is_none(),
        "skill cache must be invalidated even when no path could be resolved"
    );
}

#[test]
fn pending_unknown_tool_is_noop() {
    let tool_call = config_change_call("read_file", r#"{"path": "x"}"#);
    assert!(pending_config_change(&tool_call, &ok_output(None), None).is_none());
}

#[test]
fn pending_unparseable_args_is_noop() {
    let tool_call = config_change_call("load_tools", "not json");
    assert!(pending_config_change(&tool_call, &ok_output(None), None).is_none());
}

// -- broadcast_turn_appended tests -----------------------------------

#[test]
fn broadcast_turn_appended_sends_when_turn_exists() {
    let (tx, rx) = crossbeam_channel::unbounded::<SessionCommand>();
    let mut session = SessionState::empty();
    let (turn_id, _) = session.start_turn(Some("hello".into()));

    broadcast_turn_appended(&tx, &session, 0, turn_id);

    match rx.try_recv() {
        Ok(SessionCommand::Broadcast(DaemonMessageType::Session {
            event: SessionEvent::TurnAppended { turn_id: id, .. },
            ..
        })) => {
            assert_eq!(id, turn_id);
        }
        Ok(_) => panic!("expected TurnAppended broadcast, got different command"),
        Err(e) => panic!("expected TurnAppended broadcast, got error: {e}"),
    }
}

#[test]
fn broadcast_turn_appended_no_turn_no_broadcast() {
    let (tx, rx) = crossbeam_channel::unbounded::<SessionCommand>();
    let session = SessionState::empty();

    broadcast_turn_appended(&tx, &session, 0, 999);

    assert!(rx.try_recv().is_err(), "expected no message");
}

#[test]
fn broadcast_turn_appended_disconnected_receiver_no_panic() {
    let (tx, rx) = crossbeam_channel::unbounded::<SessionCommand>();
    let mut session = SessionState::empty();
    let (turn_id, _) = session.start_turn(Some("hello".into()));
    drop(rx);

    // Disconnected receiver should not panic — warn! is logged instead.
    broadcast_turn_appended(&tx, &session, 0, turn_id);
}

#[test]
fn broadcast_turn_appended_strips_reasoning_artifact() {
    // The client-bound TurnAppended must never carry the opaque reasoning
    // round-trip payload, even when the session's authoritative turn does.
    let (tx, rx) = crossbeam_channel::unbounded::<SessionCommand>();
    let mut session = SessionState::empty();
    let (turn_id, _) = session.start_turn(Some("hello".into()));
    session.set_assistant_response(
        turn_id,
        AssistantResponse {
            text: Some("hi".into()),
            reasoning: Some("thinking out loud".into()),
            reasoning_artifact: Some(ReasoningArtifact::ChatReasoning {
                field: ChatReasoningField::ReasoningContent,
                bytes: b"thinking".to_vec(),
            }),
            reasoning_producer: Some(ReasoningProducer {
                provider_slug: "deepseek".into(),
                model: "deepseek-v4-pro".into(),
            }),
            ..Default::default()
        },
    );

    broadcast_turn_appended(&tx, &session, 0, turn_id);

    match rx.try_recv() {
        Ok(SessionCommand::Broadcast(DaemonMessageType::Session {
            event: SessionEvent::TurnAppended {
                turn_id: id, turn, ..
            },
            ..
        })) => {
            assert_eq!(id, turn_id);
            assert_eq!(turn.reasoning_artifact, None);
            assert_eq!(turn.reasoning_producer, None);
            assert_eq!(turn.assistant_text.as_deref(), Some("hi"));
            assert_eq!(
                turn.assistant_reasoning.as_deref(),
                Some("thinking out loud")
            );
        }
        Ok(_) => panic!("expected TurnAppended broadcast, got different command"),
        Err(e) => panic!("expected TurnAppended broadcast, got error: {e}"),
    }
    // The authoritative turn keeps the artifact for the next request's builder.
    assert!(session.turns[&turn_id].reasoning_artifact.is_some());
    assert!(session.turns[&turn_id].reasoning_producer.is_some());
}

#[test]
fn finalize_and_broadcast_turn_strips_reasoning_artifact() {
    // TurnAppended is the final turn snapshot sent to clients — the
    // reasoning artifact must be stripped here too, while the DB write
    // (inside finalize_and_broadcast_turn) persists the full turn.
    let (daemon_tx, _daemon_rx) = crossbeam_channel::unbounded::<DaemonCommand>();
    let (cmd_tx, cmd_rx) = crossbeam_channel::unbounded::<SessionCommand>();
    let dir = tempfile::tempdir().unwrap();
    let db = Arc::new(redb::Database::create(dir.path().join("test.redb")).unwrap());
    let ctx = RequestContext {
        cmd_tx,
        session_id: 1,
        db,
        tool_registry: ToolRegistry::new().build().into_shared(),
        daemon_tx,
        max_turns: 0,
        lag_limits: crate::broadcast::LagLimits::default(),
        global_lag: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        substrate_credential: None,
        warm_policy: crate::cache_warm::WarmPolicy::default(),
    };
    let mut session = SessionState::empty();
    let (turn_id, _) = session.start_turn(Some("hello".into()));
    session.set_assistant_response(
        turn_id,
        AssistantResponse {
            text: Some("hi".into()),
            reasoning: Some("thinking out loud".into()),
            reasoning_artifact: Some(ReasoningArtifact::ChatReasoning {
                field: ChatReasoningField::ReasoningContent,
                bytes: b"thinking".to_vec(),
            }),
            reasoning_producer: Some(ReasoningProducer {
                provider_slug: "deepseek".into(),
                model: "deepseek-v4-pro".into(),
            }),
            ..Default::default()
        },
    );

    finalize_and_broadcast_turn(&mut session, &ctx, turn_id).unwrap();

    match cmd_rx.try_recv() {
        Ok(SessionCommand::Broadcast(DaemonMessageType::Session {
            event: SessionEvent::TurnAppended { turn, .. },
            ..
        })) => {
            assert_eq!(turn.reasoning_artifact, None);
            assert_eq!(turn.reasoning_producer, None);
            assert_eq!(turn.assistant_text.as_deref(), Some("hi"));
            assert_eq!(
                turn.assistant_reasoning.as_deref(),
                Some("thinking out loud")
            );
        }
        Ok(_) => panic!("expected TurnAppended broadcast, got different command"),
        Err(e) => panic!("expected TurnAppended broadcast, got error: {e}"),
    }
    // The authoritative turn keeps the artifact after finalize + broadcast.
    assert!(session.turns[&turn_id].reasoning_artifact.is_some());
    assert!(session.turns[&turn_id].reasoning_producer.is_some());
}
