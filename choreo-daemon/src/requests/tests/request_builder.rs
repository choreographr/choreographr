//! request builder tests (see the parent `tests` module).

use super::*;

#[test]
fn build_chat_request_messages_empty() {
    let session = SessionState::empty();
    let result = build_chat_request_messages(&session, None, TEST_PROVIDER, TEST_MODEL, None);
    assert!(result.is_empty());
}

#[test]
fn build_chat_request_messages_with_system_prompt() {
    let session = SessionState::empty();
    let result = build_chat_request_messages(
        &session,
        Some("system prompt"),
        TEST_PROVIDER,
        TEST_MODEL,
        None,
    );
    assert_eq!(result.len(), 1);
    assert_eq!(result[0].role, "system");
    assert_eq!(result[0].content.as_deref(), Some("system prompt"));
}

#[test]
fn build_chat_request_messages_user_and_assistant() {
    let session = make_session_with_turns();
    let result = build_chat_request_messages(&session, None, TEST_PROVIDER, TEST_MODEL, None);
    assert_eq!(result.len(), 2);
    assert_eq!(result[0].role, "user");
    assert_eq!(result[0].content.as_deref(), Some("hello"));
    assert_eq!(result[1].role, "assistant");
    assert_eq!(result[1].content.as_deref(), Some("hi"));
}

#[test]
fn build_chat_request_messages_with_tool_calls() {
    let mut session = SessionState::empty();
    let (tid, _) = session.start_turn(Some("list files".into()));
    let records = vec![AssistantToolCallRecord {
        call_id: "call_1".into(),
        name: "ls".into(),
        arguments_json: r#"{"path": "."}"#.into(),
        caller: None,
    }];
    session.set_assistant_response(
        tid,
        AssistantResponse {
            text: Some("thinking".into()),
            tool_calls: records.clone(),
            ..Default::default()
        },
    );
    // Placeholder results are seeded in call order; the finished tool
    // updates its slot in place.
    session.seed_tool_results(tid, &records, &[String::new()]);
    session.update_tool_result(
        tid,
        "call_1",
        "ls".into(),
        &ToolOutput {
            content: "file.txt".into(),
            is_error: false,
            invocation_description: String::new(),
            ..Default::default()
        },
    );

    let result = build_chat_request_messages(&session, None, TEST_PROVIDER, TEST_MODEL, None);
    assert_eq!(result.len(), 3);
    assert_eq!(result[0].role, "user");
    assert_eq!(result[1].role, "assistant");
    assert!(result[1].tool_calls.is_some());
    assert_eq!(result[2].role, "tool");
    assert_eq!(result[2].tool_call_id.as_deref(), Some("call_1"));
}

#[test]
fn build_chat_request_messages_repairs_a_provider_rejected_turn() {
    // The incident shape: an assistant call with a blank name and a blank id,
    // answered `unknown tool: `. The stored transcript keeps those exact bytes,
    // but the built request copy must be one a strict provider accepts.
    let mut session = SessionState::empty();
    let bad = vec![AssistantToolCallRecord {
        call_id: String::new(),
        name: String::new(),
        arguments_json: String::new(),
        caller: None,
    }];
    let (tid0, _) = session.start_turn(Some("do the thing".into()));
    session.set_assistant_response(
        tid0,
        AssistantResponse {
            text: Some("working".into()),
            tool_calls: bad.clone(),
            ..Default::default()
        },
    );
    session.seed_tool_results(tid0, &bad, &[String::new()]);
    session.update_tool_result(
        tid0,
        "",
        String::new(),
        &ToolOutput {
            content: "unknown tool: ".into(),
            is_error: true,
            invocation_description: String::new(),
            ..Default::default()
        },
    );
    // A second, normal turn so the repair runs against a real transcript.
    let (tid1, _) = session.start_turn(Some("thanks".into()));
    session.set_assistant_response(
        tid1,
        AssistantResponse {
            text: Some("done".into()),
            ..Default::default()
        },
    );

    // The stored transcript is never modified.
    assert_eq!(
        session.turns.get(&tid0).expect("turn 0").tool_calls[0].name,
        ""
    );

    let result = build_chat_request_messages(&session, None, TEST_PROVIDER, TEST_MODEL, None);
    let index = result
        .iter()
        .position(|m| m.role == "assistant" && m.tool_calls.as_ref().is_some_and(|c| !c.is_empty()))
        .expect("the repaired assistant message is present");
    let calls = result[index].tool_calls.as_ref().expect("tool calls");
    assert_eq!(calls.len(), 1);
    assert_eq!(
        calls[0].function.name, "invalid_tool_call",
        "a blank call name is repaired to the sentinel"
    );
    assert!(!calls[0].id.trim().is_empty(), "a blank call id is minted");
    assert_eq!(calls[0].function.arguments, "{}");

    // The result still answers the call, now by its repaired id.
    assert_eq!(result[index + 1].role, "tool");
    assert_eq!(
        result[index + 1].tool_call_id.as_deref(),
        Some(calls[0].id.as_str())
    );
}

#[test]
fn build_chat_request_messages_vision_model_attaches_image() {
    // On a vision-capable model, an image-bearing tool result whose turn
    // belongs to the request IN FLIGHT (in-flight marker `Some(0)` — the
    // turn id 0 is the request's first turn) yields a synthetic user message
    // carrying the image, appended after the tool message (tool_use→tool_result
    // adjacency preserved).
    let (session, _file) = session_with_image_result();
    let result = build_chat_request_messages(
        &session,
        None,
        "deepseek",
        "deepseek-v4-flash-vision-exp",
        Some(0),
    );
    assert_eq!(result.len(), 4); // user, assistant, tool, image-user
    assert_eq!(result[2].role, "tool");
    assert_eq!(result[3].role, "user");
    assert_eq!(result[3].images.len(), 1);
    assert_eq!(result[3].images[0].mime_type, "image/jpeg");
    assert!(
        result[3]
            .content
            .as_deref()
            .unwrap()
            .contains("[image from")
    );
}

#[test]
fn build_chat_request_messages_non_vision_model_gates_image() {
    // On a text-only model the gate replaces the image with a placeholder
    // text message (no pixels), so the request never 400s — even for a turn
    // that is part of the in-flight request (`Some(0)`).
    let (session, _file) = session_with_image_result();
    let result =
        build_chat_request_messages(&session, None, "deepseek", "deepseek-v4-pro", Some(0));
    assert_eq!(result.len(), 4); // user, assistant, tool, placeholder-user
    assert_eq!(result[3].role, "user");
    assert_eq!(result[3].images.len(), 0);
    assert!(
        result[3]
            .content
            .as_deref()
            .unwrap()
            .contains("does not support image input"),
        "{}",
        result[3].content.as_deref().unwrap()
    );
}

