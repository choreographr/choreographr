//! Response → turn conversion: the display text and tool calls extracted from an
//! output array, the opaque reasoning items captured into a round-trip artifact,
//! and the `incomplete` classification that decides truncation from refusal.

// The suite repeats its parent's imports one level deeper: this module lives
// inside `tests`, so the adapter is two `super`s up and `openai` three.
use super::super::super::*;
use super::super::*;
use choreo_proto::{ReasoningArtifact, TokenUsage};
use serde_json::json;

#[test]
fn responses_turn_projects_input_tokens_details_into_cached_tokens() {
    // The Responses API reports prompt-cache hits under
    // `usage.input_tokens_details.cached_tokens`; the shared projection must
    // carry that count onto `TokenUsage.cached_tokens` without inflating the
    // input count (the cached tokens are already part of it).
    let json_str = r#"{
            "id": "resp_cached",
            "status": "completed",
            "output": [
                {"type": "message", "role": "assistant",
                 "content": [{"type": "output_text", "text": "hi"}]}
            ],
            "usage": {
                "input_tokens": 1000,
                "output_tokens": 50,
                "total_tokens": 1050,
                "input_tokens_details": {"cached_tokens": 900, "cache_write_tokens": 0}
            }
        }"#;
    let payload: ResponsesResponse = serde_json::from_str(json_str).unwrap();
    let ChatTurnResult::FinalText(result) = responses_response_to_turn(payload).expect("turn")
    else {
        panic!("expected FinalText");
    };
    assert_eq!(
        result.usage,
        Some(TokenUsage {
            input_tokens: 1000,
            output_tokens: 50,
            total_tokens: 1050,
            cached_tokens: 900,
            cache_write_tokens: 0,
        })
    );
}

#[test]
fn a_reasoning_only_response_is_output_not_an_empty_response() {
    // The provider produced (and billed) a reasoning item and no visible
    // answer text. That is model output, so the turn is a `FinalText` with
    // empty content that still carries the round-trip artifact — never an
    // `EmptyResponse` error, which would discard the payload and report the
    // paid turn as a failure.
    let json_str = r#"{
            "id": "resp_only",
            "status": "completed",
            "output": [
                {"type": "reasoning", "id": "re_1", "summary": [], "encrypted_content": "eJxT_opaque"}
            ]
        }"#;
    let payload: ResponsesResponse = serde_json::from_str(json_str).unwrap();
    let result = responses_response_to_turn(payload).expect("reasoning-only turn is not empty");
    match result {
        ChatTurnResult::FinalText(f) => {
            assert_eq!(f.content, "");
            assert!(f.reasoning.is_none(), "an encrypted item has no text");
            assert!(
                matches!(
                    f.reasoning_artifact,
                    Some(ReasoningArtifact::ResponsesItems(_))
                ),
                "the captured item rides the artifact",
            );
        }
        other => panic!("expected FinalText, got {other:?}"),
    }
}

#[test]
fn an_unmodelled_output_item_kind_still_yields_the_surrounding_answer() {
    // The stateless backend may add an output-item kind this client does not
    // model. That is data it does not understand, not a malformed answer, so the
    // turn must still carry the text the surrounding message item produced —
    // failing here would discard output the provider billed for, and the
    // streaming path already ignores events it does not model.
    let json_str = r#"{
            "id": "resp_unknown_item",
            "status": "completed",
            "output": [
                {"type": "some_future_item", "id": "item_1", "payload": {"k": 1}},
                {"type": "message", "role": "assistant",
                 "content": [{"type": "output_text", "text": "the answer"}]}
            ]
        }"#;
    let payload: ResponsesResponse = serde_json::from_str(json_str).unwrap();
    match responses_response_to_turn(payload).expect("the surrounding items answer") {
        ChatTurnResult::FinalText(f) => {
            // A final-text turn by construction carries no tool calls, so the
            // only thing to prove is that the message item's text survived.
            assert_eq!(f.content, "the answer");
        }
        other => panic!("expected FinalText, got {other:?}"),
    }
}

#[test]
fn a_response_without_content_or_reasoning_is_still_an_empty_response() {
    let payload: ResponsesResponse = serde_json::from_str(r#"{"id":"r","output":[]}"#).unwrap();
    assert!(matches!(
        responses_response_to_turn(payload),
        Err(OpenAiError::EmptyResponse)
    ));
}

#[test]
fn an_incomplete_response_reports_truncation_not_an_error() {
    // `status: incomplete` with `max_output_tokens` is precisely a
    // length-capped answer: the partial text rides a truncated `FinalText`,
    // the same reading chat gives `finish_reason: length`.
    let json_str = r#"{
            "id": "resp_cut",
            "status": "incomplete",
            "incomplete_details": {"reason": "max_output_tokens"},
            "output": [
                {"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": "The answer is"}]}
            ]
        }"#;
    let payload: ResponsesResponse = serde_json::from_str(json_str).unwrap();
    let result = responses_response_to_turn(payload).expect("partial output is a turn");
    match result {
        ChatTurnResult::FinalText(f) => {
            assert_eq!(f.content, "The answer is");
            assert!(f.truncated, "a length-capped answer is truncated");
        }
        other => panic!("expected FinalText, got {other:?}"),
    }
}

#[test]
fn an_incomplete_response_with_an_unclassified_reason_is_a_truncation() {
    // A reason this client does not model must not throw away output the
    // provider charged for; it is logged and the turn keeps its text.
    let json_str = r#"{
            "status": "incomplete",
            "incomplete_details": {"reason": "some_new_reason"},
            "output": [
                {"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": "partial"}]}
            ]
        }"#;
    let payload: ResponsesResponse = serde_json::from_str(json_str).unwrap();
    match responses_response_to_turn(payload).expect("turn") {
        ChatTurnResult::FinalText(f) => {
            assert_eq!(f.content, "partial");
            assert!(f.truncated);
        }
        other => panic!("expected FinalText, got {other:?}"),
    }
}

#[test]
fn an_incomplete_response_stopped_by_the_content_filter_is_terminal() {
    // A policy block is the one incomplete reason that is an error: retrying
    // the same request can never clear it.
    let json_str = r#"{
            "status": "incomplete",
            "incomplete_details": {"reason": "content_filter"},
            "output": []
        }"#;
    let payload: ResponsesResponse = serde_json::from_str(json_str).unwrap();
    assert!(matches!(
        responses_response_to_turn(payload),
        Err(OpenAiError::ContentFiltered { .. })
    ));
}

#[test]
fn a_completed_response_is_not_truncated() {
    let json_str = r#"{
            "status": "completed",
            "output": [
                {"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": "done"}]}
            ]
        }"#;
    let payload: ResponsesResponse = serde_json::from_str(json_str).unwrap();
    match responses_response_to_turn(payload).expect("turn") {
        ChatTurnResult::FinalText(f) => assert!(!f.truncated),
        other => panic!("expected FinalText, got {other:?}"),
    }
}

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
