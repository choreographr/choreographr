//! Reasoning round-trip integration tests (phase 6): adapter capture →
//! re-emit through the real wire.
//!
//! Each test spins up a tiny scripted HTTP provider (a `TcpListener` serving
//! one canned response per request and recording every request body), points
//! a real client (`OpenAiClient` / `AnthropicClient` / `GoogleClient`) at it,
//! and drives a two-request tool loop:
//!
//!   turn 1: provider responds with a tool call + reasoning payload
//!           (chat `reasoning_content`, Anthropic thinking/redacted_thinking
//!           blocks, Gemini thought signatures) → the adapter captures the
//!           payload into the opaque `ReasoningArtifact` at the parse boundary
//!   turn 2: the same messages plus the artifact are sent back → the adapter
//!           re-emits the payload in its own wire format
//!
//! The mock's captured request bodies are then asserted: the artifact captured
//! on turn 1 is re-emitted verbatim on turn 2's tool-loop request. These tests
//! bind a real local TCP socket, so per AGENTS.md they live in `tests/` and are
//! marked `#[ignore]` (run via `cargo test-integration`).

use std::collections::VecDeque;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::{Arc, Mutex};

use choreo_ai_protocols::anthropic::{AnthropicClient, AnthropicConfig};
use choreo_ai_protocols::google::{GoogleClient, GoogleConfig};
use choreo_ai_protocols::openai::{
    AssistantToolCall, AssistantToolFunction, ChatRequestMessage, ChatToolDefinition,
    MaxTokensField, OpenAiClient, ServiceConfig,
};
use choreo_ai_protocols::{ChatAssistantToolUse, ChatTurnRequest, ChatTurnResult};
use choreo_proto::ReasoningArtifact;

// ── Scripted mock provider ──────────────────────────────────────────────

/// One HTTP request captured by the mock provider.
#[derive(Debug, Clone)]
struct CapturedRequest {
    method: String,
    path: String,
    /// Header lines captured verbatim (lowercased name → value).
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl CapturedRequest {
    /// Look up a request header by name (case-insensitive).
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k == &name.to_ascii_lowercase())
            .map(|(_, v)| v.as_str())
    }
}

impl CapturedRequest {
    fn body_json(&self) -> serde_json::Value {
        serde_json::from_slice(&self.body).expect("captured request body is JSON")
    }
}

/// A tiny scripted HTTP provider: serves one canned response per request, in
/// order (the last entry repeats for any excess requests), and records every
/// request body so tests can assert on the wire.
///
/// Only speaks the subset of HTTP/1.1 the ureq agents need: reads the request
/// head + `Content-Length` body, replies with `Connection: close` (so the
/// client opens a fresh connection per request), and drops the stream.
struct MockProvider {
    addr: std::net::SocketAddr,
    captured: Arc<Mutex<Vec<CapturedRequest>>>,
    _handle: std::thread::JoinHandle<()>,
}

