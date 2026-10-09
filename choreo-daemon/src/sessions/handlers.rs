//! `SessionCommand` dispatch and the per-command handler functions.
//!
//! Split out of `sessions.rs` to keep the session-thread entry point and the
//! session state types readable. Everything here reaches the session types and
//! helpers through an explicit `use super::{…}`.

use super::worker::{RequestWorkerArgs, run_request_worker};
use super::{
    ActiveRequest, ChildResult, ClientId, DaemonCommand, DaemonMessage, DaemonMessageType, HashMap,
    InferenceProvider, MAX_TITLE_CHARS, Path, ReplyTarget, RequestContext, SessionCommand,
    SessionEvent, SessionMcpOverlay, SessionMetadata, SessionRecord, SessionSnapshot, SessionState,
    SessionStatus, SessionSummary, SubscriberSink, TimestampMs, TokenUsage, UnicodeSegmentation,
    WarmPolicy, broadcast, debug, fail_request, info, io, model_reasoning_capability, mpsc,
    persist_session_metadata, turn_for_client, warn, write_session_retry, write_turn_retry,
};

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

/// Process a user input: validate, resolve provider, spawn a request worker.
pub(super) fn handle_run_input(
    input: &[u8],
    reply: Option<ReplyTarget>,
    state: &mut SessionState,
    shutdown_requested: &mut bool,
    ctx: &RequestContext,
) -> bool {
    // The daemon assigns the run's stream_id HERE, on the session thread, so a
    // client never chooses one (two clients' runs on one session can no longer
    // collide). Assign before every failure path below so a rejection's
    // `Started`/`Failed` broadcast also carries a distinct, non-sentinel id.
    let stream_id = state.next_stream_id;
    state.next_stream_id = state.next_stream_id.wrapping_add(1);
    debug!("session {}: RunInput id={}", ctx.session_id, stream_id);
    let text = String::from_utf8_lossy(input).trim().to_string();
    info!(
        session_id = ctx.session_id,
        input_len = text.len(),
        input_preview = %text.chars().take(120).collect::<String>(),
        "session received input",
    );
    if text.is_empty() {
        return fail_request(
            &mut state.subscribers,
            ctx,
            ctx.session_id,
            stream_id,
            reply,
            "empty input",
        );
    }
    // Lazy provider resolution: the client is built HERE, on the session
    // thread, against THIS session's socket registry — so a session created
    // while the keystore is locked still works once credentials appear, and
    // every socket it dials is cancellable with the session.
    let provider = match state.resolve_provider(ctx) {
        Ok(p) => p,
        Err(msg) => {
            return fail_request(
                &mut state.subscribers,
                ctx,
                ctx.session_id,
                stream_id,
                reply,
                msg,
            );
        }
    };
    // Re-resolve context window now that a provider is available (e.g. the
    // first request after unlocking the daemon).
    state.resolve_context_window_if_missing(ctx);
    let model = match &state.config.selected_model {
        Some(m) => m.clone(),
        None => {
            return fail_request(
                &mut state.subscribers,
                ctx,
                ctx.session_id,
                stream_id,
                reply,
                "no model selected",
            );
        }
    };
    if *shutdown_requested {
        return fail_request(
            &mut state.subscribers,
            ctx,
            ctx.session_id,
            stream_id,
            reply,
            "session is shutting down",
        );
    }
    if !state.active_requests.is_empty() {
        return fail_request(
            &mut state.subscribers,
            ctx,
            ctx.session_id,
            stream_id,
            reply,
            "session already has an active request",
        );
    }

    let started = DaemonMessageType::Session {
        session_id: Some(ctx.session_id),
        event: SessionEvent::Started {
            stream_id,
            turn_id: state.next_turn_id,
            estimated_prompt_tokens: 0,
        },
    };
    // Acceptance reply (`id: Some`) to the requester, then the unchanged
    // broadcast (`id: None`) to every subscriber. The requester receives BOTH
    // `Started`s (targeted + broadcast); the client's `handle_started` is
    // idempotent for a repeated (stream_id, turn_id), so the duplicate is
    // harmless. Keeping the broadcast is what lets other attached clients
    // follow the stream.
    if let Some(target) = reply {
        target.send(started.clone());
    }
    broadcast(&mut state.subscribers, ctx, &started);
    let (cancel_tx, cancel_rx) = crossbeam_channel::unbounded::<()>();
    state.active_requests.insert(
        stream_id,
        ActiveRequest {
            cancel_tx,
            turn_id: state.next_turn_id,
        },
    );

    // Workers don't need their own subscriber map — all broadcasts
    // are routed through SessionCommand::Broadcast to this main
    // session thread which holds the live subscriber set.
    let mut worker_session = SessionState::from_snapshot(state.snapshot(), HashMap::new());
    let ctx = ctx.clone();
    let user_text = Some(text);
    std::thread::spawn(move || {
        run_request_worker(RequestWorkerArgs {
            stream_id,
            client: &provider,
            session: &mut worker_session,
            model: &model,
            cancel_rx: &cancel_rx,
            ctx: &ctx,
            child_reply: None,
            user_text: user_text.as_deref(),
        });
    });
    false
}

