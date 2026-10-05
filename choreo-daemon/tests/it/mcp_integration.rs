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

    // ── 1. Create a temporary config directory with mcp.json ──
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
        config_path.join("mcp.json"),
        serde_json::to_string_pretty(&mcp_config).expect("serialize mcp config"),
    )
    .expect("write mcp.json");

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
    let (image_tx, image_rx) = crossbeam_channel::unbounded();
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

/// Write an `mcp.json` for a single server into a fresh config dir and
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
        config_path.join("mcp.json"),
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
fn mcp_startup_budget_bounds_slow_tool_listing() {
    std::thread::spawn(|| {
        std::thread::sleep(Duration::from_secs(120));
        eprintln!("mcp_integration: test exceeded 120s; aborting");
        std::process::abort();
    });

    // The `slow-list` fixture handshakes promptly but never answers
    // `tools/list`. The server's own request timeout is 60 s, so a caller that
    // listed on its own thread (the pre-fix behaviour) would block for that
    // long; the startup budget must bound connect AND discovery together.
    let config_dir = tempfile::tempdir().expect("tempdir for config");
    let config_path = config_dir.path().join("choreographr");
    std::fs::create_dir_all(&config_path).expect("create config dir");
    let mcp_config = serde_json::json!({
        "mcpServers": {
            "wedged": {
                "command": FIXTURE_BIN,
                "args": ["slow-list"],
                "enabled": true,
                "timeout": 60
            }
        }
    });
    std::fs::write(
        config_path.join("mcp.json"),
        serde_json::to_string_pretty(&mcp_config).expect("serialize config"),
    )
    .expect("write mcp.json");
    choreo_daemon::mcp::config::set_test_config_root(Some(config_dir.path().to_path_buf()));

    let mut registry = choreo_daemon::tools::ToolRegistry::new();
    let start = std::time::Instant::now();
    let manager = choreo_daemon::mcp::McpManager::from_config(&mut registry);
    let elapsed = start.elapsed();

    // The startup budget is 2 s; the request timeout is 60 s. Bound well below
    // the latter so the assertion proves the budget did the work.
    assert!(
        elapsed < Duration::from_secs(6),
        "from_config must bound slow discovery by the startup budget, took {elapsed:?}"
    );
    assert_eq!(
        manager.server_count(),
        0,
        "a server that never answers tools/list must be skipped, not registered"
    );

    drop(manager);
    choreo_daemon::mcp::config::set_test_config_root(None);
}

#[test]
#[ignore = "integration"]
fn mcp_catalogue_refresh_is_bounded_by_the_refresh_deadline() {
    std::thread::spawn(|| {
        std::thread::sleep(Duration::from_secs(120));
        eprintln!("mcp_integration: test exceeded 120s; aborting");
        std::process::abort();
    });

    // The fixture answers the FIRST `tools/list` (so the server connects and
    // registers normally) then parks on every later one. A catalogue refresh
    // re-lists it on the command loop, so the short catalogue-refresh deadline
    // must bound that re-listing rather than the server's 60 s request timeout.
    let (config_dir, slug) = write_single_server_config("refresh", "modern-slow-list-after-first")
        .expect("write config");
    choreo_daemon::mcp::config::set_test_config_root(Some(config_dir.path().to_path_buf()));

    let mut registry = choreo_daemon::tools::ToolRegistry::new();
    let mut manager = choreo_daemon::mcp::McpManager::from_config(&mut registry);
    assert_eq!(
        manager.server_count(),
        1,
        "the server connects and lists once at startup"
    );

    let mut rebuilt = choreo_daemon::tools::ToolRegistry::new();
    let start = std::time::Instant::now();
    manager.register_all(&mut rebuilt);
    let elapsed = start.elapsed();

    // The refresh deadline is 3 s; the request timeout here is 10 s. Bound well
    // below the latter so the assertion proves the deadline did the work.
    assert!(
        elapsed < Duration::from_secs(6),
        "register_all must bound a slow listing by the refresh deadline, took {elapsed:?}"
    );
    assert!(
        rebuilt
            .group_names()
            .iter()
            .any(|g| g == &format!("mcp/{slug}")),
        "a server that misses the refresh deadline keeps its previous tools: {:?}",
        rebuilt.group_names()
    );

    drop(manager);
    choreo_daemon::mcp::config::set_test_config_root(None);
}

