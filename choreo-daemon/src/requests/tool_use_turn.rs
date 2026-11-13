//! The agent loop's tool-use arm: everything that follows a provider reply
//! carrying tool calls.
//!
//! This module is the single convergence point for a turn that must actually
//! run something. It gates the reply's calls on the provider's accepted name
//! set, records the turn and a placeholder result per call, arms the cache
//! warmer for the blocking window that follows, and then runs the three
//! dispatch phases — session-config mutations serially, every other call
//! concurrently, and the worker's config copy mirrored last — before
//! checkpointing the completed turn.
//!
//! Two invariants are structural here rather than re-checked by the loop:
//!
//! - **An unrunnable turn is recovered, not dispatched.** A reply whose calls
//!   all fail the ingest gate never reaches the dispatcher: the shared recovery
//!   unit records what was lost and seeds the next turn's guidance, exactly as
//!   for a truncation.
//! - **The helper owns the turn counter for its arm.** Every exit that lets the
//!   loop continue has already advanced the counter, so the agent loop never
//!   touches it after [`ToolUseTurnOutcome::Continue`]. There is no "does this
//!   path skip the loop-tail increment?" question to ask — a `continue` in the
//!   loop cannot skip an increment that no longer belongs to this arm.

use std::collections::{HashMap, HashSet};
use std::io;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};
use tracing::{debug, warn};

use choreo_ai_protocols::ChatToolCall;
use choreo_ai_protocols::openai::{ChatRequestMessage, ChatToolDefinition};
use choreo_ai_protocols::{ProviderProtocol, lookup_provider};
use choreo_proto::{
    AssistantToolCallRecord, DaemonMessageType, ReasoningProducer, SessionEvent, SessionStatus,
};

use crate::cache_warm::WarmHandle;
use crate::providers::InferenceProvider;
use crate::reasoning::chains_via_response_id;
use crate::sessions::{AssistantResponse, RequestContext, SessionCommand, SessionState};
use crate::tools::ToolRegistry;
use crate::tools::context::ToolContext;
use crate::tools::tool_call_is_unrunnable;

use super::recovery::{
    RecoveryBudget, RecoveryOutcome, TRUNCATION_MIXED_RECOVERY_INSTRUCTION, TRUNCATION_RECOVERY,
    TurnRecoveryParams, UNNAMED_TOOL_CALL_RECOVERY, UNNAMED_TOOL_CALL_RECOVERY_INSTRUCTION,
    recover_unrunnable_turn, recovery_detail,
};
use super::{
    CallInfo, ExecuteToolParams, PendingConfigChange, SpawnToolArgs, ToolCompletionParams,
    ToolHandle, accumulate_token_usage, apply_pending_config_change, broadcast_token_usage,
    broadcast_turn_appended, concurrent_tool_status_label, determine_tool_timeout,
    execute_tool_with_timeout, is_cancelled_once, is_session_config_tool, missing_calls,
    panic_tool_handle, pending_config_change, persist_turn_checkpoint, record_tool_completion,
    spawn_single_tool,
};

/// Whether a provider's catalog protocol is the Anthropic Messages wire
/// format. Used by the cache-warming arm: an Anthropic request with thinking
/// enabled is the one non-replayable shape (a 1-token cap changes the thinking
/// budget, hence the prompt-cache key) — see [`crate::cache_warm::is_replayable`].
fn protocol_is_anthropic(slug: &str) -> bool {
    lookup_provider(slug).is_some_and(|p| matches!(p.protocol, ProviderProtocol::AnthropicMessages))
}

