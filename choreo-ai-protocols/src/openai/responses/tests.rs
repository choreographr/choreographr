use super::*;
use serde_json::json;

// The types and helpers the request paths reach through their own imports; the
// suite names them directly, so it imports them directly too.
use crate::ToolResultItem;
use crate::openai::{ChatRequestMessage, ChatToolDefinition, messages_to_responses_input};
use choreo_proto::ReasoningArtifact;

// ── Serialisation tests ───────────────────────────────────────────

#[test]
fn responses_input_item_message_serializes() {
    let item = ResponsesInputItem::Message {
        role: "user".to_string(),
        content: serde_json::Value::String("hello".to_string()),
    };
    let value = serde_json::to_value(&item).unwrap();
    assert_eq!(value["type"], "message");
    assert_eq!(value["role"], "user");
    assert_eq!(value["content"], "hello");
}

#[test]
fn responses_input_item_message_with_images_serializes_parts() {
    // A user message carrying an image renders `content` as an array of
    // `input_text` + `input_image` parts, with `image_url` as a flat
    // string (the Responses gotcha) and `detail` fixed to "auto".
    use crate::openai::ChatImagePart;
    let item = ResponsesInputItem::Message {
        role: "user".to_string(),
        content: ChatRequestMessage {
            role: "user",
            content: Some("look".to_string()),
            images: vec![ChatImagePart {
                data: b"\x89PNG-fake".to_vec(),
                mime_type: "image/png".to_string(),
            }],
            tool_call_id: None,
            tool_calls: None,
            reasoning_artifact: None,
        }
        .responses_content_value(),
    };
    let value = serde_json::to_value(&item).unwrap();
    let content = value["content"].as_array().expect("content array");
    assert_eq!(content[0]["type"], "input_text");
    assert_eq!(content[0]["text"], "look");
    assert_eq!(content[1]["type"], "input_image");
    assert_eq!(
        content[1]["image_url"],
        "data:image/png;base64,iVBORy1mYWtl"
    );
    assert_eq!(content[1]["detail"], "auto");
}

#[test]
fn responses_input_item_function_call_output_serializes() {
    let item = ResponsesInputItem::FunctionCallOutput {
        call_id: "call_1".to_string(),
        output: "result".to_string(),
        caller: None,
    };
    let value = serde_json::to_value(&item).unwrap();
    assert_eq!(value["type"], "function_call_output");
    assert_eq!(value["call_id"], "call_1");
    assert_eq!(value["output"], "result");
    // caller should be absent when None
    assert!(value.get("caller").is_none());
}

#[test]
#[serial_test::serial(catalog)]
fn responses_body_nests_reasoning_and_omits_it_for_non_reasoning() {
    let config = super::super::ServiceConfig {
        provider_slug: "openai".to_string(),
        ..Default::default()
    };

    // Collect the serialized body's keys as a sorted `Vec<String>` so we
    // can assert the EXACT key set — more precise than poking at
    // individual fields, which would be tautological here because
    // `ResponsesRequest` carries no top-level `reasoning_effort` / `include`
    // field of its own.
    let body_keys = |body: &serde_json::Value| -> Vec<String> {
        let mut keys: Vec<String> = body.as_object().unwrap().keys().cloned().collect();
        keys.sort();
        keys
    };

    // Build a tool-less Responses body for `model` with `effort` mapped to
    // the reasoning config; the retry/cancellation/route/chaining fields
    // are inert for body construction.
    let build = |model: &str, effort: Option<&str>| {
        let mut no_retry = None;
        let params = super::super::TurnParams {
            model,
            messages: &[],
            tools: &[],
            reasoning_effort: effort,
            on_retry: &mut no_retry,
            on_uploaded: None,
            cancel_rx: None,
            route: None,
            previous_response_id: None,
            tool_results: &[],
            programmatic_tool_calling: false,
            max_output_tokens_override: None,
            no_retry: false,
        };
        build_responses_request_body(&config, &params, false).expect("body builds")
    };

    // Case 1: non-reasoning model, no effort — no `reasoning` object at
    // all. A top-level `reasoning_effort` or `include:
    // ["reasoning.summary"]` is a hard 400 on the Responses API, so
    // neither may appear.
    let (_url, body) = build("gpt-4o", None);
    assert!(body.get("reasoning").is_none(), "{body}");
    assert_eq!(body_keys(&body), ["input", "model", "store"]);

    // Case 2: reasoning model with effort "high" — effort and summary
    // both live under the nested `reasoning` object (never top-level).
    let (_url, body) = build("gpt-5.4", Some("high"));
    assert_eq!(body["reasoning"]["effort"], "high");
    assert_eq!(body["reasoning"]["summary"], "auto");
    assert_eq!(body_keys(&body), ["input", "model", "reasoning", "store"]);

    // Case 3: non-reasoning model handed a stray non-`off` effort — the
    // whole `reasoning` object must still be suppressed (gated on the
    // capability, not on effort being present). This pins the fix:
    // emitting `reasoning: { effort }` here would be a provider 400.
    let (_url, body) = build("gpt-4o", Some("high"));
    assert!(body.get("reasoning").is_none(), "{body}");
    assert_eq!(body_keys(&body), ["input", "model", "store"]);
}

