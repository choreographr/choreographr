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
//! AGENTS.md they live in `tests/` and are marked `#[ignore = "integration"]` (run via
//! `cargo test-integration`).

// clippy's allow-*-in-tests only recognizes #[test] functions; the
// helpers in this file need this file-level allowance.
#![expect(clippy::expect_used)]

use choreo_ai_protocols::ChatTurnRequest;
use choreo_ai_protocols::ChatTurnResult;
use choreo_ai_protocols::openai::{
    ChatRequestMessage, MaxTokensField, OpenAiClient, RequestAuth, ServiceConfig,
};
use choreo_ai_protocols::test_utils::MockProvider;
use choreo_daemon::{AssistantResponse, SessionState, build_chat_request_messages};
use choreo_proto::{
    AssistantToolCallRecord, ChatReasoningField, ReasoningArtifact, ReasoningProducer,
};

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
        AssistantResponse {
            reasoning: Some(reasoning.to_string()),
            tool_calls: vec![AssistantToolCallRecord {
                call_id: "call_1".into(),
                name: "get_weather".into(),
                arguments_json: r#"{"city":"London"}"#.into(),
            }],
            reasoning_artifact: Some(ReasoningArtifact::ChatReasoning {
                field: ChatReasoningField::ReasoningContent,
                bytes: reasoning.as_bytes().to_vec(),
            }),
            reasoning_producer: Some(ReasoningProducer {
                provider_slug: provider_slug.to_string(),
                model: model.to_string(),
            }),
            ..Default::default()
        },
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
#[ignore = "integration"]
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
    let same = build_chat_request_messages(&session, None, "deepseek", "deepseek-v4-pro", None);
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
    let switched = build_chat_request_messages(&session, None, "deepseek", "deepseek-chat", None);
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
/// sees the old `reasoning_content` echoed on the same-model request but NO
/// old reasoning on the request sent after the switch (`deepseek-chat` is a
/// `requires_reasoning_content` provider, so the field is still carried — as
/// an empty string placeholder — but never the previous model's payload).
#[ignore = "integration"]
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
        (200, "application/json", final_text_response()),
        (200, "application/json", final_text_response()),
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
        RequestAuth::api_key("test-key"),
        &choreo_ai_protocols::SocketRegistry::new(),
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
                on_uploaded: None,
                cancel_rx: None,
                previous_response_id: None,
                tool_results: &[],
                programmatic_tool_calling: false,
                session_id: "42".to_string(),
                request_id: "7".to_string(),
                max_output_tokens_override: None,
                no_retry: false,
            })
            .expect("turn succeeds");
    };

    // ── Request 1: same model as the producing turns → artifacts replayed ──
    let same = build_chat_request_messages(&session, None, "deepseek", "deepseek-v4-pro", None);
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
    let switched = build_chat_request_messages(&session, None, "deepseek", "deepseek-chat", None);
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
        // After a model switch the OLD reasoning payload must never be
        // replayed. `deepseek-chat` is a `requires_reasoning_content`
        // provider, so the builder still carries `reasoning_content` — as an
        // EMPTY string, because DeepSeek 400s a tool-loop turn that omits the
        // field — but never the previous model's text. Treating "absent or
        // empty" as "no old reasoning" also stays correct if a future catalog
        // change drops the field requirement for this model.
        let rc = assistant
            .get("reasoning_content")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        assert!(
            rc.is_empty(),
            "after a model switch the wire must not replay old reasoning: {assistant}",
        );
    }
}

/// Append a tool-call turn whose reasoning payload was *observed* under an
/// arbitrary chat field — the shape a gateway produces when it streams
/// `DeepSeek` thinking under `reasoning` instead of `reasoning_content`.
fn add_tool_turn_observed(
    session: &mut SessionState,
    user_text: &str,
    reasoning: &str,
    observed: ChatReasoningField,
    provider_slug: &str,
    model: &str,
) {
    let (tid, _) = session.start_turn(Some(user_text.to_string()));
    session.set_assistant_response(
        tid,
        AssistantResponse {
            reasoning: Some(reasoning.to_string()),
            tool_calls: vec![AssistantToolCallRecord {
                call_id: "call_1".into(),
                name: "get_weather".into(),
                arguments_json: r#"{"city":"London"}"#.into(),
            }],
            reasoning_artifact: Some(ReasoningArtifact::ChatReasoning {
                field: observed,
                bytes: reasoning.as_bytes().to_vec(),
            }),
            reasoning_producer: Some(ReasoningProducer {
                provider_slug: provider_slug.to_string(),
                model: model.to_string(),
            }),
            ..Default::default()
        },
    );
}

