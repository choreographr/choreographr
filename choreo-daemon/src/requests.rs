use crate::context::{self, SkillMeta};
use crate::providers::InferenceProvider;
use crate::reasoning::{
    build_chat_request_messages, initial_prev_resp_id, reasoning_artifact_tokens,
    warn_on_missing_reasoning_artifacts,
};
use crate::sessions::{AssistantResponse, RequestContext, SessionCommand, SessionState};
use crate::tools::ToolOutput;
use crate::tools::context::ToolContext;
use crate::tools::load_tools::{LoadToolsArgs, apply_load_tools};
use crate::tools::set_working_dir::{SetWorkingDirArgs, resolve_working_dir_path};
use crate::tools::unload_tools::{UnloadToolsArgs, apply_unload_tools};
use choreo_ai_protocols::openai::{ChatRequestMessage, ChatToolDefinition};
use choreo_ai_protocols::{
    ChatToolCall, ChatTurnRequest, ChatTurnResult, StreamEvent, ToolResultItem,
    model_reasoning_capability,
};
use choreo_proto::{
    AssistantToolCallRecord, DaemonMessage, OutputStream, ReasoningProducer, SessionEvent,
    SessionStatus,
};

use std::collections::{HashMap, HashSet};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use std::time::Instant;
use tracing::{debug, warn};

mod system_content;
mod tool_execution;
pub(crate) use system_content::*;
pub(crate) use tool_execution::*;
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

/// Estimate the number of prompt tokens for the current request using
/// tiktoken.  Returns a (encoding, estimated_tokens) pair so the caller
/// can reuse the encoding for output-token counting during streaming.
///
/// The estimate counts the `messages` slice as-is, which is the FULL visible
/// conversation. For a chained request (`previous_response_id` set) that is
/// deliberate: the adapter trims only the *wire* payload to the chain tail,
/// but the provider bills the whole context it holds server-side — and the
/// full conversation in `messages` IS that chained context plus the new tail,
/// so counting it already reflects the real billed input (the only thing it
/// misses is the previous system prompt, which stays in the chain while the
/// rebuilt one is sent afresh — a bounded, sub-request-sized undercount).
/// There is therefore NO chained-context addend here: adding the last
/// request's actual `prompt_tokens` (from usage) would count the conversation
/// twice, roughly doubling the estimate.
fn estimate_prompt_tokens(
    model: &str,
    messages: &[ChatRequestMessage],
    tools: &[ChatToolDefinition],
) -> (Option<&'static tiktoken::CoreBpe>, u32) {
    let encoding =
        tiktoken::encoding_for_model(model).or_else(|| tiktoken::get_encoding("cl100k_base"));
    let estimated = match &encoding {
        Some(enc) => {
            // Reasoning artifacts are NOT excluded: since phase 4b the builder
            // attaches them to assistant messages under echo policies
            // (ToolLoop/AllTurns/Signature), and providers bill replayed
            // reasoning as input tokens (the round-trip payload is part of the
            // context on keep-all models). The legacy string fields
            // (reasoning_content/reasoning/reasoning_text) are still never
            // populated by the daemon, so only `reasoning_artifact` is counted.
            let content_tokens: u32 = messages
                .iter()
                .filter_map(|m| m.content.as_deref())
                .map(|text| enc.count(text) as u32)
                .sum();

            // Vision images are billed by the provider as tokens based on their
            // (resized) dimensions. We don't know the exact per-provider
            // tokenizer for images, so use the fixed estimate the surveyed
            // agents converge on (~1000 tokens/image): the estimate feeds the
            // context-window display and compaction weighting, not billing.
            let image_tokens: u32 = messages
                .iter()
                .map(|m| (m.images.len() as u32).saturating_mul(IMAGE_TOKEN_ESTIMATE))
                .sum();

            let tool_call_tokens: u32 = messages
                .iter()
                .filter_map(|m| m.tool_calls.as_ref())
                .flat_map(|calls| calls.iter())
                .map(|tc| {
                    enc.count(&tc.id) as u32
                        + enc.count(&tc.kind) as u32
                        + enc.count(&tc.function.name) as u32
                        + enc.count(&tc.function.arguments) as u32
                })
                .sum();

            let tool_def_tokens: u32 = tools
                .iter()
                .filter_map(|def| {
                    match serde_json::to_string(def) {
                        Ok(s) => Some(enc.count(&s) as u32),
                        Err(e) => {
                            warn!(error = %e, "failed to serialize tool definition for token estimation");
                            None
                        }
                    }
                })
                .sum();

            let artifact_tokens: u32 = messages
                .iter()
                .filter_map(|m| m.reasoning_artifact.as_ref())
                .map(|artifact| reasoning_artifact_tokens(enc, artifact))
                .sum();

            content_tokens + tool_call_tokens + tool_def_tokens + artifact_tokens + image_tokens
        }
        None => {
            // Effectively unreachable — `get_encoding("cl100k_base")` above
            // always succeeds — but kept as defense-in-depth: if the fallback
            // encoding ever fails to load, report 0 rather than panic or reuse
            // a stale estimate. The estimate is informational only (billing
            // uses the provider-reported usage).
            tracing::warn!("no tiktoken encoding available for {model}");
            0
        }
    };
    (encoding, estimated)
}

/// Re-order tool results to match the model's original call order.
///
/// Concurrent completions are collected in arrival order (fast tools first),
/// so the accumulator fed to the provider on the next call is re-sorted to
/// match the assistant message's `tool_calls` array: some providers match
/// tool messages positionally, and the order should be deterministic. Items
/// whose `call_id` has no matching tool_call (e.g. a streaming stub created
/// before the start event arrived) sink to the end, keeping their relative
/// order (stable sort). The turn's own `tool_results` never need this — they
/// are seeded in call order and updated in place by `call_id`, so their
/// order is always the model's.
fn sort_by_call_order<T>(
    tool_calls: &[AssistantToolCallRecord],
    items: &mut [T],
    call_id_of: impl Fn(&T) -> &str,
) {
    let order: HashMap<&str, usize> = tool_calls
        .iter()
        .enumerate()
        .map(|(i, tc)| (tc.call_id.as_str(), i))
        .collect();
    if order.is_empty() {
        return;
    }
    items.sort_by_key(|item| order.get(call_id_of(item)).copied().unwrap_or(usize::MAX));
}

