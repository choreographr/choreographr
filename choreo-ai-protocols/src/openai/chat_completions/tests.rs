
use super::super::{
    AssistantToolCall, AssistantToolFunction, ChatReasoningEcho, chat_wire_message,
    chat_wire_messages,
};
use super::*;

// -- finish_reason lenient deserialization -----------------------------

#[test]
fn finish_reason_parses_known_values() {
    for (raw, expected) in [
        ("stop", FinishReason::Stop),
        ("tool_calls", FinishReason::ToolCalls),
        ("length", FinishReason::Length),
        ("sensitive", FinishReason::Sensitive),
        (
            "model_context_window_exceeded",
            FinishReason::ContextWindowExceeded,
        ),
        ("network_error", FinishReason::NetworkError),
    ] {
        let parsed: FinishReason = serde_json::from_value(serde_json::json!(raw))
            .unwrap_or_else(|e| panic!("parse {raw}: {e}"));
        assert_eq!(parsed, expected, "raw {raw}");
    }
}

#[test]
fn finish_reason_maps_openai_aliases() {
    // OpenAI's legacy `function_call` and its `content_filter` must map
    // to the shared variants so behavior is provider-portable.
    let function_call: FinishReason =
        serde_json::from_value(serde_json::json!("function_call")).unwrap();
    assert_eq!(function_call, FinishReason::ToolCalls);
    let content_filter: FinishReason =
        serde_json::from_value(serde_json::json!("content_filter")).unwrap();
    assert_eq!(content_filter, FinishReason::Sensitive);
    assert!(content_filter.is_content_filtered());
}

#[test]
fn finish_reason_unknown_value_maps_to_other_not_parse_failure() {
    // A provider adding a new finish reason must degrade to Other(raw),
    // never fail the whole response parse.
    let parsed: FinishReason =
        serde_json::from_value(serde_json::json!("some_future_reason")).unwrap();
    assert_eq!(
        parsed,
        FinishReason::Other("some_future_reason".to_string())
    );
    // Not terminal in any way.
    assert!(!parsed.is_content_filtered());
    assert!(!parsed.is_context_window_exceeded());
}

#[test]
fn choice_without_finish_reason_parses_to_none() {
    let payload: ChatCompletionsResponse =
        serde_json::from_str(r#"{"choices":[{"message":{"content":"hi"}}],"usage":null}"#).unwrap();
    let choice = payload.choices.into_iter().next().unwrap();
    assert!(choice.finish_reason.is_none());
}

#[test]
fn stream_choice_parses_finish_reason_on_final_chunk() {
    let payload: ChatCompletionsStreamResponse =
        serde_json::from_str(r#"{"choices":[{"delta":{},"finish_reason":"length"}]}"#).unwrap();
    let choice = payload.choices.into_iter().next().unwrap();
    assert_eq!(choice.finish_reason, Some(FinishReason::Length));
}

// -- finish_reason behavior mapping (non-streaming) ---------------------

#[test]
fn length_on_final_text_sets_truncation_flag() {
    let json = r#"{"choices":[{"message":{"content":"partial answer"},"finish_reason":"length"}],"usage":null}"#;
    let payload: ChatCompletionsResponse = serde_json::from_str(json).unwrap();
    let turn = chat_completions_response_to_turn(payload).unwrap();
    let ChatTurnResult::FinalText(result) = turn else {
        panic!("expected FinalText");
    };
    assert!(result.truncated, "length on final text must set truncated");
    assert_eq!(result.content, "partial answer");
}

#[test]
fn length_on_tool_use_does_not_set_truncation_flag() {
    // Tool calls + length is normal tool-loop flow, not a user-visible
    // truncation: the ToolUse result carries no truncation semantics.
    let json = r#"{"choices":[{"message":{
            "content":null,
            "tool_calls":[{"id":"call_1","type":"function","function":{"name":"get_weather","arguments":"{\"city\":\"London\"}"}}],
            "reasoning_content":null,"reasoning":null,"reasoning_text":null
        },"finish_reason":"length"}],"usage":null}"#;
    let payload: ChatCompletionsResponse = serde_json::from_str(json).unwrap();
    let turn = chat_completions_response_to_turn(payload).unwrap();
    assert!(matches!(turn, ChatTurnResult::ToolUse(_)));
}

#[test]
fn all_truncated_tool_calls_without_text_is_truncated_tool_call() {
    // length + every tool call's arguments cut mid-JSON + no content: the
    // call is unsafe to execute and there is no text to fall back on, so
    // the adapter must signal TruncatedToolCall (not an empty FinalText).
    let json = r#"{"choices":[{"message":{
            "content":null,
            "tool_calls":[{"id":"call_1","type":"function","function":{"name":"write_file","arguments":"{\"path\":\"x\","}}],
            "reasoning_content":null,"reasoning":null,"reasoning_text":null
        },"finish_reason":"length"}],"usage":null}"#;
    let payload: ChatCompletionsResponse = serde_json::from_str(json).unwrap();
    let err = chat_completions_response_to_turn(payload).unwrap_err();
    assert!(
        matches!(err, crate::openai::OpenAiError::TruncatedToolCall { .. }),
        "expected TruncatedToolCall, got {err:?}"
    );
}

#[test]
fn all_truncated_tool_calls_with_text_returns_text() {
    // Same shape, but the model also produced visible text: return it so
    // the session keeps the partial answer instead of dropping everything.
    let json = r#"{"choices":[{"message":{
            "content":"here is the plan",
            "tool_calls":[{"id":"call_1","type":"function","function":{"name":"write_file","arguments":"{\"path\":"}}],
            "reasoning_content":null,"reasoning":null,"reasoning_text":null
        },"finish_reason":"length"}],"usage":null}"#;
    let payload: ChatCompletionsResponse = serde_json::from_str(json).unwrap();
    let turn = chat_completions_response_to_turn(payload).unwrap();
    let ChatTurnResult::FinalText(result) = turn else {
        panic!("expected FinalText");
    };
    assert_eq!(result.content, "here is the plan");
}

#[test]
fn stop_keeps_behavior_unchanged() {
    let json =
        r#"{"choices":[{"message":{"content":"done"},"finish_reason":"stop"}],"usage":null}"#;
    let payload: ChatCompletionsResponse = serde_json::from_str(json).unwrap();
    let turn = chat_completions_response_to_turn(payload).unwrap();
    let ChatTurnResult::FinalText(result) = turn else {
        panic!("expected FinalText");
    };
    assert!(!result.truncated);
    assert_eq!(result.content, "done");
}

#[test]
fn sensitive_is_terminal_content_filtered_error() {
    // z.ai's `sensitive` refusal often arrives with EMPTY content — it
    // must surface as the terminal ContentFiltered error, not as an
    // (empty-response) error that could be retried.
    let json = r#"{"choices":[{"message":{"content":null,"tool_calls":[],"reasoning_content":null,"reasoning":null,"reasoning_text":null},"finish_reason":"sensitive"}],"usage":null}"#;
    let payload: ChatCompletionsResponse = serde_json::from_str(json).unwrap();
    let err = chat_completions_response_to_turn(payload).unwrap_err();
    assert!(
        matches!(err, crate::openai::OpenAiError::ContentFiltered { .. }),
        "expected ContentFiltered, got {err:?}"
    );
    // And the OpenAI alias behaves identically.
    let json = r#"{"choices":[{"message":{"content":null},"finish_reason":"content_filter"}],"usage":null}"#;
    let payload: ChatCompletionsResponse = serde_json::from_str(json).unwrap();
    let err = chat_completions_response_to_turn(payload).unwrap_err();
    assert!(matches!(
        err,
        crate::openai::OpenAiError::ContentFiltered { .. }
    ));
}

#[test]
fn sensitive_maps_to_non_retryable_inference_error() {
    // The retryability contract: the mapped InferenceError must be the
    // ContentFiltered variant, which the daemon never retries (it only
    // special-cases Cancelled and TruncatedToolCall; the HTTP retry layer
    // only retries 429/5xx status codes, and this error is produced after
    // a successful 200 parse).
    let err = crate::openai::OpenAiError::ContentFiltered {
        detail: "blocked".into(),
    };
    let inference = crate::shared::provider_error_to_inference(err);
    assert!(matches!(
        inference,
        choreo_proto::InferenceError::ContentFiltered { .. }
    ));
    assert_eq!(inference.metric_label(), "content_filtered");
}

#[test]
fn context_window_exceeded_is_terminal_error() {
    let json = r#"{"choices":[{"message":{"content":null},"finish_reason":"model_context_window_exceeded"}],"usage":null}"#;
    let payload: ChatCompletionsResponse = serde_json::from_str(json).unwrap();
    let err = chat_completions_response_to_turn(payload).unwrap_err();
    match err {
        crate::openai::OpenAiError::ContextWindowExceeded { detail } => {
            assert!(detail.contains("context window"), "{detail}");
        }
        other => panic!("expected ContextWindowExceeded, got {other:?}"),
    }
    // The mapped InferenceError carries the stable metrics label.
    let err = crate::openai::OpenAiError::ContextWindowExceeded {
        detail: "too long".into(),
    };
    assert_eq!(
        crate::shared::provider_error_to_inference(err).metric_label(),
        "context_window_exceeded"
    );
}

