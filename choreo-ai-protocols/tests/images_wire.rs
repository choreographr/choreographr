//! Wire tests for the `OpenAI` image adapter, served by the shared
//! scripted HTTP provider ([`MockProvider`]).
//!
//! These tests bind a local `TcpListener`, so they exercise the full
//! request→response path against real sockets; the AGENTS.md test-discipline
//! rule keeps socket tests in `tests/` directories and marks them
//! `#[ignore]` (run via `cargo test-integration` / `nextest --run-ignored`),
//! so a plain `cargo test-fast` never opens a socket.

// AGENTS.md permits unwrap/expect/panic in tests/ files, but clippy's
// allow-*-in-tests config only recognizes #[test]-annotated functions —
// helper fns in this file need this file-level allowance.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]
use base64::Engine as _;
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
    OpenAiImageClient::new(
        config,
        "sk-test".to_string(),
        &choreo_ai_protocols::SocketRegistry::new(),
    )
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
#[ignore = "integration"]
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
#[ignore = "integration"]
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
#[ignore = "integration"]
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
#[ignore = "integration"]
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
#[ignore = "integration"]
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
    let err = OpenAiImageClient::new(
        config,
        "sk-test".to_string(),
        &choreo_ai_protocols::SocketRegistry::new(),
    )
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
#[ignore = "integration"]
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
#[ignore = "integration"]
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
#[ignore = "integration"]
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
#[ignore = "integration"]
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
#[ignore = "integration"]
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

// ── ZaiImageClient (z.ai / Zhipu GLM Images API) ─────────────────────────
//
// The mock is reused verbatim: it scripts "one canned response per
// request", so a z.ai URL-returning success is scripted as TWO responses —
// [JSON envelope, image bytes] — served to the generation POST and the
// follow-up CDN download in that order. The self-referential URL (the body
// must embed the server's own address) is why these use
// `MockProvider::start_scripted` instead of `start`.

use choreo_ai_protocols::ZaiImageClient;

/// Plain payload bytes the mock "CDN" serves. A real PNG is not needed:
/// the adapter intentionally guards only the content type (bytes are
/// validated by the daemon's prepare pipeline); this test pins byte
/// fidelity through the adapter, not image decodability.
const IMAGE_BYTES: &str = "\u{89}PNG-fixture-bytes-7a5f0e\u{82}";

fn zai_client(mock: &MockProvider) -> ZaiImageClient {
    // "paas/v4" mirrors the real z.ai PaaS base's trailing segments so the
    // composed request path mirrors what production would compose.
    let config = ServiceConfig {
        base_url: mock.base_url("paas/v4"),
        provider_slug: "zai".to_string(),
        ..Default::default()
    };
    ZaiImageClient::new(
        config,
        "zai-key".to_string(),
        &choreo_ai_protocols::SocketRegistry::new(),
    )
}

fn zai_sample_request() -> ImageGenerationRequest {
    ImageGenerationRequest::new("a lighthouse at dusk", "glm-image")
}

#[test]
#[ignore = "integration"]
fn zai_url_response_downloads_image_bytes() {
    let mock = MockProvider::start_scripted(|base| {
        vec![
            (
                200,
                "application/json",
                serde_json::json!({
                    // `created` is parsed-but-unexposed by the adapter.
                    "created": 1_700_000_000_u64,
                    "data": [{ "url": format!("{base}/cdn/img/generated.png") }]
                })
                .to_string(),
            ),
            // The "CDN": same mock listener, image content type, raw bytes.
            (200, "image/png", IMAGE_BYTES.to_string()),
        ]
    });
    let result = zai_client(&mock)
        .generate_image(&zai_sample_request(), None)
        .expect("generation succeeds");

    // Byte fidelity: the downloaded bytes are re-encoded b64 and decode
    // back to the exact payload the mock served.
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(&result.image_b64)
        .expect("result b64 decodes");
    assert_eq!(bytes, IMAGE_BYTES.as_bytes());
    // URL-variant responses carry no revised prompt; the model is echoed.
    assert_eq!(result.revised_prompt, None);
    assert_eq!(result.model, "glm-image");

    // Two requests: the generation POST and the CDN download — which must
    // NOT carry the Authorization header (the URL is pre-signed and the
    // API key must not travel to the CDN).
    let requests = mock.requests();
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[0].path, "/paas/v4/images/generations");
    assert_eq!(requests[0].header("authorization"), Some("Bearer zai-key"));
    assert_eq!(requests[1].path, "/cdn/img/generated.png");
    assert_eq!(requests[1].header("authorization"), None);
}