/// Run the agent loop on a pre-populated child session and return the result.
///
/// The caller is responsible for injecting any prompt into the session — this
/// command only triggers the agent loop on whatever turns are already
/// queued. The response is delivered through the `reply` channel.
pub(super) fn handle_run_child_input(
    // Borrowed only: the text is cloned when injected into the worker's
    // user turn; the command variant still owns it.
    user_text: Option<&str>,
    reply: std::sync::mpsc::Sender<io::Result<ChildResult>>,
    state: &mut SessionState,
    shutdown_requested: &mut bool,
    ctx: &RequestContext,
) -> bool {
    // Child runs get a daemon-assigned stream id too, so a subscriber to the
    // child session (e.g. a viewer following it) can route its events and a
    // parent's cancel propagation stays distinct.
    let stream_id = state.next_stream_id;
    state.next_stream_id = state.next_stream_id.wrapping_add(1);
    // Lazy resolution (same path as RunInput): a session thread that has
    // never run a request may still be provider-less (keystore was locked at
    // attach). The old wording ("daemon locked") is preserved for failures.
    let Ok(provider) = state.resolve_provider(ctx) else {
        let _ = reply.send(Err(io::Error::other("daemon locked")));
        return false;
    };
    let model = state.config.selected_model.clone().unwrap_or_default();
    // Own the text before crossing the thread boundary — the borrowed
    // `user_text` parameter cannot escape into the spawned worker.
    let user_text_owned = user_text.map(str::to_owned);
    if *shutdown_requested {
        let _ = reply.send(Err(io::Error::other("session is shutting down")));
        return false;
    }
    if !state.active_requests.is_empty() {
        let _ = reply.send(Err(io::Error::other(
            "session already has an active request",
        )));
        return false;
    }
    broadcast(
        &mut state.subscribers,
        ctx,
        &DaemonMessageType::Session {
            session_id: Some(ctx.session_id),
            event: SessionEvent::Started {
                stream_id,
                turn_id: state.next_turn_id,
                estimated_prompt_tokens: 0,
            },
        },
    );
    let (cancel_tx, cancel_rx) = crossbeam_channel::unbounded::<()>();
    state.active_requests.insert(
        stream_id,
        ActiveRequest {
            cancel_tx,
            turn_id: state.next_turn_id,
        },
    );
    let mut worker_session = SessionState::from_snapshot(state.snapshot(), HashMap::new());
    let ctx = ctx.clone();
    let provider = provider.clone();
    std::thread::spawn(move || {
        run_request_worker(RequestWorkerArgs {
            stream_id,
            client: &provider,
            session: &mut worker_session,
            model: &model,
            cancel_rx: &cancel_rx,
            ctx: &ctx,
            child_reply: Some(&reply),
            user_text: user_text_owned.as_deref(),
        });
    });
    false
}

/// Cancel an active request by sending on its cancel channel.
/// Child-session propagation is handled by the daemon when it processes
/// `DaemonCommand::CancelRequest`, so this function does not send any
/// additional messages back to the daemon.
/// Cancel one or all active requests.
///
/// When `stream_id` is `0` (the `CANCEL_ALL` sentinel), every active
/// request is cancelled — this is used by child-session cancellation
/// where the parent doesn't know the child's specific request ID.
/// Otherwise only the matching request is cancelled.
pub(super) fn handle_cancel(
    stream_id: u64,
    state: &mut SessionState,
    ctx: &RequestContext,
) -> bool {
    let targets: Vec<u64> = if stream_id == 0 {
        state.active_requests.keys().copied().collect()
    } else {
        vec![stream_id]
    };
    for rid in targets {
        if let Some(active) = state.active_requests.get(&rid) {
            let _ = active.cancel_tx.send(());
            broadcast(
                &mut state.subscribers,
                ctx,
                &DaemonMessageType::Session {
                    session_id: Some(ctx.session_id),
                    event: SessionEvent::Cancelled { stream_id: rid },
                },
            );
        }
    }
    false
}

/// Set the model for this session and broadcast the change.
/// Rejects invalid model names by broadcasting `ModelSelectionFailed`
/// instead of mutating state.
pub(super) fn handle_set_model(
    model: String,
    reply: Option<ReplyTarget>,
    state: &mut SessionState,
    ctx: &RequestContext,
) -> bool {
    info!("session {}: SetModel model={}", ctx.session_id, model);

    // Validate the model against the provider's model list before accepting it.
    if let Err(msg) = validate_model_via_daemon(&model, ctx) {
        warn!(
            "session {}: model '{model}' rejected: {msg}",
            ctx.session_id
        );
        // Targeted terminal failure to the requester, plus the unchanged
        // broadcast (both fire; `id` distinguishes the reply from the fan-out).
        if let Some(target) = reply {
            target.fail(msg.clone());
        }
        broadcast(
            &mut state.subscribers,
            ctx,
            &DaemonMessageType::Session {
                session_id: Some(ctx.session_id),
                event: SessionEvent::ModelSelectionFailed { model, error: msg },
            },
        );
        return false;
    }

    state.config.selected_model = Some(model.clone());
    // Static catalog facts keyed by the recorded slug — no credential-bound
    // client needed, so the context window and ModelSelected capability are
    // exact even before the keystore unlocks.
    let cw = state.resolve_context_window_for_model(&model);
    debug!(
        "session {}: resolved context_window={:?} for model={}",
        ctx.session_id, cw, model
    );
    state.config.context_window = cw;
    if let Some(cw) = cw {
        broadcast(
            &mut state.subscribers,
            ctx,
            &DaemonMessageType::Session {
                session_id: Some(ctx.session_id),
                event: SessionEvent::ContextWindowResolved { context_window: cw },
            },
        );
    }
    let capability = state
        .effective_provider_slug()
        .map(|slug| model_reasoning_capability(slug, &model));

    // Re-validate the current reasoning effort against the new model's
    // capability.  Slugs that were valid on the old model may not be
    // supported by the new one — silently reset to "off" when that happens.
    if let Some(ref cap) = capability
        && let Some(ref effort) = state.config.reasoning_effort
        && effort != "off"
        && !cap.available_effort_levels.iter().any(|l| l == effort)
    {
        warn!(
            session_id = ctx.session_id,
            old_effort = %effort,
            "reasoning effort not supported by new model, resetting to 'off'",
        );
        state.config.reasoning_effort = Some("off".to_string());
        broadcast(
            &mut state.subscribers,
            ctx,
            &DaemonMessageType::Session {
                session_id: Some(ctx.session_id),
                event: SessionEvent::ReasoningEffortSet {
                    effort: "off".to_string(),
                },
            },
        );
    }

    debug!(
        "session {}: broadcasting ModelSelected model={}",
        ctx.session_id, model
    );
    broadcast(
        &mut state.subscribers,
        ctx,
        &DaemonMessageType::Session {
            session_id: Some(ctx.session_id),
            event: SessionEvent::ModelSelected {
                model: model.clone(),
                reasoning_capability: capability,
            },
        },
    );
    persist_session_metadata(state, ctx, "SetModel");
    // Terminal success acknowledgement to the requester (`Accepted { SetModel }`),
    // in addition to the `ModelSelected` broadcast above.
    if let Some(target) = reply {
        target.accept();
    }
    false
}

