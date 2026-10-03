// A scripted MCP stdio server used as a test fixture.
//
// Implements just enough of the legacy (`2024-11-05`) JSON-RPC protocol for
// the client integration tests: `initialize`, `tools/list`, and `tools/call`.
// It is spawned as a subprocess by the test harness (see the two `tests/it`
// suites), never linked in-process, so its behaviour can be scripted through
// the scenario selector without affecting the client under test.
//
// Scenario selection: the first CLI argument, or the `MCP_FIXTURE_SCENARIO`
// environment variable. Unknown scenarios behave as `default`.
//
// Scenarios:
// - `default` — full tool set (`echo`, `boom`, `image`, `slow`, `big`, `bad`).
// - `no-init` — the `initialize` handshake returns a JSON-RPC error.
// - `crash-on-call` — the process exits the moment `tools/call` arrives.
// - `garbage` — emits a non-JSON line before each real response.

use std::io::{BufRead, Write};

/// A 1x1 opaque PNG (valid CRCs), base64-encoded, returned by the `image` tool.
const PNG_1X1: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR4nGP4z8DwHwAFAAH/iZk9HQAAAABJRU5ErkJggg==";

/// A line safely over the client's 8 MiB stdout cap, returned by the `big` tool.
const OVERSIZED_FILLER_LEN: usize = 9 * 1024 * 1024;

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
            Ok(0) | Err(_) => break,
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
            continue;
        };
        let method = req.get("method").and_then(|m| m.as_str()).unwrap_or("");

        // Simulate a crash: returning from `main` closes stdout, which the
        // client observes as an abrupt server shutdown on the in-flight call.
        if scenario == "crash-on-call" && method == "tools/call" {
            break;
        }

        if scenario == "garbage" {
            let _ = writeln!(out, "this is not json");
        }

        match method {
            "initialize" => {
                if scenario == "no-init" {
                    respond_error(&mut out, &id, -32601, "method not found");
                } else {
                    respond_result(
                        &mut out,
                        &id,
                        &serde_json::json!({
                            "protocolVersion": "2024-11-05",
                            "capabilities": {"tools": {}},
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
            "tools/call" => handle_call(&mut out, &id, &req),
            _ => respond_error(&mut out, &id, -32601, "method not found"),
        }
        let _ = out.flush();
    }
}

/// The tool catalogue the fixture advertises.
fn fixture_tools() -> serde_json::Value {
    serde_json::json!([
        {"name": "echo", "description": "Echo a message back.",
         "inputSchema": {"type": "object", "properties": {"message": {"type": "string"}}}},
        {"name": "boom", "description": "Always fails.", "inputSchema": {"type": "object"}},
        {"name": "image", "description": "Return a 1x1 PNG.", "inputSchema": {"type": "object"}},
        {"name": "slow", "description": "Sleep then answer.", "inputSchema": {"type": "object"}},
        {"name": "big", "description": "Answer with an oversized line.", "inputSchema": {"type": "object"}},
        // A tool whose schema is not an object: the client must drop it while
        // keeping the rest.
        {"name": "bad", "description": "Malformed schema.", "inputSchema": ["not", "an", "object"]},
    ])
}

/// Answer a `tools/call` according to the requested tool name.
fn handle_call(out: &mut impl Write, id: &serde_json::Value, req: &serde_json::Value) {
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
        "slow" => {
            std::thread::sleep(std::time::Duration::from_secs(5));
            respond_result(
                out,
                id,
                &serde_json::json!({
                    "content": [{"type": "text", "text": "slow done"}],
                    "isError": false,
                }),
            );
        }
        "big" => respond_result(
            out,
            id,
            &serde_json::json!({
                "content": [{"type": "text", "text": "x".repeat(OVERSIZED_FILLER_LEN)}],
                "isError": false,
            }),
        ),
        _ => respond_error(out, id, -32602, "unknown tool"),
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
