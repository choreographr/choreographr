//! `SessionCommand` dispatch and the per-command handler functions.
//!
//! The handlers are grouped by concern into the child modules: `inference`
//! (running a turn and finishing it), `subscriptions` (attach / detach and the
//! live-subscriber plumbing), `config` (session-configuration commands), and
//! `undo_redo` (turn-history mutation). This parent holds the `SessionCommand`
//! dispatch itself, the `broadcast_token_state` / `handle_sync_accumulated_usage`
//! token-refresh path, and the shutdown handler — the pieces that cut across
//! those concerns. Every child reaches the session types and shared helpers
//! through `use super::{…}`, resolving against the re-exports below (the shared
//! imports live here so the children carry no wildcard glob).

pub(in crate::sessions) use super::worker::{RequestWorkerArgs, run_request_worker};
pub(in crate::sessions) use super::{
    ActiveRequest, ChildResult, ClientId, DaemonCommand, DaemonMessage, DaemonMessageType, HashMap,
    InferenceProvider, MAX_TITLE_CHARS, Path, ReplyTarget, RequestContext, SessionCommand,
    SessionEvent, SessionMcpOverlay, SessionMetadata, SessionRecord, SessionSnapshot, SessionState,
    SessionStatus, SessionSummary, SubscriberSink, TimestampMs, TokenUsage, UnicodeSegmentation,
    WarmPolicy, broadcast, debug, fail_request, info, io, model_reasoning_capability, mpsc,
    persist_session_metadata, turn_for_client, warn, write_session_retry, write_turn_retry,
};

mod config;
mod inference;
mod subscriptions;
mod undo_redo;

pub(in crate::sessions) use self::config::{
    handle_get_reasoning_effort, handle_get_state, handle_load_tools, handle_set_account,
    handle_set_mcp_overlay, handle_set_model, handle_set_provider_slug,
    handle_set_reasoning_effort, handle_set_title, handle_set_working_dir, handle_unload_tools,
};
pub(in crate::sessions) use self::inference::{
    handle_cancel, handle_request_finished, handle_run_child_input, handle_run_input,
};
pub(in crate::sessions) use self::subscriptions::{
    handle_attach, handle_broadcast, handle_detach, handle_get_summary, handle_remove_subscriber,
    handle_status_changed,
};
pub(in crate::sessions) use self::undo_redo::{handle_redo, handle_undo};

pub(super) fn process_command(
    cmd: SessionCommand,
    state: &mut SessionState,
    shutdown_requested: &mut bool,
    ctx: &RequestContext,
) -> bool {
    match cmd {
        SessionCommand::RunInput { input, reply } => {
            handle_run_input(&input, reply, state, shutdown_requested, ctx)
        }
        SessionCommand::RunChildInput { user_text, reply } => {
            handle_run_child_input(user_text.as_deref(), reply, state, shutdown_requested, ctx)
        }
        SessionCommand::Cancel { stream_id } => handle_cancel(stream_id, state, ctx),
        SessionCommand::SetModel { model, reply } => handle_set_model(model, reply, state, ctx),
        SessionCommand::StatusChanged(new_status) => handle_status_changed(new_status, state, ctx),
        SessionCommand::Attach { client_id, tx } => handle_attach(client_id, tx, state, ctx),
        SessionCommand::Detach { client_id } => {
            handle_detach(client_id, state, *shutdown_requested, ctx)
        }
        SessionCommand::RemoveSubscriber { client_id } => {
            handle_remove_subscriber(client_id, state, *shutdown_requested, ctx)
        }
        SessionCommand::GetSummary { reply } => handle_get_summary(&reply, state, ctx),
        SessionCommand::RequestFinished {
            stream_id,
            snapshot,
        } => handle_request_finished(stream_id, snapshot, state, *shutdown_requested, ctx),
        SessionCommand::Broadcast(message) => handle_broadcast(&message, state, ctx),
        SessionCommand::SyncAccumulatedUsage {
            token_usage,
            last_prompt_tokens,
        } => handle_sync_accumulated_usage(token_usage, last_prompt_tokens, state, ctx),
        SessionCommand::SetTitle { title, reply } => handle_set_title(&title, reply, state, ctx),
        SessionCommand::SetWorkingDir {
            path,
            tool_reply,
            reply,
        } => handle_set_working_dir(&path, &tool_reply, reply, state, ctx),
        SessionCommand::LoadTools { groups, reply } => {
            handle_load_tools(&groups, &reply, state, ctx)
        }
        SessionCommand::UnloadTools { groups, reply } => {
            handle_unload_tools(&groups, &reply, state, ctx)
        }
        SessionCommand::SetMcpOverlay(overlay) => handle_set_mcp_overlay(*overlay, state, ctx),
        SessionCommand::SetAccount { name, reply } => handle_set_account(name, reply, state, ctx),
        SessionCommand::DropProvider => {
            // The daemon decided the cached client is stale (keystore locked,
            // credential removed/changed, account reconfigured). Drop it so
            // the next request rebuilds against fresh credentials; the
            // session's registry survives so its cancellable scope is stable.
            if state.provider.take().is_some() {
                info!(
                    session = ctx.session_id,
                    "dropped cached provider client; it will be rebuilt on the next request"
                );
            }
            false
        }
        SessionCommand::SetProviderSlug { slug } => handle_set_provider_slug(slug, state, ctx),
        SessionCommand::SetReasoningEffort { effort, reply } => {
            handle_set_reasoning_effort(effort, reply, state, ctx)
        }
        SessionCommand::GetReasoningEffort { reply } => {
            handle_get_reasoning_effort(&reply, state, ctx)
        }
        SessionCommand::GetState { reply } => handle_get_state(&reply, state, ctx),
        SessionCommand::Undo { reply } => handle_undo(reply, state, ctx),
        SessionCommand::Redo { reply } => handle_redo(reply, state, ctx),
        SessionCommand::Shutdown => handle_shutdown(state, shutdown_requested, ctx),
    }
}

