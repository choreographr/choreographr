use super::*;
use crate::broadcast::test_sink;
use crate::tools::{ToolOutput, ToolRegistry};
use choreo_proto::SessionStatus;
use std::collections::HashMap;
use tempfile::tempdir;

fn test_state() -> SessionState {
    let mut turns = BTreeMap::new();
    turns.insert(
        0,
        Turn {
            created_at: TimestampMs::now(),
            undone: false,
            error: None,
            user_text: Some("hello".into()),
            assistant_text: Some("hi".into()),
            assistant_reasoning: None,
            tool_calls: Vec::new(),
            token_usage: None,
            tool_results: Vec::new(),
            displayed_images: Vec::new(),
            reasoning_artifact: None,
            reasoning_producer: None,
        },
    );
    SessionState {
        config: SessionConfig {
            title: Some("test session".into()),
            selected_model: Some("gpt-4".into()),
            reasoning_effort: None,
            parent_session_id: None,
            working_dir: Some(std::path::PathBuf::from("/tmp")),
            created_at: 1000,
            last_modified: 1000,
            status: SessionStatus::Inactive,
            active_tool_groups: ["core".into(), "shell".into()].into(),
            context_config: ContextConfig::default(),
            account_name: None,
            accumulated_usage: TokenUsage::default(),
            context_window: None,
            last_prompt_tokens: None,
            last_response_id: None,
            last_response_id_producer: None,
        },
        next_turn_id: 1,
        last_undo_turn_ids: None,
        turns,
        loaded_skill_bodies: Vec::new(),
        context_cache: None,
        discovered_skills: None,
        subscribers: HashMap::new(),
        active_requests: BTreeMap::new(),
        provider: None,
    }
}

#[test]
fn set_assistant_response_stores_artifact_and_producer() {
    // Phase 4c write-through: the agent loop stores the reasoning
    // artifact + producing model on the turn so the builder can re-emit it
    // on the next request (same-model + passback policy gates).
    let mut state = SessionState::empty();
    let (tid, _) = state.start_turn(Some("hello".into()));
    let artifact = ReasoningArtifact::ChatReasoning {
        field: choreo_proto::ChatReasoningField::ReasoningContent,
        bytes: b"thinking".to_vec(),
    };
    let producer = ReasoningProducer {
        provider_slug: "deepseek".into(),
        model: "deepseek-v4-pro".into(),
    };
    // The struct carries the artifact + producer as a unit; both must land
    // on the turn so the builder's same-model provenance check can re-emit.
    state.set_assistant_response(
        tid,
        AssistantResponse {
            text: Some("hi".into()),
            reasoning_artifact: Some(artifact.clone()),
            reasoning_producer: Some(producer.clone()),
            ..Default::default()
        },
    );
    let turn = state.turns.get(&tid).expect("turn exists");
    assert_eq!(turn.reasoning_artifact, Some(artifact));
    assert_eq!(turn.reasoning_producer, Some(producer));
}

#[test]
fn set_assistant_response_no_artifact_keeps_turn_clean() {
    // A turn without an artifact (e.g. the truncated-tool-call fallback)
    // must leave both round-trip fields None.
    let mut state = SessionState::empty();
    let (tid, _) = state.start_turn(Some("hello".into()));
    state.set_assistant_response(
        tid,
        AssistantResponse {
            text: Some("hi".into()),
            ..Default::default()
        },
    );
    let turn = state.turns.get(&tid).expect("turn exists");
    assert_eq!(turn.reasoning_artifact, None);
    assert_eq!(turn.reasoning_producer, None);
}

#[test]
fn turn_for_client_strips_artifact_and_producer() {
    // The helper is the single choke point for client-bound turns: an
    // authoritative turn carrying both round-trip fields must lose them
    // in the client copy while every client-rendered field survives.
    let artifact = ReasoningArtifact::ChatReasoning {
        field: choreo_proto::ChatReasoningField::ReasoningContent,
        bytes: b"thinking".to_vec(),
    };
    let producer = ReasoningProducer {
        provider_slug: "deepseek".into(),
        model: "deepseek-v4-pro".into(),
    };
    let mut state = SessionState::empty();
    let (tid, _) = state.start_turn(Some("hello".into()));
    state.set_assistant_response(
        tid,
        AssistantResponse {
            text: Some("hi".into()),
            reasoning: Some("thinking out loud".into()),
            reasoning_artifact: Some(artifact),
            reasoning_producer: Some(producer),
            ..Default::default()
        },
    );
    let authoritative = state.turns.get(&tid).expect("turn exists");

    let client = turn_for_client(authoritative);

    // Round-trip payload is stripped from the client copy…
    assert_eq!(client.reasoning_artifact, None);
    assert_eq!(client.reasoning_producer, None);
    // …while everything clients render survives untouched.
    assert_eq!(client.assistant_text.as_deref(), Some("hi"));
    assert_eq!(
        client.assistant_reasoning.as_deref(),
        Some("thinking out loud")
    );
    assert_eq!(client.user_text.as_deref(), Some("hello"));

    // The authoritative turn keeps the full payload for the next request's builder.
    assert!(authoritative.reasoning_artifact.is_some());
    assert!(authoritative.reasoning_producer.is_some());
}

#[test]
fn turn_for_client_strips_vision_image_keeps_displayed_images() {
    // The client-facing copy must never carry vision image bytes: the request
    // builder reads `ToolResultRecord.image` from the authoritative daemon-side
    // turn, so the client clone strips them.  `displayed_images` are what the
    // client actually renders, so they must survive intact.
    let authoritative = Turn {
        created_at: TimestampMs::now(),
        undone: false,
        error: None,
        user_text: Some("what's in a.png?".into()),
        assistant_text: None,
        assistant_reasoning: None,
        tool_calls: Vec::new(),
        token_usage: None,
        tool_results: vec![ToolResultRecord {
            call_id: "c0".into(),
            name: "read_image".into(),
            content: "pixels".into(),
            is_error: false,
            invocation_description: "Reading `a.png`.".into(),
            image: Some(choreo_proto::ImageReference {
                path: "/tmp/a.png".into(),
                mime_type: "image/png".into(),
                width: 2,
                height: 2,
                data: b"\x89PNG-vision-bytes".to_vec(),
            }),
        }],
        displayed_images: vec![DisplayedImageRecord {
            metadata: choreo_proto::ImageMetadata {
                mime_type: "image/png".into(),
                width: 2,
                height: 2,
                byte_len: 8,
                alt: Some("a.png".into()),
            },
            data: b"\x89PNG-display-bytes".to_vec(),
            tool_call_id: Some("c0".into()),
        }],
        reasoning_artifact: None,
        reasoning_producer: None,
    };
    // Guard the fixture: without stripping both images would be present.
    assert!(
        !authoritative.tool_results[0]
            .image
            .as_ref()
            .unwrap()
            .data
            .is_empty()
    );
    assert!(!authoritative.displayed_images[0].data.is_empty());

    let client = turn_for_client(&authoritative);

    // Vision image bytes are stripped (daemon/model-only)…
    assert_eq!(client.tool_results[0].image, None);
    // …while the display image the client renders survives untouched.
    assert!(!client.displayed_images[0].data.is_empty());
    assert_eq!(client.displayed_images[0].data, b"\x89PNG-display-bytes");
    assert_eq!(client.tool_results[0].name, "read_image");

    // The authoritative turn keeps the vision bytes for the request builder.
    assert!(
        !authoritative.tool_results[0]
            .image
            .as_ref()
            .unwrap()
            .data
            .is_empty()
    );
}

