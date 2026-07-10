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
    let payload = bincode::serde::encode_to_vec(
        (PROTOCOL_VERSION + 1, ClientMessage::Ping),
        bincode::config::standard(),
    )
    .expect("encode");
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
fn token_usage_in_session_message_assistant_text_backward_compat() {
    // Old JSON (before token_usage was added) should deserialize with token_usage = None
    let json = r#"{"AssistantText":{"content":"hello","reasoning":null}}"#;
    let msg: SessionMessage = serde_json::from_str(json).unwrap();
    match msg {
        SessionMessage::AssistantText {
            content,
            reasoning,
            token_usage,
        } => {
            assert_eq!(content, "hello");
            assert_eq!(reasoning, None);
            assert_eq!(token_usage, None);
        }
        _ => panic!("expected AssistantText"),
    }
}

#[test]
fn token_usage_in_session_summary_backward_compat() {
    // Old JSON (before token_usage was added to SessionSummary)
    let json = r#"{"session_id":1,"title":null,"selected_model":null,"reasoning_effort":null,"parent_session_id":null,"cwd":null,"created_at":0,"message_count":0,"max_turns":null,"status":"Inactive","active_tool_groups":[],"account_name":null}"#;
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
    };
    match msg {
        DaemonMessage::Done {
            request_id,
            token_usage,
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
    };
    let frame = encode_frame(&msg).expect("encode");
    let decoded: DaemonMessage = decode_frame(&frame[4..]).expect("decode");
    match decoded {
        DaemonMessage::Done {
            request_id,
            token_usage,
        } => {
            assert_eq!(request_id, 3);
            assert_eq!(token_usage, Some(usage));
        }
        _ => panic!("expected Done"),
    }
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