#[test]
fn build_chat_request_messages_vision_model_empty_bytes_places_placeholder() {
    // A reference whose stored bytes are empty (e.g. an old persisted turn
    // that predates the bytes, or a non-vision gate) degrades to a placeholder
    // on a vision model (never a panic, never a source-path re-read).
    let mut session = SessionState::empty();
    let (tid, _) = session.start_turn(Some("look".into()));
    session.set_assistant_response(
        tid,
        AssistantResponse {
            text: Some("Reading.".into()),
            tool_calls: vec![AssistantToolCallRecord {
                call_id: "c".into(),
                name: "read_image".into(),
                arguments_json: r"{}".into(),
                caller: None,
            }],
            ..Default::default()
        },
    );
    let calls = session.turns[&tid].tool_calls.clone();
    session.seed_tool_results(tid, &calls, &[String::new()]);
    session.update_tool_result(
        tid,
        "c",
        "read_image".into(),
        &ToolOutput {
            content: "read image: (3x2)".into(),
            is_error: false,
            invocation_description: String::new(),
            image_ref: Some(choreo_proto::ImageReference {
                path: "/nonexistent/deleted.png".into(),
                mime_type: "image/jpeg".into(),
                width: 3,
                height: 2,
                data: Vec::new(),
            }),
            ..Default::default()
        },
    );
    let result = build_chat_request_messages(
        &session,
        None,
        "deepseek",
        "deepseek-v4-flash-vision-exp",
        Some(0),
    );
    assert_eq!(result.len(), 4);
    assert_eq!(result[3].role, "user");
    assert_eq!(result[3].images.len(), 0);
    assert!(
        result[3]
            .content
            .as_deref()
            .unwrap()
            .contains("image bytes are unavailable")
    );
}

#[test]
fn build_chat_request_messages_skips_undone_turns() {
    let mut session = SessionState::empty();
    let (tid0, _) = session.start_turn(Some("visible".into()));
    session.set_assistant_response(
        tid0,
        AssistantResponse {
            text: Some("ok".into()),
            ..Default::default()
        },
    );
    let (tid1, _) = session.start_turn(Some("hidden".into()));
    session.set_assistant_response(
        tid1,
        AssistantResponse {
            text: Some("nope".into()),
            ..Default::default()
        },
    );
    if let Some(turn) = session.turns.get_mut(&tid1) {
        turn.undone = true;
    }

    let result = build_chat_request_messages(&session, None, TEST_PROVIDER, TEST_MODEL, None);
    assert_eq!(result.len(), 2);
    assert_eq!(result[0].role, "user");
    assert_eq!(result[0].content.as_deref(), Some("visible"));
}

// -- Tool-result image decay (only in-flight-request turns attach pixels) --

#[test]
fn record_tool_completion_persists_vision_image_at_emit_time() {
    // Persist-at-emit for a tool-result VISION image: completing a `read_image`
    // call must write the `r{call_id}` attachment slot immediately — long before
    // the turn is finalized — so an on-demand `GetImage` arriving with the
    // mid-request (re)broadcast already resolves from the DB. This pins the one
    // production call site (`record_tool_completion`) that the DB-level
    // `write_attachment` unit test cannot cover.
    let (daemon_tx, _daemon_rx) = crossbeam_channel::unbounded::<DaemonCommand>();
    // Keep the receiver alive so the mid-completion broadcast `send` succeeds.
    let (cmd_tx, _cmd_rx) = crossbeam_channel::unbounded::<SessionCommand>();
    let dir = tempfile::tempdir().unwrap();
    let db = Arc::new(redb::Database::create(dir.path().join("test.redb")).unwrap());
    let ctx = RequestContext {
        cmd_tx,
        session_id: 1,
        db: Arc::clone(&db),
        tool_registry: ToolRegistry::new().build().into_shared(),
        daemon_tx,
        max_turns: 0,
        lag_limits: crate::broadcast::LagLimits::default(),
        global_lag: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        substrate_credential: None,
        warm_policy: crate::cache_warm::WarmPolicy::default(),
    };

    // Seed a turn whose tool-result placeholder for `call_img` the completion
    // path will fill (so `update_tool_result` matches the call id).
    let mut session = SessionState::empty();
    let (turn_id, _) = session.start_turn(Some("look at this".into()));
    let calls = vec![AssistantToolCallRecord {
        call_id: "call_img".into(),
        name: "read_image".into(),
        arguments_json: r#"{"path": "/tmp/x.png"}"#.into(),
        caller: None,
    }];
    session.set_assistant_response(
        turn_id,
        AssistantResponse {
            text: Some("Reading the image.".into()),
            tool_calls: calls.clone(),
            ..Default::default()
        },
    );
    session.seed_tool_results(turn_id, &calls, &[String::new()]);

    let tool_call = ChatToolCall {
        id: "call_img".into(),
        name: "read_image".into(),
        arguments_json: r#"{"path": "/tmp/x.png"}"#.into(),
        caller: None,
    };
    let mut output = ToolOutput {
        content: "read image: (3x2, image/jpeg)".into(),
        is_error: false,
        invocation_description: "Reading `x.png`.".into(),
        image_ref: Some(choreo_proto::ImageReference {
            path: "/tmp/x.png".into(),
            mime_type: "image/jpeg".into(),
            width: 3,
            height: 2,
            data: vec![9, 8, 7],
        }),
        ..Default::default()
    };
    let mut known_hint_paths = Vec::new();
    let mut pending_hints = Vec::new();

    record_tool_completion(ToolCompletionParams {
        stream_id: 0,
        session: &mut session,
        tool_call: &tool_call,
        output: &mut output,
        image: None,
        ctx: &ctx,
        current_turn_id: turn_id,
        known_hint_paths: &mut known_hint_paths,
        pending_hints: &mut pending_hints,
    });

    // The `r{call_id}` slot was persisted at emit time (no `finalize_turn` ran),
    // so it resolves by key exactly as a client's `GetImage` would.
    let stored = crate::db::read_attachment(
        &db,
        1,
        turn_id,
        &choreo_proto::ImageKey::ToolResult {
            call_id: "call_img".into(),
        },
    )
    .unwrap();
    assert_eq!(stored, Some(vec![9, 8, 7]));
}

