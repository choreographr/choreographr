//! Subscriber-plumbing handlers: react to a session-status change, attach /
//! detach / drop a client, answer a summary request, and fan a worker's
//! message out to the live subscriber set.

use super::{
    ClientId, DaemonCommand, DaemonMessage, DaemonMessageType, RequestContext, SessionEvent,
    SessionState, SessionStatus, SessionSummary, SubscriberSink, broadcast, debug, info,
};

/// Update the session status and broadcast to subscribers and daemon.
///
/// Status transitions (Sending/Waiting/Reasoning/Responding, `ToolCall`,
/// Retrying) are internal pipeline churn, NOT user-visible modifications — the
/// status is refreshed everywhere but `last_modified` is deliberately left
/// untouched so the sessions list does not re-sort on every tool call
/// mid-request.  Only completed requests (`handle_request_finished`) and
/// explicit metadata edits (`persist_session_metadata`) bump the timestamp.
/// The message carries the session's *current* `last_modified`, so clients'
/// monotonic `max()` guards keep the value stable.
pub(in crate::sessions) fn handle_status_changed(
    new_status: SessionStatus,
    state: &mut SessionState,
    ctx: &RequestContext,
) -> bool {
    // The status enum carries no user content (a `ToolCall` names the tool,
    // `Retrying` carries counts), so logging it is safe and gives the daemon
    // log the `Sending → Waiting → Reasoning/Responding` transition sequence —
    // otherwise every mid-turn phase change is invisible.
    debug!(session_id = ctx.session_id, ?new_status, "session status");
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
pub(in crate::sessions) fn handle_attach(
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
pub(in crate::sessions) fn handle_detach(
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
pub(in crate::sessions) fn handle_remove_subscriber(
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
pub(in crate::sessions) fn handle_get_summary(
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

/// Broadcast a message through the live subscriber map.
pub(in crate::sessions) fn handle_broadcast(
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