/// The live-failure shape: a route that *declares* `reasoning_content` while
/// the provider streamed the payload under the bare `reasoning` field. The
/// observation must not decide the destination — the declared field wins, and
/// the wire carries it on every assistant message. This is the session that
/// failed with "The `reasoning_content` in the thinking mode must be passed
/// back to the API".
#[ignore = "integration"]
#[test]
fn observed_other_field_never_starves_a_declared_echo_field() {
    const REASONING: &str = "reasoning that arrived under `reasoning`";

    let mut session = SessionState::empty();
    // Two turns exactly as the live gateway produced them: captured under
    // `reasoning`, produced by the model the request is for.
    add_tool_turn_observed(
        &mut session,
        "What's the weather?",
        REASONING,
        ChatReasoningField::Reasoning,
        "deepseek",
        "deepseek-v4-pro",
    );
    add_tool_turn_observed(
        &mut session,
        "And tomorrow?",
        REASONING,
        ChatReasoningField::Reasoning,
        "deepseek",
        "deepseek-v4-pro",
    );

    let mock = MockProvider::start(vec![(200, "application/json", final_text_response())]);
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
        RequestAuth::api_key("test-key"),
        &choreo_ai_protocols::SocketRegistry::new(),
    )
    .expect("openai client");

    let messages = build_chat_request_messages(&session, None, "deepseek", "deepseek-v4-pro", None);
    client
        .chat_completion_turn(ChatTurnRequest {
            model: "deepseek-v4-pro",
            messages: &messages,
            tools: &[],
            thinking_effort: "high".to_string(),
            on_retry: &mut None,
            on_uploaded: None,
            cancel_rx: None,
            previous_response_id: None,
            tool_results: &[],
            programmatic_tool_calling: false,
            session_id: "42".to_string(),
            request_id: "7".to_string(),
            max_output_tokens_override: None,
            no_retry: false,
        })
        .expect("turn succeeds");

    let requests = mock.requests();
    let body = requests[0].body_json();
    let assistants: Vec<&serde_json::Value> = body["messages"]
        .as_array()
        .expect("messages array")
        .iter()
        .filter(|m| m["role"] == "assistant")
        .collect();
    assert_eq!(assistants.len(), 2);
    for assistant in assistants {
        assert_eq!(
            assistant["reasoning_content"].as_str(),
            Some(REASONING),
            "the declared field must carry the reasoning, whatever field it arrived in: {assistant}",
        );
        assert!(
            assistant.get("reasoning").is_none(),
            "the observed field must not be echoed beside the declared one: {assistant}",
        );
    }
}

/// A reasoning-only turn whose artifact never made it into the session is
/// dropped from the request rather than sent as an empty assistant message
/// (which every provider rejects: chat servers with "must not be empty",
/// Anthropic and Gemini with an empty content/parts array). The turn stays in
/// the session, so the transcript still shows what happened.
#[ignore = "integration"]
#[test]
fn empty_assistant_turn_is_dropped_from_the_request() {
    let mut session = SessionState::empty();
    // Turn 1: a real answer, so the session has shape around the empty turn.
    add_tool_turn_observed(
        &mut session,
        "What's the weather?",
        "thinking",
        ChatReasoningField::ReasoningContent,
        "deepseek",
        "deepseek-v4-pro",
    );
    // Turn 2: reasoning-only, content empty, no tool calls, no artifact.
    let (tid, _) = session.start_turn(Some("continue".to_string()));
    session.set_assistant_response(
        tid,
        AssistantResponse {
            text: Some(String::new()),
            reasoning: Some("displayed but never captured".to_string()),
            ..Default::default()
        },
    );

    let mock = MockProvider::start(vec![(200, "application/json", final_text_response())]);
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
        RequestAuth::api_key("test-key"),
        &choreo_ai_protocols::SocketRegistry::new(),
    )
    .expect("openai client");

    let messages = build_chat_request_messages(&session, None, "deepseek", "deepseek-v4-pro", None);
    client
        .chat_completion_turn(ChatTurnRequest {
            model: "deepseek-v4-pro",
            messages: &messages,
            tools: &[],
            thinking_effort: "high".to_string(),
            on_retry: &mut None,
            on_uploaded: None,
            cancel_rx: None,
            previous_response_id: None,
            tool_results: &[],
            programmatic_tool_calling: false,
            session_id: "42".to_string(),
            request_id: "7".to_string(),
            max_output_tokens_override: None,
            no_retry: false,
        })
        .expect("turn succeeds");

    let requests = mock.requests();
    let body = requests[0].body_json();
    for message in body["messages"].as_array().expect("messages array") {
        // The invariant the projection enforces: an assistant message always
        // carries something — non-blank content or tool calls, never neither.
        if message["role"] == "assistant" {
            let has_content = message["content"]
                .as_str()
                .is_some_and(|content| !content.trim().is_empty());
            let has_tool_calls = message["tool_calls"]
                .as_array()
                .is_some_and(|calls| !calls.is_empty());
            assert!(
                has_content || has_tool_calls,
                "no empty assistant message may reach the wire: {message}",
            );
        }
    }
}

