use super::*;
use crate::google::requests::extract_error_detail;
use crate::openai::{AssistantToolCall, AssistantToolFunction};
use serde_json::json;

#[test]
fn model_list_response_deserialises() {
    let json = json!({
        "models": [
            {"name": "models/gemini-2.5-pro", "displayName": "Gemini 2.5 Pro"},
            {"name": "models/gemini-2.5-flash", "displayName": "Gemini 2.5 Flash"}
        ]
    });
    let resp: ModelListResponse = serde_json::from_value(json).unwrap();
    assert_eq!(resp.models.len(), 2);
    assert_eq!(resp.models[0].name, "models/gemini-2.5-pro");
    assert_eq!(resp.models[1].name, "models/gemini-2.5-flash");
}

// ── build_message_payloads tests ──────────────────────────────────────

#[test]
fn build_message_payloads_simple() {
    let msgs = vec![
        ChatRequestMessage::simple("user", "Hello".to_string()),
        ChatRequestMessage::simple("assistant", "Hi there!".to_string()),
    ];
    let (payloads, system) = build_message_payloads(&msgs);
    assert!(system.is_none());
    assert_eq!(payloads.len(), 2);
    assert_eq!(payloads[0].role, "user");
    assert_eq!(payloads[1].role, "model");
    // Verify serialization
    let json_val = serde_json::to_value(&payloads).unwrap();
    assert_eq!(json_val[0]["parts"][0]["text"], "Hello");
    assert_eq!(json_val[1]["parts"][0]["text"], "Hi there!");
}

#[test]
fn build_message_payloads_with_system() {
    let msgs = vec![
        ChatRequestMessage::simple("system", "You are a helpful assistant.".to_string()),
        ChatRequestMessage::simple("system", "Be concise.".to_string()),
        ChatRequestMessage::simple("user", "Hi!".to_string()),
    ];
    let (payloads, system) = build_message_payloads(&msgs);
    assert_eq!(
        system.as_deref(),
        Some("You are a helpful assistant.\nBe concise.")
    );
    assert_eq!(payloads.len(), 1);
    assert_eq!(payloads[0].role, "user");
    // System messages should NOT appear in contents
    for p in &payloads {
        assert_ne!(p.role, "system");
    }
}

#[test]
fn build_message_payloads_tool_result() {
    let msgs = vec![ChatRequestMessage {
        role: "tool",
        content: Some("Temperature is 72°F".to_string()),
        tool_call_id: Some("call_abc".to_string()),
        tool_calls: None,
        reasoning_content: None,
        reasoning: None,
        reasoning_text: None,
    }];
    let (payloads, system) = build_message_payloads(&msgs);
    assert!(system.is_none());
    assert_eq!(payloads.len(), 1);
    assert_eq!(payloads[0].role, "user");
    // Serialize and verify functionResponse structure
    let json_val = serde_json::to_value(&payloads[0]).unwrap();
    let fr = &json_val["parts"][0]["function_response"];
    assert_eq!(fr["name"], "call_abc");
    assert_eq!(fr["response"]["content"], "Temperature is 72°F");
}

#[test]
fn build_message_payloads_tool_call() {
    let msgs = vec![ChatRequestMessage {
        role: "assistant",
        content: Some("I'll check the weather.".to_string()),
        tool_call_id: None,
        tool_calls: Some(vec![AssistantToolCall {
            id: "call_1".to_string(),
            kind: "function".to_string(),
            function: AssistantToolFunction {
                name: "get_weather".to_string(),
                arguments: r#"{"location":"NYC"}"#.to_string(),
            },
        }]),
        reasoning_content: None,
        reasoning: None,
        reasoning_text: None,
    }];
    let (payloads, system) = build_message_payloads(&msgs);
    assert!(system.is_none());
    assert_eq!(payloads.len(), 1);
    assert_eq!(payloads[0].role, "model");
    // Serialize and verify: should have text + function_call
    let json_val = serde_json::to_value(&payloads[0]).unwrap();
    let parts = json_val["parts"].as_array().unwrap();
    assert_eq!(parts.len(), 2);
    assert_eq!(parts[0]["text"], "I'll check the weather.");
    assert_eq!(parts[1]["function_call"]["name"], "get_weather");
    assert_eq!(parts[1]["function_call"]["args"]["location"], "NYC");
}

