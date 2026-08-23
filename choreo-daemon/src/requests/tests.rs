use super::*;
use crate::daemon::DaemonCommand;
use crate::providers::InferenceProvider;
use crate::providers::test_util::{make_failing_provider, make_test_provider};
use crate::reasoning::{
    build_chat_request_messages, initial_prev_resp_id, warn_on_missing_reasoning_artifacts,
};
use crate::tools::context::ToolContext;
use crate::tools::{Tool, ToolExecError, ToolRegistry};
use choreo_ai_protocols::openai::{AssistantToolCall, AssistantToolFunction};
use choreo_proto::{ChatReasoningField, ReasoningArtifact};
use std::sync::mpsc;

fn make_session_with_turns() -> SessionState {
    let mut session = SessionState::empty();
    let (tid0, _) = session.start_turn(Some("hello".into()));
    session.set_assistant_response(
        tid0,
        AssistantResponse {
            text: Some("hi".into()),
            ..Default::default()
        },
    );
    session
}

// Neutral provider/model for structure-only tests: the slug is not in the
// catalog, so the passback policy resolves to None and no artifact is ever
// attached — the exact behavior those tests assert.
const TEST_PROVIDER: &str = "test-stub";
const TEST_MODEL: &str = "test-model";

#[test]
fn build_chat_request_messages_empty() {
    let session = SessionState::empty();
    let result = build_chat_request_messages(&session, None, TEST_PROVIDER, TEST_MODEL);
    assert!(result.is_empty());
}

#[test]
fn build_chat_request_messages_with_system_prompt() {
    let session = SessionState::empty();
    let result =
        build_chat_request_messages(&session, Some("system prompt"), TEST_PROVIDER, TEST_MODEL);
    assert_eq!(result.len(), 1);
    assert_eq!(result[0].role, "system");
    assert_eq!(result[0].content.as_deref(), Some("system prompt"));
}

#[test]
fn build_chat_request_messages_user_and_assistant() {
    let session = make_session_with_turns();
    let result = build_chat_request_messages(&session, None, TEST_PROVIDER, TEST_MODEL);
    assert_eq!(result.len(), 2);
    assert_eq!(result[0].role, "user");
    assert_eq!(result[0].content.as_deref(), Some("hello"));
    assert_eq!(result[1].role, "assistant");
    assert_eq!(result[1].content.as_deref(), Some("hi"));
}

#[test]
fn build_chat_request_messages_with_tool_calls() {
    let mut session = SessionState::empty();
    let (tid, _) = session.start_turn(Some("list files".into()));
    let records = vec![AssistantToolCallRecord {
        call_id: "call_1".into(),
        name: "ls".into(),
        arguments_json: r#"{"path": "."}"#.into(),
    }];
    session.set_assistant_response(
        tid,
        AssistantResponse {
            text: Some("thinking".into()),
            tool_calls: records.clone(),
            ..Default::default()
        },
    );
    // Placeholder results are seeded in call order; the finished tool
    // updates its slot in place.
    session.seed_tool_results(tid, &records, &["".into()]);
    session.update_tool_result(
        tid,
        "call_1",
        "ls".into(),
        "file.txt".into(),
        false,
        String::new(),
    );

    let result = build_chat_request_messages(&session, None, TEST_PROVIDER, TEST_MODEL);
    assert_eq!(result.len(), 3);
    assert_eq!(result[0].role, "user");
    assert_eq!(result[1].role, "assistant");
    assert!(result[1].tool_calls.is_some());
    assert_eq!(result[2].role, "tool");
    assert_eq!(result[2].tool_call_id.as_deref(), Some("call_1"));
}

#[test]
fn build_chat_request_messages_skips_undone_turns() {
    let mut session = SessionState::empty();
    let (tid0, _) = session.start_turn(Some("visible".into()));
    session.set_assistant_response(
        tid0,
        AssistantResponse {
            text: Some("ok".into()),
            ..Default::default()
        },
    );
    let (tid1, _) = session.start_turn(Some("hidden".into()));
    session.set_assistant_response(
        tid1,
        AssistantResponse {
            text: Some("nope".into()),
            ..Default::default()
        },
    );
    if let Some(turn) = session.turns.get_mut(&tid1) {
        turn.undone = true;
    }

    let result = build_chat_request_messages(&session, None, TEST_PROVIDER, TEST_MODEL);
    assert_eq!(result.len(), 2);
    assert_eq!(result[0].role, "user");
    assert_eq!(result[0].content.as_deref(), Some("visible"));
}

// -- Reasoning passback builder policy (phase 4b) -----------------------

fn tool_call_record(call_id: &str, name: &str) -> AssistantToolCallRecord {
    AssistantToolCallRecord {
        call_id: call_id.into(),
        name: name.into(),
        arguments_json: "{}".into(),
    }
}

fn deepseek_producer() -> ReasoningProducer {
    ReasoningProducer {
        provider_slug: "deepseek".into(),
        model: "deepseek-v4-pro".into(),
    }
}

fn anthropic_producer() -> ReasoningProducer {
    ReasoningProducer {
        provider_slug: "anthropic".into(),
        model: "claude-sonnet-5".into(),
    }
}

/// Non-DeepSeek OpenAI-compat chat provider: ToolLoop passback but no
/// `reasoning_content` injection — the generalized fallback must cover it.
const GROQ_MODEL: &str = "groq/llama-3.3-70b-versatile";

fn groq_producer() -> ReasoningProducer {
    ReasoningProducer {
        provider_slug: "groq".into(),
        model: GROQ_MODEL.into(),
    }
}

/// Pinned to `reasoning_passback = none` by the bundled overlay (the gateway
/// rejects replayed `reasoning_content`) — the fallback must respect it.
fn cerebras_producer() -> ReasoningProducer {
    ReasoningProducer {
        provider_slug: "cerebras".into(),
        model: "gpt-oss-120b".into(),
    }
}

fn artifact(bytes: &[u8]) -> ReasoningArtifact {
    ReasoningArtifact::ChatReasoning {
        field: ChatReasoningField::ReasoningContent,
        bytes: bytes.to_vec(),
    }
}

/// Add a turn with an optional artifact/producer and optional assistant
/// tool calls, returning its turn_id.
fn add_turn(
    session: &mut SessionState,
    user_text: &str,
    assistant_text: &str,
    artifact: Option<ReasoningArtifact>,
    producer: Option<ReasoningProducer>,
    tool_calls: Vec<AssistantToolCallRecord>,
) -> u32 {
    let (tid, _) = session.start_turn(Some(user_text.to_string()));
    session.set_assistant_response(
        tid,
        AssistantResponse {
            text: Some(assistant_text.to_string()),
            tool_calls,
            reasoning_artifact: artifact,
            reasoning_producer: producer,
            ..Default::default()
        },
    );
    tid
}

fn assistant_messages(result: &[ChatRequestMessage]) -> Vec<&ChatRequestMessage> {
    result.iter().filter(|m| m.role == "assistant").collect()
}

#[test]
fn builder_tool_loop_attaches_artifact_only_for_tool_involving_turns() {
    let mut session = SessionState::empty();
    // Plain text turn (no tool involvement) with an artifact: must NOT be
    // replayed under ToolLoop (DeepSeek/Kimi only require it on tool-loop
    // messages).
    add_turn(
        &mut session,
        "hello",
        "hi",
        Some(artifact(b"plain")),
        Some(deepseek_producer()),
        vec![],
    );
    // Tool-call turn with an artifact: must be replayed.
    add_turn(
        &mut session,
        "list files",
        "thinking...",
        Some(artifact(b"tool-thinking")),
        Some(deepseek_producer()),
        vec![tool_call_record("call_1", "ls")],
    );

    // deepseek-v4-pro carries an explicit `tool_loop` passback override.
    let result = build_chat_request_messages(&session, None, "deepseek", "deepseek-v4-pro");
    let assistants = assistant_messages(&result);
    assert_eq!(assistants.len(), 2);
    assert_eq!(
        assistants[0].reasoning_artifact, None,
        "plain text turn must not replay reasoning under ToolLoop",
    );
    assert_eq!(
        assistants[1].reasoning_artifact,
        Some(artifact(b"tool-thinking")),
        "tool-call turn must replay its artifact under ToolLoop",
    );
}

#[test]
fn builder_tool_loop_attaches_artifact_for_tool_result_turns() {
    let mut session = SessionState::empty();
    // A turn carrying only tool RESULTS (no assistant tool_calls on the
    // message, e.g. a mid-loop state persisted under an earlier request)
    // is tool-involving too: the next request must still echo it.
    let tid = add_turn(
        &mut session,
        "run it",
        "running",
        Some(artifact(b"mid-loop")),
        Some(deepseek_producer()),
        vec![],
    );
    session
        .turns
        .get_mut(&tid)
        .expect("turn exists")
        .tool_results
        .push(choreo_proto::ToolResultRecord {
            call_id: "call_1".into(),
            name: "sh".into(),
            content: "ok".into(),
            is_error: false,
            invocation_description: String::new(),
        });

    let result = build_chat_request_messages(&session, None, "deepseek", "deepseek-v4-pro");
    let assistants = assistant_messages(&result);
    assert_eq!(assistants.len(), 1);
    assert_eq!(
        assistants[0].reasoning_artifact,
        Some(artifact(b"mid-loop"))
    );
}

#[test]
fn builder_injects_empty_reasoning_content_for_deepseek_without_artifact() {
    // DeepSeek chat requires `reasoning_content` to be present on every
    // assistant message even when the model produced no reasoning. A tool
    // turn with no artifact must therefore carry an explicit empty string on
    // the wire (mirrors opencode's `{type:"reasoning", text:""}` injection).
    let mut session = SessionState::empty();
    add_turn(
        &mut session,
        "run it",
        "",
        None,
        Some(deepseek_producer()),
        vec![tool_call_record("call_1", "exec")],
    );

    let result = build_chat_request_messages(&session, None, "deepseek", "deepseek-v4-pro");
    let assistants = assistant_messages(&result);
    assert_eq!(assistants.len(), 1);
    assert_eq!(
        assistants[0].reasoning_content,
        Some(String::new()),
        "deepseek assistant message must carry an (empty) reasoning_content"
    );
}

#[test]
fn builder_deepseek_artifact_text_outranks_empty_injection() {
    // When a real artifact exists and is echoed, the explicit reasoning_content
    // field must stay None so the artifact re-emits its text (not an empty
    // placeholder crowding it out).
    let mut session = SessionState::empty();
    add_turn(
        &mut session,
        "run it",
        "",
        Some(artifact(b"real thinking")),
        Some(deepseek_producer()),
        vec![tool_call_record("call_1", "exec")],
    );

    let result = build_chat_request_messages(&session, None, "deepseek", "deepseek-v4-pro");
    let assistants = assistant_messages(&result);
    assert_eq!(assistants.len(), 1);
    assert_eq!(assistants[0].reasoning_content, None);
    assert_eq!(
        assistants[0].reasoning_artifact,
        Some(artifact(b"real thinking"))
    );
}

#[test]
fn builder_does_not_inject_empty_reasoning_content_for_non_deepseek() {
    // Unrelated OpenAI-chat models are untouched: no empty reasoning_content.
    let mut session = SessionState::empty();
    add_turn(
        &mut session,
        "run it",
        "",
        None,
        Some(ReasoningProducer {
            provider_slug: "openai".into(),
            model: "gpt-4".into(),
        }),
        vec![tool_call_record("call_1", "exec")],
    );

    let result = build_chat_request_messages(&session, None, "openai", "gpt-4");
    let assistants = assistant_messages(&result);
    assert_eq!(assistants.len(), 1);
    assert_eq!(assistants[0].reasoning_content, None);
}

