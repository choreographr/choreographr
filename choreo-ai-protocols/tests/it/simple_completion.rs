//! The one-shot (no-tool) completion paths: a declared terminal stop reason is
//! terminal there too.
//!
//! A `sensitive`/`content_filter` refusal arrives with no usable body, so a
//! simple path that only looked at the text would report an empty completion —
//! which reads as retryable and hides the policy denial. These tests hold both
//! simple paths to the same classification the tool-carrying turn paths use.

#![expect(clippy::expect_used)]

use choreo_ai_protocols::openai::{OpenAiError, RequestFormat, ServiceConfig};
use choreo_ai_protocols::test_utils::MockProvider;
use choreo_ai_protocols::{OpenAiClient, RequestAuth, SocketRegistry};

/// A non-streaming client on `format` pointed at `base_url`.
fn client(base_url: String, format: RequestFormat) -> OpenAiClient {
    OpenAiClient::new(
        ServiceConfig {
            base_url,
            provider_slug: "openai".to_string(),
            streaming: false,
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

#[test]
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
    let err = client(mock.base_url("v1"), RequestFormat::ChatCompletions)
        .completion("glm-5.1", "hi")
        .expect_err("a content-filter refusal is an error");
    assert!(
        matches!(err, OpenAiError::ContentFiltered { .. }),
        "expected the terminal refusal, got {err:?}"
    );
}

#[test]
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
    let err = client(mock.base_url("v1"), RequestFormat::ChatCompletions)
        .completion("gpt-5.4", "hi")
        .expect_err("an empty answer is an error");
    assert!(
        matches!(err, OpenAiError::EmptyResponse),
        "expected EmptyResponse, got {err:?}"
    );
}

#[test]
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
    let err = client(mock.base_url("v1"), RequestFormat::Responses)
        .completion("gpt-5.4", "hi")
        .expect_err("a content-filter refusal is an error");
    assert!(
        matches!(err, OpenAiError::ContentFiltered { .. }),
        "expected the terminal refusal, got {err:?}"
    );
}

#[test]
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
    let content = client(mock.base_url("v1"), RequestFormat::Responses)
        .completion("gpt-5.4", "hi")
        .expect("a truncated answer is still an answer");
    assert_eq!(content, "partial");
}