// -- finish_reason behavior mapping (streaming) -------------------------

#[test]
fn streaming_length_sets_truncation_flag_on_final_text() {
    let mut acc = ChatCompletionsStreamAccumulator::default();
    let chunk1: ChatCompletionsStreamResponse =
        serde_json::from_str(r#"{"choices":[{"delta":{"content":"partial"}}]}"#).unwrap();
    let chunk2: ChatCompletionsStreamResponse =
        serde_json::from_str(r#"{"choices":[{"delta":{},"finish_reason":"length"}]}"#).unwrap();
    acc.apply(&chunk1, &mut |_| Ok(())).unwrap();
    acc.apply(&chunk2, &mut |_| Ok(())).unwrap();
    let result = acc.into_turn_result().unwrap();
    match result {
        ChatTurnResult::FinalText(f) => assert!(f.truncated),
        other => panic!("expected FinalText, got {other:?}"),
    }
}

#[test]
fn streaming_sensitive_is_terminal_even_with_empty_body() {
    // The silent-failure fix: a content-filtered stream sends no deltas,
    // so without the finish-reason check it would fall through to the
    // generic EmptyResponse error.
    let mut acc = ChatCompletionsStreamAccumulator::default();
    let chunk: ChatCompletionsStreamResponse =
        serde_json::from_str(r#"{"choices":[{"delta":{},"finish_reason":"sensitive"}]}"#).unwrap();
    acc.apply(&chunk, &mut |_| Ok(())).unwrap();
    let err = acc.into_turn_result().unwrap_err();
    assert!(matches!(
        err,
        crate::openai::OpenAiError::ContentFiltered { .. }
    ));
}

#[test]
fn streaming_context_window_exceeded_is_terminal() {
    let mut acc = ChatCompletionsStreamAccumulator::default();
    let chunk: ChatCompletionsStreamResponse = serde_json::from_str(
        r#"{"choices":[{"delta":{},"finish_reason":"model_context_window_exceeded"}]}"#,
    )
    .unwrap();
    acc.apply(&chunk, &mut |_| Ok(())).unwrap();
    let err = acc.into_turn_result().unwrap_err();
    assert!(matches!(
        err,
        crate::openai::OpenAiError::ContextWindowExceeded { .. }
    ));
}

#[test]
fn streaming_stop_leaves_truncation_flag_false() {
    let mut acc = ChatCompletionsStreamAccumulator::default();
    let chunk1: ChatCompletionsStreamResponse =
        serde_json::from_str(r#"{"choices":[{"delta":{"content":"all done"}}]}"#).unwrap();
    let chunk2: ChatCompletionsStreamResponse =
        serde_json::from_str(r#"{"choices":[{"delta":{},"finish_reason":"stop"}]}"#).unwrap();
    acc.apply(&chunk1, &mut |_| Ok(())).unwrap();
    acc.apply(&chunk2, &mut |_| Ok(())).unwrap();
    let result = acc.into_turn_result().unwrap();
    match result {
        ChatTurnResult::FinalText(f) => {
            assert!(!f.truncated);
            assert_eq!(f.content, "all done");
        }
        other => panic!("expected FinalText, got {other:?}"),
    }
}

#[test]
fn streaming_all_truncated_tool_calls_without_text_is_truncated_tool_call() {
    // A streamed tool call whose arguments are cut mid-JSON, no content:
    // the accumulator must surface TruncatedToolCall, not an empty
    // truncated final answer.
    let mut acc = ChatCompletionsStreamAccumulator::default();
    let chunk: ChatCompletionsStreamResponse = serde_json::from_str(
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_1","type":"function","function":{"name":"write_file","arguments":"{\"path\":\"x\","}}]},"finish_reason":"length"}]}"#,
        )
        .unwrap();
    acc.apply(&chunk, &mut |_| Ok(())).unwrap();
    let err = acc.into_turn_result().unwrap_err();
    assert!(
        matches!(err, crate::openai::OpenAiError::TruncatedToolCall { .. }),
        "expected TruncatedToolCall, got {err:?}"
    );
}

#[test]
fn streaming_all_truncated_tool_calls_with_text_returns_text() {
    let mut acc = ChatCompletionsStreamAccumulator::default();
    let chunk: ChatCompletionsStreamResponse = serde_json::from_str(
            r#"{"choices":[{"delta":{"content":"partial answer","tool_calls":[{"index":0,"id":"call_1","type":"function","function":{"name":"write_file","arguments":"{\"path\":"}}]},"finish_reason":"length"}]}"#,
        )
        .unwrap();
    acc.apply(&chunk, &mut |_| Ok(())).unwrap();
    let result = acc.into_turn_result().unwrap();
    let ChatTurnResult::FinalText(f) = result else {
        panic!("expected FinalText");
    };
    assert_eq!(f.content, "partial answer");
}

// -- validate_tool_call_arguments tests --------------------------------

#[test]
fn validate_valid_arguments_kept() {
    let mut calls = vec![
        ChatToolCall {
            id: "call_1".into(),
            name: "get_weather".into(),
            arguments_json: r#"{"city":"London"}"#.into(),
            caller: None,
        },
        ChatToolCall {
            id: "call_2".into(),
            name: "search".into(),
            arguments_json: r#"{"q":"rust"}"#.into(),
            caller: None,
        },
    ];
    let discarded = crate::openai::validate_tool_call_arguments(&mut calls);
    assert_eq!(discarded, [] as [choreo_proto::DiscardedToolCall; 0]);
    assert_eq!(calls.len(), 2);
}

#[test]
fn validate_invalid_arguments_discarded() {
    let mut calls = vec![
        ChatToolCall {
            id: "call_1".into(),
            name: "get_weather".into(),
            arguments_json: r#"{"city":"London"}"#.into(),
            caller: None,
        },
        ChatToolCall {
            id: "call_2".into(),
            name: "bad_tool".into(),
            arguments_json: "truncated garbage".into(),
            caller: None,
        },
    ];
    let discarded = crate::openai::validate_tool_call_arguments(&mut calls);
    assert_eq!(
        discarded,
        vec![choreo_proto::DiscardedToolCall {
            name: "bad_tool".into(),
            arguments_json: "truncated garbage".into(),
        }]
    );
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].name, "get_weather");
}

#[test]
fn validate_all_invalid_returns_all_names() {
    let mut calls = vec![
        ChatToolCall {
            id: "call_1".into(),
            name: "tool_a".into(),
            arguments_json: "bad".into(),
            caller: None,
        },
        ChatToolCall {
            id: "call_2".into(),
            name: "tool_b".into(),
            arguments_json: "also bad".into(),
            caller: None,
        },
    ];
    let discarded = crate::openai::validate_tool_call_arguments(&mut calls);
    assert_eq!(discarded.len(), 2);
    assert_eq!(discarded[0].name, "tool_a");
    assert_eq!(discarded[0].arguments_json, "bad");
    assert_eq!(discarded[1].name, "tool_b");
    assert_eq!(discarded[1].arguments_json, "also bad");
    assert_eq!(calls, [] as [ChatToolCall; 0]);
}

#[test]
fn validate_empty_list_returns_empty() {
    let mut calls: Vec<ChatToolCall> = vec![];
    let discarded = crate::openai::validate_tool_call_arguments(&mut calls);
    assert_eq!(discarded, [] as [choreo_proto::DiscardedToolCall; 0]);
    assert_eq!(calls, [] as [ChatToolCall; 0]);
}

// -- tool call accumulation tests ------------------------------------

#[test]
fn accumulate_no_deltas_returns_empty_vec() {
    let result = accumulate_tool_calls_from_deltas(vec![]);
    assert_eq!(result, [] as [ChatToolCall; 0]);
}