#[test]
#[ignore = "integration"]
fn zai_url_download_retries_the_not_yet_published_race() {
    // Production race (observed 2026-09): z.ai's object storage advertises
    // the generation's url BEFORE the object is published, so the first
    // GET receives a non-image body while the CDN propagates, and the
    // identical URL serves a clean image seconds later. Pinned here: GET #1
    // returns an error-page-shaped 200 (non-image content type), GET #2
    // returns the real bytes — the adapter must retry the download (its own
    // 3-attempt budget, EMPTY-wait backoff in this test) and succeed on #3
    // of the script, instead of failing the whole generation. zero backoff
    // so the test sleeps nothing.
    let mock = MockProvider::start_scripted(|base| {
        vec![
            (
                200,
                "application/json",
                serde_json::json!({
                    "data": [{ "url": format!("{base}/cdn/img/race.png") }]
                })
                .to_string(),
            ),
            // GET #1: "not published yet" — an error page served with a
            // 200 by the object storage while it materializes the object.
            (200, "text/html", "<html>not an image</html>".to_string()),
            // GET #2: the same URL now serving the settled image.
            (200, "image/png", IMAGE_BYTES.to_string()),
        ]
    });
    let config = ServiceConfig {
        base_url: mock.base_url("paas/v4"),
        provider_slug: "zai".to_string(),
        retry_initial_backoff_ms: 0, // no sleeping in tests
        ..Default::default()
    };
    let result = ZaiImageClient::new(
        config,
        "zai-key".to_string(),
        &choreo_ai_protocols::SocketRegistry::new(),
    )
    .generate_image(&zai_sample_request(), None)
    .expect("second CDN fetch resolves the propagation race");

    let bytes = base64::engine::general_purpose::STANDARD
        .decode(&result.image_b64)
        .expect("result b64 decodes");
    assert_eq!(bytes, IMAGE_BYTES.as_bytes());
    // Three requests: POST, racy GET #1, retried GET #2.
    let requests = mock.requests();
    assert_eq!(requests.len(), 3, "POST + failed GET + retried GET");
    assert_eq!(requests[1].path, "/cdn/img/race.png");
    assert_eq!(requests[2].path, "/cdn/img/race.png");
}

#[test]
#[ignore = "integration"]
fn zai_url_download_stays_terminal_after_the_retry_budget() {
    // The retry budget is bounded: if EVERY GET serves the racy non-image
    // body, the download fails for real instead of looping forever — the
    // POST response, then the configured 3 download attempts (4 GET total
    // including retries over that single POST).
    let mock = MockProvider::start_scripted(|base| {
        vec![
            (
                200,
                "application/json",
                serde_json::json!({ "data": [{ "url": format!("{base}/cdn/img/stuck.png") }] })
                    .to_string(),
            ),
            // start()'s last response repeats for excess requests —
            // every download fetch gets the racy page.
            (200, "text/html", "<html>never publishes</html>".to_string()),
        ]
    });
    let config = ServiceConfig {
        base_url: mock.base_url("paas/v4"),
        provider_slug: "zai".to_string(),
        retry_initial_backoff_ms: 0, // no sleeping in tests
        ..Default::default()
    };
    let err = ZaiImageClient::new(
        config,
        "zai-key".to_string(),
        &choreo_ai_protocols::SocketRegistry::new(),
    )
    .generate_image(&zai_sample_request(), None)
    .expect_err("all-fetches-racy must be terminal after the download budget");
    match err {
        // NotReady is the dedicated "CDN answered but not with an image yet"
        // variant — the exhausted download budget surfaces it honestly
        // instead of conflating with EmptyResponse.
        InferenceError::NotReady { detail } => {
            assert!(detail.contains("non-image content type"), "{detail}");
        }
        other => panic!("expected NotReady, got {other:?}"),
    }
    // 1 POST + the full 3-fetch download budget.
    assert_eq!(mock.requests().len(), 4, "POST + 3 download attempts");
}

