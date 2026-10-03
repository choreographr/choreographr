//! Integration tests for the `choreo-mcp` Streamable HTTP transport, driven by
//! a minimal in-process HTTP fixture (a `TcpListener` on `127.0.0.1:0`).
//!
//! The fixture is scripted per test: it answers `server/discover`, `initialize`,
//! `tools/list`, and `tools/call` with either a JSON or an SSE body, can reject
//! the first probe with a retryable `503`, and can impersonate the removed
//! 2024-11-05 HTTP+SSE transport. It also validates the routing headers rmcp
//! generates, so a header regression fails the suite rather than going unnoticed.
//!
//! These bind a local socket, so they belong here and are marked `#[ignore]` per
//! the workspace test discipline (`cargo test` runs only unit tests;
//! `cargo test-integration` runs these).

use crate::common::watchdog;
use choreo_mcp::{McpError, McpProtocolMode, McpServer, McpServerConfig, McpTransport};
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

/// Build an HTTP client config pointing at `url`.
fn http_config(url: &str, protocol: McpProtocolMode) -> McpServerConfig {
    McpServerConfig {
        slug: "http-fixture".to_string(),
        transport: McpTransport::Http {
            url: url.to_string(),
            headers: HashMap::new(),
        },
        enabled: true,
        timeout: Some(Duration::from_secs(10)),
        protocol,
        max_concurrent_calls: None,
    }
}

/// Which script the fixture server should run.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Scenario {
    /// 2026-07-28 stateless server (discover, tools, SSE call).
    Modern,
    /// Legacy server: rejects `server/discover`, answers `initialize`.
    Legacy,
    /// Answers the first `server/discover` with a retryable `503`.
    Retry,
    /// Impersonates the removed 2024-11-05 HTTP+SSE transport.
    LegacySse,
}

/// A running fixture server plus the URL to reach it and the count of
/// `server/discover` POSTs it has seen (used to prove a retry happened).
struct Fixture {
    url: String,
    discover_attempts: Arc<AtomicUsize>,
    _join: std::thread::JoinHandle<()>,
}

/// Start the fixture on an ephemeral loopback port.
fn start_fixture(scenario: Scenario) -> std::io::Result<Fixture> {
    let listener = TcpListener::bind("127.0.0.1:0")?;
    let addr = listener.local_addr()?;
    let discover_attempts = Arc::new(AtomicUsize::new(0));
    let attempts = Arc::clone(&discover_attempts);
    let join = std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(stream) = stream else { continue };
            let attempts = Arc::clone(&attempts);
            std::thread::spawn(move || handle_connection(stream, scenario, &attempts));
        }
    });
    Ok(Fixture {
        url: format!("http://{addr}/mcp"),
        discover_attempts,
        _join: join,
    })
}

/// A parsed HTTP request (headers lower-cased, body as text).
struct Request {
    method: String,
    headers: HashMap<String, String>,
    body: String,
}

/// Read one HTTP/1.1 request off `stream`.
fn read_request(stream: &TcpStream) -> Option<Request> {
    let mut reader = BufReader::new(stream.try_clone().ok()?);
    let mut request_line = String::new();
    reader.read_line(&mut request_line).ok()?;
    let mut parts = request_line.split_whitespace();
    let method = parts.next()?.to_string();
    parts.next()?; // path — unused by the fixture

    let mut headers = HashMap::new();
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).ok()? == 0 {
            break;
        }
        let line = line.trim_end();
        if line.is_empty() {
            break;
        }
        if let Some((key, value)) = line.split_once(':') {
            headers.insert(key.trim().to_ascii_lowercase(), value.trim().to_string());
        }
    }
    let length: usize = headers
        .get("content-length")
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    let mut body = vec![0u8; length];
    if length > 0 {
        reader.read_exact(&mut body).ok()?;
    }
    Some(Request {
        method,
        headers,
        body: String::from_utf8_lossy(&body).into_owned(),
    })
}

/// Write an HTTP/1.1 response and close the connection.
fn write_response(stream: &mut TcpStream, status: &str, content_type: &str, body: &str) {
    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {len}\r\nConnection: close\r\n\r\n{body}",
        len = body.len()
    );
    let _ = stream.write_all(response.as_bytes());
    let _ = stream.flush();
}

/// The JSON-RPC envelope for a success response.
fn json_result(id: &serde_json::Value, result: &serde_json::Value) -> String {
    serde_json::json!({"jsonrpc": "2.0", "id": id, "result": result}).to_string()
}

/// The JSON-RPC envelope for an error response.
fn json_error(id: &serde_json::Value, code: i64, message: &str) -> String {
    serde_json::json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": {"code": code, "message": message},
    })
    .to_string()
}

/// A 2026-07-28 `DiscoverResult`.
fn discover_result() -> serde_json::Value {
    serde_json::json!({
        "resultType": "complete",
        "supportedVersions": ["2026-07-28"],
        "capabilities": {"tools": {}},
        "ttlMs": 0,
        "cacheScope": "public",
        "_meta": {
            "io.modelcontextprotocol/serverInfo": {"name": "http-fixture", "version": "0.1.0"}
        }
    })
}