// ── response_to_turn_result tests ─────────────────────────────────────

#[test]
fn response_to_turn_result_text_only() {
    let resp = GenerateContentResponse {
        candidates: vec![Candidate {
            content: Some(ContentBlock {
                parts: vec![ResponsePart::Text {
                    text: "Hello world!".to_string(),
                    thought: None,
                    thought_signature: None,
                }],
                role: Some("model".to_string()),
            }),
            finish_reason: Some("STOP".to_string()),
            index: 0,
        }],
        usage_metadata: None,
    };
    let result = response_to_turn_result(resp).unwrap();
    match result {
        ChatTurnResult::FinalText(ft) => {
            assert_eq!(ft.content, "Hello world!");
            assert!(ft.reasoning.is_none());
        }
        other => panic!("expected FinalText, got {other:?}"),
    }
}

#[test]
fn response_to_turn_result_with_tool_call() {
    let resp = GenerateContentResponse {
        candidates: vec![Candidate {
            content: Some(ContentBlock {
                parts: vec![
                    ResponsePart::Text {
                        text: "Let me look that up.".to_string(),
                        thought: None,
                        thought_signature: None,
                    },
                    ResponsePart::FunctionCall {
                        function_call: FunctionCallResponse {
                            name: "search".to_string(),
                            args: json!({"q": "weather"}),
                        },
                        thought_signature: None,
                    },
                ],
                role: Some("model".to_string()),
            }),
            finish_reason: Some("STOP".to_string()),
            index: 0,
        }],
        usage_metadata: None,
    };
    let result = response_to_turn_result(resp).unwrap();
    match result {
        ChatTurnResult::ToolUse(tu) => {
            assert_eq!(tu.content.as_deref(), Some("Let me look that up."));
            assert_eq!(tu.tool_calls.len(), 1);
            assert_eq!(tu.tool_calls[0].name, "search");
            assert_eq!(tu.tool_calls[0].id, "fc_search");
            assert!(tu.tool_calls[0].arguments_json.contains("weather"));
        }
        other => panic!("expected ToolUse, got {other:?}"),
    }
}

#[test]
fn response_to_turn_result_empty_error() {
    let resp = GenerateContentResponse {
        candidates: vec![],
        usage_metadata: None,
    };
    let result = response_to_turn_result(resp);
    assert!(result.is_err());
    match result {
        Err(GoogleError::EmptyResponse) => {}
        other => panic!("expected EmptyResponse, got {other:?}"),
    }
}

#[test]
fn response_to_turn_result_no_candidates_error() {
    let resp = GenerateContentResponse {
        candidates: vec![Candidate {
            content: None,
            finish_reason: Some("STOP".to_string()),
            index: 0,
        }],
        usage_metadata: None,
    };
    let result = response_to_turn_result(resp);
    assert!(result.is_err());
}

