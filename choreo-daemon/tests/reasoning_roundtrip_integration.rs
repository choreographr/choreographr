//! Daemon-side reasoning round-trip integration tests (phase 6): the builder
//! policy that decides *whether* a captured reasoning artifact is replayed,
//! exercised against a real `SessionState` and — for the model-switch case —
//! verified on the actual wire via a mock OpenAI-compatible provider.
//!
//! The builder itself (`build_chat_request_messages`) is exposed behind the
//! `test-utils` feature (enabled for test builds via the self dev-dependency);
//! the per-adapter capture → re-emit wire round-trips live in
//! `choreo-ai-protocols/tests/reasoning_roundtrip.rs`.
//!
//! These tests bind a real local TCP socket (the mock provider), so per
//! AGENTS.md they live in `tests/` and are marked `#[ignore]` (run via
//! `cargo test-integration`).

use std::collections::VecDeque;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::{Arc, Mutex};

use choreo_ai_protocols::ChatTurnRequest;
use choreo_ai_protocols::openai::{
    ChatRequestMessage, MaxTokensField, OpenAiClient, ServiceConfig,
};
use choreo_proto::{AssistantToolCallRecord, ReasoningArtifact, ReasoningProducer};
use choreographr::{SessionState, build_chat_request_messages};

// ── Scripted mock provider (see choreo-ai-protocols tests for the same
//    helper — duplicated here because integration test crates cannot share
//    code across workspace members) ──────────────────────────────────────

/// One HTTP request captured by the mock provider.
#[derive(Debug, Clone)]
struct CapturedRequest {
    path: String,
    body: Vec<u8>,
}

impl CapturedRequest {
    fn body_json(&self) -> serde_json::Value {
        serde_json::from_slice(&self.body).expect("captured request body is JSON")
    }
}

/// Serves one canned response per request (last entry repeats) and records
/// every request body. Speaks just enough HTTP/1.1 for the ureq agent.
struct MockProvider {
    addr: std::net::SocketAddr,
    captured: Arc<Mutex<Vec<CapturedRequest>>>,
    _handle: std::thread::JoinHandle<()>,
}

