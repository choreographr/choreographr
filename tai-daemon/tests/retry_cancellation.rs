use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::mpsc;
use std::sync::Arc;
use std::thread;
use std::time::Duration;
use tai_daemon::openai::{
    ChatRequestMessage, ChatTurnResult, OpenAiClient, OpenAiError, RetryCallback, ServiceConfig,
};

// ── Helpers ───────────────────────────────────────────────────────

/// Build an HTTP response byte buffer.
fn http_response(status_line: &str, body: &str) -> Vec<u8> {
    format!(
        "{status_line}\r\n\
         Content-Type: application/json\r\n\
         Content-Length: {}\r\n\
         Connection: close\r\n\
         \r\n\
         {body}",
        body.len(),
    )
    .into_bytes()
}

/// A 200 chat-completion response containing `text`.
fn ok_body(text: &str) -> String {
    format!(
        r#"{{"choices":[{{"message":{{"content":"{text}","tool_calls":[]}}}}]}}"#
    )
}

/// Start a local TCP server that responds to each connection in order.
///
/// Returns the port and a join handle.  The server is shut down when the
/// join handle is dropped (the test process exits).
fn spawn_http_server(responses: Vec<Vec<u8>>) -> (u16, thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let port = listener.local_addr().expect("local_addr").port();

    let handle = thread::spawn(move || {
        for response in &responses {
            let mut buf = [0u8; 4096];
            let (mut stream, _) = listener
                .accept()
                .unwrap_or_else(|e| panic!("accept: {e}"));
            // Read the HTTP request (we don't care about the content).
            let _ = stream.read(&mut buf);
            stream
                .write_all(response)
                .unwrap_or_else(|e| panic!("write: {e}"));
            stream.flush().ok();
            // Connection: close → client will open a new connection for the
            // next attempt, so the stream is dropped here.
        }
    });

    (port, handle)
}

// ── Tests ─────────────────────────────────────────────────────────

#[test]
#[ignore]
fn retry_succeeds_with_callback() {
    let (port, _server) = spawn_http_server(vec![
        http_response("HTTP/1.1 429 Too Many Requests", r#"{"error":"rate limited"}"#),
        http_response("HTTP/1.1 200 OK", &ok_body("hello from retry")),
    ]);

    let config = ServiceConfig {
        base_url: format!("http://127.0.0.1:{port}"),
        retry_max_attempts: 3,
        retry_initial_backoff_ms: 10,
        retry_max_backoff_ms: 100,
        // Use chat-completions (not streaming) so `completion` is
        // synchronous and we get a clean text result.
        default_request_format: tai_daemon::openai::RequestFormat::ChatCompletions,
        streaming: false,
        ..ServiceConfig::default()
    };
    let client =
        OpenAiClient::new(config, "test-key".into()).expect("OpenAiClient");

    let call_count = Arc::new(AtomicU32::new(0));
    let count = call_count.clone();
    let mut cb: Option<RetryCallback> = Some(Box::new(move |_attempt, _max, _delay| {
        count.fetch_add(1, Ordering::SeqCst);
    }));

    let (_cancel_tx, cancel_rx) = mpsc::channel::<()>();
    let messages = [ChatRequestMessage::simple("user", "hello".into())];

    let result = client.chat_completion_turn(
        "retry-model",
        &messages,
        &[], // no tools
        &mut cb,
        Some(&cancel_rx),
    );

    match result {
        Ok(ChatTurnResult::FinalText(content)) => {
            assert!(content.contains("hello from retry"), "content: {content:?}");
        }
        Ok(other) => panic!("unexpected Ok variant: {other:?}"),
        Err(e) => panic!("expected Ok, got {e:?}"),
    }

    assert_eq!(
        call_count.load(Ordering::SeqCst),
        1,
        "callback should fire exactly once (one retry after 429)"
    );
}

#[test]
#[ignore]
fn retry_cancelled_during_backoff() {
    // Server returns 429 and will NOT receive a second request because
    // the client should cancel during the backoff wait.
    let (port, _server) = spawn_http_server(vec![http_response(
        "HTTP/1.1 429 Too Many Requests",
        r#"{"error":"rate limited"}"#,
    )]);

    let config = ServiceConfig {
        base_url: format!("http://127.0.0.1:{port}"),
        // Use a long enough backoff so the cancel signal has time to
        // arrive before the wait expires.
        retry_max_attempts: 3,
        retry_initial_backoff_ms: 5000,
        retry_max_backoff_ms: 30000,
        default_request_format: tai_daemon::openai::RequestFormat::ChatCompletions,
        streaming: false,
        ..ServiceConfig::default()
    };
    let client =
        OpenAiClient::new(config, "test-key".into()).expect("OpenAiClient");

    let (cancel_tx, cancel_rx) = mpsc::channel::<()>();
    let messages = [ChatRequestMessage::simple("user", "hello".into())];

    // Send the cancel signal after a brief delay so the HTTP request has
    // time to start and receive the 429 before we interrupt.
    thread::spawn(move || {
        thread::sleep(Duration::from_millis(100));
        let _ = cancel_tx.send(());
    });

    let mut cb: Option<RetryCallback> = None;
    let result = client.chat_completion_turn(
        "retry-model",
        &messages,
        &[],
        &mut cb,
        Some(&cancel_rx),
    );

    assert!(
        matches!(result, Err(OpenAiError::Cancelled)),
        "expected Cancelled, got {result:?}"
    );
}
