use super::*;
use serde_json::json;

#[test]
fn model_list_response_deserialises() {
    let json = json!({
        "data": [
            {"id": "claude-sonnet-4-20250514", "type": "model"},
            {"id": "claude-opus-4-20250514", "type": "model"}
        ]
    });
    let resp: ModelListResponse = serde_json::from_value(json).unwrap();
    assert_eq!(resp.data.len(), 2);
    assert_eq!(resp.data[0].id, "claude-sonnet-4-20250514");
    assert_eq!(resp.data[1].id, "claude-opus-4-20250514");
}

#[test]
fn content_block_text_deserialises() {
    let block: ContentBlock = serde_json::from_value(json!({
        "type": "text",
        "text": "Hello, world!"
    }))
    .unwrap();
    match block {
        ContentBlock::Text { text } => assert_eq!(text, "Hello, world!"),
        other => panic!("expected Text, got {other:?}"),
    }
}

#[test]
fn content_block_tool_use_deserialises() {
    let block: ContentBlock = serde_json::from_value(json!({
        "type": "tool_use",
        "id": "tu_abc123",
        "name": "get_weather",
        "input": {"city": "London"}
    }))
    .unwrap();
    match block {
        ContentBlock::ToolUse { id, name, input } => {
            assert_eq!(id, "tu_abc123");
            assert_eq!(name, "get_weather");
            assert_eq!(input, json!({"city": "London"}));
        }
        other => panic!("expected ToolUse, got {other:?}"),
    }
}

#[test]
fn content_block_thinking_deserialises() {
    let block: ContentBlock = serde_json::from_value(json!({
        "type": "thinking",
        "thinking": "I should think about this..."
    }))
    .unwrap();
    match block {
        ContentBlock::Thinking {
            thinking,
            signature,
        } => {
            assert_eq!(thinking, "I should think about this...");
            // Signature is optional on the wire; default to empty when absent.
            assert_eq!(signature, "");
        }
        other => panic!("expected Thinking, got {other:?}"),
    }
}

#[test]
fn content_block_thinking_parses_signature() {
    let block: ContentBlock = serde_json::from_value(json!({
        "type": "thinking",
        "thinking": "I should think about this...",
        "signature": "sig_xyz"
    }))
    .unwrap();
    match block {
        ContentBlock::Thinking {
            thinking,
            signature,
        } => {
            assert_eq!(thinking, "I should think about this...");
            assert_eq!(signature, "sig_xyz");
        }
        other => panic!("expected Thinking, got {other:?}"),
    }
}

#[test]
fn messages_response_with_text_deserialises() {
    let resp: MessagesResponse = serde_json::from_value(json!({
        "id": "msg_123",
        "type": "message",
        "role": "assistant",
        "content": [{"type": "text", "text": "Hello!"}],
        "stop_reason": "end_turn",
        "model": "claude-sonnet-4-20250514",
        "usage": {"input_tokens": 10, "output_tokens": 5}
    }))
    .unwrap();
    assert_eq!(resp.role, "assistant");
    assert_eq!(resp.content.len(), 1);
}

#[test]
fn response_to_turn_result_text_only() {
    let resp = MessagesResponse {
        id: "msg_1".into(),
        r#type: "message".into(),
        role: "assistant".into(),
        content: vec![ContentBlock::Text {
            text: "Hello!".into(),
        }],
        stop_reason: Some("end_turn".into()),
        stop_sequence: None,
        model: "claude-sonnet-4-20250514".into(),
        usage: None,
    };
    let result = response_to_turn_result(resp).unwrap();
    match result {
        ChatTurnResult::FinalText(ft) => {
            assert_eq!(ft.content, "Hello!");
            assert!(ft.reasoning.is_none());
        }
        other => panic!("expected FinalText, got {other:?}"),
    }
}