#[test]
fn builder_attaches_pixels_for_current_request_and_decays_older_turn() {
    // Two image-bearing turns; the request currently in flight starts at
    // turn 1. Turn 0 is history: on a VISION model with valid stored bytes
    // it must still decay to a placeholder naming the source path, while
    // turn 1 (the request's own turn) attaches its pixels.
    let mut session = SessionState::empty();
    add_image_turn(&mut session, "first image", "/old/first.png");
    let current_tid = add_image_turn(&mut session, "look again", "/new/second.png");

    let result = build_chat_request_messages(
        &session,
        None,
        "deepseek",
        "deepseek-v4-flash-vision-exp",
        Some(current_tid),
    );

    // Per turn: user, assistant, tool, image-user (either pixel or decayed
    // placeholder) — 8 messages total.
    assert_eq!(result.len(), 8);
    // Found message index for each turn's image message: the first turn's
    // image message is at position 3, the second at position 7.
    let old_msg = &result[3];
    assert_eq!(old_msg.role, "user");
    assert_eq!(old_msg.images.len(), 0, "older turn must decay: no pixels");
    let old_text = old_msg.content.as_deref().expect("placeholder text");
    assert!(old_text.contains("[image from `read_image` tool result: /old/first.png]"));
    assert!(
        old_text.contains("no longer attached as pixels"),
        "{old_text}"
    );
    // The placeholder must name the SOURCE path so the model can re-read it.
    assert!(old_text.contains("/old/first.png"), "{old_text}");

    let current_msg = &result[7];
    assert_eq!(current_msg.role, "user");
    assert_eq!(
        current_msg.images.len(),
        1,
        "in-flight turn attaches pixels"
    );
    assert_eq!(current_msg.images[0].mime_type, "image/jpeg");
}

#[test]
fn builder_decays_all_images_without_an_in_flight_marker() {
    // No request in flight (session load / dry-run): every image-bearing
    // turn decays to its placeholder, even on a vision model — the agreed
    // force-decay-on-load behavior (no pixels ever re-attached from history).
    let mut session = SessionState::empty();
    add_image_turn(&mut session, "one", "/first.png");
    add_image_turn(&mut session, "two", "/second.png");

    let result = build_chat_request_messages(
        &session,
        None,
        "deepseek",
        "deepseek-v4-flash-vision-exp",
        None,
    );
    assert_eq!(result.len(), 8);
    // The two synthetic image/placeholder user messages (one per turn).
    let image_msgs: Vec<_> = result
        .iter()
        .filter(|m| {
            m.content
                .as_deref()
                .is_some_and(|c| c.contains("[image from"))
        })
        .collect();
    assert_eq!(image_msgs.len(), 2);
    for msg in &image_msgs {
        assert_eq!(msg.role, "user");
        assert_eq!(msg.images.len(), 0, "no pixels without an in-flight marker");
        assert!(
            msg.content
                .as_deref()
                .unwrap()
                .contains("no longer attached as pixels"),
        );
    }
    // Both paths are named in their placeholders.
    assert!(result[3].content.as_deref().unwrap().contains("/first.png"));
    assert!(
        result[7]
            .content
            .as_deref()
            .unwrap()
            .contains("/second.png")
    );
}

#[test]
fn builder_decayed_turn_on_non_vision_model_gets_decay_placeholder() {
    // The decay gate runs before the vision gate: a decayed turn on a
    // text-only model still carries the decay wording (both paths send no
    // pixels; the decay message names the source path).
    let mut session = SessionState::empty();
    add_image_turn(&mut session, "one", "/first.png");

    let result = build_chat_request_messages(&session, None, "deepseek", "deepseek-v4-pro", None);
    assert_eq!(result.len(), 4);
    assert_eq!(result[3].images.len(), 0);
    let text = result[3].content.as_deref().expect("text");
    assert!(
        text.contains("[image from `read_image` tool result: /first.png]"),
        "{text}"
    );
    assert!(text.contains("no longer attached as pixels"), "{text}");
}

#[test]
fn builder_undone_turn_image_is_not_resurrected_in_request_window() {
    // An undone turn is skipped entirely — image decay must never bring an
    // undone turn's image back, even when its id falls in the in-flight
    // request window.
    let mut session = SessionState::empty();
    add_image_turn(&mut session, "visible", "/visible.png");
    let undone_tid = add_image_turn(&mut session, "hidden", "/hidden.png");
    session
        .turns
        .get_mut(&undone_tid)
        .expect("turn exists")
        .undone = true;

    let result = build_chat_request_messages(
        &session,
        None,
        "deepseek",
        "deepseek-v4-flash-vision-exp",
        Some(0),
    );
    // Only the visible turn's 4 messages; no image or placeholder for the
    // undone turn.
    assert_eq!(result.len(), 4);
    let joined = result
        .iter()
        .filter_map(|m| m.content.as_deref())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        !joined.contains("hidden.png"),
        "undone image leaked: {joined}"
    );
    assert_eq!(result[3].images.len(), 1, "visible turn's pixels attached");
}

// -- Reasoning passback builder policy (phase 4b) -----------------------

#[test]
fn a_turn_caller_rides_the_rebuilt_assistant_tool_call() {
    // The Responses API resumes a model-generated program from the `caller` the
    // `function_call` carried, so a rebuilt history has to re-emit it: the daemon
    // copies the turn record's attribution onto every tool call it rebuilds,
    // which is what lets the next request copy it back onto the call and the
    // output that answers it.
    let caller = ToolCallCaller {
        kind: "program".into(),
        caller_id: "prog_123".into(),
    };
    let mut attributed = tool_call_record("call_1", "get_weather");
    attributed.caller = Some(caller.clone());
    let unattributed = tool_call_record("call_2", "read_file");

    let mut session = SessionState::empty();
    add_turn(
        &mut session,
        "weather?",
        "",
        None,
        None,
        vec![attributed, unattributed],
    );

    let messages = build_chat_request_messages(&session, None, TEST_PROVIDER, TEST_MODEL, None);
    let calls = assistant_messages(&messages)[0]
        .tool_calls
        .as_ref()
        .expect("the assistant turn carries its tool calls");
    assert_eq!(calls.len(), 2);
    let rebuilt = calls[0]
        .caller
        .as_ref()
        .expect("the attribution rides the rebuilt call");
    assert_eq!(rebuilt.kind, "program");
    assert_eq!(rebuilt.caller_id, "prog_123");
    // A call the provider did not attribute stays unattributed — the field is
    // not invented.
    assert!(calls[1].caller.is_none());

    // The chat-completions wire has no `caller` on a tool call, so neither the
    // attributed nor the bare call gains one there.
    let wire = wire_json(&messages, TEST_PROVIDER, TEST_MODEL);
    let assistant_wire = wire
        .iter()
        .find(|message| message["role"] == "assistant")
        .expect("the assistant turn rides the wire");
    for call in assistant_wire["tool_calls"]
        .as_array()
        .expect("the assistant turn serializes its tool calls")
    {
        assert!(call.get("caller").is_none());
    }
}