// ── SessionCommand handler functions ─────────────────────────────────────────

/// Apply the request worker's mid-turn cumulative token usage to the
/// authoritative session config, then broadcast the update.
///
/// The agent loop accumulates usage on its private worker clone and only
/// merges it back at `RequestFinished`; without this sync the main
/// thread's `accumulated_usage` stays at the pre-request value for the
/// whole turn, leaking stale totals into attach snapshots and session
/// summaries.  `apply_worker_snapshot` at `RequestFinished` still applies
/// the final value, which is >= this one — the two paths are idempotent.
pub(super) fn handle_sync_accumulated_usage(
    token_usage: TokenUsage,
    last_prompt_tokens: Option<u32>,
    state: &mut SessionState,
    ctx: &RequestContext,
) -> bool {
    // Merge, never overwrite: today a single FIFO worker per session makes a
    // blind assignment safe (the accumulated total only grows), but a
    // per-field max keeps the counter monotonic without resting on that
    // invariant — an out-of-order or overlapping sync can never regress a
    // total a client already saw.  [`TokenUsage::merge_max`] implements the
    // same policy the TUI applies to attach snapshots.
    state.config.accumulated_usage.merge_max(token_usage);
    if let Some(tokens) = last_prompt_tokens {
        state.config.last_prompt_tokens = Some(tokens);
    }
    broadcast_token_state(state, ctx);
    false
}

/// Broadcast the session's current token state (cumulative billing total +
/// last-prompt context size) and refresh the daemon's metadata index.
///
/// Shared by the mid-turn `SyncAccumulatedUsage` path and by undo/redo, whose
/// recomputed context size must reach subscribed clients and the session
/// list/detail exactly like a mid-turn update. No `last_modified` bump: this is
/// a data refresh, not a session modification. The broadcast goes through the
/// live subscriber map AFTER the state write so a client can never be ahead of
/// the snapshot it receives on attach.
fn broadcast_token_state(state: &mut SessionState, ctx: &RequestContext) {
    broadcast(
        &mut state.subscribers,
        ctx,
        &DaemonMessageType::Session {
            session_id: Some(ctx.session_id),
            event: SessionEvent::TokenUsageUpdate {
                token_usage: state.config.accumulated_usage,
                last_prompt_tokens: state.config.last_prompt_tokens,
            },
        },
    );
    let _ = ctx.daemon_tx.send(DaemonCommand::UpdateMetadata {
        session_id: ctx.session_id,
        metadata: SessionMetadata::from(&*state),
    });
}

/// Signal shutdown: cancel all active requests and check if the loop should exit.
pub(super) fn handle_shutdown(
    state: &mut SessionState,
    shutdown_requested: &mut bool,
    ctx: &RequestContext,
) -> bool {
    *shutdown_requested = true;
    for (&stream_id, active) in &state.active_requests {
        let _ = active.cancel_tx.send(());
        broadcast(
            &mut state.subscribers,
            ctx,
            &DaemonMessageType::Session {
                session_id: Some(ctx.session_id),
                event: SessionEvent::Cancelled { stream_id },
            },
        );
    }
    state.active_requests.is_empty()
}