#[test]
fn builder_requires_rc_empty_content_turn_echoes_artifact() {
    // A DeepSeek/Kimi turn recorded as reasoning-only — same-model artifact,
    // but empty content and no tool calls. ToolLoop alone would skip the
    // echo (no tool involvement), leaving the wire assistant message wholly
    // empty (`content: ""` + injected empty `reasoning_content`) — the exact
    // shape upstream rejects with "the message ... with role 'assistant'
    // must not be empty". The builder's empty-message fallback must echo the
    // artifact's real reasoning text instead.
    let mut session = SessionState::empty();
    add_turn(
        &mut session,
        "continue",
        "",
        Some(artifact(b"long reasoning text")),
        Some(deepseek_producer()),
        vec![],
    );

    let result = build_chat_request_messages(&session, None, "deepseek", "deepseek-v4-pro");
    let assistants = assistant_messages(&result);
    assert_eq!(assistants.len(), 1);
    assert_eq!(
        assistants[0].reasoning_artifact,
        Some(artifact(b"long reasoning text")),
        "empty-message fallback echoes the same-model artifact",
    );
    // The injected empty string must NOT shadow the artifact: leave the
    // explicit field None so the Serialize impl re-emits the real text.
    assert_eq!(assistants[0].reasoning_content, None);
    assert_eq!(assistants[0].content.as_deref(), Some(""));

    // The wire the provider actually receives carries the non-empty echo.
    let body = serde_json::to_value(&result[1]).unwrap();
    assert_eq!(body["content"], "");
    assert_eq!(body["reasoning_content"], "long reasoning text");
}

#[test]
fn builder_requires_rc_empty_content_turn_keeps_content_turn_bare() {
    // The fallback must NOT change plain-text turns: a non-empty assistant
    // message with a same-model artifact but no tool involvement is still
    // sent bare under ToolLoop (no echo needed — the message is valid).
    let mut session = SessionState::empty();
    add_turn(
        &mut session,
        "hello",
        "hi",
        Some(artifact(b"plain")),
        Some(deepseek_producer()),
        vec![],
    );

    let result = build_chat_request_messages(&session, None, "deepseek", "deepseek-v4-pro");
    let assistants = assistant_messages(&result);
    assert_eq!(assistants.len(), 1);
    assert_eq!(assistants[0].reasoning_artifact, None);
    assert_eq!(assistants[0].reasoning_content, Some(String::new()));
}

#[test]
fn builder_requires_rc_empty_content_turn_foreign_artifact_not_replayed() {
    // Same reasoning-only shape, but the artifact was produced by a DIFFERENT
    // model (mid-session switch scenario): the payload is model-bound and
    // must never be replayed, so the message stays empty on the wire — that
    // is the unfixable case the daemon guard flags as a "must not be empty"
    // risk rather than a silent corruption.
    let mut session = SessionState::empty();
    add_turn(
        &mut session,
        "continue",
        "",
        Some(artifact(b"claude thinking")),
        Some(anthropic_producer()),
        vec![],
    );

    let result = build_chat_request_messages(&session, None, "deepseek", "deepseek-v4-pro");
    let assistants = assistant_messages(&result);
    assert_eq!(assistants.len(), 1);
    assert_eq!(assistants[0].reasoning_artifact, None);
    assert_eq!(assistants[0].reasoning_content, Some(String::new()));

    // And the guard must flag it: wire-empty + requires_rc + nothing
    // replayable.
    assert_eq!(
        warn_on_missing_reasoning_artifacts(&session, 7, "deepseek", "deepseek-v4-pro"),
        1,
        "foreign-producer artifact on a wire-empty requires_rc turn is flagged",
    );
}

#[test]
fn guard_requires_rc_empty_content_turn_with_replayable_artifact_is_clean() {
    // Wire-empty turn whose same-model artifact the builder echoes via the
    // empty-message fallback: the request self-heals, so the guard must NOT
    // count it as a problem.
    let mut session = SessionState::empty();
    add_turn(
        &mut session,
        "continue",
        "",
        Some(artifact(b"thinking")),
        Some(deepseek_producer()),
        vec![],
    );
    assert_eq!(
        warn_on_missing_reasoning_artifacts(&session, 7, "deepseek", "deepseek-v4-pro"),
        0,
        "empty-message fallback already echoes the same-model artifact",
    );
}

#[test]
fn guard_requires_rc_empty_content_turn_missing_artifact_is_flagged() {
    // Wire-empty turn with NO artifact at all (e.g. reasoning-only response
    // captured before the artifact feature, or a producer that never sent
    // reasoning): nothing can fill the message, so the provider's "must not
    // be empty" 400 is certain — the guard must surface it.
    let mut session = SessionState::empty();
    add_turn(
        &mut session,
        "continue",
        "",
        None,
        Some(deepseek_producer()),
        vec![],
    );
    assert_eq!(
        warn_on_missing_reasoning_artifacts(&session, 7, "deepseek", "deepseek-v4-pro"),
        1,
        "wire-empty requires_rc turn with no artifact to fill it is flagged",
    );
}

#[test]
fn builder_wire_empty_turn_echoes_artifact_on_non_requires_rc_provider() {
    // The empty-message fallback is provider-agnostic: a content-less,
    // tool-less turn with a same-model artifact must echo it on ANY
    // echo-capable chat provider (here groq — ToolLoop passback, no
    // `reasoning_content` injection) because a wholly empty assistant
    // message is the "must not be empty" 400 on any OpenAI-compatible
    // endpoint, not just DeepSeek/Kimi.
    let mut session = SessionState::empty();
    add_turn(
        &mut session,
        "continue",
        "",
        Some(artifact(b"reasoned but silent")),
        Some(groq_producer()),
        vec![],
    );

    let result = build_chat_request_messages(&session, None, "groq", GROQ_MODEL);
    let assistants = assistant_messages(&result);
    assert_eq!(assistants.len(), 1);
    assert_eq!(
        assistants[0].reasoning_artifact,
        Some(artifact(b"reasoned but silent")),
        "empty-message fallback echoes the same-model artifact on any echo-capable chat provider",
    );
    // No empty-string injection on non-requires_rc models: the artifact
    // re-emits its text directly.
    assert_eq!(assistants[0].reasoning_content, None);

    let body = serde_json::to_value(&result[1]).unwrap();
    assert_eq!(body["content"], "");
    assert_eq!(body["reasoning_content"], "reasoned but silent");
}

#[test]
fn guard_wire_empty_turn_without_artifact_flagged_on_non_requires_rc() {
    // Wire-empty turn with nothing replayable is flagged on a non-requires_rc
    // ToolLoop provider too — the pre-generalization guard only caught
    // DeepSeek/Kimi turns (the missing-artifact `requires_rc` gate), leaving
    // an equally invalid empty message unflagged on e.g. groq.
    let mut session = SessionState::empty();
    add_turn(
        &mut session,
        "continue",
        "",
        None,
        Some(groq_producer()),
        vec![],
    );
    assert_eq!(
        warn_on_missing_reasoning_artifacts(&session, 7, "groq", GROQ_MODEL),
        1,
        "wire-empty turn with no artifact to fill it is flagged on any echo-capable chat provider",
    );
    let result = build_chat_request_messages(&session, None, "groq", GROQ_MODEL);
    let assistants = assistant_messages(&result);
    assert_eq!(assistants.len(), 1);
    assert_eq!(assistants[0].reasoning_artifact, None);
    assert_eq!(assistants[0].reasoning_content, None);
}

#[test]
fn builder_wire_empty_turn_never_echoes_under_none_passback() {
    // Cerebras gpt-oss-120b is pinned to `reasoning_passback = none` by the
    // bundled overlay ("the gateway rejects replayed reasoning_content"): the
    // empty-message fallback must NOT override that explicit never-replay
    // policy — echoing would swap the "must not be empty" 400 for a "must
    // not replay" 400. The guard skips None-passback requests entirely
    // (documented policy), so this stays a session_inspect-visible hazard.
    let mut session = SessionState::empty();
    add_turn(
        &mut session,
        "continue",
        "",
        Some(artifact(b"gpt-oss thinking")),
        Some(cerebras_producer()),
        vec![],
    );

    let result = build_chat_request_messages(&session, None, "cerebras", "gpt-oss-120b");
    let assistants = assistant_messages(&result);
    assert_eq!(assistants.len(), 1);
    assert_eq!(assistants[0].reasoning_artifact, None);
    assert_eq!(assistants[0].reasoning_content, None);
    assert_eq!(
        warn_on_missing_reasoning_artifacts(&session, 7, "cerebras", "gpt-oss-120b"),
        0,
        "None-passback requests skip the artifact guard entirely",
    );
}

#[test]
fn builder_all_turns_attaches_always() {
    let mut session = SessionState::empty();
    // Unknown model under the anthropic slug → protocol default AllTurns
    // (no explicit TOML override, unlike claude-sonnet-4-5 which is a
    // last-turn-only `tool_loop` model).
    let producer = ReasoningProducer {
        provider_slug: "anthropic".into(),
        model: "claude-unknown-model".into(),
    };
    add_turn(
        &mut session,
        "hello",
        "hi",
        Some(artifact(b"one")),
        Some(producer.clone()),
        vec![],
    );
    add_turn(
        &mut session,
        "again",
        "bye",
        Some(artifact(b"two")),
        Some(producer),
        vec![],
    );

    // Anthropic → AllTurns: every assistant message replays its artifact,
    // even non-tool turns.
    let result = build_chat_request_messages(&session, None, "anthropic", "claude-unknown-model");
    let assistants = assistant_messages(&result);
    assert_eq!(assistants.len(), 2);
    assert_eq!(assistants[0].reasoning_artifact, Some(artifact(b"one")));
    assert_eq!(assistants[1].reasoning_artifact, Some(artifact(b"two")));
}

#[test]
fn builder_signature_policy_attaches_always() {
    let mut session = SessionState::empty();
    add_turn(
        &mut session,
        "hello",
        "hi",
        Some(ReasoningArtifact::GoogleSignatures(b"sig".to_vec())),
        Some(ReasoningProducer {
            provider_slug: "google".into(),
            model: "gemini-2.5-pro".into(),
        }),
        vec![],
    );

    // Google → Signature: every assistant message replays the signatures.
    let result = build_chat_request_messages(&session, None, "google", "gemini-2.5-pro");
    let assistants = assistant_messages(&result);
    assert_eq!(assistants.len(), 1);
    assert_eq!(
        assistants[0].reasoning_artifact,
        Some(ReasoningArtifact::GoogleSignatures(b"sig".to_vec())),
    );
}

#[test]
fn builder_none_never_attaches() {
    let mut session = SessionState::empty();
    // A tool-involving turn WITH an artifact under a None-policy provider:
    // the artifact must never be replayed (display-only provider).
    add_turn(
        &mut session,
        "list",
        "thinking",
        Some(artifact(b"payload")),
        Some(ReasoningProducer {
            provider_slug: "unknown-provider".into(),
            model: "m".into(),
        }),
        vec![tool_call_record("call_1", "ls")],
    );

    let result = build_chat_request_messages(&session, None, "unknown-provider", "m");
    let assistants = assistant_messages(&result);
    assert_eq!(assistants.len(), 1);
    assert_eq!(assistants[0].reasoning_artifact, None);
}

#[test]
fn builder_response_id_policy_never_attaches_via_message() {
    let mut session = SessionState::empty();
    add_turn(
        &mut session,
        "list",
        "thinking",
        Some(artifact(b"payload")),
        Some(ReasoningProducer {
            provider_slug: "openai".into(),
            model: "gpt-4".into(),
        }),
        vec![tool_call_record("call_1", "ls")],
    );

    // gpt-4 is a Responses model → ResponseId policy: continuity flows via
    // previous_response_id, so the message must NOT carry the artifact.
    let result = build_chat_request_messages(&session, None, "openai", "gpt-4");
    let assistants = assistant_messages(&result);
    assert_eq!(assistants.len(), 1);
    assert_eq!(assistants[0].reasoning_artifact, None);
}