// ── Structured reasoning_details (OpenRouter / Kimi) ────────────────────

/// An `OpenRouter` route that declares `reasoning_details` (the `OpenRouter`
/// namespace dialect).
const OPENROUTER_MODEL: &str = "minimax/minimax-m2";
/// An `OpenRouter`-hosted Kimi route (bare-dialect entries + `reasoning_content`).
const KIMI_DETAILS_MODEL: &str = "moonshotai/kimi-k2.5";

fn details_artifact(entries: &serde_json::Value) -> ReasoningArtifact {
    ReasoningArtifact::ChatReasoning {
        field: ChatReasoningField::ReasoningDetails,
        bytes: serde_json::to_vec(entries).expect("serialize details"),
    }
}

/// Append a tool-call turn carrying an arbitrary reasoning artifact, produced by
/// the given provider+model (so the provenance gate accepts it).
fn add_artifact_tool_turn(
    session: &mut SessionState,
    user_text: &str,
    artifact: ReasoningArtifact,
    provider_slug: &str,
    model: &str,
) {
    let (tid, _) = session.start_turn(Some(user_text.to_string()));
    session.set_assistant_response(
        tid,
        AssistantResponse {
            tool_calls: vec![AssistantToolCallRecord {
                call_id: "call_1".into(),
                name: "get_weather".into(),
                arguments_json: r#"{"city":"London"}"#.into(),
            }],
            reasoning_artifact: Some(artifact),
            reasoning_producer: Some(ReasoningProducer {
                provider_slug: provider_slug.to_string(),
                model: model.to_string(),
            }),
            ..Default::default()
        },
    );
}

fn openrouter_client(mock: &MockProvider) -> OpenAiClient {
    OpenAiClient::new(
        ServiceConfig {
            base_url: mock.base_url("v1"),
            provider_slug: "openrouter".to_string(),
            streaming: false,
            retry_max_attempts: 1,
            connect_timeout_secs: 5,
            request_timeout_secs: 30,
            total_timeout_secs: 60,
            chat_completions_max_tokens_field: MaxTokensField::MaxCompletionTokens,
            ..Default::default()
        },
        RequestAuth::api_key("test-key"),
        &choreo_ai_protocols::SocketRegistry::new(),
    )
    .expect("openai client")
}

fn send(client: &OpenAiClient, model: &str, messages: &[ChatRequestMessage]) -> ChatTurnResult {
    client
        .chat_completion_turn(ChatTurnRequest {
            model,
            messages,
            tools: &[],
            thinking_effort: "high".to_string(),
            on_retry: &mut None,
            on_uploaded: None,
            cancel_rx: None,
            previous_response_id: None,
            tool_results: &[],
            programmatic_tool_calling: false,
            session_id: "42".to_string(),
            request_id: "7".to_string(),
            max_output_tokens_override: None,
            no_retry: false,
        })
        .expect("turn succeeds")
}

/// The single assistant message on a captured chat request body.
fn captured_assistant(body: &serde_json::Value) -> serde_json::Value {
    body["messages"]
        .as_array()
        .expect("messages array")
        .iter()
        .find(|message| message["role"] == "assistant")
        .cloned()
        .expect("an assistant message")
}

/// The `OpenRouter` dialect is replayed verbatim — index, format and signature
/// intact — and no scalar reasoning field is invented beside the array.
#[ignore = "integration"]
#[test]
fn openrouter_details_are_replayed_verbatim() {
    let entries = serde_json::json!([{
        "type":"reasoning.text",
        "text":"think it through",
        "signature":"sig_abc",
        "format":"openai-responses-v1",
        "index":0
    }]);
    let mut session = SessionState::empty();
    add_artifact_tool_turn(
        &mut session,
        "go",
        details_artifact(&entries),
        "openrouter",
        OPENROUTER_MODEL,
    );

    let mock = MockProvider::start(vec![(200, "application/json", final_text_response())]);
    let client = openrouter_client(&mock);
    let messages =
        build_chat_request_messages(&session, None, "openrouter", OPENROUTER_MODEL, None);
    send(&client, OPENROUTER_MODEL, &messages);

    let assistant = captured_assistant(&mock.requests()[0].body_json());
    assert_eq!(assistant["reasoning_details"], entries);
    for scalar in ["reasoning_content", "reasoning", "reasoning_text"] {
        assert!(
            assistant.get(scalar).is_none(),
            "no scalar may be invented beside reasoning_details: {assistant}"
        );
    }
}

