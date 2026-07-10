use crate::providers::shared::ProviderError;

use super::*;

#[test]
fn test_thinking_payload_off_returns_none() {
    assert!(thinking_payload(ThinkingEffort::Off).is_none());
}

#[test]
fn test_thinking_payload_low_returns_some() {
    assert_eq!(thinking_payload(ThinkingEffort::Low), Some("low"));
}

#[test]
fn test_thinking_payload_medium_returns_some() {
    assert_eq!(thinking_payload(ThinkingEffort::Medium), Some("medium"));
}

#[test]
fn test_thinking_payload_high_returns_some() {
    assert_eq!(thinking_payload(ThinkingEffort::High), Some("high"));
}

#[test]
fn test_error_type_label_variants() {
    assert_eq!(
        crate::providers::shared::error_type_label(&ProviderError::Unauthorized {
            status: 401,
            detail: "bad".into()
        }),
        "unauthorized"
    );
    assert_eq!(
        crate::providers::shared::error_type_label(&ProviderError::RateLimited {
            retry_after_secs: None,
            detail: "rate".into()
        }),
        "rate_limited"
    );
    assert_eq!(
        crate::providers::shared::error_type_label(&ProviderError::ServerError {
            status: 500,
            detail: "err".into()
        }),
        "server_error"
    );
    assert_eq!(
        crate::providers::shared::error_type_label(&ProviderError::ClientError {
            status: 400,
            detail: "bad".into()
        }),
        "client_error"
    );
    assert_eq!(
        crate::providers::shared::error_type_label(&ProviderError::EmptyResponse),
        "empty_response"
    );
    assert_eq!(
        crate::providers::shared::error_type_label(&ProviderError::Cancelled),
        "cancelled"
    );
    assert_eq!(
        crate::providers::shared::error_type_label(&ProviderError::Io(io::Error::other("oops"))),
        "other"
    );
}

#[test]
fn test_mistral_error_to_inference_mapping() {
    let e = ProviderError::Unauthorized {
        status: 401,
        detail: "bad".into(),
    };
    match crate::providers::shared::provider_error_to_inference(e) {
        InferenceError::Unauthorized { status, detail } => {
            assert_eq!(status, 401);
            assert_eq!(detail, "bad");
        }
        _ => panic!("expected Unauthorized"),
    }
}

#[test]
fn test_mistral_config_default() {
    let cfg = MistralConfig::default();
    assert_eq!(cfg.base_url, "https://api.mistral.ai/v1");
    assert!(cfg.streaming);
    assert_eq!(cfg.max_tokens, 4096);
    assert_eq!(cfg.retry_max_attempts, 5);
}

#[test]
fn test_mistral_config_apply_overrides() {
    let mut cfg = MistralConfig::default();
    let account_cfg = crate::accounts::AccountConfig {
        name: "test".into(),
        provider: "mistral".into(),
        base_url: Some("https://custom.mistral.ai/v1".into()),
        streaming: Some(false),
        retry_max_attempts: Some(3),
        connect_timeout_secs: Some(15),
        request_timeout_secs: Some(60),
    };
    cfg.apply_overrides(&account_cfg);
    assert_eq!(cfg.base_url, "https://custom.mistral.ai/v1");
    assert!(!cfg.streaming);
    assert_eq!(cfg.retry_max_attempts, 3);
    assert_eq!(cfg.connect_timeout_secs, 15);
    assert_eq!(cfg.request_timeout_secs, 60);
}

#[test]
fn test_build_message_payloads_system() {
    let messages = vec![ChatRequestMessage::simple(
        "system",
        "You are a helpful assistant.".into(),
    )];
    let payloads = build_message_payloads(&messages);
    assert_eq!(payloads.len(), 1);
    assert_eq!(payloads[0].role, "system");
}

#[test]
fn test_build_message_payloads_user() {
    let messages = vec![ChatRequestMessage::simple("user", "Hello".into())];
    let payloads = build_message_payloads(&messages);
    assert_eq!(payloads.len(), 1);
    assert_eq!(payloads[0].role, "user");
}

#[test]
fn test_build_message_payloads_tool() {
    let messages = vec![ChatRequestMessage {
        role: "tool",
        content: Some("result".into()),
        tool_call_id: Some("call_123".into()),
        tool_calls: None,
        reasoning_content: None,
        reasoning: None,
        reasoning_text: None,
    }];
    let payloads = build_message_payloads(&messages);
    assert_eq!(payloads.len(), 1);
    assert_eq!(payloads[0].role, "tool");
    assert_eq!(payloads[0].tool_call_id, Some("call_123"));
}

#[test]
fn test_build_tool_payloads_empty() {
    let payloads = build_tool_payloads(&[]);
    assert!(payloads.is_empty());
}

#[test]
fn test_build_tool_payloads_single() {
    let tools = vec![ChatToolDefinition::function(
        "get_weather",
        "Get the weather",
        serde_json::json!({"type": "object"}),
    )];
    let payloads = build_tool_payloads(&tools);
    assert_eq!(payloads.len(), 1);
    assert_eq!(payloads[0].function.name, "get_weather");
}

#[test]
fn test_response_to_turn_result_empty_choices_errors() {
    let response = ChatCompletionResponse {
        _id: "test".into(),
        choices: vec![],
        usage: None,
    };
    assert!(response_to_turn_result(response).is_err());
}

#[test]
fn test_response_to_turn_result_text() {
    let response = ChatCompletionResponse {
        _id: "test".into(),
        choices: vec![Choice {
            _index: 0,
            message: AssistantMessageResponse {
                content: Some("Hello world".into()),
                tool_calls: vec![],
            },
            _finish_reason: Some("stop".into()),
        }],
        usage: None,
    };
    let result = response_to_turn_result(response).unwrap();
    match result {
        ChatTurnResult::FinalText(ft) => {
            assert_eq!(ft.content, "Hello world");
            assert_eq!(ft.usage, None);
        }
        _ => panic!("expected FinalText"),
    }
}