#[test]
fn builder_same_model_mismatch_drops_artifact() {
    let mut session = SessionState::empty();
    // Current-model turn (deepseek): artifact kept.
    add_turn(
        &mut session,
        "list",
        "thinking",
        Some(artifact(b"kept")),
        Some(deepseek_producer()),
        vec![tool_call_record("call_1", "ls")],
    );
    // Turn produced by a DIFFERENT model mid-session (e.g. the user
    // switched deepseek → claude): the artifact is model-bound and must be
    // dropped even though the turn is tool-involving (replaying an
    // encrypted ChatReasoning payload into an Anthropic request — or a
    // stale deepseek payload after switching back — would corrupt the
    // conversation).
    add_turn(
        &mut session,
        "old model turn",
        "old thinking",
        Some(artifact(b"dropped")),
        Some(ReasoningProducer {
            provider_slug: "anthropic".into(),
            model: "claude-sonnet-4-5".into(),
        }),
        vec![tool_call_record("call_2", "grep")],
    );

    let result = build_chat_request_messages(&session, None, "deepseek", "deepseek-v4-pro");
    let assistants = assistant_messages(&result);
    assert_eq!(assistants.len(), 2);
    assert_eq!(assistants[0].reasoning_artifact, Some(artifact(b"kept")));
    assert_eq!(
        assistants[1].reasoning_artifact, None,
        "artifact from a previous model must be dropped",
    );
}

#[test]
fn builder_undone_turn_artifact_is_skipped() {
    let mut session = SessionState::empty();
    // Visible, tool-involving turn: its artifact must be replayed under
    // ToolLoop.
    add_turn(
        &mut session,
        "visible",
        "ok",
        Some(artifact(b"kept")),
        Some(deepseek_producer()),
        vec![tool_call_record("call_0", "pwd")],
    );
    let undone_tid = add_turn(
        &mut session,
        "hidden",
        "nope",
        Some(artifact(b"dropped")),
        Some(deepseek_producer()),
        vec![tool_call_record("call_1", "ls")],
    );
    session
        .turns
        .get_mut(&undone_tid)
        .expect("turn exists")
        .undone = true;

    let result = build_chat_request_messages(&session, None, "deepseek", "deepseek-v4-pro");
    let assistants = assistant_messages(&result);
    assert_eq!(assistants.len(), 1, "undone turn must be skipped entirely");
    assert_eq!(assistants[0].reasoning_artifact, Some(artifact(b"kept")));
}

// -- prev_resp_id persistence (phase 4c) --------------------------------

#[test]
fn initial_prev_resp_id_response_policy_restores_persisted_id() {
    let mut session = SessionState::empty();
    session.config.last_response_id = Some("resp_123".into());
    session.config.last_response_id_producer = Some(ReasoningProducer {
        provider_slug: "openai".into(),
        model: "gpt-4".into(),
    });
    // gpt-4 is an OpenAI Responses model → ResponseId policy AND the
    // persisted id was produced by the same provider+model: the id must
    // be restored to chain reasoning continuity across user turns.
    assert_eq!(
        initial_prev_resp_id(&session, "openai", "gpt-4").as_deref(),
        Some("resp_123"),
    );
}

#[test]
fn initial_prev_resp_id_other_policies_reset_to_none() {
    let mut session = SessionState::empty();
    session.config.last_response_id = Some("resp_123".into());
    session.config.last_response_id_producer = Some(ReasoningProducer {
        provider_slug: "deepseek".into(),
        model: "deepseek-v4-pro".into(),
    });
    // DeepSeek chat → ToolLoop policy: a stale id must NOT leak into a
    // request that does not understand previous_response_id — even when
    // the provenance matches.
    assert_eq!(
        initial_prev_resp_id(&session, "deepseek", "deepseek-v4-pro"),
        None,
    );
    // Unknown provider → None policy.
    assert_eq!(
        initial_prev_resp_id(&session, "unknown-provider", "m"),
        None
    );
}

#[test]
fn initial_prev_resp_id_drops_stale_id_from_other_producer() {
    // The persisted id was produced by a DIFFERENT provider+model (e.g. a
    // mid-session openai → xAI switch): restoring it would replay a stale
    // previous_response_id into a service that does not recognize it →
    // provider 400. Provenance must gate the restore, exactly like
    // reasoning artifacts.
    let mut session = SessionState::empty();
    session.config.last_response_id = Some("resp_openai".into());
    session.config.last_response_id_producer = Some(ReasoningProducer {
        provider_slug: "openai".into(),
        model: "gpt-5.4".into(),
    });
    // Same provider, different model → dropped (model-bound provenance).
    assert_eq!(
        initial_prev_resp_id(&session, "openai", "gpt-4"),
        None,
        "id from gpt-5.4 must not be restored for gpt-4",
    );
    // Matching provider+model → restored.
    assert_eq!(
        initial_prev_resp_id(&session, "openai", "gpt-5.4").as_deref(),
        Some("resp_openai"),
    );
    // No producer recorded (fresh session) → no id is ever restored.
    let fresh = SessionState::empty();
    assert_eq!(initial_prev_resp_id(&fresh, "openai", "gpt-5.4"), None);
}

// -- Precondition guard (phase 4c) --------------------------------------

#[test]
fn guard_warns_when_tool_involving_turn_lacks_artifact() {
    let mut session = SessionState::empty();
    // Tool-involving turn WITHOUT an artifact (pre-migration session state).
    let (tid, _) = session.start_turn(Some("list".into()));
    let records = vec![tool_call_record("call_1", "ls")];
    session.set_assistant_response(
        tid,
        AssistantResponse {
            text: Some("thinking".into()),
            tool_calls: records.clone(),
            ..Default::default()
        },
    );
    session.seed_tool_results(tid, &records, &["".into()]);
    // Tool-involving turn WITH an artifact: clean.
    add_turn(
        &mut session,
        "again",
        "thinking2",
        Some(artifact(b"ok")),
        Some(deepseek_producer()),
        vec![tool_call_record("call_2", "sh")],
    );

    let missing = warn_on_missing_reasoning_artifacts(&session, 7, "deepseek", "deepseek-v4-pro");
    assert_eq!(
        missing, 1,
        "only the artifact-less tool turn should be flagged",
    );
}

#[test]
fn guard_clean_when_all_tool_turns_have_artifacts() {
    let mut session = SessionState::empty();
    add_turn(
        &mut session,
        "list",
        "thinking",
        Some(artifact(b"ok")),
        Some(deepseek_producer()),
        vec![tool_call_record("call_1", "ls")],
    );
    // Non-tool turns never need an artifact.
    add_turn(
        &mut session,
        "plain",
        "hi",
        None,
        Some(deepseek_producer()),
        vec![],
    );
    assert_eq!(
        warn_on_missing_reasoning_artifacts(&session, 7, "deepseek", "deepseek-v4-pro"),
        0,
    );
}

#[test]
fn guard_all_turns_policy_flags_non_tool_turn_missing_artifact() {
    // AllTurns providers (Anthropic keep-all) echo reasoning on EVERY
    // assistant message, not just tool-involving ones — so a plain
    // assistant turn without its artifact is a violation there too (the
    // ToolLoop scope would have skipped it).
    let mut session = SessionState::empty();
    // Tool-involving turn WITH an artifact from the same producer: clean.
    add_turn(
        &mut session,
        "list",
        "thinking",
        Some(artifact(b"ok")),
        Some(anthropic_producer()),
        vec![tool_call_record("call_1", "ls")],
    );
    // Non-tool assistant turn WITH an artifact from the same producer:
    // clean under AllTurns.
    add_turn(
        &mut session,
        "plain",
        "hi",
        Some(artifact(b"ok")),
        Some(anthropic_producer()),
        vec![],
    );
    // Non-tool assistant turn WITHOUT an artifact: flagged under AllTurns.
    let (tid, _) = session.start_turn(Some("later".into()));
    session.set_assistant_response(
        tid,
        AssistantResponse {
            text: Some("hello".into()),
            ..Default::default()
        },
    );
    assert_eq!(
        warn_on_missing_reasoning_artifacts(&session, 7, "anthropic", "claude-sonnet-5"),
        1,
        "AllTurns flags the artifact-less non-tool assistant turn",
    );
}

#[test]
fn guard_user_only_turn_is_not_flagged() {
    // A turn that never produced an assistant message (in-progress or
    // failed) has no artifact by construction and must not be flagged.
    let mut session = SessionState::empty();
    add_turn(
        &mut session,
        "list",
        "thinking",
        Some(artifact(b"ok")),
        Some(anthropic_producer()),
        vec![tool_call_record("call_1", "ls")],
    );
    let _ = session.start_turn(Some("pending user text".into()));
    assert_eq!(
        warn_on_missing_reasoning_artifacts(&session, 7, "anthropic", "claude-sonnet-5"),
        0,
    );
}

#[test]
fn guard_flags_foreign_producer_artifact() {
    // A turn whose artifact was produced by a DIFFERENT model (a mid-session
    // switch) has a payload the builder will NOT replay (same-model
    // provenance) — the wire request omits the required echo, so the guard
    // flags it exactly like a missing artifact. Otherwise the provider 400
    // after a model switch would remain a mystery.
    let mut session = SessionState::empty();
    add_turn(
        &mut session,
        "list",
        "thinking",
        Some(artifact(b"ok")),
        Some(deepseek_producer()),
        vec![tool_call_record("call_1", "ls")],
    );
    assert_eq!(
        warn_on_missing_reasoning_artifacts(&session, 7, "anthropic", "claude-sonnet-5"),
        1,
        "foreign-producer artifact flagged under AllTurns",
    );
    // The same turn under its own producer+model is clean (provenance match).
    assert_eq!(
        warn_on_missing_reasoning_artifacts(&session, 7, "deepseek", "deepseek-v4-pro"),
        0,
    );
}

#[test]
fn guard_skipped_for_non_echo_policies() {
    let mut session = SessionState::empty();
    let (tid, _) = session.start_turn(Some("list".into()));
    let records = vec![tool_call_record("call_1", "ls")];
    session.set_assistant_response(
        tid,
        AssistantResponse {
            text: Some("thinking".into()),
            tool_calls: records.clone(),
            ..Default::default()
        },
    );
    session.seed_tool_results(tid, &records, &["".into()]);
    // ResponseId policy: artifacts flow via previous_response_id, so the
    // guard must not flag the missing message artifact.
    assert_eq!(
        warn_on_missing_reasoning_artifacts(&session, 7, "openai", "gpt-4"),
        0,
    );
}

// -- Concurrent tool status label tests --------------------------------

#[test]
fn concurrent_status_label_single_tool_uses_real_name() {
    // A lone non-config tool call still dispatches through the concurrent
    // bucket, so the status must show its real name, not "(parallel)".
    let label = concurrent_tool_status_label(&[config_change_call("sh", "{}")]);
    assert_eq!(label, "sh");
}

#[test]
fn concurrent_status_label_multi_tool_batch_is_parallel() {
    let label = concurrent_tool_status_label(&[
        config_change_call("sh", "{}"),
        config_change_call("grep", "{}"),
    ]);
    assert_eq!(label, "(parallel)");
}

#[test]
fn concurrent_status_label_empty_batch_is_parallel() {
    // Defensive: the caller guards with `!concurrent.is_empty()` before
    // sending, but the label should still be well-defined if reached.
    let label = concurrent_tool_status_label(&[]);
    assert_eq!(label, "(parallel)");
}

// -- Cancellation helper tests -----------------------------------------

#[test]
fn is_cancelled_once_no_signal() {
    let (_tx, rx) = crossbeam_channel::unbounded::<()>();
    assert!(!is_cancelled_once(&rx));
}

#[test]
fn is_cancelled_once_with_signal() {
    let (tx, rx) = crossbeam_channel::unbounded::<()>();
    tx.send(()).unwrap();
    assert!(is_cancelled_once(&rx));
}

// -- pending/apply config-change tests ---------------------------------

fn config_change_call(name: &str, arguments_json: &str) -> ChatToolCall {
    ChatToolCall {
        id: "call_1".into(),
        name: name.into(),
        arguments_json: arguments_json.into(),
        caller: None,
    }
}