/// Ask the daemon whether `model` is valid for this session's account.
/// Returns `Ok(())` if valid, `Err(reason)` if invalid.
/// If the daemon is unreachable or the model list is unavailable the
/// model is allowed through (`Ok(())`).
pub(super) fn validate_model_via_daemon(model: &str, ctx: &RequestContext) -> Result<(), String> {
    let (reply, rx) = mpsc::channel();
    if ctx
        .daemon_tx
        .send(DaemonCommand::ValidateModel {
            session_id: ctx.session_id,
            model: model.to_string(),
            reply,
        })
        .is_err()
    {
        warn!(
            "session {}: daemon disconnected during model validation for '{model}'",
            ctx.session_id
        );
        return Ok(());
    }

    match rx.recv() {
        Ok(Ok(())) => Ok(()),
        Ok(Err(msg)) => Err(msg),
        Err(_) => {
            warn!(
                "session {}: daemon disconnected while waiting for model validation \
                 of '{model}', allowing through",
                ctx.session_id
            );
            Ok(())
        }
    }
}

/// Update the session status and broadcast to subscribers and daemon.
///
/// Status transitions (Inference, `ToolCall`, Retrying) are internal pipeline
/// churn, NOT user-visible modifications — the status is refreshed everywhere
/// but `last_modified` is deliberately left untouched so the sessions list
/// does not re-sort on every tool call mid-request.  Only completed requests
/// (`handle_request_finished`) and explicit metadata edits
/// (`persist_session_metadata`) bump the timestamp.  The message carries the
/// session's *current* `last_modified`, so clients' monotonic `max()` guards
/// keep the value stable.
pub(super) fn handle_status_changed(
    new_status: SessionStatus,
    state: &mut SessionState,
    ctx: &RequestContext,
) -> bool {
    state.config.status = new_status.clone();
    let last_modified = state.config.last_modified;
    broadcast(
        &mut state.subscribers,
        ctx,
        &DaemonMessageType::Session {
            session_id: Some(ctx.session_id),
            event: SessionEvent::SessionStatusChanged {
                status: new_status.clone(),
                last_modified,
            },
        },
    );
    let _ = ctx.daemon_tx.send(DaemonCommand::BroadcastSessionStatus {
        session_id: ctx.session_id,
        status: new_status,
    });
    false
}

/// Attach a client to this session, sending the full session state snapshot.
///
/// If the session has active requests when the client attaches (i.e. the new
/// client is joining mid-stream), synthetic `Started` messages are sent first
/// so the client can populate its `stream_id → turn_id` mapping and route
/// subsequent streaming chunks (`OutputChunk`, `ToolResultChunk`, etc.) to
/// the correct turn — without this, those chunks would be silently dropped.
pub(super) fn handle_attach(
    client_id: ClientId,
    tx: SubscriberSink,
    state: &mut SessionState,
    ctx: &RequestContext,
) -> bool {
    info!("session {}: client {} attached", ctx.session_id, client_id);
    state.subscribers.insert(client_id, tx);

    // Notify the daemon so it can filter duplicate delivery through the
    // activity subscriber path for this client/session pair.
    let _ = ctx.daemon_tx.send(DaemonCommand::TrackSessionSubscription {
        client_id,
        session_id: ctx.session_id,
    });

    // Send synthetic Started messages for every active request so the
    // new subscriber can route in-flight streaming chunks to the correct
    // turn.  The subscriber will then populate its request_to_turn map
    // and begin accumulating streaming content from this point forward.
    //
    // These are sent directly to the joining client only — the message
    // is not forwarded through broadcast_activity because the client is
    // already a subscriber of this session via the per-session path, and
    // the broadcast_activity filter in the daemon won't see this message
    // anyway (it's sent directly, not through session's broadcast()).
    //
    // Both these and the snapshot below go through `send_unchecked`:
    // with lossless unbounded channels they are GUARANTEED to arrive, but
    // a freshly-attached client's one-shot snapshot must not trip the lag
    // cap (a large snapshot is not evidence of a lagging client), and the
    // byte accounting still keeps the writer thread's per-dequeue
    // decrement balanced.
    if !state.active_requests.is_empty()
        && let Some(tx) = state.subscribers.get(&client_id)
    {
        for (&stream_id, active) in &state.active_requests {
            tx.send_unchecked(
                &DaemonMessage::broadcast(DaemonMessageType::Session {
                    session_id: Some(ctx.session_id),
                    event: SessionEvent::Started {
                        stream_id,
                        turn_id: active.turn_id,
                        estimated_prompt_tokens: 0,
                    },
                }),
                &ctx.global_lag,
            );
        }
    }

    // The snapshot is the new client's only complete view of accumulated
    // content, so lossless delivery matters more here than anywhere else —
    // and the unbounded channel makes it guaranteed (the old code could
    // drop it on a full 128-slot buffer).
    let snapshot = state.session_state_message(ctx.session_id);
    if let Some(tx) = state.subscribers.get(&client_id) {
        // Unsolicited push of the attach snapshot to this one client; it rides
        // the wire as a broadcast (`id: None`) since it answers no request.
        tx.send_unchecked(&DaemonMessage::broadcast(snapshot), &ctx.global_lag);
    }
    false
}

