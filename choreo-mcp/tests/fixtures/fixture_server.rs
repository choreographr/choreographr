// A scripted MCP stdio server used as a test fixture.
//
// Implements enough of the protocol for the client integration tests: the
// stateless `server/discover` handshake, the legacy `initialize` handshake,
// `tools/list`, and `tools/call`. It is spawned as a subprocess by the test
// harness (via `env!("CARGO_BIN_EXE_mcp-fixture-server")`), never linked
// in-process, so its behaviour can be scripted through the scenario selector
// without affecting the client under test.
//
// Scenario selection: the first CLI argument, or the `MCP_FIXTURE_SCENARIO`
// environment variable. Unknown scenarios behave as `legacy`.
//
// Scenarios:
// - `legacy` (default) — legacy `initialize` server; rejects `server/discover`.
//   Full tool set (`echo`, `boom`, `image`, `structured`, `slow`, `progress`,
//   `need_input`).
// - `modern*` — any scenario whose name starts with `modern` is a 2026-07-28
//   stateless server answering `server/discover`; the `modern-resources`
//   variant declares (and serves) the `resources` capability.
// - `no-init` — the `initialize` handshake returns a JSON-RPC error.
// - `crash-on-call` — the process exits the moment `tools/call` arrives.
// - `garbage` — emits a non-JSON line before each real response.
// - `oversized` — emits one line far larger than any sane frame.
// - `stubborn` — never exits on stdin EOF (it sleeps forever), so the client's
//   bounded shutdown is exercised against a child that does not cooperate.
// - `modern-list-changed` — a 2026-07-28 server that declares
//   `tools.listChanged` and, on `subscriptions/listen`, acknowledges the
//   subscription and immediately emits a `notifications/tools/list_changed`
//   (the subscription-to-daemon forwarding test).
//
// Tools:
// - `slow` answers from a background thread so the read loop can observe a
//   `notifications/cancelled` (the cancellation-observation test).
// - `progress` emits two `notifications/progress` for the request's token.
// - `need_input` answers `input_required` first, then `complete` once the retry
//   carries `inputResponses` (the MRTR test).
//
// When `MCP_FIXTURE_MARKER` is set, the `slow` tool creates that file as soon
// as a `tools/call` for it arrives; when `MCP_FIXTURE_CANCEL_MARKER` is set, a
// received `notifications/cancelled` writes that file, so the cancellation
// tests can synchronise without sleeping.

use std::io::{BufRead, Write};

/// A 1x1 opaque PNG (valid CRCs), base64-encoded, returned by the `image` tool.
const PNG_1X1: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR4nGP4z8DwHwAFAAH/iZk9HQAAAABJRU5ErkJggg==";

