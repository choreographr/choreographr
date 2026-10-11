//! Wire test for the request-body upload hook and the compact, length-delimited
//! send path.
//!
//! Builds a real [`OpenAiClient`] against the scripted loopback HTTP provider and
//! asserts the two load-bearing properties of the shared `retry::HookedJsonBody`
//! send path on the actual wire: the declared `Content-Length` equals the bytes
//! the transport sent (a mismatch would spin ureq's length-delimited send loop),
//! and the upload hook (`on_uploaded`, the daemon's `Waiting` boundary) fires
//! exactly once.  The body must be compact JSON — pretty-printing would embed
//! newlines.  A second test covers the image adapter, which routes its body
//! through the same `HookedJsonBody` path.
//!
//! These tests bind a real local TCP socket, so per AGENTS.md they live in
//! `tests/` and are marked `#[ignore]` (run via `cargo test-integration`).

use std::io::Read;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use choreo_ai_protocols::openai::{
    ChatRequestMessage, MaxTokensField, OpenAiClient, RequestUploadedCallback, ServiceConfig,
};
use choreo_ai_protocols::test_utils::MockProvider;
use choreo_ai_protocols::{
    ChatTurnRequest, ChatTurnResult, ImageGenerationClient, ImageGenerationRequest,
    OpenAiImageClient, RequestCompression, SocketRegistry,
};

#[test]
#[ignore = "integration"]
fn uploaded_body_is_length_delimited_and_fires_the_hook_once() {
    let registry = SocketRegistry::new();
    let response = r#"{
        "choices":[{"message":{"content":"hello","tool_calls":[],"reasoning_content":null,"reasoning":null,"reasoning_text":null}}],
        "usage":{"prompt_tokens":1,"completion_tokens":1,"total_tokens":2}
    }"#;
    let mock = MockProvider::start(vec![(200, "application/json", response.to_string())]);

    let client = OpenAiClient::new(
        ServiceConfig {
            base_url: mock.base_url("v1"),
            provider_slug: "deepseek".to_string(),
            streaming: false,
            retry_max_attempts: 1,
            connect_timeout_secs: 5,
            request_timeout_secs: 30,
            total_timeout_secs: 60,
            chat_completions_max_tokens_field: MaxTokensField::MaxCompletionTokens,
            ..Default::default()
        },
        "test-key".to_string(),
        &registry,
    )
    .expect("openai client");

    let uploads = Arc::new(AtomicU32::new(0));
    let seen = Arc::clone(&uploads);
    let on_uploaded: RequestUploadedCallback = Arc::new(move || {
        seen.fetch_add(1, Ordering::SeqCst);
    });

    let user_message = ChatRequestMessage::simple("user", "hi".to_string());
    let result = client
        .chat_completion_turn(ChatTurnRequest {
            model: "deepseek-v4-flash",
            messages: &[user_message],
            tools: &[],
            thinking_effort: "off".to_string(),
            on_retry: &mut None,
            on_uploaded: Some(on_uploaded),
            cancel_rx: None,
            previous_response_id: None,
            tool_results: &[],
            programmatic_tool_calling: false,
            session_id: "42".to_string(),
            request_id: "7".to_string(),
            max_output_tokens_override: None,
            no_retry: false,
        })
        .expect("turn succeeds over the registered connection");
    assert!(matches!(result, ChatTurnResult::FinalText(_)));

    // The upload hook fires exactly once on this single (non-retried) attempt —
    // the moment the last body byte is handed to the transport.
    assert_eq!(
        uploads.load(Ordering::SeqCst),
        1,
        "the upload hook must fire once for a single attempt"
    );

    let requests = mock.requests();
    let req = requests.first().expect("one captured request");

    // The declared `Content-Length` must equal the bytes actually sent: a short
    // body would make ureq's length-delimited send loop spin forever (see
    // `retry::HookedJsonBody`).
    let declared: usize = req
        .header("content-length")
        .expect("Content-Length header present")
        .parse()
        .expect("Content-Length is a decimal");
    assert_eq!(
        declared,
        req.body.len(),
        "declared Content-Length must match the sent body"
    );

    // The pairing is centralized in `HookedJsonBody::apply_headers`, so the
    // media type is present alongside the length.
    assert_eq!(
        req.header("content-type"),
        Some("application/json; charset=utf-8"),
        "the request body is sent as JSON"
    );

    // The payload is compact JSON: pretty-printing would embed newlines.
    assert!(
        !req.body.contains(&b'\n'),
        "request body must be compact (no pretty-print newlines)"
    );
}