/// Detach a client from this session.
pub(super) fn handle_detach(
    client_id: ClientId,
    state: &mut SessionState,
    shutdown_requested: bool,
    ctx: &RequestContext,
) -> bool {
    info!("session {}: client {} detached", ctx.session_id, client_id);
    state.subscribers.remove(&client_id);

    // Notify the daemon so it can stop filtering duplicates through
    // the activity subscriber path for this client/session pair.
    let _ = ctx
        .daemon_tx
        .send(DaemonCommand::UntrackSessionSubscription {
            client_id,
            session_id: ctx.session_id,
        });
    state.active_requests.is_empty() && (state.subscribers.is_empty() || shutdown_requested)
}

/// Remove a subscriber at the daemon's request (client evicted for lag or
/// fully disconnected). Mirrors [`handle_detach`] but does NOT send
/// `UntrackSessionSubscription` — the daemon already removed the client from
/// its own tracking (`ClientState::sessions`) when it initiated the
/// eviction/cleanup, and sending the untrack here would race the daemon's own
/// removal. The exit predicate is the same as detach: a session with no
/// subscribers and no active requests (and not mid-shutdown) can exit.
pub(super) fn handle_remove_subscriber(
    client_id: ClientId,
    state: &mut SessionState,
    shutdown_requested: bool,
    ctx: &RequestContext,
) -> bool {
    debug!(
        "session {}: removing subscriber {}",
        ctx.session_id, client_id
    );
    state.subscribers.remove(&client_id);
    state.active_requests.is_empty() && (state.subscribers.is_empty() || shutdown_requested)
}

/// Return a `SessionSummary` for this session via the reply channel.
pub(super) fn handle_get_summary(
    reply: &std::sync::mpsc::Sender<SessionSummary>,
    state: &SessionState,
    ctx: &RequestContext,
) -> bool {
    let _ = reply.send(SessionSummary {
        session_id: ctx.session_id,
        title: state.config.title.clone(),
        selected_model: state.config.selected_model.clone(),
        reasoning_effort: state.config.reasoning_effort.clone(),
        parent_session_id: state.config.parent_session_id,
        working_dir: state
            .config
            .working_dir
            .as_ref()
            .map(|p| p.display().to_string()),
        created_at: state.config.created_at,
        last_modified: state.config.last_modified,
        // usize→u32 turn count: a session with 4 billion turns is impossible
        // in practice (each turn is a full provider round-trip).
        #[expect(clippy::cast_possible_truncation)]
        turn_count: state.turns.len() as u32,
        status: state.config.status.clone(),
        active_tool_groups: state.config.active_tool_groups.iter().cloned().collect(),
        account_name: state.config.account_name.clone(),
        token_usage: Some(state.config.accumulated_usage),
        context_window: state.config.context_window,
        last_prompt_tokens: state.config.last_prompt_tokens,
        // The session thread's `SessionConfig` does not carry the
        // daemon-owned flags; this summary is only used by tests (the daemon's
        // own `GetSession`/`ListSessions` reply from `SessionMetadata`), so the
        // defaults are fine here.
        pinned: false,
        archived_at: None,
    });
    false
}

/// Apply the worker's snapshot (config only) and merge turn state.
pub(super) fn handle_request_finished(
    stream_id: u64,
    mut snapshot: SessionSnapshot,
    state: &mut SessionState,
    shutdown_requested: bool,
    ctx: &RequestContext,
) -> bool {
    // An undo processed while the request worker was in flight leaves the
    // worker's snapshot stale in two ways: its turns carry no `undone` flags
    // (the child session never saw the undo), and its `last_response_id`
    // points at a response whose conversation includes the very turns being
    // undone. Detect that race by comparing undone-ness: any turn the live
    // state marks undone but the snapshot does not means the undo landed
    // after the request started. Preserve the undo by dropping the stale
    // response id before the snapshot's config is applied below (the undo
    // already persisted the cleared record), and by refusing to overwrite
    // those turns with the worker's pre-undo copies in the merge loop.
    let undo_during_request = snapshot.turns.iter().any(|(&turn_id, snap_turn)| {
        state
            .turns
            .get(&turn_id)
            .is_some_and(|state_turn| state_turn.undone && !snap_turn.undone)
    });
    if undo_during_request {
        debug!(
            session_id = ctx.session_id,
            stream_id,
            "undo landed while request was in flight; dropping stale response-id chain from worker snapshot",
        );
        snapshot.config.last_response_id = None;
        snapshot.config.last_response_id_producer = None;
    }
    // Apply config changes from the worker snapshot using the allowlist
    // on `SessionConfig` so that fields mutated mid-request through direct
    // SessionCommand calls (SetTitle, SetAccount, SetReasoningEffort) are
    // preserved without needing an explicit save/restore list.
    state.config.apply_worker_snapshot(&snapshot.config);

    // A completed request is a modification: bump the timestamp exactly once,
    // BEFORE persisting the record and refreshing the daemon index, so the
    // on-disk record, the daemon index, and the broadcast all agree on the
    // same value.  `.max()` keeps the bump monotonic — a future-dated value
    // (clock skew) can never regress.
    let last_modified = TimestampMs::now().as_millis();
    state.config.last_modified = state.config.last_modified.max(last_modified);

    // Persist the updated session config (accumulated usage, context_window, etc.)
    // so resolved values survive daemon restarts.
    let record = SessionRecord::from(&*state);
    if let Err(e) = write_session_retry(&ctx.db, ctx.session_id, &record) {
        warn!(error = %e, "failed to persist session config after request");
    }

    // Merge runtime state from the worker snapshot so that loaded skills,
    // context cache, and discovered skills survive across requests.
    state.loaded_skill_bodies = snapshot.loaded_skill_bodies;
    state.context_cache = snapshot.context_cache;
    state.discovered_skills = snapshot.discovered_skills;

    // Merge turns from the worker snapshot into the main session state.
    for (&turn_id, turn) in &snapshot.turns {
        let is_new = !state.turns.contains_key(&turn_id);
        // Preserve an undo that landed mid-request: the worker's copy of a
        // turn the user hid carries no `undone` flag, so overwriting would
        // resurrect the hidden turn — and re-persisting it would resurrect
        // it on disk too. Keep the undone state instead (its content is
        // skipped by the message builder anyway).
        if !is_new
            && let Some(state_turn) = state.turns.get(&turn_id)
            && state_turn.undone
            && !turn.undone
        {
            continue;
        }
        state.turns.insert(turn_id, turn.clone());
        if is_new {
            // Persist the newly created turn. The turn was already broadcast
            // during the agent loop, so no need to re-broadcast here.
            if let Err(e) = write_turn_retry(&ctx.db, ctx.session_id, turn_id, turn) {
                tracing::warn!(turn_id, error = %e, "failed to persist turn");
            }
        } else if state.turns.get(&turn_id).is_some_and(|t| t != turn) {
            // Turn was updated — persist the latest state.
            if let Err(e) = write_turn_retry(&ctx.db, ctx.session_id, turn_id, turn) {
                tracing::warn!(turn_id, error = %e, "failed to persist updated turn");
            }
        }
    }
    // Advance next_turn_id past any turns from the snapshot.
    if let Some(max_id) = snapshot.turns.keys().max() {
        state.next_turn_id = state.next_turn_id.max(max_id + 1);
    }

    state.active_requests.remove(&stream_id);
    state.config.status = SessionStatus::Inactive;
    let _ = ctx.daemon_tx.send(DaemonCommand::UpdateMetadata {
        session_id: ctx.session_id,
        metadata: SessionMetadata::from(&*state),
    });
    broadcast(
        &mut state.subscribers,
        ctx,
        &DaemonMessageType::Session {
            session_id: Some(ctx.session_id),
            event: SessionEvent::SessionStatusChanged {
                status: SessionStatus::Inactive,
                last_modified,
            },
        },
    );
    let _ = ctx.daemon_tx.send(DaemonCommand::BroadcastSessionStatus {
        session_id: ctx.session_id,
        status: SessionStatus::Inactive,
    });
    state.active_requests.is_empty() && (state.subscribers.is_empty() || shutdown_requested)
}