#[test]
fn accumulate_single_tool_call_in_one_chunk() {
    let deltas = vec![StreamToolCallDelta {
        index: 0,
        id: Some("call_1".into()),
        kind: Some("function".into()),
        function: Some(StreamToolCallFunctionDelta {
            name: Some("get_weather".into()),
            arguments: Some(r#"{"city":"London"}"#.into()),
        }),
    }];
    let result = accumulate_tool_calls_from_deltas(deltas);
    assert_eq!(result.len(), 1);
    assert_eq!(result[0].id, "call_1");
    assert_eq!(result[0].name, "get_weather");
    assert_eq!(result[0].arguments_json, r#"{"city":"London"}"#);
}

#[test]
fn accumulate_arguments_concatenated_across_chunks() {
    let deltas = vec![
        StreamToolCallDelta {
            index: 0,
            id: Some("call_1".into()),
            kind: None,
            function: Some(StreamToolCallFunctionDelta {
                name: Some("get_weather".into()),
                // First half: split inside the value string, not at a
                // quote boundary, so concatenation yields valid JSON.
                arguments: Some(r#"{"city":"Lon"#.into()),
            }),
        },
        StreamToolCallDelta {
            index: 0,
            id: None,
            kind: None,
            function: Some(StreamToolCallFunctionDelta {
                name: None,
                arguments: Some(r#"don"}"#.into()),
            }),
        },
    ];
    let result = accumulate_tool_calls_from_deltas(deltas);
    assert_eq!(result.len(), 1);
    assert_eq!(result[0].id, "call_1");
    assert_eq!(result[0].name, "get_weather");
    assert_eq!(result[0].arguments_json, r#"{"city":"London"}"#);
}

#[test]
fn accumulate_multiple_tool_calls_sorted_by_index() {
    let deltas = vec![
        // Tool call 1, first chunk
        StreamToolCallDelta {
            index: 1,
            id: Some("call_2".into()),
            kind: None,
            function: Some(StreamToolCallFunctionDelta {
                name: Some("search".into()),
                arguments: Some(r#"{"q":"rust"}"#.into()),
            }),
        },
        // Tool call 0, arrives after index 1
        StreamToolCallDelta {
            index: 0,
            id: Some("call_1".into()),
            kind: None,
            function: Some(StreamToolCallFunctionDelta {
                name: Some("get_weather".into()),
                arguments: Some(r#"{"city":"Paris"}"#.into()),
            }),
        },
    ];
    let result = accumulate_tool_calls_from_deltas(deltas);
    assert_eq!(result.len(), 2);
    // Must be sorted by index: index 0 first, then index 1
    assert_eq!(result[0].id, "call_1");
    assert_eq!(result[0].name, "get_weather");
    assert_eq!(result[1].id, "call_2");
    assert_eq!(result[1].name, "search");
}

#[test]
fn accumulate_missing_id_defaults_to_empty() {
    let deltas = vec![StreamToolCallDelta {
        index: 0,
        id: None,
        kind: None,
        function: Some(StreamToolCallFunctionDelta {
            name: Some("get_weather".into()),
            arguments: Some("{}".into()),
        }),
    }];
    let result = accumulate_tool_calls_from_deltas(deltas);
    assert_eq!(result.len(), 1);
    assert_eq!(result[0].id, "");
    assert_eq!(result[0].name, "get_weather");
}

#[test]
fn accumulate_missing_name_defaults_to_empty() {
    let deltas = vec![StreamToolCallDelta {
        index: 0,
        id: Some("call_1".into()),
        kind: None,
        function: Some(StreamToolCallFunctionDelta {
            name: None,
            arguments: Some("{}".into()),
        }),
    }];
    let result = accumulate_tool_calls_from_deltas(deltas);
    assert_eq!(result.len(), 1);
    assert_eq!(result[0].id, "call_1");
    assert_eq!(result[0].name, "");
}

#[test]
fn accumulate_no_function_delta_produces_empty_call() {
    let deltas = vec![StreamToolCallDelta {
        index: 0,
        id: Some("call_1".into()),
        kind: None,
        function: None,
    }];
    let result = accumulate_tool_calls_from_deltas(deltas);
    assert_eq!(result.len(), 1);
    assert_eq!(result[0].id, "call_1");
    assert_eq!(result[0].name, "");
    assert_eq!(result[0].arguments_json, "");
}

// -- streaming delta deserialisation tests --------------------------

#[test]
fn stream_delta_tool_calls_deserialises() {
    let payload: ChatCompletionsStreamResponse = serde_json::from_str(
        r#"{
                "choices":[{
                    "delta":{
                        "content":"Hello",
                        "tool_calls":[{
                            "index":0,
                            "id":"call_abc",
                            "type":"function",
                            "function":{"name":"get_weather","arguments":"{\"city\":\"London\"}"}
                        }]
                    }
                }]
            }"#,
    )
    .expect("parse");
    let delta = payload.choices.into_iter().next().unwrap().delta.unwrap();
    assert_eq!(delta.content.as_deref(), Some("Hello"));
    let tcs = delta.tool_calls.expect("tool_calls");
    assert_eq!(tcs.len(), 1);
    assert_eq!(tcs[0].index, 0);
    assert_eq!(tcs[0].id.as_deref(), Some("call_abc"));
    assert_eq!(tcs[0].kind.as_deref(), Some("function"));
    let func = tcs[0].function.as_ref().unwrap();
    assert_eq!(func.name.as_deref(), Some("get_weather"));
    assert_eq!(func.arguments.as_deref(), Some(r#"{"city":"London"}"#));
}

#[test]
fn stream_delta_tool_calls_absent_when_not_in_json() {
    let payload: ChatCompletionsStreamResponse =
        serde_json::from_str(r#"{"choices":[{"delta":{"content":"Hi"}}]}"#).expect("parse");
    let delta = payload.choices.into_iter().next().unwrap().delta.unwrap();
    assert_eq!(delta.content.as_deref(), Some("Hi"));
    assert!(delta.tool_calls.is_none());
}

// -- accumulated deltas -> ChatTurnResult integration test ----------

#[test]
fn accumulate_deltas_to_tool_use_result() {
    // Simulate what the streaming function does: collect deltas from
    // multiple SSE chunks and pass them through the accumulator.
    let deltas = vec![
        StreamToolCallDelta {
            index: 0,
            id: Some("call_1".into()),
            kind: None,
            function: Some(StreamToolCallFunctionDelta {
                name: Some("search".into()),
                arguments: Some(r#"{"q":"we"#.into()),
            }),
        },
        StreamToolCallDelta {
            index: 0,
            id: None,
            kind: None,
            function: Some(StreamToolCallFunctionDelta {
                name: None,
                arguments: Some(r#"ather"}"#.into()),
            }),
        },
    ];
    let tool_calls = accumulate_tool_calls_from_deltas(deltas);
    assert_eq!(tool_calls.len(), 1);
    assert_eq!(tool_calls[0].id, "call_1");
    assert_eq!(tool_calls[0].name, "search");
    assert_eq!(tool_calls[0].arguments_json, r#"{"q":"weather"}"#);

    let result = ChatTurnResult::ToolUse(ChatAssistantToolUse {
        content: Some("I'll search for that.".into()),
        tool_calls,
        reasoning: None,
        usage: None,
        response_id: None,
        reasoning_artifact: None,
    });
    match result {
        ChatTurnResult::ToolUse(use_) => {
            assert_eq!(use_.content.as_deref(), Some("I'll search for that."));
            assert_eq!(use_.tool_calls.len(), 1);
        }
        _ => panic!("expected ToolUse"),
    }
}

// -- reasoning_effort serialization tests ---------------------------

#[test]
fn reasoning_effort_serialization() {
    // Off → None (omitted from body)
    assert_eq!(crate::openai::reasoning_effort_api_value("off"), None);

    // Low → "low"
    assert_eq!(
        crate::openai::reasoning_effort_api_value("low"),
        Some("low")
    );

    // Medium → "medium"
    assert_eq!(
        crate::openai::reasoning_effort_api_value("medium"),
        Some("medium")
    );

    // High → "high"
    assert_eq!(
        crate::openai::reasoning_effort_api_value("high"),
        Some("high")
    );
}

#[test]
fn chat_completions_request_omits_reasoning_effort_when_none() {
    let body = serde_json::to_value(&ChatCompletionsRequest {
        model: "gpt-4.1",
        messages: &chat_wire_messages(
            &[ChatRequestMessage::simple("user", "hello".into())],
            ChatReasoningEcho::default(),
        )
        .messages,
        tools: None,
        stream: false,
        stream_options: None,
        max_tokens: None,
        max_completion_tokens: None,
        reasoning_effort: None,
    })
    .unwrap();
    assert!(body.get("reasoning_effort").is_none(), "should be omitted");
}

// -- token usage streaming response tests ----------------------------

#[test]
fn stream_response_deserializes_usage_chunk() {
    // OpenAI sends a usage-only chunk at the end of a stream with
    // stream_options.include_usage=true.
    let json =
        r#"{"choices":[],"usage":{"prompt_tokens":50,"completion_tokens":25,"total_tokens":75}}"#;
    let payload: ChatCompletionsStreamResponse = serde_json::from_str(json).unwrap();
    assert!(payload.choices.is_empty());
    let usage = payload.usage.expect("usage should be present");
    assert_eq!(usage.prompt_tokens, 50);
    assert_eq!(usage.completion_tokens, 25);
    assert_eq!(usage.total_tokens, 75);
}

#[test]
fn stream_response_without_usage_defaults_to_none() {
    let json = r#"{"choices":[{"delta":{"content":"hello"}}]}"#;
    let payload: ChatCompletionsStreamResponse = serde_json::from_str(json).unwrap();
    assert_eq!(payload.choices.len(), 1);
    assert!(payload.usage.is_none());
}

#[test]
fn test_chat_completions_response_non_streaming_with_usage() {
    // Non-streaming response with usage
    let json = r#"{"choices":[{"message":{"content":"Hello","tool_calls":[],"reasoning_content":null,"reasoning":null,"reasoning_text":null}}],"usage":{"prompt_tokens":10,"completion_tokens":5,"total_tokens":15}}"#;
    let resp: ChatCompletionsResponse = serde_json::from_str(json).unwrap();
    let usage = resp.usage.expect("usage should be present");
    assert_eq!(usage.prompt_tokens, 10);
    assert_eq!(usage.completion_tokens, 5);
    assert_eq!(usage.total_tokens, 15);
}

#[test]
fn usage_parses_zai_cached_prompt_tokens() {
    // z.ai-shaped usage: prompt_tokens_details.cached_tokens present.
    let json = r#"{"choices":[],"usage":{"prompt_tokens":100,"completion_tokens":20,"total_tokens":120,"prompt_tokens_details":{"cached_tokens":64}}}"#;
    let resp: ChatCompletionsResponse = serde_json::from_str(json).unwrap();
    let usage = resp.usage.expect("usage should be present");
    assert_eq!(
        usage
            .prompt_tokens_details
            .expect("details present")
            .cached_tokens,
        64
    );
}

#[test]
fn usage_without_prompt_tokens_details_defaults_to_none() {
    // OpenAI/DeepSeek-style usage with no details object must still parse.
    let json =
        r#"{"choices":[],"usage":{"prompt_tokens":10,"completion_tokens":5,"total_tokens":15}}"#;
    let resp: ChatCompletionsResponse = serde_json::from_str(json).unwrap();
    let usage = resp.usage.expect("usage should be present");
    assert!(usage.prompt_tokens_details.is_none());
}

#[test]
fn response_to_turn_maps_zai_cached_tokens_into_token_usage() {
    // z.ai reports cached prompt tokens in prompt_tokens_details; the
    // parsed TokenUsage must carry that count for cost/usage reporting.
    let json = r#"{"choices":[{"message":{"content":"hi"}}],"usage":{"prompt_tokens":100,"completion_tokens":20,"total_tokens":120,"prompt_tokens_details":{"cached_tokens":64}}}"#;
    let payload: ChatCompletionsResponse = serde_json::from_str(json).unwrap();
    let turn = chat_completions_response_to_turn(payload).expect("turn");
    let ChatTurnResult::FinalText(result) = turn else {
        panic!("expected FinalText");
    };
    assert_eq!(
        result.usage,
        Some(TokenUsage {
            input_tokens: 100,
            output_tokens: 20,
            total_tokens: 120,
            cached_tokens: 64,
            cache_write_tokens: 0,
        })
    );
}

#[test]
fn response_to_turn_defaults_cached_tokens_to_zero_when_absent() {
    let json = r#"{"choices":[{"message":{"content":"hi"}}],"usage":{"prompt_tokens":10,"completion_tokens":5,"total_tokens":15}}"#;
    let payload: ChatCompletionsResponse = serde_json::from_str(json).unwrap();
    let turn = chat_completions_response_to_turn(payload).expect("turn");
    let ChatTurnResult::FinalText(result) = turn else {
        panic!("expected FinalText");
    };
    assert_eq!(
        result.usage,
        Some(TokenUsage {
            input_tokens: 10,
            output_tokens: 5,
            total_tokens: 15,
            cached_tokens: 0,
            cache_write_tokens: 0,
        })
    );
}

// -- chat completions stream delta keeps reasoning separate -----------

#[test]
fn chat_completions_stream_delta_keeps_reasoning_separate() {
    let payload: ChatCompletionsStreamResponse = serde_json::from_str(
        r#"{"choices":[{"delta":{"content":"answer","reasoning_text":"think"}}]}"#,
    )
    .expect("parse");

    let delta = payload
        .choices
        .into_iter()
        .next()
        .expect("choice")
        .delta
        .expect("delta");
    assert_eq!(delta.content.as_deref(), Some("answer"));
    assert_eq!(delta.reasoning_text.as_deref(), Some("think"));
}

// -- reasoning artifact capture (phase 2b) ---------------------------

#[test]
fn non_streaming_captures_reasoning_content_artifact() {
    // DeepSeek/Kimi-style response: `reasoning_content` populated.
    let json = r#"{"choices":[{"message":{"content":"answer","tool_calls":[],"reasoning_content":"Let me think step-by-step","reasoning":null,"reasoning_text":null}}],"usage":null}"#;
    let payload: ChatCompletionsResponse = serde_json::from_str(json).unwrap();
    let result = chat_completions_response_to_turn(payload).unwrap();
    match result {
        ChatTurnResult::FinalText(f) => {
            assert_eq!(f.content, "answer");
            assert_eq!(f.reasoning.as_deref(), Some("Let me think step-by-step"));
            assert_eq!(
                f.reasoning_artifact,
                Some(ReasoningArtifact::ChatReasoning {
                    field: ChatReasoningField::ReasoningContent,
                    bytes: b"Let me think step-by-step".to_vec(),
                })
            );
        }
        other => panic!("expected FinalText, got {other:?}"),
    }
}

#[test]
fn non_streaming_captures_reasoning_field_artifact() {
    // Some providers populate the bare `reasoning` field instead.
    let json = r#"{"choices":[{"message":{"content":"answer","tool_calls":[],"reasoning_content":null,"reasoning":"bare reasoning field","reasoning_text":null}}],"usage":null}"#;
    let payload: ChatCompletionsResponse = serde_json::from_str(json).unwrap();
    let result = chat_completions_response_to_turn(payload).unwrap();
    match result {
        ChatTurnResult::FinalText(f) => {
            assert_eq!(f.reasoning.as_deref(), Some("bare reasoning field"));
            assert_eq!(
                f.reasoning_artifact,
                Some(ReasoningArtifact::ChatReasoning {
                    field: ChatReasoningField::Reasoning,
                    bytes: b"bare reasoning field".to_vec(),
                })
            );
        }
        other => panic!("expected FinalText, got {other:?}"),
    }
}

#[test]
fn non_streaming_captures_reasoning_text_field_artifact() {
    // And others use `reasoning_text` — whichever field is populated
    // must be the one captured.
    let json = r#"{"choices":[{"message":{"content":"answer","tool_calls":[],"reasoning_content":null,"reasoning":null,"reasoning_text":"text reasoning"}}],"usage":null}"#;
    let payload: ChatCompletionsResponse = serde_json::from_str(json).unwrap();
    let result = chat_completions_response_to_turn(payload).unwrap();
    match result {
        ChatTurnResult::FinalText(f) => {
            assert_eq!(f.reasoning.as_deref(), Some("text reasoning"));
            assert_eq!(
                f.reasoning_artifact,
                Some(ReasoningArtifact::ChatReasoning {
                    field: ChatReasoningField::ReasoningText,
                    bytes: b"text reasoning".to_vec(),
                })
            );
        }
        other => panic!("expected FinalText, got {other:?}"),
    }
}

#[test]
fn non_streaming_captures_details_artifact_and_derives_display() {
    // A route that streams only `reasoning_details` (no scalar field): the
    // array is captured as the artifact and its text entry supplies display.
    let json = r#"{"choices":[{"message":{
            "content":"answer",
            "reasoning_details":[{"type":"reasoning.text","text":"think step","index":0}]
        }}],"usage":null}"#;
    let payload: ChatCompletionsResponse = serde_json::from_str(json).unwrap();
    let result = chat_completions_response_to_turn(payload).unwrap();
    match result {
        ChatTurnResult::FinalText(f) => {
            assert_eq!(f.content, "answer");
            assert_eq!(f.reasoning.as_deref(), Some("think step"));
            let Some(ReasoningArtifact::ChatReasoning { field, bytes }) = f.reasoning_artifact
            else {
                panic!("expected a chat reasoning artifact");
            };
            assert_eq!(field, ChatReasoningField::ReasoningDetails);
            let entries: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
            assert_eq!(
                entries,
                serde_json::json!([{"type":"reasoning.text","text":"think step","index":0}])
            );
        }
        other => panic!("expected FinalText, got {other:?}"),
    }
}

#[test]
fn non_streaming_encrypted_only_details_counts_as_output() {
    // Encrypted-only reasoning_details with no visible text is still model
    // output — never an EmptyResponse.
    let json = r#"{"choices":[{"message":{
            "content":null,
            "reasoning_details":[{"type":"reasoning.encrypted","data":"opaque","index":0}]
        },"finish_reason":"stop"}],"usage":null}"#;
    let payload: ChatCompletionsResponse = serde_json::from_str(json).unwrap();
    let result = chat_completions_response_to_turn(payload).unwrap();
    match result {
        ChatTurnResult::FinalText(f) => {
            assert_eq!(f.content, "");
            assert!(f.reasoning.is_none());
            assert!(f.reasoning_artifact.is_some());
        }
        other => panic!("expected FinalText, got {other:?}"),
    }
}

#[test]
fn streaming_accumulates_details_artifact() {
    let mut acc = ChatCompletionsStreamAccumulator::default();
    let chunk1: ChatCompletionsStreamResponse = serde_json::from_str(
            r#"{"choices":[{"delta":{"reasoning_details":[{"type":"reasoning.text","text":"think ","index":0}]}}]}"#,
        )
        .unwrap();
    let chunk2: ChatCompletionsStreamResponse = serde_json::from_str(
            r#"{"choices":[{"delta":{"reasoning_details":[{"type":"reasoning.text","text":"more","index":0}],"content":"answer"}}]}"#,
        )
        .unwrap();
    let mut events = Vec::new();
    acc.apply(&chunk1, &mut |e| {
        events.push(e);
        Ok(())
    })
    .unwrap();
    acc.apply(&chunk2, &mut |e| {
        events.push(e);
        Ok(())
    })
    .unwrap();
    let result = acc.into_turn_result().unwrap();
    match result {
        ChatTurnResult::FinalText(f) => {
            assert_eq!(f.content, "answer");
            assert_eq!(f.reasoning.as_deref(), Some("think more"));
            let Some(ReasoningArtifact::ChatReasoning { field, .. }) = f.reasoning_artifact else {
                panic!("expected a chat reasoning artifact");
            };
            assert_eq!(field, ChatReasoningField::ReasoningDetails);
        }
        other => panic!("expected FinalText, got {other:?}"),
    }
    assert_eq!(
        events,
        vec![
            StreamEvent::Reasoning("think ".into()),
            // Within one chunk content is emitted before reasoning (the
            // established order); the second details fragment follows.
            StreamEvent::Answer("answer".into()),
            StreamEvent::Reasoning("more".into()),
        ]
    );
}

#[test]
fn streaming_emits_reasoning_once_when_scalar_and_details_carry_it() {
    // A route that populates both the scalar field and the structured array
    // sends the same reasoning twice; the live display must come from one
    // channel only (the structured entries), so each fragment is emitted
    // exactly once.
    let mut acc = ChatCompletionsStreamAccumulator::default();
    let chunk1: ChatCompletionsStreamResponse = serde_json::from_str(
            r#"{"choices":[{"delta":{"reasoning":"think ","reasoning_details":[{"type":"reasoning.text","text":"think ","index":0}]}}]}"#,
        )
        .unwrap();
    let chunk2: ChatCompletionsStreamResponse = serde_json::from_str(
            r#"{"choices":[{"delta":{"reasoning":"more","reasoning_details":[{"type":"reasoning.text","text":"more","index":0}]}}]}"#,
        )
        .unwrap();
    let mut events = Vec::new();
    acc.apply(&chunk1, &mut |e| {
        events.push(e);
        Ok(())
    })
    .unwrap();
    acc.apply(&chunk2, &mut |e| {
        events.push(e);
        Ok(())
    })
    .unwrap();
    assert_eq!(
        events,
        vec![
            StreamEvent::Reasoning("think ".into()),
            StreamEvent::Reasoning("more".into()),
        ]
    );
    // The artifact and the persisted display text are unaffected by the
    // deduplication: the details capture still wins the artifact, and the
    // accumulated scalar text is still the display value.
    let result = acc.into_turn_result().unwrap();
    match result {
        ChatTurnResult::FinalText(f) => {
            assert_eq!(f.reasoning.as_deref(), Some("think more"));
            let Some(ReasoningArtifact::ChatReasoning { field, bytes }) = f.reasoning_artifact
            else {
                panic!("expected a chat reasoning artifact");
            };
            assert_eq!(field, ChatReasoningField::ReasoningDetails);
            let entries: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
            assert_eq!(
                entries,
                serde_json::json!([{"type":"reasoning.text","text":"think more","index":0}])
            );
        }
        other => panic!("expected FinalText, got {other:?}"),
    }
}

#[test]
fn streaming_scalar_only_reasoning_is_still_displayed() {
    // Negative control for the one-channel rule: with no structured details
    // in the turn the scalar field is the display channel, so every delta is
    // forwarded.
    let mut acc = ChatCompletionsStreamAccumulator::default();
    let chunk: ChatCompletionsStreamResponse = serde_json::from_str(
        r#"{"choices":[{"delta":{"reasoning_content":"step ","content":"answer"}}]}"#,
    )
    .unwrap();
    let mut events = Vec::new();
    acc.apply(&chunk, &mut |e| {
        events.push(e);
        Ok(())
    })
    .unwrap();
    assert_eq!(
        events,
        vec![
            StreamEvent::Answer("answer".into()),
            StreamEvent::Reasoning("step ".into()),
        ]
    );
}

#[test]
fn streaming_encrypted_only_details_is_not_an_empty_response() {
    let mut acc = ChatCompletionsStreamAccumulator::default();
    let chunk: ChatCompletionsStreamResponse = serde_json::from_str(
            r#"{"choices":[{"delta":{"reasoning_details":[{"type":"reasoning.encrypted","data":"opaque","index":0}]},"finish_reason":"stop"}]}"#,
        )
        .unwrap();
    acc.apply(&chunk, &mut |_| Ok(())).unwrap();
    let result = acc.into_turn_result().unwrap();
    match result {
        ChatTurnResult::FinalText(f) => {
            assert!(f.reasoning.is_none());
            assert!(f.reasoning_artifact.is_some());
        }
        other => panic!("expected FinalText, got {other:?}"),
    }
}

#[test]
fn non_streaming_captures_artifact_on_tool_use() {
    // The artifact must ride along on ChatAssistantToolUse too — that's
    // the DeepSeek tool-loop case where `reasoning_content` has to be
    // echoed back on the next assistant message.
    let json = r#"{"choices":[{"message":{
            "content":null,
            "tool_calls":[{"id":"call_1","type":"function","function":{"name":"get_weather","arguments":"{\"city\":\"London\"}"}}],
            "reasoning_content":"reasoning for tool call",
            "reasoning":null,"reasoning_text":null
        }}],"usage":null}"#;
    let payload: ChatCompletionsResponse = serde_json::from_str(json).unwrap();
    let result = chat_completions_response_to_turn(payload).unwrap();
    match result {
        ChatTurnResult::ToolUse(t) => {
            assert_eq!(t.reasoning.as_deref(), Some("reasoning for tool call"));
            assert_eq!(
                t.reasoning_artifact,
                Some(ReasoningArtifact::ChatReasoning {
                    field: ChatReasoningField::ReasoningContent,
                    bytes: b"reasoning for tool call".to_vec(),
                })
            );
        }
        other => panic!("expected ToolUse, got {other:?}"),
    }
}