/// A successful tool output with the given structured result (or `None`
/// for tools whose result_json wasn't captured).
fn ok_output(result_json: Option<serde_json::Value>) -> ToolOutput {
    ToolOutput {
        content: String::new(),
        is_error: false,
        invocation_description: String::new(),
        result_json,
    }
}

#[test]
fn pending_load_tools_captures_groups_and_applies() {
    let tool_call = config_change_call("load_tools", r#"{"groups": ["shell", "x"]}"#);
    let change = pending_config_change(&tool_call, &ok_output(None), None)
        .expect("load_tools should produce a change");
    assert!(matches!(change, PendingConfigChange::LoadTools(ref g) if g == &["shell", "x"]));

    let mut session = SessionState::empty();
    session.config.active_tool_groups = ["core".into(), "git".into()].into_iter().collect();
    apply_pending_config_change(&mut session, &change);
    assert!(session.config.active_tool_groups.contains("shell"));
    assert!(session.config.active_tool_groups.contains("x"));
    assert!(session.config.active_tool_groups.contains("core"));
}

#[test]
fn pending_unload_tools_captures_groups_and_applies() {
    let tool_call = config_change_call("unload_tools", r#"{"groups": ["shell"]}"#);
    let change = pending_config_change(&tool_call, &ok_output(None), None)
        .expect("unload_tools should produce a change");
    assert!(matches!(change, PendingConfigChange::UnloadTools(ref g) if g == &["shell"]));

    let mut session = SessionState::empty();
    session.config.active_tool_groups = ["core".into(), "shell".into()].into_iter().collect();
    apply_pending_config_change(&mut session, &change);
    assert!(!session.config.active_tool_groups.contains("shell"));
    assert!(session.config.active_tool_groups.contains("core"));
}

#[test]
fn pending_set_working_dir_mirrors_executed_result() {
    // The tool executed against a path that has since been deleted.  The
    // mirror must use the EXECUTED result (the canonical path the tool
    // computed and the main loop applied) — no re-resolution, so the
    // deleted directory cannot break the mirror (no TOCTOU).
    let tool_call = config_change_call("set_working_dir", r#"{"path": "sub"}"#);
    let output = ok_output(Some(serde_json::json!({ "path": "/real/canonical/sub" })));
    let change = pending_config_change(&tool_call, &output, None)
        .expect("set_working_dir should produce a change");
    assert!(matches!(
        change,
        PendingConfigChange::SetWorkingDir(Some(ref p)) if p == &PathBuf::from("/real/canonical/sub")
    ));

    let mut session = SessionState::empty();
    session.discovered_skills = Some(Vec::new());
    apply_pending_config_change(&mut session, &change);
    assert_eq!(
        session.config.working_dir.as_deref(),
        Some(PathBuf::from("/real/canonical/sub").as_path())
    );
    assert!(
        session.discovered_skills.is_none(),
        "skill cache must be invalidated by the mirror"
    );
}

#[test]
fn pending_set_working_dir_falls_back_to_shared_resolution() {
    // result_json missing (shouldn't happen on success) — the mirror
    // falls back to the SAME resolution helper the tool uses.
    let base = tempfile::tempdir().unwrap();
    let sub = base.path().join("sub");
    std::fs::create_dir(&sub).unwrap();
    let tool_call = config_change_call("set_working_dir", r#"{"path": "sub"}"#);

    let change = pending_config_change(&tool_call, &ok_output(None), Some(base.path()))
        .expect("set_working_dir should produce a change");

    let mut session = SessionState::empty();
    apply_pending_config_change(&mut session, &change);
    assert_eq!(
        session.config.working_dir.as_deref(),
        Some(sub.canonicalize().unwrap().as_path())
    );
}

#[test]
fn pending_set_working_dir_nonexistent_path_still_invalidates_skills() {
    // The tool succeeded (result_json present) but the path is now gone.
    // The mirror still applies the executed path verbatim — no TOCTOU.
    let tool_call = config_change_call("set_working_dir", r#"{"path": "gone"}"#);
    let output = ok_output(Some(serde_json::json!({ "path": "/gone/dir" })));
    let change = pending_config_change(&tool_call, &output, None)
        .expect("set_working_dir should produce a change");

    let mut session = SessionState::empty();
    session.discovered_skills = Some(Vec::new());
    apply_pending_config_change(&mut session, &change);
    assert_eq!(
        session.config.working_dir.as_deref(),
        Some(PathBuf::from("/gone/dir").as_path())
    );
    assert!(
        session.discovered_skills.is_none(),
        "skill cache must be invalidated even when the path is gone"
    );
}

#[test]
fn pending_set_working_dir_unresolvable_fallback_still_invalidates_skills() {
    // result_json missing AND the fallback resolution fails (path does
    // not exist) — the worker skips the path update but MUST still
    // invalidate its skill cache so stale skills never leak across the
    // request boundary (RequestFinished merges discovered_skills over the
    // main loop's invalidated None).
    let base = tempfile::tempdir().unwrap();
    let tool_call = config_change_call("set_working_dir", r#"{"path": "does-not-exist"}"#);

    let change = pending_config_change(&tool_call, &ok_output(None), Some(base.path()))
        .expect("set_working_dir should still produce a change");
    assert!(matches!(change, PendingConfigChange::SetWorkingDir(None)));

    let mut session = SessionState::empty();
    session.discovered_skills = Some(Vec::new());
    apply_pending_config_change(&mut session, &change);
    assert!(session.config.working_dir.is_none());
    assert!(
        session.discovered_skills.is_none(),
        "skill cache must be invalidated even when no path could be resolved"
    );
}

#[test]
fn pending_unknown_tool_is_noop() {
    let tool_call = config_change_call("read_file", r#"{"path": "x"}"#);
    assert!(pending_config_change(&tool_call, &ok_output(None), None).is_none());
}

#[test]
fn pending_unparseable_args_is_noop() {
    let tool_call = config_change_call("load_tools", "not json");
    assert!(pending_config_change(&tool_call, &ok_output(None), None).is_none());
}

// -- broadcast_turn_appended tests -----------------------------------

#[test]
fn broadcast_turn_appended_sends_when_turn_exists() {
    let (tx, rx) = mpsc::channel::<SessionCommand>();
    let mut session = SessionState::empty();
    let (turn_id, _) = session.start_turn(Some("hello".into()));

    broadcast_turn_appended(&tx, &session, 0, turn_id);

    match rx.try_recv() {
        Ok(SessionCommand::Broadcast(DaemonMessage::Session {
            event: SessionEvent::TurnAppended { turn_id: id, .. },
            ..
        })) => {
            assert_eq!(id, turn_id);
        }
        Ok(_) => panic!("expected TurnAppended broadcast, got different command"),
        Err(e) => panic!("expected TurnAppended broadcast, got error: {e}"),
    }
}

#[test]
fn broadcast_turn_appended_no_turn_no_broadcast() {
    let (tx, rx) = mpsc::channel::<SessionCommand>();
    let session = SessionState::empty();

    broadcast_turn_appended(&tx, &session, 0, 999);

    assert!(rx.try_recv().is_err(), "expected no message");
}

#[test]
fn broadcast_turn_appended_disconnected_receiver_no_panic() {
    let (tx, rx) = mpsc::channel::<SessionCommand>();
    let mut session = SessionState::empty();
    let (turn_id, _) = session.start_turn(Some("hello".into()));
    drop(rx);

    // Disconnected receiver should not panic — warn! is logged instead.
    broadcast_turn_appended(&tx, &session, 0, turn_id);
}

#[test]
fn broadcast_turn_appended_strips_reasoning_artifact() {
    // The client-bound TurnAppended must never carry the opaque reasoning
    // round-trip payload, even when the session's authoritative turn does.
    let (tx, rx) = mpsc::channel::<SessionCommand>();
    let mut session = SessionState::empty();
    let (turn_id, _) = session.start_turn(Some("hello".into()));
    session.set_assistant_response(
        turn_id,
        AssistantResponse {
            text: Some("hi".into()),
            reasoning: Some("thinking out loud".into()),
            reasoning_artifact: Some(ReasoningArtifact::ChatReasoning {
                field: ChatReasoningField::ReasoningContent,
                bytes: b"thinking".to_vec(),
            }),
            reasoning_producer: Some(ReasoningProducer {
                provider_slug: "deepseek".into(),
                model: "deepseek-v4-pro".into(),
            }),
            ..Default::default()
        },
    );

    broadcast_turn_appended(&tx, &session, 0, turn_id);

    match rx.try_recv() {
        Ok(SessionCommand::Broadcast(DaemonMessage::Session {
            event: SessionEvent::TurnAppended {
                turn_id: id, turn, ..
            },
            ..
        })) => {
            assert_eq!(id, turn_id);
            assert_eq!(turn.reasoning_artifact, None);
            assert_eq!(turn.reasoning_producer, None);
            assert_eq!(turn.assistant_text.as_deref(), Some("hi"));
            assert_eq!(
                turn.assistant_reasoning.as_deref(),
                Some("thinking out loud")
            );
        }
        Ok(_) => panic!("expected TurnAppended broadcast, got different command"),
        Err(e) => panic!("expected TurnAppended broadcast, got error: {e}"),
    }
    // The authoritative turn keeps the artifact for the next request's builder.
    assert!(session.turns[&turn_id].reasoning_artifact.is_some());
    assert!(session.turns[&turn_id].reasoning_producer.is_some());
}

#[test]
fn finalize_and_broadcast_turn_strips_reasoning_artifact() {
    // TurnAppended is the final turn snapshot sent to clients — the
    // reasoning artifact must be stripped here too, while the DB write
    // (inside finalize_and_broadcast_turn) persists the full turn.
    let (daemon_tx, _daemon_rx) = mpsc::channel::<DaemonCommand>();
    let (cmd_tx, cmd_rx) = mpsc::channel::<SessionCommand>();
    let dir = tempfile::tempdir().unwrap();
    let db = Arc::new(redb::Database::create(dir.path().join("test.redb")).unwrap());
    let ctx = RequestContext {
        cmd_tx,
        session_id: 1,
        db,
        tool_registry: ToolRegistry::new().build(),
        daemon_tx,
        max_turns: 0,
        lag_limits: crate::broadcast::LagLimits::default(),
        global_lag: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
    };
    let mut session = SessionState::empty();
    let (turn_id, _) = session.start_turn(Some("hello".into()));
    session.set_assistant_response(
        turn_id,
        AssistantResponse {
            text: Some("hi".into()),
            reasoning: Some("thinking out loud".into()),
            reasoning_artifact: Some(ReasoningArtifact::ChatReasoning {
                field: ChatReasoningField::ReasoningContent,
                bytes: b"thinking".to_vec(),
            }),
            reasoning_producer: Some(ReasoningProducer {
                provider_slug: "deepseek".into(),
                model: "deepseek-v4-pro".into(),
            }),
            ..Default::default()
        },
    );

    finalize_and_broadcast_turn(&mut session, &ctx, turn_id).unwrap();

    match cmd_rx.try_recv() {
        Ok(SessionCommand::Broadcast(DaemonMessage::Session {
            event: SessionEvent::TurnAppended { turn, .. },
            ..
        })) => {
            assert_eq!(turn.reasoning_artifact, None);
            assert_eq!(turn.reasoning_producer, None);
            assert_eq!(turn.assistant_text.as_deref(), Some("hi"));
            assert_eq!(
                turn.assistant_reasoning.as_deref(),
                Some("thinking out loud")
            );
        }
        Ok(_) => panic!("expected TurnAppended broadcast, got different command"),
        Err(e) => panic!("expected TurnAppended broadcast, got error: {e}"),
    }
    // The authoritative turn keeps the artifact after finalize + broadcast.
    assert!(session.turns[&turn_id].reasoning_artifact.is_some());
    assert!(session.turns[&turn_id].reasoning_producer.is_some());
}

#[test]
fn agent_loop_failure_marks_and_finalizes_turn() {
    // A provider-level failure (e.g. 402 Insufficient Balance) must be
    // recorded on the turn and the turn finalized + broadcast so clients
    // render a red "Error:" block in the transcript and the failure
    // survives a daemon restart — while the loop still reports the
    // original inference error to the caller (RequestOutcome::Failed).
    let (daemon_tx, _daemon_rx) = mpsc::channel::<DaemonCommand>();
    let (cmd_tx, cmd_rx) = mpsc::channel::<SessionCommand>();
    let dir = tempfile::tempdir().unwrap();
    let db = Arc::new(redb::Database::create(dir.path().join("test.redb")).unwrap());
    let ctx = RequestContext {
        cmd_tx,
        session_id: 1,
        db,
        tool_registry: ToolRegistry::new().build(),
        daemon_tx,
        max_turns: 0,
        lag_limits: crate::broadcast::LagLimits::default(),
        global_lag: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
    };
    let provider = make_failing_provider();
    let (_cancel_tx, cancel_rx) = crossbeam_channel::unbounded::<()>();
    let mut session = SessionState::empty();

    let result = run_agent_loop(
        &provider,
        &mut session,
        "test-model",
        7,
        &cancel_rx,
        &ctx,
        Some("hi".into()),
    );

    // The inference error propagates to the caller unchanged.
    let err = result.expect_err("the failing provider must fail the request");
    let msg = err.to_string();
    assert!(
        msg.contains("402") && msg.contains("Insufficient Balance"),
        "expected the 402 client error, got: {msg}"
    );

    // The open turn carries the failure so clients can render it.
    let turn = session.turns.get(&0).expect("turn 0 exists");
    assert_eq!(
        turn.error.as_deref(),
        Some("client error (402): Insufficient Balance")
    );
    assert_eq!(turn.user_text.as_deref(), Some("hi"));

    // The turn was finalized: a TurnAppended broadcast carries the error
    // to clients (the authoritative turn keeps it too, for the DB write).
    // The stream also contains mid-turn TurnAppended broadcasts (the
    // user-text append) that legitimately carry no error, so require only
    // that an error-bearing TurnAppended arrived.
    let mut saw_error_appended = false;
    while let Ok(msg) = cmd_rx.try_recv() {
        if let SessionCommand::Broadcast(DaemonMessage::Session {
            event: SessionEvent::TurnAppended { turn, .. },
            ..
        }) = msg
            && let Some(err) = turn.error
        {
            assert_eq!(err, "client error (402): Insufficient Balance");
            saw_error_appended = true;
        }
    }
    assert!(
        saw_error_appended,
        "expected a TurnAppended broadcast carrying the failure"
    );
}

// -- resolve_reasoning_effort tests ------------------------------------

#[test]
fn resolve_reasoning_effort_off_returns_off() {
    let provider = make_test_provider();
    let result = resolve_reasoning_effort(&provider, "o3-mini", 1, 0, "off");
    assert_eq!(result, "off");
}

#[test]
fn resolve_reasoning_effort_unknown_provider_disables() {
    let provider = make_test_provider();
    let result = resolve_reasoning_effort(&provider, "o3-mini", 1, 0, "low");
    // "test-stub" slug is not in the catalog, so reasoning is unsupported.
    assert_eq!(result, "off");
}

#[test]
fn resolve_reasoning_effort_openai_supported_model_preserves() {
    let config = choreo_ai_protocols::openai::ServiceConfig::default();
    let client = choreo_ai_protocols::openai::OpenAiClient::new(config, "test-key".into()).unwrap();
    let provider = InferenceProvider::from_openai(client);

    let result = resolve_reasoning_effort(&provider, "o3-mini", 1, 0, "high");
    assert_eq!(result, "high");
}

#[test]
fn resolve_reasoning_effort_openai_unsupported_model_disables() {
    let config = choreo_ai_protocols::openai::ServiceConfig::default();
    let client = choreo_ai_protocols::openai::OpenAiClient::new(config, "test-key".into()).unwrap();
    let provider = InferenceProvider::from_openai(client);

    let result = resolve_reasoning_effort(&provider, "gpt-4.1", 1, 0, "medium");
    assert_eq!(result, "off");
}

// -- estimate_prompt_tokens tests ------------------------------------

#[test]
fn estimate_prompt_tokens_empty() {
    let (encoding, estimated) = estimate_prompt_tokens("gpt-4", &[], &[]);
    assert!(encoding.is_some());
    assert_eq!(estimated, 0);
}

#[test]
fn estimate_prompt_tokens_counts_content() {
    let messages = vec![
        ChatRequestMessage::simple("user", "hello world".into()),
        ChatRequestMessage::simple("assistant", "hi there".into()),
    ];
    let (_, estimated) = estimate_prompt_tokens("gpt-4", &messages, &[]);
    assert!(
        estimated > 0,
        "expected positive token count, got {estimated}"
    );
}

#[test]
fn estimate_prompt_tokens_does_not_count_reasoning_content() {
    let base_messages = vec![
        ChatRequestMessage::simple("user", "hello".into()),
        ChatRequestMessage::simple("assistant", "visible".into()),
    ];
    let mut with_reasoning = base_messages.clone();
    with_reasoning[1].reasoning_content = Some("thinking deep...".into());

    let (_, base_est) = estimate_prompt_tokens("gpt-4", &base_messages, &[]);
    let (_, reason_est) = estimate_prompt_tokens("gpt-4", &with_reasoning, &[]);
    assert_eq!(
        base_est, reason_est,
        "legacy reasoning_content string field is never populated by the daemon and must not count"
    );
}

#[test]
fn estimate_prompt_tokens_counts_reasoning_artifact() {
    let base_messages = vec![
        ChatRequestMessage::simple("user", "hello".into()),
        ChatRequestMessage::simple("assistant", "visible".into()),
    ];
    let mut with_artifact = base_messages.clone();
    with_artifact[1].reasoning_artifact = Some(ReasoningArtifact::ChatReasoning {
        field: ChatReasoningField::ReasoningContent,
        bytes: "thinking deep...".into(),
    });

    let (_, base_est) = estimate_prompt_tokens("gpt-4", &base_messages, &[]);
    let (_, artifact_est) = estimate_prompt_tokens("gpt-4", &with_artifact, &[]);
    assert!(
        artifact_est > base_est,
        "replayed reasoning artifact should count as input: {artifact_est} <= {base_est}",
    );
}

#[test]
fn estimate_prompt_tokens_counts_tool_call_metadata() {
    let messages = vec![ChatRequestMessage {
        role: "assistant",
        content: None,
        tool_calls: Some(vec![AssistantToolCall {
            id: "call_abc".into(),
            kind: "function".into(),
            function: AssistantToolFunction {
                name: "read_file".into(),
                arguments: r#"{"path": "/etc/hosts"}"#.into(),
            },
        }]),
        tool_call_id: None,
        reasoning_content: None,
        reasoning: None,
        reasoning_text: None,
        reasoning_artifact: None,
    }];
    let (_, estimated) = estimate_prompt_tokens("gpt-4", &messages, &[]);
    assert!(
        estimated > 0,
        "expected positive token count, got {estimated}"
    );
}

#[test]
fn estimate_prompt_tokens_includes_tool_defs() {
    let tools = vec![ChatToolDefinition::function(
        "read_file",
        "Read a file from disk",
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": {"type": "string"}
            }
        }),
    )];
    let messages = vec![ChatRequestMessage::simple("user", "read file".into())];
    let (_, with_tools) = estimate_prompt_tokens("gpt-4", &messages, &tools);
    let (_, without_tools) = estimate_prompt_tokens("gpt-4", &messages, &[]);
    assert!(
        with_tools > without_tools,
        "tool defs should increase token count: {with_tools} <= {without_tools}",
    );
}

#[test]
fn estimate_prompt_tokens_unknown_model_falls_back() {
    let messages = vec![ChatRequestMessage::simple("user", "hello".into())];
    let (encoding, estimated) = estimate_prompt_tokens("nonexistent-model-9000", &messages, &[]);
    assert!(encoding.is_some(), "should fall back to cl100k_base");
    assert!(estimated > 0);
}

#[test]
fn estimate_prompt_tokens_no_chained_context_addend() {
    // The daemon builds `messages` as the FULL conversation, not the
    // chained tail the adapter puts on the wire; the provider bills the
    // whole context it holds in the chain, which the full-conversation
    // count already covers. There is deliberately NO chained-context
    // addend — adding the last request's `prompt_tokens` would count the
    // conversation twice. This pins the counting function's contract: it
    // estimates exactly the messages it is given, nothing more.
    let messages = vec![
        ChatRequestMessage::simple("system", "rebuilt system prompt".into()),
        ChatRequestMessage::simple("user", "turn one".into()),
        ChatRequestMessage::simple("assistant", "answer".into()),
        ChatRequestMessage::simple("user", "turn two".into()),
    ];
    let (_, estimated) = estimate_prompt_tokens("gpt-4", &messages, &[]);
    // Deterministic and equal to the visible-messages count: a hidden
    // chained-context addend would inflate it far beyond the recount.
    let (_, recounted) = estimate_prompt_tokens("gpt-4", &messages, &[]);
    assert_eq!(estimated, recounted, "estimate must be deterministic");
    assert!(
        estimated > 0,
        "full conversation must count: got {estimated}"
    );
}

// -- execute_tool_with_timeout tests -----------------------------------

struct FastTestTool;

impl Tool for FastTestTool {
    type Args = serde_json::Value;
    type Return = String;
    type Error = ToolExecError;

    fn name(&self) -> &'static str {
        "_test_fast"
    }
    fn group(&self) -> &'static str {
        "test"
    }
    fn description(&self) -> &'static str {
        "test tool that completes immediately"
    }
    fn describe_invocation(&self, _args: &Self::Args) -> String {
        format!("{}.", self.description())
    }
    fn return_string(ret: &Self::Return) -> String {
        ret.clone()
    }
    fn schema(&self) -> serde_json::Value {
        serde_json::json!({})
    }
    fn execute(
        &self,
        _args: Self::Args,
        _xc: Option<&ServiceCredential>,
        _working_dir: Option<&Path>,
        _ctx: Option<&ToolContext>,
    ) -> Result<Self::Return, Self::Error> {
        Ok("fast result".into())
    }
}

struct BlockingTestTool {
    proceed: std::sync::Mutex<Option<mpsc::Receiver<()>>>,
}

impl Tool for BlockingTestTool {
    type Args = serde_json::Value;
    type Return = String;
    type Error = ToolExecError;

    fn name(&self) -> &'static str {
        "_test_blocking"
    }
    fn group(&self) -> &'static str {
        "test"
    }
    fn description(&self) -> &'static str {
        "test tool that blocks until proceed"
    }
    fn describe_invocation(&self, _args: &Self::Args) -> String {
        format!("{}.", self.description())
    }
    fn return_string(ret: &Self::Return) -> String {
        ret.clone()
    }
    fn schema(&self) -> serde_json::Value {
        serde_json::json!({})
    }
    fn execute(
        &self,
        _args: Self::Args,
        _xc: Option<&ServiceCredential>,
        _working_dir: Option<&Path>,
        _ctx: Option<&ToolContext>,
    ) -> Result<Self::Return, Self::Error> {
        Ok("ignored".into())
    }
    fn execute_streaming(
        &self,
        _args: Self::Args,
        _xc: Option<&ServiceCredential>,
        _working_dir: Option<&Path>,
        _output_tx: crossbeam_channel::Sender<Vec<u8>>,
        _ctx: Option<&ToolContext>,
    ) -> Result<Self::Return, Self::Error> {
        if let Some(rx) = self.proceed.lock().unwrap().take() {
            let _ = rx.recv();
        }
        Ok("blocked tool done".into())
    }
}

