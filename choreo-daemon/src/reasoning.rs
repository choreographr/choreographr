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
use choreo_ai_protocols::{ReasoningPassback, model_reasoning_passback, model_supports_vision};
use choreo_proto::{ReasoningArtifact, Turn};
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
///
/// `in_flight_from_turn` carries the image-decay marker: the turn id of the
/// FIRST turn of the request currently in flight (captured by the agent loop
/// before its first `start_turn`), or `None` when no request is in flight
/// (session load / dry-run). Turns with id >= the marker belong to the
/// current request and attach image bytes; every OLDER turn decays to a text
/// placeholder (see `tool_result_image_messages`). This is purely a request-
/// builder parameter — nothing is persisted on the turn, and after the request
/// finishes the next request's window simply starts at a newer turn id, so
/// all earlier images naturally decay without any cleanup.
#[must_use]
pub fn build_chat_request_messages(
    session: &SessionState,
    system_prompt: Option<&str>,
    provider_slug: &str,
    model: &str,
    in_flight_from_turn: Option<u32>,
) -> Vec<ChatRequestMessage> {
    let mut messages = Vec::new();

    // Prepend system prompt and context if provided.
    if let Some(prompt) = system_prompt {
        messages.push(ChatRequestMessage::simple("system", prompt.to_string()));
    }

    // The passback policy decides *whether* the artifact is replayed; it is
    // constant for the whole request (the model does not change mid-loop).
    let passback = model_reasoning_passback(provider_slug, model);
    // Whether the active model accepts image input. Tool-result images are
    // attached natively only when it does; otherwise they are replaced with a
    // text placeholder (the vision gate). Constant per request.
    let vision = model_supports_vision(provider_slug, model);

    for (turn_id, turn) in &session.turns {
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
        // payload replayed into this request, matching Anthropic's
        // strip-on-model-change rule) and (2) the provider's
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
        // The three legacy wire-shaped reasoning fields are gone: an artifact is
        // the only reasoning payload a message may carry, and the adapter picks
        // the wire key from the catalog's declared echo field (falling back to
        // the field the payload was observed under) — see
        // `ChatWireMessage`. *Whether* the artifact rides is the only decision
        // this builder makes; *where* it lands, and whether the field must be
        // present even when there is nothing to echo, belongs to the adapter.
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
                reasoning_artifact: None,
            });
        }
        // Synthetic user messages carrying tool-result images (vision input),
        // appended AFTER every tool message of the turn so the provider's
        // tool_use → tool_result adjacency holds (a user message interleaved
        // between tool results would break it). Image BYTES are read from the
        // stored reference (`img.data`), not re-read from the source file at
        // request time, and are attached ONLY for turns that belong to the
        // request currently in flight (turn id >= `in_flight_from_turn`) —
        // older history replays a text placeholder instead (the decay gate:
        // re-attaching megabytes of every historical screenshot would bloat
        // every request without limit).
        let attach_images = in_flight_from_turn.is_some_and(|first| *turn_id >= first);
        messages.extend(tool_result_image_messages(turn, vision, attach_images));
    }
    messages
}

