//! Unit tests for the session control thread, split by subject into the child
//! modules below.
//!
//! This parent holds the fixtures shared across children — `cid`,
//! `turn_with_usage`, `test_db`, `test_state`, `broadcast_setup`, and the
//! reply-correlation helpers `split_replies`/`target_for`/`one_reply`/
//! `targeted_started_stream_id` — plus the handler-resolution and turn /
//! client-view tests that do not belong to one of the child concerns, and the
//! `mod` declarations. Each child owns one concern's tests and imports exactly
//! what it uses.

use super::handlers::*;
use super::*;
use crate::broadcast::ClientId;
use crate::tools::ToolRegistry;
use choreo_proto::{MessageKind, SessionStatus};
use std::collections::HashMap;

/// Test-only shorthand for a raw client id.
fn cid(n: u32) -> ClientId {
    ClientId::from_raw(n)
}

/// An in-flight request fixture: the cancel channel plus the turn the run is
/// streaming into (and its prompt estimate), which is the pair `handle_attach`
/// replays to a late joiner.  Tests that only care about cancellation leave the
/// estimate at `0`, the value a freshly accepted request carries.
fn active_request(cancel_tx: crossbeam_channel::Sender<()>, turn_id: u32) -> ActiveRequest {
    ActiveRequest {
        cancel_tx,
        turn_id,
        estimated_prompt_tokens: 0,
    }
}

/// A turn for the context-size tests: `usage` is the provider-reported prompt
/// size (`None` models a cancelled / user-only / usage-less-provider turn),
/// and `undone` marks it as hidden by a prior undo.
fn turn_with_usage(usage: Option<u32>, undone: bool) -> Turn {
    Turn {
        created_at: TimestampMs::now(),
        undone,
        error: None,
        user_text: Some("u".into()),
        assistant_text: Some("a".into()),
        assistant_reasoning: None,
        tool_calls: Vec::new(),
        token_usage: usage.map(|input| TokenUsage {
            input_tokens: input,
            output_tokens: 1,
            total_tokens: input + 1,
            cached_tokens: 0,
            cache_write_tokens: 0,
        }),
        tool_results: Vec::new(),
        displayed_images: Vec::new(),
        reasoning_artifact: None,
        reasoning_producer: None,
    }
}

/// A fresh in-memory database fixture for the session tests. In-memory (no
/// filesystem path) keeps these unit tests off the system boundary — they
/// exercise the session state machine, not redb's on-disk format — and needs
/// no `tempfile`.
fn test_db() -> Arc<redb::Database> {
    Arc::new(
        redb::Database::builder()
            .create_with_backend(redb::backends::InMemoryBackend::new())
            .expect("create in-memory test db"),
    )
}

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
        next_stream_id: 1,
        redo_stack: Vec::new(),
        turns,
        loaded_skill_bodies: Vec::new(),
        context_cache: None,
        discovered_skills: None,
        project_tools: Arc::new(crate::mcp::ProjectToolSet::empty()),
        project_shadowed_groups: HashSet::new(),
        persisted_turns: HashSet::new(),
        subscribers: HashMap::new(),
        active_requests: BTreeMap::new(),
        provider: None,
        provider_slug: None,
        warm_policy: crate::cache_warm::WarmPolicy::default(),
        // A fresh empty registry: any provider client this test state builds
        // registers here, mirroring the production per-session scope.
        registry: choreo_ai_protocols::SocketRegistry::default(),
    }
}

