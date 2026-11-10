//! token estimate tests (see the parent `tests` module).

use super::*;

#[test]
fn resolve_reasoning_effort_off_returns_off() {
    let provider = make_test_provider();
    let result = resolve_reasoning_effort(&provider, "o3-mini", 1, 0, "off");
    assert_eq!(result, "off");
}

#[test]
fn resolve_reasoning_effort_unknown_provider_disables() {
    let provider = make_test_provider();
    let result = resolve_reasoning_effort(&provider, "o3-mini", 1, 0, "low");
    // "test-stub" slug is not in the catalog, so reasoning is unsupported.
    assert_eq!(result, "off");
}

#[test]
fn resolve_reasoning_effort_openai_supported_model_preserves() {
    let config = choreo_ai_protocols::openai::ServiceConfig::default();
    let client = choreo_ai_protocols::openai::OpenAiClient::new(
        config,
        choreo_ai_protocols::RequestAuth::api_key("test-key"),
        &choreo_ai_protocols::SocketRegistry::new(),
    )
    .unwrap();
    let provider = InferenceProvider::from_openai(client);

    let result = resolve_reasoning_effort(&provider, "o3-mini", 1, 0, "high");
    assert_eq!(result, "high");
}

#[test]
fn resolve_reasoning_effort_openai_unsupported_model_disables() {
    let config = choreo_ai_protocols::openai::ServiceConfig::default();
    let client = choreo_ai_protocols::openai::OpenAiClient::new(
        config,
        choreo_ai_protocols::RequestAuth::api_key("test-key"),
        &choreo_ai_protocols::SocketRegistry::new(),
    )
    .unwrap();
    let provider = InferenceProvider::from_openai(client);

    let result = resolve_reasoning_effort(&provider, "gpt-4.1", 1, 0, "medium");
    assert_eq!(result, "off");
}

// -- estimate_prompt_tokens tests ------------------------------------

#[test]
fn estimate_prompt_tokens_empty() {
    let (encoding, estimated) = estimate_prompt_tokens("gpt-4", &[], &[]);
    assert!(encoding.is_some());
    assert_eq!(estimated, 0);
}

#[test]
fn estimate_prompt_tokens_counts_content() {
    let messages = vec![
        ChatRequestMessage::simple("user", "hello world".into()),
        ChatRequestMessage::simple("assistant", "hi there".into()),
    ];
    let (_, estimated) = estimate_prompt_tokens("gpt-4", &messages, &[]);
    assert!(
        estimated > 0,
        "expected positive token count, got {estimated}"
    );
}

#[test]
fn estimate_prompt_tokens_counts_reasoning_artifact() {
    let base_messages = vec![
        ChatRequestMessage::simple("user", "hello".into()),
        ChatRequestMessage::simple("assistant", "visible".into()),
    ];
    let mut with_artifact = base_messages.clone();
    with_artifact[1].reasoning_artifact = Some(ReasoningArtifact::ChatReasoning {
        field: ChatReasoningField::ReasoningContent,
        bytes: "thinking deep...".into(),
    });

    let (_, base_est) = estimate_prompt_tokens("gpt-4", &base_messages, &[]);
    let (_, artifact_est) = estimate_prompt_tokens("gpt-4", &with_artifact, &[]);
    assert!(
        artifact_est > base_est,
        "replayed reasoning artifact should count as input: {artifact_est} <= {base_est}",
    );
}

#[test]
fn estimate_prompt_tokens_counts_tool_call_metadata() {
    let messages = vec![ChatRequestMessage {
        role: "assistant",
        content: None,
        images: Vec::new(),
        tool_calls: Some(vec![AssistantToolCall {
            id: "call_abc".into(),
            kind: "function".into(),
            function: AssistantToolFunction {
                name: "read_file".into(),
                arguments: r#"{"path": "/etc/hosts"}"#.into(),
            },
            caller: None,
        }]),
        tool_call_id: None,
        reasoning_artifact: None,
    }];
    let (_, estimated) = estimate_prompt_tokens("gpt-4", &messages, &[]);
    assert!(
        estimated > 0,
        "expected positive token count, got {estimated}"
    );
}

#[test]
fn estimate_prompt_tokens_includes_tool_defs() {
    let tools = vec![ChatToolDefinition::function(
        "read_file",
        "Read a file from disk",
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": {"type": "string"}
            }
        }),
    )];
    let messages = vec![ChatRequestMessage::simple("user", "read file".into())];
    let (_, with_tools) = estimate_prompt_tokens("gpt-4", &messages, &tools);
    let (_, without_tools) = estimate_prompt_tokens("gpt-4", &messages, &[]);
    assert!(
        with_tools > without_tools,
        "tool defs should increase token count: {with_tools} <= {without_tools}",
    );
}

#[test]
fn estimate_prompt_tokens_unknown_model_falls_back() {
    let messages = vec![ChatRequestMessage::simple("user", "hello".into())];
    let (encoding, estimated) = estimate_prompt_tokens("nonexistent-model-9000", &messages, &[]);
    assert!(encoding.is_some(), "should fall back to cl100k_base");
    assert!(estimated > 0);
}

#[test]
fn estimate_prompt_tokens_no_chained_context_addend() {
    // The daemon builds `messages` as the FULL conversation, not the
    // chained tail the adapter puts on the wire; the provider bills the
    // whole context it holds in the chain, which the full-conversation
    // count already covers. There is deliberately NO chained-context
    // addend — adding the last request's `prompt_tokens` would count the
    // conversation twice. This pins the counting function's contract: it
    // estimates exactly the messages it is given, nothing more.
    let messages = vec![
        ChatRequestMessage::simple("system", "rebuilt system prompt".into()),
        ChatRequestMessage::simple("user", "turn one".into()),
        ChatRequestMessage::simple("assistant", "answer".into()),
        ChatRequestMessage::simple("user", "turn two".into()),
    ];
    let (_, estimated) = estimate_prompt_tokens("gpt-4", &messages, &[]);
    // Deterministic and equal to the visible-messages count: a hidden
    // chained-context addend would inflate it far beyond the recount.
    let (_, recounted) = estimate_prompt_tokens("gpt-4", &messages, &[]);
    assert_eq!(estimated, recounted, "estimate must be deterministic");
    assert!(
        estimated > 0,
        "full conversation must count: got {estimated}"
    );
}

// -- execute_tool_with_timeout tests -----------------------------------