/// Build synthetic `user` messages that carry a turn's tool-result images
/// (vision input) for the request builder. Each image-bearing tool result
/// yields one user message placed after the turn's tool messages.
///
/// Three gates decide whether the normalized image bytes ride the request:
///
/// 1. **Decay** (`attach_images == false`): only turns belonging to the
///    request currently in flight attach pixels — the first request that
///    produced a tool-result image sees every pixel of it, but on every
///    LATER request (or a request built without an in-flight marker, e.g.
///    right after a session load) the turn replays only a text placeholder
///    naming the source path, so the model can re-read the file with a text
///    tool if it needs the image again. This keeps an hour-old screenshot out
///    of every subsequent request without losing the fact that one existed.
/// 2. **Vision** — on a non-vision model no pixels are ever sent, whatever
///    the decay marker says.
/// 3. **Stored bytes** — when the reference carries no bytes (e.g. an old
///    persisted turn whose `data` deserialized empty), a text placeholder is
///    emitted instead so the model is never silently left without the image.
///
/// Every branch keeps the same message shape `[image from `<tool>` tool
/// result: <path>]`, so the model can always tell which tool produced the
/// image and where its source lives.
fn tool_result_image_messages(
    turn: &Turn,
    vision: bool,
    attach_images: bool,
) -> Vec<ChatRequestMessage> {
    let mut out = Vec::new();
    for tr in &turn.tool_results {
        let Some(img) = &tr.image else {
            continue;
        };
        let lead = format!("[image from `{}` tool result: {}]", tr.name, img.path);
        if !attach_images {
            // Decay policy: the turn is not part of the request currently in
            // flight, so only the placeholder naming the source path is sent —
            // the model can re-read the file with a text tool if it needs the
            // image again. Even on a vision model with valid stored bytes.
            debug!(
                path = %img.path,
                "tool-result image decayed; attaching placeholder"
            );
            out.push(ChatRequestMessage::with_images(
                "user",
                format!(
                    "{lead} — the image was shown in an earlier request and is no longer \
                     attached as pixels; re-read it with a file tool if you need it again."
                ),
                Vec::new(),
            ));
            continue;
        }
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
        if img.data.is_empty() {
            // No stored bytes (an old persisted turn, or a reference built
            // before the bytes were carried): degrade to a text placeholder so
            // the model still learns the image existed.
            warn!(
                path = %img.path,
                "tool-result image bytes unavailable; attaching placeholder"
            );
            out.push(ChatRequestMessage::with_images(
                "user",
                format!(
                    "{lead} — the image bytes are unavailable (e.g. an old persisted turn); \
                     only its text metadata is available."
                ),
                Vec::new(),
            ));
        } else {
            // Attach the normalized bytes straight from the stored reference.
            debug!(
                path = %img.path,
                mime = %img.mime_type,
                bytes = img.data.len(),
                "attaching stored tool-result image to request"
            );
            out.push(ChatRequestMessage::with_images(
                "user",
                lead,
                vec![ChatImagePart {
                    data: img.data.clone(),
                    mime_type: img.mime_type.clone(),
                }],
            ));
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
///    its payload replayed, matching Anthropic's strip-on-model-change rule.
/// 2. **The passback policy** for the request:
///    `ToolLoop`  → tool-involving turns only
///    `AllTurns` / Signature → every turn
///    None / `ResponseId` → never via the message
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
/// non-blank content and no tool calls).
///
/// This is the shape every provider rejects — chat servers with "the message
/// ... with role 'assistant' must not be empty", Anthropic and Gemini with an
/// empty content/parts array — and the shape the protocol lowerings drop from
/// the projection rather than send. Whitespace-only text counts as empty: a
/// blank message reads as nothing to a model and is rejected by strict
/// servers, so a leading/trailing-whitespace "answer" is not a payload.
///
/// Tool-call turns always serialize a non-empty `tool_calls` array, and a turn
/// that never produced an assistant message is the caller's concern
/// (`has_assistant_message`), so neither can reach here.
fn turn_is_wire_empty(turn: &Turn) -> bool {
    turn.tool_calls.is_empty()
        && turn
            .assistant_text
            .as_deref()
            .is_none_or(|text| text.trim().is_empty())
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

/// Precondition guard: report turns whose assistant message will be *degraded*
/// on the wire.
///
/// Two independent degradations, neither of which the adapter can repair:
///
/// 1. **Wire-empty assistant turn** — the turn recorded no non-blank content and
///    no tool calls, and no same-model artifact will fill it. Every protocol
///    lowering drops such a message rather than send it (an empty assistant
///    message is rejected by chat servers, Anthropic, and Gemini alike), so the
///    turn silently leaves the model's view of the conversation.
/// 2. **Capture loss** — the turn's reasoning was displayed in the session but
///    never captured as an artifact, so the echo degrades to an empty field
///    (or disappears entirely on a route that declares none).
///
/// Both are reported as warnings about *information*, not request validity: the
/// request is already safe, because a dropped message cannot be invalid. That is
/// also why this guard no longer looks at the echo policy — which field carries
/// the echo, and whether its presence is mandatory, is the adapter's decision
/// from the catalog fact, and an assistant turn without an artifact is not a
/// hazard on its own. The previous policy-shaped version gated on the
/// `DeepSeek`/Kimi `reasoning_content` flag and flagged every tool turn the model
/// happened to answer without thinking (five spurious warnings on one request)
/// while missing the real failure mode entirely.
///
/// Returns the number of turns flagged, so tests can exercise the path
/// deterministically; turn-level diagnosis of a whole request belongs to the
/// `session_inspect` tool, which dry-runs the real lowering.
pub(crate) fn warn_on_uncaptured_reasoning(
    session: &SessionState,
    session_id: u64,
    provider_slug: &str,
    model: &str,
) -> usize {
    // Only the passback policy is needed, to know whether the builder's
    // empty-message fallback will replay a same-model artifact.
    let passback = model_reasoning_passback(provider_slug, model);
    let mut problems = 0;
    for (turn_id, turn) in &session.turns {
        if turn.undone {
            continue;
        }
        // Only a turn that emits an assistant message can be degraded.
        if turn.assistant_text.is_none() && turn.tool_calls.is_empty() {
            continue;
        }
        let artifact_fills = include_reasoning_artifact(turn, provider_slug, model, passback);
        if !artifact_fills && turn_is_wire_empty(turn) {
            problems += 1;
            warn!(
                session_id,
                turn_id,
                provider_slug,
                model,
                "assistant turn has no visible content and no artifact to replay; \
                 it is dropped from the request and the model will not see it",
            );
            continue;
        }
        if turn.reasoning_artifact.is_none() && turn.assistant_reasoning.is_some() {
            problems += 1;
            warn!(
                session_id,
                turn_id,
                provider_slug,
                model,
                "reasoning was displayed but not captured for the round-trip; \
                 the echo degrades to an empty field",
            );
        }
    }
    problems
}

/// Estimate the input tokens contributed by a reasoning artifact's payload.
///
/// The artifact is opaque bytes owned by the producing adapter; the daemon
/// never interprets it. Most payloads (chat `reasoning_content` strings,
/// Anthropic thinking-block JSON, Gemini signatures, and the structured
/// `reasoning_details` JSON array) are UTF-8 text, so count them with the
/// encoding when decodable; otherwise fall back to a bytes/4 heuristic.
/// Replayed reasoning is billed as input on keep-all models, so under-counting
/// here would mislead the context-window display.
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
        Ok(text) => {
            // u128→u32 token counts: informational estimate only; real
            // payloads are far below u32::MAX tokens.
            #[expect(clippy::cast_possible_truncation)]
            {
                enc.count(text) as u32
            }
        }
        Err(_) => {
            // Same estimate, byte/4 heuristic branch.
            #[expect(clippy::cast_possible_truncation)]
            {
                (bytes.len() / 4) as u32
            }
        }
    }
}
