//! Reasoning round-trip policy (phase 4b/4c): how the opaque reasoning
//! artifact captured by a provider adapter on one turn is replayed back to
//! the provider on subsequent turns, and how Responses-style continuity is
//! chained across user turns via `previous_response_id`.
//!
//! Extracted from `requests.rs` so the policy resolution, provenance checks,
//! and request-message building live next to each other instead of being
//! scattered through the agent loop.

use choreo_ai_protocols::openai::{AssistantToolCall, AssistantToolFunction, ChatRequestMessage};
use choreo_ai_protocols::{ReasoningPassback, model_reasoning_passback};
use choreo_proto::{ReasoningArtifact, Turn};
use tracing::warn;

use crate::sessions::SessionState;

/// Build the provider request messages for a session, applying the reasoning
/// passback policy (phase 4b): the opaque artifact captured on a previous
/// turn is replayed only when BOTH gates pass — same-model provenance (the
/// artifact is model-bound; a turn produced by a different model must not
/// have its payload replayed) and the provider's `reasoning_passback` policy
/// for this request (`ToolLoop` → tool-involving turns only, `AllTurns` →
/// every turn, `Signature` → every turn, `ResponseId`/`None` → never via the
/// message).
///
/// Public for the daemon integration tests (the `test-utils` feature); the
/// production caller is `run_agent_loop`.
pub fn build_chat_request_messages(
    session: &SessionState,
    system_prompt: Option<&str>,
    provider_slug: &str,
    model: &str,
) -> Vec<ChatRequestMessage> {
    let mut messages = Vec::new();

    // Prepend system prompt and context if provided.
    if let Some(prompt) = system_prompt {
        messages.push(ChatRequestMessage::simple("system", prompt.to_string()));
    }

    // The passback policy decides *whether* the artifact is replayed; it is
    // constant for the whole request (the model does not change mid-loop).
    let passback = model_reasoning_passback(provider_slug, model);

    for turn in session.turns.values() {
        if turn.undone {
            continue;
        }
        // User message
        if let Some(text) = &turn.user_text {
            messages.push(ChatRequestMessage::simple("user", text.clone()));
        }
        // Assistant message (text or tool calls).
        //
        // Reasoning round-trip (phase 4b): the artifact is replayed only when
        // BOTH gates pass — (1) same-model provenance (artifacts are
        // model-bound; a turn produced by a different model must not have its
        // encrypted payload replayed into this request, matching pi's
        // isSameModel and Anthropic's strip-on-model-change rule) and (2) the
        // provider's passback policy for this request:
        //   ToolLoop  → only tool-involving turns (assistant tool_calls or
        //               tool results attached) — DeepSeek/Kimi reject a tool
        //               loop whose assistant message drops reasoning_content
        //   AllTurns  → every turn (Anthropic keep-all)
        //   Signature → every turn (Gemini encrypted thought signatures)
        //   ResponseId→ never via the message; continuity flows through
        //               previous_response_id (handled in the agent loop)
        //   None      → display-only providers, never replay
        // The three legacy string fields (reasoning_content/reasoning/
        // reasoning_text) stay None: the adapter re-emits the artifact in its
        // own wire format (phase 4a), so the daemon never interprets it.
        //
        // The provenance check compares `(slug, model)` string pairs directly
        // instead of constructing a temporary `ReasoningProducer` (which
        // would allocate two Strings per turn).
        let same_model = turn
            .reasoning_producer
            .as_ref()
            .map(|p| (p.provider_slug.as_str(), p.model.as_str()))
            == Some((provider_slug, model));
        let include_artifact = same_model
            && match passback {
                ReasoningPassback::None => false,
                ReasoningPassback::ToolLoop => turn_has_tool_involvement(turn),
                ReasoningPassback::AllTurns => true,
                ReasoningPassback::Signature => true,
                ReasoningPassback::ResponseId => false,
            };
        let has_tool_calls = !turn.tool_calls.is_empty();
        if turn.assistant_text.is_some() || has_tool_calls {
            let tool_calls = if has_tool_calls {
                Some(
                    turn.tool_calls
                        .iter()
                        .map(|tc| AssistantToolCall {
                            id: tc.call_id.clone(),
                            kind: "function".to_string(),
                            function: AssistantToolFunction {
                                name: tc.name.clone(),
                                arguments: tc.arguments_json.clone(),
                            },
                        })
                        .collect(),
                )
            } else {
                None
            };
            messages.push(ChatRequestMessage {
                role: "assistant",
                content: turn.assistant_text.clone(),
                tool_call_id: None,
                tool_calls,
                reasoning_content: None,
                reasoning: None,
                reasoning_text: None,
                reasoning_artifact: if include_artifact {
                    turn.reasoning_artifact.clone()
                } else {
                    None
                },
            });
        }
        // Tool result messages
        for tr in &turn.tool_results {
            messages.push(ChatRequestMessage {
                role: "tool",
                content: Some(tr.content.clone()),
                tool_call_id: Some(tr.call_id.clone()),
                tool_calls: None,
                reasoning_content: None,
                reasoning: None,
                reasoning_text: None,
                reasoning_artifact: None,
            });
        }
    }
    messages
}

