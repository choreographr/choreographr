//! The one-shot (no-tool) completion paths: a declared terminal stop reason is
//! terminal there too.
//!
//! A `sensitive`/`content_filter` refusal arrives with no usable body, so a
//! simple path that only looked at the text would report an empty completion —
//! which reads as retryable and hides the policy denial. These tests hold every
//! simple path (non-streaming and streaming, chat and responses) to the same
//! classification the tool-carrying turn paths use.

#![expect(clippy::expect_used)]

use std::fmt::Write as _;

use choreo_ai_protocols::openai::{OpenAiError, RequestFormat, ServiceConfig};
use choreo_ai_protocols::test_utils::MockProvider;
use choreo_ai_protocols::{OpenAiClient, RequestAuth, SocketRegistry, StreamEvent};

/// A client on `format` pointed at `base_url`, streaming or not.
fn client(base_url: String, format: RequestFormat, streaming: bool) -> OpenAiClient {
    OpenAiClient::new(
        ServiceConfig {
            base_url,
            provider_slug: "openai".to_string(),
            streaming,
            retry_max_attempts: 1,
            connect_timeout_secs: 5,
            request_timeout_secs: 30,
            total_timeout_secs: 60,
            default_request_format: format,
            ..Default::default()
        },
        RequestAuth::api_key("test-key"),
        &SocketRegistry::new(),
    )
    .expect("openai client")
}

/// Frame an ordered list of SSE `data:` payloads into one response body.
fn sse(events: &[serde_json::Value]) -> String {
    let mut body = String::new();
    for event in events {
        let _ = write!(body, "data: {event}\n\n");
    }
    body.push_str("data: [DONE]\n\n");
    body
}

#[test]
#[ignore = "integration"]
fn a_chat_completion_refusal_is_terminal_not_empty() {
    // z.ai's `sensitive` (and OpenAI's `content_filter`) arrives with an empty
    // content string: the one-shot path must surface the policy denial rather
    // than read the empty body as a retryable empty completion.
    let mock = MockProvider::start(vec![(
        200,
        "application/json",
        serde_json::json!({
            "choices": [{
                "message": { "role": "assistant", "content": "" },
                "finish_reason": "sensitive"
            }]
        })
        .to_string(),
    )]);
    let err = client(mock.base_url("v1"), RequestFormat::ChatCompletions, false)
        .completion("glm-5.1", "hi")
        .expect_err("a content-filter refusal is an error");
    assert!(
        matches!(err, OpenAiError::ContentFiltered { .. }),
        "expected the terminal refusal, got {err:?}"
    );
}

#[test]
#[ignore = "integration"]
fn a_chat_completion_that_really_is_empty_stays_an_empty_response() {
    // Control: a plain `stop` with no content is still an empty completion, so
    // the classification above is not swallowing the ordinary case.
    let mock = MockProvider::start(vec![(
        200,
        "application/json",
        serde_json::json!({
            "choices": [{
                "message": { "role": "assistant", "content": "" },
                "finish_reason": "stop"
            }]
        })
        .to_string(),
    )]);
    // `glm-5.1` is not in the catalog under the `openai` slug, so the request
    // format falls back to the configured Chat Completions and the chat path is
    // genuinely exercised (`gpt-5.4` would route to the Responses API instead).
    let err = client(mock.base_url("v1"), RequestFormat::ChatCompletions, false)
        .completion("glm-5.1", "hi")
        .expect_err("an empty answer is an error");
    assert!(
        matches!(err, OpenAiError::EmptyResponse),
        "expected EmptyResponse, got {err:?}"
    );
}

#[test]
#[ignore = "integration"]
fn a_streaming_chat_completion_refusal_is_terminal_not_empty() {
    // The streaming one-shot path classifies a declared terminal finish reason
    // the same way the non-streaming one does: z.ai's `sensitive` streams no
    // content and only a `finish_reason`, which would otherwise read as a
    // (retryable) empty completion.
    let body = sse(&[serde_json::json!({
        "choices": [{"finish_reason": "sensitive"}]
    })]);
    let mock = MockProvider::start(vec![(200, "text/event-stream", body)]);
    let mut events = Vec::new();
    let err = client(mock.base_url("v1"), RequestFormat::ChatCompletions, true)
        .completion_stream("glm-5.1", "hi", |event| {
            events.push(event);
            Ok(())
        })
        .expect_err("a content-filter refusal is an error");
    assert!(
        matches!(err, OpenAiError::ContentFiltered { .. }),
        "expected the terminal refusal, got {err:?}"
    );
    assert!(
        events.is_empty(),
        "a refusal streams no content: {events:?}"
    );
}

#[test]
#[ignore = "integration"]
fn a_streaming_chat_completion_answer_is_returned() {
    // Control: an ordinary streamed answer still returns Ok and streams its
    // content, so the classification above is not swallowing the normal case.
    let body = sse(&[
        serde_json::json!({"choices": [{"delta": {"content": "hello"}}]}),
        serde_json::json!({"choices": [{"finish_reason": "stop"}]}),
    ]);
    let mock = MockProvider::start(vec![(200, "text/event-stream", body)]);
    let mut events = Vec::new();
    // `glm-5.1` keeps this on the chat path (see the empty-response control).
    client(mock.base_url("v1"), RequestFormat::ChatCompletions, true)
        .completion_stream("glm-5.1", "hi", |event| {
            events.push(event);
            Ok(())
        })
        .expect("a normal streamed answer is Ok");
    assert!(
        events
            .iter()
            .any(|event| matches!(event, StreamEvent::Answer(text) if text == "hello")),
        "the answer is streamed: {events:?}"
    );
}

#[test]
#[ignore = "integration"]
fn a_responses_refusal_is_terminal_not_empty() {
    // The Responses analogue: `status: incomplete` with a policy reason and no
    // usable output.
    let mock = MockProvider::start(vec![(
        200,
        "application/json",
        serde_json::json!({
            "id": "resp_1",
            "status": "incomplete",
            "incomplete_details": { "reason": "content_filter" },
            "output": []
        })
        .to_string(),
    )]);
    let err = client(mock.base_url("v1"), RequestFormat::Responses, false)
        .completion("gpt-5.4", "hi")
        .expect_err("a content-filter refusal is an error");
    assert!(
        matches!(err, OpenAiError::ContentFiltered { .. }),
        "expected the terminal refusal, got {err:?}"
    );
}

#[test]
#[ignore = "integration"]
fn a_responses_partial_answer_still_returns_its_text() {
    // A length-capped Responses reply is a truncation, not an error: the
    // one-shot path has no turn result to flag it, but the partial text is
    // still a usable answer.
    let mock = MockProvider::start(vec![(
        200,
        "application/json",
        serde_json::json!({
            "id": "resp_2",
            "status": "incomplete",
            "incomplete_details": { "reason": "max_output_tokens" },
            "output": [{
                "type": "message",
                "role": "assistant",
                "content": [{ "type": "output_text", "text": "partial" }]
            }]
        })
        .to_string(),
    )]);
    let content = client(mock.base_url("v1"), RequestFormat::Responses, false)
        .completion("gpt-5.4", "hi")
        .expect("a truncated answer is still an answer");
    assert_eq!(content, "partial");
}