#[test]
fn response_to_turn_result_with_tool_use() {
    let resp = MessagesResponse {
        id: "msg_2".into(),
        r#type: "message".into(),
        role: "assistant".into(),
        content: vec![
            ContentBlock::Text {
                text: "I'll look that up.".into(),
            },
            ContentBlock::ToolUse {
                id: "tu_1".into(),
                name: "search".into(),
                input: json!({"q": "weather"}),
            },
        ],
        stop_reason: Some("tool_use".into()),
        stop_sequence: None,
        model: "claude-sonnet-4-20250514".into(),
        usage: None,
    };
    let result = response_to_turn_result(resp).unwrap();
    match result {
        ChatTurnResult::ToolUse(tu) => {
            assert_eq!(tu.content.as_deref(), Some("I'll look that up."));
            assert_eq!(tu.tool_calls.len(), 1);
            assert_eq!(tu.tool_calls[0].name, "search");
            assert_eq!(tu.tool_calls[0].arguments_json, r#"{"q":"weather"}"#);
        }
        other => panic!("expected ToolUse, got {other:?}"),
    }
}

#[test]
fn response_to_turn_result_with_thinking() {
    let resp = MessagesResponse {
        id: "msg_3".into(),
        r#type: "message".into(),
        role: "assistant".into(),
        content: vec![
            ContentBlock::Thinking {
                thinking: "Let me reason...".into(),
                signature: "sig_1".into(),
            },
            ContentBlock::Text {
                text: "Here is the answer.".into(),
            },
        ],
        stop_reason: Some("end_turn".into()),
        stop_sequence: None,
        model: "claude-sonnet-4-20250514".into(),
        usage: None,
    };
    let result = response_to_turn_result(resp).unwrap();
    match result {
        ChatTurnResult::FinalText(ft) => {
            assert_eq!(ft.content, "Here is the answer.");
            assert_eq!(ft.reasoning.as_deref(), Some("Let me reason..."));
            // Thinking was present, so the round-trip artifact must be captured.
            assert!(ft.reasoning_artifact.is_some());
        }
        other => panic!("expected FinalText, got {other:?}"),
    }
}

#[test]
fn response_to_turn_result_captures_thinking_artifact() {
    // Canned response: a signed thinking block followed by a redacted_thinking
    // block, in that order, then display text. The artifact must preserve the
    // order, the signature, and the redacted data byte-for-byte, while the
    // display text (`reasoning`) keeps working as before.
    let resp = MessagesResponse {
        id: "msg_art".into(),
        r#type: "message".into(),
        role: "assistant".into(),
        content: vec![
            ContentBlock::Thinking {
                thinking: "Let me reason carefully.".into(),
                signature: "sig_abc123".into(),
            },
            ContentBlock::RedactedThinking {
                data: "eJxT_opaque_redacted".into(),
            },
            ContentBlock::Text {
                text: "Here is the answer.".into(),
            },
        ],
        stop_reason: Some("end_turn".into()),
        stop_sequence: None,
        model: "claude-sonnet-4-20250514".into(),
        usage: None,
    };
    let result = response_to_turn_result(resp).unwrap();
    match result {
        ChatTurnResult::FinalText(ft) => {
            assert_eq!(ft.content, "Here is the answer.");
            assert_eq!(ft.reasoning.as_deref(), Some("Let me reason carefully."));
            // Byte-exact: object keys serialize alphabetically (serde_json
            // default BTreeMap ordering), block order preserved.
            let expected = br#"[{"signature":"sig_abc123","thinking":"Let me reason carefully.","type":"thinking"},{"data":"eJxT_opaque_redacted","type":"redacted_thinking"}]"#;
            assert_eq!(
                ft.reasoning_artifact,
                Some(ReasoningArtifact::AnthropicThinking(expected.to_vec()))
            );
        }
        other => panic!("expected FinalText, got {other:?}"),
    }
}

#[test]
fn response_json_with_thinking_and_redacted_parses_artifact() {
    // Feed a canned Anthropic response JSON through the parse boundary: the
    // signature field must deserialize and the ordered blocks must land in the
    // artifact.
    let json = json!({
        "id": "msg_art2",
        "type": "message",
        "role": "assistant",
        "content": [
            {"type": "thinking", "thinking": "Think.", "signature": "sig_xyz"},
            {"type": "redacted_thinking", "data": "eJxT_redacted"},
            {"type": "text", "text": "Done."}
        ],
        "stop_reason": "end_turn",
        "model": "claude-sonnet-4-20250514",
        "usage": {"input_tokens": 10, "output_tokens": 5}
    });
    let resp: MessagesResponse = serde_json::from_value(json).unwrap();
    let result = response_to_turn_result(resp).unwrap();
    match result {
        ChatTurnResult::FinalText(ft) => {
            assert_eq!(ft.content, "Done.");
            let expected = br#"[{"signature":"sig_xyz","thinking":"Think.","type":"thinking"},{"data":"eJxT_redacted","type":"redacted_thinking"}]"#;
            assert_eq!(
                ft.reasoning_artifact,
                Some(ReasoningArtifact::AnthropicThinking(expected.to_vec()))
            );
        }
        other => panic!("expected FinalText, got {other:?}"),
    }
}

#[test]
fn response_to_turn_result_no_thinking_has_no_artifact() {
    // Control: a response with no thinking / redacted_thinking blocks must
    // carry no artifact.
    let resp = MessagesResponse {
        id: "msg_ctrl".into(),
        r#type: "message".into(),
        role: "assistant".into(),
        content: vec![ContentBlock::Text {
            text: "Plain response.".into(),
        }],
        stop_reason: Some("end_turn".into()),
        stop_sequence: None,
        model: "claude-sonnet-4-20250514".into(),
        usage: None,
    };
    let result = response_to_turn_result(resp).unwrap();
    match result {
        ChatTurnResult::FinalText(ft) => {
            assert!(ft.reasoning.is_none());
            assert!(ft.reasoning_artifact.is_none());
        }
        other => panic!("expected FinalText, got {other:?}"),
    }
}

#[test]
fn response_to_turn_result_tool_use_carries_thinking_artifact() {
    // Tool-use turns must carry the artifact too (the signature is required
    // back on the tool-loop replay).
    let resp = MessagesResponse {
        id: "msg_tu_art".into(),
        r#type: "message".into(),
        role: "assistant".into(),
        content: vec![
            ContentBlock::Thinking {
                thinking: "Pick a tool.".into(),
                signature: "sig_tool".into(),
            },
            ContentBlock::ToolUse {
                id: "tu_1".into(),
                name: "search".into(),
                input: json!({"q": "x"}),
            },
        ],
        stop_reason: Some("tool_use".into()),
        stop_sequence: None,
        model: "claude-sonnet-4-20250514".into(),
        usage: None,
    };
    let result = response_to_turn_result(resp).unwrap();
    match result {
        ChatTurnResult::ToolUse(tu) => {
            assert_eq!(tu.tool_calls.len(), 1);
            assert_eq!(tu.reasoning.as_deref(), Some("Pick a tool."));
            let expected =
                br#"[{"signature":"sig_tool","thinking":"Pick a tool.","type":"thinking"}]"#;
            assert_eq!(
                tu.reasoning_artifact,
                Some(ReasoningArtifact::AnthropicThinking(expected.to_vec()))
            );
        }
        other => panic!("expected ToolUse, got {other:?}"),
    }
}

#[test]
fn response_empty_content_errors() {
    let resp = MessagesResponse {
        id: "msg_4".into(),
        r#type: "message".into(),
        role: "assistant".into(),
        content: vec![],
        stop_reason: Some("end_turn".into()),
        stop_sequence: None,
        model: "claude-sonnet-4-20250514".into(),
        usage: None,
    };
    assert!(response_to_turn_result(resp).is_err());
}

#[test]
fn known_models_are_sorted() {
    let models = KNOWN_CLAUDE_MODELS;
    assert!(!models.is_empty());
    // Verify at least some well-known models are present.
    assert!(models.contains(&"claude-sonnet-4-20250514"));
    assert!(models.contains(&"claude-haiku-3-5-20241022"));
}

#[test]
fn error_type_label_maps_correctly() {
    assert_eq!(
        crate::shared::error_type_label(AnthropicError::Unauthorized {
            status: 401,
            detail: "bad key".into()
        }),
        "unauthorized"
    );
    assert_eq!(
        crate::shared::error_type_label(AnthropicError::Cancelled),
        "cancelled"
    );
    assert_eq!(
        crate::shared::error_type_label(AnthropicError::Io(std::io::Error::other("oops"))),
        "other"
    );
}

#[test]
fn anthropic_config_defaults_are_sensible() {
    let cfg = AnthropicConfig::default();
    assert_eq!(cfg.base_url, "https://api.anthropic.com");
    assert_eq!(cfg.api_version, "2023-06-01");
    assert_eq!(cfg.max_tokens, 4096);
    assert!(cfg.streaming);
}

#[test]
fn config_apply_overrides() {
    let overrides = ProviderOverrides {
        base_url: Some("https://custom.anthropic.com".into()),
        streaming: Some(false),
        retry_max_attempts: Some(3),
        connect_timeout_secs: Some(10),
        request_timeout_secs: Some(60),
        retry_initial_backoff_ms: Some(2000),
        retry_max_backoff_ms: Some(40000),
        ..ProviderOverrides::default()
    };
    let mut cfg = AnthropicConfig::default();
    cfg.apply_overrides(&overrides);
    assert_eq!(cfg.base_url, "https://custom.anthropic.com");
    assert!(!cfg.streaming);
    assert_eq!(cfg.retry_max_attempts, 3);
    assert_eq!(cfg.connect_timeout_secs, 10);
    assert_eq!(cfg.request_timeout_secs, 60);
    assert_eq!(cfg.retry_initial_backoff_ms, 2000);
    assert_eq!(cfg.retry_max_backoff_ms, 40000);
}

#[test]
fn config_context_window_for_model_resolves_per_model() {
    let mut cfg = AnthropicConfig::default();
    cfg.context_window_config.per_model = [
        ("claude-sonnet-4-20250514".into(), 200_000),
        ("claude-3-haiku-20240307".into(), 48_000),
    ]
    .into();
    cfg.context_window_config.context_window = Some(100_000);
    let client = AnthropicClient::new(cfg, "test-key".into()).unwrap();
    // Exact model match takes precedence
    assert_eq!(
        client.context_window_for_model("claude-sonnet-4-20250514"),
        Some(200_000)
    );
    assert_eq!(
        client.context_window_for_model("claude-3-haiku-20240307"),
        Some(48_000)
    );
    // Unknown model falls back to global default
    assert_eq!(
        client.context_window_for_model("unknown-model"),
        Some(100_000)
    );
}

#[test]
fn build_message_payloads_simple() {
    let msgs = vec![ChatRequestMessage::simple("user", "Hello".to_string())];
    let (payloads, system) = build_message_payloads(&msgs, &[], true).unwrap();
    assert!(system.is_none());
    assert_eq!(payloads.len(), 1);
    assert_eq!(payloads[0].role, "user");
}

#[test]
fn test_thinking_payload_off() {
    let result = super::thinking_payload("off", 4096);
    assert!(result.is_none());
}

#[test]
fn test_thinking_payload_low() {
    let result = super::thinking_payload("low", 4096);
    assert!(result.is_some());
    assert_eq!(result.unwrap().budget_tokens, 2048);
}

#[test]
fn test_thinking_payload_medium() {
    let result = super::thinking_payload("medium", 8192);
    assert!(result.is_some());
    assert_eq!(result.unwrap().budget_tokens, 4096);
}

#[test]
fn test_thinking_payload_high() {
    let result = super::thinking_payload("high", 32768);
    assert!(result.is_some());
    assert_eq!(result.unwrap().budget_tokens, 16384);
}

#[test]
fn test_thinking_payload_minimal_and_xhigh_map_to_budgets() {
    // The catalog advertises `minimal`/`xhigh` as valid Anthropic effort
    // levels — they must enable thinking with a real budget instead of being
    // silently treated as unknown and disabling thinking.
    let minimal = super::thinking_payload("minimal", 32768);
    assert!(minimal.is_some());
    assert_eq!(minimal.unwrap().budget_tokens, 1024);
    let xhigh = super::thinking_payload("xhigh", 65536);
    assert!(xhigh.is_some());
    assert_eq!(xhigh.unwrap().budget_tokens, 32768);
}

#[test]
fn test_thinking_payload_unknown_slug_disables() {
    // A truly unknown slug still degrades to no thinking (with a warn).
    assert!(super::thinking_payload("turbo", 4096).is_none());
}

#[test]
fn test_thinking_payload_clamped() {
    // max_tokens=3072 ⇒ budget_tokens can be at most 3072-1024=2048
    let result = super::thinking_payload("high", 3072);
    assert!(result.is_some());
    assert_eq!(result.unwrap().budget_tokens, 2048);
}

#[test]
fn test_thinking_payload_low_max_tokens_exact() {
    // max_tokens=2048 ⇒ budget_tokens at most 2048-1024=1024
    let result = super::thinking_payload("low", 2048);
    assert!(result.is_some());
    assert_eq!(result.unwrap().budget_tokens, 1024);
}

#[test]
fn test_thinking_payload_zero_max_tokens() {
    // max_tokens=1024 ⇒ budgets are clamped to 0
    let result = super::thinking_payload("high", 1024);
    assert!(result.is_some());
    assert_eq!(result.unwrap().budget_tokens, 0);
}

#[test]
fn build_message_payloads_with_system() {
    let msgs = vec![
        ChatRequestMessage::simple("system", "You are a helpful assistant.".to_string()),
        ChatRequestMessage::simple("user", "Hi!".to_string()),
    ];
    let (payloads, system) = build_message_payloads(&msgs, &[], true).unwrap();
    assert_eq!(system.as_deref(), Some("You are a helpful assistant."));
    assert_eq!(payloads.len(), 1);
    assert_eq!(payloads[0].role, "user");
}

// ── reasoning artifact re-emission (phase 4a) ───────────────────────────

#[test]
fn build_message_payloads_reemits_thinking_blocks_verbatim() {
    use crate::openai::{AssistantToolCall, AssistantToolFunction};
    use choreo_proto::ReasoningArtifact;

    // A thinking block + a redacted_thinking block exactly as captured by the
    // non-streaming / streaming paths (signature + redacted data intact).
    let msgs = vec![ChatRequestMessage {
        role: "assistant",
        content: Some("Here is the answer".to_string()),
        tool_call_id: None,
        tool_calls: Some(vec![AssistantToolCall {
            id: "call_1".to_string(),
            kind: "function".to_string(),
            function: AssistantToolFunction {
                name: "get_weather".to_string(),
                arguments: r#"{"city":"London"}"#.to_string(),
            },
        }]),
        reasoning_content: None,
        reasoning: None,
        reasoning_text: None,
        reasoning_artifact: Some(ReasoningArtifact::AnthropicThinking(
            br#"[{"type":"thinking","thinking":"Let me analyze.","signature":"sig_abc"},{"type":"redacted_thinking","data":"eJxT_opaque"}]"#
                .to_vec(),
        )),
    }];
    let (payloads, system) = build_message_payloads(&msgs, &[], true).unwrap();
    assert!(system.is_none());
    assert_eq!(payloads.len(), 1);
    assert_eq!(payloads[0].role, "assistant");

    // Serialize and inspect the content array: blocks are replayed verbatim,
    // in original order, ahead of text/tool_use (thinking → redacted_thinking
    // → text → tool_use). Value equality is key-order-insensitive, so the
    // object content (not byte layout) is what's pinned here.
    let blocks = serde_json::to_value(&payloads[0].content).unwrap();
    let blocks = blocks.as_array().unwrap();
    assert_eq!(blocks.len(), 4);
    assert_eq!(
        blocks[0],
        json!({"type": "thinking", "thinking": "Let me analyze.", "signature": "sig_abc"})
    );
    assert_eq!(
        blocks[1],
        json!({"type": "redacted_thinking", "data": "eJxT_opaque"})
    );
    assert_eq!(
        blocks[2],
        json!({"type": "text", "text": "Here is the answer"})
    );
    assert_eq!(
        blocks[3],
        json!({"type": "tool_use", "id": "call_1", "name": "get_weather", "input": {"city": "London"}})
    );
}

#[test]
fn build_message_payloads_drops_thinking_blocks_when_thinking_disabled() {
    use choreo_proto::ReasoningArtifact;

    // Same artifact, but the request has thinking OFF: Anthropic rejects
    // thinking blocks sent without a matching thinking config, so they must
    // be dropped entirely (goose's `!thinking_disabled` gate).
    let msgs = vec![ChatRequestMessage {
        role: "assistant",
        content: Some("answer".to_string()),
        tool_call_id: None,
        tool_calls: None,
        reasoning_content: None,
        reasoning: None,
        reasoning_text: None,
        reasoning_artifact: Some(ReasoningArtifact::AnthropicThinking(
            br#"[{"type":"thinking","thinking":"secret","signature":"sig_1"}]"#.to_vec(),
        )),
    }];
    let (payloads, _) = build_message_payloads(&msgs, &[], false).unwrap();
    let blocks = serde_json::to_value(&payloads[0].content).unwrap();
    let blocks = blocks.as_array().unwrap();
    assert_eq!(blocks.len(), 1);
    assert_eq!(blocks[0], json!({"type": "text", "text": "answer"}));
}

#[test]
fn build_message_payloads_no_artifact_no_thinking_blocks() {
    // Control: no artifact → the assistant content has no thinking blocks
    // even when thinking is enabled.
    let msgs = vec![ChatRequestMessage::simple("assistant", "plain".to_string())];
    let (payloads, _) = build_message_payloads(&msgs, &[], true).unwrap();
    let blocks = serde_json::to_value(&payloads[0].content).unwrap();
    let blocks = blocks.as_array().unwrap();
    assert_eq!(blocks.len(), 1);
    assert_eq!(blocks[0], json!({"type": "text", "text": "plain"}));
}

#[test]
fn build_message_payloads_foreign_artifact_variant_is_dropped() {
    use choreo_proto::ReasoningArtifact;

    // A non-Anthropic artifact is foreign — the payload stays opaque and
    // must not be misinterpreted as thinking blocks.
    let msgs = vec![ChatRequestMessage {
        role: "assistant",
        content: Some("answer".to_string()),
        tool_call_id: None,
        tool_calls: None,
        reasoning_content: None,
        reasoning: None,
        reasoning_text: None,
        reasoning_artifact: Some(ReasoningArtifact::ChatReasoning {
            field: choreo_proto::ChatReasoningField::ReasoningContent,
            bytes: b"not anthropic".to_vec(),
        }),
    }];
    let (payloads, _) = build_message_payloads(&msgs, &[], true).unwrap();
    let blocks = serde_json::to_value(&payloads[0].content).unwrap();
    let blocks = blocks.as_array().unwrap();
    assert_eq!(blocks.len(), 1);
    assert_eq!(blocks[0], json!({"type": "text", "text": "answer"}));
}