#[test]
fn non_streaming_no_reasoning_yields_none_artifact() {
    // Control case: a response with no reasoning at all.
    let json =
        r#"{"choices":[{"message":{"content":"plain answer","tool_calls":[]}}],"usage":null}"#;
    let payload: ChatCompletionsResponse = serde_json::from_str(json).unwrap();
    let result = chat_completions_response_to_turn(payload).unwrap();
    match result {
        ChatTurnResult::FinalText(f) => {
            assert_eq!(f.content, "plain answer");
            assert!(f.reasoning.is_none());
            assert!(f.reasoning_artifact.is_none());
        }
        other => panic!("expected FinalText, got {other:?}"),
    }
}

#[test]
fn non_streaming_empty_reasoning_yields_none_artifact() {
    // `reasoning_content: ""` captures nothing reusable — an empty
    // payload must not become an artifact.
    let json = r#"{"choices":[{"message":{"content":"answer","tool_calls":[],"reasoning_content":"","reasoning":null,"reasoning_text":null}}],"usage":null}"#;
    let payload: ChatCompletionsResponse = serde_json::from_str(json).unwrap();
    let result = chat_completions_response_to_turn(payload).unwrap();
    match result {
        ChatTurnResult::FinalText(f) => {
            assert!(f.reasoning_artifact.is_none());
        }
        other => panic!("expected FinalText, got {other:?}"),
    }
}

