//! Integration tests for the `choreo-mcp` client, driven by the in-tree
//! scripted stdio fixture server (`tests/fixtures/fixture_server.rs`, spawned
//! via `CARGO_BIN_EXE_mcp-fixture-server`).
//!
//! These replace the former `npx`-based tests: no Node.js, no network. They
//! bind no sockets but do spawn an external process, so they belong here and
//! are marked `#[ignore]` per the workspace test discipline (`cargo test` runs
//! only unit tests; `cargo test-integration` runs these).

use std::collections::HashMap;
use std::time::Duration;

/// Absolute path to the fixture server binary, provided by Cargo to
/// integration tests of this package.
const FIXTURE_BIN: &str = env!("CARGO_BIN_EXE_mcp-fixture-server");

/// Build a client config that launches the fixture in `scenario` (or the
/// default behaviour when `None`).
fn fixture_config(scenario: Option<&str>) -> choreo_mcp::McpServerConfig {
    choreo_mcp::McpServerConfig {
        slug: "fixture".to_string(),
        command: FIXTURE_BIN.to_string(),
        args: scenario.map_or_else(Vec::new, |s| vec![s.to_string()]),
        env: HashMap::new(),
        enabled: true,
        timeout: Some(Duration::from_secs(10)),
    }
}

/// Watchdog: the stdlib test harness has no per-test timeout, so a regression
/// that wedges spawn/call/shutdown would hang CI. Abort if the body outlives
/// its budget; the client's configured timeouts bound a healthy run far lower.
fn watchdog() {
    std::thread::spawn(|| {
        std::thread::sleep(Duration::from_secs(120));
        eprintln!("mcp_integration: test exceeded 120s; aborting to avoid an indefinite hang");
        std::process::abort();
    });
}

#[test]
#[ignore = "integration: spawns the fixture subprocess per workspace test discipline"]
fn fixture_lists_tools_and_calls_echo() {
    watchdog();
    let mut client = choreo_mcp::McpClient::spawn(&fixture_config(None)).expect("spawn fixture");

    let tools = client.list_tools().expect("list tools");
    assert!(
        tools.iter().any(|t| t.name == "echo"),
        "expected 'echo', got: {:?}",
        tools.iter().map(|t| &t.name).collect::<Vec<_>>()
    );
    // A tool whose schema is a non-object must be dropped, not forwarded.
    assert!(
        !tools.iter().any(|t| t.name == "bad"),
        "tool with an invalid schema should be dropped"
    );

    let result = client
        .call_tool("echo", Some(serde_json::json!({"message": "hi"})), None)
        .expect("call echo");
    assert!(!result.is_error);
    assert!(
        result.content.iter().any(|c| matches!(
            c,
            choreo_mcp::McpContent::Text { text } if text.contains("echo: hi")
        )),
        "echo should return our message, got: {:?}",
        result.content
    );
    client.shutdown();
}

#[test]
#[ignore = "integration: spawns the fixture subprocess per workspace test discipline"]
fn fixture_reports_tool_error_as_is_error() {
    watchdog();
    let mut client = choreo_mcp::McpClient::spawn(&fixture_config(None)).expect("spawn fixture");
    let result = client.call_tool("boom", None, None).expect("call boom");
    assert!(result.is_error, "server-flagged error must surface");
    client.shutdown();
}

#[test]
#[ignore = "integration: spawns the fixture subprocess per workspace test discipline"]
fn fixture_returns_image_content() {
    watchdog();
    let mut client = choreo_mcp::McpClient::spawn(&fixture_config(None)).expect("spawn fixture");
    let result = client.call_tool("image", None, None).expect("call image");
    assert!(
        result
            .content
            .iter()
            .any(|c| matches!(c, choreo_mcp::McpContent::Image { .. })),
        "expected an image content block, got: {:?}",
        result.content
    );
    client.shutdown();
}

#[test]
#[ignore = "integration: spawns the fixture subprocess per workspace test discipline"]
fn fixture_oversized_line_is_rejected() {
    watchdog();
    let mut client = choreo_mcp::McpClient::spawn(&fixture_config(None)).expect("spawn fixture");
    let err = client
        .call_tool("big", None, None)
        .expect_err("oversized line must be rejected");
    assert!(
        matches!(err, choreo_mcp::McpError::LineTooLong { .. }),
        "expected LineTooLong, got: {err:?}"
    );
    client.shutdown();
}

#[test]
#[ignore = "integration: spawns the fixture subprocess per workspace test discipline"]
fn fixture_rejected_initialize_fails_spawn() {
    watchdog();
    let Err(err) = choreo_mcp::McpClient::spawn(&fixture_config(Some("no-init"))) else {
        panic!("initialize error must fail spawn");
    };
    assert!(
        matches!(err, choreo_mcp::McpError::InitializeFailed(_)),
        "expected InitializeFailed, got: {err:?}"
    );
}

#[test]
#[ignore = "integration: spawns the fixture subprocess per workspace test discipline"]
fn fixture_crash_on_call_is_reported() {
    watchdog();
    let mut client = choreo_mcp::McpClient::spawn(&fixture_config(Some("crash-on-call")))
        .expect("spawn fixture");
    let err = client
        .call_tool("echo", Some(serde_json::json!({"message": "x"})), None)
        .expect_err("a crashed server must not return a result");
    assert!(
        matches!(err, choreo_mcp::McpError::ServerShutdown),
        "expected ServerShutdown, got: {err:?}"
    );
}
