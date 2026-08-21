use super::*;
use std::collections::{BTreeMap, HashSet};
use std::io::Cursor;

#[test]
fn encode_decode_round_trip_client_message() {
    let message = ClientMessage::RunInput {
        request_id: 42,
        input: b"hello".to_vec(),
    };
    let frame = encode_frame(&message).expect("encode");
    let decoded = decode_frame::<ClientMessage>(&frame[4..]).expect("decode");
    assert_eq!(decoded, message);
}

#[test]
fn undo_serde_round_trip() {
    let message = ClientMessage::Undo;
    let frame = encode_frame(&message).expect("encode");
    let decoded = decode_frame::<ClientMessage>(&frame[4..]).expect("decode");
    assert_eq!(decoded, message);
}

#[test]
fn redo_serde_round_trip() {
    let message = ClientMessage::Redo;
    let frame = encode_frame(&message).expect("encode");
    let decoded = decode_frame::<ClientMessage>(&frame[4..]).expect("decode");
    assert_eq!(decoded, message);
}

#[test]
fn continue_generation_serde_round_trip() {
    let message = ClientMessage::ContinueGeneration { request_id: 7 };
    let frame = encode_frame(&message).expect("encode");
    let decoded = decode_frame::<ClientMessage>(&frame[4..]).expect("decode");
    assert_eq!(decoded, message);
}

#[test]
fn refresh_models_serde_round_trip() {
    for force in [false, true] {
        let message = ClientMessage::RefreshModels { force };
        let frame = encode_frame(&message).expect("encode");
        let decoded = decode_frame::<ClientMessage>(&frame[4..]).expect("decode");
        assert_eq!(decoded, message);
    }
}

#[test]
fn refresh_messages_serde_round_trip() {
    // DaemonMessage replies: every RefreshStatus variant + the failure arm.
    for status in [
        RefreshStatus::UpToDate,
        RefreshStatus::Updated,
        RefreshStatus::Forced,
    ] {
        let message = DaemonMessage::ModelsRefreshed {
            providers: 208,
            models: 1234,
            status,
        };
        let frame = encode_frame(&message).expect("encode");
        let decoded = decode_frame::<DaemonMessage>(&frame[4..]).expect("decode");
        assert_eq!(decoded, message);
    }
    let failed = DaemonMessage::ModelsRefreshFailed {
        error: "network error".to_string(),
    };
    let frame = encode_frame(&failed).expect("encode");
    let decoded = decode_frame::<DaemonMessage>(&frame[4..]).expect("decode");
    assert_eq!(decoded, failed);

    // CatalogUpdated carries the provider list (slugs + display names).
    let updated = DaemonMessage::CatalogUpdated {
        providers: vec![
            CatalogProvider {
                slug: "openai".to_string(),
                display_name: "OpenAI".to_string(),
            },
            CatalogProvider {
                slug: "ollama".to_string(),
                display_name: "Ollama (Local)".to_string(),
            },
        ],
    };
    let frame = encode_frame(&updated).expect("encode");
    let decoded = decode_frame::<DaemonMessage>(&frame[4..]).expect("decode");
    assert_eq!(decoded, updated);
}

#[test]
fn decode_rejects_trailing_bytes() {
    let message = ClientMessage::Ping;
    let mut frame = encode_frame(&message).expect("encode");
    frame.extend_from_slice(&[1, 2, 3]);
    let err = decode_frame::<ClientMessage>(&frame[4..]).expect_err("should fail");
    assert!(matches!(err, ProtoError::TrailingBytes));
}

#[test]
fn decode_rejects_wrong_version() {
    // Encode with the *current* named-MessagePack semantics but one version
    // ahead — what a newer peer would send. Must be rejected up front.
    let payload =
        rmp_serde::to_vec_named(&(PROTOCOL_VERSION + 1, ClientMessage::Ping)).expect("encode");
    let err = decode_frame::<ClientMessage>(&payload).expect_err("should fail");
    assert!(matches!(err, ProtoError::UnsupportedVersion { .. }));
}

