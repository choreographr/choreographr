//! Integration tests for the `choreo-mcp` client, driven by the in-tree
//! scripted stdio fixture server (`tests/fixtures/fixture_server.rs`, spawned
//! via `CARGO_BIN_EXE_mcp-fixture-server`).
//!
//! They exercise both protocol eras (the stateless `server/discover` probe and
//! the legacy `initialize` handshake, plus the `Auto` fallback between them),
//! the failure modes of a misbehaving server, and cancellation.
//!
//! These bind no sockets but do spawn an external process, so they belong here
//! and are marked `#[ignore]` per the workspace test discipline (`cargo test`
//! runs only unit tests; `cargo test-integration` runs these).

use crate::common::watchdog;
use choreo_mcp::{McpError, McpProtocolMode, McpServer, McpServerConfig, McpTransport};
use std::collections::HashMap;
use std::time::Duration;

/// Absolute path to the fixture server binary, provided by Cargo to
/// integration tests of this package.
const FIXTURE_BIN: &str = env!("CARGO_BIN_EXE_mcp-fixture-server");

/// Build a client config that launches the fixture in `scenario` (or the
/// default `legacy` behaviour when `None`).
fn fixture_config(scenario: Option<&str>, protocol: McpProtocolMode) -> McpServerConfig {
    McpServerConfig {
        slug: "fixture".to_string(),
        transport: McpTransport::Stdio {
            command: FIXTURE_BIN.to_string(),
            args: scenario.map_or_else(Vec::new, |s| vec![s.to_string()]),
            env: HashMap::new(),
        },
        enabled: true,
        timeout: Some(Duration::from_secs(10)),
        protocol,
    }
}