impl MockProvider {
    fn start(responses: Vec<(u16, String)>) -> Self {
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

                let mut buf = Vec::new();
                let mut tmp = [0u8; 4096];
                let head_end = loop {
                    let n = stream.read(&mut tmp).unwrap_or(0);
                    if n == 0 {
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
                let path = head.split_whitespace().nth(1).unwrap_or("").to_string();
                captured_thread
                    .lock()
                    .unwrap()
                    .push(CapturedRequest { path, body });

                let (status, response_body) = if responses.len() > 1 {
                    responses.pop_front().expect("scripted response")
                } else {
                    responses.front().cloned().expect("scripted response")
                };
                let reason = if status == 200 { "OK" } else { "Error" };
                let head = format!(
                    "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
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

// ── Fixtures ─────────────────────────────────────────────────────────────

/// Append a tool-call turn produced by the given provider+model, carrying a
/// `ChatReasoning` artifact — exactly what `set_assistant_response` records
/// when the agent loop captures a DeepSeek/Kimi-style `reasoning_content`.
fn add_tool_turn(
    session: &mut SessionState,
    user_text: &str,
    reasoning: &str,
    provider_slug: &str,
    model: &str,
) {
    let (tid, _) = session.start_turn(Some(user_text.to_string()));
    session.set_assistant_response(
        tid,
        None,
        Some(reasoning.to_string()),
        vec![AssistantToolCallRecord {
            call_id: "call_1".into(),
            name: "get_weather".into(),
            arguments_json: r#"{"city":"London"}"#.into(),
        }],
        None,
        Some(ReasoningArtifact::ChatReasoning(
            reasoning.as_bytes().to_vec(),
        )),
        Some(ReasoningProducer {
            provider_slug: provider_slug.to_string(),
            model: model.to_string(),
        }),
    );
}

fn assistant_messages(messages: &[ChatRequestMessage]) -> Vec<&ChatRequestMessage> {
    messages.iter().filter(|m| m.role == "assistant").collect()
}

/// A final-text chat completions response, so the mock can complete a turn.
fn final_text_response() -> String {
    r#"{
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
        "usage": {"prompt_tokens": 12, "completion_tokens": 4, "total_tokens": 16}
    }"#
    .to_string()
}

// ── Model-switch tests ───────────────────────────────────────────────────

/// After switching models mid-session, the builder must drop every old turn's
/// artifact: artifacts are model-bound (per-turn `reasoning_producer`), so a
/// turn produced by `deepseek-v4-pro` must not replay its payload into a
/// request for `deepseek-chat` — even though both are tool-involving turns
/// under a ToolLoop-passback provider.
#[ignore]
#[test]
fn builder_model_switch_drops_old_turn_artifacts() {
    let mut session = SessionState::empty();
    add_tool_turn(
        &mut session,
        "What's the weather?",
        "reasoning one",
        "deepseek",
        "deepseek-v4-pro",
    );
    add_tool_turn(
        &mut session,
        "And tomorrow?",
        "reasoning two",
        "deepseek",
        "deepseek-v4-pro",
    );

    // Control — same model as the producing turns: the ToolLoop policy
    // replays both artifacts on the tool-involving assistant messages.
    let same = build_chat_request_messages(&session, None, "deepseek", "deepseek-v4-pro");
    let same_assistants = assistant_messages(&same);
    assert_eq!(same_assistants.len(), 2);
    assert!(
        same_assistants
            .iter()
            .all(|m| m.reasoning_artifact.is_some()),
        "same-model turns must keep their artifacts",
    );

    // After a mid-session model switch (deepseek-v4-pro → deepseek-chat):
    // every old turn's artifact is dropped, so nothing is replayed.
    let switched = build_chat_request_messages(&session, None, "deepseek", "deepseek-chat");
    let switched_assistants = assistant_messages(&switched);
    assert_eq!(switched_assistants.len(), 2);
    assert!(
        switched_assistants
            .iter()
            .all(|m| m.reasoning_artifact.is_none()),
        "artifacts produced under the previous model must be dropped after a switch",
    );
}

/// The model-switch behavior verified on the wire: build the messages with a
/// real `SessionState`, send them to a mock provider, and assert the mock
/// sees `reasoning_content` on the same-model request but NO reasoning on the
/// request sent after the switch.
#[ignore]
#[test]
fn model_switch_sends_no_reasoning_on_the_wire() {
    const REASONING: &str = "deepseek old reasoning payload";

    let mut session = SessionState::empty();
    add_tool_turn(
        &mut session,
        "What's the weather?",
        REASONING,
        "deepseek",
        "deepseek-v4-pro",
    );
    add_tool_turn(
        &mut session,
        "And tomorrow?",
        "more old reasoning",
        "deepseek",
        "deepseek-v4-pro",
    );

    let mock = MockProvider::start(vec![
        (200, final_text_response()),
        (200, final_text_response()),
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

    let send = |messages: &[ChatRequestMessage]| {
        client
            .chat_completion_turn(ChatTurnRequest {
                model: "deepseek-chat",
                messages,
                tools: &[],
                thinking_effort: "high".to_string(),
                on_retry: &mut None,
                cancel_rx: None,
                previous_response_id: None,
                tool_results: &[],
                programmatic_tool_calling: false,
            })
            .expect("turn succeeds");
    };

    // ── Request 1: same model as the producing turns → artifacts replayed ──
    let same = build_chat_request_messages(&session, None, "deepseek", "deepseek-v4-pro");
    send(&same);
    let requests = mock.requests();
    assert_eq!(requests[0].path, "/v1/chat/completions");
    let body1 = requests[0].body_json();
    let echoed: Vec<&serde_json::Value> = body1["messages"]
        .as_array()
        .expect("messages array")
        .iter()
        .filter(|m| m["role"] == "assistant")
        .collect();
    assert_eq!(echoed.len(), 2);
    for assistant in echoed {
        assert!(
            assistant.get("reasoning_content").is_some(),
            "same-model tool-loop request must echo reasoning_content: {assistant}",
        );
    }
    assert_eq!(
        body1["messages"].as_array().expect("messages array")[1]["reasoning_content"].as_str(),
        Some(REASONING),
        "the captured reasoning must be echoed verbatim",
    );

    // ── Request 2: after switching to deepseek-chat → nothing replayed ──
    let switched = build_chat_request_messages(&session, None, "deepseek", "deepseek-chat");
    send(&switched);
    let requests = mock.requests();
    assert_eq!(requests[1].path, "/v1/chat/completions");
    let body2 = requests[1].body_json();
    let assistants: Vec<&serde_json::Value> = body2["messages"]
        .as_array()
        .expect("messages array")
        .iter()
        .filter(|m| m["role"] == "assistant")
        .collect();
    assert_eq!(assistants.len(), 2);
    for assistant in assistants {
        assert!(
            assistant.get("reasoning_content").is_none(),
            "after a model switch the wire must not replay old reasoning: {assistant}",
        );
    }
}
