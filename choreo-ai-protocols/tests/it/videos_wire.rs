//! Wire tests for the fal.ai video queue adapter ([`FalVideoClient`]), served
//! by the shared scripted HTTP provider ([`MockProvider`]).
//!
//! These tests bind a local `TcpListener`, so they exercise the full
//! request→response path against real sockets; the AGENTS.md test-discipline
//! rule keeps socket tests in `tests/` directories and marks them
//! `#[ignore]` (run via `cargo test-integration` / `nextest --run-ignored`),
//! so a plain `cargo test-fast` never opens a socket.
//!
//! The client's poll interval is set to zero, so the provided
//! `generate_video` driver never sleeps — everything here is deterministic.

// AGENTS.md permits unwrap/expect/panic in tests/ files, but clippy's
// allow-*-in-tests config only recognizes #[test]-annotated functions —
// helper fns in this file need this file-level allowance.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use choreo_ai_protocols::openai::ServiceConfig;
use choreo_ai_protocols::test_utils::MockProvider;
use choreo_ai_protocols::{
    FalVideoClient, VideoGenerationClient, VideoGenerationRequest, VideoJobStatus, VideoResolution,
};
use choreo_proto::InferenceError;
use std::time::Duration;

const MODEL: &str = "minimax/h3-max/text-to-video";

/// Build a client pointed at the mock provider, with no inter-poll sleep and
/// no backoff sleeping.
fn client(mock: &MockProvider) -> FalVideoClient {
    let config = ServiceConfig {
        base_url: mock.base_url("fal"),
        provider_slug: "fal".to_string(),
        retry_initial_backoff_ms: 0,
        ..Default::default()
    };
    FalVideoClient::new(
        config,
        "fal-test-key".to_string(),
        &choreo_ai_protocols::SocketRegistry::new(),
    )
    .with_poll_interval(Duration::ZERO)
}

fn sample_request() -> VideoGenerationRequest {
    VideoGenerationRequest {
        resolution: Some(VideoResolution::R1080),
        duration_secs: Some(5),
        ..VideoGenerationRequest::new("a rocket launch at dawn", MODEL)
    }
}

/// The submit response body: the job id plus the three authoritative URLs,
/// all pointing back at the mock server.
fn submit_body(base: &str) -> String {
    serde_json::json!({
        "status": "IN_QUEUE",
        "request_id": "req-1",
        "status_url": format!("{base}/status/req-1"),
        "response_url": format!("{base}/result/req-1"),
        "cancel_url": format!("{base}/cancel/req-1"),
    })
    .to_string()
}

// ── happy path ───────────────────────────────────────────────────────────

#[test]
#[ignore = "integration"]
fn happy_path_submits_polls_and_collects_the_result() {
    let mock = MockProvider::start_scripted(|base| {
        vec![
            (200, "application/json", submit_body(base)),
            (
                200,
                "application/json",
                serde_json::json!({ "status": "IN_QUEUE", "queue_position": 3 }).to_string(),
            ),
            (
                200,
                "application/json",
                serde_json::json!({ "status": "IN_PROGRESS", "logs": ["rendering frame 1"] })
                    .to_string(),
            ),
            (
                200,
                "application/json",
                serde_json::json!({
                    "status": "COMPLETED",
                    "metrics": { "inference_time": 12.5 }
                })
                .to_string(),
            ),
            (
                200,
                "application/json",
                serde_json::json!({
                    "video": {
                        "url": format!("{base}/o.mp4"),
                        "content_type": "video/mp4",
                        "file_name": "o.mp4",
                        "file_size": 4242
                    }
                })
                .to_string(),
            ),
        ]
    });

    let mut progress: Vec<VideoJobStatus> = Vec::new();
    let result = client(&mock)
        .generate_video(&sample_request(), None, &mut |status| progress.push(status))
        .expect("generation succeeds");

    assert_eq!(result.url, mock.base_url("o.mp4"));
    assert_eq!(result.content_type.as_deref(), Some("video/mp4"));
    assert_eq!(result.file_size, Some(4242));
    assert_eq!(result.model, MODEL);

    // Every lifecycle stage was reported in order.
    assert!(
        matches!(progress[0], VideoJobStatus::Queued { .. }),
        "{progress:?}"
    );
    assert!(
        matches!(&progress[1], VideoJobStatus::InProgress { logs } if logs == &["rendering frame 1".to_string()]),
        "{progress:?}"
    );
    match &progress[2] {
        VideoJobStatus::Completed { metrics } => {
            assert_eq!(
                metrics.as_ref().and_then(|m| m.inference_time_secs),
                Some(12.5)
            );
        }
        other => panic!("expected Completed, got {other:?}"),
    }
    assert_eq!(progress.len(), 3);

    // The exact wire: POST to the model path with the `Key` auth scheme, then
    // three status GETs (to the verbatim status_url) and one result GET.
    let requests = mock.requests();
    assert_eq!(requests.len(), 5);
    assert_eq!(requests[0].method, "POST");
    assert_eq!(requests[0].path, format!("/fal/{MODEL}"));
    assert_eq!(
        requests[0].header("authorization"),
        Some("Key fal-test-key")
    );
    for (i, req) in requests.iter().enumerate().take(4).skip(1) {
        assert_eq!(req.method, "GET", "request {i}");
        assert_eq!(req.path, "/status/req-1", "request {i}");
        assert_eq!(req.header("authorization"), Some("Key fal-test-key"));
    }
    assert_eq!(requests[4].method, "GET");
    assert_eq!(requests[4].path, "/result/req-1");
}

