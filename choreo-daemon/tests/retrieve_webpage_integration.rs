// Integration test for the `retrieve_webpage` tool: renders a real page in a
// local headless Chromium/Chrome and returns its HTML.
//
// This lives in tests/ per AGENTS.md (system-boundary: it launches a browser
// process and hits the network) and is marked #[ignore] so plain `cargo test`
// runs only the unit tests. Run it with the integration alias
// (`cargo test-integration`) on a host that has Chromium/Chrome installed and
// network access. If no browser is found the test skips gracefully.
use choreo_ai_protocols::ChatToolCall;
use choreo_daemon::tools::{ToolOutputFormat, ToolRegistry};

#[test]
#[ignore]
fn retrieve_webpage_renders_a_real_page() {
    // No per-test timeout under the stdlib harness, so a hung browser (or a
    // networking stall) would block CI forever. Watchdog-abort if the body
    // outlives a generous budget; nextest's slow-timeout is belt-and-braces.
    std::thread::spawn(|| {
        std::thread::sleep(std::time::Duration::from_secs(90));
        eprintln!("retrieve_webpage_integration: exceeded 90s; aborting to avoid a hang");
        std::process::abort();
    });

    let registry = ToolRegistry::new().build();

    let tool_call = ChatToolCall {
        id: "call_1".to_string(),
        name: "retrieve_webpage".to_string(),
        // Fetch the HTML (default action is "content").
        arguments_json:
            r#"{"url": "https://example.com", "action": "content", "timeout_ms": 20000}"#
                .to_string(),
        caller: None,
    };

    let output = registry
        .execute_json(
            &tool_call,
            ToolOutputFormat::Text,
            None, // x_credentials
            None, // working_dir
            None, // ctx
            None, // image_tx
        )
        .expect("tool execution should return");

    // If no browser is installed on this host, the tool returns a clear error
    // instead of panicking — treat that as a skip so the ignored suite stays
    // green on browser-less machines.
    if output.is_error && output.content.contains("no chromium or chrome binary") {
        eprintln!("retrieve_webpage_integration: skipping (no chromium/chrome installed)");
        return;
    }

    assert!(
        !output.is_error,
        "retrieve_webpage should succeed: {}",
        output.content
    );
    assert!(
        output.content.contains("Example Domain"),
        "rendered HTML should contain the page title, got: {}",
        output.content.chars().take(300).collect::<String>()
    );

    // The tool must be advertised as part of the always-on `core` group.
    let mut active = std::collections::HashSet::new();
    active.insert("core".to_string());
    let defs = registry.available_definitions(&active);
    assert!(
        defs.iter().any(|d| d.function.name == "retrieve_webpage"),
        "retrieve_webpage should be in the core group's definitions"
    );
}
