//! Reasoning round-trip policy (phase 4b/4c): how the opaque reasoning
//! artifact captured by a provider adapter on one turn is replayed back to
//! the provider on subsequent turns, and how Responses-style continuity is
//! chained across user turns via `previous_response_id`.
//!
//! Extracted from `requests.rs` so the policy resolution, provenance checks,
//! and request-message building live next to each other instead of being
//! scattered through the agent loop.

use choreo_ai_protocols::openai::{
    AssistantToolCall, AssistantToolFunction, ChatImagePart, ChatRequestMessage,
};
use choreo_ai_protocols::{
    ReasoningPassback, model_reasoning_passback, model_supports_vision, requires_reasoning_content,
};
use choreo_proto::{ReasoningArtifact, Turn};
use std::path::Path;
use tracing::{debug, warn};

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
    // DeepSeek/Kimi chat requires `reasoning_content` to be PRESENT on every
    // assistant message, even (indeed especially) when the model produced no
    // reasoning on a given call — the upstream 400s a tool-loop turn that
    // omits it. Constant per request (model-bound), so resolve once.
    let requires_rc = requires_reasoning_content(provider_slug, model);
    // Whether the active model accepts image input. Tool-result images are
    // attached natively only when it does; otherwise they are replaced with a
    // text placeholder (the vision gate). Constant per request.
    let vision = model_supports_vision(provider_slug, model);

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
        // Reasoning round-trip (phase 4b): the artifact is replayed when BOTH
        // gates pass — (1) same-model provenance (artifacts are model-bound; a
        // turn produced by a different model must not have its encrypted
        // payload replayed into this request, matching pi's isSameModel and
        // Anthropic's strip-on-model-change rule) and (2) the provider's
        // passback policy for this request — OR the empty-message fallback
        // applies (see the reasoning_content comment below: a content-less,
        // tool-less turn must never ship bare under an echo-capable policy):
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
        // own wire format (phase 4a), so the daemon never interprets it. The
        // whole decision (policy + provenance + empty-fill) lives in
        // `include_reasoning_artifact`, shared with the precondition guard
        // and `session_inspect` so the three can never drift.
        let include_artifact = include_reasoning_artifact(turn, provider_slug, model, passback);
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
                images: Vec::new(),
                tool_call_id: None,
                tool_calls,
                // reasoning_content handling for the echo policy: a real
                // artifact re-emits its text (leave the explicit field None);
                // on a DeepSeek/Kimi model that requires the field present
                // and where nothing will be echoed, inject an EMPTY string so
                // the wire always carries it (mirrors opencode's transform of
                // `{type:"reasoning", text:""}` on every assistant message).
                // `include_reasoning_artifact` already forces the artifact
                // in for a content-less, tool-less turn under an echo-capable
                // policy (the "empty assistant message" 400), so the injected
                // empty string only appears when nothing real can be echoed.
                reasoning_content: if requires_rc && !include_artifact {
                    Some(String::new())
                } else {
                    None
                },
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
                images: Vec::new(),
                tool_call_id: Some(tr.call_id.clone()),
                tool_calls: None,
                reasoning_content: None,
                reasoning: None,
                reasoning_text: None,
                reasoning_artifact: None,
            });
        }
        // Synthetic user messages carrying tool-result images (vision input),
        // appended AFTER every tool message of the turn so the provider's
        // tool_use → tool_result adjacency holds (a user message interleaved
        // between tool results would break it). Each image is re-read and
        // re-normalized from its source path at request time (pass-through
        // design; no artifact store). On non-vision models the gate emits a
        // text placeholder instead of pixels.
        messages.extend(tool_result_image_messages(turn, vision));
    }
    messages
}

/// Build synthetic `user` messages that carry a turn's tool-result images
/// (vision input) for the request builder. Each image-bearing tool result
/// yields one user message placed after the turn's tool messages.
///
/// On a vision-capable model the image is re-read from its source path,
/// normalized, and attached as a [`ChatImagePart`]; on a non-vision model (or
/// when the file can no longer be read — it may have been deleted since the
/// tool ran), a text placeholder is emitted instead so the model is never
/// sent pixels it cannot process and never silently loses the image. The
/// placeholder names the source path so the model can re-read it with a text
/// tool if it wants.
fn tool_result_image_messages(turn: &Turn, vision: bool) -> Vec<ChatRequestMessage> {
    let mut out = Vec::new();
    for tr in &turn.tool_results {
        let Some(img) = &tr.image else {
            continue;
        };
        let lead = format!("[image from `{}` tool result: {}]", tr.name, img.path);
        if !vision {
            out.push(ChatRequestMessage::with_images(
                "user",
                format!(
                    "{lead} — the active model does not support image input, so the image \
                     could not be attached; its metadata is in the tool result above."
                ),
                Vec::new(),
            ));
            continue;
        }
        match crate::image_prep::load_and_normalize(Path::new(&img.path)) {
            Ok(prep) => {
                debug!(
                    path = %img.path,
                    width = prep.width,
                    height = prep.height,
                    mime = %prep.mime_type,
                    "attaching tool-result image to request"
                );
                out.push(ChatRequestMessage::with_images(
                    "user",
                    lead,
                    vec![ChatImagePart {
                        data: prep.data,
                        mime_type: prep.mime_type.to_string(),
                    }],
                ));
            }
            Err(e) => {
                warn!(
                    path = %img.path,
                    error = %e,
                    "tool-result image could not be re-read at request time; attaching placeholder"
                );
                out.push(ChatRequestMessage::with_images(
                    "user",
                    format!(
                        "{lead} — the image file could not be re-read ({e}); only its \
                              text metadata is available."
                    ),
                    Vec::new(),
                ));
            }
        }
    }
    out
}

