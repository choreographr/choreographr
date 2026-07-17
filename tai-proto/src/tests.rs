use super::*;
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
    let expected = DaemonMessage::ImageStart {
        request_id: 5,
        metadata: ImageMetadata {
            image_id: 1,
            mime_type: "image/png".to_string(),
            width: 1,
            height: 1,
            byte_len: 4,
            alt: Some("chunk".to_string()),
        },
    };

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
        crate::io::socket_path_impl(|| Some("/tmp/custom-tai.sock".to_string())),
        "/tmp/custom-tai.sock"
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
    // Must be after 2020-01-01 (well into the past as of 2026).
    assert!(
        ts.as_millis() > 1_577_836_800_000,
        "TimestampMs::now() should return a recent timestamp"
    );
}

#[test]
fn timestamp_ms_serde_deterministic() {
    let ts = TimestampMs::now();
    let millis = ts.as_millis();
    // Serialize and deserialize, then verify the value survives.
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

// ── SessionMessage tests ──────────────────────────────────────────────

#[test]
fn session_message_now_sets_timestamp() {
    let before = TimestampMs::now();
    let msg = SessionMessage::now(SessionMessageKind::SystemText {
        content: "test".into(),
    });
    let after = TimestampMs::now();
    // The created_at must be between before and after.
    assert!(
        msg.created_at.as_millis() >= before.as_millis(),
        "created_at should be >= timestamp taken before construction"
    );
    assert!(
        msg.created_at.as_millis() <= after.as_millis(),
        "created_at should be <= timestamp taken after construction"
    );
}

#[test]
fn session_message_kind_round_trip() {
    let msg = SessionMessage::now(SessionMessageKind::AssistantText {
        content: "hello".into(),
        reasoning: None,
        token_usage: None,
    });
    let frame = encode_frame(&msg).expect("encode");
    let decoded: SessionMessage = decode_frame(&frame[4..]).expect("decode");
    // Compare .kind only; .created_at may differ by a few ms.
    assert_eq!(decoded.kind, msg.kind);
}

#[test]
fn session_message_assistant_text_fields() {
    let msg = SessionMessage::now(SessionMessageKind::AssistantText {
        content: "hello".into(),
        reasoning: None,
        token_usage: None,
    });
    match &msg.kind {
        SessionMessageKind::AssistantText {
            content,
            reasoning,
            token_usage,
        } => {
            assert_eq!(content, "hello");
            assert_eq!(*reasoning, None);
            assert_eq!(*token_usage, None);
        }
        _ => panic!("expected AssistantText"),
    }
}

#[test]
fn session_message_all_variants_round_trip() {
    let variants: Vec<SessionMessageKind> = vec![
        SessionMessageKind::SystemText {
            content: "sys".into(),
        },
        SessionMessageKind::UserText {
            content: "user".into(),
        },
        SessionMessageKind::AssistantText {
            content: "assistant".into(),
            reasoning: None,
            token_usage: None,
        },
        SessionMessageKind::AssistantToolUse {
            content: Some("thinking".into()),
            tool_calls: vec![],
            reasoning: None,
            token_usage: None,
        },
        SessionMessageKind::ToolResult {
            call_id: "c1".into(),
            name: "ls".into(),
            content: "file.txt".into(),
            is_error: false,
        },
        SessionMessageKind::DisplayedImage(DisplayedImageRecord {
            metadata: ImageMetadata {
                image_id: 0,
                mime_type: "image/png".into(),
                width: 1,
                height: 1,
                byte_len: 0,
                alt: None,
            },
            data: vec![],
            tool_call_id: None,
        }),
    ];
    for kind in variants {
        let msg = SessionMessage::now(kind);
        let frame = encode_frame(&msg).expect("encode");
        let decoded: SessionMessage = decode_frame(&frame[4..]).expect("decode");
        assert_eq!(decoded.kind, msg.kind, "round-trip failed for variant");
    }
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
fn token_usage_in_session_summary_backward_compat() {
    // Old JSON (before token_usage was added to SessionSummary)
    let json = r#"{"session_id":1,"title":null,"selected_model":null,"reasoning_effort":null,"parent_session_id":null,"working_dir":null,"created_at":0,"message_count":0,"max_turns":null,"status":"Inactive","active_tool_groups":[],"account_name":null}"#;
    let summary: SessionSummary = serde_json::from_str(json).unwrap();
    assert_eq!(summary.session_id, 1);
    assert_eq!(summary.token_usage, None);
}

#[test]
fn token_usage_in_daemon_message_done_backward_compat() {
    // Old JSON (before token_usage was added to Done)
    let json = r#"{"Done":{"request_id":42}}"#;
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
        request_id: 3,
        token_usage: Some(usage.clone()),
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
// These verify that postcard handles trailing Option fields correctly when
// they are None — a regression test for the skip_serializing_if bug.

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
        message_count: 0,
        max_turns: None,
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
        message_count: 0,
        max_turns: None,
        status: SessionStatus::Inactive,
        active_tool_groups: vec![],
        account_name: None,
        token_usage: Some(usage.clone()),
        context_window: None,
        last_prompt_tokens: None,
    };
    let frame = encode_frame(&summary).expect("encode");
    let decoded: SessionSummary = decode_frame(&frame[4..]).expect("decode");
    assert_eq!(decoded.token_usage, Some(usage));
}

#[test]
fn assistant_text_none_token_usage_round_trip() {
    let msg = SessionMessage::now(SessionMessageKind::AssistantText {
        content: "hello".into(),
        reasoning: None,
        token_usage: None,
    });
    let frame = encode_frame(&msg).expect("encode");
    let decoded: SessionMessage = decode_frame(&frame[4..]).expect("decode");
    assert_eq!(decoded.kind, msg.kind);
}

#[test]
fn assistant_tool_use_none_token_usage_round_trip() {
    let msg = SessionMessage::now(SessionMessageKind::AssistantToolUse {
        content: None,
        tool_calls: vec![],
        reasoning: None,
        token_usage: None,
    });
    let frame = encode_frame(&msg).expect("encode");
    let decoded: SessionMessage = decode_frame(&frame[4..]).expect("decode");
    assert_eq!(decoded.kind, msg.kind);
}

#[test]
fn session_state_none_optionals_round_trip() {
    let state = DaemonMessage::SessionState {
        session_id: 1,
        title: None,
        selected_model: None,
        parent_session_id: None,
        working_dir: None,
        max_turns: None,
        messages: vec![],
        active_tool_groups: vec![],
        token_usage: None,
        context_window: None,
        last_prompt_tokens: None,
        status: SessionStatus::Inactive,
    };
    let frame = encode_frame(&state).expect("encode");
    let decoded: DaemonMessage = decode_frame(&frame[4..]).expect("decode");
    assert_eq!(decoded, state);
}

#[test]
fn sessions_with_none_optionals_round_trip() {
    // DaemonMessage::Sessions wraps Vec<SessionSummary>; test that a
    // session list containing summaries with None optionals round-trips.
    let summary = SessionSummary {
        session_id: 1,
        title: None,
        selected_model: None,
        reasoning_effort: None,
        parent_session_id: None,
        working_dir: None,
        created_at: 0,
        message_count: 0,
        max_turns: None,
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

// ── ThinkingEffort tests ─────────────────────────────────────────────────

#[test]
fn thinking_effort_labels() {
    assert_eq!(ThinkingEffort::Off.as_label(), "off");
    assert_eq!(ThinkingEffort::Low.as_label(), "low");
    assert_eq!(ThinkingEffort::Medium.as_label(), "medium");
    assert_eq!(ThinkingEffort::High.as_label(), "high");
}

#[test]
fn thinking_effort_serialization() {
    let json = serde_json::to_string(&ThinkingEffort::Medium).unwrap();
    assert_eq!(json, "\"medium\"");
    let deserialized: ThinkingEffort = serde_json::from_str(&json).unwrap();
    assert_eq!(deserialized, ThinkingEffort::Medium);
}