#[test]
fn decode_rejects_wrong_version_before_parsing_body() {
    // The envelope is `[0x92, version, body]`. A wrong version must be
    // rejected from the version byte alone — the message body is never
    // deserialized, so bytes from a protocol this binary does not understand
    // (here: garbage that could never parse as ClientMessage) still produce a
    // clean UnsupportedVersion rather than a codec error.
    let mut payload = vec![0x92, PROTOCOL_VERSION + 1];
    payload.extend_from_slice(&[0xff, 0xff, 0xff]); // unrecognized body
    let err = decode_frame::<ClientMessage>(&payload).expect_err("should fail");
    match err {
        ProtoError::UnsupportedVersion { version } => {
            assert_eq!(version, PROTOCOL_VERSION + 1);
        }
        other => panic!("expected UnsupportedVersion, got {other:?}"),
    }
}

#[test]
fn decode_tolerates_array_encoded_struct() {
    // Named mode writes structs as maps with field-name keys, but decode also
    // accepts the array (field-order) form — that is the compatibility
    // contract that keeps a future switch to compact mode backwards-readable.
    // Hand-build `[4, [10, 20, 30]]`: version 4, then a `TokenUsage` struct
    // serialized WITHOUT field names as a 3-element array.
    let blob = [
        0x92, // array of 2: (version, message)
        0x04, // PROTOCOL_VERSION = 4
        0x93, // array of 3: TokenUsage { input_tokens, output_tokens, total_tokens }
        0x0a, // input_tokens = 10
        0x14, // output_tokens = 20
        0x1e, // total_tokens = 30
    ];
    let decoded: TokenUsage = decode_frame(&blob).expect("decode");
    assert_eq!(
        decoded,
        TokenUsage {
            input_tokens: 10,
            output_tokens: 20,
            total_tokens: 30,
        }
    );
}

#[test]
fn sync_read_write_round_trip() {
    let expected = DaemonMessage::Pong;

    let frame = encode_frame(&expected).expect("encode");
    let mut cursor = Cursor::new(&frame[..]);
    let actual = read_message::<_, DaemonMessage>(&mut cursor).expect("read");
    assert_eq!(actual, expected);
}

#[test]
fn read_payload_rejects_oversized_frame() {
    let oversized_len = (MAX_FRAME_SIZE as u32) + 1;
    let mut cursor = Cursor::new(oversized_len.to_be_bytes().to_vec());
    let err = read_payload(&mut cursor).expect_err("should fail");
    assert!(matches!(err, ProtoError::FrameTooLarge));
}

#[test]
fn socket_path_uses_env_override() {
    assert_eq!(
        crate::io::socket_path_impl(|| Some("/tmp/custom-choreographr.sock".to_string())),
        "/tmp/custom-choreographr.sock"
    );
}

#[test]
fn socket_path_default_when_env_not_set() {
    assert_eq!(crate::io::socket_path_impl(|| None), DEFAULT_SOCKET_PATH);
}

#[test]
fn encode_rejects_oversized_message() {
    let message = ClientMessage::RunInput {
        request_id: 1,
        input: vec![0; MAX_FRAME_SIZE],
    };
    let err = encode_frame(&message).expect_err("should fail");
    assert!(matches!(err, ProtoError::FrameTooLarge));
}

#[test]
fn session_status_retrying_serde_round_trip() {
    let status = SessionStatus::Retrying {
        attempt: 2,
        max_attempts: 5,
        delay_ms: 3000,
    };
    let frame = encode_frame(&status).expect("encode");
    let decoded: SessionStatus = decode_frame(&frame[4..]).expect("decode");
    assert_eq!(decoded, status);
}

// ── TimestampMs tests ──────────────────────────────────────────────────

#[test]
fn timestamp_ms_now_returns_recent() {
    let ts = TimestampMs::now();
    assert!(
        ts.as_millis() > 1_577_836_800_000,
        "TimestampMs::now() should return a recent timestamp"
    );
}

#[test]
fn timestamp_ms_serde_deterministic() {
    let ts = TimestampMs::now();
    let millis = ts.as_millis();
    let frame = encode_frame(&ts).expect("encode");
    let decoded: TimestampMs = decode_frame(&frame[4..]).expect("decode");
    assert_eq!(decoded.as_millis(), millis);
}

#[test]
fn timestamp_ms_serde_round_trip() {
    let ts = TimestampMs::now();
    let frame = encode_frame(&ts).expect("encode");
    let decoded: TimestampMs = decode_frame(&frame[4..]).expect("decode");
    assert_eq!(decoded, ts);
}

// ── Turn tests ─────────────────────────────────────────────────────────