impl MockProvider {
    /// `responses`: `(status, content_type, body)` served in order; the last
    /// entry repeats for any excess requests.
    fn start(responses: Vec<(u16, &'static str, String)>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind mock provider");
        let addr = listener.local_addr().expect("mock provider local addr");
        let captured = Arc::new(Mutex::new(Vec::new()));
        let captured_thread = Arc::clone(&captured);
        let responses = Arc::new(Mutex::new(VecDeque::from(responses)));

        let handle = std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else {
                    break;
                };
                let mut responses = responses.lock().unwrap();

                // Read the request head (through the blank line) and then the
                // Content-Length body, recording both for later assertions.
                let mut buf = Vec::new();
                let mut tmp = [0u8; 4096];
                let head_end = loop {
                    let n = stream.read(&mut tmp).unwrap_or(0);
                    if n == 0 {
                        // Client hung up mid-request; move on.
                        break 0;
                    }
                    buf.extend_from_slice(&tmp[..n]);
                    if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                        break pos;
                    }
                    if buf.len() > 1 << 20 {
                        panic!("mock provider: request head too large");
                    }
                };
                if head_end == 0 {
                    continue;
                }
                let head = String::from_utf8_lossy(&buf[..head_end]).to_string();
                let body_start = head_end + 4;
                let content_length = head
                    .lines()
                    .find_map(|line| {
                        let (key, value) = line.split_once(':')?;
                        if !key.trim().eq_ignore_ascii_case("content-length") {
                            return None;
                        }
                        value.trim().parse::<usize>().ok()
                    })
                    .unwrap_or(0);
                while buf.len().saturating_sub(body_start) < content_length {
                    let n = stream.read(&mut tmp).unwrap_or(0);
                    if n == 0 {
                        break;
                    }
                    buf.extend_from_slice(&tmp[..n]);
                }
                let body = buf[body_start..body_start + content_length].to_vec();

                let mut parts = head.split_whitespace();
                let method = parts.next().unwrap_or("").to_string();
                let path = parts.next().unwrap_or("").to_string();
                // Capture headers (everything after the request line, before the
                // blank line) so tests can assert on outbound header values.
                let headers = head
                    .lines()
                    .skip(1)
                    .filter_map(|line| line.split_once(':'))
                    .map(|(k, v)| (k.trim().to_ascii_lowercase(), v.trim().to_string()))
                    .collect();
                captured_thread.lock().unwrap().push(CapturedRequest {
                    method,
                    path,
                    headers,
                    body,
                });

                // Serve the next scripted response (peek when it is the last
                // one so excess requests still get an answer).
                let (status, content_type, response_body) = if responses.len() > 1 {
                    responses.pop_front().expect("scripted response")
                } else {
                    responses.front().cloned().expect("scripted response")
                };
                let reason = if status == 200 { "OK" } else { "Error" };
                let head = format!(
                    "HTTP/1.1 {status} {reason}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    response_body.len()
                );
                stream.write_all(head.as_bytes()).unwrap_or_default();
                stream
                    .write_all(response_body.as_bytes())
                    .unwrap_or_default();
                stream.flush().unwrap_or_default();
            }
        });

        Self {
            addr,
            captured,
            _handle: handle,
        }
    }

    /// Base URL for the given path prefix (e.g. `"v1"` → `http://…/v1`).
    fn base_url(&self, prefix: &str) -> String {
        format!(
            "http://127.0.0.1:{}/{}",
            self.addr.port(),
            prefix.trim_matches('/')
        )
    }

    fn requests(&self) -> Vec<CapturedRequest> {
        self.captured.lock().unwrap().clone()
    }
}

// ── Shared builders ─────────────────────────────────────────────────────

/// A `get_weather` tool definition, matching the tool calls in the canned
/// provider responses.
fn tool_def() -> ChatToolDefinition {
    ChatToolDefinition::function(
        "get_weather",
        "Get the current weather for a city",
        serde_json::json!({
            "type": "object",
            "properties": {"city": {"type": "string"}},
            "required": ["city"],
        }),
    )
}

/// Convert a captured `ChatAssistantToolUse` into the assistant message the
/// daemon would send on the next tool-loop request: same text, same tool
/// calls, and the opaque reasoning artifact carried on the message so the
/// adapter can re-emit it in its own wire format.
fn assistant_message_from_tool_use(tool_use: &ChatAssistantToolUse) -> ChatRequestMessage {
    let tool_calls = tool_use
        .tool_calls
        .iter()
        .map(|tc| AssistantToolCall {
            id: tc.id.clone(),
            kind: "function".to_string(),
            function: AssistantToolFunction {
                name: tc.name.clone(),
                arguments: tc.arguments_json.clone(),
            },
        })
        .collect();
    ChatRequestMessage {
        role: "assistant",
        content: tool_use.content.clone(),
        tool_call_id: None,
        tool_calls: Some(tool_calls),
        reasoning_content: None,
        reasoning: None,
        reasoning_text: None,
        reasoning_artifact: tool_use.reasoning_artifact.clone(),
    }
}