#[test]
fn streaming_accumulates_reasoning_artifact() {
    // Two chunks carrying reasoning deltas plus an answer chunk, then
    // finalize — the concatenated deltas become the artifact bytes.
    let mut acc = ChatCompletionsStreamAccumulator::default();
    let chunk1: ChatCompletionsStreamResponse =
        serde_json::from_str(r#"{"choices":[{"delta":{"reasoning_content":"Let me think"}}]}"#)
            .unwrap();
    let chunk2: ChatCompletionsStreamResponse = serde_json::from_str(
        r#"{"choices":[{"delta":{"reasoning_content":" step-by-step","content":"answer"}}]}"#,
    )
    .unwrap();
    let mut events = Vec::new();
    acc.apply(&chunk1, &mut |e| {
        events.push(e);
        Ok(())
    })
    .unwrap();
    acc.apply(&chunk2, &mut |e| {
        events.push(e);
        Ok(())
    })
    .unwrap();
    let result = acc.into_turn_result().unwrap();
    match result {
        ChatTurnResult::FinalText(f) => {
            assert_eq!(f.content, "answer");
            assert_eq!(f.reasoning.as_deref(), Some("Let me think step-by-step"));
            assert_eq!(
                f.reasoning_artifact,
                Some(ReasoningArtifact::ChatReasoning {
                    field: ChatReasoningField::ReasoningContent,
                    bytes: b"Let me think step-by-step".to_vec(),
                })
            );
        }
        other => panic!("expected FinalText, got {other:?}"),
    }
    // Both reasoning deltas were forwarded as display events. Within a
    // chunk, content is emitted before reasoning (pre-existing order).
    assert_eq!(
        events,
        vec![
            StreamEvent::Reasoning("Let me think".into()),
            StreamEvent::Answer("answer".into()),
            StreamEvent::Reasoning(" step-by-step".into()),
        ]
    );
}

#[test]
fn streaming_tool_use_carries_reasoning_artifact() {
    let mut acc = ChatCompletionsStreamAccumulator::default();
    let chunk: ChatCompletionsStreamResponse = serde_json::from_str(
            r#"{"choices":[{"delta":{
                "reasoning_content":"deciding on tool",
                "tool_calls":[{"index":0,"id":"call_1","type":"function","function":{"name":"get_weather","arguments":"{\"city\":\"London\"}"}}]
            }}]}"#,
        )
        .unwrap();
    let mut events = Vec::new();
    acc.apply(&chunk, &mut |e| {
        events.push(e);
        Ok(())
    })
    .unwrap();
    let result = acc.into_turn_result().unwrap();
    match result {
        ChatTurnResult::ToolUse(t) => {
            assert_eq!(t.reasoning.as_deref(), Some("deciding on tool"));
            assert_eq!(
                t.reasoning_artifact,
                Some(ReasoningArtifact::ChatReasoning {
                    field: ChatReasoningField::ReasoningContent,
                    bytes: b"deciding on tool".to_vec(),
                })
            );
        }
        other => panic!("expected ToolUse, got {other:?}"),
    }
}