#[test]
fn builder_tool_loop_replays_every_turn_on_a_requires_field_route() {
    let mut session = SessionState::empty();
    // Plain text turn (no tool involvement) with an artifact. deepseek-v4-pro
    // declares `reasoning_content` as a required echo field, and DeepSeek asks
    // for every earlier turn's reasoning back once the request carries
    // `tools` — including turns that made no tool call — so this turn replays
    // its payload rather than an empty marker.
    add_turn(
        &mut session,
        "hello",
        "hi",
        Some(artifact(b"plain")),
        Some(deepseek_producer()),
        vec![],
    );
    // Tool-call turn with an artifact: must be replayed.
    add_turn(
        &mut session,
        "list files",
        "thinking...",
        Some(artifact(b"tool-thinking")),
        Some(deepseek_producer()),
        vec![tool_call_record("call_1", "ls")],
    );

    // deepseek-v4-pro carries an explicit `tool_loop` passback override.
    let result = build_chat_request_messages(&session, None, "deepseek", "deepseek-v4-pro", None);
    let assistants = assistant_messages(&result);
    assert_eq!(assistants.len(), 2);
    assert_eq!(
        assistants[0].reasoning_artifact,
        Some(artifact(b"plain")),
        "a route requiring the echo field replays every turn's payload",
    );
    assert_eq!(
        assistants[1].reasoning_artifact,
        Some(artifact(b"tool-thinking")),
        "tool-call turn must replay its artifact under ToolLoop",
    );
}

#[test]
fn builder_tool_loop_attaches_artifact_for_tool_result_turns() {
    let mut session = SessionState::empty();
    // A turn carrying only tool RESULTS (no assistant tool_calls on the
    // message, e.g. a mid-loop state persisted under an earlier request)
    // is tool-involving too: the next request must still echo it.
    let tid = add_turn(
        &mut session,
        "run it",
        "running",
        Some(artifact(b"mid-loop")),
        Some(deepseek_producer()),
        vec![],
    );
    session
        .turns
        .get_mut(&tid)
        .expect("turn exists")
        .tool_results
        .push(choreo_proto::ToolResultRecord {
            call_id: "call_1".into(),
            name: "sh".into(),
            content: "ok".into(),
            is_error: false,
            invocation_description: String::new(),
            image: None,
        });

    let result = build_chat_request_messages(&session, None, "deepseek", "deepseek-v4-pro", None);
    let assistants = assistant_messages(&result);
    assert_eq!(assistants.len(), 1);
    assert_eq!(
        assistants[0].reasoning_artifact,
        Some(artifact(b"mid-loop"))
    );
}

#[test]
fn builder_lowering_emits_empty_required_field_for_deepseek_without_artifact() {
    // DeepSeek chat requires `reasoning_content` to be present on every
    // assistant message even when the model produced no reasoning: the adapter
    // emits an empty string on the wire for a turn with nothing to echo.
    let mut session = SessionState::empty();
    add_turn(
        &mut session,
        "run it",
        "",
        None,
        Some(deepseek_producer()),
        vec![tool_call_record("call_1", "exec")],
    );

    let result = build_chat_request_messages(&session, None, "deepseek", "deepseek-v4-pro", None);
    let assistants = assistant_messages(&result);
    assert_eq!(assistants.len(), 1);
    // The builder attaches no artifact (nothing to replay) — the required-field
    // empty string is the ADAPTER's business, not a wire-shaped field the
    // daemon populates.
    assert_eq!(assistants[0].reasoning_artifact, None);
    let wire = wire_json(&result, "deepseek", "deepseek-v4-pro");
    assert_eq!(
        wire[1]["reasoning_content"], "",
        "deepseek assistant message must carry an (empty) reasoning_content"
    );
}

#[test]
fn builder_deepseek_artifact_text_outranks_empty_injection() {
    // When a real artifact exists and is echoed, the wire field carries its
    // text, not an empty placeholder.
    let mut session = SessionState::empty();
    add_turn(
        &mut session,
        "run it",
        "",
        Some(artifact(b"real thinking")),
        Some(deepseek_producer()),
        vec![tool_call_record("call_1", "exec")],
    );

    let result = build_chat_request_messages(&session, None, "deepseek", "deepseek-v4-pro", None);
    let assistants = assistant_messages(&result);
    assert_eq!(assistants.len(), 1);
    assert_eq!(
        assistants[0].reasoning_artifact,
        Some(artifact(b"real thinking"))
    );
    let wire = wire_json(&result, "deepseek", "deepseek-v4-pro");
    assert_eq!(wire[1]["reasoning_content"], "real thinking");
}

#[test]
fn builder_does_not_inject_empty_reasoning_content_for_non_deepseek() {
    // Unrelated OpenAI-chat models are untouched: no reasoning field at all on
    // a turn with nothing to echo.
    let mut session = SessionState::empty();
    add_turn(
        &mut session,
        "run it",
        "",
        None,
        Some(ReasoningProducer {
            provider_slug: "openai".into(),
            model: "gpt-4".into(),
        }),
        vec![tool_call_record("call_1", "exec")],
    );

    let result = build_chat_request_messages(&session, None, "openai", "gpt-4", None);
    let assistants = assistant_messages(&result);
    assert_eq!(assistants.len(), 1);
    assert_eq!(assistants[0].reasoning_artifact, None);
    let wire = wire_json(&result, "openai", "gpt-4");
    assert!(wire[1].get("reasoning_content").is_none());
}

