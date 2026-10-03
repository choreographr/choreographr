// Only compiled with the `mcp` feature: the whole test exercises the real
// MCP stack (McpManager + choreo-mcp), which does not exist in a plain build.
#![cfg(feature = "mcp")]

//! Integration test for MCP server spawning, tool discovery, and tool
//! execution through the full Choreographr stack (`McpManager` + `ToolRegistry`).
//!
//! Drives the in-tree scripted fixture server (`choreo-mcp/tests/fixtures/`,
//! shared into this crate's own `mcp-daemon-fixture-server` bin) instead of the
//! former `npx`-based official server, so the suite needs neither Node.js nor
//! the network.
//!
//! Marked `#[ignore = "integration"]` per AGENTS.md — integration tests belong
//! in crate-level `tests/` directories and must be ignored; `cargo test` runs
//! only unit tests.

use choreo_daemon::tools::ToolOutputFormat;
use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

/// Absolute path to the fixture server binary, provided by Cargo to this
/// package's integration tests.
const FIXTURE_BIN: &str = env!("CARGO_BIN_EXE_mcp-daemon-fixture-server");

#[test]
#[ignore = "integration"]
fn mcp_fixture_tools_are_discovered_and_callable() {
    // The stdlib test harness has no per-test timeout, so a regression in the
    // MCP stack (e.g. a shutdown that blocks) would hang CI forever. Install a
    // watchdog that aborts the process if the test body outlives its budget;
    // the internal protocol timeouts bound a healthy run to well under this.
    std::thread::spawn(|| {
        std::thread::sleep(Duration::from_secs(120));
        eprintln!("mcp_integration: test exceeded 120s; aborting to avoid an indefinite hang");
        std::process::abort();
    });

    // ── 1. Create a temporary config directory with mcp_servers.json ──
    let config_dir = tempfile::tempdir().expect("tempdir for config");
    let config_path = config_dir.path().join("choreographr");
    std::fs::create_dir_all(&config_path).expect("create Choreographr config dir");

    let mcp_config = serde_json::json!({
        "mcpServers": {
            "fixture": {
                "command": FIXTURE_BIN,
                "enabled": true,
                "timeout": 10
            }
        }
    });

    std::fs::write(
        config_path.join("mcp_servers.json"),
        serde_json::to_string_pretty(&mcp_config).expect("serialize mcp config"),
    )
    .expect("write mcp_servers.json");

    // ── 2. Override the config dir so load_mcp_config finds our file ──
    // XDG_CONFIG_HOME cannot be used for this: `dirs::config_dir()` ignores it
    // on macOS (it always returns $HOME/Library/Application Support). The
    // daemon exposes a test-only config-root hook instead.
    choreo_daemon::mcp::config::set_test_config_root(Some(config_dir.path().to_path_buf()));

    // ── 3. Build registry and spawn MCP servers via McpManager ──
    let mut registry = choreo_daemon::tools::ToolRegistry::new();
    let mcp_manager = choreo_daemon::mcp::McpManager::from_config(&mut registry);
    let registry = Arc::new(registry);

    // ── 4. Verify the dynamic group was registered ──
    let group_names = registry.group_names();
    assert!(
        group_names.iter().any(|g| g == "mcp/fixture"),
        "expected 'mcp/fixture' group, got: {group_names:?}"
    );

    // ── 5. Verify tool definitions are available ──
    let mut active = HashSet::new();
    active.insert("mcp/fixture".to_string());
    active.insert("core".to_string());
    let defs = registry.available_definitions(&active);
    let echo_name = "mcp/fixture/echo";
    assert!(
        defs.iter().any(|d| d.function.name == echo_name),
        "expected tool '{echo_name}', got: {:?}",
        defs.iter().map(|d| &d.function.name).collect::<Vec<_>>()
    );

    // ── 6. Call the echo tool through the registry ──
    let echo_call = choreo_ai_protocols::ChatToolCall {
        id: "call_1".to_string(),
        name: echo_name.to_string(),
        arguments_json: r#"{"message": "hello from choreo"}"#.to_string(),
        caller: None,
    };
    let output = registry
        .execute_json(&echo_call, ToolOutputFormat::Text, None, None, None, None)
        .expect("tool execution should succeed");
    assert!(!output.is_error, "echo should succeed: {}", output.content);
    assert!(
        output.content.contains("echo: hello from choreo"),
        "echo should return our message, got: {}",
        output.content
    );

    // ── 7. A server-flagged error surfaces as `is_error` ──
    let boom_call = choreo_ai_protocols::ChatToolCall {
        id: "call_2".to_string(),
        name: "mcp/fixture/boom".to_string(),
        arguments_json: "{}".to_string(),
        caller: None,
    };
    let boom = registry
        .execute_json(&boom_call, ToolOutputFormat::Text, None, None, None, None)
        .expect("boom execution should return an output");
    assert!(boom.is_error, "boom should be flagged as an error");

    // ── 8. Image content is attached via the image sink ──
    let image_call = choreo_ai_protocols::ChatToolCall {
        id: "call_3".to_string(),
        name: "mcp/fixture/image".to_string(),
        arguments_json: "{}".to_string(),
        caller: None,
    };
    let (image_tx, image_rx) = std::sync::mpsc::channel();
    let image_output = registry
        .execute_json(
            &image_call,
            ToolOutputFormat::Text,
            None,
            None,
            None,
            Some(image_tx),
        )
        .expect("image execution should succeed");
    let image = image_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("image should be attached via the sink");
    assert_eq!(image.mime_type(), "image/png");
    assert_eq!(image.dimensions(), (1, 1));
    assert!(
        !image_output.content.contains("[Image:"),
        "an attached image must not also appear as a text placeholder: {}",
        image_output.content
    );

    // ── 9. Shut down ──
    drop(mcp_manager);

    // ── 10. Restore the config-root override ──
    choreo_daemon::mcp::config::set_test_config_root(None);
}