#[test]
#[ignore = "integration: spawns the fixture subprocess per workspace test discipline"]
fn modern_server_lists_and_calls() {
    watchdog();
    let server = McpServer::connect(&fixture_config(Some("modern"), McpProtocolMode::Modern))
        .expect("connect modern fixture");
    let handle = server.handle();

    let tools = handle.list_tools().expect("list tools");
    assert!(
        tools.iter().any(|t| t.name == "echo"),
        "expected 'echo', got: {:?}",
        tools.iter().map(|t| &t.name).collect::<Vec<_>>()
    );
    // The structured tool advertises an output schema; the client captures it.
    let structured = tools
        .iter()
        .find(|t| t.name == "structured")
        .expect("structured tool present");
    assert!(
        structured.output_schema.is_some(),
        "outputSchema should be captured"
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
#[ignore = "integration: spawns the fixture subprocess per workspace test discipline"]
fn legacy_server_lists_and_calls() {
    watchdog();
    let server = McpServer::connect(&fixture_config(Some("legacy"), McpProtocolMode::Legacy))
        .expect("connect legacy fixture");
    let handle = server.handle();

    let tools = handle.list_tools().expect("list tools");
    assert!(tools.iter().any(|t| t.name == "echo"));

    let result = handle
        .call_tool(1, "boom", serde_json::json!({}), None)
        .expect("call boom");
    assert!(result.is_error, "server-flagged error must surface");
}

#[test]
#[ignore = "integration: spawns the fixture subprocess per workspace test discipline"]
fn auto_falls_back_to_legacy() {
    watchdog();
    // `Auto` probes `server/discover`; the legacy fixture rejects it with a
    // method-not-found error, which is not a modern-era rejection, so the
    // client falls back to `initialize`.
    let server = McpServer::connect(&fixture_config(Some("legacy"), McpProtocolMode::Auto))
        .expect("auto fallback to legacy");
    let tools = server.handle().list_tools().expect("list tools");
    assert!(tools.iter().any(|t| t.name == "echo"));
}

#[test]
#[ignore = "integration: spawns the fixture subprocess per workspace test discipline"]
fn modern_result_carries_structured_content() {
    watchdog();
    let server = McpServer::connect(&fixture_config(Some("modern"), McpProtocolMode::Auto))
        .expect("auto negotiates modern");
    let result = server
        .handle()
        .call_tool(1, "structured", serde_json::json!({}), None)
        .expect("call structured");
    assert_eq!(
        result.structured_content,
        Some(serde_json::json!({"value": 42})),
        "structuredContent should be preserved"
    );
}

#[test]
#[ignore = "integration: spawns the fixture subprocess per workspace test discipline"]
fn rejected_initialize_fails_connect() {
    watchdog();
    let err = McpServer::connect(&fixture_config(Some("no-init"), McpProtocolMode::Legacy))
        .err()
        .expect("initialize error must fail connect");
    assert!(
        matches!(err, McpError::InitializeFailed(_)),
        "expected InitializeFailed, got: {err:?}"
    );
}

#[test]
#[ignore = "integration: spawns the fixture subprocess per workspace test discipline"]
fn crash_on_call_is_reported() {
    watchdog();
    let server = McpServer::connect(&fixture_config(
        Some("crash-on-call"),
        McpProtocolMode::Legacy,
    ))
    .expect("connect fixture");
    let err = server
        .handle()
        .call_tool(1, "echo", serde_json::json!({"message": "x"}), None)
        .expect_err("a crashed server must not return a result");
    assert!(
        matches!(err, McpError::ServerShutdown | McpError::Io(_)),
        "expected a transport error, got: {err:?}"
    );
}

#[test]
#[ignore = "integration: spawns the fixture subprocess per workspace test discipline"]
fn garbage_line_is_ignored() {
    watchdog();
    let server = McpServer::connect(&fixture_config(Some("garbage"), McpProtocolMode::Legacy))
        .expect("connect fixture");
    // rmcp ignores an unparsable (non-JSON) line and keeps reading, matching the
    // other official SDKs; the real response must still arrive.
    let result = server
        .handle()
        .call_tool(1, "echo", serde_json::json!({"message": "x"}), None)
        .expect("garbage is ignored, the call still succeeds");
    assert!(!result.is_error);
}

#[test]
#[ignore = "integration: spawns the fixture subprocess per workspace test discipline"]
fn oversized_line_is_reported() {
    watchdog();
    let server = McpServer::connect(&fixture_config(Some("oversized"), McpProtocolMode::Legacy))
        .expect("connect fixture");
    let err = server
        .handle()
        .call_tool(1, "echo", serde_json::json!({"message": "x"}), None)
        .expect_err("an oversized frame must not return a result");
    assert!(
        matches!(err, McpError::ServerShutdown | McpError::Io(_)),
        "expected a transport error, got: {err:?}"
    );
}

#[test]
#[ignore = "integration: spawns the fixture subprocess per workspace test discipline"]
fn cancel_session_stops_inflight_call() {
    watchdog();
    let marker_dir = tempfile::tempdir().expect("tempdir for marker");
    let marker = marker_dir.path().join("started");

    let mut config = fixture_config(Some("legacy"), McpProtocolMode::Legacy);
    config.timeout = Some(Duration::from_secs(60));
    if let McpTransport::Stdio { env, .. } = &mut config.transport {
        env.insert(
            "MCP_FIXTURE_MARKER".to_string(),
            marker.display().to_string(),
        );
    }
    let server = McpServer::connect(&config).expect("connect fixture");
    let handle = server.handle();

    let caller = handle.clone();
    let call = std::thread::spawn(move || caller.call_tool(7, "slow", serde_json::json!({}), None));

    // Wait (bounded) until the fixture reports the call is in flight, then
    // cancel its session.
    let mut waited = 0;
    while !marker.exists() {
        std::thread::sleep(Duration::from_millis(5));
        waited += 1;
        assert!(waited < 2000, "fixture never started the slow call");
    }
    handle.cancel_session(7);

    let result = call.join().expect("call thread joins");
    assert!(
        matches!(result, Err(McpError::Cancelled)),
        "expected Cancelled, got: {result:?}"
    );
}

#[test]
#[ignore = "integration: spawns the fixture subprocess per workspace test discipline"]
fn progress_notifications_become_chunks() {
    watchdog();
    let server = McpServer::connect(&fixture_config(
        Some("modern-progress"),
        McpProtocolMode::Modern,
    ))
    .expect("connect fixture");
    let handle = server.handle();

    let (chunk_tx, chunk_rx) = crossbeam_channel::unbounded::<Vec<u8>>();
    let result = handle
        .call_tool_streaming(1, "progress", serde_json::json!({}), None, Some(chunk_tx))
        .expect("call progress");
    assert!(!result.is_error);

    // The fixture emits two messages back to back; the client rate-limits to at
    // most one per interval, so at least the first (`working`) arrives.
    let chunks: Vec<String> = chunk_rx
        .try_iter()
        .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
        .collect();
    assert!(
        chunks.iter().any(|c| c.contains("working")),
        "expected a progress chunk, got: {chunks:?}"
    );
}

#[test]
#[ignore = "integration: spawns the fixture subprocess per workspace test discipline"]
fn mrtr_decline_retries_and_completes() {
    watchdog();
    let server = McpServer::connect(&fixture_config(
        Some("modern-mrtr"),
        McpProtocolMode::Modern,
    ))
    .expect("connect fixture");
    let handle = server.handle();

    // The first call returns `input_required`; the client must decline and
    // retry with `inputResponses`, which the fixture answers `complete`.
    let result = handle
        .call_tool(1, "need_input", serde_json::json!({}), None)
        .expect("MRTR retry completes");
    assert!(!result.is_error);
    assert!(
        result.content.iter().any(
            |c| matches!(c, choreo_mcp::McpContent::Text { text } if text.contains("input accepted"))
        ),
        "expected the completed retry, got: {:?}",
        result.content
    );
}

#[test]
#[ignore = "integration: spawns the fixture subprocess per workspace test discipline"]
fn resources_are_listed_and_read() {
    watchdog();
    let server = McpServer::connect(&fixture_config(
        Some("modern-resources"),
        McpProtocolMode::Modern,
    ))
    .expect("connect fixture");
    let handle = server.handle();
    assert!(handle.supports_resources(), "resources capability declared");

    let resources = handle.list_resources().expect("list resources");
    assert_eq!(resources.len(), 2, "got: {resources:?}");
    assert!(resources.iter().any(|r| r.uri == "file:///readme.txt"));

    let contents = handle
        .read_resource("file:///readme.txt")
        .expect("read resource");
    assert!(
        contents.iter().any(
            |c| matches!(c, choreo_mcp::McpContent::Resource { text: Some(text), .. } if text.contains("hello from a resource"))
        ),
        "got: {contents:?}"
    );
}

#[test]
#[ignore = "integration: spawns the fixture subprocess per workspace test discipline"]
fn server_observes_cancellation() {
    watchdog();
    let marker_dir = tempfile::tempdir().expect("tempdir for marker");
    let start_marker = marker_dir.path().join("started");
    let cancel_marker = marker_dir.path().join("cancelled");

    let mut config = fixture_config(Some("legacy"), McpProtocolMode::Legacy);
    config.timeout = Some(Duration::from_secs(60));
    if let McpTransport::Stdio { env, .. } = &mut config.transport {
        env.insert(
            "MCP_FIXTURE_MARKER".to_string(),
            start_marker.display().to_string(),
        );
        env.insert(
            "MCP_FIXTURE_CANCEL_MARKER".to_string(),
            cancel_marker.display().to_string(),
        );
    }
    let server = McpServer::connect(&config).expect("connect fixture");
    let handle = server.handle();

    let caller = handle.clone();
    let call = std::thread::spawn(move || caller.call_tool(9, "slow", serde_json::json!({}), None));

    let mut waited = 0;
    while !start_marker.exists() {
        std::thread::sleep(Duration::from_millis(5));
        waited += 1;
        assert!(waited < 2000, "fixture never started the slow call");
    }
    handle.cancel_session(9);
    let result = call.join().expect("call thread joins");
    assert!(matches!(result, Err(McpError::Cancelled)), "got {result:?}");

    // The server must have observed a `notifications/cancelled` for the call.
    waited = 0;
    while !cancel_marker.exists() {
        std::thread::sleep(Duration::from_millis(5));
        waited += 1;
        assert!(waited < 2000, "fixture never observed the cancellation");
    }
}