#[test]
fn responses_request_serializes_with_store_true() {
    let req = ResponsesRequest {
        model: "gpt-4",
        input: Some(json!("hello")),
        instructions: None,
        tools: None,
        stream: false,
        max_output_tokens: None,
        reasoning: None,
        store: true,
        previous_response_id: None,
        parallel_tool_calls: None,
        tool_choice: None,
    };
    let value = serde_json::to_value(&req).unwrap();
    assert_eq!(value["store"], true);
    assert_eq!(value["model"], "gpt-4");
    assert_eq!(value["input"], "hello");
    // instructions and tools should be absent
    assert!(value.get("instructions").is_none());
    assert!(value.get("tools").is_none());
}

#[test]
fn responses_request_serializes_with_tools() {
    let req = ResponsesRequest {
        model: "gpt-4",
        input: Some(json!("hello")),
        instructions: None,
        tools: Some(vec![ResponsesTool {
            kind: "function".to_string(),
            name: "get_weather".to_string(),
            description: "Get the weather".to_string(),
            parameters: json!({"type": "object"}),
            strict: false,
            output_schema: None,
            allowed_callers: None,
        }]),
        stream: false,
        max_output_tokens: None,
        reasoning: None,
        store: false,
        previous_response_id: None,
        parallel_tool_calls: None,
        tool_choice: None,
    };
    let value = serde_json::to_value(&req).unwrap();
    let tools = value["tools"].as_array().unwrap();
    assert_eq!(tools.len(), 1);
    assert_eq!(tools[0]["type"], "function");
    assert_eq!(tools[0]["name"], "get_weather");
}

#[test]
fn responses_request_serializes_with_programmatic_tool_calling() {
    let req = ResponsesRequest {
        model: "gpt-4",
        input: Some(json!("hello")),
        instructions: None,
        tools: Some(vec![ResponsesTool {
            kind: "programmatic_tool_calling".to_string(),
            name: String::new(),
            description: String::new(),
            parameters: serde_json::Value::Null,
            strict: false,
            output_schema: None,
            allowed_callers: None,
        }]),
        stream: false,
        max_output_tokens: None,
        reasoning: None,
        store: false,
        previous_response_id: None,
        parallel_tool_calls: None,
        tool_choice: None,
    };
    let value = serde_json::to_value(&req).unwrap();
    let tools = value["tools"].as_array().unwrap();
    assert_eq!(tools.len(), 1);
    assert_eq!(tools[0]["type"], "programmatic_tool_calling");
    // Only the type field should be present
    assert_eq!(tools[0].as_object().unwrap().len(), 1);
}

#[test]
fn responses_request_omits_optional_fields_when_none() {
    let req = ResponsesRequest {
        model: "gpt-4",
        input: None,
        instructions: None,
        tools: None,
        stream: false,
        max_output_tokens: None,
        reasoning: None,
        store: false,
        previous_response_id: None,
        parallel_tool_calls: None,
        tool_choice: None,
    };
    let value = serde_json::to_value(&req).unwrap();
    let obj = value.as_object().unwrap();
    // Only `model` should be present (stream and store are skipped when false)
    assert_eq!(obj.len(), 1);
    assert_eq!(value["model"], "gpt-4");
    assert!(obj.get("instructions").is_none());
    assert!(obj.get("previous_response_id").is_none());
    assert!(obj.get("parallel_tool_calls").is_none());
    assert!(obj.get("tool_choice").is_none());
}

#[test]
fn responses_response_deserializes_with_message_and_usage() {
    let json_str = r#"
            {
                "id": "resp_123",
                "status": "completed",
                "output": [
                    {
                        "type": "message",
                        "role": "assistant",
                        "content": [
                            {"type": "output_text", "text": "Hello there"}
                        ]
                    }
                ],
                "usage": {
                    "prompt_tokens": 10,
                    "completion_tokens": 5,
                    "total_tokens": 15
                }
            }
        "#;
    let response: ResponsesResponse = serde_json::from_str(json_str).unwrap();
    assert_eq!(response.id.as_deref(), Some("resp_123"));
    assert_eq!(response.status.as_deref(), Some("completed"));
    assert_eq!(response.output.len(), 1);
    assert!(response.usage.is_some());
    let usage = response.usage.unwrap();
    assert_eq!(usage.prompt_tokens, 10);
    assert_eq!(usage.completion_tokens, 5);
    assert_eq!(usage.total_tokens, 15);
}