#[test]
fn builder_requires_rc_empty_content_turn_echoes_artifact() {
    // A DeepSeek/Kimi turn recorded as reasoning-only — same-model artifact,
    // but empty content and no tool calls. ToolLoop alone would skip the
    // echo (no tool involvement), leaving the wire assistant message wholly
    // empty (`content: ""` + injected empty `reasoning_content`) — the exact
    // shape upstream rejects with "the message ... with role 'assistant'
    // must not be empty". The builder's empty-message fallback must echo the
    // artifact's real reasoning text instead.
    let mut session = SessionState::empty();
    add_turn(
        &mut session,
        "continue",
        "",
        Some(artifact(b"long reasoning text")),
        Some(deepseek_producer()),
        vec![],
    );

    let result = build_chat_request_messages(&session, None, "deepseek", "deepseek-v4-pro", None);
    let assistants = assistant_messages(&result);
    assert_eq!(assistants.len(), 1);
    assert_eq!(
        assistants[0].reasoning_artifact,
        Some(artifact(b"long reasoning text")),
        "empty-message fallback echoes the same-model artifact",
    );
    assert_eq!(assistants[0].content.as_deref(), Some(""));

    // The wire the provider actually receives carries the non-empty echo.
    let wire = wire_json(&result, "deepseek", "deepseek-v4-pro");
    assert_eq!(wire[1]["content"], "");
    assert_eq!(wire[1]["reasoning_content"], "long reasoning text");
}

#[test]
fn builder_requires_field_route_echoes_a_plain_turns_payload() {
    // A plain-text assistant turn with a same-model artifact on a route that
    // declares `reasoning_content` required: the payload rides, because the
    // upstream asks for the reasoning itself back, not just the key. (The
    // empty-message fallback is not what keeps this turn alive — its content
    // is non-empty; the required-field fact is what selects the echo.)
    let mut session = SessionState::empty();
    add_turn(
        &mut session,
        "hello",
        "hi",
        Some(artifact(b"plain")),
        Some(deepseek_producer()),
        vec![],
    );

    let result = build_chat_request_messages(&session, None, "deepseek", "deepseek-v4-pro", None);
    let assistants = assistant_messages(&result);
    assert_eq!(assistants.len(), 1);
    assert_eq!(assistants[0].reasoning_artifact, Some(artifact(b"plain")));
    let wire = wire_json(&result, "deepseek", "deepseek-v4-pro");
    assert_eq!(wire[1]["reasoning_content"], "plain");
}

#[test]
fn builder_requires_rc_empty_content_turn_foreign_artifact_not_replayed() {
    // Same reasoning-only shape, but the artifact was produced by a DIFFERENT
    // model (mid-session switch scenario): the payload is model-bound and
    // must never be replayed, so the message stays empty on the wire — that
    // is the unfixable case the daemon guard flags as a "must not be empty"
    // risk rather than a silent corruption.
    let mut session = SessionState::empty();
    add_turn(
        &mut session,
        "continue",
        "",
        Some(artifact(b"claude thinking")),
        Some(anthropic_producer()),
        vec![],
    );

    let result = build_chat_request_messages(&session, None, "deepseek", "deepseek-v4-pro", None);
    let assistants = assistant_messages(&result);
    assert!(
        assistants.is_empty(),
        "the wire-empty assistant message is dropped from the request copy itself"
    );
    // Nothing is on the wire for this turn: the foreign payload is never
    // replayed, and a required-field empty string is not a payload, so the
    // request builder drops the empty message rather than sending it.
    let wire = wire_json(&result, "deepseek", "deepseek-v4-pro");
    assert_eq!(wire.len(), 1, "only the user message survives: {wire:?}");
    assert_eq!(wire[0]["role"], "user");

    // The guard reports the turn, because it has left the model's view.
    assert_eq!(
        warn_on_uncaptured_reasoning(&session, 7, "deepseek", "deepseek-v4-pro"),
        1,
    );
}

#[test]
fn guard_requires_rc_empty_content_turn_with_replayable_artifact_is_clean() {
    // Wire-empty turn whose same-model artifact the builder echoes via the
    // empty-message fallback: the request self-heals, so the guard must NOT
    // count it as a problem.
    let mut session = SessionState::empty();
    add_turn(
        &mut session,
        "continue",
        "",
        Some(artifact(b"thinking")),
        Some(deepseek_producer()),
        vec![],
    );
    assert_eq!(
        warn_on_uncaptured_reasoning(&session, 7, "deepseek", "deepseek-v4-pro"),
        0,
        "empty-message fallback already echoes the same-model artifact",
    );
}

#[test]
fn guard_requires_rc_empty_content_turn_missing_artifact_is_flagged() {
    // Wire-empty turn with NO artifact at all (e.g. reasoning-only response
    // captured before the artifact feature, or a producer that never sent
    // reasoning): nothing can fill the message, so the provider's "must not
    // be empty" 400 is certain — the guard must surface it.
    let mut session = SessionState::empty();
    add_turn(
        &mut session,
        "continue",
        "",
        None,
        Some(deepseek_producer()),
        vec![],
    );
    assert_eq!(
        warn_on_uncaptured_reasoning(&session, 7, "deepseek", "deepseek-v4-pro"),
        1,
        "wire-empty turn with no artifact to fill it is flagged",
    );
}

#[test]
fn builder_tool_loop_still_gates_per_turn_without_a_required_field() {
    // The mirror of `builder_tool_loop_replays_every_turn_on_a_requires_field_route`:
    // on a ToolLoop route that demands no echoed field (groq here), a plain-text
    // turn's payload still stays home — the upstream asks for nothing, so
    // replaying it would only spend context.
    let mut session = SessionState::empty();
    add_turn(
        &mut session,
        "hello",
        "hi",
        Some(artifact(b"plain")),
        Some(groq_producer()),
        vec![],
    );

    let result = build_chat_request_messages(&session, None, "groq", GROQ_MODEL, None);
    let assistants = assistant_messages(&result);
    assert_eq!(assistants.len(), 1);
    assert_eq!(
        assistants[0].reasoning_artifact, None,
        "a route with no required echo field keeps the per-turn ToolLoop gate",
    );
}

#[test]
fn builder_wire_empty_turn_echoes_artifact_on_non_requires_rc_provider() {
    // The empty-message fallback is provider-agnostic: a content-less,
    // tool-less turn with a same-model artifact must echo it on ANY
    // echo-capable chat provider (here groq — ToolLoop passback, no
    // `reasoning_content` injection) because a wholly empty assistant
    // message is the "must not be empty" 400 on any OpenAI-compatible
    // endpoint, not just DeepSeek/Kimi.
    let mut session = SessionState::empty();
    add_turn(
        &mut session,
        "continue",
        "",
        Some(artifact(b"reasoned but silent")),
        Some(groq_producer()),
        vec![],
    );

    let result = build_chat_request_messages(&session, None, "groq", GROQ_MODEL, None);
    let assistants = assistant_messages(&result);
    assert_eq!(assistants.len(), 1);
    assert_eq!(
        assistants[0].reasoning_artifact,
        Some(artifact(b"reasoned but silent")),
        "empty-message fallback echoes the same-model artifact on any echo-capable chat provider",
    );
    // No declared field on groq: the artifact re-emits its text directly under
    // the field it was observed in.
    let wire = wire_json(&result, "groq", GROQ_MODEL);
    assert_eq!(wire[1]["content"], "");
    assert_eq!(wire[1]["reasoning_content"], "reasoned but silent");
}