#[test]
#[ignore = "integration"]
fn project_server_slug_maps_to_its_referencing_sessions() {
    std::thread::spawn(|| {
        std::thread::sleep(Duration::from_secs(120));
        eprintln!("mcp_integration: test exceeded 120s; aborting");
        std::process::abort();
    });

    // A project whose `.mcp.json` declares the fixture server, so the server is
    // a session's PRIVATE project server (never in the daemon catalogue).
    let project = tempfile::tempdir().expect("project dir");
    let root = project.path();
    let server = serde_json::json!({
        "command": FIXTURE_BIN,
        "args": ["modern"],
        "protocol": "modern",
        "enabled": true,
        "timeout": 10
    });
    std::fs::write(
        root.join(".mcp.json"),
        serde_json::to_string(&serde_json::json!({ "mcpServers": { "proj": server } }))
            .expect("serialize .mcp.json"),
    )
    .expect("write .mcp.json");

    let mut manager = choreo_daemon::mcp::McpManager::empty();
    assert!(
        manager.sessions_for_slug("proj").is_empty(),
        "no session references the project server yet"
    );

    // A session that resolves its overlay references the project server.
    let _overlay = manager.ensure_session(7, Some(root), true);
    assert!(
        manager.sessions_for_slug("proj").contains(&7),
        "a session holding the project server must be reported for its slug"
    );

    // Releasing the session drops it from the slug's referencing set (so a
    // later list change no longer refreshes it).
    manager.release_session(7);
    assert!(
        !manager.sessions_for_slug("proj").contains(&7),
        "releasing the session must drop it from the slug's referencing set"
    );

    drop(manager);
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
        config_path.join("mcp.json"),
        serde_json::to_string_pretty(&mcp_config).expect("serialize config"),
    )
    .expect("write mcp.json");
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

/// Install the standard watchdog so a regression that hangs the MCP stack
/// aborts rather than wedging CI forever (the stdlib harness has no per-test
/// timeout).
fn watchdog() {
    std::thread::spawn(|| {
        std::thread::sleep(Duration::from_secs(120));
        eprintln!("mcp_integration: test exceeded 120s; aborting to avoid an indefinite hang");
        std::process::abort();
    });
}

/// Write an `mcp.json` whose `mcpServers` map is `servers`, into a fresh
/// config dir; returns the tempdir (kept alive by the caller).
fn write_daemon_config(
    servers: &serde_json::Value,
) -> Result<tempfile::TempDir, Box<dyn std::error::Error>> {
    let config_dir = tempfile::tempdir()?;
    let config_path = config_dir.path().join("choreographr");
    std::fs::create_dir_all(&config_path)?;
    std::fs::write(
        config_path.join("mcp.json"),
        serde_json::to_string_pretty(&serde_json::json!({ "mcpServers": servers }))?,
    )?;
    Ok(config_dir)
}

/// A modern fixture server entry, with optional extra JSON keys merged in.
fn fixture_entry(scenario: &str) -> serde_json::Value {
    serde_json::json!({
        "command": FIXTURE_BIN,
        "args": [scenario],
        "protocol": "modern",
        "enabled": true,
        "timeout": 10
    })
}