#[test]
fn response_to_turn_result_thinking_part_goes_to_reasoning() {
    // A real Gemini thinking part (`thought: true` + `thoughtSignature`) must
    // route its text to `reasoning` — NOT the assistant content — and capture
    // the signature into the opaque round-trip artifact.
    let resp = GenerateContentResponse {
        candidates: vec![Candidate {
            content: Some(ContentBlock {
                parts: vec![
                    ResponsePart::Text {
                        text: "Let me think about this...".to_string(),
                        thought: Some(true),
                        thought_signature: Some("sig-abc".to_string()),
                    },
                    ResponsePart::Text {
                        text: "Final answer.".to_string(),
                        thought: None,
                        thought_signature: None,
                    },
                ],
                role: Some("model".to_string()),
            }),
            finish_reason: Some("STOP".to_string()),
            index: 0,
        }],
        usage_metadata: None,
    };
    let result = response_to_turn_result(resp).unwrap();
    match result {
        ChatTurnResult::FinalText(ft) => {
            // The leak is fixed: thinking text must never appear in content.
            assert_eq!(ft.content, "Final answer.");
            assert_eq!(ft.reasoning.as_deref(), Some("Let me think about this..."));
            // Payload is the JSON array of captured signatures, byte-exact.
            assert_eq!(
                ft.reasoning_artifact,
                Some(ReasoningArtifact::GoogleSignatures(
                    br#"["sig-abc"]"#.to_vec()
                ))
            );
        }
        other => panic!("expected FinalText, got {other:?}"),
    }
}

#[test]
fn response_to_turn_result_function_call_captures_signature() {
    // `thoughtSignature` can appear on ANY part type — here on a functionCall.
    // The signature is captured and the tool call stays intact.
    let resp = GenerateContentResponse {
        candidates: vec![Candidate {
            content: Some(ContentBlock {
                parts: vec![ResponsePart::FunctionCall {
                    function_call: FunctionCallResponse {
                        name: "get_weather".to_string(),
                        args: json!({"location": "NYC"}),
                    },
                    thought_signature: Some("sig-fn".to_string()),
                }],
                role: Some("model".to_string()),
            }),
            finish_reason: Some("STOP".to_string()),
            index: 0,
        }],
        usage_metadata: None,
    };
    let result = response_to_turn_result(resp).unwrap();
    match result {
        ChatTurnResult::ToolUse(tu) => {
            assert_eq!(tu.tool_calls.len(), 1);
            assert_eq!(tu.tool_calls[0].name, "get_weather");
            assert_eq!(tu.tool_calls[0].id, "fc_get_weather");
            assert_eq!(
                tu.reasoning_artifact,
                Some(ReasoningArtifact::GoogleSignatures(
                    br#"["sig-fn"]"#.to_vec()
                ))
            );
        }
        other => panic!("expected ToolUse, got {other:?}"),
    }
}

#[test]
fn response_to_turn_result_no_thinking_no_artifact() {
    // Control: a response without thinking captures no artifact and no
    // reasoning text.
    let resp = GenerateContentResponse {
        candidates: vec![Candidate {
            content: Some(ContentBlock {
                parts: vec![ResponsePart::Text {
                    text: "Plain answer".to_string(),
                    thought: None,
                    thought_signature: None,
                }],
                role: Some("model".to_string()),
            }),
            finish_reason: Some("STOP".to_string()),
            index: 0,
        }],
        usage_metadata: None,
    };
    let result = response_to_turn_result(resp).unwrap();
    match result {
        ChatTurnResult::FinalText(ft) => {
            assert_eq!(ft.content, "Plain answer");
            assert!(ft.reasoning.is_none());
            assert!(ft.reasoning_artifact.is_none());
        }
        other => panic!("expected FinalText, got {other:?}"),
    }
}

// ── Config tests ──────────────────────────────────────────────────────

#[test]
fn google_config_defaults() {
    let cfg = GoogleConfig::default();
    assert_eq!(
        cfg.base_url,
        "https://generativelanguage.googleapis.com/v1beta"
    );
    assert!(cfg.streaming);
    assert_eq!(cfg.retry_max_attempts, 5);
    assert_eq!(cfg.retry_initial_backoff_ms, 1000);
    assert_eq!(cfg.retry_max_backoff_ms, 30000);
    assert_eq!(cfg.connect_timeout_secs, 30);
    assert_eq!(cfg.request_timeout_secs, 120);
}