#[test]
#[ignore = "integration"]
fn h3_wire_body_pins_expansion_mode_and_resolution() {
    let mock = MockProvider::start_scripted(|base| {
        vec![
            (200, "application/json", submit_body(base)),
            (
                200,
                "application/json",
                serde_json::json!({ "status": "COMPLETED" }).to_string(),
            ),
            (
                200,
                "application/json",
                serde_json::json!({ "video": { "url": format!("{base}/o.mp4") } }).to_string(),
            ),
        ]
    });

    let _ = client(&mock)
        .generate_video(&sample_request(), None, &mut |_| {})
        .expect("generation succeeds");

    let body = mock.requests()[0].body_json();
    assert_eq!(body["prompt"], "a rocket launch at dawn");
    assert_eq!(body["prompt_expansion_mode"], "balanced");
    assert_eq!(body["resolution"], "1080P"); // H3 uppercase P
    assert_eq!(body["duration"], 5); // integer seconds
    // The model is the URL path segment, never a body field.
    assert!(body.get("model").is_none());
}

// ── cancel ────────────────────────────────────────────────────────────────

#[test]
#[ignore = "integration"]
fn cancel_signals_the_job_and_returns_cancelled() {
    // The job stays IN_PROGRESS forever; a pre-queued cancel makes the first
    // inter-poll wait resolve immediately (no sleeping).
    let mock = MockProvider::start_scripted(|base| {
        vec![
            (200, "application/json", submit_body(base)),
            (
                200,
                "application/json",
                serde_json::json!({ "status": "IN_PROGRESS" }).to_string(),
            ),
        ]
    });

    let (tx, rx) = crossbeam_channel::unbounded::<()>();
    tx.send(()).expect("pre-queue the cancel");

    let err = client(&mock)
        .generate_video(&sample_request(), Some(&rx), &mut |_| {})
        .expect_err("a cancelled run errors");
    assert!(matches!(err, InferenceError::Cancelled), "{err:?}");

    // submit, one status GET, then the cancel PUT to the verbatim cancel_url.
    let requests = mock.requests();
    assert_eq!(requests.len(), 3, "{requests:?}");
    assert_eq!(requests[2].method, "PUT");
    assert_eq!(requests[2].path, "/cancel/req-1");
    assert_eq!(
        requests[2].header("authorization"),
        Some("Key fal-test-key")
    );
}

// ── errors ─────────────────────────────────────────────────────────────────

#[test]
#[ignore = "integration"]
fn submit_content_policy_maps_to_content_filtered_without_retry() {
    let mock = MockProvider::start(vec![(
        422,
        "application/json",
        r#"{"detail":[{"loc":["body"],"msg":"prompt flagged","type":"content_policy_violation"}]}"#
            .to_string(),
    )]);
    let err = client(&mock)
        .generate_video(&sample_request(), None, &mut |_| {})
        .expect_err("content policy is terminal");
    match err {
        InferenceError::ContentFiltered { detail } => assert_eq!(detail, "prompt flagged"),
        other => panic!("expected ContentFiltered, got {other:?}"),
    }
    // A duplicate submit double-bills — never retried.
    assert_eq!(mock.requests().len(), 1);
}

#[test]
#[ignore = "integration"]
fn submit_runner_error_maps_to_server_error() {
    // A flat infra error body carrying the machine-readable error_type.
    let mock = MockProvider::start(vec![(
        500,
        "application/json",
        r#"{"detail":"runner crashed","error_type":"runner_error"}"#.to_string(),
    )]);
    let err = client(&mock)
        .generate_video(&sample_request(), None, &mut |_| {})
        .expect_err("runner failure is a server error");
    match err {
        InferenceError::ServerError { detail, .. } => assert_eq!(detail, "runner crashed"),
        other => panic!("expected ServerError, got {other:?}"),
    }
    // Submit is never retried even on a retryable 5xx.
    assert_eq!(mock.requests().len(), 1);
}