#[test]
fn session_state_message_strips_artifacts_from_turns() {
    // `SessionState` snapshots ride on attach — every turn in the map must
    // be the client-bound copy, even though the session's authoritative
    // turns still carry their artifacts for the daemon's own use.
    let artifact = ReasoningArtifact::ChatReasoning {
        field: choreo_proto::ChatReasoningField::ReasoningContent,
        bytes: b"thinking".to_vec(),
    };
    let producer = ReasoningProducer {
        provider_slug: "deepseek".into(),
        model: "deepseek-v4-pro".into(),
    };
    let mut state = SessionState::empty();
    let (tid, _) = state.start_turn(Some("hello".into()));
    state.set_assistant_response(
        tid,
        AssistantResponse {
            text: Some("hi".into()),
            reasoning: Some("thinking out loud".into()),
            reasoning_artifact: Some(artifact),
            reasoning_producer: Some(producer),
            ..Default::default()
        },
    );

    let DaemonMessage::Session {
        event: SessionEvent::SessionState { turns, .. },
        ..
    } = state.session_state_message(7)
    else {
        panic!("expected SessionState message");
    };
    let client_turn = turns.get(&tid).expect("turn present in message");
    assert_eq!(client_turn.reasoning_artifact, None);
    assert_eq!(client_turn.reasoning_producer, None);
    assert_eq!(client_turn.assistant_text.as_deref(), Some("hi"));
    assert_eq!(
        client_turn.assistant_reasoning.as_deref(),
        Some("thinking out loud")
    );

    // The authoritative state must be untouched — the next request's
    // builder still reads the artifact from the session's turn map.
    let authoritative = state.turns.get(&tid).expect("turn exists");
    assert!(authoritative.reasoning_artifact.is_some());
    assert!(authoritative.reasoning_producer.is_some());
}

#[test]
fn session_record_carries_last_response_id_from_config() {
    // Phase 4c: `last_response_id` (+ its producer) is worker-owned runtime
    // state that must survive the state → record conversion (and the
    // reverse on session load) so ResponseId-policy models chain across
    // user turns and daemon restarts.
    let mut state = SessionState::empty();
    state.config.last_response_id = Some("resp_9".into());
    state.config.last_response_id_producer = Some(ReasoningProducer {
        provider_slug: "openai".into(),
        model: "gpt-5.4".into(),
    });
    let record = SessionRecord::from(&state);
    assert_eq!(record.last_response_id.as_deref(), Some("resp_9"));
    assert_eq!(
        record
            .last_response_id_producer
            .as_ref()
            .map(|p| p.model.as_str()),
        Some("gpt-5.4"),
        "producer must survive the state → record conversion",
    );

    // Round-trip back into a config (mirrors `session_main` restore).
    let restored = SessionConfig {
        last_response_id: record.last_response_id.clone(),
        last_response_id_producer: record.last_response_id_producer.clone(),
        ..SessionConfig::default()
    };
    assert_eq!(restored.last_response_id.as_deref(), Some("resp_9"));
    assert_eq!(restored.last_response_id_producer.unwrap().model, "gpt-5.4",);
}

fn broadcast_setup() -> (SessionState, RequestContext) {
    let dir = tempdir().unwrap();
    let db = Arc::new(redb::Database::create(dir.path().join("test.redb")).unwrap());
    let tool_registry = ToolRegistry::new().build();
    let (daemon_tx, _) = mpsc::channel();
    let (cmd_tx, _) = mpsc::channel();
    let ctx = RequestContext {
        cmd_tx,
        session_id: 1,
        db,
        tool_registry,
        daemon_tx,
        max_turns: 0,
        lag_limits: LagLimits::default(),
        global_lag: Arc::new(AtomicUsize::new(0)),
        substrate_credential: None,
    };
    (test_state(), ctx)
}

#[test]
fn session_state_round_trip_metadata() {
    let state = test_state();
    let meta: SessionMetadata = (&state).into();
    assert_eq!(meta.title, state.config.title);
    assert_eq!(meta.selected_model, state.config.selected_model);
    assert_eq!(meta.turn_count, 1);
    assert_eq!(meta.status, state.config.status);
}

#[test]
fn session_state_to_record() {
    let state = test_state();
    let record: SessionRecord = (&state).into();
    assert_eq!(record.title, state.config.title);
    assert_eq!(record.selected_model, state.config.selected_model);
    assert_eq!(record.turn_count, 1);
}

#[test]
fn apply_worker_snapshot_preserves_main_loop_config_mutations() {
    let mut state = test_state();
    // Simulate a mid-request SessionCommand mutation applied on the main
    // loop (e.g. SetWorkingDir / LoadTools from the new tools): the
    // authoritative config changes while the worker is running.
    state.config.working_dir = Some(PathBuf::from("/main-loop-wd"));
    state.config.active_tool_groups.insert("x".into());
    state.config.title = Some("main-loop title".into());

    // The worker snapshot was taken BEFORE those mutations, so it
    // carries stale values for every config field.
    let mut snapshot = state.config.clone();
    snapshot.working_dir = Some(PathBuf::from("/stale-wd"));
    snapshot.active_tool_groups = ["core".into()].into_iter().collect();
    snapshot.title = Some("stale title".into());
    snapshot.accumulated_usage = TokenUsage {
        input_tokens: 10,
        output_tokens: 5,
        total_tokens: 15,
    };
    snapshot.context_window = Some(8192);
    snapshot.last_prompt_tokens = Some(10);

    state.config.apply_worker_snapshot(&snapshot);

    // Worker-owned fields (usage, context window) ARE applied.
    assert_eq!(state.config.accumulated_usage.total_tokens, 15);
    assert_eq!(state.config.context_window, Some(8192));
    assert_eq!(state.config.last_prompt_tokens, Some(10));

    // Config fields mutated on the main loop are NOT clobbered — this
    // is the regression guard for the set_working_dir / load_tools /
    // unload_tools lost-update bug.
    assert_eq!(
        state.config.working_dir,
        Some(PathBuf::from("/main-loop-wd"))
    );
    assert!(
        state.config.active_tool_groups.contains("x"),
        "active_tool_groups must not be clobbered by the worker snapshot"
    );
    assert_eq!(state.config.title.as_deref(), Some("main-loop title"));
}

// -- SessionCommand::Broadcast tests -----------------------------------

#[test]
fn broadcast_delivers_message_to_all_subscribers() {
    let (tx1, rx1) = test_sink();
    let (tx2, rx2) = test_sink();
    let (mut state, ctx) = broadcast_setup();
    state.subscribers.insert(10, tx1);
    state.subscribers.insert(20, tx2);

    let mut shutdown = false;
    process_command(
        SessionCommand::Broadcast(DaemonMessage::Session {
            session_id: Some(ctx.session_id),
            event: SessionEvent::Done {
                request_id: 5,
                token_usage: None,
                last_prompt_tokens: None,
            },
        }),
        &mut state,
        &mut shutdown,
        &ctx,
    );

    assert_eq!(
        rx1.recv().unwrap(),
        DaemonMessage::Session {
            session_id: Some(ctx.session_id),
            event: SessionEvent::Done {
                request_id: 5,
                token_usage: None,
                last_prompt_tokens: None,
            },
        }
    );
    assert_eq!(
        rx2.recv().unwrap(),
        DaemonMessage::Session {
            session_id: Some(ctx.session_id),
            event: SessionEvent::Done {
                request_id: 5,
                token_usage: None,
                last_prompt_tokens: None,
            },
        }
    );
    assert!(!shutdown);
}

