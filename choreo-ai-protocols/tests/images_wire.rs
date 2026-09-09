//! Wire tests for the OpenAI image adapter, served by the shared
//! scripted HTTP provider ([`MockProvider`]).
//!
//! These tests bind a local `TcpListener`, so they exercise the full
//! request→response path against real sockets; the AGENTS.md test-discipline
//! rule keeps socket tests in `tests/` directories and marks them
//! `#[ignore]` (run via `cargo test-integration` / `nextest --run-ignored`),
//! so a plain `cargo test-fast` never opens a socket.

use choreo_ai_protocols::images::{
    Background, ImageGenerationRequest, ImageQuality, ImageSize, OutputFormat,
};
use choreo_ai_protocols::openai::ServiceConfig;
use choreo_ai_protocols::test_utils::MockProvider;
use choreo_ai_protocols::{ImageGenerationClient, OpenAiImageClient};
use choreo_proto::InferenceError;

/// Build a client pointed at the mock provider.
fn client(mock: &MockProvider) -> OpenAiImageClient {
    let config = ServiceConfig {
        base_url: mock.base_url("v1"),
        ..Default::default()
    };
    OpenAiImageClient::new(config, "sk-test".to_string())
}

fn sample_request() -> ImageGenerationRequest {
    ImageGenerationRequest::new("a lighthouse at dusk", "gpt-image-1")
}

fn success_body() -> String {
    serde_json::json!({
        "data": [{
            "b64_json": "aVNTaGVQclBuZ0J5dGVz",
            "revised_prompt": "a lighthouse at dusk, painterly"
        }]
    })
    .to_string()
}

#[test]
#[ignore]
fn success_maps_b64_and_revised_prompt() {
    let mock = MockProvider::start(vec![(200, "application/json", success_body())]);
    let result = client(&mock)
        .generate_image(&sample_request(), None)
        .expect("generation succeeds");
    assert_eq!(result.image_b64, "aVNTaGVQclBuZ0J5dGVz");
    assert_eq!(
        result.revised_prompt.as_deref(),
        Some("a lighthouse at dusk, painterly")
    );
    assert_eq!(result.model, "gpt-image-1");

    // One request, POSTed to the images endpoint with the auth header.
    let requests = mock.requests();
    assert_eq!(requests.len(), 1);
    let req = &requests[0];
    assert_eq!(req.method, "POST");
    assert_eq!(req.path, "/v1/images/generations");
    assert_eq!(req.header("authorization"), Some("Bearer sk-test"));
}

#[test]
#[ignore]
fn wire_body_has_model_prompt_n1_and_no_response_format() {
    // gpt-image-1 rejects `response_format` (it always returns b64_json), so
    // the wire body must never carry it — pinned here so a future refactor
    // cannot silently reintroduce the field and 400 every gpt-image call.
    let mock = MockProvider::start(vec![(200, "application/json", success_body())]);
    client(&mock)
        .generate_image(&sample_request(), None)
        .expect("generation succeeds");

    let body = mock.requests()[0].body_json();
    assert_eq!(body["model"], "gpt-image-1");
    assert_eq!(body["prompt"], "a lighthouse at dusk");
    assert_eq!(body["n"], 1);
    assert!(body.get("response_format").is_none());
    // Default knobs are OMITTED from the wire body entirely (minimal-body
    // policy: proxies that don't implement a knob reject its presence even
    // as an explicit default), so an all-defaults request is just
    // `{model, prompt, n}`.
    assert!(body.get("size").is_none());
    assert!(body.get("quality").is_none());
    assert!(body.get("output_format").is_none());
    assert!(body.get("background").is_none());
}

#[test]
#[ignore]
fn non_2xx_error_body_message_surfaces() {
    // The standard OpenAI error envelope's `error.message` must reach the
    // caller instead of the raw body dump.
    let mock = MockProvider::start(vec![(
        400,
        "application/json",
        r#"{"error":{"message":"Invalid size for this model","type":"invalid_request_error"}}"#
            .to_string(),
    )]);
    let err = client(&mock)
        .generate_image(&sample_request(), None)
        .expect_err("400 is terminal");
    match err {
        InferenceError::ClientError { status, detail } => {
            assert_eq!(status, 400);
            assert!(detail.contains("Invalid size for this model"), "{detail}");
        }
        other => panic!("expected ClientError, got {other:?}"),
    }
}

#[test]
#[ignore]
fn rate_limited_with_small_retry_after_is_retried_then_succeeds() {
    // Retry-After: 0 fits the backoff budget (wait = zero → no sleeping in
    // the test) and the frugal 2-attempt budget allows exactly one retry.
    let mock = MockProvider::start(vec![
        (
            429,
            "application/json",
            r#"{"error":{"message":"slow down"}}"#.to_string(),
        ),
        (200, "application/json", success_body()),
    ]);
    let result = client(&mock)
        .generate_image(&sample_request(), None)
        .expect("second attempt succeeds");
    assert_eq!(result.image_b64, "aVNTaGVQclBuZ0J5dGVz");
    assert_eq!(mock.requests().len(), 2, "429 must be retried once");
}

