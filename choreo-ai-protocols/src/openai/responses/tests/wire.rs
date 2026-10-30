//! Wire types: how a request body, an input item, a tool definition, and a
//! response envelope serialise to (and deserialise from) the Responses JSON.
//!
//! Everything here is a pure `serde` assertion — no request is built and no
//! provider is called — so a wire-shape regression names exactly the field that
//! moved.

// The suite repeats its parent's imports one level deeper: this module lives
// inside `tests`, so the adapter is two `super`s up and `openai` three.
use super::super::super::*;
use super::super::*;
use crate::CallerInfo;
use serde_json::json;

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
    // No attribution on this call: the field is omitted outright, never sent as
    // a null the API would have to interpret.
    assert!(value.get("caller").is_none());
}

#[test]
fn a_program_caller_serializes_on_both_item_kinds() {
    // The Responses API needs the `caller` copied back verbatim onto both the
    // replayed `function_call` and the `function_call_output` that answers it,
    // so the service can resume the program that generated the call (see
    // <https://developers.openai.com/api/docs/guides/tools-programmatic-tool-calling>).
    let caller = CallerInfo {
        kind: "program".to_string(),
        caller_id: "prog_123".to_string(),
    };
    let call = ResponsesInputItem::FunctionCall {
        call_id: "call_1".to_string(),
        name: "get_weather".to_string(),
        arguments: "{}".to_string(),
        caller: Some(caller.clone()),
    };
    let output = ResponsesInputItem::FunctionCallOutput {
        call_id: "call_1".to_string(),
        output: "72F".to_string(),
        caller: Some(caller),
    };
    let call = serde_json::to_value(&call).unwrap();
    let output = serde_json::to_value(&output).unwrap();
    assert_eq!(
        call["caller"],
        json!({"type": "program", "caller_id": "prog_123"})
    );
    assert_eq!(
        output["caller"],
        json!({"type": "program", "caller_id": "prog_123"})
    );
}

#[test]
fn no_caller_means_no_caller_field_on_either_item_kind() {
    // A history with no attribution must not send the field at all: an empty
    // `caller` object would be a value the API has to interpret.
    let call = ResponsesInputItem::FunctionCall {
        call_id: "call_1".to_string(),
        name: "get_weather".to_string(),
        arguments: "{}".to_string(),
        caller: None,
    };
    let output = ResponsesInputItem::FunctionCallOutput {
        call_id: "call_1".to_string(),
        output: "72F".to_string(),
        caller: None,
    };
    assert!(serde_json::to_value(&call).unwrap().get("caller").is_none());
    assert!(
        serde_json::to_value(&output)
            .unwrap()
            .get("caller")
            .is_none()
    );
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
        include: None,
        store: Some(true),
        previous_response_id: None,
        parallel_tool_calls: None,
        tool_choice: None,
        prompt_cache_key: None,
        session_id: None,
        prompt_cache_retention: None,
        prompt_cache_options: None,
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
fn responses_request_serializes_store_false_explicitly() {
    // `store` is tri-state: `Some(false)` is an explicit "don't persist" that
    // must reach the wire, and only `None` omits the field. A backend that
    // stores by default would otherwise persist a request that asked not to.
    let req = ResponsesRequest {
        model: "gpt-4",
        input: Some(json!("hello")),
        instructions: None,
        tools: None,
        stream: false,
        max_output_tokens: None,
        reasoning: None,
        include: None,
        store: Some(false),
        previous_response_id: None,
        parallel_tool_calls: None,
        tool_choice: None,
        prompt_cache_key: None,
        session_id: None,
        prompt_cache_retention: None,
        prompt_cache_options: None,
    };
    let value = serde_json::to_value(&req).unwrap();
    assert_eq!(value["store"], false, "{value}");
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
        include: None,
        store: None,
        previous_response_id: None,
        parallel_tool_calls: None,
        tool_choice: None,
        prompt_cache_key: None,
        session_id: None,
        prompt_cache_retention: None,
        prompt_cache_options: None,
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
        include: None,
        store: None,
        previous_response_id: None,
        parallel_tool_calls: None,
        tool_choice: None,
        prompt_cache_key: None,
        session_id: None,
        prompt_cache_retention: None,
        prompt_cache_options: None,
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
        include: None,
        store: None,
        previous_response_id: None,
        parallel_tool_calls: None,
        tool_choice: None,
        prompt_cache_key: None,
        session_id: None,
        prompt_cache_retention: None,
        prompt_cache_options: None,
    };
    let value = serde_json::to_value(&req).unwrap();
    let obj = value.as_object().unwrap();
    // Only `model` should be present (stream is skipped when false, store when None)
    assert_eq!(obj.len(), 1);
    assert_eq!(value["model"], "gpt-4");
    assert!(obj.get("instructions").is_none());
    assert!(obj.get("previous_response_id").is_none());
    assert!(obj.get("parallel_tool_calls").is_none());
    assert!(obj.get("tool_choice").is_none());
    assert!(obj.get("store").is_none());
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
                    "input_tokens": 10,
                    "output_tokens": 5,
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

#[test]
fn response_output_item_deserializes_an_unknown_kind() {
    // The stateless backend is undocumented and unversioned, so an item kind this
    // client does not model has to deserialize rather than fail the whole
    // response: one unmodelled item must not discard an answer the provider
    // billed for.
    let json_str = r#"{"type":"some_future_item","id":"item_1","payload":{"k":1}}"#;
    let item: ResponseOutputItem = serde_json::from_str(json_str).unwrap();
    assert!(
        matches!(item, ResponseOutputItem::Unknown),
        "an unmodelled item kind is Unknown, got {item:?}"
    );
}

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
