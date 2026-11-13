use crate::cache_warm::{CacheWarmingMode, spawn_warmer};
use crate::context::{self, SkillMeta};
use crate::providers::InferenceProvider;
use crate::reasoning::{
    build_chat_request_messages_recording, initial_prev_resp_id, warn_on_uncaptured_reasoning,
};
use crate::sessions::{RequestContext, SessionCommand, SessionState};
use choreo_ai_protocols::{
    ChatTurnRequest, ChatTurnResult, StreamEvent, model_reasoning_capability,
};
use choreo_proto::{DaemonMessageType, OutputStream, SessionEvent, SessionStatus};

use std::io;
use std::path::PathBuf;
use tracing::{debug, warn};

mod config_tools;
mod final_text;
mod recovery;
mod stream;
mod system_content;
mod tokens;
mod tool_execution;
mod tool_use_turn;
pub(crate) use config_tools::*;
// Named, not globbed: the recovery unit is driven from the agent loop and a
// handful of tests, so spelling out its surface keeps an accidental rename from
// silently re-exporting (or hiding) one of its items.
pub(crate) use recovery::{
    RecoveryBudget, RecoveryOutcome, TRUNCATION_RECOVERY, TurnRecoveryParams, discarded_names,
    recover_unrunnable_turn,
};
pub(crate) use stream::*;
pub(crate) use system_content::*;
pub(crate) use tokens::*;
pub(crate) use tool_execution::*;
// The two turn arms are called only from `run_agent_loop`, so their surface is
// imported rather than re-exported: no other module needs a name from them.
use final_text::{FinalTextTurnOutcome, FinalTextTurnParams, handle_final_text_turn};
use tool_use_turn::{ToolUseTurnOutcome, ToolUseTurnParams, handle_tool_use_turn};

/// Resolve the effective reasoning effort for a turn, disabling it if the
/// model/provider combination does not support it.
fn resolve_reasoning_effort(
    client: &InferenceProvider,
    model: &str,
    session_id: u64,
    turn_iter: u32,
    configured_effort: &str,
) -> String {
    if configured_effort == "off" {
        return configured_effort.to_string();
    }
    let slug = client.provider_slug();
    let capability = model_reasoning_capability(slug, model);
    if capability.available_effort_levels.is_empty() {
        warn!(
            session_id, turn = turn_iter, model,
            effort = %configured_effort,
            "model does not support reasoning, disabling",
        );
        "off".to_string()
    } else if !capability
        .available_effort_levels
        .iter()
        .any(|l| l == configured_effort)
    {
        warn!(
            session_id, turn = turn_iter, model,
            effort = %configured_effort,
            valid = ?capability.available_effort_levels,
            "reasoning effort '{}' not in model's capability set, disabling",
            configured_effort,
        );
        "off".to_string()
    } else {
        configured_effort.to_string()
    }
}