/// Broadcast a message through the live subscriber map.
pub(super) fn handle_broadcast(
    // Borrowed only: the payload is forwarded by reference into
    // `broadcast` (which wraps it as a broadcast for the daemon-level
    // activity fan-out and the per-session fan-out); the command variant
    // still owns the payload.
    message: &DaemonMessageType,
    state: &mut SessionState,
    ctx: &RequestContext,
) -> bool {
    // Broadcast through the main session thread's live subscriber
    // map so that in-flight worker broadcasts respect detach.
    broadcast(&mut state.subscribers, ctx, message);
    false
}

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
    // Broadcast through the live subscriber map AFTER the state write so
    // a client can never be ahead of the snapshot it receives on attach.
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
    // Refresh the daemon's session-metadata index (no last_modified bump:
    // this is a data refresh, not a modification) so session-list / detail
    // token totals are accurate mid-turn on the next ListSessions.
    let _ = ctx.daemon_tx.send(DaemonCommand::UpdateMetadata {
        session_id: ctx.session_id,
        metadata: SessionMetadata::from(&*state),
    });
    false
}

/// Set the session title, broadcasting the change to subscribers and
/// notifying the daemon so session listings reflect the new title
/// immediately.
pub(super) fn handle_set_title(
    title: &str,
    reply: Option<ReplyTarget>,
    state: &mut SessionState,
    ctx: &RequestContext,
) -> bool {
    // Defense-in-depth: cap title length by grapheme clusters so
    // multi-byte scripts and composed emoji are treated as single
    // user-perceived characters.  The tool-level validation in
    // set_session_title.rs catches this first; the session handler
    // is the second line of defence against any code path that sends
    // SetTitle directly (e.g. future internal commands).
    if title.graphemes(true).count() > MAX_TITLE_CHARS {
        warn!(
            session_id = ctx.session_id,
            length = title.graphemes(true).count(),
            max = MAX_TITLE_CHARS,
            "rejecting SetTitle: title too long (defense-in-depth)",
        );
        // A client-originated title request (none today — the tool drives
        // this) would learn of the rejection here.
        if let Some(target) = reply {
            target.fail("title too long");
        }
        return false;
    }

    info!(
        session_id = ctx.session_id,
        old_title = ?state.config.title,
        new_title = %title,
        "session title changed",
    );
    state.config.title = Some(title.to_string());

    // Broadcast to session subscribers (e.g. TUI) so they reflect the
    // new title immediately, without waiting for the next persist cycle.
    broadcast(
        &mut state.subscribers,
        ctx,
        &DaemonMessageType::Session {
            session_id: Some(ctx.session_id),
            event: SessionEvent::SessionTitleSet {
                title: title.to_owned(),
            },
        },
    );

    persist_session_metadata(state, ctx, "SetTitle");

    if let Some(target) = reply {
        target.accept();
    }
    false
}

/// Apply a daemon-pushed MCP overlay: store the session's private project tool
/// set and the daemon-tier groups it shadows. Never persisted — the overlay is
/// recomputed by the daemon on every working-directory change.
pub(super) fn handle_set_mcp_overlay(
    overlay: SessionMcpOverlay,
    state: &mut SessionState,
    ctx: &RequestContext,
) -> bool {
    debug!(
        session_id = ctx.session_id,
        tools = overlay.tools.len(),
        shadowed = overlay.shadowed_groups.len(),
        project_root = ?overlay.project_root,
        "applied MCP overlay"
    );
    state.project_tools = overlay.tools;
    state.project_shadowed_groups = overlay.shadowed_groups;
    false
}

