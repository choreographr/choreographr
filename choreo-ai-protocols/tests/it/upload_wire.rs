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

// AGENTS.md permits unwrap/expect/panic in tests/ files, but clippy's
// allow-*-in-tests config only recognizes #[test]-annotated functions —
// helper fns in this file need this file-level allowance.
#![expect(clippy::expect_used)]

use std::io::Read;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use choreo_ai_protocols::openai::{
    ChatRequestMessage, MaxTokensField, OpenAiClient, RequestAuth, RequestUploadedCallback,
    ServiceConfig,
};
use choreo_ai_protocols::test_utils::{CapturedRequest, MockProvider};
use choreo_ai_protocols::{
    ChatTurnRequest, ChatTurnResult, ImageGenerationClient, ImageGenerationRequest,
    OpenAiImageClient, RequestCompression, SocketRegistry,
};

/// The canned chat-completions success body the mock returns.  Its content is
/// irrelevant to the wire assertions; the client only needs a well-formed turn.
const CHAT_RESPONSE: &str = r#"{
    "choices":[{"message":{"content":"hello","tool_calls":[],"reasoning_content":null,"reasoning":null,"reasoning_text":null}}],
    "usage":{"prompt_tokens":1,"completion_tokens":1,"total_tokens":2}
}"#;

/// Build the `OpenAiClient` the wire tests exercise: one attempt (so a capture
/// count is deterministic), a bounded timeout, and the caller's `compression`
/// policy.  The registry is passed in so it outlives the client reference.
fn chat_client(
    mock: &MockProvider,
    registry: &SocketRegistry,
    slug: &str,
    compression: RequestCompression,
) -> OpenAiClient {
    OpenAiClient::new(
        ServiceConfig {
            base_url: mock.base_url("v1"),
            provider_slug: slug.to_string(),
            streaming: false,
            retry_max_attempts: 1,
            connect_timeout_secs: 5,
            request_timeout_secs: 30,
            total_timeout_secs: 60,
            chat_completions_max_tokens_field: MaxTokensField::MaxCompletionTokens,
            request_compression: compression,
            ..Default::default()
        },
        RequestAuth::api_key("test-key"),
        registry,
    )
    .expect("openai client")
}

/// Send one minimal chat turn, returning the parsed result.  The `on_uploaded`
/// hook is threaded through so the caller can count its firings.
fn run_chat_turn(
    client: &OpenAiClient,
    on_uploaded: Option<RequestUploadedCallback>,
) -> ChatTurnResult {
    let message = ChatRequestMessage::simple("user", "hi".to_string());
    // `on_retry` borrows a local slot so the whole request lives within this
    // call; the tests never wire a retry callback.
    let mut on_retry = None;
    client
        .chat_completion_turn(ChatTurnRequest {
            model: "deepseek-v4-flash",
            messages: std::slice::from_ref(&message),
            tools: &[],
            thinking_effort: "off".to_string(),
            on_retry: &mut on_retry,
            on_uploaded,
            cancel_rx: None,
            previous_response_id: None,
            tool_results: &[],
            programmatic_tool_calling: false,
            session_id: "42".to_string(),
            request_id: "7".to_string(),
            max_output_tokens_override: None,
            no_retry: false,
        })
        .expect("turn succeeds over the registered connection")
}

/// Parse the declared `Content-Length` of a captured request.
fn declared_content_length(req: &CapturedRequest) -> usize {
    req.header("content-length")
        .expect("Content-Length header present")
        .parse()
        .expect("Content-Length is a decimal")
}

/// Gunzip a captured request body.
fn gunzip(bytes: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    flate2::read::GzDecoder::new(bytes)
        .read_to_end(&mut out)
        .expect("gunzip the request body");
    out
}

/// Decode a captured zstd request body.
fn unzstd(bytes: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    structured_zstd::decoding::StreamingDecoder::new(std::io::Cursor::new(bytes))
        .read_to_end(&mut out)
        .expect("decode the zstd request body");
    out
}

#[test]
#[ignore = "integration"]
fn uploaded_body_is_length_delimited_and_fires_the_hook_once() {
    let registry = SocketRegistry::new();
    let mock = MockProvider::start(vec![(200, "application/json", CHAT_RESPONSE.to_string())]);
    let client = chat_client(&mock, &registry, "deepseek", RequestCompression::Identity);

    let uploads = Arc::new(AtomicU32::new(0));
    let seen = Arc::clone(&uploads);
    let on_uploaded: RequestUploadedCallback = Arc::new(move || {
        seen.fetch_add(1, Ordering::SeqCst);
    });

    let result = run_chat_turn(&client, Some(on_uploaded));
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
    assert_eq!(
        declared_content_length(req),
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
    assert_eq!(
        declared_content_length(captured),
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
    let mock = MockProvider::start(vec![(200, "application/json", CHAT_RESPONSE.to_string())]);
    let client = chat_client(
        &mock,
        &registry,
        "deepseek-compress",
        RequestCompression::Gzip,
    );

    let result = run_chat_turn(&client, None);
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
    assert_eq!(
        declared_content_length(req),
        req.body.len(),
        "declared Content-Length must match the compressed body"
    );
    // The compressed body must gunzip back to exactly the JSON a provider
    // would parse.
    let json = String::from_utf8(gunzip(&req.body)).expect("decompressed body is UTF-8 JSON");
    assert!(
        json.contains("\"messages\""),
        "decompressed body must be the chat-completions JSON"
    );
}

#[test]
#[ignore = "integration"]
fn compressed_chat_body_is_zstd_and_length_delimited() {
    let registry = SocketRegistry::new();
    let mock = MockProvider::start(vec![(200, "application/json", CHAT_RESPONSE.to_string())]);
    let client = chat_client(
        &mock,
        &registry,
        "deepseek-compress-zstd",
        RequestCompression::Zstd,
    );

    let result = run_chat_turn(&client, None);
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
    assert_eq!(
        declared_content_length(req),
        req.body.len(),
        "declared Content-Length must match the compressed body"
    );
    // The compressed body must decode back to the JSON a provider would parse.
    let json = String::from_utf8(unzstd(&req.body)).expect("decompressed body is UTF-8 JSON");
    assert!(
        json.contains("\"messages\""),
        "decompressed body must be the chat-completions JSON"
    );
}

#[test]
#[ignore = "integration"]
fn rejected_gzip_body_downgrades_to_identity_and_retries() {
    let registry = SocketRegistry::new();
    // The provider rejects the gzip body with 415 first, then accepts the
    // identity retry (the mock repeats its last response for later requests).
    let mock = MockProvider::start(vec![
        (415, "application/json", "{}".to_string()),
        (200, "application/json", CHAT_RESPONSE.to_string()),
    ]);
    // Even with no retry budget, the 415 downgrade re-sends once with an
    // uncompressed body — a 415 is an encoding mismatch, not a transient
    // failure.
    let client = chat_client(
        &mock,
        &registry,
        "deepseek-compress-415",
        RequestCompression::Gzip,
    );

    let result = run_chat_turn(&client, None);
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