/// Whether a turn participates in the tool loop (its assistant message
/// carries tool calls, or tool results are attached to it). DeepSeek/Kimi
/// reject a tool loop whose assistant message drops `reasoning_content`, so
/// `ToolLoop`-policy providers need the artifact echoed exactly on these
/// turns.
fn turn_has_tool_involvement(turn: &Turn) -> bool {
    !turn.tool_calls.is_empty() || !turn.tool_results.is_empty()
}

/// Resolve the starting `previous_response_id` for a new agent-loop
/// invocation. ResponseId-policy providers (OpenAI/xAI Responses) chain
/// reasoning continuity across user turns, so the id persisted on the session
/// config after the last model call is restored here. Every other policy
/// resets to None: the id is meaningless outside Responses-style APIs and
/// must not leak into a request that does not understand it.
///
/// Restoration is additionally gated on provenance: the persisted id is
/// restored only when the current provider+model is the one that produced it
/// (same-model provenance as reasoning artifacts), so a stale id from a
/// different provider or model is never replayed.
pub(crate) fn initial_prev_resp_id(
    session: &SessionState,
    provider_slug: &str,
    model: &str,
) -> Option<String> {
    // A response id is service-bound: a stale id persisted under a different
    // provider (mid-session openai → xAI switch) would be rejected with a 400
    // if replayed, and it is model-bound for continuity purposes too — so
    // require an exact producer match, mirroring the artifact provenance check.
    let same_producer = session
        .config
        .last_response_id_producer
        .as_ref()
        .map(|p| (p.provider_slug.as_str(), p.model.as_str()))
        == Some((provider_slug, model));
    if same_producer
        && model_reasoning_passback(provider_slug, model) == ReasoningPassback::ResponseId
    {
        session.config.last_response_id.clone()
    } else {
        None
    }
}