/// Set the session working directory, broadcasting the change to
/// subscribers and notifying the daemon so session listings reflect it
/// immediately.
///
/// This runs in the session's main loop, where the authoritative
/// `SessionConfig` lives — so the change survives the request and is picked
/// up by the next turn's snapshot.  (The pre-refactor implementation
/// mutated the request worker's throwaway copy, which was discarded at
/// request end, silently reverting the change.)  Replies with the canonical
/// path that was applied so the calling tool knows the round-trip succeeded.
pub(super) fn handle_set_working_dir(
    // Borrowed only: the path is cloned into `state.config.working_dir` and
    // stringified for the reply below; the command variant still owns it.
    path: &Path,
    tool_reply: &mpsc::Sender<Result<String, String>>,
    reply: Option<ReplyTarget>,
    state: &mut SessionState,
    ctx: &RequestContext,
) -> bool {
    info!(
        session_id = ctx.session_id,
        old_path = ?state.config.working_dir,
        new_path = %path.display(),
        "session working directory changed",
    );
    state.config.working_dir = Some(path.to_path_buf());
    // Skills are discovered relative to the working directory — invalidate
    // the cache so they are re-discovered from the new location on the next
    // agent-loop turn.  (The system-prompt context cache is fingerprint-keyed
    // and self-invalidates when the working directory changes.)
    state.discovered_skills = None;

    // Broadcast to session subscribers (e.g. TUI) so they reflect the new
    // path immediately, without waiting for the next persist cycle.
    broadcast(
        &mut state.subscribers,
        ctx,
        &DaemonMessageType::Session {
            session_id: Some(ctx.session_id),
            event: SessionEvent::SessionWorkingDirSet {
                path: Some(path.to_string_lossy().into_owned()),
            },
        },
    );

    persist_session_metadata(state, ctx, "SetWorkingDir");

    // Ask the daemon to re-resolve this session's MCP overlay for the new
    // working directory. `persist_session_metadata` queued the UpdateMetadata
    // that records the new directory FIRST, so the command loop sees the fresh
    // directory when it handles this. `cancel_inflight` is left false: the
    // command loop decides whether the change actually left the project (it
    // compares the previous and new roots), so a same-project change neither
    // cancels in-flight calls nor reconnects the project's servers.
    let _ = ctx.daemon_tx.send(DaemonCommand::McpEnsureSession {
        session_id: ctx.session_id,
        cancel_inflight: false,
    });

    let _ = tool_reply.send(Ok(path.to_string_lossy().into_owned()));

    if let Some(target) = reply {
        target.accept();
    }
    false
}

/// Activate tool groups on the authoritative session state, broadcast the
/// updated group set, persist, and reply to the calling tool with a summary
/// of what changed.
pub(super) fn handle_load_tools(
    groups: &[String],
    reply: &mpsc::Sender<Result<String, String>>,
    state: &mut SessionState,
    ctx: &RequestContext,
) -> bool {
    info!(session_id = ctx.session_id, groups = ?groups, "session load_tools");

    // Defense-in-depth: the tool validates group names against the live
    // registry before sending, so unknown names normally never reach the
    // handler.  Re-validate here so a directly-sent command can never
    // persist a typo'd group into the authoritative active set.
    let known = ctx.tool_registry.load().known_group_names();
    if let Some(unknown) = crate::tools::unknown_group_names(groups, &known) {
        let _ = reply.send(Err(format!(
            "Unknown tool group(s): {}",
            unknown.join(", ")
        )));
        return false;
    }

    let result =
        crate::tools::load_tools::apply_load_tools(&mut state.config.active_tool_groups, groups);

    // Broadcast updated session state so the client (e.g. TUI status bar)
    // picks up the new active_tool_groups immediately.
    let session_state = state.session_state_message(ctx.session_id);
    broadcast(&mut state.subscribers, ctx, &session_state);
    persist_session_metadata(state, ctx, "LoadTools");
    let _ = reply.send(Ok(result));

    false
}

/// Deactivate tool groups on the authoritative session state, broadcast the
/// updated group set, persist, and reply to the calling tool with a summary
/// of what changed.
pub(super) fn handle_unload_tools(
    groups: &[String],
    reply: &mpsc::Sender<Result<String, String>>,
    state: &mut SessionState,
    ctx: &RequestContext,
) -> bool {
    info!(session_id = ctx.session_id, groups = ?groups, "session unload_tools");

    // Defense-in-depth: reject unknown group names (same rationale as
    // handle_load_tools).  "core" is known and handled below as protected.
    let known = ctx.tool_registry.load().known_group_names();
    if let Some(unknown) = crate::tools::unknown_group_names(groups, &known) {
        let _ = reply.send(Err(format!(
            "Unknown tool group(s): {}",
            unknown.join(", ")
        )));
        return false;
    }

    // The protected set lives in the live registry ("core" always; "ios"
    // once register_platform_tools ran) — one source of truth shared with
    // the unload_tools tool and the request worker's mirror.
    let result = crate::tools::unload_tools::apply_unload_tools(
        &mut state.config.active_tool_groups,
        groups,
        ctx.tool_registry.load().protected_groups(),
    );

    // Broadcast updated session state so the client picks up the new
    // active_tool_groups immediately.
    let session_state = state.session_state_message(ctx.session_id);
    broadcast(&mut state.subscribers, ctx, &session_state);
    persist_session_metadata(state, ctx, "UnloadTools");
    let _ = reply.send(Ok(result));

    false
}

/// Apply a provider-slug update pushed by the daemon's accounts-reload path.
///
/// The slug is a NON-SECRET catalog fact; refreshing it here keeps slug-keyed
/// static catalog lookups (context window, reasoning capability) exact right
/// after an external account edit, instead of going stale until the next
/// request rebuilds the client. `None` (the account was removed) clears it.
pub(super) fn handle_set_provider_slug(
    slug: Option<String>,
    state: &mut SessionState,
    ctx: &RequestContext,
) -> bool {
    debug!(
        session_id = ctx.session_id,
        old = ?state.provider_slug,
        new = ?slug,
        "recorded provider slug updated from account reload"
    );
    state.provider_slug = slug;
    false
}

