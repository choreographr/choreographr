use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;
use tai_daemon::openai::{
    ChatRequestMessage, OpenAiClient, OpenAiError, RetryCallback, ServiceConfig,
};
use tai_daemon::providers::{ChatTurnRequest, ChatTurnResult, StreamEvent};

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
    format!(r#"{{"choices":[{{"message":{{"content":"{text}","tool_calls":[]}}}}]}}"#)
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
            let (mut stream, _) = listener.accept().unwrap_or_else(|e| panic!("accept: {e}"));
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
        http_response(
            "HTTP/1.1 429 Too Many Requests",
            r#"{"error":"rate limited"}"#,
        ),
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
    let client = OpenAiClient::new(config, "test-key".into()).expect("OpenAiClient");

    let call_count = Arc::new(AtomicU32::new(0));
    let count = call_count.clone();
    let mut cb: Option<RetryCallback> = Some(Box::new(move |_attempt, _max, _delay| {
        count.fetch_add(1, Ordering::SeqCst);
    }));

    let (_cancel_tx, cancel_rx) = mpsc::channel::<()>();
    let messages = [ChatRequestMessage::simple("user", "hello".into())];

    let result = client.chat_completion_turn(ChatTurnRequest {
        model: "retry-model",
        messages: &messages,
        tools: &[], // no tools
        thinking_effort: "off".to_string(),
        on_retry: &mut cb,
        cancel_rx: Some(&cancel_rx),
        previous_response_id: None,
        tool_results: &[],
        programmatic_tool_calling: false,
    });

    match result {
        Ok(ChatTurnResult::FinalText(final_text)) => {
            assert!(
                final_text.content.contains("hello from retry"),
                "content: {:?}",
                final_text.content
            );
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
    let client = OpenAiClient::new(config, "test-key".into()).expect("OpenAiClient");

    let (cancel_tx, cancel_rx) = mpsc::channel::<()>();
    let messages = [ChatRequestMessage::simple("user", "hello".into())];

    // Send the cancel signal after a brief delay so the HTTP request has
    // time to start and receive the 429 before we interrupt.
    thread::spawn(move || {
        thread::sleep(Duration::from_millis(100));
        let _ = cancel_tx.send(());
    });

    let mut cb: Option<RetryCallback> = None;
    let result = client.chat_completion_turn(ChatTurnRequest {
        model: "retry-model",
        messages: &messages,
        tools: &[], // no tools
        thinking_effort: "off".to_string(),
        on_retry: &mut cb,
        cancel_rx: Some(&cancel_rx),
        previous_response_id: None,
        tool_results: &[],
        programmatic_tool_calling: false,
    });

    assert!(
        matches!(result, Err(OpenAiError::Cancelled)),
        "expected Cancelled, got {result:?}"
    );
}

/// Start a local TCP server that sends SSE events continuously.
///
/// The server emits an SSE chunk every 20ms so the client's cancellation
/// check can run between reads.  The returned sender allows the test to
/// request extra events before the timer-driven stream kicks in.
fn spawn_sse_server() -> (u16, mpsc::Sender<()>, thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let port = listener.local_addr().expect("local_addr").port();

    let (tx, rx) = mpsc::channel::<()>();

    let handle = thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept");
        let mut buf = [0u8; 4096];
        let _ = stream.read(&mut buf);

        let headers = "HTTP/1.1 200 OK\r\n\
             Content-Type: text/event-stream\r\n\
             Cache-Control: no-cache\r\n\
             Connection: close\r\n\
             \r\n";
        stream.write_all(headers.as_bytes()).expect("write headers");
        stream.flush().ok();

        // Drain any immediate event requests from the test, then keep
        // sending events on a timer until the client disconnects.
        loop {
            // Check for explicit event requests (non-blocking).
            while let Ok(()) = rx.try_recv() {
                let event = "data: {\"choices\":[{\"delta\":{\"content\":\"chunk\"}}]}\n\n";
                if stream.write_all(event.as_bytes()).is_err() || stream.flush().is_err() {
                    return;
                }
            }
            // Timer-driven event so the client's cancellation check fires.
            thread::sleep(Duration::from_millis(20));
            let event = "data: {\"choices\":[{\"delta\":{\"content\":\"chunk\"}}]}\n\n";
            if stream.write_all(event.as_bytes()).is_err() || stream.flush().is_err() {
                return;
            }
        }
    });

    (port, tx, handle)
}

/// Helper: build a minimal client config for the local SSE server.
fn sse_test_config(port: u16) -> ServiceConfig {
    ServiceConfig {
        base_url: format!("http://127.0.0.1:{port}"),
        default_request_format: tai_daemon::openai::RequestFormat::ChatCompletions,
        streaming: true,
        retry_max_attempts: 1,
        ..ServiceConfig::default()
    }
}

#[test]
#[ignore]
fn streaming_cancelled_during_sse_events() {
    let (port, event_tx, _server) = spawn_sse_server();
    let client = OpenAiClient::new(sse_test_config(port), "test-key".into()).expect("OpenAiClient");

    let (cancel_tx, cancel_rx) = mpsc::channel::<()>();

    // Let a few SSE events through first, then cancel after a delay.
    event_tx.send(()).ok();
    event_tx.send(()).ok();

    let cancel_handle = thread::spawn(move || {
        thread::sleep(Duration::from_millis(100));
        let _ = cancel_tx.send(());
    });

    let result = client.chat_completion_turn_streaming(
        ChatTurnRequest {
            model: "test-model",
            messages: &[ChatRequestMessage::simple("user", "hello".into())],
            tools: &[],
            thinking_effort: "off".to_string(),
            on_retry: &mut None,
            cancel_rx: Some(&cancel_rx),
            previous_response_id: None,
            tool_results: &[],
            programmatic_tool_calling: false,
        },
        |_event: StreamEvent| -> std::io::Result<()> { Ok(()) },
    );

    cancel_handle.join().ok();

    assert!(
        matches!(result, Err(OpenAiError::Cancelled)),
        "expected Cancelled, got {result:?}"
    );
}

#[test]
#[ignore]
fn streaming_cancelled_before_first_event() {
    let (port, _event_tx, _server) = spawn_sse_server();
    let client = OpenAiClient::new(sse_test_config(port), "test-key".into()).expect("OpenAiClient");

    let (cancel_tx, cancel_rx) = mpsc::channel::<()>();

    // Send cancel before the streaming call — the HTTP request hasn't been
    // made yet, so cancellation happens during the retry/connect phase.
    cancel_tx.send(()).ok();

    let result = client.chat_completion_turn_streaming(
        ChatTurnRequest {
            model: "test-model",
            messages: &[ChatRequestMessage::simple("user", "hello".into())],
            tools: &[],
            thinking_effort: "off".to_string(),
            on_retry: &mut None,
            cancel_rx: Some(&cancel_rx),
            previous_response_id: None,
            tool_results: &[],
            programmatic_tool_calling: false,
        },
        |_event: StreamEvent| -> std::io::Result<()> { Ok(()) },
    );

    assert!(
        matches!(result, Err(OpenAiError::Cancelled)),
        "expected Cancelled, got {result:?}"
    );
}
