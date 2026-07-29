use choreographr::tools::ToolOutputFormat;
/// Integration test for MCP server spawning, tool discovery, and tool
/// execution through the full Choreographr stack (McpManager + ToolRegistry).
///
/// Writes an `mcp_servers.json` pointing to the official
/// `@modelcontextprotocol/server-everything` reference server, sets up a
/// ToolRegistry with an McpManager, and verifies the whole pipeline:
///
/// 1. The dynamic group `mcp/everything` appears in `registry.group_names()`.
/// 2. Tool definitions from the server are available via
///    `registry.available_definitions()`.
/// 3. The `echo` tool can be called and returns the expected message.
/// 4. Dropping the McpManager shuts down the server cleanly.
///
/// Requires Node.js and `npx` to be available on the system.
///
/// Marked #[ignore] per AGENTS.md — integration tests belong in crate-level
/// tests/ directories and must be ignored; `cargo test` runs only unit tests.
use std::collections::HashSet;
use std::sync::Arc;

#[test]
#[ignore]
fn mcp_server_everything_tools_are_discovered_and_callable() {
    // ── 1. Create a temporary config directory with mcp_servers.json ──
    let config_dir = tempfile::tempdir().expect("tempdir for config");
    let config_path = config_dir.path().join("choreographr");
    std::fs::create_dir_all(&config_path).expect("create Choreographr config dir");

    let mcp_config = serde_json::json!({
        "mcpServers": {
            "everything": {
                "command": "npx",
                "args": ["-y", "@modelcontextprotocol/server-everything"],
                "enabled": true
            }
        }
    });

    std::fs::write(
        config_path.join("mcp_servers.json"),
        serde_json::to_string_pretty(&mcp_config).expect("serialize mcp config"),
    )
    .expect("write mcp_servers.json");

    // ── 2. Override XDG_CONFIG_HOME so load_mcp_config finds our file ──
    let prev_config_home = std::env::var("XDG_CONFIG_HOME").ok();
    // SAFETY: Single-threaded test; no other code reads XDG_CONFIG_HOME at
    // this point, so the unsafe env mutation is sound.
    unsafe { std::env::set_var("XDG_CONFIG_HOME", config_dir.path()) };

    // ── 3. Build registry and spawn MCP servers via McpManager ──
    let mut registry = choreographr::tools::ToolRegistry::new();
    let mcp_manager = choreographr::mcp::McpManager::from_config(&mut registry);
    let registry = Arc::new(registry);

    // ── 4. Verify the dynamic group was registered ──
    let group_names = registry.group_names();
    assert!(
        group_names.iter().any(|g| g == "mcp/everything"),
        "expected 'mcp/everything' group, got: {group_names:?}"
    );

    // ── 5. Verify tool definitions are available ──
    let mut active = HashSet::new();
    active.insert("mcp/everything".to_string());
    active.insert("core".to_string());
    let defs = registry.available_definitions(&active);
    let echo_name = "mcp/everything/echo";
    assert!(
        defs.iter().any(|d| d.function.name == echo_name),
        "expected tool '{echo_name}', got: {:?}",
        defs.iter().map(|d| &d.function.name).collect::<Vec<_>>()
    );

    // ── 6. Call the echo tool through the registry ──
    let tool_call = choreographr::providers::ChatToolCall {
        id: "call_1".to_string(),
        name: echo_name.to_string(),
        arguments_json: r#"{"message": "hello from choreo"}"#.to_string(),
        caller: None,
    };

    let output = registry.execute_json(
        &tool_call,
        ToolOutputFormat::Text,
        None, // x_credentials
        None, // working_dir
        None, // ctx
        None, // image_tx
    );

    let output = output.expect("tool execution should succeed");
    assert!(!output.is_error, "echo should succeed: {}", output.content);
    assert!(
        output.content.contains("hello from choreo"),
        "echo should return our message, got: {}",
        output.content
    );

    // ── 7. Shut down ──
    drop(mcp_manager);

    // ── 8. Restore XDG_CONFIG_HOME ──
    // SAFETY: Single-threaded test; restoring env var to previous state.
    unsafe {
        match prev_config_home {
            Some(v) => std::env::set_var("XDG_CONFIG_HOME", v),
            None => std::env::remove_var("XDG_CONFIG_HOME"),
        }
    }
}