/// Everything [`handle_tool_use_turn`] needs, bundled so the routine takes one
/// value rather than twenty positional arguments.
pub(crate) struct ToolUseTurnParams<'a> {
    /// The provider's tool-use reply, moved in whole: its calls are dispatched
    /// and the prose/reasoning/artifact/usage it carries are recorded on the
    /// turn.
    pub tool_use: choreo_ai_protocols::ChatAssistantToolUse,
    /// The session whose transcript the turn and its tool results are written
    /// to.
    pub session: &'a mut SessionState,
    /// Request context (command channel, session id, database) for the
    /// broadcasts and the config mirror.
    pub ctx: &'a RequestContext,
    /// The turn the reply belongs to.
    pub current_turn_id: u32,
    /// The request's stream id, carried on every broadcast and log line.
    pub stream_id: u64,
    /// This request's turn counter: read for logging, advanced on every exit
    /// that lets the loop continue (see [`ToolUseTurnOutcome`]).
    pub turn_iter: &'a mut u32,
    /// The account's provider slug: reasoning provenance, metric dimensions,
    /// and the Anthropic-protocol lookup the warmer needs.
    pub provider_slug: &'a str,
    /// The model that produced the reply, for the same purposes.
    pub model: &'a str,
    /// This turn's pinned tool-registry snapshot (definitions were built
    /// against it, and invocation descriptions come from it).
    pub tool_registry: &'a Arc<ToolRegistry>,
    /// The request's cancellation channel, checked before the serial batch and
    /// selected on while the concurrent batch drains.
    pub cancel_rx: &'a crossbeam_channel::Receiver<()>,
    /// The request's cache warmer, armed for the blocking tool window; `None`
    /// when warming is off.
    pub warmer: Option<&'a WarmHandle>,
    /// Relays a cancel to the warmer: it must not share the loop's receiver
    /// (one message, one receiver), so every cancel site calls this.
    pub relay_warm_cancel: &'a dyn Fn(),
    /// The provider client the request was sent through — the warmer's arming
    /// gate reads its cache dialect and retention tier.
    pub client: &'a InferenceProvider,
    /// The prompt-token estimate for the request just sent, so the warmer
    /// schedules against the real prefix size.
    pub estimated_prompt_tokens: u32,
    /// The message prefix the request just sent (replayed by a warm ping).
    pub messages: &'a [ChatRequestMessage],
    /// The tool definitions the request just sent (replayed by a warm ping).
    pub tools: &'a [ChatToolDefinition],
    /// Whether extended thinking is on for this turn; an Anthropic thinking
    /// request is not replayable with a capped output.
    pub thinking_enabled: bool,
    /// The consecutive-recovery budget an unrunnable reply draws down, and the
    /// registry of everything a productive turn resets.
    pub recovery_budget: &'a mut RecoveryBudget,
    /// The in-request response-id chain: chained onto this turn's reply, and
    /// dropped by a recovery.
    pub prev_resp_id: &'a mut Option<String>,
    /// The next turn's user text: the recovery instruction, or the mixed-reply
    /// guidance telling the model which calls were dropped.
    pub pending_user_text: &'a mut Option<String>,
    /// Hint paths the session has already been told about, accumulated across
    /// the whole request.
    pub known_hint_paths: &'a mut Vec<PathBuf>,
    /// Hints to inject into the next turn's system content.
    pub pending_hints: &'a mut Vec<String>,
}

/// What the agent loop does after a tool-use turn.
pub(crate) enum ToolUseTurnOutcome {
    /// The turn is complete (or was recovered) and the helper has already
    /// advanced the turn counter: the loop continues without touching it.
    Continue,
    /// The request ends now with this value.
    End(bool),
}

