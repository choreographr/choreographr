use super::super::{ChatReasoningEcho, chat_wire_messages};
use super::*;
// The types the request paths reach through their own imports; the suite names
// them directly, so it imports them directly too.
use crate::openai::reasoning_details::MAX_REASONING_BYTES;
use crate::types::{ChatAssistantToolUse, ChatToolCall};
use choreo_proto::{ChatReasoningField, ReasoningArtifact, TokenUsage};

// The reasoning re-emission (wire projection) cases live in their own
// subject module: they assert the wire shape of one projected message.
mod wire_projection;

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
        prompt_cache_key: None,
        session_id: None,
        prompt_cache_retention: None,
        prompt_cache_options: None,
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

#[test]
fn an_over_cap_encrypted_only_response_is_output_not_an_empty_response() {
    // A response whose only output was an encrypted reasoning block larger than
    // the per-turn replay cap: the payload is dropped (nothing to replay), but
    // the provider DID produce output, so this is a `FinalText` with no visible
    // text — not the `EmptyResponse` error, which would fail the turn and be
    // retried into the same result.
    let big = "z".repeat(MAX_REASONING_BYTES + 1);
    let json = format!(
        r#"{{"choices":[{{"message":{{"content":"","reasoning_details":[{{"type":"reasoning.encrypted","data":"{big}","index":0}}]}}}}]}}"#
    );
    let payload: ChatCompletionsResponse = serde_json::from_str(&json).unwrap();
    let turn = chat_completions_response_to_turn(payload).expect("turn is not empty");
    let ChatTurnResult::FinalText(result) = turn else {
        panic!("expected FinalText");
    };
    assert_eq!(result.content, "");
    assert!(
        result.reasoning_artifact.is_none(),
        "the over-cap payload is not replayed"
    );
}