#[test]
#[ignore = "integration"]
fn submit_error_type_in_header_backstops_a_bare_body() {
    // A body with no error_type relies on the X-Fal-Error-Type header (which
    // MockProvider cannot set, so this uses a one-shot local server). Submit
    // is not retried → one request is enough.
    let addr = one_shot_response(
        503,
        &[("X-Fal-Error-Type", "runner_disconnected")],
        r#"{"detail":"runner went away"}"#,
    );
    let config = ServiceConfig {
        base_url: format!("http://{addr}/fal"),
        provider_slug: "fal".to_string(),
        ..Default::default()
    };
    let err = FalVideoClient::new(
        config,
        "fal-test-key".to_string(),
        &choreo_ai_protocols::SocketRegistry::new(),
    )
    .with_poll_interval(Duration::ZERO)
    .generate_video(&sample_request(), None, &mut |_| {})
    .expect_err("503 is a server error");
    match err {
        InferenceError::ServerError { status, detail } => {
            assert_eq!(status, 503);
            assert_eq!(detail, "runner went away");
        }
        other => panic!("expected ServerError, got {other:?}"),
    }
}

#[test]
#[ignore = "integration"]
fn completed_status_carrying_error_maps_to_server_error() {
    // The THIRD error site: a 2xx status body whose `status` is COMPLETED but
    // which carries `error` + `error_type` for a failed job.
    let mock = MockProvider::start_scripted(|base| {
        vec![
            (200, "application/json", submit_body(base)),
            (
                200,
                "application/json",
                serde_json::json!({
                    "status": "COMPLETED",
                    "error": "the runner crashed",
                    "error_type": "runner_crash"
                })
                .to_string(),
            ),
        ]
    });
    let err = client(&mock)
        .generate_video(&sample_request(), None, &mut |_| {})
        .expect_err("a failed job errors");
    match err {
        InferenceError::ServerError { detail, .. } => assert_eq!(detail, "the runner crashed"),
        other => panic!("expected ServerError, got {other:?}"),
    }
}

#[test]
#[ignore = "integration"]
fn submit_401_maps_to_unauthorized() {
    let mock = MockProvider::start(vec![(
        401,
        "application/json",
        r#"{"detail":"invalid api key"}"#.to_string(),
    )]);
    let err = client(&mock)
        .generate_video(&sample_request(), None, &mut |_| {})
        .expect_err("401 is terminal");
    assert!(
        matches!(err, InferenceError::Unauthorized { status: 401, .. }),
        "{err:?}"
    );
}

#[test]
#[ignore = "integration"]
fn submit_429_with_retry_after_maps_to_rate_limited() {
    // MockProvider cannot script a Retry-After header, so this uses a one-shot
    // local server. Retry-After must survive into the typed error even though
    // submit is not retried.
    let addr = one_shot_response(
        429,
        &[("Retry-After", "3600")],
        r#"{"detail":"quota exhausted"}"#,
    );
    let config = ServiceConfig {
        base_url: format!("http://{addr}/fal"),
        provider_slug: "fal".to_string(),
        ..Default::default()
    };
    let err = FalVideoClient::new(
        config,
        "fal-test-key".to_string(),
        &choreo_ai_protocols::SocketRegistry::new(),
    )
    .with_poll_interval(Duration::ZERO)
    .generate_video(&sample_request(), None, &mut |_| {})
    .expect_err("429 is terminal on submit");
    match err {
        InferenceError::RateLimited {
            status,
            retry_after_secs: Some(3600),
            ..
        } => assert_eq!(status, 429),
        other => panic!("expected RateLimited 429 with Retry-After 3600, got {other:?}"),
    }
}

/// A one-shot local server that serves a fixed status + custom headers + body
/// to every connection (so a retry reconnect still sees the scripted status).
/// For cases [`MockProvider`] cannot script (it only sets Content-Type).
fn one_shot_response(
    status: u16,
    headers: &[(&'static str, &'static str)],
    body: &'static str,
) -> std::net::SocketAddr {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("addr");
    let mut extra = String::new();
    for (k, v) in headers {
        extra.push_str(k);
        extra.push_str(": ");
        extra.push_str(v);
        extra.push_str("\r\n");
    }
    let head = format!(
        "HTTP/1.1 {status} Error\r\n{extra}Content-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    std::thread::spawn(move || {
        use std::io::{Read as _, Write as _};
        while let Ok((mut stream, _)) = listener.accept() {
            // Drain the request head + body before responding so a client
            // still writing does not see an RST/EPIPE instead of the scripted
            // status (same rationale as the images wire tests).
            let _ = stream.set_read_timeout(Some(Duration::from_millis(300)));
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