/// Set the account for this session and try to resolve its provider.
pub(super) fn handle_set_account(
    name: String,
    reply: Option<ReplyTarget>,
    state: &mut SessionState,
    ctx: &RequestContext,
) -> bool {
    info!("session {}: SetAccount account={}", ctx.session_id, name);
    // Switching accounts must never leave the PREVIOUS account's client in
    // place: `resolve_provider` returns any cached client unconditionally, so a
    // stale client would keep serving requests against the old provider (wrong
    // endpoint, wrong key) under the newly-set account name. Drop it now and
    // rebuild below on success, or lazily on the next request on failure
    // (locked / no credential). Re-setting the SAME account keeps the warm
    // client — no needless churn.
    let switching = state.config.account_name.as_deref() != Some(name.as_str());
    if switching {
        state.provider = None;
    }

    // Try to resolve the account config + API key from the daemon; the client
    // itself is built HERE against this session's registry so its sockets are
    // session-cancellable. If resolution fails (locked, no credential yet) the
    // provider stays None — the account name is still recorded and the next
    // request retries lazily (see `SessionState::resolve_provider`).
    let (account_reply, rx) = crossbeam_channel::unbounded();
    let _ = ctx.daemon_tx.send(DaemonCommand::ResolveAccountCmd {
        account: name.clone(),
        reply: account_reply,
    });
    let resolved = rx.recv().ok().flatten();
    // The provider slug and warm policy are NON-SECRET facts the daemon
    // resolved for this account: record them from the account config even when
    // the keystore is locked (config present, key absent) or the client can't be
    // built. A `None` reply (unknown account) clears them so stale facts can't
    // masquerade for the new account.
    state.provider_slug = resolved.as_ref().map(|a| a.config.provider.clone());
    state.warm_policy = resolved
        .as_ref()
        .map_or_else(WarmPolicy::default, |a| a.warm_policy);
    // Build the client when a credential is present. Held in a local so the
    // immutable borrow of `state.registry` is released before the assignment
    // below (a let-chain condition would otherwise keep it live into the body).
    let new_provider = resolved.as_ref().and_then(|a| {
        let key = a.api_key.as_ref()?;
        InferenceProvider::from_account_config(
            &a.config,
            // See resolve_provider: the Zeroizing wrapper protects the
            // in-transit key; the client constructor takes ownership from here
            // and stores the credential in its own config.
            Some((**key).clone()),
            &state.registry,
        )
        .ok()
    });
    if let Some(provider) = new_provider {
        state.provider = Some(provider);
    }
    // Re-resolve the context window after an account switch: a pure catalog
    // read keyed by the recorded slug, so it works even when no client could
    // be built (locked keystore) — the client-config override still wins when a
    // client exists (see `resolve_context_window_for_model`).
    if switching && let Some(model) = &state.config.selected_model {
        let cw = state.resolve_context_window_for_model(model);
        debug!(
            "session {}: re-resolved context_window={:?} after account change for model={}",
            ctx.session_id, cw, model
        );
        state.config.context_window = cw;
        if let Some(cw) = cw {
            broadcast(
                &mut state.subscribers,
                ctx,
                &DaemonMessageType::Session {
                    session_id: Some(ctx.session_id),
                    event: SessionEvent::ContextWindowResolved { context_window: cw },
                },
            );
        }
    }
    // Always store the account name on the session, even if the
    // provider wasn't resolvable yet (e.g. no credential stored,
    // or daemon hasn't unlocked).  The provider can be resolved
    // lazily when RunInput is called.  This way the user can set
    // an account on a session before unlocking.
    state.config.account_name = Some(name.clone());
    broadcast(
        &mut state.subscribers,
        ctx,
        &DaemonMessageType::Session {
            session_id: Some(ctx.session_id),
            event: SessionEvent::SessionAccountSet { account: name },
        },
    );
    persist_session_metadata(state, ctx, "SetAccount");
    // The account's existence was verified on the connection thread before this
    // command was sent, so reaching here is success: ack the requester in
    // addition to the `SessionAccountSet` broadcast above.
    if let Some(target) = reply {
        target.accept();
    }
    false
}

/// Set the reasoning effort for this session, validating against the model.
pub(super) fn handle_set_reasoning_effort(
    effort: String,
    reply: Option<ReplyTarget>,
    state: &mut SessionState,
    ctx: &RequestContext,
) -> bool {
    // Reject overly long slugs early (defense in depth).
    if effort.len() > 64 {
        let msg = format!("reasoning effort slug too long ({} bytes)", effort.len());
        warn!(session_id = ctx.session_id, error = %msg, "reasoning effort rejected");
        if let Some(target) = reply {
            target.fail(msg.clone());
        }
        broadcast(
            &mut state.subscribers,
            ctx,
            &DaemonMessageType::Session {
                session_id: Some(ctx.session_id),
                event: SessionEvent::ReasoningEffortSetFailed { effort, error: msg },
            },
        );
        return false;
    }

    // Compute capability for the current model (if any).
    let capability = state.config.selected_model.as_ref().and_then(|model| {
        let slug = state.effective_provider_slug()?;
        Some(model_reasoning_capability(slug, model))
    });

    // "off" is always valid — every model can disable reasoning. Otherwise
    // check the slug is in the model's capability set.
    let valid = effort == "off"
        || capability
            .as_ref()
            .is_some_and(|c| c.available_effort_levels.contains(&effort))
        // No model selected yet: accept the preference optimistically (it
        // will be validated when inference actually runs in
        // resolve_reasoning_effort).
        || capability.is_none();

    if valid {
        state.config.reasoning_effort = Some(effort.clone());
        info!(
            session_id = ctx.session_id,
            effort = %effort,
            model = ?state.config.selected_model,
            "reasoning effort set",
        );
        broadcast(
            &mut state.subscribers,
            ctx,
            &DaemonMessageType::Session {
                session_id: Some(ctx.session_id),
                event: SessionEvent::ReasoningEffortSet { effort },
            },
        );
        // Terminal success ack in addition to the broadcast above.
        if let Some(target) = reply {
            target.accept();
        }
        return false;
    }
    let model = state.config.selected_model.as_deref().unwrap_or("(none)");
    let msg = format!("model '{model}' does not support reasoning effort '{effort}'");
    warn!(session_id = ctx.session_id, error = %msg, "reasoning effort rejected");
    if let Some(target) = reply {
        target.fail(msg.clone());
    }
    broadcast(
        &mut state.subscribers,
        ctx,
        &DaemonMessageType::Session {
            session_id: Some(ctx.session_id),
            event: SessionEvent::ReasoningEffortSetFailed { effort, error: msg },
        },
    );
    false
}