#[test]
fn turn_serde_round_trip() {
    let turn = Turn {
        created_at: TimestampMs::now(),
        undone: false,
        error: None,
        user_text: Some("hello".into()),
        assistant_text: Some("hi".into()),
        assistant_reasoning: None,
        tool_calls: vec![],
        token_usage: None,
        tool_results: vec![],
        displayed_images: vec![],
        reasoning_artifact: None,
        reasoning_producer: None,
    };
    let frame = encode_frame(&turn).expect("encode");
    let decoded: Turn = decode_frame(&frame[4..]).expect("decode");
    assert_eq!(decoded, turn);
}

#[test]
fn tool_result_record_serde_round_trip() {
    let record = ToolResultRecord {
        call_id: "call_1".into(),
        name: "ls".into(),
        content: "file.txt".into(),
        is_error: false,
        invocation_description: String::new(),
    };
    let frame = encode_frame(&record).expect("encode");
    let decoded: ToolResultRecord = decode_frame(&frame[4..]).expect("decode");
    assert_eq!(decoded, record);
}

#[test]
fn turn_with_tool_results_round_trip() {
    let turn = Turn {
        created_at: TimestampMs::now(),
        undone: false,
        error: None,
        user_text: Some("list files".into()),
        assistant_text: None,
        assistant_reasoning: None,
        tool_calls: vec![AssistantToolCallRecord {
            call_id: "call_1".into(),
            name: "ls".into(),
            arguments_json: "{}".into(),
        }],
        token_usage: Some(TokenUsage {
            input_tokens: 10,
            output_tokens: 20,
            total_tokens: 30,
        }),
        tool_results: vec![ToolResultRecord {
            call_id: "call_1".into(),
            name: "ls".into(),
            content: "file.txt".into(),
            is_error: false,
            invocation_description: String::new(),
        }],
        displayed_images: vec![],
        reasoning_artifact: None,
        reasoning_producer: None,
    };
    let frame = encode_frame(&turn).expect("encode");
    let decoded: Turn = decode_frame(&frame[4..]).expect("decode");
    assert_eq!(decoded, turn);
}

// ── TokenUsage tests ──────────────────────────────────────────────────

#[test]
fn token_usage_default_is_zero() {
    let u = TokenUsage::default();
    assert_eq!(u.input_tokens, 0);
    assert_eq!(u.output_tokens, 0);
    assert_eq!(u.total_tokens, 0);
}

#[test]
fn token_usage_serde_round_trip() {
    let usage = TokenUsage {
        input_tokens: 150,
        output_tokens: 75,
        total_tokens: 225,
    };
    let frame = encode_frame(&usage).expect("encode");
    let decoded: TokenUsage = decode_frame(&frame[4..]).expect("decode");
    assert_eq!(decoded, usage);
}

#[test]
fn session_summary_tolerates_missing_and_unknown_fields() {
    // `token_usage` and the other trailing optional fields use
    // `#[serde(default)]`, so a payload that omits them must still parse — that
    // is the contract that lets the protocol grow additively. `max_turns` was
    // removed from SessionSummary; serde ignores unknown fields by default, so a
    // payload that still carries it must parse too. This fixture pins both
    // behaviors.
    let json = r#"{"session_id":1,"title":null,"selected_model":null,"reasoning_effort":null,"parent_session_id":null,"working_dir":null,"created_at":0,"last_modified":0,"turn_count":0,"max_turns":null,"status":"Inactive","active_tool_groups":[],"account_name":null}"#;
    let summary: SessionSummary = serde_json::from_str(json).unwrap();
    assert_eq!(summary.session_id, 1);
    assert_eq!(summary.token_usage, None);
}

#[test]
fn done_tolerates_missing_optional_fields() {
    // `token_usage` on the session-scoped `Done` event is optional: a payload
    // that omits it (and the `#[serde(default)]` `last_prompt_tokens`) must
    // parse and default to None. The wire shape is now the v4 envelope —
    // `DaemonMessage::Session { session_id, event: SessionEvent::Done }` — so
    // the fixture nests the event inside the envelope.
    let json = r#"{"Session":{"session_id":1,"event":{"Done":{"request_id":42}}}}"#;
    let msg: DaemonMessage = serde_json::from_str(json).unwrap();
    match msg {
        DaemonMessage::Session {
            session_id,
            event:
                SessionEvent::Done {
                    request_id,
                    token_usage,
                    ..
                },
        } => {
            assert_eq!(session_id, Some(1));
            assert_eq!(request_id, 42);
            assert_eq!(token_usage, None);
        }
        _ => panic!("expected Session(Done)"),
    }

    // The same shape must round-trip through the actual v4 frame codec too
    // (the version gate stays consistent because all constants are local).
    let msg = DaemonMessage::Session {
        session_id: Some(1),
        event: SessionEvent::Done {
            request_id: 42,
            token_usage: None,
            last_prompt_tokens: None,
        },
    };
    let frame = encode_frame(&msg).expect("encode");
    let decoded: DaemonMessage = decode_frame(&frame[4..]).expect("decode");
    assert_eq!(decoded, msg);
}