#[test]
fn response_output_item_deserializes_function_call() {
    let json_str =
        r#"{"type":"function_call","call_id":"call_1","name":"get_weather","arguments":"{}"}"#;
    let item: ResponseOutputItem = serde_json::from_str(json_str).unwrap();
    match item {
        ResponseOutputItem::FunctionCall {
            call_id,
            name,
            arguments,
            caller,
        } => {
            assert_eq!(call_id, "call_1");
            assert_eq!(name, "get_weather");
            assert_eq!(arguments, "{}");
            assert!(caller.is_none());
        }
        other => panic!("expected FunctionCall, got {other:?}"),
    }
}

#[test]
fn response_output_item_deserializes_reasoning() {
    let json_str = r#"{"type":"reasoning","summary":[{"text":"thinking..."}]}"#;
    let item: ResponseOutputItem = serde_json::from_str(json_str).unwrap();
    match item {
        ResponseOutputItem::Reasoning {
            id,
            summary,
            encrypted_content,
        } => {
            assert!(id.is_none());
            assert_eq!(summary.len(), 1);
            assert_eq!(summary[0]["text"], "thinking...");
            assert!(encrypted_content.is_none());
        }
        other => panic!("expected Reasoning, got {other:?}"),
    }
}

// ── Reasoning artifact capture (phase 2c) ───────────────────────────

#[test]
fn non_streaming_captures_reasoning_items_artifact() {
    // A stateless-mode response carries `encrypted_content` on the
    // reasoning item — it must ride along in the artifact verbatim.
    let json_str = r#"{
            "id": "resp_abc",
            "output": [
                {
                    "type": "reasoning",
                    "id": "re_1",
                    "summary": [{"type": "summary_text", "text": "think carefully"}],
                    "encrypted_content": "eJxT_opaque"
                },
                {
                    "type": "message",
                    "role": "assistant",
                    "content": [{"type": "output_text", "text": "Here is the answer"}]
                }
            ]
        }"#;
    let payload: ResponsesResponse = serde_json::from_str(json_str).unwrap();
    let result = responses_response_to_turn(payload).unwrap();
    match result {
        ChatTurnResult::FinalText(f) => {
            assert_eq!(f.content, "Here is the answer");
            // Display text unchanged — the artifact is additive.
            assert_eq!(f.reasoning.as_deref(), Some("think carefully"));
            let expected = serde_json::to_vec(&json!([
                {
                    "type": "reasoning",
                    "id": "re_1",
                    "summary": [{"type": "summary_text", "text": "think carefully"}],
                    "encrypted_content": "eJxT_opaque"
                }
            ]))
            .unwrap();
            assert_eq!(
                f.reasoning_artifact,
                Some(ReasoningArtifact::ResponsesItems(expected))
            );
        }
        other => panic!("expected FinalText, got {other:?}"),
    }
}

#[test]
fn non_streaming_captures_artifact_on_tool_use() {
    let json_str = r#"{
            "id": "resp_123",
            "output": [
                {"type": "reasoning", "id": "re_1", "summary": [{"text": "picking tool"}]},
                {"type": "function_call", "call_id": "call_1", "name": "get_weather", "arguments": "{\"city\":\"London\"}"}
            ]
        }"#;
    let payload: ResponsesResponse = serde_json::from_str(json_str).unwrap();
    let result = responses_response_to_turn(payload).unwrap();
    match result {
        ChatTurnResult::ToolUse(t) => {
            assert_eq!(t.reasoning.as_deref(), Some("picking tool"));
            // Response id still captured (unchanged behavior).
            assert_eq!(t.response_id.as_deref(), Some("resp_123"));
            let expected = serde_json::to_vec(&json!([
                {"type": "reasoning", "id": "re_1", "summary": [{"text": "picking tool"}]}
            ]))
            .unwrap();
            assert_eq!(
                t.reasoning_artifact,
                Some(ReasoningArtifact::ResponsesItems(expected))
            );
        }
        other => panic!("expected ToolUse, got {other:?}"),
    }
}