#[test]
fn google_config_apply_overrides() {
    let overrides = ProviderOverrides {
        base_url: Some("https://custom.googleapis.com".into()),
        streaming: Some(false),
        retry_max_attempts: Some(3),
        connect_timeout_secs: Some(10),
        request_timeout_secs: Some(60),
        retry_initial_backoff_ms: Some(2000),
        retry_max_backoff_ms: Some(40000),
        ..ProviderOverrides::default()
    };
    let mut cfg = GoogleConfig::default();
    cfg.apply_overrides(&overrides);
    assert_eq!(cfg.base_url, "https://custom.googleapis.com");
    assert!(!cfg.streaming);
    assert_eq!(cfg.retry_max_attempts, 3);
    assert_eq!(cfg.connect_timeout_secs, 10);
    assert_eq!(cfg.request_timeout_secs, 60);
    assert_eq!(cfg.retry_initial_backoff_ms, 2000);
    assert_eq!(cfg.retry_max_backoff_ms, 40000);
}

#[test]
fn google_config_context_window_for_model_resolves_per_model() {
    let mut cfg = GoogleConfig::default();
    cfg.context_window_config.per_model = [
        ("gemini-2.5-pro-exp-03-25".into(), 1_048_576),
        ("gemini-1.5-flash-001".into(), 512_000),
    ]
    .into();
    cfg.context_window_config.context_window = Some(128_000);
    let client = GoogleClient::new(cfg, "test-key".into()).unwrap();
    assert_eq!(
        client.context_window_for_model("gemini-2.5-pro-exp-03-25"),
        Some(1_048_576)
    );
    assert_eq!(
        client.context_window_for_model("gemini-1.5-flash-001"),
        Some(512_000)
    );
    assert_eq!(
        client.context_window_for_model("unknown-model"),
        Some(128_000)
    );
}

// ── Client tests ──────────────────────────────────────────────────────

#[test]
fn google_client_new() {
    let cfg = GoogleConfig::default();
    let client = GoogleClient::new(cfg, "test-key".into()).unwrap();
    assert_eq!(client.api_key(), "test-key");
    assert_eq!(
        client.config().base_url,
        "https://generativelanguage.googleapis.com/v1beta"
    );
}

#[test]
fn google_client_list_models() {
    let cfg = GoogleConfig::default();
    let client = GoogleClient::new(cfg, "test-key".into()).unwrap();
    let models = client.validate_and_list_models().unwrap();
    assert!(!models.is_empty());
    assert!(models.contains(&"gemini-2.5-pro".to_string()));
    assert!(models.contains(&"gemini-2.5-flash".to_string()));
}

// ── SSE reader tests ──────────────────────────────────────────────────

#[test]
fn sse_reader_parses_data_lines() {
    let input = "data: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"Hello\"}]}}]}\n\ndata: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"World\"}]}}]}\n";
    let mut reader = super::requests::GeminiSseReader::from_reader(input.as_bytes());
    let event1 = reader.next_event().unwrap().unwrap();
    assert!(event1.contains("Hello"), "event1: {event1}");
    let event2 = reader.next_event().unwrap().unwrap();
    assert!(event2.contains("World"), "event2: {event2}");
    let done = reader.next_event().unwrap();
    assert!(done.is_none());
}

#[test]
fn sse_reader_handles_done() {
    let input = "data: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"Hello\"}]}}]}\n\ndata: [DONE]\n";
    let mut reader = super::requests::GeminiSseReader::from_reader(input.as_bytes());
    let event = reader.next_event().unwrap().unwrap();
    assert!(event.contains("\"Hello\""));
    let done = reader.next_event().unwrap();
    assert!(done.is_none());
}

#[test]
fn sse_reader_empty_input() {
    let mut reader = super::requests::GeminiSseReader::from_reader("".as_bytes());
    let event = reader.next_event().unwrap();
    assert!(event.is_none());
}

#[test]
fn sse_reader_only_done() {
    let mut reader = super::requests::GeminiSseReader::from_reader("data: [DONE]\n".as_bytes());
    let event = reader.next_event().unwrap();
    assert!(event.is_none());
}