#[test]
#[ignore = "integration"]
fn zai_b64_json_present_is_used_directly() {
    // b64_json is not part of glm-image's contract but is tolerated at the
    // parse level — when present (and non-empty) it must be preferred over
    // a URL fetch.
    let mock = MockProvider::start(vec![(
        200,
        "application/json",
        r#"{"created":1,"data":[{"b64_json":"aVNTaGVQclBuZ0J5dGVz"}]}"#.to_string(),
    )]);
    let result = zai_client(&mock)
        .generate_image(&zai_sample_request(), None)
        .expect("generation succeeds");
    // The payload passes through verbatim; no download happened.
    assert_eq!(result.image_b64, "aVNTaGVQclBuZ0J5dGVz");
    assert_eq!(mock.requests().len(), 1, "no CDN fetch expected");
}

#[test]
#[ignore = "integration"]
fn zai_wire_body_has_model_prompt_only_plus_mapped_knobs() {
    // glm-image's schema is {model, prompt} + optional {size, quality}.
    // n / response_format / background / output_format are NOT documented
    // by z.ai and must never be sent, even when our request sets them
    // (explicit background is silently ignored — documented decision).
    let mock = MockProvider::start(vec![(
        200,
        "application/json",
        r#"{"created":1,"data":[{"url":"http://127.0.0.1:1/img.png"}]}"#.to_string(),
    )]);
    let req = ImageGenerationRequest {
        size: ImageSize::Portrait1024x1536,
        quality: ImageQuality::High,
        output_format: OutputFormat::Webp,
        background: Background::Transparent,
        ..zai_sample_request()
    };
    zai_client(&mock).generate_image(&req, None).expect_err(
        "127.0.0.1:1 download must fail (SSRF guard); the POST body assertions below still run",
    );
    // The generation POST body is what matters here.
    let body = mock.requests()[0].body_json();
    assert_eq!(body["model"], "glm-image");
    assert_eq!(body["prompt"], "a lighthouse at dusk");
    assert_eq!(body["size"], "1024x1536"); // verbatim wire string
    assert_eq!(body["quality"], "hd"); // High → hd (glm-image's sharp tier)
    // The never-send fields, pinned explicitly.
    assert!(body.get("n").is_none());
    assert!(body.get("response_format").is_none());
    assert!(body.get("output_format").is_none());
    assert!(body.get("background").is_none());
}

#[test]
#[ignore = "integration"]
fn zai_quality_map_low_medium_are_standard_high_is_hd_auto_omitted() {
    let mock = MockProvider::start(vec![
        (
            200,
            "application/json",
            r#"{"data":[{"b64_json":"aGk"}]}"#.to_string(),
        ), // High
        (
            200,
            "application/json",
            r#"{"data":[{"b64_json":"aGk"}]}"#.to_string(),
        ), // Medium
        (
            200,
            "application/json",
            r#"{"data":[{"b64_json":"aGk"}]}"#.to_string(),
        ), // Low
        (
            200,
            "application/json",
            r#"{"data":[{"b64_json":"aGk"}]}"#.to_string(),
        ), // Auto
    ]);
    for (quality, expected) in [
        (ImageQuality::High, Some("hd")),
        (ImageQuality::Medium, Some("standard")),
        (ImageQuality::Low, Some("standard")),
        (ImageQuality::Auto, None), // omitted entirely
    ] {
        let req = ImageGenerationRequest {
            quality,
            ..zai_sample_request()
        };
        zai_client(&mock).generate_image(&req, None).expect("ok");
        let body = mock.requests().last().expect("a request").body_json();
        match expected {
            Some(w) => assert_eq!(body["quality"], w, "quality {quality:?}"),
            None => assert!(body.get("quality").is_none(), "Auto must be omitted"),
        }
    }
    // And the POST path is the base the docs pin: "/paas/v4/…" composes
    // even against the coding-gateway base used for chat.
    assert_eq!(mock.requests()[0].path, "/paas/v4/images/generations");
}