fn run_exec_tool(
    tool: impl Tool + 'static,
    tool_name: &str,
    tool_args: &str,
    timeout_dur: Duration,
    cancel_rx: crossbeam_channel::Receiver<()>,
) -> (ToolOutput, bool, mpsc::Receiver<SessionCommand>) {
    let (daemon_tx, _daemon_rx) = mpsc::channel::<DaemonCommand>();
    let (cmd_tx, cmd_rx) = mpsc::channel::<SessionCommand>();

    let dir = tempfile::tempdir().expect("tempdir");
    let db = redb::Database::create(dir.path().join("test.redb")).expect("Database");

    let mut session = SessionState::empty();

    let mut registry = ToolRegistry::new();
    registry.register(tool);
    let registry = registry.build();

    let tool_call = ChatToolCall {
        id: "call_test".into(),
        name: tool_name.into(),
        arguments_json: tool_args.into(),
        caller: None,
    };

    let ctx = RequestContext {
        cmd_tx,
        session_id: 1,
        db: Arc::new(db),
        tool_registry: registry,
        daemon_tx,
        max_turns: 0,
        lag_limits: crate::broadcast::LagLimits::default(),
        global_lag: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
    };
    let (result, cancelled, _image) = execute_tool_with_timeout(
        &tool_call,
        None,
        None,
        timeout_dur,
        1,
        1,
        &mut session,
        &cancel_rx,
        &ctx,
        "test invocation",
    );
    (result, cancelled, cmd_rx)
}