/// A one-shot mock that serves a single response carrying a custom header,
/// for cases [`MockProvider`] cannot script (it only sets Content-Type).
/// Returns the bound address; the listener lives for the process lifetime of
/// the test (an ephemeral port on loopback, dropped at test end).
fn single_response_with_header(
    status: u16,
    header: (&'static str, &'static str),
    body: &'static str,
) -> std::net::SocketAddr {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("addr");
    let head = format!(
        "HTTP/1.1 {status} Error\r\n{}: {}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        header.0,
        header.1,
        body.len()
    );
    std::thread::spawn(move || {
        use std::io::{Read as _, Write as _};
        // Serve the same canned response to every connection: the frugal
        // retry budget may reconnect, and a closed listener would turn the
        // retry into a spurious "connection refused" instead of the scripted
        // status the test asserts on.
        while let Ok((mut stream, _)) = listener.accept() {
            // Drain the request head + body before responding: ureq may
            // still be writing when we would otherwise close the socket,
            // and the resulting RST surfaces client-side as a spurious
            // EPIPE instead of the scripted status. A short read timeout
            // (rather than read-to-EOF, which would deadlock — ureq keeps
            // its write side open while awaiting the response) bounds the
            // drain.
            let _ = stream.set_read_timeout(Some(std::time::Duration::from_millis(300)));
            let mut buf = [0u8; 4096];
            loop {
                match stream.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {}
                }
            }
            let _ = stream.write_all(head.as_bytes());
            let _ = stream.write_all(body.as_bytes());
            let _ = stream.flush();
        }
    });
    addr
}

#[test]
#[ignore]
fn rate_limited_with_oversized_retry_after_is_terminal() {
    // A 1-hour cooldown outlives the 30 s backoff ceiling → the request must
    // fail immediately instead of waiting (and the frugal budget would not
    // clear it either way). MockProvider cannot script a Retry-After header,
    // so this one-shot local server carries it.
    let addr = single_response_with_header(
        429,
        ("Retry-After", "3600"),
        r#"{"error":{"message":"quota exhausted"}}"#,
    );
    let config = ServiceConfig {
        base_url: format!("http://{addr}/v1"),
        ..Default::default()
    };
    let err = OpenAiImageClient::new(config, "sk-test".to_string())
        .generate_image(&sample_request(), None)
        .expect_err("oversized Retry-After is terminal");
    assert!(matches!(err, InferenceError::RateLimited { .. }), "{err:?}");
    // Retry-After must survive into the typed error (the Display omits it).
    match err {
        InferenceError::RateLimited {
            retry_after_secs: Some(3600),
            ..
        } => {}
        other => panic!("expected Retry-After 3600, got {other:?}"),
    }
}

#[test]
#[ignore]
fn empty_data_array_is_empty_response_error() {
    let mock = MockProvider::start(vec![(
        200,
        "application/json",
        r#"{"data":[]}"#.to_string(),
    )]);
    let err = client(&mock)
        .generate_image(&sample_request(), None)
        .expect_err("empty data must error");
    assert!(
        matches!(err, InferenceError::EmptyResponse),
        "expected EmptyResponse, got {err:?}"
    );
}

#[test]
#[ignore]
fn malformed_json_response_errors() {
    let mock = MockProvider::start(vec![(
        200,
        "application/json",
        "not json at all".to_string(),
    )]);
    let err = client(&mock)
        .generate_image(&sample_request(), None)
        .expect_err("malformed JSON must error");
    assert!(
        matches!(err, InferenceError::Io(_)),
        "expected Io, got {err:?}"
    );
}

#[test]
#[ignore]
fn data_item_without_b64_is_empty_response_error() {
    // A 200 envelope whose item lacks b64_json is "no image data", not a
    // deserialization crash.
    let mock = MockProvider::start(vec![(
        200,
        "application/json",
        r#"{"data":[{"revised_prompt":"x"}]}"#.to_string(),
    )]);
    let err = client(&mock)
        .generate_image(&sample_request(), None)
        .expect_err("missing b64_json must error");
    assert!(matches!(err, InferenceError::EmptyResponse), "{err:?}");
}

#[test]
#[ignore]
fn non_auto_enums_serialize_as_wire_strings() {
    let mock = MockProvider::start(vec![(200, "application/json", success_body())]);
    let req = ImageGenerationRequest {
        size: ImageSize::Landscape1536x1024,
        quality: ImageQuality::High,
        output_format: OutputFormat::Webp,
        background: Background::Transparent,
        ..sample_request()
    };
    client(&mock)
        .generate_image(&req, None)
        .expect("generation succeeds");
    let body = mock.requests()[0].body_json();
    assert_eq!(body["size"], "1536x1024");
    assert_eq!(body["quality"], "high");
    assert_eq!(body["output_format"], "webp");
    assert_eq!(body["background"], "transparent");
}

#[test]
#[ignore]
fn defaults_and_trait_accessors() {
    let mock = MockProvider::start(vec![]);
    let c = client(&mock);
    assert_eq!(c.provider_slug(), "openai");
    // Defaults land on the "auto"/default variants.
    let req = ImageGenerationRequest::new("p", "gpt-image-1");
    assert_eq!(req.size, ImageSize::Auto);
    assert_eq!(req.quality, ImageQuality::Auto);
    assert_eq!(req.output_format, OutputFormat::Png);
    assert_eq!(req.background, Background::Auto);
    // Display mirrors the serde wire strings for every variant.
    assert_eq!(ImageSize::Portrait1024x1536.to_string(), "1024x1536");
    assert_eq!(ImageQuality::Medium.to_string(), "medium");
    assert_eq!(OutputFormat::Jpeg.to_string(), "jpeg");
    assert_eq!(Background::Opaque.to_string(), "opaque");
}
