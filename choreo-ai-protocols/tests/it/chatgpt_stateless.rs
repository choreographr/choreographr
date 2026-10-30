//! The stateless ChatGPT/Codex tool loop, end to end over the wire.
//!
//! The `chatgpt` provider is the stateless Responses dialect: every request
//! carries the WHOLE conversation, `store` is off, nothing chains through
//! `previous_response_id`, and the system prompt rides the top-level
//! `instructions` string instead of a `role: "system"` input item — which the
//! real Codex backend rejects outright.
//!
//! This test drives a real two-iteration tool loop against a mock backend that
//! enforces exactly the two rejections the real backend returns, then asserts on
//! the captured request bodies that the second request replayed the full history
//! (the assistant's tool call plus the `function_call_output` answering it). It
//! is the wire-level counterpart of the daemon's tool loop, which rebuilds the
//! whole conversation from the session turns on every iteration.
//!
//! The mock is the crate's shared [`MockProvider`] in its per-request mode, so
//! the rejection is a property of the REQUEST rather than of arrival order: a
//! body the backend would refuse can never be answered as a success, and a
//! refused request cannot consume the turn a later legal one needs.
//!
//! The test binds a real local TCP socket, so per AGENTS.md it lives in
//! `tests/` and is marked `#[ignore]` (run via `cargo test-integration`).

// AGENTS.md permits unwrap/expect/panic in tests/ files, but clippy's
// allow-*-in-tests config only recognizes #[test]-annotated functions —
// helper fns in this file need this file-level allowance.
#![expect(clippy::expect_used)]

use std::fmt::Write as _;

use choreo_ai_protocols::openai::{
    AssistantToolCall, AssistantToolFunction, ChatRequestMessage, ChatToolDefinition, OpenAiClient,
    RequestAuth, RequestFormat, ServiceConfig,
};
use choreo_ai_protocols::test_utils::{CapturedRequest, MockProvider};
use choreo_ai_protocols::{ChatTurnRequest, ChatTurnResult, SocketRegistry};

/// The system prompt hoisted into `instructions`.
const SYSTEM_PROMPT: &str = "be helpful";
/// The user turn that opens the conversation.
const USER_PROMPT: &str = "weather in Paris?";
/// The tool result the daemon appends after executing the call.
const TOOL_OUTPUT: &str = "72F";
/// The call id the first response names and the second request answers.
const TOOL_CALL_ID: &str = "call_1";

/// What the real Codex backend answers when a `role: "system"` item arrives in
/// `input`: the system prompt has to be the top-level `instructions` string.
const SYSTEM_ITEM_REJECTION: &str = r#"{"detail":"System messages are not allowed"}"#;
/// What it answers to a chain request: it keeps no server-side transcript, so
/// `previous_response_id` is not a thing it honours.
const CHAIN_REJECTION: &str = r#"{"detail":"previous_response_id is not supported"}"#;

// ── Mock Codex backend ───────────────────────────────────────────────────

/// A loopback stand-in for the stateless ChatGPT/Codex Responses endpoint.
///
/// Serves one turn per request, but only after applying the two rejections the
/// real backend applies — a request the real backend would 400 must never look
/// like a success here, or the test would pass on a body the provider refuses.
struct CodexMock {
    provider: MockProvider,
}

impl CodexMock {
    fn start() -> Self {
        Self {
            provider: MockProvider::start_responding(respond),
        }
    }

    /// The API root to point a client at (`…/v1`, matching the default paths).
    fn base_url(&self) -> String {
        self.provider.base_url("v1")
    }

    /// Every request body the backend received, in arrival order.
    fn bodies(&self) -> Vec<serde_json::Value> {
        self.provider
            .requests()
            .iter()
            .map(CapturedRequest::body_json)
            .collect()
    }
}

/// The backend's verdict on a request: the turn it serves, or the 400 body it
/// refuses the request with.
fn respond(request: &CapturedRequest) -> (u16, &'static str, String) {
    // A body that is not JSON is refused rather than panicking the serve thread.
    let Ok(body) = serde_json::from_slice::<serde_json::Value>(&request.body) else {
        return (
            400,
            "application/json",
            r#"{"detail":"body is not JSON"}"#.to_string(),
        );
    };
    if let Some(rejection) = rejection(&body) {
        return (400, "application/json", rejection.to_string());
    }
    // Which turn to serve is a property of the request, not of how many have
    // arrived: once the conversation answers a tool call, the model's next turn
    // is the final answer.
    if answers_a_tool_call(&body) {
        (200, "text/event-stream", final_text_stream())
    } else {
        (200, "text/event-stream", tool_call_stream())
    }
}

/// Whether the request's `input` carries a tool result — i.e. the conversation
/// has already made a call the model now has the answer to.
fn answers_a_tool_call(body: &serde_json::Value) -> bool {
    body.get("input")
        .and_then(serde_json::Value::as_array)
        .is_some_and(|items| {
            items.iter().any(|item| {
                item.get("type").and_then(serde_json::Value::as_str) == Some("function_call_output")
            })
        })
}

