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

use choreo_ai_protocols::anthropic::{AnthropicClient, AnthropicConfig};
use choreo_ai_protocols::google::{GoogleClient, GoogleConfig};
use choreo_ai_protocols::openai::{
    AssistantToolCall, AssistantToolFunction, ChatRequestMessage, ChatToolDefinition,
    MaxTokensField, OpenAiClient, ServiceConfig,
};
use choreo_ai_protocols::test_utils::{CapturedRequest, MockProvider};
use choreo_ai_protocols::{ChatAssistantToolUse, ChatTurnRequest, ChatTurnResult};
use choreo_proto::{ChatReasoningField, ReasoningArtifact};

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
        images: Vec::new(),
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
        images: Vec::new(),
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
            provider_slug: "deepseek".to_string(),
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
            session_id: "42".to_string(),
            request_id: "7".to_string(),
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
        Some(ReasoningArtifact::ChatReasoning {
            field: ChatReasoningField::ReasoningContent,
            bytes: REASONING.as_bytes().to_vec(),
        }),
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
            session_id: "42".to_string(),
            request_id: "7".to_string(),
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
            session_id: "42".to_string(),
            request_id: "7".to_string(),
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
            session_id: "42".to_string(),
            request_id: "7".to_string(),
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
            session_id: "42".to_string(),
            request_id: "7".to_string(),
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
            session_id: "42".to_string(),
            request_id: "7".to_string(),
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

// ── 4. Responses: reasoning continuity via previous_response_id ────────

/// OpenAI/xAI Responses providers do not echo `reasoning_content` (that field
/// is chat-completions-only); reasoning continuity flows through the server-
/// side chain (`previous_response_id`) plus the opaque reasoning items, which
/// are captured into the round-trip artifact and re-emitted into `input` only
/// on non-chained turns (daemon-side, see the daemon integration tests).
#[ignore]
#[test]
fn responses_chains_reasoning_continuity_via_response_id() {
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
            provider_slug: "openai".to_string(),
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
            session_id: "42".to_string(),
            request_id: "7".to_string(),
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

    // ── Turn 2 (fresh user turn chaining onto resp_1) ──
    // The new request chains continuity via `previous_response_id`: the server
    // retains the full conversation (including the reasoning items) up to the
    // last response, so `input` must carry ONLY the messages that postdate
    // the last assistant message — the new user message. Resending the full
    // history would duplicate every prior turn on top of the chained context.
    let messages = vec![
        user_message("What's the weather in London?"),
        assistant_message_from_tool_use(&tool_use),
        tool_result_message(&tool_use),
        // The final-text assistant turn (what resp_1's chain ended with).
        ChatRequestMessage::simple(
            "assistant",
            "The weather in London is 72°F and sunny.".to_string(),
        ),
        user_message("What about Paris?"),
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
            session_id: "42".to_string(),
            request_id: "7".to_string(),
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
    // The chained input is minimal: just the new user message. The old turns
    // (and their reasoning items) are NOT replayed — the server already has
    // them in resp_1's context, and re-sending them would duplicate them.
    let input = body2["input"].as_array().expect("input items");
    assert_eq!(
        input.len(),
        1,
        "chained turn sends only the new user message"
    );
    assert_eq!(input[0]["type"], "message");
    assert_eq!(input[0]["role"], "user");
    assert_eq!(input[0]["content"], "What about Paris?");
    assert!(
        input.iter().all(|item| item["type"] != "reasoning"),
        "reasoning items stay in the chained context; they must not be re-sent",
    );
}

// ── opencode gateway header tests ────────────────────────────────────────────

/// Drive one non-streaming chat completion turn against the mock provider and
/// return the captured request. Uses the real turn path (not the prompt API),
/// because only turns carry the session identity that feeds the gateway
/// routing headers.
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
        .chat_completion_turn(ChatTurnRequest {
            model: "deepseek-v4-flash",
            messages: &[user_message("hi")],
            tools: &[],
            thinking_effort: "off".to_string(),
            on_retry: &mut None,
            cancel_rx: None,
            previous_response_id: None,
            tool_results: &[],
            programmatic_tool_calling: false,
            session_id: "18446744073709551615".to_string(),
            request_id: "7".to_string(),
        })
        .expect("turn");
    mock.requests().into_iter().next().expect("one request")
}

#[test]
#[ignore]
fn opencode_provider_sends_per_session_gateway_headers() {
    // opencode-go (the go tier) and opencode (zen) route each turn by hashing
    // the per-session sticky id, so the headers must carry the REAL session
    // and request ids from the turn — never a fixed constant (a constant would
    // pin every choreographr session to one upstream bucket) — plus the client
    // identifier, mirroring upstream's own client.
    for slug in ["opencode-go", "opencode"] {
        let req = chat_turn_request(ServiceConfig {
            provider_slug: slug.to_string(),
            streaming: false,
            ..Default::default()
        });
        assert_eq!(
            req.header("x-opencode-session"),
            Some("18446744073709551615"),
            "slug {slug} must send x-opencode-session with the turn's real session id"
        );
        assert_eq!(
            req.header("x-opencode-request"),
            Some("7"),
            "slug {slug} must send x-opencode-request with the turn's request id"
        );
        assert_eq!(
            req.header("x-opencode-client"),
            Some("choreographr"),
            "slug {slug} must send x-opencode-client: choreographr"
        );
    }
}

#[test]
#[ignore]
fn user_agent_from_config_is_sent_on_inference_requests() {
    // The daemon sets "choreographr/<version>" on every client config (see
    // providers::from_account_config); build_agent must put it on the wire in
    // place of ureq's generic default, for all providers.
    let req = chat_turn_request(ServiceConfig {
        provider_slug: "openai".to_string(),
        streaming: false,
        user_agent: Some("choreographr/9.9.9-test".to_string()),
        ..Default::default()
    });
    assert_eq!(
        req.header("user-agent"),
        Some("choreographr/9.9.9-test"),
        "inference requests must carry the configured User-Agent"
    );
}

#[test]
#[ignore]
fn non_opencode_provider_omits_x_opencode_session_header() {
    // Providers that aren't opencode gateways must not get the headers — the
    // gateway routing semantics only apply to opencode.ai endpoints.
    for slug in ["openai", "deepseek"] {
        let req = chat_turn_request(ServiceConfig {
            provider_slug: slug.to_string(),
            streaming: false,
            ..Default::default()
        });
        assert_eq!(
            req.header("x-opencode-session"),
            None,
            "slug {slug} must not send x-opencode-session"
        );
        assert_eq!(
            req.header("x-opencode-request"),
            None,
            "slug {slug} must not send x-opencode-request"
        );
    }
}