/// Return the current reasoning effort via the reply channel.
pub(super) fn handle_get_reasoning_effort(
    reply: &mpsc::Sender<String>,
    state: &SessionState,
    ctx: &RequestContext,
) -> bool {
    let _ = ctx;
    let current = state
        .config
        .reasoning_effort
        .clone()
        .unwrap_or_else(|| "off".to_string());
    let _ = reply.send(current);
    false
}

/// Reply with the session's current [`SessionEvent::SessionState`] snapshot —
/// the answer to a `GetSessionState` request from a client that is NOT
/// attached. Reuses `session_state_message`, the same builder the attach push
/// uses, so the snapshot can never drift from the attach snapshot.
fn handle_get_state(
    reply: &mpsc::Sender<io::Result<DaemonMessageType>>,
    state: &SessionState,
    ctx: &RequestContext,
) -> bool {
    let _ = reply.send(Ok(state.session_state_message(ctx.session_id)));
    false
}

/// Handle Undo: mark the most recent user turn's subtree as deleted.
/// Uses a quick-reference `HashMap` to avoid an O(n) scan per ID.
pub(super) fn handle_undo(
    reply: Option<ReplyTarget>,
    state: &mut SessionState,
    ctx: &RequestContext,
) -> bool {
    let Some(turn_ids) = state.undo_turns() else {
        debug!(
            session_id = ctx.session_id,
            "undo requested but no user turn to undo",
        );
        // An empty `/undo` is the silent-success gap this closes: the requester
        // now gets an explicit `Failed { kind: Undo, error: "nothing to undo" }`
        // instead of nothing at all.
        if let Some(target) = reply {
            target.fail("nothing to undo");
        }
        return false;
    };
    info!(
        session_id = ctx.session_id,
        turn_count = turn_ids.len(),
        "undo: marked turns as undone",
    );
    // Undoing turns invalidates the server-side response chain: the persisted
    // `previous_response_id` points at a response whose conversation includes
    // the turns being undone, so restoring it on the next request would leak
    // that context back into the model (the builder skips undone turns, but
    // the chain does not). Clear it (and its provenance) so the next request
    // falls back to a non-chained one carrying only the visible turns. Redo
    // deliberately does NOT restore the id — the turns come back, but the
    // chain is reset; a stateless request is always safe, and the old id was
    // already discarded.
    if state.config.last_response_id.is_some() {
        state.config.last_response_id = None;
        state.config.last_response_id_producer = None;
        // Persist the cleared id so a daemon restart cannot resurrect the
        // stale chain from the on-disk record. Writing the record directly
        // (rather than `persist_session_metadata`) keeps undo's observable
        // behavior unchanged: `last_modified` is not bumped, so the sessions
        // list does not reorder.
        let record = SessionRecord::from(&*state);
        if let Err(e) = write_session_retry(&ctx.db, ctx.session_id, &record) {
            tracing::warn!(error = %e, "failed to persist session record after Undo");
        }
    }
    // Persist the updated turns.
    for &id in &turn_ids {
        if let Some(turn) = state.turns.get(&id)
            && let Err(e) = write_turn_retry(&ctx.db, ctx.session_id, id, turn)
        {
            tracing::warn!(turn_id = id, error = %e, "failed to persist undone turn");
        }
    }
    broadcast(
        &mut state.subscribers,
        ctx,
        &DaemonMessageType::Session {
            session_id: Some(ctx.session_id),
            event: SessionEvent::TurnsUndone { turn_ids },
        },
    );
    if let Some(target) = reply {
        target.accept();
    }
    false
}

/// Reinstate the turns that were hidden by the preceding undo,
/// persisting the restored state so it survives daemon restart.
pub(super) fn handle_redo(
    reply: Option<ReplyTarget>,
    state: &mut SessionState,
    ctx: &RequestContext,
) -> bool {
    let Some(turns) = state.redo_turns() else {
        debug!(
            session_id = ctx.session_id,
            "redo requested but nothing to redo (no prior undo, or new input after undo)",
        );
        if let Some(target) = reply {
            target.fail("nothing to redo");
        }
        return false;
    };
    info!(
        session_id = ctx.session_id,
        turn_count = turns.len(),
        "redo: restored previously-undone turns",
    );
    for (&id, turn) in &turns {
        if let Err(e) = write_turn_retry(&ctx.db, ctx.session_id, id, turn) {
            tracing::warn!(turn_id = id, error = %e, "failed to persist redone turn");
        }
    }
    broadcast(
        &mut state.subscribers,
        ctx,
        &DaemonMessageType::Session {
            session_id: Some(ctx.session_id),
            event: SessionEvent::TurnsRedone {
                turns: turns
                    .iter()
                    .map(|(&turn_id, turn)| (turn_id, turn_for_client(turn)))
                    .collect(),
            },
        },
    );
    if let Some(target) = reply {
        target.accept();
    }
    false
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