#[test]
fn streaming_captures_thought_signatures() {
    use super::requests::handle_stream_part;

    // Two SSE chunks, exactly as the real streamGenerateContent wire delivers
    // them: a thinking part (`thought: true` + `thoughtSignature`) followed by
    // an ordinary answer part. Gemini does not fragment a part across chunks,
    // so each chunk carries complete parts.
    let input = concat!(
        "data: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"Let me think...\",\"thought\":true,\"thoughtSignature\":\"sig-abc\"}]}}]}\n",
        "\n",
        "data: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"Final answer.\"}]}}]}\n",
        "\n",
        "data: [DONE]\n",
    );
    let mut reader = super::requests::GeminiSseReader::from_reader(input.as_bytes());

    // Drive the same accumulation the streaming loop uses, feeding it parts
    // parsed from the real SSE payloads.
    let mut has_any_output = false;
    let mut full_text = String::new();
    let mut full_reasoning = String::new();
    let mut pending_tool_calls: Vec<super::ChatToolCall> = Vec::new();
    let mut signatures: Vec<String> = Vec::new();
    let mut events: Vec<String> = Vec::new();

    while let Some(data) = reader.next_event().unwrap() {
        let payload: super::GenerateContentResponse = serde_json::from_str(&data).unwrap();
        let Some(candidate) = payload.candidates.into_iter().next() else {
            continue;
        };
        let Some(content) = candidate.content else {
            continue;
        };
        for part in content.parts {
            if let Some(event) = handle_stream_part(
                part.classify(),
                &mut has_any_output,
                &mut full_text,
                &mut full_reasoning,
                &mut pending_tool_calls,
                &mut signatures,
            )
            .unwrap()
            {
                match event {
                    StreamEvent::Answer(t) => events.push(format!("A:{t}")),
                    StreamEvent::Reasoning(t) => events.push(format!("R:{t}")),
                }
            }
        }
    }

    // The streaming leak is fixed too: thinking text goes to reasoning, never
    // the answer, and is emitted as a Reasoning event.
    assert_eq!(full_reasoning, "Let me think...");
    assert_eq!(full_text, "Final answer.");
    assert_eq!(events, vec!["R:Let me think...", "A:Final answer."]);
    assert!(has_any_output);
    assert!(pending_tool_calls.is_empty());

    // Signature captured from the streamed thinking part, byte-exact payload.
    assert_eq!(signatures, vec!["sig-abc"]);
    let artifact = super::google_signatures_artifact(&signatures)
        .unwrap()
        .unwrap();
    assert_eq!(
        artifact,
        ReasoningArtifact::GoogleSignatures(br#"["sig-abc"]"#.to_vec())
    );
}

// ── model_url tests ──────────────────────────────────────────────────

#[test]
fn model_url_generate_content() {
    let url = model_url(
        "https://generativelanguage.googleapis.com/v1beta",
        "gemini-2.5-pro",
        "generateContent",
    )
    .unwrap();
    assert_eq!(
        url,
        "https://generativelanguage.googleapis.com/v1beta/models/gemini-2.5-pro:generateContent"
    );
}

#[test]
fn model_url_stream_generate() {
    let url = model_url(
        "https://generativelanguage.googleapis.com/v1beta",
        "gemini-2.5-flash",
        "streamGenerateContent?alt=sse",
    )
    .unwrap();
    assert_eq!(
        url,
        "https://generativelanguage.googleapis.com/v1beta/models/gemini-2.5-flash:streamGenerateContent?alt=sse"
    );
}

#[test]
fn model_url_with_trailing_slash() {
    let url = model_url(
        "https://generativelanguage.googleapis.com/v1beta/",
        "gemini-2.5-pro",
        "generateContent",
    )
    .unwrap();
    assert_eq!(
        url,
        "https://generativelanguage.googleapis.com/v1beta/models/gemini-2.5-pro:generateContent"
    );
}

// ── Error tests ───────────────────────────────────────────────────────

#[test]
fn error_type_label_maps_correctly() {
    assert_eq!(
        crate::shared::error_type_label(GoogleError::Unauthorized {
            status: 401,
            detail: "bad key".into(),
        }),
        "unauthorized"
    );
    assert_eq!(
        crate::shared::error_type_label(GoogleError::RateLimited {
            retry_after_secs: None,
            detail: "too many".into(),
        }),
        "rate_limited"
    );
    assert_eq!(
        crate::shared::error_type_label(GoogleError::ServerError {
            status: 500,
            detail: "oops".into(),
        }),
        "server_error"
    );
    assert_eq!(
        crate::shared::error_type_label(GoogleError::Cancelled),
        "cancelled"
    );
    assert_eq!(
        crate::shared::error_type_label(GoogleError::Io(std::io::Error::other("oops"))),
        "other"
    );
    assert_eq!(
        crate::shared::error_type_label(GoogleError::EmptyResponse),
        "empty_response"
    );
}

#[test]
fn known_gemini_models_are_present() {
    let models = KNOWN_GEMINI_MODELS;
    assert!(!models.is_empty());
    assert!(models.contains(&"gemini-2.5-pro"));
    assert!(models.contains(&"gemini-2.5-flash"));
    assert!(models.contains(&"gemini-1.5-pro"));
}

// ── Build tool payloads tests ─────────────────────────────────────────

#[test]
fn build_tool_payloads_empty() {
    let tools = [];
    let payload = build_tool_payloads(&tools);
    assert_eq!(payload, json!([{"functionDeclarations": []}]));
}

#[test]
fn build_tool_payloads_with_tools() {
    let tools = [ChatToolDefinition::function(
        "get_weather",
        "Get the weather for a location",
        json!({
            "type": "object",
            "properties": {
                "location": {"type": "string"}
            },
            "required": ["location"]
        }),
    )];
    let payload = build_tool_payloads(&tools);
    let decl = &payload[0]["functionDeclarations"][0];
    assert_eq!(decl["name"], "get_weather");
    assert_eq!(decl["description"], "Get the weather for a location");
    assert_eq!(
        decl["parameters"]["properties"]["location"]["type"],
        "string"
    );
}

// ── Extract error detail tests ────────────────────────────────────────

#[test]
fn extract_error_detail_parses_gemini_error() {
    let body =
        r#"{"error":{"code":400,"message":"API key not valid.","status":"INVALID_ARGUMENT"}}"#;
    let detail = extract_error_detail(body);
    assert!(detail.contains("INVALID_ARGUMENT"));
    assert!(detail.contains("API key not valid"));
}

#[test]
fn extract_error_detail_fallback_to_raw_body() {
    let body = "some raw error text";
    let detail = extract_error_detail(body);
    assert_eq!(detail, "some raw error text");
}

// ── ProviderClient trait implementation test ──────────────────────────

#[test]
fn provider_client_trait_impl() {
    // Verify that GoogleClient implements ProviderClient at compile time.
    fn takes_provider_client(_: &dyn ProviderClient) {}
    let cfg = GoogleConfig::default();
    let client = GoogleClient::new(cfg, "test-key".into()).unwrap();
    takes_provider_client(&client);
    // If we get here, the trait is implemented.
}

// ── Status to GoogleError mapping tests ───────────────────────────────

#[test]
fn status_to_google_error_unauthorized() {
    let detail = extract_error_detail(
        r#"{"error":{"code":403,"message":"permission denied","status":"PERMISSION_DENIED"}}"#,
    );
    // The 400/401/403 mapping is handled in the requests module via status_to_google_error
    let err = crate::retry::ProviderHttpError::Unauthorized {
        status: 401,
        detail: detail.clone(),
    };
    let google_err: GoogleError = err.into();
    match google_err {
        GoogleError::Unauthorized { status, detail: _ } => {
            assert_eq!(status, 401);
        }
        other => panic!("expected Unauthorized, got {other:?}"),
    }
}

#[test]
fn status_to_google_error_rate_limited() {
    let err = crate::retry::ProviderHttpError::RateLimited {
        retry_after_secs: Some(30),
        detail: "rate limited".into(),
    };
    let google_err: GoogleError = err.into();
    match google_err {
        GoogleError::RateLimited {
            retry_after_secs,
            detail: _,
        } => {
            assert_eq!(retry_after_secs, Some(30));
        }
        other => panic!("expected RateLimited, got {other:?}"),
    }
}

// ── ResponsePart deserialization tests ────────────────────────────────

#[test]
fn response_part_text_deserialises() {
    let part: ResponsePart = serde_json::from_value(json!({
        "text": "Hello"
    }))
    .unwrap();
    match part {
        ResponsePart::Text {
            text,
            thought,
            thought_signature,
        } => {
            assert_eq!(text, "Hello");
            // Ordinary text parts carry no thinking marker or signature.
            assert_eq!(thought, None);
            assert_eq!(thought_signature, None);
        }
        other => panic!("expected Text, got {other:?}"),
    }
}

#[test]
fn response_part_function_call_deserialises() {
    let part: ResponsePart = serde_json::from_value(json!({
        "functionCall": {
            "name": "get_weather",
            "args": {"location": "NYC"}
        }
    }))
    .unwrap();
    match part {
        ResponsePart::FunctionCall {
            function_call,
            thought_signature,
        } => {
            assert_eq!(function_call.name, "get_weather");
            assert_eq!(function_call.args["location"], "NYC");
            assert_eq!(thought_signature, None);
        }
        other => panic!("expected FunctionCall, got {other:?}"),
    }
}

// ── Thinking config tests ─────────────────────────────────────────────

#[test]
fn test_thinking_config_off() {
    assert!(super::thinking_config_payload("off").is_none());
}

#[test]
fn test_thinking_config_on() {
    let payload = super::thinking_config_payload("on");
    assert!(payload.is_some());
    assert!(payload.unwrap().include_thoughts);
}

#[test]
fn test_response_part_thinking_deserialization() {
    // The real Gemini wire format: a thinking part is a regular text part
    // flagged `thought: true`, carrying the encrypted `thoughtSignature`.
    // There is no `thinking` key in the wire format.
    let json = r#"{"text":"Let me think...","thought":true,"thoughtSignature":"sig-abc"}"#;
    let part: super::ResponsePart = serde_json::from_str(json).unwrap();
    match part {
        super::ResponsePart::Text {
            text,
            thought,
            thought_signature,
        } => {
            assert_eq!(text, "Let me think...");
            assert_eq!(thought, Some(true));
            assert_eq!(thought_signature.as_deref(), Some("sig-abc"));
        }
        _ => panic!("expected Text variant"),
    }
}

#[test]
fn test_response_part_thinking_deserialization_no_signature() {
    // `thoughtSignature` is optional — a thinking part without one still
    // parses to the Text-with-thought shape.
    let json = r#"{"text":"Simple thought","thought":true}"#;
    let part: super::ResponsePart = serde_json::from_str(json).unwrap();
    match part {
        super::ResponsePart::Text {
            text,
            thought,
            thought_signature,
        } => {
            assert_eq!(text, "Simple thought");
            assert_eq!(thought, Some(true));
            assert!(thought_signature.is_none());
        }
        _ => panic!("expected Text variant"),
    }
}

#[test]
fn response_part_function_call_no_args() {
    let part: ResponsePart = serde_json::from_value(json!({
        "functionCall": {
            "name": "get_weather"
        }
    }))
    .unwrap();
    match part {
        ResponsePart::FunctionCall {
            function_call,
            thought_signature,
        } => {
            assert_eq!(function_call.name, "get_weather");
            assert_eq!(function_call.args, serde_json::Value::Null);
            assert_eq!(thought_signature, None);
        }
        other => panic!("expected FunctionCall, got {other:?}"),
    }
}