/// A persisted Kimi-shaped payload keeps a stream `index`; replay rebuilds it
/// minimally (index stripped) and mirrors the summary text as
/// `reasoning_content`.
#[ignore = "integration"]
#[test]
fn kimi_details_replay_strips_index_and_adds_companion() {
    let entries = serde_json::json!([
        {"type":"summary","summary":"step one ","index":0},
        {"type":"summary","summary":"and two","index":0}
    ]);
    let mut session = SessionState::empty();
    add_artifact_tool_turn(
        &mut session,
        "go",
        details_artifact(&entries),
        "openrouter",
        KIMI_DETAILS_MODEL,
    );

    let mock = MockProvider::start(vec![(200, "application/json", final_text_response())]);
    let client = openrouter_client(&mock);
    let messages =
        build_chat_request_messages(&session, None, "openrouter", KIMI_DETAILS_MODEL, None);
    send(&client, KIMI_DETAILS_MODEL, &messages);

    let assistant = captured_assistant(&mock.requests()[0].body_json());
    assert_eq!(
        assistant["reasoning_details"],
        serde_json::json!([
            {"type":"summary","summary":"step one "},
            {"type":"summary","summary":"and two"}
        ]),
        "bare Kimi entries are rebuilt minimally without the stream index"
    );
    // The merged summary text rides along as the scalar companion.
    assert_eq!(assistant["reasoning_content"], "step one and two");
}

/// An unrecognised detail `type` is dropped at replay rather than forwarded.
#[ignore = "integration"]
#[test]
fn unknown_details_type_is_dropped_on_replay() {
    let entries = serde_json::json!([
        {"type":"reasoning.text","text":"keep","index":0},
        {"type":"vendor.mystery","payload":"drop me"}
    ]);
    let mut session = SessionState::empty();
    add_artifact_tool_turn(
        &mut session,
        "go",
        details_artifact(&entries),
        "openrouter",
        OPENROUTER_MODEL,
    );

    let mock = MockProvider::start(vec![(200, "application/json", final_text_response())]);
    let client = openrouter_client(&mock);
    let messages =
        build_chat_request_messages(&session, None, "openrouter", OPENROUTER_MODEL, None);
    send(&client, OPENROUTER_MODEL, &messages);

    let assistant = captured_assistant(&mock.requests()[0].body_json());
    assert_eq!(
        assistant["reasoning_details"],
        serde_json::json!([{"type":"reasoning.text","text":"keep","index":0}])
    );
}

/// A details-declaring route injects `reasoning_details: []` on an assistant
/// message with nothing to echo, and an empty assistant message is still dropped
/// (`[]` is a presence marker, not a payload).
#[ignore = "integration"]
#[test]
fn empty_details_is_present_but_does_not_keep_an_empty_message_alive() {
    let mut session = SessionState::empty();
    // Turn 1: a tool turn with no reasoning at all — must carry `[]`.
    let (tid, _) = session.start_turn(Some("first".to_string()));
    session.set_assistant_response(
        tid,
        AssistantResponse {
            tool_calls: vec![AssistantToolCallRecord {
                call_id: "call_1".into(),
                name: "get_weather".into(),
                arguments_json: r#"{"city":"London"}"#.into(),
            }],
            reasoning_producer: Some(ReasoningProducer {
                provider_slug: "openrouter".into(),
                model: OPENROUTER_MODEL.into(),
            }),
            ..Default::default()
        },
    );
    // Turn 2: a wholly empty assistant turn — dropped from the projection.
    let (empty_tid, _) = session.start_turn(Some("second".to_string()));
    session.set_assistant_response(
        empty_tid,
        AssistantResponse {
            text: Some(String::new()),
            ..Default::default()
        },
    );

    let mock = MockProvider::start(vec![(200, "application/json", final_text_response())]);
    let client = openrouter_client(&mock);
    let messages =
        build_chat_request_messages(&session, None, "openrouter", OPENROUTER_MODEL, None);
    send(&client, OPENROUTER_MODEL, &messages);

    let body = mock.requests()[0].body_json();
    let assistants: Vec<&serde_json::Value> = body["messages"]
        .as_array()
        .expect("messages array")
        .iter()
        .filter(|message| message["role"] == "assistant")
        .collect();
    assert_eq!(
        assistants.len(),
        1,
        "the empty assistant turn must be dropped: {body}"
    );
    assert_eq!(assistants[0]["reasoning_details"], serde_json::json!([]));
    assert!(assistants[0].get("reasoning_content").is_none());
}