/// The tool-result message the daemon appends after executing a tool call.
fn tool_result_message(tool_use: &ChatAssistantToolUse) -> ChatRequestMessage {
    ChatRequestMessage {
        role: "tool",
        content: Some("72°F and sunny".to_string()),
        tool_call_id: Some(tool_use.tool_calls[0].id.clone()),
        tool_calls: None,
        reasoning_content: None,
        reasoning: None,
        reasoning_text: None,
        reasoning_artifact: None,
    }
}

fn user_message(text: &str) -> ChatRequestMessage {
    ChatRequestMessage::simple("user", text.to_string())
}

// ── 1. DeepSeek / OpenAI-chat tool-loop echo ─────────────────────────────

/// A two-tool-call turn with thinking enabled must receive the captured
/// `reasoning_content` on the second (tool-loop) request, byte-for-byte.
/// This is the DeepSeek/Kimi contract: a tool-loop request whose assistant
/// message drops `reasoning_content` is rejected with a 400.
#[ignore]
#[test]
fn deepseek_tool_loop_echoes_reasoning_content_verbatim() {
    const REASONING: &str = "DeepSeek is analyzing the weather data step by step.";

    let tool_use_response = format!(
        r#"{{
            "choices": [{{
                "message": {{
                    "role": "assistant",
                    "content": null,
                    "tool_calls": [{{
                        "id": "call_1",
                        "type": "function",
                        "function": {{"name": "get_weather", "arguments": "{{\"city\":\"London\"}}"}}
                    }}],
                    "reasoning_content": "{REASONING}",
                    "reasoning": null,
                    "reasoning_text": null
                }},
                "finish_reason": "tool_calls"
            }}],
            "usage": {{"prompt_tokens": 12, "completion_tokens": 9, "total_tokens": 21}}
        }}"#
    );
    let final_response = r#"{
        "choices": [{
            "message": {
                "role": "assistant",
                "content": "The weather in London is 72°F and sunny.",
                "tool_calls": [],
                "reasoning_content": null,
                "reasoning": null,
                "reasoning_text": null
            },
            "finish_reason": "stop"
        }],
        "usage": {"prompt_tokens": 20, "completion_tokens": 4, "total_tokens": 24}
    }"#;

    let mock = MockProvider::start(vec![
        (200, "application/json", tool_use_response),
        (200, "application/json", final_response.to_string()),
    ]);

    let client = OpenAiClient::new(
        ServiceConfig {
            base_url: mock.base_url("v1"),
            provider_slug: "deepseek",
            streaming: false,
            retry_max_attempts: 1,
            connect_timeout_secs: 5,
            request_timeout_secs: 30,
            total_timeout_secs: 60,
            chat_completions_max_tokens_field: MaxTokensField::MaxCompletionTokens,
            ..Default::default()
        },
        "test-key".to_string(),
    )
    .expect("openai client");

    let tools = vec![tool_def()];

    // ── Turn 1: provider replies with a tool call + reasoning_content ──
    let turn1 = client
        .chat_completion_turn(ChatTurnRequest {
            model: "deepseek-v4-pro",
            messages: &[user_message("What's the weather in London?")],
            tools: &tools,
            thinking_effort: "high".to_string(),
            on_retry: &mut None,
            cancel_rx: None,
            previous_response_id: None,
            tool_results: &[],
            programmatic_tool_calling: false,
        })
        .expect("turn 1");
    let ChatTurnResult::ToolUse(tool_use) = turn1 else {
        panic!("expected ToolUse on turn 1, got {turn1:?}");
    };
    assert_eq!(tool_use.tool_calls.len(), 1);
    // The reasoning text is captured as an opaque artifact at the parse
    // boundary — before the display field is consumed.
    assert_eq!(
        tool_use.reasoning_artifact,
        Some(ReasoningArtifact::ChatReasoning(
            REASONING.as_bytes().to_vec()
        )),
        "reasoning_content must be captured into the round-trip artifact",
    );

    // ── Turn 2 (tool loop): re-emit the artifact on the assistant message ──
    let messages = vec![
        user_message("What's the weather in London?"),
        assistant_message_from_tool_use(&tool_use),
        tool_result_message(&tool_use),
    ];
    let turn2 = client
        .chat_completion_turn(ChatTurnRequest {
            model: "deepseek-v4-pro",
            messages: &messages,
            tools: &tools,
            thinking_effort: "high".to_string(),
            on_retry: &mut None,
            cancel_rx: None,
            previous_response_id: None,
            tool_results: &[],
            programmatic_tool_calling: false,
        })
        .expect("turn 2");
    assert!(matches!(turn2, ChatTurnResult::FinalText(_)));

    // ── Assert on the wire: the artifact survived capture → carry → re-emit ──
    let reqs = mock.requests();
    assert_eq!(reqs.len(), 2, "exactly two requests hit the mock");
    assert_eq!(reqs[0].method, "POST");
    assert_eq!(reqs[1].method, "POST");
    assert_eq!(reqs[0].path, "/v1/chat/completions");
    assert_eq!(reqs[1].path, "/v1/chat/completions");

    // Turn 1 has nothing to echo yet: no message may carry reasoning.
    let body1 = reqs[0].body_json();
    for msg in body1["messages"].as_array().expect("messages array") {
        assert!(
            msg.get("reasoning_content").is_none(),
            "turn 1 must not invent reasoning_content: {msg}",
        );
    }

    // Turn 2: the assistant tool-call message must carry the captured
    // reasoning_content VERBATIM.
    let body2 = reqs[1].body_json();
    let assistant = body2["messages"]
        .as_array()
        .expect("messages array")
        .iter()
        .find(|m| m["role"] == "assistant" && m.get("tool_calls").is_some())
        .expect("assistant tool-call message on turn 2");
    assert_eq!(
        assistant["reasoning_content"].as_str(),
        Some(REASONING),
        "reasoning_content must be echoed verbatim on the tool-loop request",
    );
}