/// A successful session-config tool mutation, captured in Phase 1 and
/// applied to the worker's config copy in Phase 3.
///
/// The authoritative mutation is applied by the session main loop (via
/// DaemonCommand → SessionCommand routing); this worker copy must be updated
/// as well so the NEXT agent-loop iteration observes the change when it
/// rebuilds tool definitions, system content, and working-dir-relative file
/// operations.
enum PendingConfigChange {
    LoadTools(Vec<String>),
    UnloadTools(Vec<String>),
    /// The canonical path the tool resolved and the session main loop applied
    /// verbatim, taken from the tool's EXECUTED result (so no re-resolution
    /// and therefore no TOCTOU window).  `None` only when the executed result
    /// was unavailable AND re-resolution failed — the worker then skips the
    /// path update but still invalidates its skill cache so a stale cache
    /// never survives the request boundary.
    SetWorkingDir(Option<PathBuf>),
}

/// Whether `name` is one of the session-config tools that must run serially
/// and whose successful mutations are mirrored onto the worker config copy.
/// Single source of truth for the tool-name list used by the dispatch
/// partition and the mirror capture.
fn is_session_config_tool(name: &str) -> bool {
    matches!(name, "load_tools" | "unload_tools" | "set_working_dir")
}

/// Status label shown while a batch of concurrent tool calls executes.
///
/// Every non-session-config tool call lands in the concurrent dispatch
/// bucket, even a lone one — so the label must not claim parallelism for a
/// single call. Show the real tool name for `len == 1` and reserve
/// "(parallel)" for genuine multi-tool batches.
fn concurrent_tool_status_label(tools: &[ChatToolCall]) -> String {
    if tools.len() == 1 {
        tools[0].name.clone()
    } else {
        "(parallel)".into()
    }
}

/// Capture a successful session-config tool's mutation into a typed
/// [`PendingConfigChange`] for later application.  Called only for tools that
/// actually executed without error.
///
/// `base_working_dir` is the working directory in effect when the response
/// was planned — every `set_working_dir` call in the response resolved
/// against it, so the (rare) re-resolution fallback must too (chaining
/// relative resolutions against the mutated copy would diverge from the
/// canonical paths the tools sent to the main loop, which applies them
/// verbatim in call order).
fn pending_config_change(
    tool_call: &ChatToolCall,
    output: &ToolOutput,
    base_working_dir: Option<&Path>,
) -> Option<PendingConfigChange> {
    if !is_session_config_tool(&tool_call.name) {
        return None;
    }
    match tool_call.name.as_str() {
        "load_tools" => {
            let Ok(args) = serde_json::from_str::<LoadToolsArgs>(&tool_call.arguments_json) else {
                warn!(
                    tool_call_id = %tool_call.id,
                    "load_tools: could not parse args to mirror onto worker config",
                );
                return None;
            };
            Some(PendingConfigChange::LoadTools(args.groups))
        }
        "unload_tools" => {
            let Ok(args) = serde_json::from_str::<UnloadToolsArgs>(&tool_call.arguments_json)
            else {
                warn!(
                    tool_call_id = %tool_call.id,
                    "unload_tools: could not parse args to mirror onto worker config",
                );
                return None;
            };
            Some(PendingConfigChange::UnloadTools(args.groups))
        }
        "set_working_dir" => {
            // Prefer the canonical path from the tool's EXECUTED result: it
            // matches byte-for-byte what the session main loop applied, with
            // no re-resolution (and therefore no TOCTOU window in which the
            // directory could vanish between the tool's resolution and this
            // mirror).
            if let Some(path) = output
                .result_json
                .as_ref()
                .and_then(|v| v.get("path"))
                .and_then(|v| v.as_str())
            {
                return Some(PendingConfigChange::SetWorkingDir(Some(PathBuf::from(
                    path,
                ))));
            }
            // Fallback (practically unreachable — result_json is populated on
            // every successful execution): re-run the tool's own shared
            // resolution.  If even that fails, still return a None-path change
            // so the caller invalidates the worker's skill cache — a stale
            // cache must never survive the request boundary.
            let Ok(args) = serde_json::from_str::<SetWorkingDirArgs>(&tool_call.arguments_json)
            else {
                warn!(
                    tool_call_id = %tool_call.id,
                    "set_working_dir: could not parse args to mirror onto worker config",
                );
                return Some(PendingConfigChange::SetWorkingDir(None));
            };
            let path = resolve_working_dir_path(&args.path, base_working_dir).ok();
            Some(PendingConfigChange::SetWorkingDir(path))
        }
        _ => None,
    }
}

/// Apply a captured session-config mutation to the worker's config copy.
fn apply_pending_config_change(session: &mut SessionState, change: &PendingConfigChange) {
    match change {
        PendingConfigChange::LoadTools(groups) => {
            apply_load_tools(&mut session.config.active_tool_groups, groups);
            debug!(groups = ?groups, "mirrored load_tools onto worker session config");
        }
        PendingConfigChange::UnloadTools(groups) => {
            apply_unload_tools(&mut session.config.active_tool_groups, groups);
            debug!(groups = ?groups, "mirrored unload_tools onto worker session config");
        }
        PendingConfigChange::SetWorkingDir(path) => {
            if let Some(path) = path {
                session.config.working_dir = Some(path.clone());
            }
            // Always invalidate the skill cache: even when we could not
            // determine the new path, the authoritative state changed and a
            // stale cache would leak across the request boundary
            // (RequestFinished merges the worker's discovered_skills over the
            // main loop's invalidated None).  The main-loop handler does the
            // same for the authoritative state.
            session.discovered_skills = None;
            debug!(path = ?path, "mirrored set_working_dir onto worker session config");
        }
    }
}