#[test]
fn execute_tool_normal_completion() {
    let (_cancel_tx, cancel_rx) = crossbeam_channel::unbounded::<()>();
    let (result, cancelled, _cmd_rx) = run_exec_tool(
        FastTestTool,
        "_test_fast",
        "{}",
        Duration::from_secs(60),
        cancel_rx,
    );
    assert!(!result.is_error, "expected success: {}", result.content);
    assert!(result.content.contains("fast result"), "{}", result.content);
    assert!(!cancelled, "completion must not report a cancel");
}

#[test]
fn execute_tool_cancelled_before_execution() {
    let (cancel_tx, cancel_rx) = crossbeam_channel::unbounded::<()>();
    cancel_tx.send(()).expect("send cancel");
    drop(cancel_tx);

    // A blocking tool makes the outcome deterministic: the cancel is
    // pre-sent, so the wait-loop's biased cancel arm fires first and,
    // with the tool still running, the "cancelled" output is produced
    // (a just-completed fast tool could otherwise race the cancel drain
    // and return its real result alongside the sticky cancel flag).
    let (proceed_tx, proceed_rx) = mpsc::channel::<()>();
    let (result, cancelled, _cmd_rx) = run_exec_tool(
        BlockingTestTool {
            proceed: std::sync::Mutex::new(Some(proceed_rx)),
        },
        "_test_blocking",
        "{}",
        Duration::from_secs(60),
        cancel_rx,
    );
    assert!(result.is_error, "expected error: {}", result.content);
    assert!(result.content.contains("cancelled"), "{}", result.content);
    // The wait observed the cancellation signal; the caller must stop the
    // request (the sticky-cancel contract).
    assert!(cancelled, "cancel must be reported to the caller");
    // The serial path now carries the invocation description onto the
    // synthesized cancel output (the concurrent path always did), so the
    // transcript renders the same invocation context for both phases.
    assert_eq!(result.invocation_description, "test invocation");

    // Release the still-blocked tool so its execution thread exits.
    drop(proceed_tx);
}

#[test]
fn execute_tool_timeout() {
    let (_cancel_tx, cancel_rx) = crossbeam_channel::unbounded::<()>();
    let (proceed_tx, proceed_rx) = mpsc::channel::<()>();

    // A zero-duration timeout is deterministic: `crossbeam_channel::after`
    // with a deadline that is already in the past is immediately ready in
    // the biased select's fast path (the `at` flavor's `try_recv` checks
    // `Instant::now() >= delivery_time`), so the deadline arm fires
    // without any time-based wait — no sleeps in unit tests (AGENTS.md).
    // The blocking tool can never win the race (it has not finished), and
    // no cancel is pending, so the deadline arm is the only ready one.
    let (result, cancelled, _cmd_rx) = run_exec_tool(
        BlockingTestTool {
            proceed: std::sync::Mutex::new(Some(proceed_rx)),
        },
        "_test_blocking",
        "{}",
        Duration::ZERO,
        cancel_rx,
    );

    assert!(result.is_error, "expected error: {}", result.content);
    assert!(result.content.contains("timed out"), "{}", result.content);
    assert!(!cancelled, "a timeout is not a cancellation");

    drop(proceed_tx);
}

// -- drain_queued_or_synthesize tests ---------------------------------
//
// Deterministic: each test fully populates (or deliberately leaves
// empty / disconnects) the exec channel before calling the function, so
// the outcome is fully ordered — no time-based waits (AGENTS.md).

#[test]
fn drain_queued_result_beats_stop_message() {
    // A result that queued in the same instant the stop fired must win
    // over the synthesized stop message — the finish-vs-stop race is
    // resolved in favor of the real outcome.
    let (tx, rx) = crossbeam_channel::unbounded::<Result<ToolOutput, ToolError>>();
    tx.send(Ok(ToolOutput {
        content: "real result".into(),
        invocation_description: "real desc".into(),
        ..Default::default()
    }))
    .expect("send result");

    let (output, cancelled) = drain_queued_or_synthesize(
        "_test",
        std::time::Instant::now(),
        "test invocation",
        &rx,
        "tool '_test' cancelled".to_string(),
        true,
    );
    assert_eq!(output.content, "real result");
    assert!(!output.is_error);
    assert_eq!(output.invocation_description, "real desc");
    // The sticky-cancel flag still travels with a drained result: the
    // cancel signal was consumed, so the caller must stop the request.
    assert!(cancelled, "sticky cancel must survive a drained result");
}

#[test]
fn drain_queued_empty_synthesizes_stop_message() {
    // No result queued (sender alive, tool still running): the stop
    // message is synthesized with the caller's invocation description.
    let (_tx, rx) = crossbeam_channel::unbounded::<Result<ToolOutput, ToolError>>();
    let (output, cancelled) = drain_queued_or_synthesize(
        "_test",
        std::time::Instant::now(),
        "test invocation",
        &rx,
        "tool '_test' timed out after 60s".to_string(),
        false,
    );
    assert_eq!(output.content, "tool '_test' timed out after 60s");
    assert!(output.is_error);
    assert_eq!(output.invocation_description, "test invocation");
    assert!(!cancelled, "a timeout is not a request cancel");
}

#[test]
fn drain_queued_disconnected_reports_panic_not_stop() {
    // The execution thread died (sender dropped) at the stop instant: the
    // real cause is a panic, not the stop message — a deadline/cancel arm
    // must not mislabel a dead execution thread as "timed out" or
    // "cancelled".
    let (tx, rx) = crossbeam_channel::unbounded::<Result<ToolOutput, ToolError>>();
    drop(tx);
    let (output, cancelled) = drain_queued_or_synthesize(
        "_test",
        std::time::Instant::now(),
        "test invocation",
        &rx,
        "tool '_test' cancelled".to_string(),
        true,
    );
    assert_eq!(output.content, "tool execution thread panicked");
    assert!(output.is_error);
    assert_eq!(output.invocation_description, "test invocation");
    assert!(cancelled, "sticky flag still applies on the panic path");
}

struct StreamingTestTool;

impl Tool for StreamingTestTool {
    type Args = serde_json::Value;
    type Return = String;
    type Error = ToolExecError;

    fn name(&self) -> &'static str {
        "_test_streaming"
    }
    fn group(&self) -> &'static str {
        "test"
    }
    fn description(&self) -> &'static str {
        "test tool that sends streaming output"
    }
    fn describe_invocation(&self, _args: &Self::Args) -> String {
        format!("{}.", self.description())
    }
    fn return_string(ret: &Self::Return) -> String {
        ret.clone()
    }
    fn schema(&self) -> serde_json::Value {
        serde_json::json!({})
    }
    fn execute(
        &self,
        _args: Self::Args,
        _xc: Option<&ServiceCredential>,
        _working_dir: Option<&Path>,
        _ctx: Option<&ToolContext>,
    ) -> Result<Self::Return, Self::Error> {
        Ok("exec result".into())
    }
    fn supports_streaming_output() -> bool {
        true
    }

    fn execute_streaming(
        &self,
        _args: Self::Args,
        _xc: Option<&ServiceCredential>,
        _working_dir: Option<&Path>,
        output_tx: crossbeam_channel::Sender<Vec<u8>>,
        _ctx: Option<&ToolContext>,
    ) -> Result<Self::Return, Self::Error> {
        let _ = output_tx.send(b"streamed payload".to_vec());
        Ok("streaming done".into())
    }
}

#[test]
fn execute_tool_forwards_streaming_output() {
    let (_cancel_tx, cancel_rx) = crossbeam_channel::unbounded::<()>();
    let (result, cancelled, cmd_rx) = run_exec_tool(
        StreamingTestTool,
        "_test_streaming",
        "{}",
        Duration::from_secs(60),
        cancel_rx,
    );

    assert!(!result.is_error, "expected success: {}", result.content);
    assert!(
        result.content.contains("streaming done"),
        "{}",
        result.content
    );
    assert!(!cancelled, "completion must not report a cancel");

    // The invocation description is no longer streamed as a chunk (it
    // rides on ToolCallStarted + the seeded placeholder); the only chunk
    // is the tool's own payload from execute_streaming.
    match cmd_rx.recv() {
        Ok(SessionCommand::Broadcast(DaemonMessage::Session {
            event: SessionEvent::ToolResultChunk { data, .. },
            ..
        })) => {
            assert_eq!(data, b"streamed payload");
        }
        Ok(_other) => panic!("expected ToolResultChunk, got unexpected SessionCommand"),
        Err(e) => panic!("channel disconnected while waiting for streaming output: {e}"),
    }
}

// -- forwarding-thread tests -----------------------------------------
//
// These exercise `spawn_forwarding_thread` directly and deterministically:
// the returned `JoinHandle` lets the test observe thread exit without any
// time-based waits (AGENTS.md forbids sleeps in unit tests).

#[test]
fn forwarding_thread_drains_queued_output_before_kill() {
    let (cmd_tx, cmd_rx) = mpsc::channel::<SessionCommand>();
    let (output_tx, output_rx) = crossbeam_channel::unbounded::<Vec<u8>>();
    let (kill_tx, kill_rx) = crossbeam_channel::unbounded::<()>();

    let handle = spawn_forwarding_thread(cmd_tx, 1, 1, "call_1".into(), output_rx, kill_rx);

    // Queue a chunk and then a kill back-to-back. The test sends the chunk
    // BEFORE the kill, so the forwarder can never observe the kill arm as
    // ready while the output arm is still pending — the biased select
    // (output first) must therefore forward the chunk before honoring the
    // kill, in any interleaving.
    output_tx
        .send(b"queued chunk".to_vec())
        .expect("send chunk");
    kill_tx.send(()).expect("send kill");

    match cmd_rx.recv() {
        Ok(SessionCommand::Broadcast(DaemonMessage::Session {
            event: SessionEvent::ToolResultChunk { data, .. },
            ..
        })) => {
            assert_eq!(data, b"queued chunk");
        }
        Ok(_other) => panic!("expected ToolResultChunk, got unexpected SessionCommand"),
        Err(e) => panic!("channel disconnected while waiting for chunk: {e}"),
    }
    // Only the kill can terminate the forwarder now (output_tx still alive),
    // so a successful join proves the kill was honored after the drain.
    handle.join().expect("forwarder should exit after kill");
}