// ── 2. Anthropic: thinking blocks echoed byte-identical ─────────────────

/// Anthropic requires the encrypted thinking blocks (signature +
/// redacted_thinking data) from turn 1 to be echoed back, complete and
/// unmodified, alongside the `tool_use` block on the next request — modified
/// or missing blocks are a 400. The wire body must contain the exact block
/// JSON captured at parse time.
#[ignore]
#[test]
fn anthropic_thinking_blocks_echoed_byte_identical() {
    // The exact artifact the non-streaming path assembles from these blocks:
    // keys serialize alphabetically (serde_json default BTreeMap ordering),
    // block order preserved.
    const EXPECTED_ARTIFACT: &[u8] = br#"[{"signature":"sig_abc123","thinking":"Let me analyze.","type":"thinking"},{"data":"eJxT_opaque","type":"redacted_thinking"}]"#;

    let tool_use_response = r#"{
        "id": "msg_01",
        "type": "message",
        "role": "assistant",
        "content": [
            {"type": "thinking", "thinking": "Let me analyze.", "signature": "sig_abc123"},
            {"type": "redacted_thinking", "data": "eJxT_opaque"},
            {"type": "tool_use", "id": "call_1", "name": "get_weather", "input": {"city": "London"}}
        ],
        "stop_reason": "tool_use",
        "model": "claude-haiku-4-5",
        "usage": {"input_tokens": 10, "output_tokens": 5}
    }"#;
    let final_response = r#"{
        "id": "msg_02",
        "type": "message",
        "role": "assistant",
        "content": [{"type": "text", "text": "The weather in London is 72°F and sunny."}],
        "stop_reason": "end_turn",
        "model": "claude-haiku-4-5",
        "usage": {"input_tokens": 24, "output_tokens": 6}
    }"#;

    let mock = MockProvider::start(vec![
        (200, "application/json", tool_use_response.to_string()),
        (200, "application/json", final_response.to_string()),
    ]);

    let client = AnthropicClient::new(
        AnthropicConfig {
            base_url: mock.base_url(""),
            streaming: false,
            retry_max_attempts: 1,
            connect_timeout_secs: 5,
            request_timeout_secs: 30,
            total_timeout_secs: 60,
            ..Default::default()
        },
        "test-key".to_string(),
    )
    .expect("anthropic client");

    let tools = vec![tool_def()];

    // ── Turn 1: thinking + redacted_thinking + tool_use blocks ──
    let turn1 = client
        .chat_completion_turn(ChatTurnRequest {
            model: "claude-haiku-4-5",
            messages: &[user_message("What's the weather in London?")],
            tools: &tools,
            thinking_effort: "high".to_string(),
            on_retry: &mut None,
            cancel_rx: None,
            previous_response_id: None,
            tool_results: &[],
            programmatic_tool_calling: false,
        })
        .expect("turn 1");
    let ChatTurnResult::ToolUse(tool_use) = turn1 else {
        panic!("expected ToolUse on turn 1, got {turn1:?}");
    };
    assert_eq!(tool_use.tool_calls.len(), 1);
    // Signatures + redacted data intact, original block order preserved.
    assert_eq!(
        tool_use.reasoning_artifact,
        Some(ReasoningArtifact::AnthropicThinking(
            EXPECTED_ARTIFACT.to_vec()
        )),
        "thinking + redacted_thinking blocks must be captured byte-exactly",
    );

    // ── Turn 2 (tool loop, thinking still enabled) ──
    let messages = vec![
        user_message("What's the weather in London?"),
        assistant_message_from_tool_use(&tool_use),
        tool_result_message(&tool_use),
    ];
    let turn2 = client
        .chat_completion_turn(ChatTurnRequest {
            model: "claude-haiku-4-5",
            messages: &messages,
            tools: &tools,
            thinking_effort: "high".to_string(),
            on_retry: &mut None,
            cancel_rx: None,
            previous_response_id: None,
            tool_results: &[],
            programmatic_tool_calling: false,
        })
        .expect("turn 2");
    assert!(matches!(turn2, ChatTurnResult::FinalText(_)));

    // ── Assert on the wire ──
    let reqs = mock.requests();
    assert_eq!(reqs.len(), 2);
    assert_eq!(reqs[0].path, "/v1/messages");
    assert_eq!(reqs[1].path, "/v1/messages");

    // Semantic equality: the assistant content array starts with the captured
    // thinking + redacted_thinking blocks, then the tool_use block.
    let body2 = reqs[1].body_json();
    let assistant = body2["messages"]
        .as_array()
        .expect("messages array")
        .iter()
        .find(|m| m["role"] == "assistant")
        .expect("assistant message on turn 2");
    let blocks = assistant["content"].as_array().expect("content blocks");
    assert_eq!(blocks.len(), 3);
    assert_eq!(
        blocks[0],
        serde_json::json!({"type": "thinking", "thinking": "Let me analyze.", "signature": "sig_abc123"}),
        "thinking block must be echoed unmodified",
    );
    assert_eq!(
        blocks[1],
        serde_json::json!({"type": "redacted_thinking", "data": "eJxT_opaque"}),
        "redacted_thinking block must be echoed unmodified",
    );
    assert_eq!(blocks[2]["type"], "tool_use");

    // Byte-identical: extract the wire's thinking + redacted_thinking blocks
    // and serialize them compactly — the adapter re-emits the exact
    // serde_json values captured at parse time, so the compact serialization
    // (key order and content) must match the artifact byte-for-byte. (The raw
    // wire body is pretty-printed by ureq's send_json, so whitespace is
    // normalized here; the VALUES are the same objects.)
    let wire_artifact = serde_json::to_vec(&blocks[..2]).expect("serialize blocks");
    assert_eq!(
        wire_artifact, EXPECTED_ARTIFACT,
        "thinking blocks must be echoed byte-identical",
    );
}

