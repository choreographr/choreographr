/// Integration test for the choreo-mcp crate.
///
/// Spawns the official `@modelcontextprotocol/server-everything` reference
/// server via `npx`, lists its tools, calls the echo tool, and verifies the
/// response.  This test requires Node.js and `npx` to be available on the
/// system.
///
/// It is marked #[ignore] per AGENTS.md: integration tests (which bind
/// network sockets, spawn external processes, or exercise the full handler
/// pipeline) belong in crate-level tests/ directories and must be ignored so
/// that `cargo test` runs only unit tests.
#[test]
#[ignore]
fn mcp_server_everything_can_be_spawned_and_tools_listed() {
    let config = choreo_mcp::McpServerConfig {
        slug: "everything".to_string(),
        command: "npx".to_string(),
        args: vec![
            "-y".to_string(),
            "@modelcontextprotocol/server-everything".to_string(),
        ],
        env: std::collections::HashMap::new(),
        enabled: true,
        auto_load: true,
    };

    let mut client = choreo_mcp::McpClient::spawn(&config).expect("spawn MCP server");

    // List available tools and verify the echo tool is present.
    let tools = client.list_tools().expect("list tools");
    assert!(!tools.is_empty(), "should discover at least one tool");
    assert!(
        tools.iter().any(|t| t.name == "echo"),
        "expected 'echo' tool, got: {:?}",
        tools.iter().map(|t| &t.name).collect::<Vec<_>>()
    );

    // Call the echo tool with a test message.
    let result = client
        .call_tool(
            "echo",
            Some(serde_json::json!({"message": "hello from choreo"})),
            None,
        )
        .expect("call echo tool");

    assert!(!result.is_error, "echo should succeed");
    assert!(
        result.content.iter().any(|c| {
            matches!(
                c,
                choreo_mcp::McpContent::Text { text } if text.contains("hello from choreo")
            )
        }),
        "echo should return our message, got: {:?}",
        result.content
    );

    // Clean shutdown.
    client.shutdown();
}