/// The tool catalogue the fixture advertises.
fn tools() -> serde_json::Value {
    serde_json::json!([
        {"name": "echo", "description": "Echo a message back.",
         "inputSchema": {"type": "object", "properties": {"message": {"type": "string"}}}},
        {"name": "sse", "description": "Answer over an SSE stream.", "inputSchema": {"type": "object"}},
    ])
}

/// Serve one connection according to the script.
fn handle_connection(mut stream: TcpStream, scenario: Scenario, attempts: &Arc<AtomicUsize>) {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(10)));
    let Some(request) = read_request(&stream) else {
        return;
    };
    if request.method == "GET" {
        if scenario == Scenario::LegacySse {
            // The removed HTTP+SSE transport: the first event is `endpoint`.
            write_response(
                &mut stream,
                "200 OK",
                "text/event-stream",
                "event: endpoint\ndata: /messages?sessionId=abc\n\n",
            );
        } else {
            // Streamable HTTP removed the GET stream.
            write_response(&mut stream, "405 Method Not Allowed", "text/plain", "");
        }
        return;
    }

    let Ok(value) = serde_json::from_str::<serde_json::Value>(&request.body) else {
        write_response(&mut stream, "400 Bad Request", "text/plain", "bad json");
        return;
    };
    let method = value.get("method").and_then(|m| m.as_str()).unwrap_or("");
    // Notifications carry no `id` and expect only an acknowledgement.
    let Some(id) = value.get("id") else {
        write_response(&mut stream, "202 Accepted", "text/plain", "");
        return;
    };

    match scenario {
        Scenario::Modern => modern_route(&mut stream, &request, method, id, &value),
        Scenario::Legacy => legacy_route(&mut stream, method, id),
        Scenario::Retry => retry_route(&mut stream, method, id, attempts),
        Scenario::LegacySse => {
            // Every POST is rejected so the client falls back to the GET probe.
            write_response(
                &mut stream,
                "400 Bad Request",
                "text/plain",
                "use the GET endpoint",
            );
        }
    }
}

/// Route a POST for the modern server, validating the generated headers.
fn modern_route(
    stream: &mut TcpStream,
    request: &Request,
    method: &str,
    id: &serde_json::Value,
    value: &serde_json::Value,
) {
    // Every request must carry the protocol version; the routing headers are
    // required for the standard-header era.
    if !request.headers.contains_key("mcp-protocol-version") {
        write_response(
            stream,
            "400 Bad Request",
            "application/json",
            &json_error(id, -32600, "missing MCP-Protocol-Version header"),
        );
        return;
    }
    match method {
        "server/discover" => write_response(
            stream,
            "200 OK",
            "application/json",
            &json_result(id, &discover_result()),
        ),
        "tools/list" => {
            if request.headers.get("mcp-method").map(String::as_str) != Some("tools/list") {
                write_response(
                    stream,
                    "400 Bad Request",
                    "application/json",
                    &json_error(id, -32600, "missing Mcp-Method header"),
                );
                return;
            }
            write_response(
                stream,
                "200 OK",
                "application/json",
                &json_result(id, &serde_json::json!({"tools": tools()})),
            );
        }
        "tools/call" => {
            let name = value
                .pointer("/params/name")
                .and_then(|n| n.as_str())
                .unwrap_or("");
            if request.headers.get("mcp-name").map(String::as_str) != Some(name) {
                write_response(
                    stream,
                    "400 Bad Request",
                    "application/json",
                    &json_error(id, -32600, "missing or mismatched Mcp-Name header"),
                );
                return;
            }
            call_route(stream, id, name, value);
        }
        _ => write_response(
            stream,
            "404 Not Found",
            "application/json",
            &json_error(id, -32601, "method not found"),
        ),
    }
}

/// Answer a `tools/call` with either a JSON body (`echo`) or an SSE stream
/// (`sse`).
fn call_route(
    stream: &mut TcpStream,
    id: &serde_json::Value,
    name: &str,
    value: &serde_json::Value,
) {
    if name == "sse" {
        let payload = json_result(
            id,
            &serde_json::json!({
                "content": [{"type": "text", "text": "sse: ok"}],
                "isError": false,
            }),
        );
        let body = format!("event: message\ndata: {payload}\n\n");
        write_response(stream, "200 OK", "text/event-stream", &body);
    } else {
        let message = value
            .pointer("/params/arguments/message")
            .and_then(|m| m.as_str())
            .unwrap_or("");
        write_response(
            stream,
            "200 OK",
            "application/json",
            &json_result(
                id,
                &serde_json::json!({
                    "content": [{"type": "text", "text": format!("echo: {message}")}],
                    "isError": false,
                }),
            ),
        );
    }
}