/// Write an `mcp_servers.json` for a single server into a fresh config dir and
/// return the tempdir (kept alive by the caller) plus the server slug.
fn write_single_server_config(
    slug: &str,
    scenario: &str,
) -> Result<(tempfile::TempDir, String), Box<dyn std::error::Error>> {
    let config_dir = tempfile::tempdir()?;
    let config_path = config_dir.path().join("choreographr");
    std::fs::create_dir_all(&config_path)?;
    let server = serde_json::json!({
        "command": FIXTURE_BIN,
        "args": [scenario],
        "protocol": "modern",
        "enabled": true,
        "timeout": 10
    });
    let mut servers = serde_json::Map::new();
    servers.insert(slug.to_string(), server);
    let mcp_config = serde_json::json!({ "mcpServers": servers });
    std::fs::write(
        config_path.join("mcp_servers.json"),
        serde_json::to_string_pretty(&mcp_config)?,
    )?;
    Ok((config_dir, slug.to_string()))
}

#[test]
#[ignore = "integration"]
fn mcp_resource_tools_are_registered_and_callable() {
    std::thread::spawn(|| {
        std::thread::sleep(Duration::from_secs(120));
        eprintln!("mcp_integration: test exceeded 120s; aborting");
        std::process::abort();
    });

    let (config_dir, slug) =
        write_single_server_config("res", "modern-resources").expect("write config");
    choreo_daemon::mcp::config::set_test_config_root(Some(config_dir.path().to_path_buf()));

    let mut registry = choreo_daemon::tools::ToolRegistry::new();
    let manager = choreo_daemon::mcp::McpManager::from_config(&mut registry);
    let registry = Arc::new(registry);

    let mut active = HashSet::new();
    active.insert(format!("mcp/{slug}"));
    active.insert("core".to_string());
    let defs = registry.available_definitions(&active);
    for expected in ["list_resources", "read_resource"] {
        let name = format!("mcp/{slug}/{expected}");
        assert!(
            defs.iter().any(|d| d.function.name == name),
            "expected resource tool '{name}', got: {:?}",
            defs.iter().map(|d| &d.function.name).collect::<Vec<_>>()
        );
    }

    let list_call = choreo_ai_protocols::ChatToolCall {
        id: "call_r1".to_string(),
        name: format!("mcp/{slug}/list_resources"),
        arguments_json: "{}".to_string(),
        caller: None,
    };
    let listed = registry
        .execute_json(&list_call, ToolOutputFormat::Text, None, None, None, None)
        .expect("list_resources executes");
    assert!(
        listed.content.contains("readme"),
        "listing should name resources, got: {}",
        listed.content
    );

    let read_call = choreo_ai_protocols::ChatToolCall {
        id: "call_r2".to_string(),
        name: format!("mcp/{slug}/read_resource"),
        arguments_json: r#"{"uri": "file:///readme.txt"}"#.to_string(),
        caller: None,
    };
    let read = registry
        .execute_json(&read_call, ToolOutputFormat::Text, None, None, None, None)
        .expect("read_resource executes");
    assert!(
        read.content.contains("hello from a resource"),
        "read should return the resource text, got: {}",
        read.content
    );

    drop(manager);
    choreo_daemon::mcp::config::set_test_config_root(None);
}