pub(crate) fn run_agent_loop(
    client: &InferenceProvider,
    session: &mut SessionState,
    model: &str,
    request_id: u32,
    cancel_rx: &crossbeam_channel::Receiver<()>,
    ctx: &RequestContext,
    user_text: Option<String>,
) -> io::Result<bool> {
    let max_turns = ctx.max_turns;
    // `max_turns == 0` means *unlimited* — the loop runs until the model
    // produces a final answer, is cancelled, or hits an error.
    let limited = max_turns > 0;
    let provider_slug = client.provider_slug();

    // Phase 4c: ResponseId-policy providers chain reasoning continuity across
    // user turns via `previous_response_id`. The last response id is persisted
    // on the session config after every model call and restored here, so a new
    // user request continues the chain instead of resetting it. All other
    // policies reset to None — the id is meaningless outside Responses-style
    // APIs and must not leak across requests.
    let mut prev_resp_id = initial_prev_resp_id(session, provider_slug, model);
    let mut tool_results: Vec<ToolResultItem> = Vec::new();
    let mut known_hint_paths: Vec<PathBuf> = Vec::new();
    let mut pending_hints: Vec<String> = Vec::new();

    // Precondition guard (phase 4c): before sending a request whose passback
    // policy requires echoing reasoning, verify every turn that will carry an
    // assistant message has its artifact — and that the artifact's producer
    // matches the current model (a mid-session model switch omits the echo on
    // the wire there too, exactly like a missing artifact). `ToolLoop` checks
    // only tool-involving turns (where the provider demands the echo);
    // `AllTurns`/`Signature` echo on every assistant message. A turn recorded
    // before the artifact was captured (e.g. a pre-migration session) would
    // otherwise produce a mysterious 400 from the provider; surface it as a
    // diagnosable warning.
    warn_on_missing_reasoning_artifacts(session, ctx.session_id, provider_slug, model);

    // Lazily cache discovered skills — they don't change during a session
    if session.discovered_skills.is_none()
        && let Some(ref wd) = session.config.working_dir
    {
        session.discovered_skills = Some(context::discover_skills(wd));
    }

    let mut turn_iter: u32 = 0;
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
            resolve_reasoning_effort(client, model, ctx.session_id, turn_iter, configured);
        crate::metrics::record_turn(model);
        let tools = ctx
            .tool_registry
            .available_definitions(&session.config.active_tool_groups);
        if is_cancelled_once(cancel_rx) {
            return Ok(true);
        }

        // Start a new turn for this agent loop iteration.
        let turn_user_text = if turn_iter == 0 {
            user_text.clone()
        } else {
            None
        };
        let (current_turn_id, _) = session.start_turn(turn_user_text);
        broadcast_turn_appended(&ctx.cmd_tx, session, ctx.session_id, current_turn_id);
        if ctx
            .cmd_tx
            .send(SessionCommand::StatusChanged(SessionStatus::Inference))
            .is_err()
        {
            return Ok(false);
        }

        let system_content = {
            // Scope the immutable borrow on session so it ends before the
            // mutable borrows that follow (start_turn, set_assistant_response, etc.).
            let skills: &[SkillMeta] = session.discovered_skills.as_deref().unwrap_or_default();
            build_system_content(
                SystemContentParams {
                    working_dir: session.config.working_dir.as_deref(),
                    context_config: &session.config.context_config,
                    skills,
                    loaded_skill_bodies: &session.loaded_skill_bodies,
                    tool_registry: &ctx.tool_registry,
                    pending_hints: &pending_hints,
                    session_title: session.config.title.as_deref(),
                },
                &mut session.context_cache,
            )
        };
        pending_hints.clear();
        let messages =
            build_chat_request_messages(session, system_content.as_deref(), provider_slug, model);

        // The estimate counts `messages` as-is — the FULL conversation, not
        // the chained tail the adapter puts on the wire. That is intentional:
        // the provider bills the whole context it holds in the chain, and the
        // full conversation in `messages` already includes that context (plus
        // the new tail), so there is no separate chained-context addend — one
        // would count the conversation twice (billing itself is unaffected; it
        // uses the provider-reported usage, not this estimate).
        let (encoding, estimated_prompt_tokens) = estimate_prompt_tokens(model, &messages, &tools);

        let _ = ctx
            .cmd_tx
            .send(SessionCommand::Broadcast(DaemonMessage::Session {
                session_id: Some(ctx.session_id),
                event: SessionEvent::Started {
                    request_id,
                    turn_id: current_turn_id,
                    estimated_prompt_tokens,
                },
            }));

        let mut retry_cb: Option<choreo_ai_protocols::openai::RetryCallback> = Some(Box::new({
            let cmd_tx = ctx.cmd_tx.clone();
            move |attempt, max_attempts, delay| {
                let _ = cmd_tx.send(SessionCommand::StatusChanged(SessionStatus::Retrying {
                    attempt,
                    max_attempts,
                    delay_ms: delay.as_millis() as u64,
                }));
            }
        }));

        // Running count of output tokens produced by the current turn.
        let mut output_token_count: u32 = 0;

        // Gateway routing identity for the opencode zen/go providers: the
        // session's real id plus this turn's request id, as strings (the
        // gateway hashes the last 4 characters to pick an upstream bucket and
        // keys its sticky provider tracker on the session id). Owned locals,
        // so the borrowed fields outlive the provider call below.
        let oc_session_id = ctx.session_id.to_string();
        let oc_request_id = request_id.to_string();

        match client.chat_completion_turn_streaming(
            ChatTurnRequest {
                model,
                messages: &messages,
                tools: &tools,
                thinking_effort,
                on_retry: &mut retry_cb,
                cancel_rx: Some(cancel_rx),
                previous_response_id: prev_resp_id.as_deref(),
                tool_results: &tool_results,
                programmatic_tool_calling: client.supports_programmatic_tool_calling(model),
                session_id: oc_session_id,
                request_id: oc_request_id,
            },
            &mut |event| {
                match event {
                    StreamEvent::Answer(text) => {
                        if let Some(enc) = &encoding {
                            output_token_count += enc.count(&text) as u32;
                        }
                        let _ =
                            ctx.cmd_tx
                                .send(SessionCommand::Broadcast(DaemonMessage::Session {
                                    session_id: Some(ctx.session_id),
                                    event: SessionEvent::OutputChunk {
                                        request_id,
                                        stream: OutputStream::Answer,
                                        data: text.into_bytes(),
                                    },
                                }));
                        // Let the UI update its live token display on every
                        // chunk so the count feels responsive.
                        let _ =
                            ctx.cmd_tx
                                .send(SessionCommand::Broadcast(DaemonMessage::Session {
                                    session_id: Some(ctx.session_id),
                                    event: SessionEvent::LiveOutputTokenCount {
                                        request_id,
                                        output_tokens: output_token_count,
                                    },
                                }));
                    }
                    StreamEvent::Reasoning(text) => {
                        if let Some(enc) = &encoding {
                            output_token_count += enc.count(&text) as u32;
                        }
                        let _ =
                            ctx.cmd_tx
                                .send(SessionCommand::Broadcast(DaemonMessage::Session {
                                    session_id: Some(ctx.session_id),
                                    event: SessionEvent::OutputChunk {
                                        request_id,
                                        stream: OutputStream::Reasoning,
                                        data: text.into_bytes(),
                                    },
                                }));
                        let _ =
                            ctx.cmd_tx
                                .send(SessionCommand::Broadcast(DaemonMessage::Session {
                                    session_id: Some(ctx.session_id),
                                    event: SessionEvent::LiveOutputTokenCount {
                                        request_id,
                                        output_tokens: output_token_count,
                                    },
                                }));
                    }
                    // `StreamEvent` is #[non_exhaustive] — a future event kind
                    // this loop doesn't forward should be ignored, not crash
                    // the agent loop.
                    _ => {}
                }
                Ok(())
            },
        ) {
            Ok(ChatTurnResult::FinalText(final_text)) => {
                debug!(
                    session_id = ctx.session_id,
                    turn = turn_iter,
                    response_len = final_text.content.len(),
                    reasoning = final_text.reasoning.as_deref().unwrap_or_default(),
                    "model returned final text",
                );
                let token_usage = final_text.usage;
                accumulate_token_usage(session, &token_usage, turn_iter, ctx);
                broadcast_token_usage(ctx, session);
                // Write the reasoning artifact + producing model through to the
                // turn (phase 4c): the builder re-emits it on the next request
                // when the same model is still active and the passback policy
                // asks for it.
                // Record the producing provider+model once: it feeds both the
                // turn's provenance and the persisted response-id provenance.
                let producer = ReasoningProducer {
                    provider_slug: provider_slug.to_string(),
                    model: model.to_string(),
                };
                session.set_assistant_response(
                    current_turn_id,
                    AssistantResponse {
                        text: Some(final_text.content),
                        reasoning: final_text.reasoning,
                        token_usage,
                        reasoning_artifact: final_text.reasoning_artifact.clone(),
                        reasoning_producer: Some(producer.clone()),
                        ..Default::default()
                    },
                );
                // Persist the response id + its producing model so a
                // ResponseId-policy provider can chain the next user turn via
                // previous_response_id (restored at the top of the next loop
                // invocation only when the same provider+model is still
                // active — the id is service-bound and must not be replayed
                // into a different provider).
                session.config.last_response_id = final_text.response_id.clone();
                session.config.last_response_id_producer = Some(producer);
                finalize_and_broadcast_turn(session, ctx, current_turn_id)?;
                tool_results.clear();
                return Ok(false);
            }
            Ok(ChatTurnResult::ToolUse(tool_use)) => {
                let token_usage = tool_use.usage;
                accumulate_token_usage(session, &token_usage, turn_iter, ctx);
                broadcast_token_usage(ctx, session);
                // Build the call records once so the same ordered list seeds
                // both the assistant message's tool_calls and the placeholder
                // tool results (they must agree so the in-place updates below
                // match by call_id).
                let tool_call_records: Vec<AssistantToolCallRecord> = tool_use
                    .tool_calls
                    .iter()
                    .map(|tc| AssistantToolCallRecord {
                        call_id: tc.id.clone(),
                        name: tc.name.clone(),
                        arguments_json: tc.arguments_json.clone(),
                    })
                    .collect();
                // Invocation descriptions for the same calls, in the same
                // order: seeding them onto the placeholder results lets every
                // client render the tool's context (e.g. "Running command:
                // `…`.") from the moment the seeded turn is broadcast — before
                // any output streams — instead of waiting for a streaming
                // chunk that may be dropped or for the final record.
                let description_by_call: HashMap<String, String> = tool_use
                    .tool_calls
                    .iter()
                    .map(|tc| (tc.id.clone(), ctx.tool_registry.describe_invocation(tc)))
                    .collect();
                // Seed in call order by deriving the parallel slice from the
                // map, so `describe_invocation` runs exactly once per call.
                // The map is reused by the serial/concurrent dispatch phases
                // below — a second computation would be wasteful (`vm`
                // formats its source via rustfmt, `series` describes every
                // step).
                let invocation_descriptions: Vec<String> = tool_call_records
                    .iter()
                    .map(|tc| {
                        description_by_call
                            .get(&tc.call_id)
                            .cloned()
                            .unwrap_or_default()
                    })
                    .collect();
                // Record the producing provider+model once: it feeds both the
                // turn's provenance and the persisted response-id provenance.
                let producer = ReasoningProducer {
                    provider_slug: provider_slug.to_string(),
                    model: model.to_string(),
                };
                session.set_assistant_response(
                    current_turn_id,
                    AssistantResponse {
                        text: tool_use.content.clone(),
                        reasoning: tool_use.reasoning.clone(),
                        tool_calls: tool_call_records.clone(),
                        token_usage,
                        reasoning_artifact: tool_use.reasoning_artifact.clone(),
                        reasoning_producer: Some(producer.clone()),
                    },
                );
                // Seed one placeholder tool result per call, in the model's
                // call order, so the transcript renders every tool result in
                // that order at all times — each placeholder is filled in
                // place as its tool streams or finalizes.  The seeded
                // placeholder already carries the invocation description so
                // the live header matches the final record's exactly.
                session.seed_tool_results(
                    current_turn_id,
                    &tool_call_records,
                    &invocation_descriptions,
                );
                broadcast_turn_appended(&ctx.cmd_tx, session, ctx.session_id, current_turn_id);
                // Store response_id for chaining tool results back to this
                // turn, and persist it (+ its producing model) on the session
                // config so ResponseId-policy providers can chain across user
                // turns (restored at the top of the next loop invocation only
                // when the same provider+model is still active).
                prev_resp_id = tool_use.response_id.clone();
                session.config.last_response_id = prev_resp_id.clone();
                session.config.last_response_id_producer = Some(producer);
                tool_results.clear();

                // Partition tool calls into serial and concurrent.
                // Session-config tools (load_tools, unload_tools,
                // set_working_dir) run serially even though they are now
                // registry tools: their mutations are applied by the session
                // main loop via daemon → session command routing, and serial
                // execution preserves the model's call order so e.g. a
                // load_tools followed by a set_working_dir lands in the
                // intended sequence.
                let (mutators, concurrent): (Vec<_>, Vec<_>) = tool_use
                    .tool_calls
                    .into_iter()
                    .partition(|tc| is_session_config_tool(&tc.name));

                // All session-config tools in this response resolve relative
                // paths against the working directory in effect when the
                // response was planned. Capture it once so the (rare) Phase 3
                // mirror fallback reproduces exactly the canonical paths the
                // tools sent to the main loop (which applies them verbatim, in
                // call order).
                let turn_base_working_dir = session.config.working_dir.clone();

                // Successful session-config mutations, in call order, to be
                // mirrored onto this worker's config copy once every tool in
                // the response has executed (see Phase 3 below).
                let mut pending_config_changes: Vec<PendingConfigChange> = Vec::new();

                // Sticky cancellation: a cancel observed during Phase 1 or
                // Phase 2 stops the request, but only AFTER Phase 3 has
                // mirrored the config changes from the tools that already
                // ran — the same ordering the no-cancel path uses.
                let mut cancelled = false;

                // call_ids whose results were actually recorded, so a
                // cancelled request can mark the never-executed placeholders
                // (see `SessionState::mark_unexecuted_tool_results`).
                let mut executed_tool_calls: HashSet<String> = HashSet::new();

                // ── Phase 1: Session-config tools (serial) ────────
                for tool_call in mutators.into_iter() {
                    if is_cancelled_once(cancel_rx) {
                        cancelled = true;
                        break;
                    }

                    // The invocation description (computed once, above) rides
                    // the ToolCallStarted broadcast so clients render the
                    // tool's context — e.g. "Running command: `…`." — from
                    // the start event, not from a streaming chunk that may be
                    // dropped; the serial error/panic outputs carry it too (a
                    // timed-out or cancelled tool renders with the same
                    // invocation context the concurrent path shows).
                    let invocation_description = description_by_call
                        .get(&tool_call.id)
                        .cloned()
                        .unwrap_or_default();

                    if let Err(e) =
                        ctx.cmd_tx
                            .send(SessionCommand::Broadcast(DaemonMessage::Session {
                                session_id: Some(ctx.session_id),
                                event: SessionEvent::ToolCallStarted {
                                    request_id,
                                    call_id: tool_call.id.clone(),
                                    tool_name: tool_call.name.clone(),
                                    arguments_json: tool_call.arguments_json.clone(),
                                    invocation_description: invocation_description.clone(),
                                },
                            }))
                    {
                        warn!(%request_id, call_id = %tool_call.id, error = %e, "failed to broadcast ToolCallStarted");
                    }

                    let tool_timeout =
                        determine_tool_timeout(&tool_call.name, &tool_call.arguments_json)
                            .unwrap_or(Duration::from_secs(60));

                    if ctx
                        .cmd_tx
                        .send(SessionCommand::StatusChanged(SessionStatus::ToolCall(
                            tool_call.name.clone(),
                        )))
                        .is_err()
                    {
                        return Ok(false);
                    }

                    debug!(
                        session_id = ctx.session_id,
                        turn = turn_iter,
                        tool_name = %tool_call.name,
                        tool_call_id = %tool_call.id,
                        args_preview = %(&tool_call.arguments_json[..tool_call.arguments_json.len().min(200)]),
                        "executing tool (serial)",
                    );

                    let turn_working_dir = session.config.working_dir.clone();
                    // TEMPORARY: pass the daemon's Substrate credential through
                    // the single `x_credentials` slot so the content write tools
                    // can build a ChainAccount. This single-slot reuse is a
                    // stopgap (the X tools use the same slot, so only one
                    // credential rides it) until a proper tool→keystore
                    // credential-access system replaces it.
                    let (mut output, tool_cancelled, image) = execute_tool_with_timeout(
                        &tool_call,
                        ctx.substrate_credential.as_ref(),
                        turn_working_dir.as_deref(),
                        tool_timeout,
                        request_id,
                        ctx.session_id,
                        session,
                        cancel_rx,
                        ctx,
                        &invocation_description,
                    );
                    if tool_cancelled {
                        // The wait observed a cancellation signal (consumed by
                        // its `select!`), so the request must stop after this
                        // tool's result is recorded below.
                        cancelled = true;
                    }

                    record_tool_completion(
                        request_id,
                        session,
                        &tool_call,
                        &mut output,
                        image,
                        ctx,
                        current_turn_id,
                        &mut tool_results,
                        &mut known_hint_paths,
                        &mut pending_hints,
                    );
                    executed_tool_calls.insert(tool_call.id.clone());

                    // Only mirror mutations that were actually accepted: an
                    // error (e.g. inactive session, daemon communication
                    // failure) means the authoritative state was NOT changed,
                    // so this worker must not pretend it was.
                    if !output.is_error
                        && let Some(change) = pending_config_change(
                            &tool_call,
                            &output,
                            turn_base_working_dir.as_deref(),
                        )
                    {
                        pending_config_changes.push(change);
                    }

                    if cancelled {
                        // Stop executing further serial tools; the concurrent
                        // batch is skipped and Phase 3 still runs below.
                        break;
                    }
                }

                // ── Phase 2: All remaining tools (concurrent) ───────
                if !cancelled && !concurrent.is_empty() {
                    for tc in concurrent.iter() {
                        // Carry the invocation description (computed once,
                        // above) on the start event so clients render the
                        // tool's context (e.g. "Running command: `…`.") from
                        // the broadcast rather than from a streaming chunk —
                        // chunks are droppable under load, and this event is
                        // queued before the tool even starts.
                        let invocation_description =
                            description_by_call.get(&tc.id).cloned().unwrap_or_default();
                        if let Err(e) =
                            ctx.cmd_tx
                                .send(SessionCommand::Broadcast(DaemonMessage::Session {
                                    session_id: Some(ctx.session_id),
                                    event: SessionEvent::ToolCallStarted {
                                        request_id,
                                        call_id: tc.id.clone(),
                                        tool_name: tc.name.clone(),
                                        arguments_json: tc.arguments_json.clone(),
                                        invocation_description,
                                    },
                                }))
                        {
                            warn!(%request_id, call_id = %tc.id, error = %e, "failed to broadcast ToolCallStarted");
                        }
                    }

                    if ctx
                        .cmd_tx
                        .send(SessionCommand::StatusChanged(SessionStatus::ToolCall(
                            concurrent_tool_status_label(&concurrent),
                        )))
                        .is_err()
                    {
                        return Ok(false);
                    }

                    debug!(
                        session_id = ctx.session_id,
                        turn = turn_iter,
                        count = concurrent.len(),
                        "dispatching {} tools concurrently",
                        concurrent.len(),
                    );

                    let cancel_flag = Arc::new(AtomicBool::new(false));

                    let tool_ctx = ToolContext {
                        session_id: ctx.session_id,
                        db: Arc::clone(&ctx.db),
                        daemon_tx: ctx.daemon_tx.clone(),
                        active_tool_groups: session.config.active_tool_groups.clone(),
                        reasoning_effort: session.config.reasoning_effort.clone(),
                        selected_model: session.config.selected_model.clone(),
                        working_dir: session.config.working_dir.clone(),
                        cancelled: Arc::clone(&cancel_flag),
                        account_name: session.config.account_name.clone(),
                    };

                    let cmd_tx = ctx.cmd_tx.clone();
                    let reg = Arc::clone(&ctx.tool_registry);

                    // Shared batch channel: every wait-loop thread delivers its
                    // final ToolHandle here the moment the tool completes
                    // (success, error, timeout, or panic). No joins — results
                    // arrive in *completion* order, so a fast tool broadcasts
                    // immediately instead of waiting for the slowest tool the
                    // model listed before it.
                    let (batch_tx, batch_rx) = crossbeam_channel::unbounded::<ToolHandle>();

                    // Dispatch-order metadata for every call, retained for the
                    // (rare) fallback synthesis below: rebuilding the results
                    // of wait-loop threads that died before delivering.
                    let mut call_infos: Vec<CallInfo> = Vec::with_capacity(concurrent.len());
                    for tool_call in concurrent.into_iter() {
                        let timeout =
                            determine_tool_timeout(&tool_call.name, &tool_call.arguments_json);
                        let invocation_description = description_by_call
                            .get(&tool_call.id)
                            .cloned()
                            .unwrap_or_default();
                        // One dispatch-time instant for both the handle (delivered
                        // path) and the CallInfo (panic-synthesis path), so the
                        // collector's per-tool elapsed log is consistent either way.
                        let started_at = Instant::now();
                        let call_id = tool_call.id.clone();
                        let tool_name = tool_call.name.clone();
                        let arguments_json = tool_call.arguments_json.clone();
                        // A call counts as executed only once its result is
                        // actually recorded (`process_tool_handle`): the drain
                        // below can stop on a cancel, so a dispatched but
                        // unfinished call must still be swept as unexecuted.
                        // The spawn returns the collector-side kill sender;
                        // it is retained in the CallInfo for the whole batch
                        // drain so a mid-batch cancel can stop every still-
                        // running wait-loop promptly.
                        let kill_tx = spawn_single_tool(SpawnToolArgs {
                            tool_call,
                            timeout,
                            request_id,
                            session_id: ctx.session_id,
                            registry: Arc::clone(&reg),
                            cmd_tx: cmd_tx.clone(),
                            // TEMPORARY: clone the daemon's Substrate credential
                            // into the single `x_credentials` slot so the content
                            // write tools can build a ChainAccount. This
                            // single-slot reuse is a stopgap (the X tools use the
                            // same slot, so only one credential rides it) until a
                            // proper tool→keystore credential-access system
                            // replaces it.
                            x_credentials: ctx.substrate_credential.clone(),
                            working_dir: session.config.working_dir.clone(),
                            ctx: tool_ctx.clone(),
                            invocation_description: invocation_description.clone(),
                            started_at,
                            result_tx: batch_tx.clone(),
                        });
                        call_infos.push(CallInfo {
                            call_id,
                            tool_name,
                            arguments_json,
                            invocation_description,
                            started_at,
                            kill_tx,
                        });
                    }
                    // Drop our own sender: the batch channel disconnects only
                    // when every wait-loop thread has finished (sent or died),
                    // which is the receive loop's completion signal.
                    drop(batch_tx);

                    let batch_size = call_infos.len();

                    // Per-tool completion processing: broadcast the result the
                    // moment it arrives and accumulate it for the next model
                    // call. Extracted into a closure so the happy path and the
                    // panic-synthesis fallback share one implementation.
                    let mut process_tool_handle =
                        |ToolHandle {
                             tool_call,
                             mut output,
                             image,
                             started_at,
                         }: ToolHandle| {
                            let elapsed = started_at.elapsed();

                            debug!(
                                session_id = ctx.session_id,
                                turn = turn_iter,
                                tool_name = %tool_call.name,
                                elapsed_ms = elapsed.as_millis(),
                                result_len = output.content.len(),
                                is_error = output.is_error,
                                "tool finished (concurrent)",
                            );

                            record_tool_completion(
                                request_id,
                                session,
                                &tool_call,
                                &mut output,
                                image,
                                ctx,
                                current_turn_id,
                                &mut tool_results,
                                &mut known_hint_paths,
                                &mut pending_hints,
                            );
                            // The result is recorded now — this call_id must
                            // not be swept by the cancelled-turn placeholder
                            // sweep (`mark_unexecuted_tool_results`).
                            executed_tool_calls.insert(tool_call.id.clone());
                        };

                    // Which call_ids actually delivered, so the disconnected-
                    // channel fallback below synthesizes only the genuinely
                    // missing tools (handles arrive in completion order, NOT
                    // dispatch order).
                    let mut delivered: HashSet<String> = HashSet::with_capacity(batch_size);
                    while delivered.len() < batch_size {
                        // Block until a tool completes OR the request is
                        // cancelled.  `select_biased!` (cancel arm first) makes
                        // both waits event-driven: a cancel wakes this loop the
                        // instant it is sent, and a quiet batch costs nothing
                        // (no 200 ms ticks).  The bias is a preference, not a
                        // guarantee: an already-queued cancel is selected
                        // deterministically (the biased fast path scans arms in
                        // order), while a cancel that lands mid-block only
                        // *tends* to beat a simultaneously-ready result.  Both
                        // outcomes are handled correctly — a cancel always
                        // stops the batch, and any result queued at that
                        // instant is drained rather than discarded.  The cancel
                        // sender cannot disconnect while the worker runs (it
                        // is dropped only on RequestFinished), so a firing
                        // cancel arm always means "cancel".
                        let (cancelled_now, handle_msg) = crossbeam_channel::select_biased! {
                            recv(cancel_rx) -> _ => (true, None),
                            recv(batch_rx) -> msg => (false, Some(msg)),
                        };
                        if cancelled_now {
                            cancel_flag.store(true, Ordering::Relaxed);
                            cancelled = true;
                            // Bias for cancel: stop waiting for the slowest
                            // tool right now.  First, kill every still-running
                            // wait-loop so its forwarder stops streaming
                            // promptly, its cooperative
                            // `ToolContext.cancelled` flag is set, and it
                            // delivers a "cancelled" result instead of waiting
                            // for the tool (sends to wait-loops that already
                            // exited fail silently).  Then don't discard
                            // results that already landed in the same instant
                            // — drain them (non-blocking) so the transcript
                            // keeps the real output of tools that did
                            // complete.
                            for info in &call_infos {
                                let _ = info.kill_tx.send(());
                            }
                            while let Ok(handle) = batch_rx.try_recv() {
                                delivered.insert(handle.tool_call.id.clone());
                                process_tool_handle(handle);
                            }
                            // Every live wait-loop selects on its kill channel,
                            // so after the kill broadcast each one delivers its
                            // outcome (a real result that won the same-instant
                            // race, or a "cancelled" result) promptly.  Keep
                            // draining until all batch_size handles have
                            // arrived: this makes the transcript deterministic
                            // — a killed wait-loop's "cancelled" handle can no
                            // longer be dropped by a race with the placeholder
                            // sweep, so no unfinished call is swept that
                            // actually delivered an outcome.  The wait is
                            // bounded by thread scheduling, not by the slowest
                            // tool (its execution thread keeps running in the
                            // background either way, and its late result is
                            // discarded once the wait-loop exits).  A
                            // disconnect means a wait-loop thread died before
                            // delivering — synthesize its result, matching the
                            // normal batch-end path below.
                            while delivered.len() < batch_size {
                                match batch_rx.recv() {
                                    Ok(handle) => {
                                        delivered.insert(handle.tool_call.id.clone());
                                        process_tool_handle(handle);
                                    }
                                    Err(_) => {
                                        warn!(
                                            session_id = ctx.session_id,
                                            request_id,
                                            delivered = delivered.len(),
                                            expected = batch_size,
                                            "concurrent tool batch ended early after cancel; synthesizing missing tool results",
                                        );
                                        for info in missing_calls(&call_infos, &delivered) {
                                            process_tool_handle(panic_tool_handle(info));
                                        }
                                        break;
                                    }
                                }
                            }
                            break;
                        }
                        if let Some(msg) = handle_msg {
                            match msg {
                                Ok(handle) => {
                                    delivered.insert(handle.tool_call.id.clone());
                                    process_tool_handle(handle);
                                }
                                Err(_) => {
                                    // Every wait-loop thread has exited but fewer
                                    // handles arrived than expected: some thread
                                    // panicked before sending. Synthesize the same
                                    // "tool thread panicked" output the old
                                    // join-based path produced, for the missing
                                    // slots only (by call_id), so the turn still
                                    // records a result for every call.
                                    warn!(
                                        session_id = ctx.session_id,
                                        request_id,
                                        delivered = delivered.len(),
                                        expected = batch_size,
                                        "concurrent tool batch ended early; synthesizing missing tool results",
                                    );
                                    for info in missing_calls(&call_infos, &delivered) {
                                        process_tool_handle(panic_tool_handle(info));
                                    }
                                    break;
                                }
                            }
                        }
                    }

                    // ── Phase 2b: Normalize the next-call accumulator ──
                    //
                    // The receive loop above processed results in completion
                    // order so each broadcast hit the TUI the moment its tool
                    // finished (and streaming chunks flow even earlier). The
                    // turn's own tool_results never need re-ordering: they
                    // were seeded in call order before execution and updated
                    // in place by call_id, so the transcript is always in the
                    // model's order. The accumulator sent to the provider on
                    // the next agent-loop iteration should mirror the
                    // assistant message's tool_calls array, so re-sort it now
                    // the batch is complete — reusing `tool_call_records` (the
                    // same ordered list that seeded the placeholders and the
                    // assistant message) instead of re-reading the turn.
                    sort_by_call_order(&tool_call_records, &mut tool_results, |r| {
                        r.call_id.as_str()
                    });
                }

                // ── Phase 3: Mirror session-config changes onto the
                //    worker's config copy ────────────────────────────
                //
                // The authoritative mutations were applied by the session
                // main loop. The worker's throwaway copy must be updated too,
                // or the next loop iteration would keep building tool
                // definitions, system content, and file ops from the stale
                // pre-change state. This runs only after every tool in the
                // response has executed: the model planned all of them
                // against the state at the start of the turn (they are a
                // parallel batch), so applying the change earlier — e.g.
                // right after Phase 1 — would silently alter the semantics
                // of tools batched alongside the config change. The worker
                // copy is still discarded at request end, so the two copies
                // cannot drift across requests.
                for change in &pending_config_changes {
                    apply_pending_config_change(session, change);
                }

                // A cancel observed during tool execution stops the request
                // here — after Phase 3 has mirrored the config changes from
                // the tools that already ran, matching the no-cancel ordering.
                if cancelled {
                    // Tools that never ran still hold empty seeded placeholders;
                    // mark them so the transcript and the next provider request
                    // don't carry empty tool messages for calls that were never
                    // executed (the cancelled turn is not finalized, so it
                    // survives into the next request's history).
                    session.mark_unexecuted_tool_results(current_turn_id, &executed_tool_calls);
                    broadcast_turn_appended(&ctx.cmd_tx, session, ctx.session_id, current_turn_id);
                    return Ok(true);
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
            Err(e) => {
                // Finalize the turn so the session doesn't have an orphaned
                // open turn that confuses the LLM on the next request.
                if matches!(&e, choreo_proto::InferenceError::TruncatedToolCall { .. }) {
                    tracing::warn!(?e, "truncated tool call, finalizing turn gracefully");
                    session.set_assistant_response(
                        current_turn_id,
                        AssistantResponse {
                            text: Some(format!("[tool call truncated: {e}]")),
                            // No artifact or producer: the model never completed a
                            // response, so there is nothing to replay. Everything
                            // else (tool_calls, usage) stays at its default.
                            ..Default::default()
                        },
                    );
                    finalize_and_broadcast_turn(session, ctx, current_turn_id)?;
                    tool_results.clear();
                    return Ok(false);
                }
                // Any other inference failure (provider 4xx/5xx, network error,
                // deadline) leaves the current turn open and without a visible
                // record. Mark the failure on the turn and finalize + broadcast
                // it so clients render a red "Error:" block in the transcript
                // and the failure survives a daemon restart (finalize persists
                // the turn). The finalize is best-effort: a storage error must
                // not mask the original inference error, which the caller needs
                // to surface as RequestOutcome::Failed.
                session.set_turn_error(current_turn_id, e.to_string());
                tracing::debug!(
                    session_id = ctx.session_id,
                    turn_id = current_turn_id,
                    %e,
                    "failure marked on turn; finalize will deliver the error turn to clients via TurnAppended",
                );
                if let Err(persist_err) = finalize_and_broadcast_turn(session, ctx, current_turn_id)
                {
                    warn!(
                        session_id = ctx.session_id,
                        turn_id = current_turn_id,
                        error = %persist_err,
                        "failed to persist the failed turn; the inference error is still reported",
                    );
                }
                return Err(e.into());
            }
        }

        // Advance the turn counter for the next iteration.
        turn_iter += 1;
    }
}

/// Fixed per-image token estimate for prompt-token accounting. Providers bill
/// image input as tokens derived from (resized) dimensions, with no portable
/// way to compute the exact count client-side; the surveyed agents converge on
/// ~1000 tokens/image, which is a good middle estimate (DeepSeek caps at 384,
/// Anthropic/OpenAI high-detail run higher). This feeds the context-window
/// display and compaction weighting, not billing (which uses provider usage).
pub const IMAGE_TOKEN_ESTIMATE: u32 = 1000;
pub const REQUEST_IMAGE_BYTES: &[u8] = include_bytes!("../assets/dua.jpg");
pub const REQUEST_IMAGE_MIME_TYPE: &str = "image/jpeg";
pub const REQUEST_IMAGE_WIDTH: u32 = 640;
pub const REQUEST_IMAGE_HEIGHT: u32 = 640;

#[cfg(test)]
// Every test in this module reads the process-wide `PROVIDER_CATALOG`
// ArcSwap (via `build_chat_request_messages`/`initial_prev_resp_id`/
// `warn_on_missing_reasoning_artifacts` → `model_reasoning_passback`, and
// `resolve_reasoning_effort` → `model_reasoning_capability`), and the daemon
// catalog-swap tests (`daemon.rs`, `#[serial(catalog)]`) mutate that global
// concurrently. Under libtest's in-process parallel execution a swap can land
// mid-assertion and the passback policy resolves from the wrong catalog
// (nextest isolates each test in its own process, so this only bites the
// `cargo test` fallback). Sharing the `catalog` serial key with every catalog
// reader/mutator in this binary serializes them against each other.
#[serial_test::serial(catalog)]
mod tests;