#[test]
fn broadcast_with_no_subscribers_does_not_panic() {
    let (mut state, ctx) = broadcast_setup();
    let mut shutdown = false;
    process_command(
        SessionCommand::Broadcast(DaemonMessage::Session {
            session_id: Some(ctx.session_id),
            event: SessionEvent::Done {
                request_id: 0,
                token_usage: None,
                last_prompt_tokens: None,
            },
        }),
        &mut state,
        &mut shutdown,
        &ctx,
    );
    assert!(!shutdown);
}

#[test]
fn broadcast_handles_disconnected_subscriber_gracefully() {
    let (tx, _rx) = test_sink();
    drop(_rx);
    let (mut state, ctx) = broadcast_setup();
    state.subscribers.insert(99, tx);

    let mut shutdown = false;
    process_command(
        SessionCommand::Broadcast(DaemonMessage::Pong),
        &mut state,
        &mut shutdown,
        &ctx,
    );
    assert!(!shutdown);
}

#[test]
fn broadcast_enqueues_losslessly_and_signals_eviction() {
    // Lossless + lag-eviction: a session subscriber whose queue crosses
    // the lag cap still receives the crossing message (never dropped),
    // and the session thread signals the daemon to evict that client
    // (the daemon owns the connection and does the disconnect).
    let (mut state, mut ctx) = broadcast_setup();
    ctx.lag_limits = LagLimits {
        per_client_cap: 16,
        global_budget: usize::MAX,
    };
    let (tx, rx) = test_sink();
    state.subscribers.insert(10, tx);

    // A message large enough to cross the tiny per-client cap.
    let broadcast = DaemonMessage::Session {
        session_id: Some(ctx.session_id),
        event: SessionEvent::Failed {
            request_id: 5,
            error: "x".repeat(100),
        },
    };
    let mut shutdown = false;
    process_command(
        SessionCommand::Broadcast(broadcast.clone()),
        &mut state,
        &mut shutdown,
        &ctx,
    );

    // Lossless: the crossing message was delivered, not dropped.
    assert_eq!(rx.recv().unwrap(), broadcast);
    // The subscriber stays in the map (eviction happens daemon-side via
    // the EvictClient signal + the daemon's handle_evict_client).
    assert!(state.subscribers.contains_key(&10));
    // The session itself keeps running.
    assert!(!shutdown);
}

// -- Session-config tool command tests --------------------------------