#[test]
fn guard_wire_empty_turn_without_artifact_flagged_on_non_requires_rc() {
    // Wire-empty turn with nothing replayable is flagged on a non-requires_rc
    // ToolLoop provider too — the pre-generalization guard only caught
    // DeepSeek/Kimi turns (the missing-artifact `requires_rc` gate), leaving
    // an equally invalid empty message unflagged on e.g. groq.
    let mut session = SessionState::empty();
    add_turn(
        &mut session,
        "continue",
        "",
        None,
        Some(groq_producer()),
        vec![],
    );
    assert_eq!(
        warn_on_uncaptured_reasoning(&session, 7, "groq", GROQ_MODEL),
        1,
        "wire-empty turn with no artifact to fill it is flagged on any echo-capable chat provider",
    );
    let result = build_chat_request_messages(&session, None, "groq", GROQ_MODEL, None);
    let assistants = assistant_messages(&result);
    assert!(
        assistants.is_empty(),
        "the wire-empty assistant message is dropped from the request copy itself"
    );
    // groq declares no echo field and nothing was captured, so the empty
    // assistant message never reaches the wire (the hazard the guard
    // just reported is dropped by the request builder).
    let wire = wire_json(&result, "groq", GROQ_MODEL);
    assert_eq!(wire.len(), 1, "only the user message survives: {wire:?}");
    assert_eq!(wire[0]["role"], "user");
}

#[test]
fn builder_wire_empty_turn_never_echoes_under_none_passback() {
    // Cerebras gpt-oss-120b is pinned to `reasoning_passback = none` by the
    // bundled overlay ("the gateway rejects replayed reasoning_content"): the
    // empty-message fallback must NOT override that explicit never-replay
    // policy — echoing would swap the "must not be empty" 400 for a "must
    // not replay" 400. The guard skips None-passback requests entirely
    // (documented policy), so this stays a session_inspect-visible hazard.
    let mut session = SessionState::empty();
    add_turn(
        &mut session,
        "continue",
        "",
        Some(artifact(b"gpt-oss thinking")),
        Some(cerebras_producer()),
        vec![],
    );

    let result = build_chat_request_messages(&session, None, "cerebras", "gpt-oss-120b", None);
    let assistants = assistant_messages(&result);
    assert!(
        assistants.is_empty(),
        "the wire-empty assistant message is dropped from the request copy itself"
    );
    // The never-replay pin means the wire-empty message cannot be filled at
    // all, so the request builder drops it instead of sending an empty
    // assistant message (which the gateway would reject just as hard).
    let wire = wire_json(&result, "cerebras", "gpt-oss-120b");
    assert_eq!(wire.len(), 1, "only the user message survives: {wire:?}");
    assert_eq!(
        warn_on_uncaptured_reasoning(&session, 7, "cerebras", "gpt-oss-120b"),
        1,
    );
}

#[test]
fn builder_all_turns_attaches_always() {
    let mut session = SessionState::empty();
    // Unknown model under the anthropic slug → protocol default AllTurns
    // (no explicit TOML override, unlike claude-sonnet-4-5 which is a
    // last-turn-only `tool_loop` model).
    let producer = ReasoningProducer {
        provider_slug: "anthropic".into(),
        model: "claude-unknown-model".into(),
    };
    add_turn(
        &mut session,
        "hello",
        "hi",
        Some(artifact(b"one")),
        Some(producer.clone()),
        vec![],
    );
    add_turn(
        &mut session,
        "again",
        "bye",
        Some(artifact(b"two")),
        Some(producer),
        vec![],
    );

    // Anthropic → AllTurns: every assistant message replays its artifact,
    // even non-tool turns.
    let result =
        build_chat_request_messages(&session, None, "anthropic", "claude-unknown-model", None);
    let assistants = assistant_messages(&result);
    assert_eq!(assistants.len(), 2);
    assert_eq!(assistants[0].reasoning_artifact, Some(artifact(b"one")));
    assert_eq!(assistants[1].reasoning_artifact, Some(artifact(b"two")));
}

#[test]
fn builder_signature_policy_attaches_always() {
    let mut session = SessionState::empty();
    add_turn(
        &mut session,
        "hello",
        "hi",
        Some(ReasoningArtifact::GoogleSignatures(b"sig".to_vec())),
        Some(ReasoningProducer {
            provider_slug: "google".into(),
            model: "gemini-2.5-pro".into(),
        }),
        vec![],
    );

    // Google → Signature: every assistant message replays the signatures.
    let result = build_chat_request_messages(&session, None, "google", "gemini-2.5-pro", None);
    let assistants = assistant_messages(&result);
    assert_eq!(assistants.len(), 1);
    assert_eq!(
        assistants[0].reasoning_artifact,
        Some(ReasoningArtifact::GoogleSignatures(b"sig".to_vec())),
    );
}

#[test]
fn builder_none_never_attaches() {
    let mut session = SessionState::empty();
    // A tool-involving turn WITH an artifact under a None-policy provider:
    // the artifact must never be replayed (display-only provider).
    add_turn(
        &mut session,
        "list",
        "thinking",
        Some(artifact(b"payload")),
        Some(ReasoningProducer {
            provider_slug: "unknown-provider".into(),
            model: "m".into(),
        }),
        vec![tool_call_record("call_1", "ls")],
    );

    let result = build_chat_request_messages(&session, None, "unknown-provider", "m", None);
    let assistants = assistant_messages(&result);
    assert_eq!(assistants.len(), 1);
    assert_eq!(assistants[0].reasoning_artifact, None);
}

#[test]
fn builder_response_id_policy_never_attaches_via_message() {
    let mut session = SessionState::empty();
    add_turn(
        &mut session,
        "list",
        "thinking",
        Some(artifact(b"payload")),
        Some(ReasoningProducer {
            provider_slug: "openai".into(),
            model: "gpt-4".into(),
        }),
        vec![tool_call_record("call_1", "ls")],
    );

    // gpt-4 is a Responses model → ResponseId policy: continuity flows via
    // previous_response_id, so the message must NOT carry the artifact.
    let result = build_chat_request_messages(&session, None, "openai", "gpt-4", None);
    let assistants = assistant_messages(&result);
    assert_eq!(assistants.len(), 1);
    assert_eq!(assistants[0].reasoning_artifact, None);
}

