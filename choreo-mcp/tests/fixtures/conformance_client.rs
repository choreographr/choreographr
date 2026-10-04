// Conformance client for the official `@modelcontextprotocol/conformance` suite.
//
// The suite starts a scenario server, then runs this binary with the server URL
// as `argv[1]` and the scenario name in `MCP_CONFORMANCE_SCENARIO`. When the
// runner passes `--spec-version`, the resolved version arrives in
// `MCP_CONFORMANCE_PROTOCOL_VERSION`; the lifecycle is derived from it (dated
// versions through 2025-11-25 use the stateful `initialize` handshake, while the
// 2026-07-28 era is stateless). Some scenarios hand the client the exact tool
// calls to make (e.g. the SEP-2243 custom-header encoding edge cases) in
// `MCP_CONFORMANCE_CONTEXT`. This binary maps each scenario the client supports
// onto `choreo-mcp` operations and exits non-zero only when the client itself
// could not perform the scenario (a server-side tool error is judged from the
// wire, not here).
//
// Not a shipped binary: it is a test fixture the conformance runner
// (`scripts/mcp-conformance.sh`) and CI build and drive. Scenarios that need a
// capability this client does not advertise (OAuth `auth/*`, elicitation) are
// deliberately not handled and are recorded in the committed expected-failures
// baseline instead.

use choreo_mcp::{McpProtocolMode, McpServer, McpServerConfig, McpTransport};
use serde_json::{Map, Value, json};
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
/// builds forward no version (the pinned legacy suite does not), so fall back to
/// the scenario name: the suite's core legacy scenarios must use the stateful
/// `initialize` handshake rather than an `auto` discover probe the scenario's
/// mock server does not answer.
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

/// The exact tool calls a scenario handed the client, if any.
///
/// The suite injects a scenario-specific JSON object in
/// `MCP_CONFORMANCE_CONTEXT`; when it carries a `toolCalls` array the client is
/// expected to issue exactly those calls (arguments included) so the scenario
/// can inspect the resulting wire traffic. Returns `None` when the scenario
/// supplied no calls, so the caller falls back to exercising the catalogue.
fn context_tool_calls() -> Option<Vec<(String, Value)>> {
    let raw = std::env::var("MCP_CONFORMANCE_CONTEXT").ok()?;
    let context: Value = serde_json::from_str(&raw).ok()?;
    let calls = context.get("toolCalls")?.as_array()?;
    let calls: Vec<(String, Value)> = calls
        .iter()
        .filter_map(|call| {
            let name = call.get("name")?.as_str()?.to_string();
            let args = call.get("arguments").cloned().unwrap_or(Value::Null);
            Some((name, args))
        })
        .collect();
    if calls.is_empty() { None } else { Some(calls) }
}

/// Synthesize a schema-shaped argument object for a tool.
///
/// The scenario servers accept any well-formed object (they validate the
/// request/header shape, not the argument semantics), so a dummy value per
/// declared property is enough to exercise the call path — including tools with
/// `required` arguments, which an empty object would otherwise fail before it
/// left the client.
fn dummy_args(schema: &Value) -> Value {
    let mut args = Map::new();
    if let Some(properties) = schema.get("properties").and_then(Value::as_object) {
        for (name, property) in properties {
            args.insert(name.clone(), dummy_value(property));
        }
    }
    Value::Object(args)
}

/// A placeholder value matching a property's declared JSON Schema type.
fn dummy_value(property: &Value) -> Value {
    match property.get("type").and_then(Value::as_str) {
        Some("string") => json!("x"),
        Some("integer" | "number") => json!(1),
        Some("boolean") => json!(true),
        Some("array") => json!([]),
        Some("object") => json!({}),
        Some("null") => Value::Null,
        _ => json!("x"),
    }
}

/// Run one scenario's operations against `url`.
fn run(scenario: &str, url: &str) -> Result<(), String> {
    let version = std::env::var("MCP_CONFORMANCE_PROTOCOL_VERSION").ok();
    let protocol = protocol_for(scenario, version.as_deref());
    let server = McpServer::connect(&config(url, protocol)).map_err(|e| e.to_string())?;
    let handle = server.handle();
    let tools = handle.list_tools().map_err(|e| e.to_string())?;
    tracing::debug!(count = tools.len(), "conformance: listed tools");

    // A scenario that supplied calls wants exactly those calls; issue them and
    // stop. A per-tool error is the server's own result (or a deliberate
    // rejection the scenario is testing), so it is judged from the wire and not
    // treated as a client failure here.
    if let Some(calls) = context_tool_calls() {
        for (name, args) in calls {
            let _ = handle.call_tool(0, &name, args, None);
        }
        return Ok(());
    }

    match scenario {
        // List-only scenarios: the server inspects the client's negotiation and
        // `tools/list` traffic; there is nothing further to invoke.
        "initialize" | "request-metadata" | "json-schema-ref-no-deref" => Ok(()),
        // The preservation scenario diffs the schema the client received against
        // the fixture, so the client must echo the focal tool's `inputSchema`
        // back verbatim through the permissive echo tool.
        "json-schema-2020-12-preservation" => {
            let focal = tools
                .iter()
                .find(|t| t.name == "json_schema_2020_12_tool")
                .ok_or_else(|| {
                    "the scenario did not advertise `json_schema_2020_12_tool`".to_string()
                })?;
            handle
                .call_tool(
                    0,
                    "json_schema_echo",
                    json!({ "schema": focal.input_schema }),
                    None,
                )
                .map_err(|e| e.to_string())?;
            Ok(())
        }
        // Default: exercise every advertised tool so the server observes the
        // client's request shape and generated headers. This is also how a
        // scenario proves the client *excluded* a malformed tool — an excluded
        // tool is simply absent from `tools`, so it is never called.
        _ => {
            for tool in &tools {
                let args = dummy_args(&tool.input_schema);
                let _ = handle.call_tool(0, &tool.name, args, None);
            }
            Ok(())
        }
    }
}
