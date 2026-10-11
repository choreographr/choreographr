//! Turn-history handlers: the idle-gated `handle_undo` / `handle_redo` pair
//! and the `refuse_unless_idle` gate they share.

use super::{
    DaemonMessageType, ReplyTarget, RequestContext, SessionEvent, SessionRecord, SessionState,
    broadcast, broadcast_token_state, debug, info, turn_for_client, write_session_retry,
    write_turn_retry,
};

/// Refuse an undo/redo while the session is not idle.
///
/// Undo and redo mutate the turn history in ways the request worker cannot
/// see: they flip `undone` flags, push/pop the redo stack, and re-derive the
/// context size. A worker owns that history on a private clone for the whole
/// request and merges it back at `RequestFinished`, so a history mutation
/// underneath it would race the merge (resurrecting hidden turns and the
/// cleared response chain). Both commands therefore run only on an idle
/// session. The daemon is the authority — the TUI's identical client-side
/// check just spares the user a round-trip.
///
/// Returns `true` when the session is busy: the requester (if any) has been
/// answered with `Failed { kind, "session is not idle" }` and the caller must
/// return immediately without touching the history. Returns `false` when the
/// session is idle and the caller may proceed.
fn refuse_unless_idle(
    reply: &mut Option<ReplyTarget>,
    state: &SessionState,
    ctx: &RequestContext,
) -> bool {
    if state.active_requests.is_empty() {
        return false;
    }
    debug!(
        session_id = ctx.session_id,
        "undo/redo refused: session is not idle",
    );
    if let Some(target) = reply.take() {
        target.fail("session is not idle");
    }
    true
}

/// Handle Undo: mark the newest live user turn and every higher-id live turn
/// as `undone`, pushing the flipped set onto the redo stack.
/// Only possible on an idle session (see [`refuse_unless_idle`]).
pub(in crate::sessions) fn handle_undo(
    mut reply: Option<ReplyTarget>,
    state: &mut SessionState,
    ctx: &RequestContext,
) -> bool {
    if refuse_unless_idle(&mut reply, state, ctx) {
        return false;
    }
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
    // The context-size readout is derived from the live turns, so it shrank
    // with this undo: push the recomputed value to clients (and metadata) just
    // like a mid-turn token refresh, rather than leaving the pre-undo value on
    // screen until the next request completes.
    broadcast_token_state(state, ctx);
    if let Some(target) = reply {
        target.accept();
    }
    false
}

/// Reinstate the turns that were hidden by the preceding undo,
/// persisting the restored state so it survives daemon restart.
/// Only possible on an idle session (see [`refuse_unless_idle`]).
pub(in crate::sessions) fn handle_redo(
    mut reply: Option<ReplyTarget>,
    state: &mut SessionState,
    ctx: &RequestContext,
) -> bool {
    if refuse_unless_idle(&mut reply, state, ctx) {
        return false;
    }
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
    // Redo grew the context back; push the recomputed size (mirrors undo).
    broadcast_token_state(state, ctx);
    if let Some(target) = reply {
        target.accept();
    }
    false
}