/// Route a POST for the legacy server (rejects discover, answers initialize).
fn legacy_route(stream: &mut TcpStream, method: &str, id: &serde_json::Value) {
    match method {
        "server/discover" => write_response(
            stream,
            "200 OK",
            "application/json",
            &json_error(id, -32601, "method not found"),
        ),
        "initialize" => write_response(
            stream,
            "200 OK",
            "application/json",
            &json_result(
                id,
                &serde_json::json!({
                    "protocolVersion": "2025-11-25",
                    "capabilities": {"tools": {}},
                    "serverInfo": {"name": "http-fixture", "version": "0.1.0"},
                }),
            ),
        ),
        "tools/list" => write_response(
            stream,
            "200 OK",
            "application/json",
            &json_result(id, &serde_json::json!({"tools": tools()})),
        ),
        _ => write_response(
            stream,
            "404 Not Found",
            "application/json",
            &json_error(id, -32601, "method not found"),
        ),
    }
}

/// Route a POST for the retry server: the first discover is a transient 503.
fn retry_route(
    stream: &mut TcpStream,
    method: &str,
    id: &serde_json::Value,
    attempts: &AtomicUsize,
) {
    match method {
        "server/discover" => {
            if attempts.fetch_add(1, Ordering::SeqCst) == 0 {
                write_response(stream, "503 Service Unavailable", "text/plain", "try later");
            } else {
                write_response(
                    stream,
                    "200 OK",
                    "application/json",
                    &json_result(id, &discover_result()),
                );
            }
        }
        "tools/list" => write_response(
            stream,
            "200 OK",
            "application/json",
            &json_result(id, &serde_json::json!({"tools": tools()})),
        ),
        _ => write_response(
            stream,
            "404 Not Found",
            "application/json",
            &json_error(id, -32601, "method not found"),
        ),
    }
}

#[test]
#[ignore = "integration: binds a local HTTP socket per workspace test discipline"]
fn http_modern_lists_and_calls() {
    watchdog();
    let fixture = start_fixture(Scenario::Modern).expect("start fixture");
    let server = McpServer::connect(&http_config(&fixture.url, McpProtocolMode::Auto))
        .expect("connect modern HTTP fixture");
    let handle = server.handle();

    let tools = handle.list_tools().expect("list tools");
    assert!(
        tools.iter().any(|t| t.name == "echo"),
        "expected 'echo', got: {:?}",
        tools.iter().map(|t| &t.name).collect::<Vec<_>>()
    );

    let result = handle
        .call_tool(1, "echo", serde_json::json!({"message": "hi"}), None)
        .expect("call echo");
    assert!(!result.is_error);
    assert!(
        result.content.iter().any(
            |c| matches!(c, choreo_mcp::McpContent::Text { text } if text.contains("echo: hi"))
        ),
        "echo should return our message, got: {:?}",
        result.content
    );
}

#[test]
#[ignore = "integration: binds a local HTTP socket per workspace test discipline"]
fn http_sse_call_streams_result() {
    watchdog();
    let fixture = start_fixture(Scenario::Modern).expect("start fixture");
    let server = McpServer::connect(&http_config(&fixture.url, McpProtocolMode::Auto))
        .expect("connect modern HTTP fixture");

    let result = server
        .handle()
        .call_tool(1, "sse", serde_json::json!({}), None)
        .expect("call sse");
    assert!(
        result.content.iter().any(
            |c| matches!(c, choreo_mcp::McpContent::Text { text } if text.contains("sse: ok"))
        ),
        "the SSE response should be parsed, got: {:?}",
        result.content
    );
}

#[test]
#[ignore = "integration: binds a local HTTP socket per workspace test discipline"]
fn http_auto_falls_back_to_legacy() {
    watchdog();
    // `Auto` probes `server/discover`; the legacy fixture rejects it with a
    // correlated method-not-found error (not a modern rejection), so the client
    // falls back to `initialize` over the same Streamable HTTP endpoint.
    let fixture = start_fixture(Scenario::Legacy).expect("start fixture");
    let server = McpServer::connect(&http_config(&fixture.url, McpProtocolMode::Auto))
        .expect("auto fallback to legacy over HTTP");
    let tools = server.handle().list_tools().expect("list tools");
    assert!(tools.iter().any(|t| t.name == "echo"));
}

#[test]
#[ignore = "integration: binds a local HTTP socket per workspace test discipline"]
fn http_retries_a_retryable_connect() {
    watchdog();
    // The first discover probe is a 503; the client must retry and succeed.
    let fixture = start_fixture(Scenario::Retry).expect("start fixture");
    let server = McpServer::connect(&http_config(&fixture.url, McpProtocolMode::Auto))
        .expect("connect should retry past the 503");
    let tools = server.handle().list_tools().expect("list tools");
    assert!(tools.iter().any(|t| t.name == "echo"));
    assert!(
        fixture.discover_attempts.load(Ordering::SeqCst) >= 2,
        "the client should have retried the discover probe"
    );
}

#[test]
#[ignore = "integration: binds a local HTTP socket per workspace test discipline"]
fn http_rejects_the_deprecated_http_sse_transport() {
    watchdog();
    let fixture = start_fixture(Scenario::LegacySse).expect("start fixture");
    let err = McpServer::connect(&http_config(&fixture.url, McpProtocolMode::Auto))
        .err()
        .expect("the removed HTTP+SSE transport must be rejected");
    assert!(
        matches!(err, McpError::UnsupportedTransport(_)),
        "expected UnsupportedTransport, got: {err:?}"
    );
}