pub(crate) fn run_agent_loop(
    client: &InferenceProvider,
    session: &mut SessionState,
    model: &str,
    stream_id: u64,
    cancel_rx: &crossbeam_channel::Receiver<()>,
    ctx: &RequestContext,
    user_text: Option<&str>,
) -> io::Result<bool> {
    let max_turns = ctx.max_turns;
    // `max_turns == 0` means *unlimited* — the loop runs until the model
    // produces a final answer, is cancelled, or hits an error.
    let limited = max_turns > 0;
    // Own the slug (rather than borrowing the client) so the client can be
    // REBUILT in place on an OAuth 401 refresh below; a refresh does not change
    // the account's provider slug, so the owned copy stays correct.
    let provider_slug = client.provider_slug().to_string();
    // The active client: a clone of the resolved provider that a 401 refresh
    // replaces with a freshly-built client carrying the new access token.
    let mut active_client = client.clone();

    // Cache warmer: spawn the per-request thread only in Streaming mode (the
    // common default is Off, which must cost nothing). It owns a clone of the
    // provider and is armed after each `ToolUse` result, just before the tools
    // run — the blocking window a warm ping bridges. The `WarmHandle` is an
    // RAII guard: it is dropped at every exit path (final text, cancel, error,
    // panic during unwind), stopping and joining the thread.
    //
    // The warmer gets its OWN cancel channel, relayed onto below at each point
    // the loop observes a cancel. It must NOT share the loop's `cancel_rx`:
    // crossbeam delivers each message to exactly one receiver, so a shared
    // receiver would let the warmer steal a cancel the loop needs to act on.
    //
    // The policy is the session's own (refreshed whenever the account is
    // (re-)resolved), read once here so nothing below borrows it.
    let warm_policy = session.warm_policy;
    let (warm_cancel_tx, warm_cancel_rx) = crossbeam_channel::unbounded::<()>();
    let warmer = (warm_policy.mode == CacheWarmingMode::Streaming).then(|| {
        spawn_warmer(
            InferenceProvider::clone(client),
            warm_policy,
            warm_cancel_rx,
        )
    });
    // Relay a cancel to the warmer at each point the loop observes one. A single
    // closure keeps the relay sites in sync (see the shared-receiver note above).
    let relay_warm_cancel = || {
        let _ = warm_cancel_tx.send(());
    };

    // Phase 4c: ResponseId-policy providers chain reasoning continuity across
    // user turns via `previous_response_id`. The last response id is persisted
    // on the session config after every model call and restored here, so a new
    // user request continues the chain instead of resetting it. All other
    // policies reset to None — the id is meaningless outside Responses-style
    // APIs and must not leak across requests.
    let mut prev_resp_id = initial_prev_resp_id(session, &provider_slug, model);
    let mut known_hint_paths: Vec<PathBuf> = Vec::new();
    let mut pending_hints: Vec<String> = Vec::new();

    // Turn recovery: `pending_user_text` seeds the NEXT loop iteration's user
    // turn (see `recover_unrunnable_turn`), and `recovery_budget` tracks each
    // cause's consecutive-recovery count. The budget resets on any turn that
    // produces runnable output, so it bounds a stuck model rather than a
    // long-but-productive request.
    let mut pending_user_text: Option<String> = None;
    let mut recovery_budget = RecoveryBudget::new();

    // Capture-fidelity guard: reasoning the session displays but never
    // captured for the round-trip is lost payload (the wire echo degrades to an
    // empty field). Wire *validity* is the adapter's job and is not checked
    // here; `session_inspect` diagnoses a whole request.
    warn_on_uncaptured_reasoning(session, ctx.session_id, &provider_slug, model);

    // Lazily cache discovered skills — they don't change during a session.
    // Global skills are always discoverable; discovery handles the optional
    // project scope internally, so a dir-less session still gets them.
    if session.discovered_skills.is_none() {
        session.discovered_skills = Some(context::discover_skills_ambient(
            session.config.working_dir.as_deref(),
        ));
    }

    let mut turn_iter: u32 = 0;
    // Tool-result image decay: capture the turn id this request's FIRST turn
    // will get — `start_turn` hands out exactly `session.next_turn_id`, so
    // reading it here (before the loop's first `start_turn`) is the exact
    // boundary, not an approximation. Everything in the builder with a turn id
    // at or after it belongs to this request (the user turn + every tool-loop
    // turn it creates) and attaches image bytes; turns from BEFORE this
    // request decay to text placeholders. Nothing is persisted: after the
    // request ends, the next request captures a newer boundary, so old images
    // stay decayed without any cleanup.
    let request_first_turn_id = session.next_turn_id;
    loop {
        // Enforce the iteration limit only when one is configured.
        // When `max_turns == 0` the loop is unbounded.
        if limited && turn_iter >= max_turns {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("tool loop exceeded {max_turns} iterations"),
            ));
        }
        debug!(
            session_id = ctx.session_id,
            turn = turn_iter,
            "agent loop turn"
        );
        let configured = session.config.reasoning_effort.as_deref().unwrap_or("off");
        let thinking_effort =
            resolve_reasoning_effort(&active_client, model, ctx.session_id, turn_iter, configured);
        // Snapshot the reasoning gate before `thinking_effort` is moved into the
        // request below: the warmer's replayability check needs it (an Anthropic
        // request with thinking enabled cannot be replayed with a 1-token cap).
        let thinking_enabled = thinking_effort != "off";
        crate::metrics::record_turn(model);
        // Load the current tool catalogue once per iteration: the shared
        // `ArcSwap` lets a list-changed rebuild swap in a new registry between
        // turns, and this pull pins the definitions used for THIS turn (the
        // whole turn is built against one consistent snapshot).
        let tool_registry = ctx.tool_registry.load_full();
        // Merge the shared registry's definitions (active ∪ protected, minus the
        // daemon-tier `mcp/<slug>` groups the session's project servers shadow)
        // with the session's own project tools. The project tools are pinned
        // with the same registry snapshot for this turn, so the whole turn is
        // built against one consistent tool list.
        let mut tools = tool_registry.available_definitions_excluding(
            &session.config.active_tool_groups,
            &session.project_shadowed_groups,
        );
        tools.extend(session.project_tools.definitions());
        // Preflight the assembled list before it is sent. MCP names and schemas
        // are sanitized at registration, so a definition a provider would reject
        // (an illegal function name, or a non-object `parameters` schema) means a
        // regression crept in. Such a tool invalidates the WHOLE request, and the
        // provider may answer with a bare 400 naming no offending field — so drop
        // the offender here (keeping the session usable) rather than dispatching a
        // request the provider is guaranteed to reject. A function name already
        // seen earlier in the list is dropped the same way, since a provider
        // cannot disambiguate two tools of one name.
        let dropped_tools = crate::tools::retain_valid_tool_definitions(&mut tools);
        if !dropped_tools.is_empty() {
            warn!(
                session_id = ctx.session_id,
                dropped = ?dropped_tools,
                "dropping provider-invalid or duplicate tool definitions before send"
            );
        }
        if is_cancelled_once(cancel_rx) {
            // Relay the cancel so an armed warmer stops pinging promptly.
            relay_warm_cancel();
            return Ok(true);
        }

        // Start a new turn for this agent loop iteration. The first iteration
        // carries the caller's user text; a later iteration carries the
        // truncation-recovery instruction when the previous turn was cut off.
        let turn_user_text = if turn_iter == 0 {
            user_text.map(std::string::ToString::to_string)
        } else {
            pending_user_text.take()
        };
        let (current_turn_id, _) = session.start_turn(turn_user_text);
        broadcast_turn_appended(&ctx.cmd_tx, session, ctx.session_id, current_turn_id);
        if ctx
            .cmd_tx
            .send(SessionCommand::StatusChanged(SessionStatus::Sending))
            .is_err()
        {
            return Ok(false);
        }

        let system_content = {
            // Scope the immutable borrow on session so it ends before the
            // mutable borrows that follow (start_turn, set_assistant_response, etc.).
            let skills: &[SkillMeta] = session.discovered_skills.as_deref().unwrap_or_default();
            build_system_content(
                &SystemContentParams {
                    working_dir: session.config.working_dir.as_deref(),
                    context_config: &session.config.context_config,
                    skills,
                    loaded_skill_bodies: &session.loaded_skill_bodies,
                    tool_registry: &tool_registry,
                    pending_hints: &pending_hints,
                    session_title: session.config.title.as_deref(),
                },
                &mut session.context_cache,
            )
        };
        pending_hints.clear();
        let messages = build_chat_request_messages_recording(
            session,
            Some(&system_content),
            &provider_slug,
            model,
            Some(request_first_turn_id),
        );

        // The estimate counts `messages` as-is — the FULL conversation, not
        // the chained tail the adapter puts on the wire. That is intentional:
        // the provider bills the whole context it holds in the chain, and the
        // full conversation in `messages` already includes that context (plus
        // the new tail), so there is no separate chained-context addend — one
        // would count the conversation twice (billing itself is unaffected; it
        // uses the provider-reported usage, not this estimate).
        let (encoding, estimated_prompt_tokens) = estimate_prompt_tokens(model, &messages, &tools);

        // Announce the iteration's turn and its freshly counted prompt, so the
        // session thread keeps the in-flight record current for any client that
        // attaches mid-run and so subscribers route the chunks below correctly.
        let _ = ctx
            .cmd_tx
            .send(SessionCommand::Broadcast(DaemonMessageType::Session {
                session_id: Some(ctx.session_id),
                event: SessionEvent::started(stream_id, current_turn_id, estimated_prompt_tokens),
            }));

        let mut retry_cb: Option<choreo_ai_protocols::openai::RetryCallback> = Some(Box::new({
            let cmd_tx = ctx.cmd_tx.clone();
            move |attempt, max_attempts, delay| {
                let _ = cmd_tx.send(SessionCommand::StatusChanged(SessionStatus::Retrying {
                    attempt,
                    max_attempts,
                    // u128→u64: retry delays are at most minutes; no truncation in practice.
                    #[expect(clippy::cast_possible_truncation)]
                    delay_ms: delay.as_millis() as u64,
                }));
            }
        }));

        // Broadcast the `Waiting` boundary once the request payload has been
        // fully uploaded (every body byte written, before the response is
        // read).  A one-shot event fired from the transport layer BEFORE any
        // delta, so — unlike the per-delta `StreamPhase` advances below — it
        // does not participate in the forward-only phase comparison; it simply
        // marks the handoff from "getting the request there" to "the provider
        // is thinking".  The `Arc` lets the transport clone it into each retry
        // attempt's request body.
        let uploaded_cb: Option<choreo_ai_protocols::RequestUploadedCallback> =
            Some(std::sync::Arc::new({
                let cmd_tx = ctx.cmd_tx.clone();
                move || {
                    let _ = cmd_tx.send(SessionCommand::StatusChanged(SessionStatus::Waiting));
                }
            }));

        // Running count of output tokens produced by the current turn.
        let mut output_token_count: u32 = 0;

        // The live streaming phase for this turn, advanced forward-only by
        // `advance_stream_phase` from the streaming callback below.  A retry
        // (`Retrying`) always fires before the first delta — the provider's
        // retry loop wraps the initial request send, before the response body
        // is read — so the phase is still `Sending` when a retry is reported
        // and the first post-retry chunk advances it correctly.
        let mut stream_phase = StreamPhase::Sending;

        // Gateway routing identity for the opencode zen/go providers: the
        // session's real id plus this turn's request id, as strings (the
        // gateway hashes the last 4 characters to pick an upstream bucket and
        // keys its sticky provider tracker on the session id). Owned locals,
        // so the borrowed fields outlive the provider call below.
        let oc_session_id = ctx.session_id.to_string();
        let oc_request_id = stream_id.to_string();

        // Wrap the provider call so a 401 from an OAuth account can refresh the
        // token, rebuild the client, and retry the request ONCE (never a loop).
        let mut oauth_refresh_attempted = false;
        let turn_result = loop {
            let result = active_client.chat_completion_turn_streaming(
                ChatTurnRequest {
                    model,
                    messages: &messages,
                    tools: &tools,
                    thinking_effort: thinking_effort.clone(),
                    on_retry: &mut retry_cb,
                    on_uploaded: uploaded_cb.clone(),
                    cancel_rx: Some(cancel_rx),
                    previous_response_id: prev_resp_id.as_deref(),
                    programmatic_tool_calling: active_client
                        .supports_programmatic_tool_calling(model),
                    session_id: oc_session_id.clone(),
                    request_id: oc_request_id.clone(),
                    max_output_tokens_override: None,
                    no_retry: false,
                },
                &mut |event| {
                    match event {
                        StreamEvent::Answer(text) => {
                            // An answer chunk means the model has begun writing the
                            // reply, so promote the phase to `Responding`.
                            advance_stream_phase(&mut stream_phase, StreamPhase::Responding, ctx);
                            if let Some(enc) = &encoding {
                                // u128→u32 token counts, informational only.
                                let n = enc.count(&text);
                                #[expect(clippy::cast_possible_truncation)]
                                {
                                    output_token_count += n as u32;
                                }
                            }
                            broadcast_stream_chunk(
                                ctx,
                                stream_id,
                                OutputStream::Answer,
                                text,
                                output_token_count,
                            );
                        }
                        StreamEvent::Reasoning(text) => {
                            // A reasoning chunk means hidden reasoning is streaming,
                            // so promote the phase to `Reasoning` (distinct from the
                            // pre-token send/wait boundary).
                            advance_stream_phase(&mut stream_phase, StreamPhase::Reasoning, ctx);
                            if let Some(enc) = &encoding {
                                // u128→u32 token counts, informational only.
                                let n = enc.count(&text);
                                #[expect(clippy::cast_possible_truncation)]
                                {
                                    output_token_count += n as u32;
                                }
                            }
                            broadcast_stream_chunk(
                                ctx,
                                stream_id,
                                OutputStream::Reasoning,
                                text,
                                output_token_count,
                            );
                        }
                        // `StreamEvent` is #[non_exhaustive] — a future event kind
                        // this loop doesn't forward should be ignored, not crash
                        // the agent loop.
                        _ => {}
                    }
                    Ok(())
                },
            );
            // On a 401 for an OAuth account, force-refresh (ignoring the expiry
            // buffer), rebuild the client with the new token, and retry the
            // request ONCE. A terminal failure becomes a re-login-required error
            // (the source has already broadcast the notice); a transient failure
            // or a non-OAuth account falls through with the original 401.
            if matches!(
                &result,
                Err(choreo_proto::InferenceError::Unauthorized { .. })
            ) && !oauth_refresh_attempted
            {
                oauth_refresh_attempted = true;
                match session.refresh_oauth_token(true, ctx) {
                    Ok(crate::oauth_refresh::RefreshTokenOutcome::Refreshed) => {
                        match session.resolve_provider(ctx) {
                            Ok(rebuilt) => {
                                active_client = rebuilt;
                                continue;
                            }
                            Err(e) => {
                                break Err(choreo_proto::InferenceError::OAuthReloginRequired {
                                    detail: e,
                                });
                            }
                        }
                    }
                    Ok(_) => break result,
                    Err(crate::oauth_refresh::RefreshFailure::Terminal) => {
                        break Err(choreo_proto::InferenceError::OAuthReloginRequired {
                            detail: "oauth grant rejected; sign in again".to_string(),
                        });
                    }
                    Err(crate::oauth_refresh::RefreshFailure::Transient { .. }) => break result,
                }
            }
            break result;
        };
        match turn_result {
            Ok(ChatTurnResult::FinalText(final_text)) => {
                match handle_final_text_turn(FinalTextTurnParams {
                    final_text,
                    session,
                    ctx,
                    current_turn_id,
                    stream_id,
                    turn_iter: &mut turn_iter,
                    provider_slug: &provider_slug,
                    model,
                    recovery_budget: &mut recovery_budget,
                    prev_resp_id: &mut prev_resp_id,
                    pending_user_text: &mut pending_user_text,
                })? {
                    FinalTextTurnOutcome::Continue => continue,
                    FinalTextTurnOutcome::End => return Ok(false),
                }
            }
            // The arm's own work — the ingest gate, the mixed-reply guidance, the
            // id minting, the turn recording, the cache-warmer arming and the
            // three dispatch phases — lives in `final_text`'s sibling
            // `tool_use_turn`, which also owns this arm's turn counter.
            Ok(ChatTurnResult::ToolUse(tool_use)) => {
                match handle_tool_use_turn(ToolUseTurnParams {
                    tool_use,
                    session,
                    ctx,
                    current_turn_id,
                    stream_id,
                    turn_iter: &mut turn_iter,
                    provider_slug: &provider_slug,
                    model,
                    tool_registry: &tool_registry,
                    cancel_rx,
                    warmer: warmer.as_ref(),
                    relay_warm_cancel: &relay_warm_cancel,
                    client,
                    estimated_prompt_tokens,
                    messages: &messages,
                    tools: &tools,
                    thinking_enabled,
                    recovery_budget: &mut recovery_budget,
                    prev_resp_id: &mut prev_resp_id,
                    pending_user_text: &mut pending_user_text,
                    known_hint_paths: &mut known_hint_paths,
                    pending_hints: &mut pending_hints,
                })? {
                    ToolUseTurnOutcome::Continue => continue,
                    ToolUseTurnOutcome::End(v) => return Ok(v),
                }
            }
            Ok(_) => {
                // A new ChatTurnResult variant (this enum is #[non_exhaustive])
                // is not handled here — fail loudly rather than silently
                // treating unknown output as success.
                warn!("provider returned an unhandled ChatTurnResult variant");
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "provider returned an unhandled turn result variant",
                ));
            }
            Err(choreo_proto::InferenceError::Cancelled) => {
                return Ok(true);
            }
            Err(e) => match e {
                // A truncated tool call is not a terminal failure: the provider
                // cut the response off at its output-token limit mid-call, so
                // the partial call was discarded (unsafe to execute). The shared
                // recovery tells the model what happened and retries in smaller
                // steps, bounded by the cause's budget; the loop then continues
                // to the next iteration instead of ending the request, so the
                // session survives without a restart.
                choreo_proto::InferenceError::TruncatedToolCall { discarded, finish } => {
                    // The affected tool(s) are named for the visible record by
                    // `discarded_names`, which keeps only provider-safe names
                    // and drops the rest — see its docs. The cropped JSON never
                    // reaches the transcript note.
                    crate::metrics::record_tool_call_discarded(
                        &provider_slug,
                        model,
                        TRUNCATION_RECOVERY.label,
                        finish,
                        discarded.len(),
                    );
                    let outcome = recover_unrunnable_turn(TurnRecoveryParams {
                        session,
                        ctx,
                        turn_id: current_turn_id,
                        stream_id,
                        cause: TRUNCATION_RECOVERY,
                        detail: &discarded_names(&discarded),
                        finish,
                        budget: &mut recovery_budget,
                        // The model never completed this response, so there is
                        // no reasoning to replay and no producer to attribute.
                        prose: None,
                        reasoning: None,
                        reasoning_artifact: None,
                        producer: None,
                        prev_resp_id: &mut prev_resp_id,
                        pending_user_text: &mut pending_user_text,
                    })?;
                    if outcome == RecoveryOutcome::Stop {
                        return Ok(false);
                    }
                    // No return: turn_iter advances at the loop bottom and the
                    // request re-sends with the seeded guidance.
                }
                // Any other inference failure (provider 4xx/5xx, network error,
                // deadline) leaves the current turn open and without a visible
                // record. Mark the failure on the turn and finalize + broadcast
                // it so clients render a red "Error:" block in the transcript
                // and the failure survives a daemon restart (finalize persists
                // the turn). The finalize is best-effort: a storage error must
                // not mask the original inference error, which the caller needs
                // to surface as RequestOutcome::Failed.
                other => {
                    session.set_turn_error(current_turn_id, other.to_string());
                    tracing::debug!(
                        session_id = ctx.session_id,
                        turn_id = current_turn_id,
                        error = %other,
                        "failure marked on turn; finalize will deliver the error turn to clients via TurnAppended",
                    );
                    if let Err(persist_err) =
                        finalize_and_broadcast_turn(session, ctx, current_turn_id)
                    {
                        warn!(
                            session_id = ctx.session_id,
                            turn_id = current_turn_id,
                            error = %persist_err,
                            "failed to persist the failed turn; the inference error is still reported",
                        );
                    }
                    return Err(other.into());
                }
            },
        }

        // Advance the turn counter for the next iteration. The two result arms
        // that continue on their own advance it inside their helper (each owns
        // the counter for its arm), so this increment serves the truncated
        // tool-call recovery above, whose arm falls through to here.
        turn_iter += 1;
    }
}

#[cfg(test)]
// Every test in this module reads the process-wide `PROVIDER_CATALOG`
// ArcSwap (via `build_chat_request_messages`/`initial_prev_resp_id`/
// `warn_on_uncaptured_reasoning`/`chat_reasoning_echo` → `model_reasoning_passback`, and
// `resolve_reasoning_effort` → `model_reasoning_capability`), and the daemon
// catalog-swap tests (`daemon.rs`, `#[serial(catalog)]`) mutate that global
// concurrently. Under libtest's in-process parallel execution a swap can land
// mid-assertion and the passback policy resolves from the wrong catalog
// (nextest isolates each test in its own process, so this only bites the
// `cargo test` fallback). Sharing the `catalog` serial key with every catalog
// reader/mutator in this binary serializes them against each other.
#[serial_test::serial(catalog)]
mod tests;