#[test]
fn non_streaming_captures_unknown_reasoning_fields_verbatim() {
    // A newer reasoning item shape may carry reasoning text in a `content`
    // array alongside `summary`. The raw-value capture must preserve the
    // unknown field verbatim in the artifact — the previous typed
    // re-serialization would have dropped it (and the display text in
    // `summary` is still picked up for `reasoning`).
    let json_str = r#"{
            "id": "resp_new",
            "output": [
                {
                    "type": "reasoning",
                    "id": "re_new",
                    "summary": [{"type": "summary_text", "text": "visible summary"}],
                    "content": [{"type": "output_text", "text": "thinking"}]
                },
                {
                    "type": "message",
                    "role": "assistant",
                    "content": [{"type": "output_text", "text": "Answer here"}]
                }
            ]
        }"#;
    let payload: ResponsesResponse = serde_json::from_str(json_str).unwrap();
    let result = responses_response_to_turn(payload).unwrap();
    match result {
        ChatTurnResult::FinalText(f) => {
            assert_eq!(f.content, "Answer here");
            // Display text still comes from `summary`.
            assert_eq!(f.reasoning.as_deref(), Some("visible summary"));
            let ReasoningArtifact::ResponsesItems(bytes) =
                f.reasoning_artifact.expect("artifact captured")
            else {
                panic!("expected ResponsesItems artifact");
            };
            let items: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
            // The unknown `content` field survives verbatim alongside the
            // known ones — nothing is dropped or re-ordered.
            assert_eq!(items[0]["type"], "reasoning");
            assert_eq!(items[0]["id"], "re_new");
            assert_eq!(items[0]["summary"][0]["text"], "visible summary");
            assert_eq!(items[0]["content"][0]["type"], "output_text");
            assert_eq!(items[0]["content"][0]["text"], "thinking");
        }
        other => panic!("expected FinalText, got {other:?}"),
    }
}

#[test]
fn non_streaming_no_reasoning_yields_none_artifact() {
    // Control case: a response with no reasoning output items.
    let json_str = r#"{
            "output": [
                {"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": "plain"}]}
            ]
        }"#;
    let payload: ResponsesResponse = serde_json::from_str(json_str).unwrap();
    let result = responses_response_to_turn(payload).unwrap();
    match result {
        ChatTurnResult::FinalText(f) => {
            assert_eq!(f.content, "plain");
            assert!(f.reasoning.is_none());
            assert!(f.reasoning_artifact.is_none());
        }
        other => panic!("expected FinalText, got {other:?}"),
    }
}

#[test]
fn merge_reasoning_item_dedupes_by_id_keeping_later() {
    // Providers emit `output_item.added` (item opens) then
    // `output_item.done` (item complete) for the same reasoning item.
    // The later, complete value must win.
    let mut items: Vec<serde_json::Value> = Vec::new();
    merge_reasoning_item(
        &mut items,
        json!({ "id": "re_1", "type": "reasoning", "summary": [] }),
    );
    merge_reasoning_item(
        &mut items,
        json!({
            "id": "re_1",
            "type": "reasoning",
            "summary": [{"text": "final summary"}],
            "encrypted_content": "eJxT_done"
        }),
    );
    merge_reasoning_item(
        &mut items,
        json!({ "id": "re_2", "type": "reasoning", "summary": [] }),
    );
    assert_eq!(items.len(), 2);
    assert_eq!(items[0]["id"], "re_1");
    assert_eq!(items[0]["encrypted_content"], "eJxT_done");
    assert_eq!(items[1]["id"], "re_2");
}

#[test]
fn merge_reasoning_item_without_id_is_appended() {
    let mut items: Vec<serde_json::Value> = Vec::new();
    merge_reasoning_item(&mut items, json!({ "type": "reasoning", "summary": [1] }));
    merge_reasoning_item(&mut items, json!({ "type": "reasoning", "summary": [2] }));
    assert_eq!(items.len(), 2);
}