/// Drive `resolve_provider` to completion without the keystore: a fake
/// daemon serves the `ResolveAccountCmd` reply from a thread, exactly the
/// round-trip the real command loop performs.
#[test]
fn resolve_provider_rebuilds_lazily_after_client_drop() {
    use zeroize::Zeroizing;

    let db = test_db();
    let tool_registry = ToolRegistry::new().build().into_shared();
    let (daemon_tx, daemon_rx) = crossbeam_channel::unbounded();
    let (cmd_tx, _) = crossbeam_channel::unbounded();
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
        warm_policy: crate::cache_warm::WarmPolicy::default(),
    };

    // The fake daemon: answer exactly ONE ResolveAccountCmd, then exit when
    // the channel closes. Deterministic — the session's `recv` unblocks only
    // after this thread's reply is sent.
    let server = std::thread::spawn(move || {
        while let Ok(DaemonCommand::ResolveAccountCmd { account, reply }) = daemon_rx.recv() {
            assert_eq!(account, "mock-account");
            let mut config = crate::accounts::AccountConfig::simple("mock-account", "openai");
            config.base_url = Some("https://mock.invalid/v1".to_string());
            // Same Zeroizing shape the real command loop sends: the reply
            // payload is the credential exit-point. The warm policy rides the
            // same reply, so resolving the account also refreshes warming.
            let _ = reply.send(Some(crate::daemon::ResolvedAccount {
                config,
                auth: Some(crate::providers::ProviderAuth::ApiKey {
                    key: Zeroizing::new("test-key".to_string()),
                }),
                warm_policy: crate::cache_warm::WarmPolicy {
                    mode: crate::cache_warm::CacheWarmingMode::Streaming,
                    meter: crate::cache_warm::MeterKind::Tokens,
                    prompt_cache_enabled: true,
                    min_prefix_tokens: 1,
                    min_expected_savings: 0.0,
                },
            }));
        }
    });

    let mut state = SessionState::empty();
    state.config.account_name = Some("mock-account".into());
    assert!(state.provider.is_none(), "clientless after the drop");

    let provider = state.resolve_provider(&ctx).expect("resolution succeeds");
    // The client was built from the served config (OpenAI protocol slug)
    // and cached on the session for reuse.
    assert_eq!(provider.provider_slug(), "openai");
    assert_eq!(
        state
            .provider
            .as_ref()
            .map(super::super::providers::InferenceProvider::provider_slug),
        Some("openai")
    );
    // The reply's warm policy is applied to the session, so warming follows the
    // account rather than a value frozen at spawn.
    assert_eq!(
        state.warm_policy.mode,
        crate::cache_warm::CacheWarmingMode::Streaming,
        "resolve applied the reply's warm policy"
    );

    // Drop the handles; the channel disconnects and the server thread exits
    // promptly (nothing else holds the sender after this).
    drop(ctx.daemon_tx);
    drop(ctx.cmd_tx);
    server.join().unwrap();
    drop(provider);
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
fn turn_for_client_strips_all_image_bytes_keeps_metadata() {
    // The client-facing copy must carry NO image bytes: the request builder
    // reads the vision bytes in `ToolResultRecord.image` from the
    // authoritative daemon-side turn, and both attachment kinds' bytes are
    // fetched on demand via `GetImage`, so the client clone strips every byte
    // payload. The METADATA survives for both: the displayed image keeps its
    // `ImageMetadata` (dimensions, mime, byte_len, alt) and the vision image
    // keeps its `ImageReference` with `data` emptied (path, mime, dimensions)
    // — so the client can size the placeholder and knows there is something to
    // fetch under the corresponding [`ImageKey`].
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
    assert_ne!(
        authoritative.tool_results[0].image.as_ref().unwrap().data,
        [] as [u8; 0]
    );
    assert_ne!(authoritative.displayed_images[0].data, [] as [u8; 0]);

    let client = turn_for_client(&authoritative);

    // The vision image's BYTES are stripped (daemon/model-only)…
    let client_ref = client.tool_results[0]
        .image
        .as_ref()
        .expect("client view keeps the vision reference metadata");
    assert_eq!(client_ref.data, [] as [u8; 0]);
    // …but its metadata survives so the client knows an image exists and how to
    // size it, and can fetch the bytes under `ImageKey::ToolResult { call_id }`.
    assert_eq!(client_ref.path, "/tmp/a.png");
    assert_eq!(client_ref.mime_type, "image/png");
    assert_eq!((client_ref.width, client_ref.height), (2, 2));
    // …and the displayed-image bytes are stripped too (fetched on demand)…
    assert_eq!(client.displayed_images[0].data, [] as [u8; 0]);
    // …but the display image's metadata survives so the client can lay out a
    // placeholder and issue its `GetImage` fetch (`byte_len > 0` is the
    // "there is something to fetch" signal).
    assert_eq!(client.displayed_images[0].metadata.byte_len, 8);
    assert_eq!(client.displayed_images[0].metadata.mime_type, "image/png");
    assert_eq!(
        client.displayed_images[0].tool_call_id.as_deref(),
        Some("c0")
    );
    assert_eq!(client.tool_results[0].name, "read_image");

    // The authoritative turn keeps both byte payloads (the request builder and
    // the DB image store rely on them).
    assert_ne!(
        authoritative.tool_results[0].image.as_ref().unwrap().data,
        [] as [u8; 0]
    );
    assert_ne!(authoritative.displayed_images[0].data, [] as [u8; 0]);
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

    let DaemonMessageType::Session {
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
#[serial_test::serial(catalog)]
fn slug_keyed_catalog_facts_resolve_without_a_live_provider() {
    // Regression: a session bound to an account whose provider slug is
    // recorded but which has NO provider client yet (keystore locked, no
    // first request) must still resolve its static catalog facts from the
    // recorded slug. Precedence: the provider's own client-config override
    // when a client exists, else the catalog; the reasoning capability always
    // resolves from the slug.
    use choreo_proto::SessionEvent;

    // Sanity: the bundled catalog pins a known window for this model.
    assert_eq!(
        choreo_ai_protocols::lookup_context_window("openai", "gpt-5"),
        Some(400_000)
    );

    let mut state = SessionState::empty();
    state.config.selected_model = Some("gpt-5".into());
    state.provider_slug = Some("openai".into());
    assert!(state.provider.is_none(), "precondition: no live client");

    // The context window resolves from the slug alone.
    assert_eq!(
        state.resolve_context_window_for_model("gpt-5"),
        Some(400_000)
    );

    // ...and the attach snapshot reports the model's reasoning capability,
    // so Alt+R never shows "reasoning capability not yet available" on a
    // locked daemon.
    let DaemonMessageType::Session {
        event:
            SessionEvent::SessionState {
                reasoning_capability,
                ..
            },
        ..
    } = state.session_state_message(7)
    else {
        panic!("expected SessionState message");
    };
    let capability = reasoning_capability.expect("capability reported without a live provider");
    assert!(
        !capability.available_effort_levels.is_empty(),
        "reasoning capability must report at least one effort level"
    );
}

#[test]
fn set_provider_slug_command_updates_and_clears_recorded_slug() {
    // The daemon's accounts-reload path pushes the account's (non-secret)
    // provider slug here; the handler records it, and `None` (account removed)
    // clears it so a stale slug can't keep feeding catalog lookups.
    let db = test_db();
    let (daemon_tx, _daemon_rx) = crossbeam_channel::unbounded();
    let (cmd_tx, _cmd_rx) = crossbeam_channel::unbounded();
    let ctx = RequestContext {
        cmd_tx,
        session_id: 9,
        db,
        tool_registry: ToolRegistry::new().build().into_shared(),
        daemon_tx,
        max_turns: 0,
        lag_limits: LagLimits::default(),
        global_lag: Arc::new(AtomicUsize::new(0)),
        substrate_credential: None,
        warm_policy: crate::cache_warm::WarmPolicy::default(),
    };

    let mut state = SessionState::empty();
    assert!(!handle_set_provider_slug(
        Some("anthropic".into()),
        &mut state,
        &ctx
    ));
    assert_eq!(state.provider_slug.as_deref(), Some("anthropic"));
    // Removal clears the recorded slug.
    assert!(!handle_set_provider_slug(None, &mut state, &ctx));
    assert_eq!(state.provider_slug, None);
}

#[test]
fn set_account_switches_slug_and_drops_stale_client_when_locked() {
    // Switching to a KNOWN account while the keystore is locked: the config
    // resolves but there is no key, so no client is built. The new slug must
    // still be recorded (so static facts are exact) AND the previous account's
    // client dropped — `resolve_provider` returns a cached client
    // unconditionally, so keeping it would dial the old provider under the new
    // account name.
    use choreo_ai_protocols::openai::{OpenAiClient, RequestAuth, ServiceConfig};

    let db = test_db();
    let (daemon_tx, daemon_rx) = crossbeam_channel::unbounded();
    let (cmd_tx, _cmd_rx) = crossbeam_channel::unbounded();
    let ctx = RequestContext {
        cmd_tx,
        session_id: 3,
        db,
        tool_registry: ToolRegistry::new().build().into_shared(),
        daemon_tx,
        max_turns: 0,
        lag_limits: LagLimits::default(),
        global_lag: Arc::new(AtomicUsize::new(0)),
        substrate_credential: None,
        warm_policy: crate::cache_warm::WarmPolicy::default(),
    };

    // The fake daemon resolves the new account's CONFIG but serves no key
    // (keystore locked) — the exact shape the real loop sends.
    let server = std::thread::spawn(move || {
        while let Ok(DaemonCommand::ResolveAccountCmd { reply, .. }) = daemon_rx.recv() {
            let config = crate::accounts::AccountConfig::simple("new-account", "anthropic");
            let _ = reply.send(Some(crate::daemon::ResolvedAccount {
                config,
                auth: None,
                warm_policy: crate::cache_warm::WarmPolicy::default(),
            }));
        }
    });

    // Pre-existing client for a DIFFERENT account.
    let mut state = SessionState::empty();
    let client = OpenAiClient::new(
        ServiceConfig::default(),
        RequestAuth::api_key("old-key"),
        &state.registry,
    )
    .unwrap();
    state.provider = Some(InferenceProvider::from_openai(client));
    state.provider_slug = Some("openai".into());
    state.config.account_name = Some("old-account".into());

    handle_set_account("new-account".into(), None, &mut state, &ctx);

    assert!(
        state.provider.is_none(),
        "account switch must drop the previous account's client"
    );
    assert_eq!(
        state.provider_slug.as_deref(),
        Some("anthropic"),
        "the new account's slug is recorded even without a client"
    );
    assert_eq!(state.config.account_name.as_deref(), Some("new-account"));

    drop(ctx.daemon_tx);
    drop(ctx.cmd_tx);
    server.join().unwrap();
}

#[test]
fn set_account_clears_slug_and_client_when_new_account_unknown() {
    // The new account can't be resolved at all (unknown): both the stale
    // client and the stale slug must be cleared so neither keeps serving the
    // old account's facts.
    use choreo_ai_protocols::openai::{OpenAiClient, RequestAuth, ServiceConfig};

    let db = test_db();
    let (daemon_tx, daemon_rx) = crossbeam_channel::unbounded();
    let (cmd_tx, _cmd_rx) = crossbeam_channel::unbounded();
    let ctx = RequestContext {
        cmd_tx,
        session_id: 4,
        db,
        tool_registry: ToolRegistry::new().build().into_shared(),
        daemon_tx,
        max_turns: 0,
        lag_limits: LagLimits::default(),
        global_lag: Arc::new(AtomicUsize::new(0)),
        substrate_credential: None,
        warm_policy: crate::cache_warm::WarmPolicy::default(),
    };

    // Unknown account: the daemon replies `None`.
    let server = std::thread::spawn(move || {
        while let Ok(DaemonCommand::ResolveAccountCmd { reply, .. }) = daemon_rx.recv() {
            let _ = reply.send(None);
        }
    });

    let mut state = SessionState::empty();
    let client = OpenAiClient::new(
        ServiceConfig::default(),
        RequestAuth::api_key("old-key"),
        &state.registry,
    )
    .unwrap();
    state.provider = Some(InferenceProvider::from_openai(client));
    state.provider_slug = Some("openai".into());
    state.config.account_name = Some("old-account".into());

    handle_set_account("ghost-account".into(), None, &mut state, &ctx);

    assert!(state.provider.is_none(), "stale client dropped");
    assert_eq!(state.provider_slug, None, "stale slug cleared");
    assert_eq!(state.config.account_name.as_deref(), Some("ghost-account"));

    drop(ctx.daemon_tx);
    drop(ctx.cmd_tx);
    server.join().unwrap();
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
    let db = test_db();
    let tool_registry = ToolRegistry::new().build().into_shared();
    let (daemon_tx, _) = crossbeam_channel::unbounded();
    let (cmd_tx, _) = crossbeam_channel::unbounded();
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
        warm_policy: crate::cache_warm::WarmPolicy::default(),
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
        cached_tokens: 0,
        cache_write_tokens: 0,
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

/// Split every message a sink has delivered into id-stamped replies and
/// `id: None` broadcasts.
fn split_replies(
    rx: &crossbeam_channel::Receiver<DaemonMessage>,
) -> (Vec<DaemonMessage>, Vec<DaemonMessage>) {
    rx.try_iter().partition(|m| m.id.is_some())
}

/// A [`ReplyTarget`] over `sink` for request `id`, sharing the context's
/// daemon-wide lag counter exactly as the connection thread's mint does.
fn target_for(
    id: u64,
    kind: MessageKind,
    sink: &SubscriberSink,
    ctx: &RequestContext,
) -> ReplyTarget {
    ReplyTarget::new(id, kind, sink.clone(), Arc::clone(&ctx.global_lag))
}

/// Assert exactly one id-stamped reply of the expected shape and return the
/// accompanying `id: None` broadcasts for further inspection.
fn one_reply(
    rx: &crossbeam_channel::Receiver<DaemonMessage>,
    id: u64,
    expected: &DaemonMessageType,
) -> Vec<DaemonMessage> {
    let (replies, broadcasts) = split_replies(rx);
    assert_eq!(replies.len(), 1, "exactly one id-stamped reply");
    assert_eq!(
        replies[0].id,
        Some(id),
        "the reply must carry the request id"
    );
    assert_eq!(&replies[0].inner, expected);
    broadcasts
}

/// Extract the daemon-assigned `stream_id` from the single id-stamped
/// `Started` reply on `replies` (the acceptance reply to `id`).
fn targeted_started_stream_id(replies: &[DaemonMessage], id: u64) -> u64 {
    assert_eq!(replies.len(), 1, "exactly one id-stamped reply");
    assert_eq!(replies[0].id, Some(id));
    match &replies[0].inner {
        DaemonMessageType::Session {
            event: SessionEvent::Started { stream_id, .. },
            ..
        } => *stream_id,
        other => panic!("expected a targeted Started, got {other:?}"),
    }
}

mod broadcast;
mod commands;
mod reply_correlation;
mod runtime_state;
mod token_usage;
mod undo_redo;