#[test]
fn builder_stateless_codex_provider_replays_captured_reasoning_items() {
    // The ChatGPT/Codex Responses backend honours no `previous_response_id` and
    // stores no transcript, so its models pin `all_turns` in the bundled
    // overlay: every assistant turn's preserved ResponsesItems artifact must
    // reach the adapter, which replays it verbatim into `input`. This is the
    // only reasoning-continuity channel such a request has.
    let mut session = SessionState::empty();
    add_turn(
        &mut session,
        "solve it",
        "done",
        Some(responses_items()),
        Some(ReasoningProducer {
            provider_slug: "chatgpt".into(),
            model: "gpt-5.6-luna".into(),
        }),
        vec![],
    );

    // A plain tool-less turn still replays — `all_turns` is unconditional, and
    // the tool loop's own turns are tool-involving anyway.
    let result = build_chat_request_messages(&session, None, "chatgpt", "gpt-5.6-luna", None);
    let assistants = assistant_messages(&result);
    assert_eq!(assistants.len(), 1);
    assert_eq!(
        assistants[0].reasoning_artifact,
        Some(responses_items()),
        "the stateless provider must replay every turn's captured items",
    );
}

#[test]
fn builder_same_model_mismatch_drops_artifact() {
    let mut session = SessionState::empty();
    // Current-model turn (deepseek): artifact kept.
    add_turn(
        &mut session,
        "list",
        "thinking",
        Some(artifact(b"kept")),
        Some(deepseek_producer()),
        vec![tool_call_record("call_1", "ls")],
    );
    // Turn produced by a DIFFERENT model mid-session (e.g. the user
    // switched deepseek → claude): the artifact is model-bound and must be
    // dropped even though the turn is tool-involving (replaying an
    // encrypted ChatReasoning payload into an Anthropic request — or a
    // stale deepseek payload after switching back — would corrupt the
    // conversation).
    add_turn(
        &mut session,
        "old model turn",
        "old thinking",
        Some(artifact(b"dropped")),
        Some(ReasoningProducer {
            provider_slug: "anthropic".into(),
            model: "claude-sonnet-4-5".into(),
        }),
        vec![tool_call_record("call_2", "grep")],
    );

    let result = build_chat_request_messages(&session, None, "deepseek", "deepseek-v4-pro", None);
    let assistants = assistant_messages(&result);
    assert_eq!(assistants.len(), 2);
    assert_eq!(assistants[0].reasoning_artifact, Some(artifact(b"kept")));
    assert_eq!(
        assistants[1].reasoning_artifact, None,
        "artifact from a previous model must be dropped",
    );
}

#[test]
fn builder_undone_turn_artifact_is_skipped() {
    let mut session = SessionState::empty();
    // Visible, tool-involving turn: its artifact must be replayed under
    // ToolLoop.
    add_turn(
        &mut session,
        "visible",
        "ok",
        Some(artifact(b"kept")),
        Some(deepseek_producer()),
        vec![tool_call_record("call_0", "pwd")],
    );
    let undone_tid = add_turn(
        &mut session,
        "hidden",
        "nope",
        Some(artifact(b"dropped")),
        Some(deepseek_producer()),
        vec![tool_call_record("call_1", "ls")],
    );
    session
        .turns
        .get_mut(&undone_tid)
        .expect("turn exists")
        .undone = true;

    let result = build_chat_request_messages(&session, None, "deepseek", "deepseek-v4-pro", None);
    let assistants = assistant_messages(&result);
    assert_eq!(assistants.len(), 1, "undone turn must be skipped entirely");
    assert_eq!(assistants[0].reasoning_artifact, Some(artifact(b"kept")));
}

// -- prev_resp_id persistence (phase 4c) --------------------------------

#[test]
fn stateless_chatgpt_neither_persists_nor_restores_a_response_id_chain() {
    // Persisting a returned chain id (the agent loop's two write sites) and
    // restoring one (`initial_prev_resp_id`) share this single predicate.
    assert!(
        chains_via_response_id("openai", "gpt-5.4"),
        "the standard Responses API chains via previous_response_id",
    );
    assert!(
        !chains_via_response_id("chatgpt", "gpt-5.6-luna"),
        "the stateless Codex provider replays reasoning items, it does not chain",
    );

    // Even with a matching producer recorded, a stateless route restores
    // nothing: an id it may have handed back means nothing to a later request.
    let mut session = SessionState::empty();
    session.config.last_response_id = Some("resp_1".into());
    session.config.last_response_id_producer = Some(ReasoningProducer {
        provider_slug: "chatgpt".into(),
        model: "gpt-5.6-luna".into(),
    });
    assert_eq!(
        initial_prev_resp_id(&session, "chatgpt", "gpt-5.6-luna"),
        None,
    );
}

#[test]
fn initial_prev_resp_id_response_policy_restores_persisted_id() {
    let mut session = SessionState::empty();
    session.config.last_response_id = Some("resp_123".into());
    session.config.last_response_id_producer = Some(ReasoningProducer {
        provider_slug: "openai".into(),
        model: "gpt-4".into(),
    });
    // gpt-4 is an OpenAI Responses model → ResponseId policy AND the
    // persisted id was produced by the same provider+model: the id must
    // be restored to chain reasoning continuity across user turns.
    assert_eq!(
        initial_prev_resp_id(&session, "openai", "gpt-4").as_deref(),
        Some("resp_123"),
    );
}

#[test]
fn initial_prev_resp_id_other_policies_reset_to_none() {
    let mut session = SessionState::empty();
    session.config.last_response_id = Some("resp_123".into());
    session.config.last_response_id_producer = Some(ReasoningProducer {
        provider_slug: "deepseek".into(),
        model: "deepseek-v4-pro".into(),
    });
    // DeepSeek chat → ToolLoop policy: a stale id must NOT leak into a
    // request that does not understand previous_response_id — even when
    // the provenance matches.
    assert_eq!(
        initial_prev_resp_id(&session, "deepseek", "deepseek-v4-pro"),
        None,
    );
    // Unknown provider → None policy.
    assert_eq!(
        initial_prev_resp_id(&session, "unknown-provider", "m"),
        None
    );
}