#[test]
fn responses_items_artifact_serializes_exact_bytes() {
    // The artifact payload is the serde_json serialization of the
    // collected items — pinned byte-exact so a serde refactor cannot
    // silently change the round-trip payload.
    let items = vec![json!({ "type": "reasoning", "id": "re_1" })];
    let artifact = responses_items_artifact(&items).unwrap().unwrap();
    assert_eq!(
        artifact,
        ReasoningArtifact::ResponsesItems(br#"[{"id":"re_1","type":"reasoning"}]"#.to_vec())
    );
    // Empty collection -> None.
    assert!(responses_items_artifact(&[]).unwrap().is_none());
}

#[test]
fn responses_items_artifact_keeps_encrypted_content() {
    // The opaque encrypted payload must survive the serialize step
    // uninterpreted — this is the stateless-mode continuity contract.
    let items = vec![json!({
        "type": "reasoning",
        "id": "re_9",
        "summary": [],
        "encrypted_content": "eJxT_opaque_bytes"
    })];
    let artifact = responses_items_artifact(&items).unwrap().unwrap();
    let decoded: serde_json::Value = match artifact {
        ReasoningArtifact::ResponsesItems(bytes) => serde_json::from_slice(&bytes).unwrap(),
        other => panic!("expected ResponsesItems, got {other:?}"),
    };
    assert_eq!(decoded[0]["encrypted_content"], "eJxT_opaque_bytes");
}

#[test]
fn streaming_reasoning_item_events_flow_into_artifact_bytes() {
    // End-to-end for the streaming capture path without HTTP: feed
    // canned SSE event data through the parser + merge helper, exactly
    // as the streaming loop does, and assert the artifact bytes match.
    let mut items: Vec<serde_json::Value> = Vec::new();
    let added = parse_responses_stream_event(
            r#"{"type":"response.output_item.added","output_index":0,"item":{"id":"re_1","type":"reasoning","summary":[]}}"#,
        )
        .unwrap()
        .expect("event");
    let done = parse_responses_stream_event(
            r#"{"type":"response.output_item.done","output_index":0,"item":{"id":"re_1","type":"reasoning","summary":[{"text":"streamed reasoning"}],"encrypted_content":"eJxT_stream"}}"#,
        )
        .unwrap()
        .expect("event");
    match (added, done) {
        (ResponsesStreamEvent::ReasoningItem(a), ResponsesStreamEvent::ReasoningItem(d)) => {
            merge_reasoning_item(&mut items, a);
            merge_reasoning_item(&mut items, d);
        }
        other => panic!("expected two ReasoningItem events, got {other:?}"),
    }
    let artifact = responses_items_artifact(&items).unwrap().unwrap();
    let expected = serde_json::to_vec(&json!([{
        "id": "re_1",
        "type": "reasoning",
        "summary": [{"text": "streamed reasoning"}],
        "encrypted_content": "eJxT_stream"
    }]))
    .unwrap();
    assert_eq!(artifact, ReasoningArtifact::ResponsesItems(expected));
}

#[test]
fn response_output_item_deserializes_program() {
    let json_str = r#"{"type":"program","call_id":"prog_1","code":"console.log('hello')","fingerprint":"fp_1"}"#;
    let item: ResponseOutputItem = serde_json::from_str(json_str).unwrap();
    match item {
        ResponseOutputItem::Program {
            call_id,
            code,
            id,
            fingerprint,
        } => {
            assert_eq!(call_id, "prog_1");
            assert_eq!(code.as_deref(), Some("console.log('hello')"));
            assert_eq!(fingerprint.as_deref(), Some("fp_1"));
            assert!(id.is_none());
        }
        other => panic!("expected Program, got {other:?}"),
    }
}

#[test]
fn response_output_item_deserializes_program_output() {
    let json_str =
        r#"{"type":"program_output","call_id":"prog_1","result":"ok","status":"completed"}"#;
    let item: ResponseOutputItem = serde_json::from_str(json_str).unwrap();
    match item {
        ResponseOutputItem::ProgramOutput {
            call_id,
            result,
            status,
            id,
        } => {
            assert_eq!(call_id, "prog_1");
            assert_eq!(result.as_deref(), Some("ok"));
            assert_eq!(status.as_deref(), Some("completed"));
            assert!(id.is_none());
        }
        other => panic!("expected ProgramOutput, got {other:?}"),
    }
}

// ── Input building tests ──────────────────────────────────────────

#[test]
fn build_responses_input_empty_returns_empty_array() {
    let result = build_responses_input(&[], &[], None).unwrap();
    let value = result.expect("expected Some value");
    // Empty messages produce an empty-string input
    assert_eq!(value, json!(""));
}

#[test]
fn build_responses_input_with_tool_results() {
    let tool_results = vec![ToolResultItem {
        call_id: "call_1".to_string(),
        output: "weather_data".to_string(),
        caller: None,
    }];
    // The tool-results branch ignores messages and the chain id entirely:
    // the server reconstructs history from `previous_response_id`.
    let result = build_responses_input(&tool_results, &[], Some("resp_1")).unwrap();
    let value = result.expect("expected Some value");
    let arr = value.as_array().unwrap();
    assert_eq!(arr.len(), 1);
    assert_eq!(arr[0]["type"], "function_call_output");
    assert_eq!(arr[0]["call_id"], "call_1");
    assert_eq!(arr[0]["output"], "weather_data");
}

#[test]
fn build_responses_input_with_messages() {
    let messages = vec![
        ChatRequestMessage::simple("user", "hello".into()),
        ChatRequestMessage::simple("assistant", "hi there".into()),
    ];
    let result = build_responses_input(&[], &messages, None).unwrap();
    let value = result.expect("expected Some value");
    let arr = value.as_array().unwrap();
    assert_eq!(arr.len(), 2);
    assert_eq!(arr[0]["role"], "user");
    assert_eq!(arr[0]["content"], "hello");
    assert_eq!(arr[1]["role"], "assistant");
    assert_eq!(arr[1]["content"], "hi there");
}

#[test]
fn build_responses_input_chained_turn_sends_only_trailing_messages() {
    // Chaining a fresh user turn via `previous_response_id`: the server
    // already holds everything up to the last assistant response, so
    // resending the full history would duplicate it. Only the trailing
    // user message (+ any system prompt) may be sent.
    let messages = vec![
        ChatRequestMessage::simple("system", "You are a helpful assistant.".into()),
        ChatRequestMessage::simple("user", "turn one".into()),
        ChatRequestMessage::simple("assistant", "old answer".into()),
        ChatRequestMessage::simple("user", "turn two".into()),
    ];
    let result = build_responses_input(&[], &messages, Some("resp_1")).unwrap();
    let value = result.expect("expected Some value");
    let arr = value.as_array().unwrap();
    // system + the new user message only — the old turns are chained
    // server-side and must not be replayed.
    assert_eq!(arr.len(), 2);
    assert_eq!(arr[0]["role"], "system");
    assert_eq!(arr[1]["role"], "user");
    assert_eq!(arr[1]["content"], "turn two");
}

#[test]
fn build_responses_input_chained_turn_keeps_system_prompt() {
    // The system prompt is rebuilt by the daemon on every request (skills,
    // working dir), so the newest one must still reach the model even
    // though the chain carries the old one — system messages survive the
    // trailing filter.
    let messages = vec![
        ChatRequestMessage::simple("system", "old system".into()),
        ChatRequestMessage::simple("user", "turn one".into()),
        ChatRequestMessage::simple("assistant", "answer".into()),
    ];
    let result = build_responses_input(&[], &messages, Some("resp_1")).unwrap();
    let value = result.expect("expected Some value");
    let arr = value.as_array().unwrap();
    assert_eq!(arr.len(), 1);
    assert_eq!(arr[0]["role"], "system");
    assert_eq!(arr[0]["content"], "old system");
}

#[test]
fn build_responses_input_chained_without_assistant_keeps_all() {
    // Defensive: a chain id with no assistant message yet — nothing has
    // been chained server-side, so every message must be kept.
    let messages = vec![
        ChatRequestMessage::simple("user", "hello".into()),
        ChatRequestMessage::simple("user", "again".into()),
    ];
    let result = build_responses_input(&[], &messages, Some("resp_1")).unwrap();
    let value = result.expect("expected Some value");
    let arr = value.as_array().unwrap();
    assert_eq!(arr.len(), 2);
}

#[test]
fn build_responses_input_chained_keeps_only_newest_system_message() {
    // If the caller ever accumulates more than one system message, only the
    // newest may be sent: the older one is already in the chained response's
    // context and resending it would duplicate it (billing + context
    // inflation). The newest system message leads the input.
    let messages = vec![
        ChatRequestMessage::simple("system", "old system".into()),
        ChatRequestMessage::simple("user", "turn one".into()),
        ChatRequestMessage::simple("assistant", "answer".into()),
        ChatRequestMessage::simple("system", "new system".into()),
        ChatRequestMessage::simple("user", "turn two".into()),
    ];
    let result = build_responses_input(&[], &messages, Some("resp_1")).unwrap();
    let value = result.expect("expected Some value");
    let arr = value.as_array().unwrap();
    assert_eq!(arr.len(), 2);
    assert_eq!(arr[0]["role"], "system");
    assert_eq!(arr[0]["content"], "new system");
    assert_eq!(arr[1]["role"], "user");
    assert_eq!(arr[1]["content"], "turn two");
}

#[test]
fn build_responses_input_chained_preserves_mid_tail_order() {
    // Defensive shape: a system prompt appearing INSIDE the tail (after a
    // user message) must stay in its original position rather than being
    // hoisted to the front — the conversation order is preserved. The
    // pre-existing system message at the head is dropped (older prompt,
    // already in the chained context).
    let messages = vec![
        ChatRequestMessage::simple("system", "old system".into()),
        ChatRequestMessage::simple("user", "turn one".into()),
        ChatRequestMessage::simple("assistant", "answer".into()),
        ChatRequestMessage::simple("user", "mid".into()),
        ChatRequestMessage::simple("system", "late system".into()),
        ChatRequestMessage::simple("user", "turn two".into()),
    ];
    let result = build_responses_input(&[], &messages, Some("resp_1")).unwrap();
    let value = result.expect("expected Some value");
    let arr = value.as_array().unwrap();
    assert_eq!(arr.len(), 3);
    assert_eq!(arr[0]["role"], "user");
    assert_eq!(arr[0]["content"], "mid");
    assert_eq!(arr[1]["role"], "system");
    assert_eq!(arr[1]["content"], "late system");
    assert_eq!(arr[2]["role"], "user");
    assert_eq!(arr[2]["content"], "turn two");
}

// ── Reasoning extraction tests ────────────────────────────────────

#[test]
fn extract_reasoning_text_plain_strings() {
    let input = vec![
        serde_json::Value::String("think".into()),
        serde_json::Value::String("more".into()),
    ];
    assert_eq!(extract_reasoning_text(&input), Some("think more".into()));
}

#[test]
fn extract_reasoning_text_objects_with_text_field() {
    let input = vec![json!({"text": "thinking..."})];
    assert_eq!(extract_reasoning_text(&input), Some("thinking...".into()));
}

#[test]
fn extract_reasoning_text_mixed() {
    let input = vec![json!("first"), json!({"text": "second"}), json!("third")];
    assert_eq!(
        extract_reasoning_text(&input),
        Some("first second third".into())
    );
}

#[test]
fn extract_reasoning_text_empty_returns_none() {
    let input: Vec<serde_json::Value> = vec![];
    assert_eq!(extract_reasoning_text(&input), None);
}

// ── ResponsesTool tests ───────────────────────────────────────────

#[test]
fn responses_tool_from_chat_tool_definition() {
    let tool_def =
        ChatToolDefinition::function("test_func", "A test function", json!({"type": "object"}));
    let tool = ResponsesTool::from(&tool_def);
    assert_eq!(tool.kind, "function");
    assert_eq!(tool.name, "test_func");
    assert_eq!(tool.description, "A test function");
    assert_eq!(tool.parameters, json!({"type": "object"}));
    assert!(!tool.strict);
    assert!(tool.output_schema.is_none());
    assert!(tool.allowed_callers.is_none());
}

#[test]
fn responses_tool_serializes_programmatic_only_as_type_only() {
    let tool = ResponsesTool {
        kind: "programmatic_tool_calling".to_string(),
        name: String::new(),
        description: String::new(),
        parameters: serde_json::Value::Null,
        strict: false,
        output_schema: None,
        allowed_callers: None,
    };
    let value = serde_json::to_value(&tool).unwrap();
    let obj = value.as_object().unwrap();
    assert_eq!(obj.len(), 1);
    assert_eq!(value["type"], "programmatic_tool_calling");
}

// ── AccCall tests ─────────────────────────────────────────────────

#[test]
fn acc_call_new_sets_index() {
    let call = AccCall::new(5);
    assert_eq!(call.index, 5);
    assert!(call.name.is_none());
    assert_eq!(call.arguments, "");
}

// ── Request body building tests ───────────────────────────────────

#[test]
fn build_simple_responses_body_sets_store_false() {
    let config = super::super::ServiceConfig {
        base_url: "https://api.openai.com/v1".into(),
        ..Default::default()
    };
    let (_url, body) = build_simple_responses_body(&config, "gpt-4", "hello", false).unwrap();
    // store: false is skipped via skip_serializing_if — the field
    // is absent rather than explicit false.
    assert!(
        body.get("store").is_none(),
        "store should be absent when false"
    );
    assert_eq!(body["model"], "gpt-4");
    assert_eq!(body["input"], "hello");
    assert!(body.get("tools").is_none());
    assert!(body.get("instructions").is_none());
}

#[test]
fn build_simple_responses_body_sets_stream() {
    let config = super::super::ServiceConfig {
        base_url: "https://api.openai.com/v1".into(),
        ..Default::default()
    };
    let (_url, body) = build_simple_responses_body(&config, "gpt-4", "hello", true).unwrap();
    assert_eq!(body["stream"], true);
}

// ── Full response deserialisation integration ─────────────────────

#[test]
fn responses_response_deserializes_empty_output() {
    let result: ResponsesResponse = serde_json::from_str(r#"{"output": []}"#).unwrap();
    assert_eq!(result.output, [] as [serde_json::Value; 0]);
    assert!(result.id.is_none());
    assert!(result.status.is_none());
    assert!(result.usage.is_none());
}

#[test]
fn responses_response_deserializes_multiple_items() {
    let json_str = r#"
            {
                "output": [
                    {
                        "type": "message",
                        "role": "assistant",
                        "content": [
                            {"type": "output_text", "text": "Let me check"}
                        ]
                    },
                    {
                        "type": "function_call",
                        "call_id": "call_1",
                        "name": "get_weather",
                        "arguments": "{\"city\":\"London\"}"
                    }
                ]
            }
        "#;
    let response: ResponsesResponse = serde_json::from_str(json_str).unwrap();
    assert_eq!(response.output.len(), 2);
}

// ── Reasoning artifact re-emission (phase 4a) ────────────────────────

#[test]
fn messages_to_responses_input_reemits_reasoning_items() {
    // An assistant message carrying the opaque ResponsesItems artifact must
    // replay those items verbatim into `input`, ahead of the message
    // content item (provider output ordering).
    let msg = ChatRequestMessage {
        role: "assistant",
        content: Some("Here is the answer".to_string()),
        images: Vec::new(),
        tool_call_id: None,
        tool_calls: None,
        reasoning_artifact: Some(ReasoningArtifact::ResponsesItems(
            br#"[{"type":"reasoning","id":"re_1","summary":[{"text":"think carefully"}]}]"#
                .to_vec(),
        )),
    };
    let items = messages_to_responses_input(&[msg]).unwrap();
    assert_eq!(items.len(), 2);
    assert_eq!(items[0]["type"], "reasoning");
    assert_eq!(items[0]["id"], "re_1");
    assert_eq!(items[0]["summary"][0]["text"], "think carefully");
    assert_eq!(items[1]["type"], "message");
    assert_eq!(items[1]["content"], "Here is the answer");
    // `reasoning_content` is chat-completions-only — never on Responses
    // messages (neither the replayed reasoning items nor the message).
    assert!(items[0].get("reasoning_content").is_none());
    assert!(items[1].get("reasoning_content").is_none());
}

#[test]
fn messages_to_responses_input_reasoning_items_preserve_order() {
    // Multiple reasoning items + tool calls: item order must be preserved.
    let msg = ChatRequestMessage {
            role: "assistant",
            content: Some("Let me check".to_string()),
            images: Vec::new(),
            tool_call_id: None,
            tool_calls: None,
            reasoning_artifact: Some(ReasoningArtifact::ResponsesItems(
                br#"[{"type":"reasoning","id":"re_1"},{"type":"reasoning","id":"re_2","encrypted_content":"eJxT_opaque"}]"#
                    .to_vec(),
            )),
        };
    let items = messages_to_responses_input(&[msg]).unwrap();
    assert_eq!(items.len(), 3);
    assert_eq!(items[0]["id"], "re_1");
    assert_eq!(items[1]["id"], "re_2");
    assert_eq!(items[1]["encrypted_content"], "eJxT_opaque");
    assert_eq!(items[2]["type"], "message");
}

#[test]
fn messages_to_responses_input_no_artifact_plain_message() {
    // Control: no artifact → plain message item, no reasoning on the wire.
    let msg = ChatRequestMessage::simple("assistant", "plain".into());
    let items = messages_to_responses_input(&[msg]).unwrap();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["type"], "message");
    assert!(items[0].get("reasoning_content").is_none());
    assert!(items[0].get("reasoning_artifact").is_none());
}

#[test]
fn messages_to_responses_input_skips_empty_assistant_content() {
    // A blank assistant turn produces no message item: an empty text item is
    // meaningless and rejects on strict endpoints.
    let msg = ChatRequestMessage::simple("assistant", "  \n ".into());
    let items = messages_to_responses_input(&[msg]).unwrap();
    assert!(items.is_empty(), "{items:?}");
}

#[test]
fn messages_to_responses_input_keeps_reasoning_items_for_an_empty_turn() {
    // The message item is skipped, but the replayed reasoning items still
    // ride — continuity is the payload, not the empty content.
    let msg = ChatRequestMessage {
        role: "assistant",
        content: Some(String::new()),
        images: Vec::new(),
        tool_call_id: None,
        tool_calls: None,
        reasoning_artifact: Some(ReasoningArtifact::ResponsesItems(
            br#"[{"type":"reasoning","id":"rs_1","encrypted_content":"eJxT"}]"#.to_vec(),
        )),
    };
    let items = messages_to_responses_input(&[msg]).unwrap();
    assert_eq!(items.len(), 1, "{items:?}");
    assert_eq!(items[0]["type"], "reasoning");
}

#[test]
#[serial_test::serial(catalog)]
fn responses_max_output_tokens_prefers_the_override() {
    // A per-call cap (the cache-warming ping's 1-token cap) must win over
    // the configured output cap on the wire; `None` keeps the config value.
    let config = super::super::ServiceConfig {
        provider_slug: "openai".to_string(),
        responses_max_output_tokens: Some(4096),
        ..Default::default()
    };
    let build = |override_: Option<u32>| {
        let mut no_retry = None;
        let params = super::super::TurnParams {
            model: "gpt-5.4",
            messages: &[],
            tools: &[],
            reasoning_effort: None,
            on_retry: &mut no_retry,
            on_uploaded: None,
            cancel_rx: None,
            route: None,
            previous_response_id: None,
            tool_results: &[],
            programmatic_tool_calling: false,
            max_output_tokens_override: override_,
            no_retry: false,
        };
        build_responses_request_body(&config, &params, false)
            .expect("body builds")
            .1
    };
    assert_eq!(build(Some(1))["max_output_tokens"], 1);
    assert_eq!(build(None)["max_output_tokens"], 4096);
}