/// Record a tool-use turn, run its calls, and report how the loop proceeds.
///
/// The turn is recovered rather than dispatched when no call in the reply is
/// runnable, and the request stops after Phase 3 mirrors the config changes when
/// a cancel was observed mid-dispatch — so the caller reads only the outcome,
/// never the intermediate state.
pub(crate) fn handle_tool_use_turn(
    params: ToolUseTurnParams<'_>,
) -> io::Result<ToolUseTurnOutcome> {
    let ToolUseTurnParams {
        tool_use,
        session,
        ctx,
        current_turn_id,
        stream_id,
        turn_iter,
        provider_slug,
        model,
        tool_registry,
        cancel_rx,
        warmer,
        relay_warm_cancel,
        client,
        estimated_prompt_tokens,
        messages,
        tools,
        thinking_enabled,
        recovery_budget,
        prev_resp_id,
        pending_user_text,
        known_hint_paths,
        pending_hints,
    } = params;

    let choreo_ai_protocols::ChatAssistantToolUse {
        content,
        tool_calls,
        discarded_tool_calls,
        finish,
        reasoning,
        usage,
        response_id,
        reasoning_artifact,
    } = tool_use;
    let token_usage = usage;
    accumulate_token_usage(
        session,
        token_usage.as_ref(),
        *turn_iter,
        provider_slug,
        model,
        ctx,
    );
    broadcast_token_usage(ctx, session);

    // Split the reply's calls on whether they name a tool the
    // provider will accept. An unacceptable name can never be
    // dispatched — the registry answers "unknown tool", the model
    // retries the same malformed call, and an unlimited
    // (`max_turns == 0`) loop would never stop — so such a call is
    // dropped here, before anything downstream can see it. Keeping
    // only the runnable subset also means `is_session_config_tool`,
    // `determine_tool_timeout`, `describe_invocation` and the
    // registry never receive one.
    let (mut runnable_calls, unnamed_calls): (Vec<ChatToolCall>, Vec<ChatToolCall>) = tool_calls
        .into_iter()
        .partition(|tc| !tool_call_is_unrunnable(&tc.name));
    let unnamed_call_count = unnamed_calls.len();
    if runnable_calls.is_empty() {
        // Nothing runnable survived: the turn is recovered exactly
        // like a truncated one — its calls are never recorded or
        // dispatched, and the model is told what went wrong.
        let producer = ReasoningProducer {
            provider_slug: provider_slug.to_string(),
            model: model.to_string(),
        };
        let outcome = recover_unrunnable_turn(TurnRecoveryParams {
            session,
            ctx,
            turn_id: current_turn_id,
            stream_id,
            cause: UNNAMED_TOOL_CALL_RECOVERY,
            detail: &recovery_detail(&unnamed_calls),
            finish,
            budget: recovery_budget,
            prose: content,
            reasoning,
            reasoning_artifact,
            producer: Some(producer),
            prev_resp_id,
            pending_user_text,
        })?;
        if outcome == RecoveryOutcome::Stop {
            return Ok(ToolUseTurnOutcome::End(false));
        }
        // The helper owns this arm's turn counter: the agent loop's tail
        // increment belongs to its error-path recovery, so the counter advances
        // here for the iteration this recovery just seeded.
        *turn_iter += 1;
        return Ok(ToolUseTurnOutcome::Continue);
    }
    // This turn produced runnable output, so the model is not
    // stuck: every recovery budget resets. A mixed reply therefore
    // takes no strike, and neither does a productive turn between
    // two failure bursts.
    recovery_budget.note_progress();
    // A mixed reply can lose calls in two ways at once, and both are
    // told to the model rather than silently swallowed just because
    // the turn also did real work. The guidance is accumulated into
    // one user-role message: `pending_user_text` is a single slot
    // consumed by the next iteration.
    let mut mixed_guidance: Vec<&str> = Vec::new();
    if unnamed_call_count > 0 {
        // The named calls run below; the next turn's user text tells
        // the model one of its calls was dropped so it can reissue it
        // with a real name.
        mixed_guidance.push(UNNAMED_TOOL_CALL_RECOVERY_INSTRUCTION);
    }
    if !discarded_tool_calls.is_empty() {
        // A call the provider cut off mid-arguments was dropped while
        // its siblings ran. Same treatment, same no-strike rule: the
        // model is told, and the metric records why it happened.
        crate::metrics::record_tool_call_discarded(
            provider_slug,
            model,
            TRUNCATION_RECOVERY.label,
            finish,
            discarded_tool_calls.len(),
        );
        warn!(
            session_id = ctx.session_id,
            stream_id,
            turn_id = current_turn_id,
            discarded = discarded_tool_calls.len(),
            kept = runnable_calls.len(),
            finish = finish.label(),
            "a mixed reply dropped a truncated tool call; the runnable ones still run",
        );
        mixed_guidance.push(TRUNCATION_MIXED_RECOVERY_INSTRUCTION);
    }
    if !mixed_guidance.is_empty() {
        pending_user_text.replace(mixed_guidance.join("\n\n"));
    }
    // Mint a deterministic id for any call whose provider id was
    // blank, before the turn is recorded. A blank id already breaks
    // the daemon's own bookkeeping — `seed_tool_results` keys
    // placeholders by `call_id`, `executed_tool_calls` is a
    // `HashSet<String>`, and the in-place streaming update matches
    // by id — so two blank ids in one reply would collide on "".
    // The formula is the one the wire repair uses, so a replayed id
    // equals the recorded one. Names and arguments are left
    // untouched here: the dispatcher needs the provider's exact
    // bytes, and repair belongs on the wire.
    for (position, call) in runnable_calls.iter_mut().enumerate() {
        if call.id.trim().is_empty() {
            call.id = crate::wire::mint_call_id(&call.name, &call.arguments_json, position);
        }
    }
    // Build the call records once so the same ordered list seeds
    // both the assistant message's tool_calls and the placeholder
    // tool results (they must agree so the in-place updates below
    // match by call_id).
    let tool_call_records: Vec<AssistantToolCallRecord> = runnable_calls
        .iter()
        .map(|tc| AssistantToolCallRecord {
            call_id: tc.id.clone(),
            name: tc.name.clone(),
            arguments_json: tc.arguments_json.clone(),
            // The provider's caller attribution rides the turn so a
            // rebuilt request can copy it back onto the call and the
            // output that answers it; without it the API cannot
            // resume the program that generated the call.
            caller: tc.caller.as_ref().map(Into::into),
        })
        .collect();
    // Invocation descriptions for the same calls, in the same
    // order: seeding them onto the placeholder results lets every
    // client render the tool's context (e.g. "Running command:
    // `…`.") from the moment the seeded turn is broadcast — before
    // any output streams — instead of waiting for a streaming
    // chunk that may be dropped or for the final record.
    let description_by_call: HashMap<String, String> = runnable_calls
        .iter()
        .map(|tc| {
            let desc = session
                .project_tools
                .describe_invocation_json(&tc.name, &tc.arguments_json)
                .unwrap_or_else(|| tool_registry.describe_invocation(tc));
            (tc.id.clone(), desc)
        })
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
    // Seed one placeholder tool result per call, in the model's
    // call order, so the transcript renders every tool result in
    // that order at all times — each placeholder is filled in
    // place as its tool streams or finalizes.  The seeded
    // placeholder already carries the invocation description so
    // the live header matches the final record's exactly.
    //
    // Seeding runs before the assistant write because it only
    // borrows `tool_call_records`, which that write consumes. The
    // order is otherwise free: the two calls touch disjoint turn
    // fields (`tool_results` versus the assistant text/reasoning/
    // calls/artifact/producer). Every payload the record needs is
    // MOVED into it rather than cloned — this point is only reached
    // with runnable calls, and nothing after it reads the text, the
    // reasoning or the opaque reasoning artifact (a provider
    // round-trip blob that can run to kilobytes), so the record is
    // their last consumer.
    session.seed_tool_results(
        current_turn_id,
        &tool_call_records,
        &invocation_descriptions,
    );
    session.set_assistant_response(
        current_turn_id,
        AssistantResponse {
            text: content,
            reasoning,
            tool_calls: tool_call_records,
            token_usage,
            reasoning_artifact,
            // The producer is one struct two call sites need (the
            // turn's provenance here, the response-id provenance
            // below), so it is the one value that is copied.
            reasoning_producer: Some(producer.clone()),
        },
    );
    broadcast_turn_appended(&ctx.cmd_tx, session, ctx.session_id, current_turn_id);
    // Store response_id for chaining tool results back to this
    // turn, and persist it (+ its producing model) on the session
    // config so ResponseId-policy providers can chain across user
    // turns (restored at the top of the next loop invocation only
    // when the same provider+model is still active). The in-loop
    // value stays unconditional: it chains the tool-result
    // follow-up within THIS request, which a chained provider may
    // want regardless of its cross-turn passback policy (the
    // stateless body builder simply never sends it). The persisted
    // copy is the cross-turn chain, so it follows the passback
    // policy — see the `FinalText` arm's record.
    prev_resp_id.clone_from(&response_id);
    if chains_via_response_id(provider_slug, model) {
        session.config.last_response_id.clone_from(prev_resp_id);
        session.config.last_response_id_producer = Some(producer);
    }

    // Arm the cache warmer for the blocking tool window that
    // follows: the request just sent is the prefix the next real
    // turn will re-read, so a 1-token re-send mid-tool refreshes its
    // provider cache TTL. The arming policy (including the
    // affinity safety gate, which must see the exact dialect this
    // request sends) lives in `cache_warm::arming`, so this module
    // holds no policy knowledge. Only a tool turn arms — it is the
    // turn that then blocks; a `FinalText` result returns first.
    if let Some(warmer) = warmer {
        crate::cache_warm::arm_for_tool_window(
            warmer,
            &crate::cache_warm::ToolWindowWarm {
                provider_slug,
                model,
                // The dialect the request bodies actually send
                // (catalog + account override), so a forced-on hint
                // that was then rejected is detected and a forced-off
                // one is not mistaken for a sent hint.
                affinity: client.cache_affinity(),
                // And the tier the request bodies actually ask for,
                // so a model on the extended-retention selector is
                // scheduled against its real (long) cache lifetime.
                extended_retention: client.extended_cache_retention(model),
                estimated_prompt_tokens,
                messages,
                tools,
                session_id: ctx.session_id,
                stream_id,
                is_anthropic: protocol_is_anthropic(provider_slug),
                thinking_enabled,
            },
        );
    }

    // Partition tool calls into serial and concurrent.
    // Session-config tools (load_tools, unload_tools,
    // set_working_dir) run serially even though they are now
    // registry tools: their mutations are applied by the session
    // main loop via daemon → session command routing, and serial
    // execution preserves the model's call order so e.g. a
    // load_tools followed by a set_working_dir lands in the
    // intended sequence.
    let (mutators, concurrent): (Vec<_>, Vec<_>) = runnable_calls
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
    for tool_call in mutators {
        if is_cancelled_once(cancel_rx) {
            // Relay the cancel so an armed warmer stops pinging
            // promptly (it must not share the loop's receiver).
            relay_warm_cancel();
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

        if let Err(e) = ctx
            .cmd_tx
            .send(SessionCommand::Broadcast(DaemonMessageType::Session {
                session_id: Some(ctx.session_id),
                event: SessionEvent::ToolCallStarted {
                    stream_id,
                    call_id: tool_call.id.clone(),
                    tool_name: tool_call.name.clone(),
                    arguments_json: tool_call.arguments_json.clone(),
                    invocation_description: invocation_description.clone(),
                },
            }))
        {
            warn!(%stream_id, call_id = %tool_call.id, error = %e, "failed to broadcast ToolCallStarted");
        }

        let tool_timeout = determine_tool_timeout(&tool_call.name, &tool_call.arguments_json)
            .unwrap_or(Duration::from_secs(60));

        if ctx
            .cmd_tx
            .send(SessionCommand::StatusChanged(SessionStatus::ToolCall(
                tool_call.name.clone(),
            )))
            .is_err()
        {
            return Ok(ToolUseTurnOutcome::End(false));
        }

        debug!(
            session_id = ctx.session_id,
            turn = *turn_iter,
            tool_name = %tool_call.name,
            tool_call_id = %tool_call.id,
            // Length only: `arguments_json` is the model's tool input
            // and must never be logged (see AGENTS.md -> Logging).
            args_len = tool_call.arguments_json.len(),
            "executing tool (serial)",
        );

        let turn_working_dir = session.config.working_dir.clone();
        // Clone the session's private MCP tool set out before the
        // mutable borrow of `session` in the params literal below.
        let session_tools = Arc::clone(&session.project_tools);
        // TEMPORARY: pass the daemon's Substrate credential through
        // the single `x_credentials` slot so the content write tools
        // can build a ChainAccount. This single-slot reuse is a
        // stopgap (the X tools use the same slot, so only one
        // credential rides it) until a proper tool→keystore
        // credential-access system replaces it.
        let (mut output, tool_cancelled, image) = execute_tool_with_timeout(ExecuteToolParams {
            tool_call: &tool_call,
            x_credentials: ctx.substrate_credential.as_ref(),
            working_dir: turn_working_dir.as_deref(),
            timeout_dur: tool_timeout,
            stream_id,
            session_id: ctx.session_id,
            session: &mut *session,
            session_tools,
            cancel_rx,
            ctx,
            invocation_description: &invocation_description,
        });
        if tool_cancelled {
            // The wait observed a cancellation signal (consumed by
            // its `select!`), so the request must stop after this
            // tool's result is recorded below.
            cancelled = true;
            // Relay the cancel to the warmer too (see the shared-
            // receiver note at the spawn site).
            relay_warm_cancel();
        }

        record_tool_completion(ToolCompletionParams {
            stream_id,
            session: &mut *session,
            tool_call: &tool_call,
            output: &mut output,
            image,
            ctx,
            current_turn_id,
            known_hint_paths: &mut *known_hint_paths,
            pending_hints: &mut *pending_hints,
        });
        executed_tool_calls.insert(tool_call.id.clone());

        // Only mirror mutations that were actually accepted: an
        // error (e.g. inactive session, daemon communication
        // failure) means the authoritative state was NOT changed,
        // so this worker must not pretend it was.
        if !output.is_error
            && let Some(change) =
                pending_config_change(&tool_call, &output, turn_base_working_dir.as_deref())
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
        for tc in &concurrent {
            // Carry the invocation description (computed once,
            // above) on the start event so clients render the
            // tool's context (e.g. "Running command: `…`.") from
            // the broadcast rather than from a streaming chunk —
            // chunks are droppable under load, and this event is
            // queued before the tool even starts.
            let invocation_description =
                description_by_call.get(&tc.id).cloned().unwrap_or_default();
            if let Err(e) = ctx
                .cmd_tx
                .send(SessionCommand::Broadcast(DaemonMessageType::Session {
                    session_id: Some(ctx.session_id),
                    event: SessionEvent::ToolCallStarted {
                        stream_id,
                        call_id: tc.id.clone(),
                        tool_name: tc.name.clone(),
                        arguments_json: tc.arguments_json.clone(),
                        invocation_description,
                    },
                }))
            {
                warn!(%stream_id, call_id = %tc.id, error = %e, "failed to broadcast ToolCallStarted");
            }
        }

        if ctx
            .cmd_tx
            .send(SessionCommand::StatusChanged(SessionStatus::ToolCall(
                concurrent_tool_status_label(&concurrent),
            )))
            .is_err()
        {
            return Ok(ToolUseTurnOutcome::End(false));
        }

        debug!(
            session_id = ctx.session_id,
            turn = *turn_iter,
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
            // Share the session's discovered-skill snapshot with the
            // tool so `load_skill` resolves against the SAME list the
            // system-prompt listing used (one resolution, no
            // divergence between the returned and persisted body).
            discovered_skills: session.discovered_skills.clone().map(std::sync::Arc::new),
        };

        let cmd_tx = ctx.cmd_tx.clone();
        let reg = Arc::clone(tool_registry);
        // The session's private MCP tools, cloned once per batch so
        // each dispatched call consults it before the shared
        // registry.
        let session_tools = Arc::clone(&session.project_tools);

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
        for tool_call in concurrent {
            let timeout = determine_tool_timeout(&tool_call.name, &tool_call.arguments_json);
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
                stream_id,
                session_id: ctx.session_id,
                registry: Arc::clone(&reg),
                session_tools: Arc::clone(&session_tools),
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
        let mut process_tool_handle = |ToolHandle {
                                           tool_call,
                                           mut output,
                                           image,
                                           started_at,
                                       }: ToolHandle| {
            let elapsed = started_at.elapsed();

            debug!(
                session_id = ctx.session_id,
                turn = *turn_iter,
                tool_name = %tool_call.name,
                elapsed_ms = elapsed.as_millis(),
                result_len = output.content.len(),
                is_error = output.is_error,
                "tool finished (concurrent)",
            );

            record_tool_completion(ToolCompletionParams {
                stream_id,
                session: &mut *session,
                tool_call: &tool_call,
                output: &mut output,
                image,
                ctx,
                current_turn_id,
                known_hint_paths: &mut *known_hint_paths,
                pending_hints: &mut *pending_hints,
            });
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
                // Relay the cancel to the warmer too (see the shared-
                // receiver note at the spawn site): it must not share
                // this receiver, so this is where it hears about a
                // cancel that fired during a concurrent tool batch.
                relay_warm_cancel();
                // NOTE: no provider-socket force-close here. The
                // cancel is DECIDED on the daemon command loop
                // (`handle_cancel_request`), which closes THIS
                // session's socket registry directly — a wedged
                // reader cannot observe a channel message, so the
                // close must happen outside the worker. This arm
                // only handles the cooperative part: stopping the
                // tool wait and unwinding the turn.
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
                    if let Ok(handle) = batch_rx.recv() {
                        delivered.insert(handle.tool_call.id.clone());
                        process_tool_handle(handle);
                    } else {
                        warn!(
                            session_id = ctx.session_id,
                            stream_id,
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
                break;
            }
            if let Some(msg) = handle_msg {
                if let Ok(handle) = msg {
                    delivered.insert(handle.tool_call.id.clone());
                    process_tool_handle(handle);
                } else {
                    // Every wait-loop thread has exited but fewer
                    // handles arrived than expected: some thread
                    // panicked before sending. Synthesize the same
                    // "tool thread panicked" output the old
                    // join-based path produced, for the missing
                    // slots only (by call_id), so the turn still
                    // records a result for every call.
                    warn!(
                        session_id = ctx.session_id,
                        stream_id,
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
        apply_pending_config_change(session, change, ctx.tool_registry.load().protected_groups());
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
        // The turn is complete as far as this request will take it,
        // so checkpoint it before unwinding — a cancel is a normal
        // outcome, and the work the tools already did must not be
        // lost if the process dies before `RequestFinished`.
        persist_turn_checkpoint(session, ctx, current_turn_id);
        return Ok(ToolUseTurnOutcome::End(true));
    }

    // The turn is complete: the assistant message is recorded, every
    // call has a result (a real one, or the synthesized "panicked"
    // / "cancelled" placeholder), and the config changes have been
    // mirrored. Checkpoint it now — the loop may go on for many more
    // iterations (unbounded when `max_turns` is 0), and everything
    // below this point is a new provider round-trip whose failure
    // would otherwise discard this turn too.
    persist_turn_checkpoint(session, ctx, current_turn_id);

    // The helper owns this arm's turn counter: the loop's tail increment serves
    // only its error-path recovery, so the counter advances here for the
    // iteration the loop is about to run.
    *turn_iter += 1;
    Ok(ToolUseTurnOutcome::Continue)
}
