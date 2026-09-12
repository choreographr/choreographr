//! Integration test for the `generate_image` tool.
//!
//! Spins a local mock HTTP server returning a tiny valid PNG (base64) in the
//! `OpenAI` Images API response shape, builds a full `ToolContext` with a
//! crossbeam reply channel pre-loaded with an `ImageProviderHandle` wired to
//! `OpenAiImageClient` pointed at the mock URL, and executes the tool
//! end-to-end. Real sockets + real HTTP → `tests/` + `#[ignore = "integration"]` per the
//! Test Discipline policy (run via `cargo test-integration`).

// AGENTS.md permits unwrap/expect/panic in tests/ files, but clippy's
// allow-*-in-tests config only recognizes #[test]-annotated functions —
// helper fns in this file need this file-level allowance.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::panic_in_result_fn,
    clippy::indexing_slicing
)]
use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use choreo_ai_protocols::{OpenAiImageClient, openai::ServiceConfig};
use choreo_daemon::tools::Tool;
use choreo_daemon::tools::context::ToolContext;
use choreo_daemon::tools::image::DisplayImageReturn;
use choreo_daemon::tools::image_gen::{GenerateImage, GenerateImageArgs};
use choreo_daemon::{DaemonCommand, providers::ImageProviderHandle};
mod common;
use common::test_db;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::mpsc;
use std::thread;

/// One tiny 1x1 red PNG (67 bytes), so the prepare pipeline (dimension probe)
/// exercises the real `image` crate decode path.
const TINY_PNG: &[u8] = &[
    0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48, 0x44, 0x52,
    0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x02, 0x00, 0x00, 0x00, 0x90, 0x77, 0x53,
    0xDE, 0x00, 0x00, 0x00, 0x0C, 0x49, 0x44, 0x41, 0x54, 0x08, 0xD7, 0x63, 0xF8, 0xCF, 0xC0, 0x00,
    0x00, 0x03, 0x01, 0x01, 0x00, 0x18, 0xDD, 0x8D, 0xB0, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45, 0x4E,
    0x44, 0xAE, 0x42, 0x60, 0x82,
];

/// Mock `OpenAI` Images API server: serves one POST /v1/images/generations and
/// replies with the standard envelope carrying the tiny PNG as `b64_json`.
fn start_mock_images_server() -> (String, thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind ephemeral port");
    let addr = listener.local_addr().unwrap().to_string();
    let handle = thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept");
        let mut buf = [0u8; 8192];
        let n = stream.read(&mut buf).expect("read request");
        let request = String::from_utf8_lossy(&buf[..n]).to_string();
        assert!(
            request.contains("POST /v1/images/generations"),
            "unexpected request: {request}"
        );
        assert!(request.contains("Bearer sk-test"), "auth header missing");
        let b64 = BASE64.encode(TINY_PNG);
        let body = serde_json::json!({
            "data": [{
                "b64_json": b64,
                "revised_prompt": "a tiny red square, professionally lit"
            }]
        })
        .to_string();
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(),
            body
        );
        stream
            .write_all(response.as_bytes())
            .and_then(|()| stream.flush())
            .expect("write response");
    });
    (format!("http://{addr}/v1"), handle)
}

#[test]
#[ignore = "integration"]
fn generate_image_end_to_end_against_mock_server() {
    let (base_url, server) = start_mock_images_server();

    let client = OpenAiImageClient::new(
        ServiceConfig {
            base_url,
            ..Default::default()
        },
        "sk-test".to_string(),
        &choreo_ai_protocols::SocketRegistry::new(),
    );
    let handle = ImageProviderHandle {
        slug: "openai".to_string(),
        client: std::sync::Arc::new(client),
    };

    let db = std::sync::Arc::new(test_db());
    let (daemon_tx, daemon_rx) = mpsc::channel::<DaemonCommand>();
    // Mock daemon command loop: answer the tool's provider-resolution
    // command with the handle wired to the mock server.
    thread::spawn(move || match daemon_rx.recv().unwrap() {
        DaemonCommand::GetImageGenerationProvider { reply, .. } => {
            reply.send(Ok(handle)).unwrap();
        }
        _ => panic!("unexpected daemon command"),
    });
    let ctx = ToolContext::new(7, db, daemon_tx);

    let args: GenerateImageArgs = serde_json::from_str(
        r#"{"prompt": "a tiny red square", "output_format": "png", "alt": "tiny red"}"#,
    )
    .unwrap();
    let ret: DisplayImageReturn = GenerateImage::new()
        .execute(args, None, None, Some(&ctx))
        .expect("tool should succeed");

    assert_eq!(ret.image.mime_type(), "image/png");
    assert_eq!(ret.image.dimensions(), (1, 1));
    assert_eq!(ret.image.data(), TINY_PNG);
    assert_eq!(ret.image.alt_text(), Some("tiny red"));
    assert!(ret.text.contains("generated image"));
    assert!(
        ret.text
            .contains("revised prompt: a tiny red square, professionally lit")
    );
    // extract_image (the client-pipeline hook) reads the image off the return.
    let tool = GenerateImage::new();
    let extracted = tool.extract_image(&ret).expect("image should be extracted");
    assert_eq!(extracted.dimensions(), (1, 1));

    server.join().unwrap();
}