#[test]
fn test_response_to_turn_result_text_with_usage() {
    let response = ChatCompletionResponse {
        _id: "test".into(),
        choices: vec![Choice {
            _index: 0,
            message: AssistantMessageResponse {
                content: Some("Hello world".into()),
                tool_calls: vec![],
            },
            _finish_reason: Some("stop".into()),
        }],
        usage: Some(UsageInfo {
            prompt_tokens: 20,
            completion_tokens: 10,
            total_tokens: 30,
        }),
    };
    let result = response_to_turn_result(response).unwrap();
    match result {
        ChatTurnResult::FinalText(ft) => {
            assert_eq!(ft.content, "Hello world");
            let usage = ft.usage.expect("usage should be present");
            assert_eq!(usage.input_tokens, 20);
            assert_eq!(usage.output_tokens, 10);
            assert_eq!(usage.total_tokens, 30);
        }
        _ => panic!("expected FinalText"),
    }
}

#[test]
fn test_response_to_turn_result_empty_content_errors() {
    let response = ChatCompletionResponse {
        _id: "test".into(),
        choices: vec![Choice {
            _index: 0,
            message: AssistantMessageResponse {
                content: None,
                tool_calls: vec![],
            },
            _finish_reason: Some("stop".into()),
        }],
        usage: None,
    };
    assert!(response_to_turn_result(response).is_err());
}

#[test]
fn test_response_to_turn_result_tool_calls() {
    let response = ChatCompletionResponse {
        _id: "test".into(),
        choices: vec![Choice {
            _index: 0,
            message: AssistantMessageResponse {
                content: Some("Using tool".into()),
                tool_calls: vec![ToolCallResponse {
                    _id: "call_1".into(),
                    _kind: "function".into(),
                    function: ToolCallFunctionResponse {
                        name: "get_weather".into(),
                        arguments: "{\"loc\": \"Paris\"}".into(),
                    },
                }],
            },
            _finish_reason: Some("tool_calls".into()),
        }],
        usage: None,
    };
    let result = response_to_turn_result(response).unwrap();
    match result {
        ChatTurnResult::ToolUse(tu) => {
            assert_eq!(tu.tool_calls.len(), 1);
            assert_eq!(tu.tool_calls[0].name, "get_weather");
            assert_eq!(tu.usage, None);
        }
        _ => panic!("expected ToolUse"),
    }
}

#[test]
fn test_response_to_turn_result_tool_with_usage() {
    let response = ChatCompletionResponse {
        _id: "test".into(),
        choices: vec![Choice {
            _index: 0,
            message: AssistantMessageResponse {
                content: Some("Using tool".into()),
                tool_calls: vec![ToolCallResponse {
                    _id: "call_1".into(),
                    _kind: "function".into(),
                    function: ToolCallFunctionResponse {
                        name: "get_weather".into(),
                        arguments: "{\"loc\": \"Paris\"}".into(),
                    },
                }],
            },
            _finish_reason: Some("tool_calls".into()),
        }],
        usage: Some(UsageInfo {
            prompt_tokens: 30,
            completion_tokens: 15,
            total_tokens: 45,
        }),
    };
    let result = response_to_turn_result(response).unwrap();
    match result {
        ChatTurnResult::ToolUse(tu) => {
            assert_eq!(tu.tool_calls.len(), 1);
            let usage = tu.usage.expect("usage should be present");
            assert_eq!(usage.input_tokens, 30);
            assert_eq!(usage.output_tokens, 15);
            assert_eq!(usage.total_tokens, 45);
        }
        _ => panic!("expected ToolUse"),
    }
}

#[test]
fn test_known_mistral_models_contains_large() {
    assert!(KNOWN_MISTRAL_MODELS.contains(&"mistral-large-latest"));
    assert!(KNOWN_MISTRAL_MODELS.contains(&"codestral-latest"));
}

#[test]
fn test_mistral_client_construction() {
    let cfg = MistralConfig::default();
    let client = MistralClient::new(cfg, "test-key".into()).unwrap();
    assert_eq!(client.api_key(), "test-key");
    assert_eq!(client.config().base_url, "https://api.mistral.ai/v1");
}

#[test]
fn test_from_provider_http_error_unauthorized() {
    let err = crate::retry::ProviderHttpError::Unauthorized {
        status: 401,
        detail: "bad key".into(),
    };
    let provider_err: ProviderError = err.into();
    match provider_err {
        ProviderError::Unauthorized { status, detail } => {
            assert_eq!(status, 401);
            assert_eq!(detail, "bad key");
        }
        _ => panic!("expected Unauthorized"),
    }
}

#[test]
fn test_serde_chat_completion_request_roundtrip() {
    let payloads = vec![MessagePayload {
        role: "user",
        content: Some(MessageContent::Text("hello")),
        tool_calls: None,
        tool_call_id: None,
        prefix: false,
    }];
    let req = ChatCompletionRequest {
        model: "mistral-large-latest",
        messages: payloads,
        tools: None,
        stream: false,
        max_tokens: Some(4096),
        reasoning_effort: None,
    };
    let json = serde_json::to_string(&req).unwrap();
    assert!(json.contains("\"model\":\"mistral-large-latest\""));
    assert!(json.contains("\"content\":\"hello\""));
}

#[test]
fn test_serde_message_content_text() {
    let content = MessageContent::Text("hello");
    let json = serde_json::to_string(&content).unwrap();
    assert_eq!(json, "\"hello\"");
}