// ── 3. Gemini: thought signatures re-emitted ─────────────────────────────

/// Gemini captures encrypted `thoughtSignature` values from thinking parts
/// (the `thought: true` marker can carry the signature on ANY part type) and
/// requires them back for reasoning continuity. The signature(s) captured on
/// turn 1 must be attached to the assistant parts of the turn-2 request.
#[ignore]
#[test]
fn gemini_thought_signatures_reemitted() {
    let tool_use_response = r#"{
        "candidates": [{
            "content": {
                "parts": [
                    {"text": "Let me think about the weather.", "thought": true, "thoughtSignature": "encrypted-sig-1"},
                    {"functionCall": {"name": "get_weather", "args": {"city": "London"}}, "thoughtSignature": "encrypted-sig-2"}
                ],
                "role": "model"
            },
            "finishReason": "STOP",
            "index": 0
        }],
        "usageMetadata": {"promptTokenCount": 10, "candidatesTokenCount": 5, "totalTokenCount": 15}
    }"#;
    let final_response = r#"{
        "candidates": [{
            "content": {"parts": [{"text": "The weather in London is 72°F and sunny."}], "role": "model"},
            "finishReason": "STOP",
            "index": 0
        }]
    }"#;

    let mock = MockProvider::start(vec![
        (200, "application/json", tool_use_response.to_string()),
        (200, "application/json", final_response.to_string()),
    ]);

    let client = GoogleClient::new(
        GoogleConfig {
            base_url: mock.base_url("v1beta"),
            streaming: false,
            retry_max_attempts: 1,
            connect_timeout_secs: 5,
            request_timeout_secs: 30,
            total_timeout_secs: 60,
            ..Default::default()
        },
        "test-key".to_string(),
    )
    .expect("google client");

    let tools = vec![tool_def()];

    // ── Turn 1: a thinking part + a function-call part, both signed ──
    let turn1 = client
        .chat_completion_turn(ChatTurnRequest {
            model: "gemini-2.5-pro",
            messages: &[user_message("What's the weather in London?")],
            tools: &tools,
            thinking_effort: "on".to_string(),
            on_retry: &mut None,
            cancel_rx: None,
            previous_response_id: None,
            tool_results: &[],
            programmatic_tool_calling: false,
        })
        .expect("turn 1");
    let ChatTurnResult::ToolUse(tool_use) = turn1 else {
        panic!("expected ToolUse on turn 1, got {turn1:?}");
    };
    assert_eq!(tool_use.tool_calls.len(), 1);
    // Both signatures captured in wire order; the thinking text stays
    // display-only (`reasoning`), never part of the assistant content.
    assert_eq!(
        tool_use.reasoning_artifact,
        Some(ReasoningArtifact::GoogleSignatures(
            br#"["encrypted-sig-1","encrypted-sig-2"]"#.to_vec()
        )),
        "thought signatures must be captured in wire order",
    );

    // ── Turn 2 (tool loop): the signatures are attached to the parts ──
    let messages = vec![
        user_message("What's the weather in London?"),
        assistant_message_from_tool_use(&tool_use),
        tool_result_message(&tool_use),
    ];
    let turn2 = client
        .chat_completion_turn(ChatTurnRequest {
            model: "gemini-2.5-pro",
            messages: &messages,
            tools: &tools,
            thinking_effort: "on".to_string(),
            on_retry: &mut None,
            cancel_rx: None,
            previous_response_id: None,
            tool_results: &[],
            programmatic_tool_calling: false,
        })
        .expect("turn 2");
    assert!(matches!(turn2, ChatTurnResult::FinalText(_)));

    // ── Assert on the wire ──
    let reqs = mock.requests();
    assert_eq!(reqs.len(), 2);
    assert_eq!(
        reqs[0].path,
        "/v1beta/models/gemini-2.5-pro:generateContent"
    );
    assert_eq!(
        reqs[1].path,
        "/v1beta/models/gemini-2.5-pro:generateContent"
    );

    // Gemini attaches the FINAL captured signature to the LAST assistant part
    // (here the functionCall part), exactly where pi attaches tool-call
    // signatures — the wire must carry the encrypted blob back verbatim.
    let body2 = reqs[1].body_json();
    let model_content = body2["contents"]
        .as_array()
        .expect("contents array")
        .iter()
        .find(|c| c["role"] == "model")
        .expect("model content on turn 2");
    let parts = model_content["parts"].as_array().expect("parts array");
    assert_eq!(parts.len(), 1, "only the functionCall part is re-emitted");
    assert_eq!(parts[0]["function_call"]["name"], "get_weather");
    assert_eq!(
        parts[0]["thoughtSignature"].as_str(),
        Some("encrypted-sig-2"),
        "the turn's final thought signature must be attached to the last part",
    );
}