#[test]
fn streaming_no_reasoning_yields_none_artifact() {
    // Control case: stream with only answer text.
    let mut acc = ChatCompletionsStreamAccumulator::default();
    let chunk: ChatCompletionsStreamResponse =
        serde_json::from_str(r#"{"choices":[{"delta":{"content":"hi"}}]}"#).unwrap();
    let mut events = Vec::new();
    acc.apply(&chunk, &mut |e| {
        events.push(e);
        Ok(())
    })
    .unwrap();
    let result = acc.into_turn_result().unwrap();
    match result {
        ChatTurnResult::FinalText(f) => {
            assert!(f.reasoning.is_none());
            assert!(f.reasoning_artifact.is_none());
        }
        other => panic!("expected FinalText, got {other:?}"),
    }
}

#[test]
fn streaming_locks_reasoning_field_on_first_delta() {
    // A stream that sends `reasoning_text` (not `reasoning_content`) must
    // capture ONLY that field: the round-trip artifact is re-emitted to
    // the same field the provider used, so a later stray
    // `reasoning_content` delta must not be mixed into the payload (it
    // would corrupt the replayed reasoning on the next tool-loop turn).
    let mut acc = ChatCompletionsStreamAccumulator::default();
    let chunk1: ChatCompletionsStreamResponse =
        serde_json::from_str(r#"{"choices":[{"delta":{"reasoning_text":"think step by step"}}]}"#)
            .unwrap();
    let chunk2: ChatCompletionsStreamResponse = serde_json::from_str(
        r#"{"choices":[{"delta":{"reasoning_content":"stray other field","content":"answer"}}]}"#,
    )
    .unwrap();
    let mut events = Vec::new();
    acc.apply(&chunk1, &mut |e| {
        events.push(e);
        Ok(())
    })
    .unwrap();
    acc.apply(&chunk2, &mut |e| {
        events.push(e);
        Ok(())
    })
    .unwrap();
    let result = acc.into_turn_result().unwrap();
    match result {
        ChatTurnResult::FinalText(f) => {
            assert_eq!(f.content, "answer");
            assert_eq!(f.reasoning.as_deref(), Some("think step by step"));
            assert_eq!(
                f.reasoning_artifact,
                Some(ReasoningArtifact::ChatReasoning {
                    field: ChatReasoningField::ReasoningText,
                    bytes: b"think step by step".to_vec(),
                }),
                "artifact must capture only the locked-in reasoning_text field",
            );
            // The field tag must record WHICH wire field was captured, so
            // re-emission targets reasoning_text (not reasoning_content).
            if let Some(ReasoningArtifact::ChatReasoning { field, .. }) = f.reasoning_artifact {
                assert_eq!(field, ChatReasoningField::ReasoningText);
            }
        }
        other => panic!("expected FinalText, got {other:?}"),
    }
    // Only the locked-in field's delta is forwarded as a display event;
    // the stray reasoning_content delta is neither displayed nor captured.
    assert_eq!(
        events,
        vec![
            StreamEvent::Reasoning("think step by step".into()),
            StreamEvent::Answer("answer".into()),
        ]
    );
}

#[test]
fn streaming_single_chunk_prefers_reasoning_content() {
    // A single chunk carrying BOTH reasoning_content and reasoning must
    // pick reasoning_content — the non-streaming precedence
    // (take_reasoning consumes reasoning_content first).
    let mut acc = ChatCompletionsStreamAccumulator::default();
    let chunk: ChatCompletionsStreamResponse = serde_json::from_str(
        r#"{"choices":[{"delta":{"reasoning_content":"primary","reasoning":"secondary"}}]}"#,
    )
    .unwrap();
    acc.apply(&chunk, &mut |_| Ok(())).unwrap();
    let result = acc.into_turn_result().unwrap();
    match result {
        ChatTurnResult::FinalText(f) => {
            assert_eq!(f.reasoning.as_deref(), Some("primary"));
            assert_eq!(
                f.reasoning_artifact,
                Some(ReasoningArtifact::ChatReasoning {
                    field: ChatReasoningField::ReasoningContent,
                    bytes: b"primary".to_vec(),
                }),
            );
        }
        other => panic!("expected FinalText, got {other:?}"),
    }
}

// -- reasoning artifact re-emission (phase 4a) -------------------------

#[test]
fn chat_request_message_reemits_chat_reasoning_artifact() {
    // An assistant message carrying the opaque ChatReasoning artifact must
    // re-emit it as `reasoning_content` on the wire (DeepSeek/Kimi reject
    // a tool-loop turn that drops it).
    let msg = ChatRequestMessage {
        role: "assistant",
        content: Some("answer".into()),
        images: Vec::new(),
        tool_call_id: None,
        tool_calls: None,
        reasoning_artifact: Some(ReasoningArtifact::ChatReasoning {
            field: ChatReasoningField::ReasoningContent,
            bytes: b"Let me think step-by-step".to_vec(),
        }),
    };
    let body = serde_json::to_value(chat_wire_message(&msg, ChatReasoningEcho::default())).unwrap();
    assert_eq!(body["reasoning_content"], "Let me think step-by-step");
    // The artifact field itself never appears on the wire.
    assert!(body.get("reasoning_artifact").is_none());
}

#[test]
fn chat_request_message_reemits_to_captured_field() {
    // The artifact records WHICH chat field the provider used; re-emission
    // must target that same field — a `reasoning_text` artifact goes back
    // as `reasoning_text`, a `reasoning` artifact as `reasoning`, and
    // neither leaks into `reasoning_content` (the historical default).
    for (field, wire_key) in [
        (ChatReasoningField::ReasoningText, "reasoning_text"),
        (ChatReasoningField::Reasoning, "reasoning"),
    ] {
        let msg = ChatRequestMessage {
            role: "assistant",
            content: Some("answer".into()),
            images: Vec::new(),
            tool_call_id: None,
            tool_calls: None,
            reasoning_artifact: Some(ReasoningArtifact::ChatReasoning {
                field,
                bytes: b"provider reasoning".to_vec(),
            }),
        };
        let body =
            serde_json::to_value(chat_wire_message(&msg, ChatReasoningEcho::default())).unwrap();
        assert_eq!(body[wire_key], "provider reasoning");
        for other in ["reasoning_content", "reasoning", "reasoning_text"] {
            if other != wire_key {
                assert!(
                    body.get(other).is_none(),
                    "{other} must be absent when re-emitting {wire_key}"
                );
            }
        }
        assert!(body.get("reasoning_artifact").is_none());
    }
}

#[test]
fn chat_request_message_without_artifact_omits_reasoning_content() {
    // Control: no artifact → no `reasoning_content` on the wire.
    let msg = ChatRequestMessage::simple("assistant", "plain".into());
    let body = serde_json::to_value(chat_wire_message(&msg, ChatReasoningEcho::default())).unwrap();
    assert!(body.get("reasoning_content").is_none());
}

#[test]
fn chat_request_message_corrupt_artifact_is_dropped_not_fatal() {
    // A persisted artifact whose bytes are not valid UTF-8 must NOT fail
    // the whole request serialization (the pre-fix behavior surfaced a
    // corrupted DB blob as a hard error on every subsequent turn). It is
    // logged and dropped: the message serializes as a plain assistant
    // message with no reasoning echo.
    let msg = ChatRequestMessage {
        role: "assistant",
        content: Some("answer".into()),
        images: Vec::new(),
        tool_call_id: None,
        tool_calls: None,
        reasoning_artifact: Some(ReasoningArtifact::ChatReasoning {
            field: ChatReasoningField::ReasoningContent,
            // 0xFF is never valid UTF-8.
            bytes: vec![0xFF, 0xFE, 0x00, 0x41],
        }),
    };
    let body = serde_json::to_value(chat_wire_message(&msg, ChatReasoningEcho::default())).unwrap();
    assert_eq!(body["content"], "answer");
    assert!(body.get("reasoning_content").is_none());
    assert!(body.get("reasoning_artifact").is_none());
}

#[test]
fn chat_request_message_non_assistant_role_drops_artifact() {
    // The artifact is assistant-only: a user message must never carry
    // `reasoning_content`, even if an artifact were attached.
    let msg = ChatRequestMessage {
        role: "user",
        content: Some("hi".into()),
        images: Vec::new(),
        tool_call_id: None,
        tool_calls: None,
        reasoning_artifact: Some(ReasoningArtifact::ChatReasoning {
            field: ChatReasoningField::ReasoningContent,
            bytes: b"should not leak".to_vec(),
        }),
    };
    let body = serde_json::to_value(chat_wire_message(&msg, ChatReasoningEcho::default())).unwrap();
    assert!(body.get("reasoning_content").is_none());
    assert!(body.get("reasoning_artifact").is_none());
}

#[test]
fn chat_request_message_wrong_artifact_variant_does_not_leak() {
    // A non-ChatReasoning artifact is foreign to this adapter — it must
    // not be misinterpreted as chat reasoning.
    let msg = ChatRequestMessage {
        role: "assistant",
        content: Some("answer".into()),
        images: Vec::new(),
        tool_call_id: None,
        tool_calls: None,
        reasoning_artifact: Some(ReasoningArtifact::GoogleSignatures(
            b"encrypted-sig".to_vec(),
        )),
    };
    let body = serde_json::to_value(chat_wire_message(&msg, ChatReasoningEcho::default())).unwrap();
    assert!(body.get("reasoning_content").is_none());
    assert!(body.get("reasoning_artifact").is_none());
}

// -- route-declared reasoning echo policy ----------------------------

/// A chat artifact whose payload was observed under `observed`.
fn chat_artifact(observed: ChatReasoningField, text: &[u8]) -> ReasoningArtifact {
    ReasoningArtifact::ChatReasoning {
        field: observed,
        bytes: text.to_vec(),
    }
}

fn assistant_with(artifact: ReasoningArtifact) -> ChatRequestMessage {
    ChatRequestMessage {
        role: "assistant",
        content: Some("answer".into()),
        images: Vec::new(),
        tool_call_id: None,
        tool_calls: None,
        reasoning_artifact: Some(artifact),
    }
}

/// The echo policy a route that declares `field` resolves to.
fn declared(field: ChatReasoningField) -> ChatReasoningEcho {
    ChatReasoningEcho {
        field: Some(field),
        require_present: true,
    }
}

fn wire(msg: &ChatRequestMessage, echo: ChatReasoningEcho) -> serde_json::Value {
    serde_json::to_value(chat_wire_message(msg, echo)).unwrap()
}

/// A structured details artifact from a JSON entry array.
fn details_artifact(entries: &serde_json::Value) -> ReasoningArtifact {
    ReasoningArtifact::ChatReasoning {
        field: ChatReasoningField::ReasoningDetails,
        bytes: serde_json::to_vec(entries).unwrap(),
    }
}

#[test]
fn declared_details_replays_openrouter_entries_verbatim() {
    // Verbatim replay: index, format and signature stay intact, and no
    // scalar field is invented beside the array.
    let entries = serde_json::json!([{
        "type":"reasoning.text","text":"think","signature":"sig","format":"f","index":0
    }]);
    let msg = assistant_with(details_artifact(&entries));
    let body = wire(&msg, declared(ChatReasoningField::ReasoningDetails));
    assert_eq!(body["reasoning_details"], entries);
    for scalar in ["reasoning_content", "reasoning", "reasoning_text"] {
        assert!(body.get(scalar).is_none(), "{scalar} must stay absent");
    }
}

#[test]
fn declared_details_rebuilds_kimi_entries_with_companion() {
    // A persisted Kimi payload keeps a stream `index`; replay strips it and
    // mirrors the summary text as `reasoning_content`.
    let msg = assistant_with(details_artifact(&serde_json::json!([
        {"type":"summary","summary":"step one","index":0}
    ])));
    let body = wire(&msg, declared(ChatReasoningField::ReasoningDetails));
    assert_eq!(
        body["reasoning_details"],
        serde_json::json!([{"type":"summary","summary":"step one"}])
    );
    assert_eq!(body["reasoning_content"], "step one");
}

#[test]
fn declaring_details_route_emits_an_empty_array_when_nothing_to_echo() {
    let msg = ChatRequestMessage::simple("assistant", "plain".into());
    let body = wire(&msg, declared(ChatReasoningField::ReasoningDetails));
    assert_eq!(body["reasoning_details"], serde_json::json!([]));
    assert!(body.get("reasoning_content").is_none());
}

#[test]
fn injected_empty_details_does_not_keep_an_empty_message_alive() {
    // `reasoning_details: []` is a presence marker, not a payload.
    let messages = vec![
        ChatRequestMessage::simple("user", "q".into()),
        ChatRequestMessage::simple("assistant", String::new()),
    ];
    assert_eq!(
        project(&messages, declared(ChatReasoningField::ReasoningDetails)),
        (vec!["user".to_string()], 1)
    );
}

#[test]
fn non_empty_details_keeps_an_empty_assistant_message_alive() {
    // The array IS the payload: dropping the message would starve the echo.
    let mut msg = assistant_with(details_artifact(&serde_json::json!([
        {"type":"reasoning.encrypted","data":"opaque","index":0}
    ])));
    msg.content = Some(String::new());
    let messages = vec![ChatRequestMessage::simple("user", "q".into()), msg];
    let (roles, dropped) = project(&messages, declared(ChatReasoningField::ReasoningDetails));
    assert_eq!(roles, vec!["user".to_string(), "assistant".to_string()]);
    assert_eq!(dropped, 0);
}

#[test]
fn observed_details_replays_details_without_a_declaration() {
    // With no declared field the observation routes the payload: a details
    // artifact goes back as `reasoning_details`.
    let msg = assistant_with(details_artifact(&serde_json::json!([
        {"type":"reasoning.text","text":"t","index":0}
    ])));
    let body = wire(&msg, ChatReasoningEcho::default());
    assert!(body.get("reasoning_details").is_some());
    assert!(body.get("reasoning_content").is_none());
}

#[test]
fn declared_scalar_route_uses_details_display_text() {
    // Cross-route mismatch: a structured artifact on a route that declares a
    // scalar field replays its derived text, never raw JSON.
    let msg = assistant_with(details_artifact(&serde_json::json!([
        {"type":"reasoning.text","text":"think","index":0},
        {"type":"reasoning.summary","summary":" more","index":0}
    ])));
    let body = wire(&msg, declared(ChatReasoningField::ReasoningContent));
    assert_eq!(body["reasoning_content"], "think more");
}

#[test]
fn scalar_artifact_on_a_details_route_round_trips_as_a_text_entry() {
    // The mirror image of `declared_scalar_route_uses_details_display_text`:
    // a scalar payload observed under `reasoning_content` reaching a route
    // that declares the structured array is replayed as one
    // `reasoning.text` entry rather than discarded (a catalog/overlay flip
    // can put a scalar artifact on a details route).
    let msg = assistant_with(chat_artifact(
        ChatReasoningField::ReasoningContent,
        b"recoverable thought",
    ));
    let body = wire(&msg, declared(ChatReasoningField::ReasoningDetails));
    assert_eq!(
        body["reasoning_details"],
        serde_json::json!([{"type":"reasoning.text","text":"recoverable thought"}])
    );
    // Exactly one destination: no scalar reasoning key beside the array.
    for scalar in ["reasoning_content", "reasoning", "reasoning_text"] {
        assert!(body.get(scalar).is_none(), "{scalar} must stay absent");
    }
}

#[test]
fn undeclared_route_injects_no_empty_details_array_for_an_unreplayable_artifact() {
    // Nothing is declared, so the destination came only from the
    // observation: an artifact that lowers to nothing must not materialize a
    // brand-new `reasoning_details: []` key the route never asked for.
    let msg = assistant_with(ReasoningArtifact::ChatReasoning {
        field: ChatReasoningField::ReasoningDetails,
        bytes: b"not json".to_vec(),
    });
    let body = wire(&msg, ChatReasoningEcho::default());
    assert!(
        body.get("reasoning_details").is_none(),
        "an undeclared route must not receive an injected empty array: {body}"
    );
    for scalar in ["reasoning_content", "reasoning", "reasoning_text"] {
        assert!(body.get(scalar).is_none(), "{scalar} must stay absent");
    }
}

#[test]
fn declaring_details_route_still_emits_an_empty_array_for_a_corrupt_artifact() {
    // A declared structured route keeps its presence marker even when the
    // persisted payload is unreplayable: the array is `[]`, never absent.
    let msg = assistant_with(ReasoningArtifact::ChatReasoning {
        field: ChatReasoningField::ReasoningDetails,
        bytes: b"not json".to_vec(),
    });
    let body = wire(&msg, declared(ChatReasoningField::ReasoningDetails));
    assert_eq!(body["reasoning_details"], serde_json::json!([]));
    assert!(body.get("reasoning_content").is_none());
}

#[test]
fn declared_field_overrides_the_observed_field() {
    // The live-failure shape: the gateway streamed the reasoning under
    // `reasoning` yet rejects any history without `reasoning_content`.
    // The catalog's declared field wins over the observation, and exactly
    // one reasoning key reaches the wire.
    let msg = assistant_with(chat_artifact(ChatReasoningField::Reasoning, b"thought"));
    let body = wire(&msg, declared(ChatReasoningField::ReasoningContent));
    assert_eq!(body["reasoning_content"], "thought");
    assert!(
        body.get("reasoning").is_none(),
        "the observed field must not survive beside the declared one"
    );
}

#[test]
fn declared_non_content_field_also_wins() {
    // The policy is not hard-wired to `reasoning_content`: any declared
    // field wins over the observation.
    let msg = assistant_with(chat_artifact(
        ChatReasoningField::ReasoningContent,
        b"thought",
    ));
    let body = wire(&msg, declared(ChatReasoningField::ReasoningText));
    assert_eq!(body["reasoning_text"], "thought");
    assert!(body.get("reasoning_content").is_none());
}

#[test]
fn observed_field_is_used_when_the_route_declares_none() {
    // With no declared field the observation is the destination — a
    // provider that answers under `reasoning_text` gets `reasoning_text`
    // back rather than a key it never used.
    for observed in [
        ChatReasoningField::ReasoningContent,
        ChatReasoningField::Reasoning,
        ChatReasoningField::ReasoningText,
    ] {
        let msg = assistant_with(chat_artifact(observed, b"payload"));
        let body = wire(&msg, ChatReasoningEcho::default());
        let key = match observed {
            ChatReasoningField::ReasoningContent => "reasoning_content",
            ChatReasoningField::Reasoning => "reasoning",
            ChatReasoningField::ReasoningText => "reasoning_text",
            ChatReasoningField::ReasoningDetails => "reasoning_details",
        };
        assert_eq!(body[key], "payload", "{key}");
        for other in ["reasoning_content", "reasoning", "reasoning_text"] {
            if other != key {
                assert!(body.get(other).is_none(), "{other} must stay absent");
            }
        }
    }
}

#[test]
fn required_field_is_present_but_empty_when_there_is_nothing_to_echo() {
    // An assistant message with no artifact on a route that requires the
    // field: the key exists, empty. This is what keeps a DeepSeek/GLM
    // history valid on a turn that produced no reasoning at all.
    let msg = ChatRequestMessage::simple("assistant", "plain".into());
    let body = wire(&msg, declared(ChatReasoningField::ReasoningContent));
    assert_eq!(body["reasoning_content"], "");
}

#[test]
fn no_key_when_the_route_declares_nothing_and_there_is_nothing_to_echo() {
    // Without a declared field and without an artifact, no reasoning key is
    // invented — the message is serialized exactly as recorded.
    let msg = ChatRequestMessage::simple("assistant", "plain".into());
    let body = wire(&msg, ChatReasoningEcho::default());
    for key in ["reasoning_content", "reasoning", "reasoning_text"] {
        assert!(body.get(key).is_none(), "{key} must not be invented");
    }
}

#[test]
fn required_field_is_present_for_a_foreign_artifact_variant() {
    // An artifact this adapter cannot interpret must never satisfy the
    // required field with garbage — but the field itself still has to be
    // there, so it goes out empty.
    let msg = assistant_with(ReasoningArtifact::GoogleSignatures(b"sig".to_vec()));
    let body = wire(&msg, declared(ChatReasoningField::ReasoningContent));
    assert_eq!(body["reasoning_content"], "");
    assert!(body.get("reasoning_artifact").is_none());
}

#[test]
fn corrupt_artifact_still_satisfies_a_required_field() {
    // A corrupted persisted blob drops the payload but must not remove the
    // required key: the message stays valid instead of 400-ing forever.
    let msg = assistant_with(chat_artifact(
        ChatReasoningField::ReasoningContent,
        &[0xFF, 0xFE],
    ));
    let body = wire(&msg, declared(ChatReasoningField::ReasoningContent));
    assert_eq!(body["reasoning_content"], "");
    assert_eq!(body["content"], "answer");
}

// -- empty-message projection ----------------------------------------

/// Lower a slice, returning the wire roles and how many were dropped.
fn project(messages: &[ChatRequestMessage], echo: ChatReasoningEcho) -> (Vec<String>, usize) {
    let lowered = chat_wire_messages(messages, echo);
    (
        lowered
            .messages
            .iter()
            .map(|m| m.role().to_string())
            .collect(),
        lowered.dropped,
    )
}

#[test]
fn empty_assistant_message_is_dropped() {
    // An assistant turn that produced no visible content and carries no
    // reasoning payload is dropped from the request rather than sent empty
    // (a message with neither content nor tool calls is rejected outright).
    let messages = vec![
        ChatRequestMessage::simple("user", "q".into()),
        ChatRequestMessage::simple("assistant", String::new()),
    ];
    assert_eq!(
        project(&messages, ChatReasoningEcho::default()),
        (vec!["user".to_string()], 1)
    );
}

#[test]
fn whitespace_only_assistant_message_is_dropped() {
    // A blank "answer" is not a payload: strict servers treat it as an empty
    // message, so the projection trims before deciding.
    let messages = vec![
        ChatRequestMessage::simple("user", "q".into()),
        ChatRequestMessage::simple("assistant", " \n\t ".into()),
    ];
    assert_eq!(
        project(&messages, ChatReasoningEcho::default()),
        (vec!["user".to_string()], 1)
    );
}

#[test]
fn empty_assistant_message_with_a_reasoning_artifact_survives() {
    // The echo IS the payload: dropping this message would starve the
    // reasoning round-trip the route requires.
    let mut msg = assistant_with(chat_artifact(ChatReasoningField::Reasoning, b"thinking"));
    msg.content = Some(String::new());
    let messages = vec![ChatRequestMessage::simple("user", "q".into()), msg];
    let (roles, dropped) = project(&messages, declared(ChatReasoningField::ReasoningContent));
    assert_eq!(roles, vec!["user".to_string(), "assistant".to_string()]);
    assert_eq!(dropped, 0);
}

#[test]
fn empty_assistant_message_with_tool_calls_survives() {
    let mut msg = ChatRequestMessage::simple("assistant", String::new());
    msg.tool_calls = Some(vec![AssistantToolCall {
        id: "call_1".into(),
        kind: "function".into(),
        function: AssistantToolFunction {
            name: "sh".into(),
            arguments: "{}".into(),
        },
    }]);
    let messages = vec![ChatRequestMessage::simple("user", "q".into()), msg];
    let (roles, dropped) = project(&messages, ChatReasoningEcho::default());
    assert_eq!(roles, vec!["user".to_string(), "assistant".to_string()]);
    assert_eq!(dropped, 0);
}

#[test]
fn required_empty_field_does_not_keep_an_empty_message_alive() {
    // The required-field empty string is a presence marker, not a payload: a
    // still-empty assistant message is dropped even on a route that declares
    // the field.
    let messages = vec![
        ChatRequestMessage::simple("user", "q".into()),
        ChatRequestMessage::simple("assistant", String::new()),
    ];
    assert_eq!(
        project(&messages, declared(ChatReasoningField::ReasoningContent)),
        (vec!["user".to_string()], 1)
    );
}

#[test]
fn non_assistant_messages_are_never_dropped() {
    // The projection only removes assistant messages: every other role is
    // part of the history the request was built from, so a request can never
    // end up with an empty messages array.
    let messages = vec![
        ChatRequestMessage::simple("system", String::new()),
        ChatRequestMessage::simple("user", String::new()),
    ];
    let (roles, dropped) = project(&messages, ChatReasoningEcho::default());
    assert_eq!(roles, vec!["system".to_string(), "user".to_string()]);
    assert_eq!(dropped, 0);
}

// -- per-call output cap (cache-warming ping) --------------------------

#[test]
fn max_tokens_override_lands_in_the_models_output_field() {
    // The cache-warming ping's cap must reach the wire in the ONE field a
    // model uses, with the other slot absent. Build the body the turn
    // builder would from the resolved pair and assert the serialized keys.
    let serialize = |model: &str, config: &super::super::ServiceConfig, override_: Option<u32>| {
        let (max_tokens, max_completion_tokens) =
            config.max_tokens_field_pair_with_override(model, override_);
        serde_json::to_value(&ChatCompletionsRequest {
            model,
            messages: &[] as &[String],
            tools: None,
            stream: false,
            stream_options: None,
            max_tokens,
            max_completion_tokens,
            reasoning_effort: None,
        })
        .unwrap()
    };

    // Default field is MaxCompletionTokens (o-series / gpt-5.x): the cap
    // lands in the second slot and `max_tokens` is omitted entirely.
    let config = super::super::ServiceConfig::default();
    let body = serialize("gpt-5.4", &config, Some(1));
    assert_eq!(body["max_completion_tokens"], 1);
    assert!(body.get("max_tokens").is_none(), "{body}");

    // A model pinned to `max_tokens` gets the cap in the first slot and
    // omits `max_completion_tokens`.
    let mut config = super::super::ServiceConfig::default();
    config.model_max_tokens_fields.insert(
        "legacy-gpt".to_string(),
        super::super::MaxTokensField::MaxTokens,
    );
    let body = serialize("legacy-gpt", &config, Some(1));
    assert_eq!(body["max_tokens"], 1);
    assert!(body.get("max_completion_tokens").is_none(), "{body}");
}