#[test]
#[ignore = "integration"]
fn image_generation_body_is_compact_and_length_delimited() {
    let registry = SocketRegistry::new();
    // Minimal Images API response: one inline b64 item, no revised_prompt.
    let response = r#"{"data":[{"b64_json":"AAAA"}]}"#;
    let mock = MockProvider::start(vec![(200, "application/json", response.to_string())]);

    // The image adapter has no `Result` constructor (it fixes its own deadline
    // and retry budget), so a plain config + key is all it needs.
    let client = OpenAiImageClient::new(
        ServiceConfig {
            base_url: mock.base_url("v1"),
            provider_slug: "openai".to_string(),
            connect_timeout_secs: 5,
            ..Default::default()
        },
        "test-key".to_string(),
        &registry,
    );

    let req = ImageGenerationRequest::new("a cat", "gpt-image-1");
    let result = client
        .generate_image(&req, None)
        .expect("image generation succeeds over the mock");
    assert_eq!(result.image_b64, "AAAA");

    let requests = mock.requests();
    let captured = requests.first().expect("one captured request");
    // The image adapter routes its body through the same compact,
    // length-delimited `HookedJsonBody` path as the chat turn, so the declared
    // length must equal the bytes sent (a mismatch would spin ureq's
    // length-delimited send loop) and the payload must carry no pretty-print
    // newlines.
    let declared: usize = captured
        .header("content-length")
        .expect("Content-Length header present")
        .parse()
        .expect("Content-Length is a decimal");
    assert_eq!(
        declared,
        captured.body.len(),
        "declared Content-Length must match the sent body"
    );
    assert!(
        !captured.body.contains(&b'\n'),
        "request body must be compact (no pretty-print newlines)"
    );
    assert_eq!(
        captured.header("content-type"),
        Some("application/json; charset=utf-8"),
        "the request body is sent as JSON"
    );
}

#[test]
#[ignore = "integration"]
fn compressed_chat_body_is_gzip_and_length_delimited() {
    let registry = SocketRegistry::new();
    let response = r#"{
        "choices":[{"message":{"content":"hello","tool_calls":[],"reasoning_content":null,"reasoning":null,"reasoning_text":null}}],
        "usage":{"prompt_tokens":1,"completion_tokens":1,"total_tokens":2}
    }"#;
    let mock = MockProvider::start(vec![(200, "application/json", response.to_string())]);

    let client = OpenAiClient::new(
        ServiceConfig {
            base_url: mock.base_url("v1"),
            provider_slug: "deepseek-compress".to_string(),
            streaming: false,
            retry_max_attempts: 1,
            connect_timeout_secs: 5,
            request_timeout_secs: 30,
            total_timeout_secs: 60,
            chat_completions_max_tokens_field: MaxTokensField::MaxCompletionTokens,
            request_compression: RequestCompression::Gzip,
            ..Default::default()
        },
        "test-key".to_string(),
        &registry,
    )
    .expect("openai client");

    let user_message = ChatRequestMessage::simple("user", "hi".to_string());
    let result = client
        .chat_completion_turn(ChatTurnRequest {
            model: "deepseek-v4-flash",
            messages: &[user_message],
            tools: &[],
            thinking_effort: "off".to_string(),
            on_retry: &mut None,
            on_uploaded: None,
            cancel_rx: None,
            previous_response_id: None,
            tool_results: &[],
            programmatic_tool_calling: false,
            session_id: "42".to_string(),
            request_id: "7".to_string(),
            max_output_tokens_override: None,
            no_retry: false,
        })
        .expect("turn succeeds over the registered connection");
    assert!(matches!(result, ChatTurnResult::FinalText(_)));

    let requests = mock.requests();
    let req = requests.first().expect("one captured request");
    assert_eq!(
        req.header("content-encoding"),
        Some("gzip"),
        "the request body must be gzip-encoded"
    );
    // The declared `Content-Length` must equal the compressed bytes actually
    // sent (a mismatch would spin ureq's length-delimited send loop).
    let declared: usize = req
        .header("content-length")
        .expect("Content-Length header present")
        .parse()
        .expect("Content-Length is a decimal");
    assert_eq!(
        declared,
        req.body.len(),
        "declared Content-Length must match the compressed body"
    );
    // The compressed body must gunzip back to exactly the JSON a provider
    // would parse.
    let mut decoded = Vec::new();
    flate2::read::GzDecoder::new(&req.body[..])
        .read_to_end(&mut decoded)
        .expect("gunzip the request body");
    let json = std::str::from_utf8(&decoded).expect("decompressed body is UTF-8 JSON");
    assert!(
        json.contains("\"messages\""),
        "decompressed body must be the chat-completions JSON"
    );
}