#[test]
fn daemon_message_done_without_usage() {
    let msg = DaemonMessage::Session {
        session_id: Some(1),
        event: SessionEvent::Done {
            request_id: 7,
            token_usage: None,
            last_prompt_tokens: None,
        },
    };
    match msg {
        DaemonMessage::Session {
            session_id,
            event:
                SessionEvent::Done {
                    request_id,
                    token_usage,
                    ..
                },
        } => {
            assert_eq!(session_id, Some(1));
            assert_eq!(request_id, 7);
            assert_eq!(token_usage, None);
        }
        _ => panic!("expected Session(Done)"),
    }
}

#[test]
fn daemon_message_done_with_usage_round_trip() {
    let usage = TokenUsage {
        input_tokens: 100,
        output_tokens: 50,
        total_tokens: 150,
    };
    let msg = DaemonMessage::Session {
        session_id: Some(1),
        event: SessionEvent::Done {
            request_id: 3,
            token_usage: Some(usage),
            last_prompt_tokens: None,
        },
    };
    let frame = encode_frame(&msg).expect("encode");
    let decoded: DaemonMessage = decode_frame(&frame[4..]).expect("decode");
    match decoded {
        DaemonMessage::Session {
            session_id,
            event:
                SessionEvent::Done {
                    request_id,
                    token_usage,
                    ..
                },
        } => {
            assert_eq!(session_id, Some(1));
            assert_eq!(request_id, 3);
            assert_eq!(token_usage, Some(usage));
        }
        _ => panic!("expected Session(Done)"),
    }
}

// ── Postcard round-trip with None optionals ─────────────────────────────

#[test]
fn session_summary_none_optionals_round_trip() {
    let summary = SessionSummary {
        session_id: 1,
        title: Some("test".into()),
        selected_model: None,
        reasoning_effort: None,
        parent_session_id: None,
        working_dir: None,
        created_at: 0,
        last_modified: 0,
        turn_count: 0,
        status: SessionStatus::Inactive,
        active_tool_groups: vec![],
        account_name: None,
        token_usage: None,
        context_window: None,
        last_prompt_tokens: None,
    };
    let frame = encode_frame(&summary).expect("encode");
    let decoded: SessionSummary = decode_frame(&frame[4..]).expect("decode");
    assert_eq!(decoded, summary);
}

#[test]
fn session_summary_some_token_usage_round_trip() {
    let usage = TokenUsage {
        input_tokens: 10,
        output_tokens: 20,
        total_tokens: 30,
    };
    let summary = SessionSummary {
        session_id: 2,
        title: None,
        selected_model: None,
        reasoning_effort: None,
        parent_session_id: None,
        working_dir: None,
        created_at: 0,
        last_modified: 0,
        turn_count: 0,
        status: SessionStatus::Inactive,
        active_tool_groups: vec![],
        account_name: None,
        token_usage: Some(usage),
        context_window: None,
        last_prompt_tokens: None,
    };
    let frame = encode_frame(&summary).expect("encode");
    let decoded: SessionSummary = decode_frame(&frame[4..]).expect("decode");
    assert_eq!(decoded.token_usage, Some(usage));
}

#[test]
fn session_state_none_optionals_round_trip() {
    let state = DaemonMessage::Session {
        session_id: Some(1),
        event: SessionEvent::SessionState {
            title: None,
            selected_model: None,
            parent_session_id: None,
            working_dir: None,
            turns: BTreeMap::new(),
            active_tool_groups: vec![],
            token_usage: None,
            context_window: None,
            last_prompt_tokens: None,
            status: SessionStatus::Inactive,
            reasoning_effort: None,
            reasoning_capability: None,
        },
    };
    let frame = encode_frame(&state).expect("encode");
    let decoded: DaemonMessage = decode_frame(&frame[4..]).expect("decode");
    assert_eq!(decoded, state);
}