#[test]
#[ignore = "integration"]
fn mcp_list_change_is_forwarded_and_reregisters() {
    std::thread::spawn(|| {
        std::thread::sleep(Duration::from_secs(120));
        eprintln!("mcp_integration: test exceeded 120s; aborting");
        std::process::abort();
    });

    // The fixture declares `tools.listChanged` and emits one tools list-changed
    // notification right after acknowledging the subscription, so the manager's
    // shared channel receives exactly one event.
    let (config_dir, slug) =
        write_single_server_config("lc", "modern-list-changed").expect("write config");
    choreo_daemon::mcp::config::set_test_config_root(Some(config_dir.path().to_path_buf()));

    let mut registry = choreo_daemon::tools::ToolRegistry::new();
    let mut manager = choreo_daemon::mcp::McpManager::from_config(&mut registry);
    let rx = manager
        .take_list_change_rx()
        .expect("a list-change channel is created for connected servers");

    let change = rx
        .recv_timeout(Duration::from_secs(10))
        .expect("expected a forwarded list change");
    assert_eq!(change.slug, slug);
    assert_eq!(change.kind, choreo_mcp::McpListKind::Tools);

    // Rebuild into a fresh registry the way the daemon's list-change handler
    // does, and confirm the server's catalogue is re-registered.
    let mut rebuilt = choreo_daemon::tools::ToolRegistry::new();
    manager.register_all(&mut rebuilt);
    assert!(
        rebuilt
            .group_names()
            .iter()
            .any(|g| g == &format!("mcp/{slug}")),
        "the refreshed registry must re-register the server's group: {:?}",
        rebuilt.group_names()
    );

    drop(manager);
    choreo_daemon::mcp::config::set_test_config_root(None);
}

#[test]
#[ignore = "integration"]
fn mcp_shutdown_all_is_bounded_with_a_stubborn_server() {
    std::thread::spawn(|| {
        std::thread::sleep(Duration::from_secs(120));
        eprintln!("mcp_integration: test exceeded 120s; aborting");
        std::process::abort();
    });

    // A server that ignores stdin EOF and never exits must not be able to wedge
    // `McpManager::shutdown_all`: each slot's `Drop` joins its dispatcher with a
    // bounded wait. `protocol` is left at the default (`auto`) so the legacy
    // `stubborn` fixture is reachable via the discover→initialize fallback.
    let config_dir = tempfile::tempdir().expect("tempdir for config");
    let config_path = config_dir.path().join("choreographr");
    std::fs::create_dir_all(&config_path).expect("create config dir");
    let mcp_config = serde_json::json!({
        "mcpServers": {
            "stubborn": {
                "command": FIXTURE_BIN,
                "args": ["stubborn"],
                "enabled": true,
                "timeout": 60
            }
        }
    });
    std::fs::write(
        config_path.join("mcp_servers.json"),
        serde_json::to_string_pretty(&mcp_config).expect("serialize config"),
    )
    .expect("write mcp_servers.json");
    choreo_daemon::mcp::config::set_test_config_root(Some(config_dir.path().to_path_buf()));

    let mut registry = choreo_daemon::tools::ToolRegistry::new();
    let manager = choreo_daemon::mcp::McpManager::from_config(&mut registry);
    assert_eq!(
        manager.server_count(),
        1,
        "the stubborn server should connect"
    );

    let start = std::time::Instant::now();
    drop(manager);
    let elapsed = start.elapsed();
    assert!(
        elapsed < Duration::from_secs(15),
        "shutdown_all exceeded its bounded wait: {elapsed:?}"
    );

    choreo_daemon::mcp::config::set_test_config_root(None);
}

#[test]
#[ignore = "integration"]
fn mcp_progress_streams_through_the_wrapper() {
    std::thread::spawn(|| {
        std::thread::sleep(Duration::from_secs(120));
        eprintln!("mcp_integration: test exceeded 120s; aborting");
        std::process::abort();
    });

    let (config_dir, slug) =
        write_single_server_config("prog", "modern-progress").expect("write config");
    choreo_daemon::mcp::config::set_test_config_root(Some(config_dir.path().to_path_buf()));

    let mut registry = choreo_daemon::tools::ToolRegistry::new();
    let manager = choreo_daemon::mcp::McpManager::from_config(&mut registry);
    let registry = Arc::new(registry);

    let call = choreo_ai_protocols::ChatToolCall {
        id: "call_p1".to_string(),
        name: format!("mcp/{slug}/progress"),
        arguments_json: "{}".to_string(),
        caller: None,
    };
    let (tx, rx) = crossbeam_channel::unbounded();
    let output = registry
        .execute_streaming_json(&call, ToolOutputFormat::Text, tx, None, None, None, None)
        .expect("progress tool executes");

    let chunks: Vec<String> = rx
        .try_iter()
        .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
        .collect();
    assert!(
        chunks.iter().any(|c| c.contains("working")),
        "expected a streamed progress chunk, got: {chunks:?}"
    );
    assert!(
        output.content.contains("progress done"),
        "final output should carry the result, got: {}",
        output.content
    );

    drop(manager);
    choreo_daemon::mcp::config::set_test_config_root(None);
}