/// A details artifact produced by a different model is dropped by the
/// provenance gate — but the turn itself survives (its content/tool calls are
/// intact), carrying the injected empty array.
#[ignore = "integration"]
#[test]
fn provenance_mismatch_drops_details_but_keeps_the_turn() {
    let entries = serde_json::json!([{"type":"reasoning.text","text":"stale","index":0}]);
    let mut session = SessionState::empty();
    add_artifact_tool_turn(
        &mut session,
        "go",
        details_artifact(&entries),
        "openai",
        "gpt-4",
    );

    let mock = MockProvider::start(vec![(200, "application/json", final_text_response())]);
    let client = openrouter_client(&mock);
    let messages =
        build_chat_request_messages(&session, None, "openrouter", OPENROUTER_MODEL, None);
    send(&client, OPENROUTER_MODEL, &messages);

    let assistant = captured_assistant(&mock.requests()[0].body_json());
    assert_eq!(
        assistant["reasoning_details"],
        serde_json::json!([]),
        "a foreign artifact must not be replayed"
    );
    assert!(
        assistant["tool_calls"]
            .as_array()
            .is_some_and(|arr| !arr.is_empty()),
        "the turn itself must survive: {assistant}"
    );
}

/// A persisted details artifact larger than the per-turn cap is dropped at
/// replay (an empty array) instead of failing the request.
#[ignore = "integration"]
#[test]
fn oversized_details_are_dropped_on_replay() {
    let big = "x".repeat(1024 * 1024 + 16);
    let artifact = ReasoningArtifact::ChatReasoning {
        field: ChatReasoningField::ReasoningDetails,
        bytes: format!(r#"[{{"type":"reasoning.text","text":"{big}","index":0}}]"#).into_bytes(),
    };
    let mut session = SessionState::empty();
    add_artifact_tool_turn(&mut session, "go", artifact, "openrouter", OPENROUTER_MODEL);

    let mock = MockProvider::start(vec![(200, "application/json", final_text_response())]);
    let client = openrouter_client(&mock);
    let messages =
        build_chat_request_messages(&session, None, "openrouter", OPENROUTER_MODEL, None);
    send(&client, OPENROUTER_MODEL, &messages);

    let assistant = captured_assistant(&mock.requests()[0].body_json());
    assert_eq!(
        assistant["reasoning_details"],
        serde_json::json!([]),
        "an oversized payload is dropped rather than sent"
    );
}

/// Capture-side display: text/summary entries supply the display text when no
/// scalar field streamed, and an encrypted-only turn still counts as output
/// (never an `EmptyResponse`).
#[ignore = "integration"]
#[test]
fn details_capture_drives_display_and_encrypted_only_is_output() {
    let text_response = r#"{
        "choices": [{
            "message": {
                "role": "assistant",
                "content": null,
                "reasoning_details": [
                    {"type":"reasoning.text","text":"deep think","index":0}
                ]
            },
            "finish_reason": "stop"
        }],
        "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2}
    }"#
    .to_string();
    let encrypted_response = r#"{
        "choices": [{
            "message": {
                "role": "assistant",
                "content": null,
                "reasoning_details": [
                    {"type":"reasoning.encrypted","data":"opaque","index":0}
                ]
            },
            "finish_reason": "stop"
        }],
        "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2}
    }"#
    .to_string();

    let mock = MockProvider::start(vec![
        (200, "application/json", text_response),
        (200, "application/json", encrypted_response),
    ]);
    let client = openrouter_client(&mock);
    let messages = vec![ChatRequestMessage::simple("user", "hi".to_string())];

    let first = send(&client, OPENROUTER_MODEL, &messages);
    let ChatTurnResult::FinalText(first) = first else {
        panic!("expected FinalText");
    };
    assert_eq!(first.reasoning.as_deref(), Some("deep think"));
    assert!(first.reasoning_artifact.is_some());

    // Encrypted-only: no display text, but the turn is model output, so no
    // EmptyResponse and the replayable artifact is captured.
    let second = send(&client, OPENROUTER_MODEL, &messages);
    let ChatTurnResult::FinalText(second) = second else {
        panic!("expected FinalText");
    };
    assert!(second.reasoning.is_none());
    assert!(second.reasoning_artifact.is_some());
}