#[test]
#[ignore = "integration"]
fn compressed_chat_body_is_zstd_and_length_delimited() {
    let registry = SocketRegistry::new();
    let response = r#"{
        "choices":[{"message":{"content":"hello","tool_calls":[],"reasoning_content":null,"reasoning":null,"reasoning_text":null}}],
        "usage":{"prompt_tokens":1,"completion_tokens":1,"total_tokens":2}
    }"#;
    let mock = MockProvider::start(vec![(200, "application/json", response.to_string())]);

    let client = OpenAiClient::new(
        ServiceConfig {
            base_url: mock.base_url("v1"),
            provider_slug: "deepseek-compress-zstd".to_string(),
            streaming: false,
            retry_max_attempts: 1,
            connect_timeout_secs: 5,
            request_timeout_secs: 30,
            total_timeout_secs: 60,
            chat_completions_max_tokens_field: MaxTokensField::MaxCompletionTokens,
            request_compression: RequestCompression::Zstd,
            ..Default::default()
        },
        "test-key".to_string(),
        &registry,
    )
    .expect("openai client");

    let user_message = ChatRequestMessage::simple("user", "hi".to_string());
    let result = client
        .chat_completion_turn(ChatTurnRequest {
            model: "deepseek-v4-flash",
            messages: &[user_message],
            tools: &[],
            thinking_effort: "off".to_string(),
            on_retry: &mut None,
            on_uploaded: None,
            cancel_rx: None,
            previous_response_id: None,
            tool_results: &[],
            programmatic_tool_calling: false,
            session_id: "42".to_string(),
            request_id: "7".to_string(),
            max_output_tokens_override: None,
            no_retry: false,
        })
        .expect("turn succeeds over the registered connection");
    assert!(matches!(result, ChatTurnResult::FinalText(_)));

    let requests = mock.requests();
    let req = requests.first().expect("one captured request");
    assert_eq!(
        req.header("content-encoding"),
        Some("zstd"),
        "the request body must be zstd-encoded"
    );
    // The declared `Content-Length` must equal the compressed bytes actually
    // sent (a mismatch would spin ureq's length-delimited send loop).
    let declared: usize = req
        .header("content-length")
        .expect("Content-Length header present")
        .parse()
        .expect("Content-Length is a decimal");
    assert_eq!(
        declared,
        req.body.len(),
        "declared Content-Length must match the compressed body"
    );
    // The compressed body must decode back to the JSON a provider would parse.
    let mut decoder =
        structured_zstd::decoding::StreamingDecoder::new(std::io::Cursor::new(&req.body[..]))
            .expect("init zstd decoder");
    let mut decoded = Vec::new();
    decoder
        .read_to_end(&mut decoded)
        .expect("decode the zstd request body");
    let json = std::str::from_utf8(&decoded).expect("decompressed body is UTF-8 JSON");
    assert!(
        json.contains("\"messages\""),
        "decompressed body must be the chat-completions JSON"
    );
}

#[test]
#[ignore = "integration"]
fn rejected_gzip_body_downgrades_to_identity_and_retries() {
    let registry = SocketRegistry::new();
    let response = r#"{
        "choices":[{"message":{"content":"hello","tool_calls":[],"reasoning_content":null,"reasoning":null,"reasoning_text":null}}],
        "usage":{"prompt_tokens":1,"completion_tokens":1,"total_tokens":2}
    }"#;
    // The provider rejects the gzip body with 415 first, then accepts the
    // identity retry (the mock repeats its last response for later requests).
    let mock = MockProvider::start(vec![
        (415, "application/json", "{}".to_string()),
        (200, "application/json", response.to_string()),
    ]);

    let client = OpenAiClient::new(
        ServiceConfig {
            base_url: mock.base_url("v1"),
            provider_slug: "deepseek-compress-415".to_string(),
            streaming: false,
            // Even with no retry budget, the 415 downgrade re-sends once with
            // an uncompressed body — a 415 is an encoding mismatch, not a
            // transient failure.
            retry_max_attempts: 1,
            connect_timeout_secs: 5,
            request_timeout_secs: 30,
            total_timeout_secs: 60,
            chat_completions_max_tokens_field: MaxTokensField::MaxCompletionTokens,
            request_compression: RequestCompression::Gzip,
            ..Default::default()
        },
        "test-key".to_string(),
        &registry,
    )
    .expect("openai client");

    let user_message = ChatRequestMessage::simple("user", "hi".to_string());
    let result = client
        .chat_completion_turn(ChatTurnRequest {
            model: "deepseek-v4-flash",
            messages: &[user_message],
            tools: &[],
            thinking_effort: "off".to_string(),
            on_retry: &mut None,
            on_uploaded: None,
            cancel_rx: None,
            previous_response_id: None,
            tool_results: &[],
            programmatic_tool_calling: false,
            session_id: "42".to_string(),
            request_id: "7".to_string(),
            max_output_tokens_override: None,
            no_retry: false,
        })
        .expect("turn succeeds after the identity downgrade");
    assert!(matches!(result, ChatTurnResult::FinalText(_)));

    let requests = mock.requests();
    assert_eq!(
        requests.len(),
        2,
        "one gzip attempt then one identity retry"
    );
    assert_eq!(
        requests[0].header("content-encoding"),
        Some("gzip"),
        "the first attempt is compressed"
    );
    assert_eq!(
        requests[1].header("content-encoding"),
        None,
        "the retry after 415 must be uncompressed"
    );
}