#[test]
fn sessions_with_none_optionals_round_trip() {
    let summary = SessionSummary {
        session_id: 1,
        title: None,
        selected_model: None,
        reasoning_effort: None,
        parent_session_id: None,
        working_dir: None,
        created_at: 0,
        last_modified: 0,
        turn_count: 0,
        status: SessionStatus::Inactive,
        active_tool_groups: vec![],
        account_name: None,
        token_usage: None,
        context_window: None,
        last_prompt_tokens: None,
    };
    let msg = DaemonMessage::Sessions {
        sessions: vec![summary.clone(), summary],
    };
    let frame = encode_frame(&msg).expect("encode");
    let decoded: DaemonMessage = decode_frame(&frame[4..]).expect("decode");
    assert_eq!(decoded, msg);
}

// ── ReasoningCapability tests ─────────────────────────────────────────────

#[test]
fn reasoning_capability_cycle_from_basic() {
    let cap = ReasoningCapability {
        available_effort_levels: vec!["off".into(), "low".into(), "medium".into(), "high".into()],
    };
    assert_eq!(cap.cycle_from("off"), Some("low".to_string()));
    assert_eq!(cap.cycle_from("low"), Some("medium".to_string()));
    assert_eq!(cap.cycle_from("medium"), Some("high".to_string()));
    assert_eq!(cap.cycle_from("high"), Some("off".to_string()));
}

#[test]
fn reasoning_capability_cycle_from_empty() {
    let cap = ReasoningCapability {
        available_effort_levels: vec![],
    };
    assert_eq!(cap.cycle_from("off"), None);
}

#[test]
fn reasoning_capability_cycle_from_unknown_starts_at_zero() {
    let cap = ReasoningCapability {
        available_effort_levels: vec!["off".into(), "low".into(), "medium".into(), "high".into()],
    };
    // Unknown current slug starts at index 0, cycle_from returns the next ("low")
    assert_eq!(cap.cycle_from("unknown"), Some("low".to_string()));
}

#[test]
fn reasoning_capability_serde_round_trip() {
    let cap = ReasoningCapability {
        available_effort_levels: vec!["off".into(), "on".into()],
    };
    let frame = encode_frame(&cap).expect("encode");
    let decoded: ReasoningCapability = decode_frame(&frame[4..]).expect("decode");
    assert_eq!(decoded, cap);
}

// ── TurnAppended / Evicted round-trip tests ───────────────────────

#[test]
fn turn_appended_serde_round_trip() {
    let turn = Turn {
        created_at: TimestampMs::now(),
        undone: false,
        error: None,
        user_text: Some("hello".into()),
        assistant_text: None,
        assistant_reasoning: None,
        tool_calls: vec![],
        token_usage: None,
        tool_results: vec![],
        displayed_images: vec![],
        reasoning_artifact: None,
        reasoning_producer: None,
    };
    let msg = DaemonMessage::Session {
        session_id: Some(1),
        event: SessionEvent::TurnAppended {
            turn_id: 1,
            turn: turn.clone(),
        },
    };
    let frame = encode_frame(&msg).expect("encode");
    let decoded: DaemonMessage = decode_frame(&frame[4..]).expect("decode");
    assert_eq!(decoded, msg);
}

#[test]
fn evicted_serde_round_trip() {
    // Evicted is a unit variant (best-effort lag-eviction advisory): it must
    // round-trip through the wire format. Origin-session attribution for the
    // activity-broadcast dedup is now carried explicitly on the broadcast
    // command, not derived from the message, so no session_id assertion here.
    let msg = DaemonMessage::Evicted;
    let frame = encode_frame(&msg).expect("encode");
    let decoded: DaemonMessage = decode_frame(&frame[4..]).expect("decode");
    assert_eq!(decoded, DaemonMessage::Evicted);
}

// ── approx_wire_size tests ─────────────────────────────────────

