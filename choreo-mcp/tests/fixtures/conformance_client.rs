// Conformance client for the official `@modelcontextprotocol/conformance` suite.
//
// The suite starts a scenario server, then runs this binary with the server URL
// as `argv[1]` and the scenario name in `MCP_CONFORMANCE_SCENARIO`. When the
// runner passes `--spec-version`, the resolved version arrives in
// `MCP_CONFORMANCE_PROTOCOL_VERSION`; the lifecycle is derived from it (dated
// versions through 2025-11-25 use the stateful `initialize` handshake, while the
// 2026-07-28 era is stateless). This binary maps each scenario the client
// supports onto `choreo-mcp` operations and exits non-zero on any failure.
//
// Not a shipped binary: it is a test fixture the conformance runner
// (`scripts/mcp-conformance.sh`) and CI build and drive. Scenarios that need a
// capability this client does not advertise (OAuth `auth/*`, elicitation) are
// deliberately not handled and are recorded in the committed expected-failures
// baseline instead.

use choreo_mcp::{McpProtocolMode, McpServer, McpServerConfig, McpTransport};
use std::collections::HashMap;
use std::process::ExitCode;
use std::time::Duration;

fn main() -> ExitCode {
    let scenario = std::env::var("MCP_CONFORMANCE_SCENARIO").unwrap_or_default();
    let Some(url) = std::env::args().nth(1) else {
        eprintln!(
            "usage: mcp-conformance-client <server-url>\n\
             (MCP_CONFORMANCE_SCENARIO must be set)"
        );
        return ExitCode::FAILURE;
    };
    match run(&scenario, &url) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("conformance scenario {scenario:?} failed: {e}");
            ExitCode::FAILURE
        }
    }
}

/// The lifecycle to use for a scenario.
///
/// When the runner forwards the requested spec version (newer suite builds do,
/// via `--spec-version`), derive the era from it: the 2026-07-28 era is
/// stateless, every dated version through 2025-11-25 is stateful. Older suite
/// builds forward no version (the pinned 0.1.16 does not), so fall back to the
/// scenario name: the suite's core client scenarios are legacy-era and must use
/// the stateful `initialize` handshake rather than an `auto` discover probe the
/// scenario's mock server does not answer.
fn protocol_for(scenario: &str, version: Option<&str>) -> McpProtocolMode {
    if let Some(v) = version {
        return if v.starts_with("2026") {
            McpProtocolMode::Modern
        } else {
            McpProtocolMode::Legacy
        };
    }
    match scenario {
        "initialize" | "tools_call" | "tools-call" => McpProtocolMode::Legacy,
        _ => McpProtocolMode::Auto,
    }
}

/// Build the HTTP client config the harness connects with.
fn config(url: &str, protocol: McpProtocolMode) -> McpServerConfig {
    McpServerConfig {
        slug: "conformance".into(),
        transport: McpTransport::Http {
            url: url.into(),
            headers: HashMap::new(),
        },
        enabled: true,
        timeout: Some(Duration::from_secs(20)),
        protocol,
        max_concurrent_calls: None,
        max_restarts: None,
        disabled_tools: Vec::new(),
    }
}

/// Run one scenario's operations against `url`.
fn run(scenario: &str, url: &str) -> Result<(), String> {
    let version = std::env::var("MCP_CONFORMANCE_PROTOCOL_VERSION").ok();
    let protocol = protocol_for(scenario, version.as_deref());
    match scenario {
        // `initialize`: connect (stateful or stateless per the requested
        // version), list the catalogue, and disconnect.
        "initialize" => {
            let server = McpServer::connect(&config(url, protocol)).map_err(|e| e.to_string())?;
            let tools = server.handle().list_tools().map_err(|e| e.to_string())?;
            tracing::debug!(count = tools.len(), "conformance: listed tools");
            Ok(())
        }
        // `tools_call`: the scenario's server exposes `add_numbers` and records
        // success only when the client invokes it, so list then call it.
        "tools_call" | "tools-call" => {
            let server = McpServer::connect(&config(url, protocol)).map_err(|e| e.to_string())?;
            let handle = server.handle();
            let tools = handle.list_tools().map_err(|e| e.to_string())?;
            let name = tools
                .iter()
                .find(|t| t.name == "add_numbers")
                .map(|t| t.name.clone())
                .ok_or_else(|| "the scenario did not advertise `add_numbers`".to_string())?;
            let result = handle
                .call_tool(0, &name, serde_json::json!({"a": 2, "b": 3}), None)
                .map_err(|e| e.to_string())?;
            if result.is_error {
                return Err("`add_numbers` returned an error result".to_string());
            }
            Ok(())
        }
        other => Err(format!("unsupported scenario {other:?}")),
    }
}