/// The backend's verdict on a request body: the 400 body it answers with, or
/// `None` when the request is one it would serve.
fn rejection(body: &serde_json::Value) -> Option<&'static str> {
    // A `role: "system"` input item is refused: the system prompt belongs in
    // the top-level `instructions` field.
    let carries_system_item = body
        .get("input")
        .and_then(serde_json::Value::as_array)
        .is_some_and(|items| {
            items
                .iter()
                .any(|item| item.get("role").and_then(serde_json::Value::as_str) == Some("system"))
        });
    if carries_system_item {
        return Some(SYSTEM_ITEM_REJECTION);
    }
    // A non-null `previous_response_id` is refused: there is no stored
    // transcript to chain onto.
    if body
        .get("previous_response_id")
        .is_some_and(|id| !id.is_null())
    {
        return Some(CHAIN_REJECTION);
    }
    None
}

/// Frame an ordered list of SSE `data:` payloads into one response body.
fn sse(events: &[serde_json::Value]) -> String {
    let mut body = String::new();
    for event in events {
        let _ = writeln!(body, "data: {event}\n");
    }
    body.push_str("data: [DONE]\n\n");
    body
}

/// The first response: one `get_weather` tool call, then completion.
fn tool_call_stream() -> String {
    sse(&[
        // The complete arguments string — the done event REPLACES whatever the
        // delta events accumulated, so it is what the accumulator reports.
        serde_json::json!({
            "type": "response.function_call_arguments.done",
            "item_id": "fc_1",
            "output_index": 0,
            "call_id": TOOL_CALL_ID,
            "name": "get_weather",
            "arguments": "{}",
        }),
        serde_json::json!({
            "type": "response.completed",
            "response": {
                "id": "resp_1",
                "status": "completed",
                "usage": {"input_tokens": 11, "output_tokens": 3, "total_tokens": 14},
            },
        }),
    ])
}

/// The second response: the final answer text, then completion.
fn final_text_stream() -> String {
    sse(&[
        serde_json::json!({"type": "response.output_text.delta", "delta": "done"}),
        serde_json::json!({"type": "response.output_text.done", "text": "done"}),
        serde_json::json!({
            "type": "response.completed",
            "response": {"id": "resp_2", "status": "completed"},
        }),
    ])
}

// ── Shared builders ──────────────────────────────────────────────────────

/// A `get_weather` tool definition, matching the call the mock streams.
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

/// A client on the stateless ChatGPT/Codex dialect pointed at the mock.
fn codex_client(mock: &CodexMock) -> OpenAiClient {
    OpenAiClient::new(
        ServiceConfig {
            base_url: mock.base_url(),
            provider_slug: "chatgpt".to_string(),
            streaming: true,
            // One attempt, so a captured request count is deterministic and a
            // rejected body surfaces immediately instead of being retried.
            retry_max_attempts: 1,
            connect_timeout_secs: 5,
            request_timeout_secs: 30,
            total_timeout_secs: 60,
            // The catalog records no format for an unknown model, so pin the
            // dialect the provider actually speaks.
            default_request_format: RequestFormat::Responses,
            ..Default::default()
        },
        RequestAuth::api_key("test-key"),
        &SocketRegistry::new(),
    )
    .expect("chatgpt client")
}

/// The assistant turn the daemon appends after a tool call: the call itself,
/// so the following `function_call_output` has a call to answer.
fn assistant_tool_call_message(call: &choreo_ai_protocols::ChatToolCall) -> ChatRequestMessage {
    ChatRequestMessage {
        role: "assistant",
        content: None,
        images: Vec::new(),
        tool_call_id: None,
        tool_calls: Some(vec![AssistantToolCall {
            id: call.id.clone(),
            kind: "function".to_string(),
            function: AssistantToolFunction {
                name: call.name.clone(),
                arguments: call.arguments_json.clone(),
            },
        }]),
        reasoning_artifact: None,
    }
}

/// The tool-result turn the daemon appends after executing the call.
fn tool_result_message(call_id: &str) -> ChatRequestMessage {
    ChatRequestMessage {
        role: "tool",
        content: Some(TOOL_OUTPUT.to_string()),
        images: Vec::new(),
        tool_call_id: Some(call_id.to_string()),
        tool_calls: None,
        reasoning_artifact: None,
    }
}

// ── The tool loop ────────────────────────────────────────────────────────