/// Whether a turn participates in the tool loop (its assistant message
/// carries tool calls, or tool results are attached to it). DeepSeek/Kimi
/// reject a tool loop whose assistant message drops `reasoning_content`, so
/// `ToolLoop`-policy providers need the artifact echoed exactly on these
/// turns.
fn turn_has_tool_involvement(turn: &Turn) -> bool {
    !turn.tool_calls.is_empty() || !turn.tool_results.is_empty()
}

/// Single source of truth for whether a turn's opaque reasoning artifact is
/// replayed on the wire for the current `(provider_slug, model)` request.
/// The request builder, the precondition guard, and the `session_inspect`
/// dry-run all compute the same decision through this one helper so the
/// three can never drift.
///
/// Two gates, both required:
/// 1. **Same-model provenance** — artifacts are model-bound; a turn produced
///    by a different model (or with an unrecorded producer) must not have
///    its payload replayed, matching pi's isSameModel and Anthropic's
///    strip-on-model-change rule.
/// 2. **The passback policy** for the request:
///    ToolLoop  → tool-involving turns only
///    AllTurns / Signature → every turn
///    None / ResponseId → never via the message
///
/// `passback` must be the resolved policy for `(provider_slug, model)`
/// ([`model_reasoning_passback`]); callers that also need the value for
/// their own logic resolve it once per request and pass it in, so the helper
/// never re-scans the catalog per turn.
///
/// PLUS the **empty-message fallback**: the builder must never ship a
/// provably-invalid assistant message — a turn recorded with empty content
/// and no tool calls (e.g. a reasoning-only response) would serialize as a
/// wholly empty message, which OpenAI-compatible chat providers reject with
/// "the message ... with role 'assistant' must not be empty". If such a turn
/// carries a same-model artifact, the artifact's real reasoning text is the
/// only non-empty payload available, so it is included even though the
/// policy alone would skip it. Artifacts always carry non-empty bytes (the
/// capture path filters empty reasoning), so including one guarantees the
/// wire message is non-empty. The fallback is provider-agnostic — it fires
/// on every passback that may legally echo (`ToolLoop`/`AllTurns`/
/// `Signature`), not only the DeepSeek/Kimi `requires_rc` models — but
/// deliberately does NOT fire under `None` (an explicit never-replay
/// override — the gateway may itself reject replayed reasoning, e.g.
/// Cerebras gpt-oss) or `ResponseId` (continuity flows through
/// `previous_response_id`/input items, not the message reasoning field);
/// those stay flag-only.
pub(crate) fn include_reasoning_artifact(
    turn: &Turn,
    provider_slug: &str,
    model: &str,
    passback: ReasoningPassback,
) -> bool {
    // Same-model provenance: compare `(slug, model)` string pairs directly
    // instead of constructing a temporary `ReasoningProducer` (which would
    // allocate two Strings per turn). An unrecorded producer fails this too
    // (None != Some(..)), so a pre-migration artifact is never replayed.
    let same_model = turn
        .reasoning_producer
        .as_ref()
        .map(|p| (p.provider_slug.as_str(), p.model.as_str()))
        == Some((provider_slug, model));
    if !same_model || turn.reasoning_artifact.is_none() {
        return false;
    }
    let policy_echo = match passback {
        ReasoningPassback::None | ReasoningPassback::ResponseId => false,
        ReasoningPassback::ToolLoop => turn_has_tool_involvement(turn),
        ReasoningPassback::AllTurns | ReasoningPassback::Signature => true,
    };
    // Empty-message fallback: a content-less, tool-less turn would otherwise
    // ship a wholly empty assistant message (the "must not be empty" 400);
    // the artifact's real reasoning text is the only payload that can keep
    // it valid. `None`/`ResponseId` cannot use it (see the doc comment).
    let can_echo = !matches!(
        passback,
        ReasoningPassback::None | ReasoningPassback::ResponseId
    );
    policy_echo || (can_echo && turn_is_wire_empty(turn))
}