#[test]
fn forwarding_thread_exits_when_output_disconnects() {
    let (cmd_tx, cmd_rx) = mpsc::channel::<SessionCommand>();
    let (output_tx, output_rx) = crossbeam_channel::unbounded::<Vec<u8>>();
    let (_kill_tx, kill_rx) = crossbeam_channel::unbounded::<()>();

    let handle = spawn_forwarding_thread(cmd_tx, 1, 1, "call_2".into(), output_rx, kill_rx);

    output_tx.send(b"last chunk".to_vec()).expect("send chunk");
    // Tool finished: dropping the output sender makes the forwarder's next
    // `recv(output_rx)` return Err (disconnect) → drain → exit.
    drop(output_tx);

    match cmd_rx.recv() {
        Ok(SessionCommand::Broadcast(DaemonMessage::Session {
            event: SessionEvent::ToolResultChunk { data, .. },
            ..
        })) => {
            assert_eq!(data, b"last chunk");
        }
        Ok(_other) => panic!("expected ToolResultChunk, got unexpected SessionCommand"),
        Err(e) => panic!("channel disconnected while waiting for chunk: {e}"),
    }
    handle
        .join()
        .expect("forwarder should exit on output disconnect");
}

#[test]
fn forwarding_thread_exits_when_kill_sender_dropped() {
    let (cmd_tx, _cmd_rx) = mpsc::channel::<SessionCommand>();
    let (output_tx, output_rx) = crossbeam_channel::unbounded::<Vec<u8>>();
    let (kill_tx, kill_rx) = crossbeam_channel::unbounded::<()>();

    let handle = spawn_forwarding_thread(cmd_tx, 1, 1, "call_3".into(), output_rx, kill_rx);

    // Dropping the kill sender disconnects kill_rx; with no output traffic
    // the select returns on the kill arm immediately and the thread exits.
    drop(kill_tx);
    handle
        .join()
        .expect("forwarder should exit when kill sender dropped");
    drop(output_tx);
}

#[test]
fn forwarding_thread_honors_kill_while_output_is_still_alive() {
    // A tool that keeps streaming keeps the output arm always-ready, which
    // would starve the biased-last kill arm if the forwarder only checked
    // the kill channel via the select.  The between-chunk kill re-check
    // must stop the thread even though the output sender is still alive
    // and has chunks queued — otherwise a busy stream would forward
    // forever after the caller stopped waiting.  Deterministic: the test
    // sends chunks and then a kill; the forwarder forwards the queued
    // burst (bounded by the queue length at kill time) and then exits,
    // never waiting on the output channel to disconnect.
    let (cmd_tx, cmd_rx) = mpsc::channel::<SessionCommand>();
    let (output_tx, output_rx) = crossbeam_channel::unbounded::<Vec<u8>>();
    let (kill_tx, kill_rx) = crossbeam_channel::unbounded::<()>();

    let handle = spawn_forwarding_thread(cmd_tx, 1, 1, "call_4".into(), output_rx, kill_rx);

    for i in 0..5 {
        output_tx
            .send(format!("chunk {i}").into_bytes())
            .expect("send chunk");
    }
    kill_tx.send(()).expect("send kill");

    // The first queued chunk is forwarded (FIFO) before the kill is
    // honored; the rest of the kill-time burst may be drained too.
    match cmd_rx.recv() {
        Ok(SessionCommand::Broadcast(DaemonMessage::Session {
            event: SessionEvent::ToolResultChunk { data, .. },
            ..
        })) => {
            assert_eq!(data, b"chunk 0", "first queued chunk should be forwarded");
        }
        Ok(_other) => panic!("expected ToolResultChunk, got unexpected SessionCommand"),
        Err(e) => panic!("channel disconnected while waiting for chunk: {e}"),
    }
    // output_tx is still alive, so the thread can only terminate by
    // honoring the kill between chunks — a successful join proves the
    // busy-stream kill starvation is closed.
    handle
        .join()
        .expect("forwarder should exit on kill while output is still live");
    drop(output_tx);
    drop(kill_tx);
}

// -- determine_tool_timeout tests ----------------------------------

#[test]
fn determine_tool_timeout_subsession_none() {
    assert!(determine_tool_timeout("spawn_subsession").is_none());
}

#[test]
fn determine_tool_timeout_shell_300() {
    for name in &["sh", "nushell", "fish", "exec"] {
        assert_eq!(
            determine_tool_timeout(name),
            Some(Duration::from_secs(300)),
            "tool {name} should have 300s timeout",
        );
    }
}

#[test]
fn determine_tool_timeout_default_60() {
    for name in &[
        "read_file",
        "write_file",
        "run_riscv",
        "grep",
        "http_request",
    ] {
        assert_eq!(
            determine_tool_timeout(name),
            Some(Duration::from_secs(60)),
            "tool {name} should have 60s timeout",
        );
    }
}

// -- spawn_single_tool tests ---------------------------------------

/// Build a throwaway `ToolContext` and command channel for
/// `spawn_single_tool` tests. Receivers are dropped, which is fine — no
/// assertion inspects the daemon or session command streams here.
fn spawn_test_ctx() -> (ToolContext, mpsc::Sender<SessionCommand>) {
    let (cmd_tx, _cmd_rx) = mpsc::channel::<SessionCommand>();
    let (_daemon_tx, _daemon_rx) = mpsc::channel::<DaemonCommand>();
    let dir = tempfile::tempdir().expect("tempdir");
    let db = Arc::new(redb::Database::create(dir.path().join("test.redb")).expect("Database"));
    let ctx = ToolContext {
        session_id: 1,
        db,
        daemon_tx: _daemon_tx,
        active_tool_groups: std::collections::HashSet::new(),
        reasoning_effort: None,
        selected_model: None,
        working_dir: None,
        cancelled: Arc::new(AtomicBool::new(false)),
        account_name: None,
    };
    (ctx, cmd_tx)
}

fn run_spawn_single_tool(
    tool: impl Tool + 'static,
    tool_name: &str,
    tool_args: &str,
    timeout: Option<Duration>,
) -> ToolHandle {
    let (ctx, cmd_tx) = spawn_test_ctx();

    let mut registry = ToolRegistry::new();
    registry.register(tool);
    let registry = registry.build();

    let tool_call = ChatToolCall {
        id: "call_test".into(),
        name: tool_name.into(),
        arguments_json: tool_args.into(),
        caller: None,
    };

    let invocation_description = registry
        .describe_invocation_for(&tool_call.name, &tool_call.arguments_json)
        .unwrap_or_default();

    let (result_tx, result_rx) = crossbeam_channel::unbounded::<ToolHandle>();
    // Hold the kill sender for the duration of this wait — dropping it
    // would disconnect the kill channel and stop the wait-loop early.
    let _kill_tx = spawn_single_tool(SpawnToolArgs {
        tool_call,
        timeout,
        request_id: 1,
        session_id: 1,
        registry,
        cmd_tx,
        x_credentials: None,
        working_dir: None,
        ctx,
        invocation_description,
        started_at: Instant::now(),
        result_tx,
    });

    result_rx.recv().expect("tool did not deliver a result")
}

#[test]
fn spawn_single_tool_fast_returns_result() {
    let handle = run_spawn_single_tool(
        FastTestTool,
        "_test_fast",
        "{}",
        Some(Duration::from_secs(60)),
    );
    assert!(
        !handle.output.is_error,
        "expected success: {}",
        handle.output.content
    );
    assert!(
        handle.output.content.contains("fast result"),
        "{}",
        handle.output.content
    );
    assert!(handle.image.is_none(), "expected no image from fast tool");
}

#[test]
fn spawn_single_tool_no_timeout_still_completes() {
    let handle = run_spawn_single_tool(FastTestTool, "_test_fast", "{}", None);
    assert!(
        !handle.output.is_error,
        "expected success: {}",
        handle.output.content
    );
    assert!(
        handle.output.content.contains("fast result"),
        "{}",
        handle.output.content
    );
}

#[test]
fn concurrent_tools_deliver_in_completion_order() {
    // Tool A (dispatched first) blocks until released; tool B (dispatched
    // second) completes immediately. B must arrive through the shared
    // batch channel before A — a fast tool is no longer gated by the
    // slowest tool the model listed before it.
    let (proceed_tx, proceed_rx) = mpsc::channel::<()>();
    let (ctx, cmd_tx) = spawn_test_ctx();

    let mut registry = ToolRegistry::new();
    registry.register(BlockingTestTool {
        proceed: std::sync::Mutex::new(Some(proceed_rx)),
    });
    registry.register(FastTestTool);
    let registry = registry.build();

    let slow_call = ChatToolCall {
        id: "call_slow".into(),
        name: "_test_blocking".into(),
        arguments_json: "{}".into(),
        caller: None,
    };
    let fast_call = ChatToolCall {
        id: "call_fast".into(),
        name: "_test_fast".into(),
        arguments_json: "{}".into(),
        caller: None,
    };

    let (batch_tx, batch_rx) = crossbeam_channel::unbounded::<ToolHandle>();

    // Dispatch the slow tool first, then the fast one.  The slow tool's
    // 5s timeout is a deadlock guard only: in the correct implementation
    // the fast result arrives immediately and the slow tool is released
    // well before its deadline, so the blocking `recv`s below never wait
    // on a timer — they are deterministic (AGENTS.md forbids time-based
    // waits in unit tests).  If a regression made the collector wait in
    // dispatch order, the slow tool would hit its timeout instead and
    // the name assertion below would fail the test rather than hang it.
    // The kill senders are held for the drain's lifetime — dropping them
    // would disconnect the kill channels and stop the wait-loops early.
    let _slow_kill = spawn_single_tool(SpawnToolArgs {
        tool_call: slow_call,
        timeout: Some(Duration::from_secs(5)),
        request_id: 1,
        session_id: 1,
        registry: Arc::clone(&registry),
        cmd_tx: cmd_tx.clone(),
        x_credentials: None,
        working_dir: None,
        ctx: ctx.clone(),
        invocation_description: String::new(),
        started_at: Instant::now(),
        result_tx: batch_tx.clone(),
    });
    let _fast_kill = spawn_single_tool(SpawnToolArgs {
        tool_call: fast_call,
        timeout: Some(Duration::from_secs(60)),
        request_id: 1,
        session_id: 1,
        registry,
        cmd_tx,
        x_credentials: None,
        working_dir: None,
        ctx,
        invocation_description: String::new(),
        started_at: Instant::now(),
        result_tx: batch_tx,
    });

    // The fast tool must arrive first despite being dispatched second.
    let first = batch_rx.recv().expect("expected a first tool result");
    assert_eq!(first.tool_call.name, "_test_fast");
    assert!(
        first.output.content.contains("fast result"),
        "{}",
        first.output.content
    );

    // Release the slow tool; its result arrives second.
    drop(proceed_tx);
    let second = batch_rx.recv().expect("expected the slow tool result");
    assert_eq!(second.tool_call.name, "_test_blocking");
    assert!(
        second.output.content.contains("blocked tool done"),
        "{}",
        second.output.content
    );
}

#[test]
fn wait_loop_honors_kill_while_tool_is_still_running() {
    // A tool that blocks until released; a collector kill must stop the
    // wait-loop (forwarder + cooperative flag) and deliver a "cancelled"
    // result immediately, without waiting for the tool to finish — even
    // with NO timeout, where the wait-loop would otherwise block on the
    // tool's result channel forever.
    let (proceed_tx, proceed_rx) = mpsc::channel::<()>();
    let (ctx, cmd_tx) = spawn_test_ctx();
    let cancel_flag = Arc::clone(&ctx.cancelled);

    let mut registry = ToolRegistry::new();
    registry.register(BlockingTestTool {
        proceed: std::sync::Mutex::new(Some(proceed_rx)),
    });
    let registry = registry.build();

    let tool_call = ChatToolCall {
        id: "call_kill".into(),
        name: "_test_blocking".into(),
        arguments_json: "{}".into(),
        caller: None,
    };

    let (result_tx, result_rx) = crossbeam_channel::unbounded::<ToolHandle>();
    let kill_tx = spawn_single_tool(SpawnToolArgs {
        tool_call,
        timeout: None, // unbounded — the kill is the only wakeup
        request_id: 1,
        session_id: 1,
        registry,
        cmd_tx,
        x_credentials: None,
        working_dir: None,
        ctx,
        invocation_description: String::new(),
        started_at: Instant::now(),
        result_tx,
    });

    // The tool is blocked inside execute_streaming_json; the kill must
    // reach the wait-loop and produce a cancelled result without the tool
    // finishing.  Deterministic: `recv` blocks until the cancelled handle
    // arrives, and the wait-loop sends it only after setting the flag.
    kill_tx.send(()).expect("send kill");

    let handle = result_rx
        .recv()
        .expect("cancelled result must be delivered");
    assert!(handle.output.is_error, "{}", handle.output.content);
    assert!(
        handle.output.content.contains("cancelled"),
        "{}",
        handle.output.content
    );
    // The cooperative flag must be set so the tool itself can stop early.
    assert!(
        cancel_flag.load(Ordering::Relaxed),
        "cooperative cancel flag must be set"
    );

    // Release the still-blocked tool so its execution thread exits.
    drop(proceed_tx);
}

