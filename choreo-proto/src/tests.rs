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
fn decode_rejects_trailing_bytes() {
    let message = ClientMessage::Ping;
    let mut frame = encode_frame(&message).expect("encode");
    frame.extend_from_slice(&[1, 2, 3]);
    let err = decode_frame::<ClientMessage>(&frame[4..]).expect_err("should fail");
    assert!(matches!(err, ProtoError::TrailingBytes));
}

#[test]
fn decode_rejects_wrong_version() {
    let payload =
        postcard::to_allocvec(&(PROTOCOL_VERSION + 1, ClientMessage::Ping)).expect("encode");
    let err = decode_frame::<ClientMessage>(&payload).expect_err("should fail");
    assert!(matches!(err, ProtoError::UnsupportedVersion { .. }));
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
    // `token_usage` on Done is optional (`#[serde(default)]`): a payload that
    // omits it must parse and default to None.
    let json = r#"{"Done":{"session_id":1,"request_id":42}}"#;
    let msg: DaemonMessage = serde_json::from_str(json).unwrap();
    match msg {
        DaemonMessage::Done {
            request_id,
            token_usage,
            ..
        } => {
            assert_eq!(request_id, 42);
            assert_eq!(token_usage, None);
        }
        _ => panic!("expected Done"),
    }
}

#[test]
fn daemon_message_done_without_usage() {
    let msg = DaemonMessage::Done {
        session_id: 1,
        request_id: 7,
        token_usage: None,
        last_prompt_tokens: None,
    };
    match msg {
        DaemonMessage::Done {
            request_id,
            token_usage,
            ..
        } => {
            assert_eq!(request_id, 7);
            assert_eq!(token_usage, None);
        }
        _ => panic!("expected Done"),
    }
}

#[test]
fn daemon_message_done_with_usage_round_trip() {
    let usage = TokenUsage {
        input_tokens: 100,
        output_tokens: 50,
        total_tokens: 150,
    };
    let msg = DaemonMessage::Done {
        session_id: 1,
        request_id: 3,
        token_usage: Some(usage),
        last_prompt_tokens: None,
    };
    let frame = encode_frame(&msg).expect("encode");
    let decoded: DaemonMessage = decode_frame(&frame[4..]).expect("decode");
    match decoded {
        DaemonMessage::Done {
            request_id,
            token_usage,
            ..
        } => {
            assert_eq!(request_id, 3);
            assert_eq!(token_usage, Some(usage));
        }
        _ => panic!("expected Done"),
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
    let state = DaemonMessage::SessionState {
        session_id: 1,
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

// ── TurnAppended / TurnFinalized round-trip tests ───────────────────────

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
    let msg = DaemonMessage::TurnAppended {
        session_id: 1,
        turn_id: 1,
        turn: turn.clone(),
    };
    let frame = encode_frame(&msg).expect("encode");
    let decoded: DaemonMessage = decode_frame(&frame[4..]).expect("decode");
    assert_eq!(decoded, msg);
}

#[test]
fn turn_finalized_serde_round_trip() {
    let turn = Turn {
        created_at: TimestampMs::now(),
        undone: false,
        error: None,
        user_text: Some("hello".into()),
        assistant_text: Some("response".into()),
        assistant_reasoning: Some("thinking".into()),
        tool_calls: vec![],
        token_usage: Some(TokenUsage {
            input_tokens: 10,
            output_tokens: 20,
            total_tokens: 30,
        }),
        tool_results: vec![],
        displayed_images: vec![],
        reasoning_artifact: None,
        reasoning_producer: None,
    };
    let msg = DaemonMessage::TurnFinalized {
        session_id: 1,
        turn_id: 2,
        turn: turn.clone(),
    };
    let frame = encode_frame(&msg).expect("encode");
    let decoded: DaemonMessage = decode_frame(&frame[4..]).expect("decode");
    assert_eq!(decoded, msg);
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