fn main() {
    let scenario = std::env::args()
        .nth(1)
        .or_else(|| std::env::var("MCP_FIXTURE_SCENARIO").ok())
        .unwrap_or_default();

    let stdin = std::io::stdin();
    let mut reader = stdin.lock();
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    let mut line = String::new();

    loop {
        line.clear();
        match reader.read_line(&mut line) {
            Ok(0) | Err(_) => {
                // A `stubborn` server ignores stdin EOF and keeps running, so
                // the client's bounded shutdown/join is exercised against a
                // child that does not exit on its own. The client's process
                // group kill is what ultimately ends it.
                if scenario == "stubborn" {
                    loop {
                        std::thread::sleep(std::time::Duration::from_secs(3600));
                    }
                }
                break;
            }
            Ok(_) => {}
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let req: serde_json::Value = match serde_json::from_str(trimmed) {
            Ok(v) => v,
            Err(_) => continue,
        };

        // Notifications carry no `id` and expect no response.
        let Some(id) = req.get("id").cloned() else {
            // Record a cancellation so the client-side cancellation test can
            // assert the SERVER observed `notifications/cancelled` (not just
            // that the client stopped waiting).
            if req.get("method").and_then(|m| m.as_str()) == Some("notifications/cancelled")
                && let Ok(marker) = std::env::var("MCP_FIXTURE_CANCEL_MARKER")
            {
                let request_id = req
                    .pointer("/params/requestId")
                    .cloned()
                    .unwrap_or(serde_json::Value::Null);
                let _ = std::fs::write(marker, request_id.to_string());
            }
            continue;
        };
        let method = req.get("method").and_then(|m| m.as_str()).unwrap_or("");

        // Simulate a crash: returning from `main` closes stdout, which the
        // client observes as an abrupt shutdown on the in-flight call.
        if scenario == "crash-on-call" && method == "tools/call" {
            break;
        }

        if scenario == "garbage" {
            let _ = writeln!(out, "this is not json");
        }
        if scenario == "oversized" && method == "tools/call" {
            // A single line far larger than any sane frame, and not valid JSON:
            // the client must not buffer it unboundedly nor treat it as a result.
            let _ = writeln!(out, "{}", "x".repeat(9 * 1024 * 1024));
            let _ = out.flush();
            break;
        }

        match method {
            "server/discover" if scenario.starts_with("modern") => respond_result(
                &mut out,
                &id,
                &serde_json::json!({
                    "resultType": "complete",
                    "supportedVersions": ["2026-07-28"],
                    "capabilities": capabilities(&scenario),
                    "ttlMs": 0,
                    "cacheScope": "public",
                    "_meta": {
                        "io.modelcontextprotocol/serverInfo": {
                            "name": "fixture",
                            "version": "0.1.0"
                        }
                    }
                }),
            ),
            "server/discover" => respond_error(&mut out, &id, -32601, "method not found"),
            "initialize" => {
                if scenario == "no-init" {
                    respond_error(&mut out, &id, -32601, "method not found");
                } else {
                    respond_result(
                        &mut out,
                        &id,
                        &serde_json::json!({
                            "protocolVersion": "2025-11-25",
                            "capabilities": capabilities(&scenario),
                            "serverInfo": {"name": "fixture", "version": "0.1.0"},
                        }),
                    );
                }
            }
            "tools/list" => {
                respond_result(
                    &mut out,
                    &id,
                    &serde_json::json!({ "tools": fixture_tools() }),
                );
            }
            "tools/call" => handle_call(&mut out, &id, &req, &scenario),
            // `subscriptions/listen`: acknowledge, then emit one tools
            // list-changed notification on the same subscription id. No result
            // is sent — the request is long-lived, so the client reads the
            // stream until it cancels. Only the list-changed scenario opens a
            // stream; any other server answers method-not-found.
            "subscriptions/listen" if scenario.contains("list-changed") => {
                let sub_meta = serde_json::json!({
                    "io.modelcontextprotocol/subscriptionId": id
                });
                let acknowledged = serde_json::json!({
                    "jsonrpc": "2.0",
                    "method": "notifications/subscriptions/acknowledged",
                    "params": {
                        "_meta": sub_meta,
                        "notifications": {"toolsListChanged": true}
                    }
                });
                let _ = writeln!(out, "{acknowledged}");
                let changed = serde_json::json!({
                    "jsonrpc": "2.0",
                    "method": "notifications/tools/list_changed",
                    "params": {
                        "_meta": {"io.modelcontextprotocol/subscriptionId": id}
                    }
                });
                let _ = writeln!(out, "{changed}");
            }
            "resources/list" => respond_result(
                &mut out,
                &id,
                &serde_json::json!({
                    "resources": [
                        {"uri": "file:///readme.txt", "name": "readme", "mimeType": "text/plain"},
                        {"uri": "file:///data.bin", "name": "data", "mimeType": "application/octet-stream", "description": "binary data"}
                    ]
                }),
            ),
            "resources/read" => {
                let uri = req
                    .pointer("/params/uri")
                    .and_then(|u| u.as_str())
                    .unwrap_or("");
                if uri == "file:///readme.txt" {
                    respond_result(
                        &mut out,
                        &id,
                        &serde_json::json!({
                            "contents": [
                                {"uri": uri, "mimeType": "text/plain", "text": "hello from a resource"}
                            ]
                        }),
                    );
                } else {
                    respond_error(&mut out, &id, -32602, "resource not found");
                }
            }
            _ => respond_error(&mut out, &id, -32601, "method not found"),
        }
        let _ = out.flush();
    }
}

/// Server capabilities advertised for a scenario: the `resources` scenarios
/// additionally declare the `resources` capability so the client registers its
/// resource wrapper tools, and the `modern-list-changed` scenario declares
/// `tools.listChanged` so the client opens a `subscriptions/listen` stream.
fn capabilities(scenario: &str) -> serde_json::Value {
    let tools = if scenario.contains("list-changed") {
        serde_json::json!({"listChanged": true})
    } else {
        serde_json::json!({})
    };
    if scenario.contains("resources") {
        serde_json::json!({"tools": tools, "resources": {}})
    } else {
        serde_json::json!({"tools": tools})
    }
}

/// The tool catalogue the fixture advertises.
///
/// Every `inputSchema` is a JSON object (never `null`, never an array): rmcp
/// enforces that shape at decode time, so a non-object schema would fail the
/// whole listing rather than exercise the client's per-tool guard.
fn fixture_tools() -> serde_json::Value {
    serde_json::json!([
        {"name": "echo", "description": "Echo a message back.",
         "inputSchema": {"type": "object", "properties": {"message": {"type": "string"}}}},
        {"name": "boom", "description": "Always fails.", "inputSchema": {"type": "object"}},
        {"name": "image", "description": "Return a 1x1 PNG.", "inputSchema": {"type": "object"}},
        {"name": "structured", "description": "Return structured content.",
         "inputSchema": {"type": "object"},
         "outputSchema": {"type": "object", "properties": {"value": {"type": "number"}}}},
        {"name": "slow", "description": "Block until cancelled.", "inputSchema": {"type": "object"}},
        {"name": "progress", "description": "Report progress then complete.", "inputSchema": {"type": "object"}},
        {"name": "need_input", "description": "Ask for input, then complete on retry.", "inputSchema": {"type": "object"}},
    ])
}

/// Answer a `tools/call` according to the requested tool name.
fn handle_call(
    out: &mut impl Write,
    id: &serde_json::Value,
    req: &serde_json::Value,
    scenario: &str,
) {
    let name = req
        .pointer("/params/name")
        .and_then(|n| n.as_str())
        .unwrap_or("");
    let message = req
        .pointer("/params/arguments/message")
        .and_then(|m| m.as_str())
        .unwrap_or("");

    match name {
        "echo" => respond_result(
            out,
            id,
            &serde_json::json!({
                "content": [{"type": "text", "text": format!("echo: {message}")}],
                "isError": false,
            }),
        ),
        // Emit two progress notifications echoing the request's progress token,
        // then complete — exercises progress → streaming chunks.
        "progress" => {
            if let Some(token) = req.pointer("/params/_meta/progressToken").cloned() {
                for (step, message) in [(1, "working"), (2, "almost done")] {
                    let note = serde_json::json!({
                        "jsonrpc": "2.0",
                        "method": "notifications/progress",
                        "params": {"progressToken": token, "progress": step, "total": 2, "message": message},
                    });
                    let _ = writeln!(out, "{note}");
                }
            }
            respond_result(
                out,
                id,
                &serde_json::json!({
                    "content": [{"type": "text", "text": "progress done"}],
                    "isError": false,
                }),
            );
        }
        // Answer the first call with `input_required`; a retry that carries
        // `inputResponses` completes. Exercises the MRTR decline loop.
        "need_input" => {
            let has_responses = req
                .pointer("/params/inputResponses")
                .and_then(|v| v.as_object())
                .is_some_and(|m| !m.is_empty());
            if has_responses {
                respond_result(
                    out,
                    id,
                    &serde_json::json!({
                        "content": [{"type": "text", "text": "input accepted"}],
                        "isError": false,
                    }),
                );
            } else {
                respond_result(
                    out,
                    id,
                    &serde_json::json!({
                        "resultType": "input_required",
                        "inputRequests": {
                            "q1": {
                                "method": "elicitation/create",
                                "params": {
                                    "mode": "form",
                                    "message": "Which environment?",
                                    "requestedSchema": {"type": "object", "properties": {}}
                                }
                            }
                        },
                        "requestState": "opaque-state-1"
                    }),
                );
            }
        }
        "boom" => respond_result(
            out,
            id,
            &serde_json::json!({
                "content": [{"type": "text", "text": "boom failed"}],
                "isError": true,
            }),
        ),
        "image" => respond_result(
            out,
            id,
            &serde_json::json!({
                "content": [{"type": "image", "data": PNG_1X1, "mimeType": "image/png"}],
                "isError": false,
            }),
        ),
        "structured" => respond_result(
            out,
            id,
            &serde_json::json!({
                "content": [{"type": "text", "text": "{\"value\":42}"}],
                "structuredContent": {"value": 42},
                "isError": false,
            }),
        ),
        "slow" => {
            // Signal that the call is in flight (used by the cancellation test),
            // then answer from a background thread. The sleep must NOT run on
            // the read loop: a cancellation can only be observed by the loop
            // reading the next line, so blocking here would hide the very
            // `notifications/cancelled` the test asserts on.
            if let Ok(marker) = std::env::var("MCP_FIXTURE_MARKER") {
                let _ = std::fs::write(marker, b"started");
            }
            let id = id.clone();
            std::thread::spawn(move || {
                // Long enough that the client always cancels first.
                std::thread::sleep(std::time::Duration::from_secs(30));
                let response = serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": {
                        "content": [{"type": "text", "text": "slow done"}],
                        "isError": false,
                    }
                });
                println!("{response}");
            });
        }
        _ => {
            let _ = scenario;
            respond_error(out, id, -32602, "unknown tool");
        }
    }
}

/// Write a JSON-RPC success response as one line.
fn respond_result(out: &mut impl Write, id: &serde_json::Value, result: &serde_json::Value) {
    let msg = serde_json::json!({"jsonrpc": "2.0", "id": id, "result": result});
    let _ = writeln!(out, "{msg}");
}

/// Write a JSON-RPC error response as one line.
fn respond_error(out: &mut impl Write, id: &serde_json::Value, code: i64, message: &str) {
    let msg = serde_json::json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": {"code": code, "message": message},
    });
    let _ = writeln!(out, "{msg}");
}