// ── 4. Responses: opaque reasoning items re-emitted into input ──────────

/// OpenAI/xAI Responses providers do not echo `reasoning_content` (that field
/// is chat-completions-only); continuity flows through opaque reasoning items
/// re-emitted into `input` ahead of the assistant message, plus
/// `previous_response_id` (daemon-side, see the daemon integration tests).
#[ignore]
#[test]
fn responses_reasoning_items_reemitted_into_input() {
    let tool_use_response = r#"{
        "id": "resp_1",
        "object": "response",
        "status": "completed",
        "output": [
            {
                "type": "reasoning",
                "id": "rs_1",
                "summary": [{"type": "summary_text", "text": "deciding on tool"}]
            },
            {
                "type": "function_call",
                "id": "fc_1",
                "call_id": "call_1",
                "name": "get_weather",
                "arguments": "{\"city\":\"London\"}"
            }
        ],
        "usage": {"prompt_tokens": 10, "completion_tokens": 8, "total_tokens": 18}
    }"#;
    let final_response = r#"{
        "id": "resp_2",
        "object": "response",
        "status": "completed",
        "output": [
            {
                "type": "message",
                "id": "msg_2",
                "role": "assistant",
                "content": [{"type": "output_text", "text": "The weather in London is 72°F and sunny."}]
            }
        ],
        "usage": {"prompt_tokens": 24, "completion_tokens": 6, "total_tokens": 30}
    }"#;

    let mock = MockProvider::start(vec![
        (200, "application/json", tool_use_response.to_string()),
        (200, "application/json", final_response.to_string()),
    ]);

    let client = OpenAiClient::new(
        ServiceConfig {
            base_url: mock.base_url("v1"),
            provider_slug: "openai",
            streaming: false,
            retry_max_attempts: 1,
            connect_timeout_secs: 5,
            request_timeout_secs: 30,
            total_timeout_secs: 60,
            // gpt-5.4 is a Responses model in the catalog; the config default
            // is overridden to Responses so the path is deterministic.
            default_request_format: choreo_ai_protocols::openai::RequestFormat::Responses,
            ..Default::default()
        },
        "test-key".to_string(),
    )
    .expect("openai client");

    let tools = vec![tool_def()];

    // ── Turn 1: a reasoning output item + a function_call output item ──
    let turn1 = client
        .chat_completion_turn(ChatTurnRequest {
            model: "gpt-5.4",
            messages: &[user_message("What's the weather in London?")],
            tools: &tools,
            thinking_effort: "high".to_string(),
            on_retry: &mut None,
            cancel_rx: None,
            previous_response_id: None,
            tool_results: &[],
            programmatic_tool_calling: false,
        })
        .expect("turn 1");
    let ChatTurnResult::ToolUse(tool_use) = turn1 else {
        panic!("expected ToolUse on turn 1, got {turn1:?}");
    };
    assert_eq!(tool_use.response_id.as_deref(), Some("resp_1"));
    assert_eq!(tool_use.tool_calls[0].name, "get_weather");
    assert_eq!(
        tool_use.tool_calls[0].arguments_json,
        r#"{"city":"London"}"#
    );
    // The opaque reasoning items (with their ids) are captured for the
    // round-trip artifact.
    let ReasoningArtifact::ResponsesItems(bytes) = tool_use
        .reasoning_artifact
        .as_ref()
        .expect("responses artifact")
    else {
        panic!("expected ResponsesItems artifact");
    };
    let items: Vec<serde_json::Value> =
        serde_json::from_slice(bytes).expect("artifact is JSON items");
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["type"], "reasoning");
    assert_eq!(items[0]["id"], "rs_1");

    // ── Turn 2 (tool loop): reasoning items re-emitted into `input` ──
    let messages = vec![
        user_message("What's the weather in London?"),
        assistant_message_from_tool_use(&tool_use),
        tool_result_message(&tool_use),
    ];
    let turn2 = client
        .chat_completion_turn(ChatTurnRequest {
            model: "gpt-5.4",
            messages: &messages,
            tools: &tools,
            thinking_effort: "high".to_string(),
            on_retry: &mut None,
            cancel_rx: None,
            previous_response_id: Some("resp_1"),
            tool_results: &[],
            programmatic_tool_calling: false,
        })
        .expect("turn 2");
    assert!(matches!(turn2, ChatTurnResult::FinalText(_)));

    // ── Assert on the wire ──
    let reqs = mock.requests();
    assert_eq!(reqs.len(), 2);
    assert_eq!(reqs[1].path, "/v1/responses");

    let body2 = reqs[1].body_json();
    assert_eq!(
        body2["previous_response_id"].as_str(),
        Some("resp_1"),
        "responses continuity chains via previous_response_id",
    );
    // The captured reasoning item is re-emitted into `input` verbatim. For a
    // tool-use turn the assistant message carries no text content, so the
    // input is: user message → replayed reasoning item → function_call_output
    // (the turn's tool result) — the reasoning item rides ahead of the
    // content that follows it, mirroring the provider's output ordering.
    let input = body2["input"].as_array().expect("input items");
    assert_eq!(input[0]["type"], "message");
    assert_eq!(input[0]["role"], "user");
    let reasoning_idx = input
        .iter()
        .position(|item| item["type"] == "reasoning" && item["id"] == "rs_1")
        .expect("reasoning item in input");
    let output_idx = input
        .iter()
        .position(|item| item["type"] == "function_call_output")
        .expect("tool result in input");
    assert!(
        reasoning_idx < output_idx,
        "reasoning item must precede the turn's tool result",
    );
    assert_eq!(
        input[reasoning_idx]["summary"][0]["text"], "deciding on tool",
        "opaque reasoning items re-emitted verbatim",
    );
}

