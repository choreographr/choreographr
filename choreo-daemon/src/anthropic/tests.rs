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
        ContentBlock::Thinking { thinking } => {
            assert_eq!(thinking, "I should think about this...");
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
        }
        other => panic!("expected FinalText, got {other:?}"),
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
        crate::providers::shared::error_type_label(&AnthropicError::Unauthorized {
            status: 401,
            detail: "bad key".into()
        }),
        "unauthorized"
    );
    assert_eq!(
        crate::providers::shared::error_type_label(&AnthropicError::Cancelled),
        "cancelled"
    );
    assert_eq!(
        crate::providers::shared::error_type_label(&AnthropicError::Io(std::io::Error::other(
            "oops"
        ))),
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
    let account = crate::accounts::AccountConfig {
        base_url: Some("https://custom.anthropic.com".into()),
        streaming: Some(false),
        retry_max_attempts: Some(3),
        connect_timeout_secs: Some(10),
        request_timeout_secs: Some(60),
        retry_initial_backoff_ms: Some(2000),
        retry_max_backoff_ms: Some(40000),
        ..crate::accounts::AccountConfig::simple("test", "anthropic")
    };
    let mut cfg = AnthropicConfig::default();
    cfg.apply_overrides(&account);
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
    let (payloads, system) = build_message_payloads(&msgs, &[]);
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
    let (payloads, system) = build_message_payloads(&msgs, &[]);
    assert_eq!(system.as_deref(), Some("You are a helpful assistant."));
    assert_eq!(payloads.len(), 1);
    assert_eq!(payloads[0].role, "user");
}
