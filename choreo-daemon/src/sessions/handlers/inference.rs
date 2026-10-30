//! Inference-path handlers: start a user turn (on the session itself or a
//! pre-populated child session), cancel an active request, and merge a
//! finished request's result back into the authoritative session state.

use super::{
    ActiveRequest, ChildResult, DaemonCommand, DaemonMessageType, HashMap, ReplyTarget,
    RequestContext, RequestWorkerArgs, SessionEvent, SessionMetadata, SessionRecord,
    SessionSnapshot, SessionState, SessionStatus, TimestampMs, broadcast, debug, fail_request,
    info, io, run_request_worker, warn, write_session_retry, write_turn_retry,
};

/// Process a user input: validate, resolve provider, spawn a request worker.
pub(in crate::sessions) fn handle_run_input(
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
    // Log only the length, never the text: the user's prompt must not appear
    // in the daemon's log.
    info!(
        session_id = ctx.session_id,
        input_len = text.len(),
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
            // Nothing has counted the prompt yet: this run's first `Started`
            // (broadcast by the worker once it has built the request) is what
            // fills the estimate in.
            estimated_prompt_tokens: 0,
        },
    );

    // Workers don't need their own subscriber map — all broadcasts
    // are routed through SessionCommand::Broadcast to this main
    // session thread which holds the live subscriber set.
    let mut worker_session = SessionState::from_snapshot(state.snapshot(), HashMap::new());
    // Hand the worker this session's registry so a client it REBUILDS (an OAuth
    // 401 refresh re-resolves the provider on the worker thread) registers its
    // sockets in the same cancellable scope as the one built at spawn.
    worker_session.registry = state.registry.clone();
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
pub(in crate::sessions) fn handle_run_child_input(
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
            // Filled in by the worker's first per-iteration `Started`, exactly
            // like the `RunInput` path.
            estimated_prompt_tokens: 0,
        },
    );
    let mut worker_session = SessionState::from_snapshot(state.snapshot(), HashMap::new());
    // See `handle_run_input`: a rebuilt client must share the session's
    // cancellable socket scope.
    worker_session.registry = state.registry.clone();
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
pub(in crate::sessions) fn handle_cancel(
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

/// Apply the worker's snapshot (config only) and merge turn state.
pub(in crate::sessions) fn handle_request_finished(
    stream_id: u64,
    snapshot: SessionSnapshot,
    state: &mut SessionState,
    shutdown_requested: bool,
    ctx: &RequestContext,
) -> bool {
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
        // Compare against the pre-merge copy BEFORE inserting, so a turn the
        // worker changed since the previous merge is detected. (Comparing after
        // the insert would compare the freshly inserted clone to itself and
        // never fire.)
        let already_present = state.turns.contains_key(&turn_id);
        let changed = state.turns.get(&turn_id).is_some_and(|old| old != turn);
        state.turns.insert(turn_id, turn.clone());
        // A new turn is written at least once even if unchanged; an existing one
        // only when its content moved.
        let needs_write = !already_present || changed;
        // Skip a turn the worker already checkpointed to disk unchanged (see
        // `persist_turn_checkpoint`): the row matches the snapshot, so rewriting
        // it would only cost a redundant transaction + fsync. A turn the worker
        // did NOT persist (e.g. the turn open when a provider-side cancel
        // returned) is absent from `persisted_turns` and is still written here.
        if !needs_write || snapshot.persisted_turns.contains(&turn_id) {
            continue;
        }
        // The turn was already broadcast during the agent loop, so only the
        // durable write remains.
        if let Err(e) = write_turn_retry(&ctx.db, ctx.session_id, turn_id, turn) {
            if already_present {
                tracing::warn!(turn_id, error = %e, "failed to persist updated turn");
            } else {
                tracing::warn!(turn_id, error = %e, "failed to persist turn");
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