// ── opencode x-opencode-session header tests ─────────────────────────────

/// Drive one non-streaming chat completion turn against the mock provider and
/// return the captured request.
fn chat_turn_request(config: ServiceConfig) -> CapturedRequest {
    let response = r#"{
        "choices":[{"message":{"content":"hello","tool_calls":[],"reasoning_content":null,"reasoning":null,"reasoning_text":null}}],
        "usage":{"prompt_tokens":1,"completion_tokens":1,"total_tokens":2}
    }"#;
    let mock = MockProvider::start(vec![(200, "application/json", response.to_string())]);

    let client = OpenAiClient::new(
        ServiceConfig {
            base_url: mock.base_url("v1"),
            retry_max_attempts: 1,
            ..config
        },
        "test-key".to_string(),
    )
    .expect("openai client");

    client
        .completion("deepseek-v4-flash", "hi")
        .expect("completion");
    mock.requests().into_iter().next().expect("one request")
}

#[test]
#[ignore]
fn opencode_provider_sends_x_opencode_session_header() {
    // opencode-go (the go tier) and opencode (zen) both send the fixed session
    // header so the gateway routes to a stable, working upstream provider.
    for slug in ["opencode-go", "opencode"] {
        let req = chat_turn_request(ServiceConfig {
            provider_slug: slug,
            streaming: false,
            ..Default::default()
        });
        assert_eq!(
            req.header("x-opencode-session"),
            Some("choreographr"),
            "slug {slug} must send x-opencode-session: choreographr"
        );
    }
}

#[test]
#[ignore]
fn non_opencode_provider_omits_x_opencode_session_header() {
    // Providers that aren't opencode gateways must not get the header — the
    // header is only meaningful to opencode.ai's routing.
    for slug in ["openai", "deepseek"] {
        let req = chat_turn_request(ServiceConfig {
            provider_slug: slug,
            streaming: false,
            ..Default::default()
        });
        assert_eq!(
            req.header("x-opencode-session"),
            None,
            "slug {slug} must not send x-opencode-session"
        );
    }
}