#[test]
fn an_encrypted_only_response_keeps_its_artifact_below_the_cap() {
    // The same shape under the cap keeps its replay payload, so the turn is
    // output for both reasons — a control for the test above.
    let json = r#"{"choices":[{"message":{"content":"","reasoning_details":[{"type":"reasoning.encrypted","data":"opaque","index":0}]}}]}"#;
    let payload: ChatCompletionsResponse = serde_json::from_str(json).unwrap();
    let turn = chat_completions_response_to_turn(payload).expect("turn");
    let ChatTurnResult::FinalText(result) = turn else {
        panic!("expected FinalText");
    };
    assert!(result.reasoning_artifact.is_some());
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
fn streaming_scalar_first_keeps_the_details_out_of_the_display() {
    // A route that streams the scalar field in an early chunk and the structured
    // array later sends the same reasoning twice; the scalar channel claimed the
    // display first, so the later details fragments must not be rendered a second
    // time — while the details payload still wins the artifact.
    let mut acc = ChatCompletionsStreamAccumulator::default();
    let chunk1: ChatCompletionsStreamResponse =
        serde_json::from_str(r#"{"choices":[{"delta":{"reasoning_content":"step one "}}]}"#)
            .unwrap();
    let chunk2: ChatCompletionsStreamResponse = serde_json::from_str(
        r#"{"choices":[{"delta":{"reasoning_content":"step two","reasoning_details":[{"type":"reasoning.text","text":"step one step two","index":0}]}}]}"#,
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
            StreamEvent::Reasoning("step one ".into()),
            StreamEvent::Reasoning("step two".into()),
        ],
        "the scalar channel owns the display for the whole turn"
    );
    let result = acc.into_turn_result().unwrap();
    match result {
        ChatTurnResult::FinalText(f) => {
            assert_eq!(f.reasoning.as_deref(), Some("step one step two"));
            let Some(ReasoningArtifact::ChatReasoning { field, .. }) = f.reasoning_artifact else {
                panic!("expected a chat reasoning artifact");
            };
            assert_eq!(field, ChatReasoningField::ReasoningDetails);
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
fn non_streaming_empty_scalar_does_not_mask_a_populated_field() {
    // A provider that always emits `reasoning_content: ""` beside a real
    // `reasoning` payload must not have the populated field discarded: the
    // first NON-empty field wins (here `reasoning`), becoming both the display
    // text and the artifact, tagged with the field it arrived under. The
    // non-streaming fold shares the streaming state machine, so this matches
    // the streaming first-non-empty rule.
    let json = r#"{"choices":[{"message":{"content":"answer","tool_calls":[],"reasoning_content":"","reasoning":"real thought","reasoning_text":null}}],"usage":null}"#;
    let payload: ChatCompletionsResponse = serde_json::from_str(json).unwrap();
    let result = chat_completions_response_to_turn(payload).unwrap();
    match result {
        ChatTurnResult::FinalText(f) => {
            assert_eq!(f.reasoning.as_deref(), Some("real thought"));
            assert_eq!(
                f.reasoning_artifact,
                Some(ReasoningArtifact::ChatReasoning {
                    field: ChatReasoningField::Reasoning,
                    bytes: b"real thought".to_vec(),
                })
            );
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
            prompt_cache_key: None,
            session_id: None,
            prompt_cache_retention: None,
            prompt_cache_options: None,
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

// -- per-session cache affinity -------------------------------------

/// Build a plain, non-streaming body through the shared builder for one
/// affinity dialect and session id.
fn affinity_body(affinity: crate::CacheAffinity, session_id: Option<&str>) -> serde_json::Value {
    let config = super::super::ServiceConfig {
        cache_affinity: affinity,
        ..Default::default()
    };
    let messages = [ChatRequestMessage::simple("user", "hi".into())];
    build_chat_completions_body(
        &config,
        &ChatCompletionsBodyParams {
            model: "gpt-5.4",
            messages: &messages,
            tools: None,
            stream: false,
            stream_options: None,
            reasoning_effort: None,
            max_output_tokens_override: None,
            session_id,
        },
    )
    .unwrap()
}

#[test]
fn body_carries_prompt_cache_key_for_the_prompt_cache_key_dialect() {
    let body = affinity_body(
        crate::CacheAffinity::PromptCacheKey,
        Some("18446744073709551615"),
    );
    // The value is the REAL session id, never a constant.
    assert_eq!(body["prompt_cache_key"], "18446744073709551615");
    // The two affinity fields are mutually exclusive.
    assert!(body.get("session_id").is_none(), "{body}");
}

#[test]
fn body_carries_session_id_and_not_prompt_cache_key_for_openrouter() {
    let body = affinity_body(
        crate::CacheAffinity::OpenRouterSession,
        Some("sess-abc_123"),
    );
    assert_eq!(body["session_id"], "sess-abc_123");
    assert!(body.get("prompt_cache_key").is_none(), "{body}");
}

#[test]
fn body_carries_no_affinity_field_when_off() {
    let body = affinity_body(crate::CacheAffinity::Off, Some("1844"));
    assert!(body.get("prompt_cache_key").is_none(), "{body}");
    assert!(body.get("session_id").is_none(), "{body}");
}

#[test]
fn body_omits_affinity_when_the_session_id_is_invalid_for_openrouter() {
    // A space is outside OpenRouter's `[A-Za-z0-9_-]` charset, so the id must
    // be dropped rather than sent as a value OpenRouter would ignore.
    let body = affinity_body(crate::CacheAffinity::OpenRouterSession, Some("bad id"));
    assert!(body.get("session_id").is_none(), "{body}");
    assert!(body.get("prompt_cache_key").is_none(), "{body}");
}

#[test]
fn body_sends_no_affinity_without_a_session_id() {
    // The session-less one-shot prompt API sends no hint even when the provider
    // dialect is set.
    let body = affinity_body(crate::CacheAffinity::PromptCacheKey, None);
    assert!(body.get("prompt_cache_key").is_none(), "{body}");
    assert!(body.get("session_id").is_none(), "{body}");
}

// -- extended prompt-cache retention --------------------------------
//
// Retention is per model and gated on the curated long TTL, so these tests
// swap the process-global catalog with a one-model entry that declares the
// dialect and long tier. The swap is serialized under the catalog key and the
// guard restores the bundled catalog on drop.

/// Build a plain chat body for `provider`/`model` with no catalog-dependent
/// overrides, so only the retention resolver contributes a selector.
fn retention_body(provider: &str, model: &str) -> serde_json::Value {
    let config = super::super::ServiceConfig {
        provider_slug: provider.to_string(),
        ..Default::default()
    };
    let messages = [ChatRequestMessage::simple("user", "hi".into())];
    build_chat_completions_body(
        &config,
        &ChatCompletionsBodyParams {
            model,
            messages: &messages,
            tools: None,
            stream: false,
            stream_options: None,
            reasoning_effort: None,
            max_output_tokens_override: None,
            session_id: None,
        },
    )
    .unwrap()
}

/// Swap in a catalog whose single model declares `dialect` and a long TTL tier
/// (or none, when `long_secs` is `None`).
fn replace_with_retention_catalog(
    model: &str,
    dialect: crate::CacheRetentionDialect,
    long_secs: Option<u32>,
) -> crate::catalog::test_util::RestoreBundledOnDrop {
    let restore = crate::catalog::test_util::RestoreBundledOnDrop;
    let mut catalog = crate::catalog::test_util::tiny_catalog();
    catalog[0].slug = "retention-chat-test".to_string();
    let entry = &mut catalog[0].models[0];
    entry.model = model.to_string();
    entry.cache_retention = dialect;
    entry.prompt_cache = Some(crate::PromptCacheTtl {
        short_secs: Some(300),
        long_secs,
    });
    crate::catalog::replace_catalog(catalog);
    restore
}

#[test]
#[serial_test::serial(catalog)]
fn body_emits_no_retention_by_default() {
    // The `None` dialect emits nothing even though the model declares a long
    // tier — the overlay must opt in explicitly.
    let _restore =
        replace_with_retention_catalog("ret-old", crate::CacheRetentionDialect::None, Some(86_400));
    let body = retention_body("retention-chat-test", "ret-old");
    assert!(body.get("prompt_cache_retention").is_none(), "{body}");
    assert!(body.get("prompt_cache_options").is_none(), "{body}");
}

#[test]
#[serial_test::serial(catalog)]
fn body_emits_the_pre_5_6_retention_spelling() {
    let _restore = replace_with_retention_catalog(
        "ret-old",
        crate::CacheRetentionDialect::PromptCacheRetention,
        Some(86_400),
    );
    let body = retention_body("retention-chat-test", "ret-old");
    assert_eq!(body["prompt_cache_retention"], "24h");
    assert!(body.get("prompt_cache_options").is_none(), "{body}");
}

#[test]
#[serial_test::serial(catalog)]
fn body_requires_a_declared_long_tier() {
    // The dialect alone is not enough: without a curated long tier there is no
    // extended retention to ask for, so nothing is emitted.
    let _restore = replace_with_retention_catalog(
        "ret-old",
        crate::CacheRetentionDialect::PromptCacheRetention,
        None,
    );
    let body = retention_body("retention-chat-test", "ret-old");
    assert!(body.get("prompt_cache_retention").is_none(), "{body}");
}

#[test]
#[serial_test::serial(catalog)]
fn body_never_emits_the_12h_options_spelling_on_chat_completions() {
    // `prompt_cache_options.ttl = "12h"` is Responses-only; Chat Completions
    // omits the selector entirely rather than sending an unsupported value.
    let _restore = replace_with_retention_catalog(
        "ret-new",
        crate::CacheRetentionDialect::PromptCacheOptions,
        Some(43_200),
    );
    let body = retention_body("retention-chat-test", "ret-new");
    assert!(body.get("prompt_cache_options").is_none(), "{body}");
    assert!(body.get("prompt_cache_retention").is_none(), "{body}");
}

#[test]
#[serial_test::serial(catalog)]
fn body_withholds_retention_after_a_rejected_field() {
    // A 400 naming the retention field downgrades the provider; the next body
    // carries no selector. The slug is unique because the registry is
    // process-wide, and the catalog is swapped so the model would otherwise
    // emit.
    let provider = "retention-rejected-chat-test";
    let _restore = crate::catalog::test_util::RestoreBundledOnDrop;
    let mut catalog = crate::catalog::test_util::tiny_catalog();
    catalog[0].slug = provider.to_string();
    catalog[0].models[0].model = "ret-old".to_string();
    catalog[0].models[0].cache_retention = crate::CacheRetentionDialect::PromptCacheRetention;
    catalog[0].models[0].prompt_cache = Some(crate::PromptCacheTtl {
        short_secs: Some(300),
        long_secs: Some(86_400),
    });
    crate::catalog::replace_catalog(catalog);
    crate::session_routing::note_cache_field_rejected(
        provider,
        crate::cache_retention::PROMPT_CACHE_RETENTION_FIELD,
    );
    let body = retention_body(provider, "ret-old");
    assert!(body.get("prompt_cache_retention").is_none(), "{body}");
}