/// A same-root re-resolve must REUSE the pooled project connection rather than
/// reconnect it. The fixture appends its pid to `MCP_FIXTURE_CONNECT_LOG` on
/// every startup, so counting the log lines proves how many connections were
/// actually opened — no time-based wait needed.
#[test]
#[ignore = "integration"]
fn same_root_re_resolve_reuses_connections() {
    watchdog();

    let project = tempfile::tempdir().expect("project dir");
    let root = project.path();
    let log_path = root.join("connects.log");
    let mut server = fixture_entry("modern");
    server["env"] = serde_json::json!({ "MCP_FIXTURE_CONNECT_LOG": log_path });
    std::fs::write(
        root.join(".mcp.json"),
        serde_json::to_string(&serde_json::json!({ "mcpServers": { "proj": server } }))
            .expect("serialize .mcp.json"),
    )
    .expect("write .mcp.json");

    let mut manager = choreo_daemon::mcp::McpManager::empty();

    let first = manager.ensure_session(7, Some(root), true);
    assert!(
        first.statuses.iter().any(|s| s.slug == "proj"),
        "the project server must connect on the first resolve: {:?}",
        first.statuses
    );
    // A second resolve for the SAME root re-ensures and reuses the connection.
    let second = manager.ensure_session(7, Some(root), true);
    assert!(
        second.statuses.iter().any(|s| s.slug == "proj"),
        "the reused server must still be present: {:?}",
        second.statuses
    );

    let connections = std::fs::read_to_string(&log_path)
        .expect("connect log")
        .lines()
        .count();
    assert_eq!(
        connections, 1,
        "a same-root re-resolve must reuse the pooled connection, not reconnect (started {connections} servers)"
    );

    drop(manager);
}

/// An UNTRUSTED project `.mcp.json` is merely reported; its slugs must NOT
/// suppress a daemon-tier `shared = false` server of the same name.
#[test]
#[ignore = "integration"]
fn untrusted_project_does_not_suppress_daemon_per_session_server() {
    watchdog();

    let mut daemon_server = fixture_entry("modern");
    daemon_server["shared"] = serde_json::json!(false);
    let config_dir = write_daemon_config(&serde_json::json!({ "stateful": daemon_server }))
        .expect("write config");
    choreo_daemon::mcp::config::set_test_config_root(Some(config_dir.path().to_path_buf()));

    // A project declaring the SAME slug, left untrusted.
    let project = tempfile::tempdir().expect("project dir");
    std::fs::write(
        project.path().join(".mcp.json"),
        serde_json::to_string(
            &serde_json::json!({ "mcpServers": { "stateful": fixture_entry("modern") } }),
        )
        .expect("serialize"),
    )
    .expect("write .mcp.json");

    let mut registry = choreo_daemon::tools::ToolRegistry::new();
    let mut manager = choreo_daemon::mcp::McpManager::from_config(&mut registry);
    let overlay = manager.ensure_session(7, Some(project.path()), false);

    assert!(
        overlay
            .ignored_project_servers
            .contains(&"stateful".to_string()),
        "the untrusted project's slug must be reported as ignored: {:?}",
        overlay.ignored_project_servers
    );
    assert!(
        overlay
            .statuses
            .iter()
            .any(|s| s.slug == "stateful" && s.tier == "daemon"),
        "the daemon per-session server must still connect: {:?}",
        overlay.statuses
    );

    drop(manager);
    choreo_daemon::mcp::config::set_test_config_root(None);
}

/// `/mcp reconnect <slug>` must rebuild a per-session (`shared = false`)
/// daemon-tier server's connection, not report it unknown.
#[test]
#[ignore = "integration"]
fn reconnect_rebuilds_a_daemon_per_session_slot() {
    watchdog();

    let mut server = fixture_entry("modern");
    server["shared"] = serde_json::json!(false);
    let config_dir =
        write_daemon_config(&serde_json::json!({ "stateful": server })).expect("write config");
    choreo_daemon::mcp::config::set_test_config_root(Some(config_dir.path().to_path_buf()));

    let mut registry = choreo_daemon::tools::ToolRegistry::new();
    let mut manager = choreo_daemon::mcp::McpManager::from_config(&mut registry);
    let overlay = manager.ensure_session(7, None, false);
    assert!(
        overlay.statuses.iter().any(|s| s.slug == "stateful"),
        "the per-session server must connect first: {:?}",
        overlay.statuses
    );

    manager
        .reconnect("stateful")
        .expect("reconnect must rebuild a per-session slot");
    assert!(
        manager
            .session_status(7)
            .iter()
            .any(|s| s.slug == "stateful"),
        "the reconnected per-session server must still be present"
    );

    drop(manager);
    choreo_daemon::mcp::config::set_test_config_root(None);
}