#[test]
#[ignore = "integration"]
fn a_two_iteration_stateless_tool_loop_replays_the_full_history() {
    let mock = CodexMock::start();
    let client = codex_client(&mock);
    let tools = vec![tool_def()];

    // ── Turn 1: the daemon's opening request — system prompt + user turn ──
    let turn1_messages = vec![
        ChatRequestMessage::simple("system", SYSTEM_PROMPT.to_string()),
        ChatRequestMessage::simple("user", USER_PROMPT.to_string()),
    ];
    let turn1 = client
        .chat_completion_turn_streaming(
            ChatTurnRequest {
                model: "gpt-5.6-luna",
                messages: &turn1_messages,
                tools: &tools,
                thinking_effort: "medium".to_string(),
                on_retry: &mut None,
                on_uploaded: None,
                cancel_rx: None,
                // The stateless dialect never chains: the full history is the
                // request.
                previous_response_id: None,
                programmatic_tool_calling: false,
                session_id: "42".to_string(),
                request_id: "7".to_string(),
                max_output_tokens_override: None,
                no_retry: false,
            },
            |_event| Ok(()),
        )
        .expect("turn 1: the stateless body is accepted by the backend");
    let ChatTurnResult::ToolUse(tool_use) = turn1 else {
        panic!("expected a tool call from turn 1, got {turn1:?}");
    };
    assert_eq!(tool_use.tool_calls.len(), 1, "{:?}", tool_use.tool_calls);
    let call = &tool_use.tool_calls[0];
    assert_eq!(call.id, TOOL_CALL_ID);
    assert_eq!(call.name, "get_weather");
    assert_eq!(call.arguments_json, "{}");

    // ── Turn 2: the full history, tool loop iteration included ──────────
    // No separate tool-result buffer exists: the daemon rebuilds the whole
    // conversation from the session turns, so the assistant call and its
    // output ride the same `messages` list as the opening turns.
    let turn2_messages = vec![
        ChatRequestMessage::simple("system", SYSTEM_PROMPT.to_string()),
        ChatRequestMessage::simple("user", USER_PROMPT.to_string()),
        assistant_tool_call_message(call),
        tool_result_message(TOOL_CALL_ID),
    ];
    let turn2 = client
        .chat_completion_turn_streaming(
            ChatTurnRequest {
                model: "gpt-5.6-luna",
                messages: &turn2_messages,
                tools: &tools,
                thinking_effort: "medium".to_string(),
                on_retry: &mut None,
                on_uploaded: None,
                cancel_rx: None,
                previous_response_id: None,
                programmatic_tool_calling: false,
                session_id: "42".to_string(),
                request_id: "8".to_string(),
                max_output_tokens_override: None,
                no_retry: false,
            },
            |_event| Ok(()),
        )
        .expect("turn 2: the replayed full history is accepted by the backend");
    let ChatTurnResult::FinalText(answer) = turn2 else {
        panic!("expected the final answer from turn 2, got {turn2:?}");
    };
    assert_eq!(answer.content, "done");

    // ── The wire: both requests carry the stateless shape ────────────────
    let bodies = mock.bodies();
    assert_eq!(
        bodies.len(),
        2,
        "exactly two requests — no retry, no rejected body"
    );
    for (index, body) in bodies.iter().enumerate() {
        // The system prompt is hoisted out of `input` into `instructions`.
        assert_eq!(
            body["instructions"], SYSTEM_PROMPT,
            "request {index}: the system prompt must ride instructions: {body}"
        );
        let input = body["input"]
            .as_array()
            .unwrap_or_else(|| panic!("request {index}: input is an item array: {body}"));
        assert!(
            input
                .iter()
                .all(|item| item.get("role").and_then(serde_json::Value::as_str) != Some("system")),
            "request {index}: no system-role item may reach the backend: {body}"
        );
        // `store: false` is explicit on the wire — the stateless contract is
        // "persist nothing", which cannot be left to a backend default.
        assert_eq!(
            body["store"], false,
            "request {index}: stateless requests must send store: false: {body}"
        );
        // Nothing chains through a server-side transcript.
        assert!(
            body.get("previous_response_id").is_none(),
            "request {index}: stateless requests must not chain: {body}"
        );
        // The encrypted reasoning payload is the only continuity a `store: false`
        // request has.
        assert_eq!(
            body["include"],
            serde_json::json!(["reasoning.encrypted_content"]),
            "request {index}: the encrypted reasoning include must be requested: {body}"
        );
    }

    // ── The wire: turn 2 replays the history, not just the new tail ──────
    let second = &bodies[1];
    let input = second["input"]
        .as_array()
        .expect("request 2: input is an item array");
    let call_item = input
        .iter()
        .position(|item| {
            item.get("type").and_then(serde_json::Value::as_str) == Some("function_call")
        })
        .unwrap_or_else(|| panic!("the assistant's tool call was never replayed: {second}"));
    let output_item = input
        .iter()
        .position(|item| {
            item.get("type").and_then(serde_json::Value::as_str) == Some("function_call_output")
        })
        .unwrap_or_else(|| panic!("the tool output was never replayed: {second}"));
    assert_eq!(
        input[output_item]["call_id"], TOOL_CALL_ID,
        "the replay answers the call it was produced for"
    );
    assert_eq!(
        input[output_item]["output"], TOOL_OUTPUT,
        "the replayed output carries the tool's result"
    );
    assert!(
        call_item < output_item,
        "the call must be introduced before the output that answers it: {second}"
    );
    assert!(
        input.iter().any(|item| {
            item.get("role").and_then(serde_json::Value::as_str) == Some("user")
                && item.get("content") == Some(&serde_json::Value::String(USER_PROMPT.to_string()))
        }),
        "the original user turn still rides the replayed history: {second}"
    );
}