#[test]
#[ignore = "integration"]
fn zai_flat_error_body_message_surfaces() {
    // z.ai errors are the FLAT {code, message} shape (not OpenAI's nested
    // error envelope) — the retry layer's `message` fallback must surface it.
    let mock = MockProvider::start(vec![(
        400,
        "application/json",
        r#"{"code":1212,"message":"invalid size"}"#.to_string(),
    )]);
    let err = zai_client(&mock)
        .generate_image(&zai_sample_request(), None)
        .expect_err("400 is terminal");
    match err {
        InferenceError::ClientError { status, detail } => {
            assert_eq!(status, 400);
            assert!(detail.contains("invalid size"), "{detail}");
            assert!(!detail.contains("1212"), "raw code must not leak");
        }
        other => panic!("expected ClientError, got {other:?}"),
    }
}

#[test]
#[ignore = "integration"]
fn zai_content_filter_blocks_with_clear_message_not_empty_response() {
    // Documented semantics: level 0 (most severe) ..= 3; any entry at level
    // 0..=2 marks the generation BLOCKED — a ContentFiltered error, NOT
    // EmptyResponse (and never a retry: policy blocks cannot clear on
    // resend). No HTTP status is fabricated: the response itself succeeded.
    let mock = MockProvider::start(vec![
        (
            200,
            "application/json",
            r#"{"created":1,"data":[{"url":"http://127.0.0.1:1/x.png"}],"content_filter":[{"role":"user","level":1}]}"#
                .to_string(),
        ),
    ]);
    let err = zai_client(&mock)
        .generate_image(&zai_sample_request(), None)
        .expect_err("level-1 filter entry must block");
    let rendered = err.to_string();
    match err {
        InferenceError::ContentFiltered { detail } => {
            // The variant's Display carries the human-readable policy message
            // (the detail holds the flag facts); no HTTP status is fabricated.
            assert!(
                rendered.contains("content filter blocked the generation"),
                "{rendered}"
            );
            assert!(detail.contains("level 1"), "{detail}");
            assert!(detail.contains("user"), "{detail}");
        }
        other => panic!("expected ContentFiltered, got {other:?}"),
    }
    assert_eq!(mock.requests().len(), 1, "blocked is terminal — no retry");
}

#[test]
#[ignore = "integration"]
fn zai_content_filter_level_3_is_advisory_not_blocking() {
    // Level 3 is the least severe (docs: 0 most severe, 3 least) — a level-3
    // entry must NOT turn the generation into an error.
    let mock = MockProvider::start(vec![(
        200,
        "application/json",
        r#"{"created":1,"data":[{"b64_json":"b2th"}],"content_filter":[{"role":"assistant","level":3}]}"#
            .to_string(),
    )]);
    let result = zai_client(&mock)
        .generate_image(&zai_sample_request(), None)
        .expect("level-3 is advisory");
    assert_eq!(result.image_b64, "b2th");
}

#[test]
#[ignore = "integration"]
fn zai_empty_data_is_empty_response_and_neither_field_is_nor() {
    // Empty data array → EmptyResponse (same convention as the OpenAI
    // adapter), not a deserialization failure.
    let mock = MockProvider::start(vec![(
        200,
        "application/json",
        r#"{"created":1,"data":[]}"#.to_string(),
    )]);
    let err = zai_client(&mock)
        .generate_image(&zai_sample_request(), None)
        .expect_err("empty data must error");
    assert!(matches!(err, InferenceError::EmptyResponse), "{err:?}");

    // An item with NEITHER url NOR b64_json → also EmptyResponse.
    let mock = MockProvider::start(vec![(
        200,
        "application/json",
        r#"{"created":1}"#.to_string(),
    )]);
    let err = zai_client(&mock)
        .generate_image(&zai_sample_request(), None)
        .expect_err("no data at all must error");
    assert!(matches!(err, InferenceError::EmptyResponse), "{err:?}");
}

#[test]
#[ignore = "integration"]
fn zai_defaults_and_trait_accessors() {
    let mock = MockProvider::start(vec![]);
    let c = zai_client(&mock);
    assert_eq!(c.provider_slug(), "zai");
    // The 180 s attempt deadline + 2-attempt budget come from the shared
    // adapter policy constants even when the chat config said otherwise.
    let mut config = ServiceConfig {
        base_url: mock.base_url("paas/v4"),
        provider_slug: "zai".to_string(),
        ..Default::default()
    };
    config.total_timeout_secs = 3600;
    let c = ZaiImageClient::new(
        config,
        "k".to_string(),
        &choreo_ai_protocols::SocketRegistry::new(),
    );
    assert_eq!(c.config().total_timeout_secs, 180);
}