/// Whether the turn's assistant message would be EMPTY on the wire (no
/// content and no tool calls) — the shape OpenAI-compatible chat providers
/// reject with "the message ... with role 'assistant' must not be empty".
/// Tool-call turns always serialize a non-empty `tool_calls` array, and text
/// with only whitespace still serializes a non-empty string, so the only way
/// to reach this state is a recorded `assistant_text` of `""` (or `None`,
/// which emits no assistant message at all — callers guard with
/// `has_assistant_message` first).
fn turn_is_wire_empty(turn: &Turn) -> bool {
    turn.tool_calls.is_empty() && turn.assistant_text.as_deref().is_none_or(str::is_empty)
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
/// Additionally, an assistant message that would be **empty on the wire** (no
/// content, no tool calls — e.g. a reasoning-only response) is flagged when
/// the builder has no replayable same-model artifact to fill it: the
/// provider's "message ... must not be empty" 400 is then the certain outcome
/// on ANY OpenAI-compatible chat provider, not just the DeepSeek/Kimi
/// (`requires_rc`) ones — the empty `reasoning_content` injection those
/// models get is no substitute for a real payload. Whether the turn is on a
/// `requires_rc` model is logged alongside so the two hazards stay
/// distinguishable.
///
/// Provenance is checked exactly like the builder: an artifact is only
/// replayed when its producer matches the current (provider_slug, model), so
/// after a deliberate mid-session model switch every pre-switch turn is
/// flagged on each request. That is the intended diagnostic signal — the
/// built request genuinely omits those echoes; if the provider accepts the
/// switch, the warnings are informational rather than a blocker. The replay
/// decision (including the empty-message fallback) comes from
/// [`include_reasoning_artifact`], the same helper the builder uses, so the
/// guard can never disagree with the request that is actually sent.
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
    // Mirror the builder's `requires_reasoning_content` resolution so the
    // wire-empty hazard is logged with the same injection the builder will
    // apply (a requires_rc turn that is unfixable still ships the empty
    // string, distinguishable from a plain empty message).
    let requires_rc = requires_reasoning_content(provider_slug, model);
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
        // The artifact is on the wire exactly when the builder (and the
        // `session_inspect` dry-run) replay it — one helper, no drift. When
        // it is echoed the message is non-empty (artifacts always carry
        // non-empty bytes) and the required echo is present: clean.
        if include_reasoning_artifact(turn, provider_slug, model, passback) {
            continue;
        }
        // Nothing was echoed. That is a problem when the passback policy
        // demands the echo on this turn (AllTurns/Signature everywhere,
        // ToolLoop on tool-involving turns), OR when omitting it leaves a
        // wire-EMPTY assistant message — the "must not be empty" 400 is then
        // certain on ANY OpenAI-compatible chat provider (the `requires_rc`
        // empty-string injection is no substitute for a real payload), so
        // the unfixable turns are flagged wherever the guard has
        // jurisdiction.
        let wire_empty = turn_is_wire_empty(turn);
        let policy_demands_echo = check_all_turns || turn_has_tool_involvement(turn);
        if !policy_demands_echo && !wire_empty {
            continue;
        }
        // Mirror the builder's provenance gate exactly: an artifact is
        // replayed only when its producer matches the current (provider_slug,
        // model), so every turn that reaches the classification below has a
        // producer mismatch (or none) — the `include_reasoning_artifact`
        // early-return above already skipped every replayable same-model
        // turn. Flagging it like a missing artifact keeps the provider 400
        // after a model switch diagnosable.
        problems += 1;
        match (&turn.reasoning_artifact, turn.reasoning_producer.as_ref()) {
            (None, _) => {
                warn!(
                    session_id,
                    turn_id,
                    provider_slug,
                    model,
                    passback = ?passback,
                    requires_rc,
                    wire_empty,
                    "reasoning artifact missing for turn; provider may reject this request",
                );
            }
            // Artifact bytes exist but their provenance was never recorded (a
            // pre-migration state or a capture bug): the builder cannot prove
            // same-model provenance, so the payload is dropped — flag it like
            // a missing artifact rather than claiming a model mismatch.
            (Some(_), None) => {
                warn!(
                    session_id,
                    turn_id,
                    provider_slug,
                    model,
                    passback = ?passback,
                    requires_rc,
                    wire_empty,
                    "reasoning artifact present but its producer is unrecorded; it will not be replayed and the provider may reject this request",
                );
            }
            (Some(_), Some(_)) => {
                warn!(
                    session_id,
                    turn_id,
                    provider_slug,
                    model,
                    passback = ?passback,
                    requires_rc,
                    wire_empty,
                    "reasoning artifact produced by a different model; it will not be replayed and the provider may reject this request",
                );
            }
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