#[test]
fn set_working_dir_updates_config_and_broadcasts() {
    let (mut state, ctx) = broadcast_setup();
    let (tx, rx) = test_sink();
    state.subscribers.insert(10, tx);
    let (reply_tx, reply_rx) = mpsc::channel();
    // Pre-populate the skill cache so we can verify it gets invalidated.
    state.discovered_skills = Some(Vec::new());
    let new_path = PathBuf::from("/tmp/new-wd");

    let mut shutdown = false;
    process_command(
        SessionCommand::SetWorkingDir {
            path: new_path.clone(),
            reply: reply_tx,
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
    match rx.recv().unwrap() {
        DaemonMessage::Session {
            session_id: Some(session_id),
            event: SessionEvent::SessionWorkingDirSet { path },
        } => {
            assert_eq!(session_id, ctx.session_id);
            assert_eq!(path.as_deref(), Some("/tmp/new-wd"));
        }
        other => panic!("expected SessionWorkingDirSet, got {:?}", other),
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
            assert_eq!(msg, "All specified groups were already active.")
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
        Ok(Ok(msg)) => assert_eq!(msg, "The 'core' group cannot be unloaded."),
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
            assert!(msg.contains("Unknown tool group(s): not-a-real-group"))
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
            assert!(msg.contains("Unknown tool group(s): not-a-real-group"))
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
    state.active_requests.insert(
        1,
        ActiveRequest {
            cancel_tx,
            turn_id: 1,
        },
    );

    let mut shutdown = false;
    process_command(
        SessionCommand::Cancel { request_id: 1 },
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
    state.active_requests.insert(
        1,
        ActiveRequest {
            cancel_tx: cancel_tx1,
            turn_id: 1,
        },
    );
    state.active_requests.insert(
        2,
        ActiveRequest {
            cancel_tx: cancel_tx2,
            turn_id: 2,
        },
    );

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

// ── Token accumulation tests ──────────────────────────────────────────

#[test]
fn accumulated_usage_starts_at_zero() {
    let state = SessionState::empty();
    assert_eq!(state.config.accumulated_usage.input_tokens, 0);
    assert_eq!(state.config.accumulated_usage.output_tokens, 0);
    assert_eq!(state.config.accumulated_usage.total_tokens, 0);
}

#[test]
fn accumulated_usage_reconstructed_from_turns() {
    let mut state = SessionState::empty();

    // Turn 0: no token_usage (should be filtered out)
    state.turns.insert(
        0,
        Turn {
            created_at: TimestampMs::now(),
            undone: false,
            error: None,
            user_text: Some("hello".into()),
            assistant_text: Some("hi".into()),
            assistant_reasoning: None,
            tool_calls: Vec::new(),
            token_usage: None,
            tool_results: Vec::new(),
            displayed_images: Vec::new(),
            reasoning_artifact: None,
            reasoning_producer: None,
        },
    );

    // Turn 1: has token_usage
    state.turns.insert(
        1,
        Turn {
            created_at: TimestampMs::now(),
            undone: false,
            error: None,
            user_text: Some("turn 2".into()),
            assistant_text: Some("response 2".into()),
            assistant_reasoning: None,
            tool_calls: Vec::new(),
            token_usage: Some(TokenUsage {
                input_tokens: 10,
                output_tokens: 20,
                total_tokens: 30,
            }),
            tool_results: Vec::new(),
            displayed_images: Vec::new(),
            reasoning_artifact: None,
            reasoning_producer: None,
        },
    );

    // Turn 2: another with token_usage
    state.turns.insert(
        2,
        Turn {
            created_at: TimestampMs::now(),
            undone: false,
            error: None,
            user_text: Some("turn 3".into()),
            assistant_text: Some("response 3".into()),
            assistant_reasoning: None,
            tool_calls: Vec::new(),
            token_usage: Some(TokenUsage {
                input_tokens: 100,
                output_tokens: 50,
                total_tokens: 150,
            }),
            tool_results: Vec::new(),
            displayed_images: Vec::new(),
            reasoning_artifact: None,
            reasoning_producer: None,
        },
    );

    // Turn 3: no token_usage (should be filtered out)
    state.turns.insert(
        3,
        Turn {
            created_at: TimestampMs::now(),
            undone: false,
            error: None,
            user_text: Some("no usage".into()),
            assistant_text: None,
            assistant_reasoning: None,
            tool_calls: Vec::new(),
            token_usage: None,
            tool_results: Vec::new(),
            displayed_images: Vec::new(),
            reasoning_artifact: None,
            reasoning_producer: None,
        },
    );

    // Run the same reconstruction logic from session_main
    let mut accumulated_usage = TokenUsage::default();
    let mut last_prompt_tokens = None;
    for turn in state.turns.values() {
        if let Some(u) = turn.token_usage {
            accumulated_usage.input_tokens += u.input_tokens;
            accumulated_usage.output_tokens += u.output_tokens;
            accumulated_usage.total_tokens += u.total_tokens;
            last_prompt_tokens = Some(u.input_tokens);
        }
    }
    state.config.accumulated_usage = accumulated_usage;
    state.config.last_prompt_tokens = last_prompt_tokens;

    // Expected: 10+100 = 110 input, 20+50 = 70 output, 30+150 = 180 total
    assert_eq!(state.config.accumulated_usage.input_tokens, 110);
    assert_eq!(state.config.accumulated_usage.output_tokens, 70);
    assert_eq!(state.config.accumulated_usage.total_tokens, 180);
    // last_prompt_tokens should be the most recent turn's input_tokens (turn 2 = 100)
    assert_eq!(state.config.last_prompt_tokens, Some(100));
}

#[test]
fn last_prompt_tokens_from_latest_usage_turn() {
    let mut state = SessionState::empty();

    // Turn 0: no token_usage
    state.turns.insert(
        0,
        Turn {
            created_at: TimestampMs::now(),
            undone: false,
            error: None,
            user_text: Some("no usage".into()),
            assistant_text: None,
            assistant_reasoning: None,
            tool_calls: Vec::new(),
            token_usage: None,
            tool_results: Vec::new(),
            displayed_images: Vec::new(),
            reasoning_artifact: None,
            reasoning_producer: None,
        },
    );

    // Turn 1: has token_usage
    state.turns.insert(
        1,
        Turn {
            created_at: TimestampMs::now(),
            undone: false,
            error: None,
            user_text: Some("first".into()),
            assistant_text: Some("response".into()),
            assistant_reasoning: None,
            tool_calls: Vec::new(),
            token_usage: Some(TokenUsage {
                input_tokens: 5,
                output_tokens: 10,
                total_tokens: 15,
            }),
            tool_results: Vec::new(),
            displayed_images: Vec::new(),
            reasoning_artifact: None,
            reasoning_producer: None,
        },
    );

    // Turn 2: has token_usage with larger input
    state.turns.insert(
        2,
        Turn {
            created_at: TimestampMs::now(),
            undone: false,
            error: None,
            user_text: Some("second".into()),
            assistant_text: Some("response 2".into()),
            assistant_reasoning: None,
            tool_calls: Vec::new(),
            token_usage: Some(TokenUsage {
                input_tokens: 42,
                output_tokens: 7,
                total_tokens: 49,
            }),
            tool_results: Vec::new(),
            displayed_images: Vec::new(),
            reasoning_artifact: None,
            reasoning_producer: None,
        },
    );

    // Reconstruct
    let mut accumulated_usage = TokenUsage::default();
    let mut last_prompt_tokens = None;
    for turn in state.turns.values() {
        if let Some(u) = turn.token_usage {
            accumulated_usage.input_tokens += u.input_tokens;
            accumulated_usage.output_tokens += u.output_tokens;
            accumulated_usage.total_tokens += u.total_tokens;
            last_prompt_tokens = Some(u.input_tokens);
        }
    }
    state.config.accumulated_usage = accumulated_usage;
    state.config.last_prompt_tokens = last_prompt_tokens;

    // total usage: 5+42 = 47 input, 10+7 = 17 output, 15+49 = 64 total
    assert_eq!(state.config.accumulated_usage.input_tokens, 47);
    assert_eq!(state.config.accumulated_usage.output_tokens, 17);
    assert_eq!(state.config.accumulated_usage.total_tokens, 64);
    // Most recent turn with usage is turn 2 → input_tokens = 42
    assert_eq!(state.config.last_prompt_tokens, Some(42));
}

#[test]
fn last_prompt_tokens_none_when_no_turns_have_usage() {
    let mut state = SessionState::empty();
    state.turns.insert(
        0,
        Turn {
            created_at: TimestampMs::now(),
            undone: false,
            error: None,
            user_text: Some("no usage".into()),
            assistant_text: None,
            assistant_reasoning: None,
            tool_calls: Vec::new(),
            token_usage: None,
            tool_results: Vec::new(),
            displayed_images: Vec::new(),
            reasoning_artifact: None,
            reasoning_producer: None,
        },
    );

    let mut accumulated_usage = TokenUsage::default();
    let mut last_prompt_tokens = None;
    for turn in state.turns.values() {
        if let Some(u) = turn.token_usage {
            accumulated_usage.input_tokens += u.input_tokens;
            accumulated_usage.output_tokens += u.output_tokens;
            accumulated_usage.total_tokens += u.total_tokens;
            last_prompt_tokens = Some(u.input_tokens);
        }
    }
    state.config.accumulated_usage = accumulated_usage;
    state.config.last_prompt_tokens = last_prompt_tokens;

    assert_eq!(state.config.accumulated_usage.input_tokens, 0);
    assert_eq!(state.config.last_prompt_tokens, None);
}

#[test]
fn accumulated_usage_in_snapshot() {
    let mut state = SessionState::empty();
    state.config.accumulated_usage = TokenUsage {
        input_tokens: 50,
        output_tokens: 25,
        total_tokens: 75,
    };
    let snap = state.snapshot();
    assert_eq!(snap.config.accumulated_usage.input_tokens, 50);
    assert_eq!(snap.config.accumulated_usage.output_tokens, 25);
    assert_eq!(snap.config.accumulated_usage.total_tokens, 75);
}

#[test]
fn accumulated_usage_in_session_summary() {
    let (mut state, ctx) = broadcast_setup();
    state.config.accumulated_usage = TokenUsage {
        input_tokens: 80,
        output_tokens: 40,
        total_tokens: 120,
    };

    let (reply, rx) = mpsc::channel();
    let mut shutdown = false;
    process_command(
        SessionCommand::GetSummary { reply },
        &mut state,
        &mut shutdown,
        &ctx,
    );

    let summary: SessionSummary = rx.recv().unwrap();
    let summary_usage = summary
        .token_usage
        .expect("token_usage should be present in SessionSummary");
    assert_eq!(summary_usage.input_tokens, 80);
    assert_eq!(summary_usage.output_tokens, 40);
    assert_eq!(summary_usage.total_tokens, 120);
}

#[test]
fn accumulated_usage_in_attach_snapshot() {
    let (mut state, ctx) = broadcast_setup();
    state.config.accumulated_usage = TokenUsage {
        input_tokens: 30,
        output_tokens: 15,
        total_tokens: 45,
    };

    let (sub_tx, sub_rx) = test_sink();
    let mut shutdown = false;
    process_command(
        SessionCommand::Attach {
            client_id: 42,
            tx: sub_tx,
        },
        &mut state,
        &mut shutdown,
        &ctx,
    );

    let msg = sub_rx.recv().unwrap();
    match msg {
        DaemonMessage::Session {
            event: SessionEvent::SessionState { token_usage, .. },
            ..
        } => {
            let usage = token_usage.expect("token_usage in SessionState");
            assert_eq!(usage.input_tokens, 30);
            assert_eq!(usage.output_tokens, 15);
            assert_eq!(usage.total_tokens, 45);
        }
        other => panic!("expected SessionState, got {other:?}"),
    }
}

#[test]
fn sync_accumulated_usage_updates_config_and_broadcasts() {
    // A live daemon channel is required here: `broadcast_setup()` drops the
    // receiver, but this test asserts the `UpdateMetadata` refresh lands.
    let dir = tempdir().unwrap();
    let db = Arc::new(redb::Database::create(dir.path().join("test.redb")).unwrap());
    let tool_registry = ToolRegistry::new().build();
    let (daemon_tx, daemon_rx) = mpsc::channel();
    let (cmd_tx, _cmd_rx) = mpsc::channel();
    let ctx = RequestContext {
        cmd_tx,
        session_id: 1,
        db,
        tool_registry,
        daemon_tx,
        max_turns: 0,
        lag_limits: LagLimits::default(),
        global_lag: Arc::new(AtomicUsize::new(0)),
        substrate_credential: None,
    };
    let mut state = test_state();

    let (sub_tx, sub_rx) = test_sink();
    state.subscribers.insert(42, sub_tx);

    // The worker's cumulative total, as routed from the private clone in
    // `broadcast_token_usage` (requests.rs).
    let synced = TokenUsage {
        input_tokens: 30,
        output_tokens: 15,
        total_tokens: 45,
    };
    let mut shutdown = false;
    process_command(
        SessionCommand::SyncAccumulatedUsage {
            token_usage: synced,
            last_prompt_tokens: Some(30),
        },
        &mut state,
        &mut shutdown,
        &ctx,
    );

    // The authoritative config is updated first, so attach snapshots and
    // session metadata read the fresh mid-turn totals.
    assert_eq!(state.config.accumulated_usage, synced);
    assert_eq!(state.config.last_prompt_tokens, Some(30));

    // ...and the update is broadcast from the authoritative state.
    let msg = sub_rx.recv().unwrap();
    match msg {
        DaemonMessage::Session {
            session_id: Some(session_id),
            event:
                SessionEvent::TokenUsageUpdate {
                    token_usage,
                    last_prompt_tokens,
                },
        } => {
            assert_eq!(session_id, ctx.session_id);
            assert_eq!(token_usage, synced);
            assert_eq!(last_prompt_tokens, Some(30));
        }
        other => panic!("expected TokenUsageUpdate, got {other:?}"),
    }

    // `broadcast` first forwards the activity to the daemon, then the
    // handler refreshes the session-metadata index — both on the same
    // thread, so ordering is deterministic.
    match daemon_rx.recv().unwrap() {
        DaemonCommand::BroadcastActivity { .. } => {}
        _ => panic!("expected BroadcastActivity forward before UpdateMetadata"),
    }
    match daemon_rx.recv().unwrap() {
        DaemonCommand::UpdateMetadata {
            session_id,
            metadata,
        } => {
            assert_eq!(session_id, ctx.session_id);
            assert_eq!(metadata.accumulated_usage, synced);
        }
        _ => panic!("expected UpdateMetadata with refreshed accumulated usage"),
    }
    assert!(!shutdown);
}

#[test]
fn attach_snapshot_carries_mid_turn_accumulated_usage() {
    // Regression test for the stale-snapshot bug: the worker accumulates
    // usage on its private session clone, so without `SyncAccumulatedUsage`
    // the main config would still carry the pre-request total and the
    // attach snapshot would leak that stale value for the whole turn.
    let (mut state, ctx) = broadcast_setup();

    let synced = TokenUsage {
        input_tokens: 30,
        output_tokens: 15,
        total_tokens: 45,
    };
    let mut shutdown = false;
    process_command(
        SessionCommand::SyncAccumulatedUsage {
            token_usage: synced,
            last_prompt_tokens: Some(30),
        },
        &mut state,
        &mut shutdown,
        &ctx,
    );

    let (sub_tx, sub_rx) = test_sink();
    process_command(
        SessionCommand::Attach {
            client_id: 42,
            tx: sub_tx,
        },
        &mut state,
        &mut shutdown,
        &ctx,
    );

    let msg = sub_rx.recv().unwrap();
    match msg {
        DaemonMessage::Session {
            event: SessionEvent::SessionState { token_usage, .. },
            ..
        } => {
            let usage = token_usage.expect("token_usage in SessionState");
            assert_eq!(usage.input_tokens, 30);
            assert_eq!(usage.output_tokens, 15);
            assert_eq!(usage.total_tokens, 45);
        }
        other => panic!("expected SessionState, got {other:?}"),
    }
}

#[test]
fn sync_accumulated_usage_never_regresses_config() {
    // The config must be monotonic even if a sync arrives out of order or
    // from an overlapping worker: a per-field max, never a blind assign.
    let dir = tempdir().unwrap();
    let db = Arc::new(redb::Database::create(dir.path().join("test.redb")).unwrap());
    let tool_registry = ToolRegistry::new().build();
    let (daemon_tx, _daemon_rx) = mpsc::channel();
    let (cmd_tx, _cmd_rx) = mpsc::channel();
    let ctx = RequestContext {
        cmd_tx,
        session_id: 1,
        db,
        tool_registry,
        daemon_tx,
        max_turns: 0,
        lag_limits: LagLimits::default(),
        global_lag: Arc::new(AtomicUsize::new(0)),
        substrate_credential: None,
    };
    let mut state = test_state();
    let mut shutdown = false;

    // A fresher (larger) total lands first…
    process_command(
        SessionCommand::SyncAccumulatedUsage {
            token_usage: TokenUsage {
                input_tokens: 30,
                output_tokens: 15,
                total_tokens: 45,
            },
            last_prompt_tokens: Some(30),
        },
        &mut state,
        &mut shutdown,
        &ctx,
    );
    // …then a stale (smaller) sync that a blind assignment would regress.
    process_command(
        SessionCommand::SyncAccumulatedUsage {
            token_usage: TokenUsage {
                input_tokens: 5,
                output_tokens: 3,
                total_tokens: 8,
            },
            last_prompt_tokens: None,
        },
        &mut state,
        &mut shutdown,
        &ctx,
    );

    assert_eq!(state.config.accumulated_usage.total_tokens, 45);
    assert_eq!(state.config.accumulated_usage.input_tokens, 30);
    assert_eq!(state.config.accumulated_usage.output_tokens, 15);
    assert_eq!(state.config.last_prompt_tokens, Some(30));
}

#[test]
fn attach_with_active_requests_sends_started_to_new_subscriber() {
    let (mut state, ctx) = broadcast_setup();

    // Populate active requests as if a request is in flight.
    let (cancel_tx1, _cancel_rx1) = crossbeam_channel::unbounded::<()>();
    let (cancel_tx2, _cancel_rx2) = crossbeam_channel::unbounded::<()>();
    state.active_requests.insert(
        10,
        ActiveRequest {
            cancel_tx: cancel_tx1,
            turn_id: 3,
        },
    );
    state.active_requests.insert(
        20,
        ActiveRequest {
            cancel_tx: cancel_tx2,
            turn_id: 7,
        },
    );

    let (sub_tx, sub_rx) = test_sink();
    let mut shutdown = false;
    process_command(
        SessionCommand::Attach {
            client_id: 42,
            tx: sub_tx,
        },
        &mut state,
        &mut shutdown,
        &ctx,
    );

    // Expect Start messages for each active request, in insertion order.
    match sub_rx.recv().unwrap() {
        DaemonMessage::Session {
            session_id: Some(1),
            event:
                SessionEvent::Started {
                    request_id: 10,
                    turn_id: 3,
                    estimated_prompt_tokens: 0,
                },
        } => {}
        other => panic!("expected Started(10, turn=3), got {other:?}"),
    }
    match sub_rx.recv().unwrap() {
        DaemonMessage::Session {
            session_id: Some(1),
            event:
                SessionEvent::Started {
                    request_id: 20,
                    turn_id: 7,
                    estimated_prompt_tokens: 0,
                },
        } => {}
        other => panic!("expected Started(20, turn=7), got {other:?}"),
    }

    // Followed by SessionState.
    match sub_rx.recv().unwrap() {
        DaemonMessage::Session {
            event: SessionEvent::SessionState { .. },
            ..
        } => {}
        other => panic!("expected SessionState, got {other:?}"),
    }

    assert!(!shutdown);
}

#[test]
fn attach_without_active_requests_does_not_send_started() {
    let (mut state, ctx) = broadcast_setup();
    // No active_requests — default empty.

    let (sub_tx, sub_rx) = test_sink();
    let mut shutdown = false;
    process_command(
        SessionCommand::Attach {
            client_id: 42,
            tx: sub_tx,
        },
        &mut state,
        &mut shutdown,
        &ctx,
    );

    // Only SessionState — no Started messages.
    match sub_rx.recv().unwrap() {
        DaemonMessage::Session {
            event: SessionEvent::SessionState { .. },
            ..
        } => {}
        other => panic!("expected SessionState, got {other:?}"),
    }

    // No more messages.
    assert!(sub_rx.try_recv().is_err());
    assert!(!shutdown);
}

// -- start_turn / undo_turns / redo_turns tests -----------------------

#[test]
fn start_turn_assigns_increasing_ids() {
    let mut state = SessionState::empty();
    let (id0, _) = state.start_turn(Some("first".into()));
    let (id1, _) = state.start_turn(Some("second".into()));
    assert_eq!(id0, 0);
    assert_eq!(id1, 1);
    assert_eq!(state.turns.len(), 2);
}

#[test]
fn seed_then_update_tool_results_preserves_call_order() {
    // The model issued calls a, b, c; the tools finish in reverse order.
    // Because results are seeded in call order and updated in place by
    // call_id, the turn's tool_results must stay a, b, c at all times.
    let mut state = SessionState::empty();
    let (tid, _) = state.start_turn(Some("run tools".into()));
    let calls = vec![
        AssistantToolCallRecord {
            call_id: "a".into(),
            name: "read_file".into(),
            arguments_json: "{}".into(),
        },
        AssistantToolCallRecord {
            call_id: "b".into(),
            name: "grep".into(),
            arguments_json: "{}".into(),
        },
        AssistantToolCallRecord {
            call_id: "c".into(),
            name: "sh".into(),
            arguments_json: "{}".into(),
        },
    ];
    state.seed_tool_results(
        tid,
        &calls,
        &[
            "Reading `a`.".into(),
            "Grepping `b`.".into(),
            "Running `c`.".into(),
        ],
    );

    let order_of = |state: &SessionState| {
        state
            .turns
            .get(&tid)
            .map(|t| {
                t.tool_results
                    .iter()
                    .map(|r| r.call_id.clone())
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default()
    };
    assert_eq!(order_of(&state), vec!["a", "b", "c"]);
    // Each placeholder carries its invocation description from the start
    // (the live header matches the final record's).
    assert_eq!(
        state.turns[&tid].tool_results[0].invocation_description,
        "Reading `a`."
    );
    assert_eq!(
        state.turns[&tid].tool_results[1].invocation_description,
        "Grepping `b`."
    );
    assert_eq!(
        state.turns[&tid].tool_results[2].invocation_description,
        "Running `c`."
    );

    // c finishes first — its placeholder is filled in place.
    state.update_tool_result(
        tid,
        "c",
        "sh".into(),
        &ToolOutput {
            content: "c-out".into(),
            is_error: false,
            invocation_description: String::new(),
            ..Default::default()
        },
    );
    assert_eq!(order_of(&state), vec!["a", "b", "c"]);
    assert_eq!(state.turns[&tid].tool_results[2].content, "c-out");

    // Then a, then b — order still unchanged.
    state.update_tool_result(
        tid,
        "a",
        "read_file".into(),
        &ToolOutput {
            content: "a-out".into(),
            is_error: false,
            invocation_description: String::new(),
            ..Default::default()
        },
    );
    state.update_tool_result(
        tid,
        "b",
        "grep".into(),
        &ToolOutput {
            content: "b-out".into(),
            is_error: false,
            invocation_description: String::new(),
            ..Default::default()
        },
    );
    assert_eq!(order_of(&state), vec!["a", "b", "c"]);
    assert_eq!(state.turns[&tid].tool_results[0].content, "a-out");
    assert_eq!(state.turns[&tid].tool_results[1].content, "b-out");

    // Error results also land in place.
    state.update_tool_result(
        tid,
        "b",
        "grep".into(),
        &ToolOutput {
            content: "boom".into(),
            is_error: true,
            invocation_description: String::new(),
            ..Default::default()
        },
    );
    assert_eq!(order_of(&state), vec!["a", "b", "c"]);
    assert!(state.turns[&tid].tool_results[1].is_error);
    assert_eq!(state.turns[&tid].tool_results[1].content, "boom");
}

#[test]
fn update_tool_result_unknown_call_id_is_noop() {
    // An update for an unseeded call id must not append — it would break
    // the call-order invariant.
    let mut state = SessionState::empty();
    let (tid, _) = state.start_turn(None);
    state.seed_tool_results(tid, &[], &[]);
    state.update_tool_result(
        tid,
        "ghost",
        "read_file".into(),
        &ToolOutput {
            content: "x".into(),
            is_error: false,
            invocation_description: String::new(),
            ..Default::default()
        },
    );
    assert!(state.turns[&tid].tool_results.is_empty());
}

#[test]
fn update_tool_result_sets_all_record_fields_from_output() {
    // The collapsed signature takes the five per-record fields from a
    // `ToolOutput`; this pins that it still populates every field on the
    // matched record, including the vision image reference (which was the
    // 7th parameter the vision commit added).
    let mut state = SessionState::empty();
    let (tid, _) = state.start_turn(Some("look".into()));
    let calls = vec![AssistantToolCallRecord {
        call_id: "a".into(),
        name: "read_image".into(),
        arguments_json: "{}".into(),
    }];
    state.seed_tool_results(tid, &calls, &["".into()]);

    let output = ToolOutput {
        content: "pixel".into(),
        is_error: true,
        invocation_description: "Reading `a`.".into(),
        image_ref: Some(choreo_proto::ImageReference {
            path: "/tmp/x.png".into(),
            mime_type: "image/jpeg".into(),
            width: 3,
            height: 2,
            data: Vec::new(),
        }),
        ..Default::default()
    };
    state.update_tool_result(tid, "a", "read_image".into(), &output);

    let record = &state.turns[&tid].tool_results[0];
    assert_eq!(record.name, "read_image");
    assert_eq!(record.content, "pixel");
    assert!(record.is_error);
    assert_eq!(record.invocation_description, "Reading `a`.");
    let img = record.image.as_ref().expect("image reference set");
    assert_eq!(img.path, "/tmp/x.png");
    assert_eq!(img.mime_type, "image/jpeg");
    assert_eq!(img.width, 3);
    assert_eq!(img.height, 2);
}

#[test]
fn mark_unexecuted_tool_results_marks_only_unexecuted() {
    // The model issued calls a, b, c; only a's result was recorded before
    // the request was cancelled. b and c must be marked "[cancelled —
    // result not recorded]" so the transcript and the next provider
    // request don't carry empty tool messages for calls whose outcome is
    // unknown.
    let mut state = SessionState::empty();
    let (tid, _) = state.start_turn(Some("run tools".into()));
    let calls = vec![
        AssistantToolCallRecord {
            call_id: "a".into(),
            name: "read_file".into(),
            arguments_json: "{}".into(),
        },
        AssistantToolCallRecord {
            call_id: "b".into(),
            name: "grep".into(),
            arguments_json: "{}".into(),
        },
        AssistantToolCallRecord {
            call_id: "c".into(),
            name: "sh".into(),
            arguments_json: "{}".into(),
        },
    ];
    state.seed_tool_results(tid, &calls, &["".into(), "".into(), "".into()]);
    // Only a's result was recorded before the cancel.
    state.update_tool_result(
        tid,
        "a",
        "read_file".into(),
        &ToolOutput {
            content: "a-out".into(),
            is_error: false,
            invocation_description: String::new(),
            ..Default::default()
        },
    );
    let executed = HashSet::from(["a".to_string()]);
    state.mark_unexecuted_tool_results(tid, &executed);

    let results = &state.turns[&tid].tool_results;
    assert_eq!(results[0].content, "a-out");
    assert!(!results[0].is_error);
    assert_eq!(results[1].content, "[cancelled — result not recorded]");
    assert!(results[1].is_error);
    assert_eq!(results[2].content, "[cancelled — result not recorded]");
    assert!(results[2].is_error);
}

#[test]
fn mark_unexecuted_tool_results_preserves_recorded_error_results() {
    // A result that WAS recorded — even an error like a timeout — has its
    // call_id in the executed set and must not be overwritten by the
    // sweep.
    let mut state = SessionState::empty();
    let (tid, _) = state.start_turn(Some("run tools".into()));
    let calls = vec![AssistantToolCallRecord {
        call_id: "a".into(),
        name: "sh".into(),
        arguments_json: "{}".into(),
    }];
    state.seed_tool_results(tid, &calls, &["".into(), "".into(), "".into()]);
    state.update_tool_result(
        tid,
        "a",
        "sh".into(),
        &ToolOutput {
            content: "timed out".into(),
            is_error: true,
            invocation_description: String::new(),
            ..Default::default()
        },
    );
    state.mark_unexecuted_tool_results(tid, &HashSet::from(["a".to_string()]));
    assert_eq!(state.turns[&tid].tool_results[0].content, "timed out");
    assert!(state.turns[&tid].tool_results[0].is_error);
}

#[test]
fn undo_turns_marks_range_and_returns_ids() {
    let mut state = SessionState::empty();
    let _ = state.start_turn(Some("user 1".into()));
    let _ = state.start_turn(Some("user 2".into()));
    assert!(state.turns.values().all(|t| !t.undone));

    let ids = state
        .undo_turns()
        .expect("undo_turns should find a user turn");
    assert_eq!(ids.len(), 1, "only the most recent user turn");
    assert!(state.turns.get(&1).unwrap().undone);
}

#[test]
fn undo_turns_returns_none_when_no_user_turn() {
    let mut state = SessionState::empty();
    let _ = state.start_turn(None); // No user_text, only system
    assert!(state.undo_turns().is_none());
}

#[test]
fn redo_turns_restores_undone_turns() {
    let mut state = SessionState::empty();
    let _ = state.start_turn(Some("user".into()));
    let ids = state.undo_turns().expect("undo succeeds");
    assert!(!ids.is_empty());

    let restored = state.redo_turns().expect("redo succeeds");
    assert_eq!(restored.len(), ids.len());
    assert!(state.turns.values().all(|t| !t.undone));
}

#[test]
fn redo_turns_returns_none_when_nothing_to_redo() {
    let mut state = SessionState::empty();
    let _ = state.start_turn(Some("user".into()));
    assert!(state.redo_turns().is_none());
}

#[test]
fn redo_turns_cleared_by_new_turn_start() {
    let mut state = SessionState::empty();
    let _ = state.start_turn(Some("first".into()));
    state.undo_turns();
    // New user turn clears redo stack
    let _ = state.start_turn(Some("second".into()));
    assert!(state.redo_turns().is_none());
}

#[test]
fn undo_clears_last_response_id_for_chain_invalidation() {
    // A persisted `previous_response_id` points at a server-side response
    // whose conversation includes the undone turns — restoring it on the
    // next request would leak the undone context back into the model. Undo
    // must clear the id (and its provenance) so the next request falls
    // back to a non-chained one carrying only the visible turns.
    let (mut state, ctx) = broadcast_setup();
    // Simulate a session that chained a Responses-policy response.
    state.config.last_response_id = Some("resp_9".into());
    state.config.last_response_id_producer = Some(ReasoningProducer {
        provider_slug: "openai".into(),
        model: "gpt-5.4".into(),
    });
    let _ = state.start_turn(Some("user 2".into()));

    let mut shutdown = false;
    process_command(SessionCommand::Undo, &mut state, &mut shutdown, &ctx);

    assert_eq!(state.config.last_response_id, None);
    assert_eq!(state.config.last_response_id_producer, None);
    // The cleared id must also be persisted so a daemon restart cannot
    // resurrect the stale chain from the on-disk record.
    let record = SessionRecord::from(&state);
    assert_eq!(record.last_response_id, None);
    assert!(!shutdown);
}

#[test]
fn undo_without_response_id_leaves_session_untouched() {
    // A session that never chained must be unaffected by the undo path's
    // chain-invalidation (no record write, no id churn).
    let (mut state, ctx) = broadcast_setup();
    let _ = state.start_turn(Some("user 2".into()));
    let mut shutdown = false;
    process_command(SessionCommand::Undo, &mut state, &mut shutdown, &ctx);
    assert_eq!(state.config.last_response_id, None);
    assert_eq!(state.config.last_response_id_producer, None);
    // The most recent user turn was marked undone; the seeded turn 0 is
    // older and stays visible (undo marks only the newest user subtree).
    assert!(state.turns.get(&1).expect("turn exists").undone);
    assert!(!state.turns.get(&0).expect("turn exists").undone);
    assert!(!shutdown);
}

#[test]
fn request_finished_after_in_flight_undo_preserves_chain_break_and_undone_turns() {
    // Regression: an undo processed while a request worker is in flight
    // must survive the worker's snapshot merge. The worker's child session
    // never saw the undo, so its snapshot carries the stale response id
    // AND copies of the undone turns without `undone` set; without the
    // guard in `handle_request_finished`, applying it resurrected the
    // chain (the leak the undo exists to prevent) and un-hid the turns.
    let (mut state, ctx) = broadcast_setup();
    state.config.last_response_id = Some("resp_9".into());
    state.config.last_response_id_producer = Some(ReasoningProducer {
        provider_slug: "openai".into(),
        model: "gpt-5.4".into(),
    });
    // Turn 1 is the newest user turn the undo will target.
    let _ = state.start_turn(Some("user 2".into()));
    let mut shutdown = false;

    // 1. The undo lands while the request is in flight: it clears the
    //    chain id and marks the newest user subtree undone.
    process_command(SessionCommand::Undo, &mut state, &mut shutdown, &ctx);
    assert_eq!(state.config.last_response_id, None);

    // 2. The worker finishes; its snapshot predates the undo — the stale
    //    id is back, its turn copies carry no `undone` flag, and it
    //    created a genuinely new turn mid-flight (id 2).
    let mut snapshot = state.snapshot();
    snapshot.config.last_response_id = Some("resp_9".into());
    snapshot.config.last_response_id_producer = Some(ReasoningProducer {
        provider_slug: "openai".into(),
        model: "gpt-5.4".into(),
    });
    for turn in snapshot.turns.values_mut() {
        turn.undone = false;
    }
    let mut in_flight = snapshot.turns.get(&0).cloned().expect("seeded turn");
    in_flight.user_text = Some("user 3 (in-flight)".into());
    snapshot.turns.insert(2, in_flight);

    process_command(
        SessionCommand::RequestFinished {
            request_id: 1,
            snapshot,
        },
        &mut state,
        &mut shutdown,
        &ctx,
    );

    // The stale chain must NOT be resurrected — in memory or on disk.
    assert_eq!(state.config.last_response_id, None);
    assert_eq!(state.config.last_response_id_producer, None);
    let record = SessionRecord::from(&state);
    assert_eq!(record.last_response_id, None);
    // The undone turn stays undone (the worker's pre-undo copy is
    // dropped, not merged back over the undo).
    assert!(state.turns.get(&1).expect("turn exists").undone);
    // The worker's genuinely-new turn still merges into the session.
    assert_eq!(
        state
            .turns
            .get(&2)
            .expect("turn exists")
            .user_text
            .as_deref(),
        Some("user 3 (in-flight)"),
    );
    assert!(!shutdown);
}

#[test]
fn request_finished_without_undo_restores_chain_id() {
    // Positive control: when no undo intervened, the worker's new response
    // id must still survive the snapshot merge — the normal chaining path
    // the undo guard must not disturb.
    let (mut state, ctx) = broadcast_setup();
    let mut snapshot = state.snapshot();
    snapshot.config.last_response_id = Some("resp_10".into());
    snapshot.config.last_response_id_producer = Some(ReasoningProducer {
        provider_slug: "openai".into(),
        model: "gpt-5.4".into(),
    });
    let mut shutdown = false;
    process_command(
        SessionCommand::RequestFinished {
            request_id: 1,
            snapshot,
        },
        &mut state,
        &mut shutdown,
        &ctx,
    );
    assert_eq!(state.config.last_response_id.as_deref(), Some("resp_10"));
    assert_eq!(
        state
            .config
            .last_response_id_producer
            .as_ref()
            .unwrap()
            .model,
        "gpt-5.4",
    );
    assert!(!shutdown);
}

// -- loaded_skill_bodies / context_cache field tests -------------------

#[test]
fn loaded_skill_bodies_default_is_empty() {
    let state = SessionState::empty();
    assert!(state.loaded_skill_bodies.is_empty());
}

#[test]
fn context_cache_default_is_none() {
    let state = SessionState::empty();
    assert!(state.context_cache.is_none());
}

#[test]
fn loaded_skill_bodies_survives_snapshot_round_trip() {
    let mut state = SessionState::empty();
    state.loaded_skill_bodies.push(LoadedSkill {
        name: "test".to_string(),
        body: "body content".to_string(),
    });

    let snap = state.snapshot();
    assert_eq!(snap.loaded_skill_bodies.len(), 1);
    assert_eq!(snap.loaded_skill_bodies[0].name, "test");

    let restored = SessionState::from_snapshot(snap, HashMap::new());
    assert_eq!(restored.loaded_skill_bodies.len(), 1);
    assert_eq!(restored.loaded_skill_bodies[0].name, "test");
    assert_eq!(restored.loaded_skill_bodies[0].body, "body content");
}

#[test]
fn context_cache_survives_snapshot_round_trip() {
    let mut state = SessionState::empty();
    state.context_cache = Some((42, Arc::new("cached content".to_string())));

    let snap = state.snapshot();
    assert_eq!(
        snap.context_cache,
        Some((42, Arc::new("cached content".to_string())))
    );

    let restored = SessionState::from_snapshot(snap, HashMap::new());
    assert_eq!(
        restored.context_cache,
        Some((42, Arc::new("cached content".to_string())))
    );
}

#[test]
fn shutdown_join_poll_joins_when_finished() {
    // Deterministic: the finish check passes immediately, so the reap
    // callback runs and no clock/sleep is consulted.
    let mut reaped = false;
    let exited = shutdown_join_poll(
        1,
        std::time::Duration::from_millis(30),
        || true,
        || reaped = true,
        std::time::Instant::now,
        |_| panic!("must not sleep when already finished"),
    );
    assert!(exited, "finished thread must be joined successfully");
    assert!(reaped, "the reap callback must run when the check passes");
}

#[test]
fn shutdown_join_poll_abandons_after_deadline() {
    // Deterministic: a fake clock advances 10 ms per read and sleep is a
    // no-op collector, so no real time elapses.  The loop must give up once
    // the 30 ms deadline passes and never sleep past the 50 ms poll cap.
    let base = std::time::Instant::now();
    let mut elapsed = std::time::Duration::ZERO;
    let mut clock = move || {
        elapsed += std::time::Duration::from_millis(10);
        base + elapsed
    };
    let mut slept: Vec<std::time::Duration> = Vec::new();
    let exited = shutdown_join_poll(
        1,
        std::time::Duration::from_millis(30),
        || false,
        || panic!("must not reap a thread that has not finished"),
        &mut clock,
        |d| slept.push(d),
    );
    assert!(!exited, "stuck thread must be abandoned, not joined");
    assert!(!slept.is_empty(), "the poll loop must sleep while waiting");
    assert!(
        slept
            .iter()
            .all(|d| *d <= std::time::Duration::from_millis(50)),
        "each sleep must respect the 50 ms poll cap"
    );
}

#[test]
fn poll_join_with_grace_abandons_stuck_thread() {
    // A thread blocked on a channel that is never sent to models a request
    // worker stuck in a provider read that a cancel cannot interrupt.  The
    // bounded join must give up after the grace period instead of hanging
    // the caller forever.  Fully deterministic: the fake clock advances
    // instantly and `sleep` is a no-op, so no real time elapses.
    let (tx, rx) = mpsc::channel::<()>();
    let handle = std::thread::spawn(move || {
        let _ = rx.recv();
    });
    let base = std::time::Instant::now();
    let mut elapsed = std::time::Duration::ZERO;
    let mut clock = move || {
        elapsed += std::time::Duration::from_millis(10);
        base + elapsed
    };
    let mut no_sleep = |_: std::time::Duration| {};
    let exited = poll_join_with_grace(
        handle,
        1,
        std::time::Duration::from_millis(30),
        &mut clock,
        &mut no_sleep,
    );
    assert!(!exited, "stuck thread must be abandoned, not joined");
    // Drop the last sender so the blocked thread's `recv` errors out and
    // it exits promptly (no leaked threads in the test runner).
    drop(tx);
}
