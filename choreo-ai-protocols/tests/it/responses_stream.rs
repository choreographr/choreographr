//! Responses-API streaming wire tests: the terminal-event shapes, the
//! truncation/refusal classification, and the reason a reasoning-only stream is
//! never reported as an empty response.
//!
//! The provider emits the documented event shapes — `response.completed` /
//! `response.incomplete` / `response.failed` each wrap the whole `Response`
//! object under `response` — so these tests are also what holds the parser to
//! that shape rather than to a flattened one.

#![expect(clippy::expect_used)]

use std::fmt::Write as _;

use choreo_ai_protocols::openai::OpenAiError;
use choreo_ai_protocols::openai::RequestFormat;
use choreo_ai_protocols::test_utils::MockProvider;
use choreo_ai_protocols::{
    ChatTurnRequest, ChatTurnResult, OpenAiClient, RequestAuth, SocketRegistry, StreamEvent,
};

use choreo_ai_protocols::openai::ServiceConfig;

/// Frame an ordered list of SSE `data:` payloads into one response body.
fn sse(events: &[serde_json::Value]) -> String {
    let mut body = String::new();
    for event in events {
        let _ = write!(body, "data: {event}\n\n");
    }
    body.push_str("data: [DONE]\n\n");
    body
}

/// A client on the Responses API pointed at `base_url`.
fn responses_client(base_url: String) -> OpenAiClient {
    OpenAiClient::new(
        ServiceConfig {
            base_url,
            provider_slug: "openai".to_string(),
            streaming: true,
            retry_max_attempts: 1,
            connect_timeout_secs: 5,
            request_timeout_secs: 30,
            total_timeout_secs: 60,
            default_request_format: RequestFormat::Responses,
            ..Default::default()
        },
        RequestAuth::api_key("test-key"),
        &SocketRegistry::new(),
    )
    .expect("openai client")
}

/// Drive one streaming turn against `mock`, collecting the display events.
fn stream_turn(
    mock: &MockProvider,
    events: &mut Vec<StreamEvent>,
) -> Result<ChatTurnResult, choreo_ai_protocols::openai::OpenAiError> {
    let client = responses_client(mock.base_url("v1"));
    client.chat_completion_turn_streaming(
        ChatTurnRequest {
            model: "gpt-5.4",
            messages: &[],
            tools: &[],
            thinking_effort: "high".to_string(),
            on_retry: &mut None,
            on_uploaded: None,
            cancel_rx: None,
            previous_response_id: None,
            programmatic_tool_calling: false,
            session_id: "1".to_string(),
            request_id: "1".to_string(),
            max_output_tokens_override: None,
            no_retry: false,
        },
        |event| {
            events.push(event);
            Ok(())
        },
    )
}

#[test]
#[ignore = "integration"]
fn reasoning_only_stream_is_a_turn_not_an_empty_response() {
    // An encrypted reasoning item and no answer text: billed output, so the
    // turn must be a `FinalText` carrying the artifact — and the id from the
    // *wrapped* `response` object is what re-arms the continuity chain.
    let body = sse(&[
        serde_json::json!({
            "type": "response.output_item.done",
            "output_index": 0,
            "item": {
                "id": "rs_1",
                "type": "reasoning",
                "summary": [],
                "encrypted_content": "eJxT_opaque"
            }
        }),
        serde_json::json!({
            "type": "response.completed",
            "response": {
                "id": "resp_1",
                "status": "completed",
                "usage": {"input_tokens": 5, "output_tokens": 40, "total_tokens": 45}
            }
        }),
    ]);
    let mock = MockProvider::start(vec![(200, "text/event-stream", body)]);
    let mut events = Vec::new();
    let result = stream_turn(&mock, &mut events).expect("a reasoning-only stream is a turn");
    let ChatTurnResult::FinalText(text) = result else {
        panic!("expected FinalText, got {result:?}");
    };
    assert_eq!(text.content, "");
    assert!(!text.truncated);
    assert_eq!(text.response_id.as_deref(), Some("resp_1"));
    assert!(
        text.reasoning_artifact.is_some(),
        "the reasoning item is captured for the round trip",
    );
    let usage = text.usage.expect("usage from the wrapped response object");
    assert_eq!(usage.total_tokens, 45);
}

#[test]
#[ignore = "integration"]
fn streaming_reasoning_summary_text_is_displayed() {
    // The summary text arrives as `response.reasoning_summary_text.delta`
    // events; that is what the session shows live.
    let body = sse(&[
        serde_json::json!({
            "type": "response.reasoning_summary_text.delta",
            "item_id": "rs_1",
            "delta": "step one"
        }),
        serde_json::json!({
            "type": "response.output_text.delta",
            "delta": "42"
        }),
        serde_json::json!({
            "type": "response.completed",
            "response": {"id": "resp_2", "status": "completed"}
        }),
    ]);
    let mock = MockProvider::start(vec![(200, "text/event-stream", body)]);
    let mut events = Vec::new();
    let result = stream_turn(&mock, &mut events).expect("turn");
    let ChatTurnResult::FinalText(text) = result else {
        panic!("expected FinalText, got {result:?}");
    };
    assert_eq!(text.content, "42");
    assert_eq!(text.reasoning.as_deref(), Some("step one"));
    assert!(
        events.iter().any(|event| matches!(
            event,
            StreamEvent::Reasoning(part) if part == "step one"
        )),
        "the summary is streamed to the display: {events:?}",
    );
}

#[test]
#[ignore = "integration"]
fn a_length_capped_stream_is_truncated_not_failed() {
    // `response.incomplete` + `max_output_tokens` is a length cap: the partial
    // answer is a truncated turn, and the id/usage still arrive.
    let body = sse(&[
        serde_json::json!({
            "type": "response.output_text.delta",
            "delta": "The answer is"
        }),
        serde_json::json!({
            "type": "response.incomplete",
            "response": {
                "id": "resp_3",
                "status": "incomplete",
                "incomplete_details": {"reason": "max_output_tokens"},
                "usage": {"input_tokens": 2, "output_tokens": 9, "total_tokens": 11}
            }
        }),
    ]);
    let mock = MockProvider::start(vec![(200, "text/event-stream", body)]);
    let mut events = Vec::new();
    let result = stream_turn(&mock, &mut events).expect("a length-capped stream is a turn");
    let ChatTurnResult::FinalText(text) = result else {
        panic!("expected FinalText, got {result:?}");
    };
    assert_eq!(text.content, "The answer is");
    assert!(text.truncated, "the turn reports the truncation");
    assert_eq!(text.response_id.as_deref(), Some("resp_3"));
    assert_eq!(text.usage.expect("usage").total_tokens, 11);
}

#[test]
#[ignore = "integration"]
fn a_content_filtered_stream_is_a_terminal_error() {
    // A policy block is the one incomplete reason that is an error: retrying
    // the same request can never clear it.
    let body = sse(&[serde_json::json!({
        "type": "response.incomplete",
        "response": {
            "id": "resp_4",
            "status": "incomplete",
            "incomplete_details": {"reason": "content_filter"}
        }
    })]);
    let mock = MockProvider::start(vec![(200, "text/event-stream", body)]);
    let mut events = Vec::new();
    let error = stream_turn(&mock, &mut events).expect_err("a filter block is terminal");
    assert!(
        matches!(error, OpenAiError::ContentFiltered { .. }),
        "got {error:?}",
    );
}