/// `/mcp reload` must drop a daemon `shared = false` slot whose config changed,
/// so the next resolve reconnects with the new config, and report the affected
/// session.
#[test]
#[ignore = "integration"]
fn reload_drops_a_changed_daemon_per_session_slot() {
    watchdog();

    let mut server = fixture_entry("modern");
    server["shared"] = serde_json::json!(false);
    let config_dir =
        write_daemon_config(&serde_json::json!({ "stateful": server })).expect("write config");
    choreo_daemon::mcp::config::set_test_config_root(Some(config_dir.path().to_path_buf()));

    let mut registry = choreo_daemon::tools::ToolRegistry::new();
    let mut manager = choreo_daemon::mcp::McpManager::from_config(&mut registry);
    let overlay = manager.ensure_session(7, None, false);
    assert!(
        overlay.statuses.iter().any(|s| s.slug == "stateful"),
        "the per-session server must connect first: {:?}",
        overlay.statuses
    );

    // Rewrite the config with a changed value (the timeout).
    let mut changed = fixture_entry("modern");
    changed["shared"] = serde_json::json!(false);
    changed["timeout"] = serde_json::json!(20);
    std::fs::write(
        config_dir.path().join("choreographr").join("mcp.json"),
        serde_json::to_string_pretty(&serde_json::json!({ "mcpServers": { "stateful": changed } }))
            .expect("serialize"),
    )
    .expect("rewrite mcp.json");

    let outcome = manager.reload().expect("reload succeeds");
    assert!(
        outcome.summary.contains("1 restarted"),
        "a changed per-session server must count as restarted: {}",
        outcome.summary
    );
    assert!(
        outcome.affected_sessions.contains(&7),
        "the session that held the changed slot must be reported: {:?}",
        outcome.affected_sessions
    );
    assert!(
        manager.session_status(7).is_empty(),
        "the stale per-session slot must be dropped by reload"
    );

    drop(manager);
    choreo_daemon::mcp::config::set_test_config_root(None);
}

/// A project server that fails to connect must NOT shadow the daemon-tier
/// group of the same slug.
#[test]
#[ignore = "integration"]
fn failed_project_connect_does_not_shadow_daemon_group() {
    watchdog();

    // A daemon-tier shared server that connects, so `mcp/shared` exists.
    let config_dir = write_daemon_config(&serde_json::json!({ "shared": fixture_entry("modern") }))
        .expect("write config");
    choreo_daemon::mcp::config::set_test_config_root(Some(config_dir.path().to_path_buf()));

    // A project declaring the SAME slug with a command that cannot connect.
    let project = tempfile::tempdir().expect("project dir");
    std::fs::write(
        project.path().join(".mcp.json"),
        serde_json::to_string(&serde_json::json!({
            "mcpServers": { "shared": { "command": "/nonexistent-mcp-fixture-binary", "enabled": true, "timeout": 5 } }
        }))
        .expect("serialize"),
    )
    .expect("write .mcp.json");

    let mut registry = choreo_daemon::tools::ToolRegistry::new();
    let mut manager = choreo_daemon::mcp::McpManager::from_config(&mut registry);
    let overlay = manager.ensure_session(7, Some(project.path()), true);

    assert!(
        !overlay.shadowed_groups.contains("mcp/shared"),
        "a failed project connect must not shadow the daemon group: {:?}",
        overlay.shadowed_groups
    );

    drop(manager);
    choreo_daemon::mcp::config::set_test_config_root(None);
}