#[test]
fn approx_wire_size_scales_with_payload() {
    // A variant carrying a 100-byte String must estimate at least 100 bytes:
    // the string payload itself dominates the serialized size.
    let msg = DaemonMessage::Session {
        session_id: Some(1),
        event: SessionEvent::Failed {
            request_id: 1,
            error: "x".repeat(100),
        },
    };
    assert!(msg.approx_wire_size() >= 100);

    // A turn-bearing variant must track the turn's own estimate (a 100-byte
    // assistant_text inside the turn).
    let turn = Turn {
        created_at: TimestampMs::now(),
        undone: false,
        error: None,
        user_text: Some("hi".into()),
        assistant_text: Some("x".repeat(100)),
        assistant_reasoning: None,
        tool_calls: vec![],
        token_usage: None,
        tool_results: vec![],
        displayed_images: vec![],
        reasoning_artifact: None,
        reasoning_producer: None,
    };
    let turn_size = turn.approx_size();
    assert!(turn_size >= 100);
    let msg = DaemonMessage::Session {
        session_id: Some(1),
        event: SessionEvent::TurnAppended {
            turn_id: 1,
            turn: turn.clone(),
        },
    };
    assert!(msg.approx_wire_size() >= turn_size);
    // A second copy of the same turn must not change the estimate.
    let msg2 = DaemonMessage::Session {
        session_id: Some(1),
        event: SessionEvent::TurnAppended { turn_id: 1, turn },
    };
    assert_eq!(msg.approx_wire_size(), msg2.approx_wire_size());
}

#[test]
fn approx_wire_size_empty_variants_are_small_positive() {
    // Empty/unit variants must still report a small positive fixed envelope,
    // so lag accounting never sees a zero-byte message.
    assert!(DaemonMessage::Pong.approx_wire_size() > 0);
    assert!(DaemonMessage::Pong.approx_wire_size() < 128);
    assert!(DaemonMessage::Evicted.approx_wire_size() > 0);
    assert!(DaemonMessage::Evicted.approx_wire_size() < 128);
}

// ── InferenceError metric_label tests ─────────────────────────

#[test]
fn inference_error_metric_labels_are_stable() {
    assert_eq!(
        InferenceError::Unauthorized {
            status: 401,
            detail: "x".into()
        }
        .metric_label(),
        "unauthorized"
    );
    assert_eq!(
        InferenceError::RateLimited {
            status: 429,
            retry_after_secs: None,
            detail: "x".into()
        }
        .metric_label(),
        "rate_limited"
    );
    assert_eq!(
        InferenceError::ServerError {
            status: 500,
            detail: "x".into()
        }
        .metric_label(),
        "server_error"
    );
    assert_eq!(
        InferenceError::ClientError {
            status: 400,
            detail: "x".into()
        }
        .metric_label(),
        "client_error"
    );
    assert_eq!(
        InferenceError::EmptyResponse.metric_label(),
        "empty_response"
    );
    assert_eq!(InferenceError::Cancelled.metric_label(), "cancelled");
    assert_eq!(
        InferenceError::DeadlineExceeded.metric_label(),
        "deadline_exceeded"
    );
    assert_eq!(
        InferenceError::TruncatedToolCall {
            discarded: vec![DiscardedToolCall {
                name: "t".into(),
                arguments_json: "{}".into()
            }]
        }
        .metric_label(),
        "truncated_tool_call"
    );
    assert_eq!(
        InferenceError::Io(std::io::Error::other("oops")).metric_label(),
        "other"
    );
}

#[test]
fn inference_error_metric_labels_are_distinct() {
    // Every variant must map to a unique label. Collisions would silently
    // merge distinct error classes in the Prometheus counters, hiding real
    // failure modes behind a single `error_type` value.
    let labels: HashSet<&str> = [
        InferenceError::Unauthorized {
            status: 401,
            detail: "x".into(),
        }
        .metric_label(),
        InferenceError::RateLimited {
            status: 429,
            retry_after_secs: None,
            detail: "x".into(),
        }
        .metric_label(),
        InferenceError::ServerError {
            status: 500,
            detail: "x".into(),
        }
        .metric_label(),
        InferenceError::ClientError {
            status: 400,
            detail: "x".into(),
        }
        .metric_label(),
        InferenceError::EmptyResponse.metric_label(),
        InferenceError::Cancelled.metric_label(),
        InferenceError::DeadlineExceeded.metric_label(),
        InferenceError::TruncatedToolCall { discarded: vec![] }.metric_label(),
        InferenceError::Io(std::io::Error::other("oops")).metric_label(),
    ]
    .into_iter()
    .collect();
    assert_eq!(
        labels.len(),
        9,
        "each InferenceError variant must have a distinct metric label"
    );
}