// -- missing_calls tests ------------------------------------------------

#[test]
fn missing_calls_skips_delivered_by_id_not_index() {
    // A (slow, dies before delivering), B (fast, delivered), C (slow,
    // dies): handles arrive in completion order, so B is delivered first
    // and received == 1.  Skipping the first 1 entry by *index* would
    // mark A delivered and synthesize C in its place — misattributing the
    // panic to the wrong tool.  Filtering by call_id must synthesize A
    // and C (in dispatch order), never B.
    let call_infos = vec![
        CallInfo {
            call_id: "a".into(),
            tool_name: "slow_1".into(),
            arguments_json: "{}".into(),
            invocation_description: "a".into(),
            started_at: Instant::now(),
            // Never sent to — the kill channel is irrelevant to the
            // missing-call filter under test.
            kill_tx: crossbeam_channel::unbounded().0,
        },
        CallInfo {
            call_id: "b".into(),
            tool_name: "fast".into(),
            arguments_json: "{}".into(),
            invocation_description: "b".into(),
            started_at: Instant::now(),
            kill_tx: crossbeam_channel::unbounded().0,
        },
        CallInfo {
            call_id: "c".into(),
            tool_name: "slow_2".into(),
            arguments_json: "{}".into(),
            invocation_description: "c".into(),
            started_at: Instant::now(),
            kill_tx: crossbeam_channel::unbounded().0,
        },
    ];
    let delivered = HashSet::from(["b".to_string()]);
    let missing: Vec<&str> = missing_calls(&call_infos, &delivered)
        .map(|info| info.call_id.as_str())
        .collect();
    assert_eq!(missing, vec!["a", "c"]);
}

#[test]
fn missing_calls_empty_when_all_delivered() {
    let call_infos = vec![CallInfo {
        call_id: "a".into(),
        tool_name: "read_file".into(),
        arguments_json: "{}".into(),
        invocation_description: "a".into(),
        started_at: Instant::now(),
        // Never sent to — the kill channel is irrelevant to the
        // missing-call filter under test.
        kill_tx: crossbeam_channel::unbounded().0,
    }];
    let delivered = HashSet::from(["a".to_string()]);
    assert_eq!(missing_calls(&call_infos, &delivered).count(), 0);
}

// -- sort_by_call_order tests -----------------------------------------

#[test]
fn sort_by_call_order_restores_model_order() {
    // Model issued calls a, b, c; the tools completed in the reverse order
    // (c first). The next-call accumulator must be restored to a, b, c so
    // tool messages mirror the assistant's tool_calls array.
    let tool_calls = vec![
        AssistantToolCallRecord {
            call_id: "a".into(),
            name: "read_file".into(),
            arguments_json: "{}".into(),
        },
        AssistantToolCallRecord {
            call_id: "b".into(),
            name: "grep".into(),
            arguments_json: "{}".into(),
        },
        AssistantToolCallRecord {
            call_id: "c".into(),
            name: "sh".into(),
            arguments_json: "{}".into(),
        },
    ];
    let mut items = vec![
        ToolResultItem {
            call_id: "c".into(),
            output: "c-out".into(),
            caller: None,
        },
        ToolResultItem {
            call_id: "a".into(),
            output: "a-out".into(),
            caller: None,
        },
        ToolResultItem {
            call_id: "b".into(),
            output: "b-out".into(),
            caller: None,
        },
    ];
    sort_by_call_order(&tool_calls, &mut items, |r| r.call_id.as_str());
    let order: Vec<_> = items.iter().map(|r| r.call_id.as_str()).collect();
    assert_eq!(order, vec!["a", "b", "c"]);
}

#[test]
fn sort_by_call_order_sinks_unknown_call_ids() {
    // A streaming stub created before its start event arrived has no
    // matching tool_call; it must sink to the end, keeping relative order.
    let tool_calls = vec![AssistantToolCallRecord {
        call_id: "a".into(),
        name: "read_file".into(),
        arguments_json: "{}".into(),
    }];
    let mut items = vec![
        ToolResultItem {
            call_id: "ghost".into(),
            output: "g-out".into(),
            caller: None,
        },
        ToolResultItem {
            call_id: "a".into(),
            output: "a-out".into(),
            caller: None,
        },
    ];
    sort_by_call_order(&tool_calls, &mut items, |r| r.call_id.as_str());
    let order: Vec<_> = items.iter().map(|r| r.call_id.as_str()).collect();
    assert_eq!(order, vec!["a", "ghost"]);
}

// -- extract_json_string tests ------------------------------------------

#[test]
fn extract_json_string_gets_value() {
    let json = r#"{"name": "test-skill", "path": "src/main.rs"}"#;
    assert_eq!(
        extract_json_string(json, "name").as_deref(),
        Some("test-skill")
    );
    assert_eq!(
        extract_json_string(json, "path").as_deref(),
        Some("src/main.rs")
    );
}

#[test]
fn extract_json_string_missing_key() {
    assert_eq!(extract_json_string(r#"{"other": "val"}"#, "name"), None);
}

#[test]
fn extract_json_string_invalid_json() {
    assert_eq!(extract_json_string("not json", "name"), None);
}

// -- persist_loaded_skill tests -----------------------------------------

#[test]
fn persist_loaded_skill_adds_to_session() {
    let dir = tempfile::tempdir().unwrap();
    let skill_dir = dir.path().join(".agents/skills/test-skill");
    std::fs::create_dir_all(&skill_dir).unwrap();
    std::fs::write(
        skill_dir.join("SKILL.md"),
        "\
---\n\
name: test-skill\n\
description: A test skill\n\
---\n\
Hello, this is the skill body.\n\
---\n",
    )
    .unwrap();

    let mut session = SessionState::empty();
    session.config.working_dir = Some(dir.path().to_path_buf());
    assert!(session.loaded_skill_bodies.is_empty());

    persist_loaded_skill(&mut session, "load_skill", r#"{"name": "test-skill"}"#);

    assert_eq!(session.loaded_skill_bodies.len(), 1);
    assert_eq!(session.loaded_skill_bodies[0].name, "test-skill");
    assert!(session.loaded_skill_bodies[0].body.contains("skill body"));
}

#[test]
fn persist_loaded_skill_skips_non_load_skill() {
    let mut session = SessionState::empty();
    persist_loaded_skill(&mut session, "read_file", r#"{"path": "Cargo.toml"}"#);
    assert!(session.loaded_skill_bodies.is_empty());
}

#[test]
fn persist_loaded_skill_skips_missing_name() {
    let mut session = SessionState::empty();
    session.config.working_dir = Some(PathBuf::from("/tmp"));
    persist_loaded_skill(&mut session, "load_skill", r#"{}"#);
    assert!(session.loaded_skill_bodies.is_empty());
}

#[test]
fn persist_loaded_skill_skips_without_working_dir() {
    let mut session = SessionState::empty();
    persist_loaded_skill(&mut session, "load_skill", r#"{"name": "test-skill"}"#);
    assert!(session.loaded_skill_bodies.is_empty());
}

// -- build_system_content tests -----------------------------------------

fn setup_build_system_content_session() -> (SessionState, Arc<ToolRegistry>, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("AGENTS.md"), "Project rules").unwrap();

    let mut registry = ToolRegistry::new();
    registry.register(FastTestTool);
    let registry = registry.build();

    let mut session = SessionState::empty();
    session.config.working_dir = Some(dir.path().to_path_buf());
    (session, registry, dir)
}

/// Call build_system_content with standard defaults derived from the
/// session state and optional pending_hints overrides.
fn test_build_content(
    session: &mut SessionState,
    registry: &ToolRegistry,
    pending_hints: &[String],
) -> Option<String> {
    build_system_content(
        SystemContentParams {
            working_dir: session.config.working_dir.as_deref(),
            context_config: &session.config.context_config,
            skills: &[],
            loaded_skill_bodies: &session.loaded_skill_bodies,
            tool_registry: registry,
            pending_hints,
            session_title: session.config.title.as_deref(),
        },
        &mut session.context_cache,
    )
}

#[test]
fn build_system_content_with_working_dir() {
    let (mut session, registry, _dir) = setup_build_system_content_session();
    let content = test_build_content(&mut session, &registry, &[]);
    assert!(content.is_some());
    let content = content.unwrap();
    assert!(content.contains("Tool groups"));
    assert!(content.contains("core"));
    assert!(content.contains("Project rules"));
}

#[test]
fn build_system_content_without_working_dir() {
    let mut session = SessionState::empty();
    let registry = ToolRegistry::new().build();
    let content = test_build_content(&mut session, &registry, &[]);
    assert!(content.is_none());
}

#[test]
fn build_system_content_includes_loaded_skills() {
    let (mut session, registry, _dir) = setup_build_system_content_session();
    session.loaded_skill_bodies.push(LoadedSkill {
        name: "loaded-test".to_string(),
        body: "Loaded body text.".to_string(),
    });
    let content = test_build_content(&mut session, &registry, &[]);
    assert!(content.is_some());
    let content = content.unwrap();
    assert!(content.contains("Loaded skills"));
    assert!(content.contains("loaded-test"));
    assert!(content.contains("Loaded body text."));
}

#[test]
fn build_system_content_populates_context_cache() {
    let (mut session, registry, _dir) = setup_build_system_content_session();
    assert!(session.context_cache.is_none());

    let _ = test_build_content(&mut session, &registry, &[]);
    assert!(
        session.context_cache.is_some(),
        "context_cache should be populated after first call"
    );
    let (fp, _) = session.context_cache.as_ref().unwrap();
    assert!(*fp > 0, "fingerprint should be non-zero");
}

#[test]
fn build_system_content_includes_pending_hints() {
    let (mut session, registry, _dir) = setup_build_system_content_session();
    let pending_hints = vec!["Hint about subdirectory config.".to_string()];
    let content = test_build_content(&mut session, &registry, &pending_hints);
    assert!(content.is_some());
    let content = content.unwrap();
    assert!(content.contains("New context from project subdirectories"));
    assert!(content.contains("Hint about subdirectory config."));
}

#[test]
fn build_system_content_includes_session_title() {
    let (mut session, registry, _dir) = setup_build_system_content_session();
    session.config.title = Some("Refactoring the database layer".into());
    let content = test_build_content(&mut session, &registry, &[]);
    assert!(content.is_some());
    let content = content.unwrap();
    assert!(content.contains("## Current Session Title"));
    assert!(content.contains("Refactoring the database layer"));
}

#[test]
fn build_system_content_omits_empty_title() {
    let (mut session, registry, _dir) = setup_build_system_content_session();
    // Title is None by default — no "Current Session Title" section.
    let content = test_build_content(&mut session, &registry, &[]);
    assert!(content.is_some());
    let content = content.unwrap();
    assert!(!content.contains("## Current Session Title"));

    // Also omit when the title is an empty string.
    session.config.title = Some("".into());
    let content2 = test_build_content(&mut session, &registry, &[]);
    assert!(content2.is_some());
    let content2 = content2.unwrap();
    assert!(!content2.contains("## Current Session Title"));
}