#[test]
fn initial_prev_resp_id_drops_stale_id_from_other_producer() {
    // The persisted id was produced by a DIFFERENT provider+model (e.g. a
    // mid-session openai → xAI switch): restoring it would replay a stale
    // previous_response_id into a service that does not recognize it →
    // provider 400. Provenance must gate the restore, exactly like
    // reasoning artifacts.
    let mut session = SessionState::empty();
    session.config.last_response_id = Some("resp_openai".into());
    session.config.last_response_id_producer = Some(ReasoningProducer {
        provider_slug: "openai".into(),
        model: "gpt-5.4".into(),
    });
    // Same provider, different model → dropped (model-bound provenance).
    assert_eq!(
        initial_prev_resp_id(&session, "openai", "gpt-4"),
        None,
        "id from gpt-5.4 must not be restored for gpt-4",
    );
    // Matching provider+model → restored.
    assert_eq!(
        initial_prev_resp_id(&session, "openai", "gpt-5.4").as_deref(),
        Some("resp_openai"),
    );
    // No producer recorded (fresh session) → no id is ever restored.
    let fresh = SessionState::empty();
    assert_eq!(initial_prev_resp_id(&fresh, "openai", "gpt-5.4"), None);
}

// -- Precondition guard (degraded-wire detection) -----------------------

#[test]
fn guard_flags_capture_loss_when_reasoning_displayed_without_artifact() {
    // The model reasoned, the session shows it, but no artifact was captured:
    // the echo degrades to an empty field. Nothing else about this turn is
    // unusual (non-empty content, no tool calls), so capture loss is the only
    // thing that can be flagged — and it must be.
    let mut session = SessionState::empty();
    let (tid, _) = session.start_turn(Some("list".into()));
    session.set_assistant_response(
        tid,
        AssistantResponse {
            text: Some("thinking".into()),
            reasoning: Some("displayed thoughts".into()),
            ..Default::default()
        },
    );
    assert_eq!(
        warn_on_uncaptured_reasoning(&session, 7, "deepseek", "deepseek-v4-pro"),
        1,
        "displayed reasoning with no artifact is a capture loss",
    );
}

#[test]
fn guard_clean_when_every_assistant_turn_has_its_artifact() {
    let mut session = SessionState::empty();
    add_turn(
        &mut session,
        "list",
        "thinking",
        Some(artifact(b"ok")),
        Some(deepseek_producer()),
        vec![tool_call_record("call_1", "ls")],
    );
    // A turn the model answered without thinking needs no echo at all.
    add_turn(
        &mut session,
        "plain",
        "hi",
        None,
        Some(deepseek_producer()),
        vec![],
    );
    assert_eq!(
        warn_on_uncaptured_reasoning(&session, 7, "deepseek", "deepseek-v4-pro"),
        0,
    );
}

#[test]
fn guard_flags_wire_empty_turn_without_artifact_on_any_policy() {
    // A reasoning-only turn whose artifact never made it into the session:
    // nothing can fill the assistant message, so it ships empty and the
    // provider rejects it. The check is protocol/policy independent — the
    // "must not be empty" rule is not a DeepSeek/Kimi one.
    let mut session = SessionState::empty();
    add_turn(
        &mut session,
        "continue",
        "",
        None,
        Some(anthropic_producer()),
        vec![],
    );
    for (provider, model) in [
        ("anthropic", "claude-sonnet-5"),
        ("deepseek", "deepseek-v4-pro"),
        ("openai", "gpt-4"),
    ] {
        assert_eq!(
            warn_on_uncaptured_reasoning(&session, 7, provider, model),
            1,
            "{provider}/{model}: a wire-empty assistant message is always a hazard",
        );
    }
}

#[test]
fn guard_flags_a_whitespace_only_assistant_turn() {
    // A blank answer is not a payload: the projection trims before deciding, so a
    // whitespace-only turn is dropped and the guard must report it.
    let mut session = SessionState::empty();
    add_turn(
        &mut session,
        "continue",
        "  \n\t ",
        None,
        Some(deepseek_producer()),
        vec![],
    );
    assert_eq!(
        warn_on_uncaptured_reasoning(&session, 7, "deepseek", "deepseek-v4-pro"),
        1,
    );
}

#[test]
fn guard_user_only_turn_is_not_flagged() {
    // A turn that never produced an assistant message (in-progress or
    // failed) has no artifact by construction and must not be flagged.
    let mut session = SessionState::empty();
    add_turn(
        &mut session,
        "list",
        "thinking",
        Some(artifact(b"ok")),
        Some(anthropic_producer()),
        vec![tool_call_record("call_1", "ls")],
    );
    let _ = session.start_turn(Some("pending user text".into()));
    assert_eq!(
        warn_on_uncaptured_reasoning(&session, 7, "anthropic", "claude-sonnet-5"),
        0,
    );
}

#[test]
fn guard_does_not_flag_model_bound_artifacts() {
    // A turn whose artifact was produced by a DIFFERENT model (a mid-session
    // switch) has a payload the builder will NOT replay (same-model
    // provenance). That is the intended strip-on-model-change rule, not a
    // defect: the message is still valid (content + tool calls), so the guard
    // stays quiet.
    let mut session = SessionState::empty();
    add_turn(
        &mut session,
        "list",
        "thinking",
        Some(artifact(b"ok")),
        Some(deepseek_producer()),
        vec![tool_call_record("call_1", "ls")],
    );
    assert_eq!(
        warn_on_uncaptured_reasoning(&session, 7, "anthropic", "claude-sonnet-5"),
        0,
        "a model-bound artifact is dropped by design, not a capture loss",
    );
    assert_eq!(
        warn_on_uncaptured_reasoning(&session, 7, "deepseek", "deepseek-v4-pro"),
        0,
    );
}

#[test]
fn guard_ignores_the_route_echo_policy() {
    // Whether the route declares a reasoning field is the adapter's business;
    // a turn that has nothing to echo and a valid message is clean either way.
    let mut session = SessionState::empty();
    add_turn(
        &mut session,
        "list",
        "thinking",
        None,
        None,
        vec![tool_call_record("call_1", "ls")],
    );
    assert_eq!(
        warn_on_uncaptured_reasoning(&session, 7, "openai", "gpt-4"),
        0,
    );
    assert_eq!(
        warn_on_uncaptured_reasoning(&session, 7, "deepseek", "deepseek-v4-pro"),
        0,
    );
}

// -- Concurrent tool status label tests --------------------------------