/// Precondition guard for reasoning-echo policies (phase 4c).
///
/// Before sending a request whose passback policy requires echoing reasoning
/// (ToolLoop/AllTurns/Signature), check that every turn that will carry an
/// assistant message has its artifact. A turn recorded before the artifact
/// was captured (e.g. a pre-migration session state) would otherwise be sent
/// without the reasoning payload the provider demands on tool-loop turns,
/// surfacing as a mysterious 400 — this turns that into a diagnosable log
/// line. Returns the number of turns that will omit a required reasoning
/// echo (missing artifact, or an artifact produced by a different model), 0
/// when clean, so tests can exercise the path deterministically.
///
/// The guard deliberately does NOT disable thinking for the request: for
/// ToolLoop providers (DeepSeek/Kimi) the 400 comes from *history*, not the
/// current request's thinking setting, so flipping the effort to "off" would
/// not fix the failure and would silently change model behavior — the warn is
/// the honest signal. ResponseId/None policies skip the check entirely (no
/// artifact is expected on the wire).
///
/// Scope: `ToolLoop` cares only about tool-involving turns (that is where the
/// provider demands the echo); `AllTurns`/`Signature` echo on every assistant
/// message, so any assistant turn missing its artifact is a violation there.
///
/// Provenance is checked exactly like the builder: an artifact is only
/// replayed when its producer matches the current (provider_slug, model), so
/// after a deliberate mid-session model switch every pre-switch turn is
/// flagged on each request. That is the intended diagnostic signal — the
/// built request genuinely omits those echoes; if the provider accepts the
/// switch, the warnings are informational rather than a blocker.
pub(crate) fn warn_on_missing_reasoning_artifacts(
    session: &SessionState,
    session_id: u64,
    provider_slug: &str,
    model: &str,
) -> usize {
    let passback = model_reasoning_passback(provider_slug, model);
    if matches!(
        passback,
        ReasoningPassback::None | ReasoningPassback::ResponseId
    ) {
        return 0;
    }
    // AllTurns/Signature echo on every assistant message; ToolLoop only on
    // tool-involving turns.
    let check_all_turns = matches!(
        passback,
        ReasoningPassback::AllTurns | ReasoningPassback::Signature
    );
    let mut problems = 0;
    for (turn_id, turn) in session.turns.iter() {
        if turn.undone {
            continue;
        }
        // Only turns that emit an assistant message in the builder need an
        // artifact; a user-only turn (in-progress or failed) has none.
        let has_assistant_message = turn.assistant_text.is_some() || !turn.tool_calls.is_empty();
        if !has_assistant_message {
            continue;
        }
        if !check_all_turns && !turn_has_tool_involvement(turn) {
            continue;
        }
        // Mirror the builder's provenance gate exactly: an artifact is
        // replayed only when its producer matches the current (provider_slug,
        // model), so a turn recorded under a different model (mid-session
        // switch) omits its echo on the wire even though the artifact bytes
        // exist — flag it like a missing artifact, or the provider 400 after
        // the switch stays a mystery.
        let same_producer = turn
            .reasoning_producer
            .as_ref()
            .map(|p| (p.provider_slug.as_str(), p.model.as_str()))
            == Some((provider_slug, model));
        if turn.reasoning_artifact.is_none() {
            problems += 1;
            warn!(
                session_id,
                turn_id,
                provider_slug,
                model,
                passback = ?passback,
                "reasoning artifact missing for turn; provider may reject this request",
            );
        } else if !same_producer {
            problems += 1;
            warn!(
                session_id,
                turn_id,
                provider_slug,
                model,
                passback = ?passback,
                "reasoning artifact produced by a different model; it will not be replayed and the provider may reject this request",
            );
        }
    }
    problems
}

/// Estimate the input tokens contributed by a reasoning artifact's payload.
///
/// The artifact is opaque bytes owned by the producing adapter; the daemon
/// never interprets it. Most current payloads (chat `reasoning_content`
/// strings, Anthropic thinking-block JSON, Gemini signatures) are UTF-8 text,
/// so count them with the encoding when decodable; otherwise fall back to a
/// bytes/4 heuristic. Replayed reasoning is billed as input on keep-all
/// models, so under-counting here would mislead the context-window display.
pub(crate) fn reasoning_artifact_tokens(
    enc: &tiktoken::CoreBpe,
    artifact: &ReasoningArtifact,
) -> u32 {
    let bytes = match artifact {
        // ChatReasoning is a struct variant (field tag + bytes); the field
        // tag only matters for re-emission targeting, not token estimation,
        // so bind just the payload.
        ReasoningArtifact::ChatReasoning { bytes: b, .. }
        | ReasoningArtifact::AnthropicThinking(b)
        | ReasoningArtifact::GoogleSignatures(b)
        | ReasoningArtifact::ResponsesItems(b) => b,
    };
    match std::str::from_utf8(bytes) {
        Ok(text) => enc.count(text) as u32,
        Err(_) => (bytes.len() / 4) as u32,
    }
}
