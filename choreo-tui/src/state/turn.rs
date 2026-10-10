//! Turn-request event handling: the [`TurnEventHandler`] implementation for
//! `App`, plus the shared request-terminal teardown.
//!
//! A "request" is one inference/tool turn the daemon streams to this client.
//! The handlers here fold those wire events into the per-session
//! [`SessionDisplayState`]: streamed output and tool-call lifecycle into the
//! in-flight turn, the attach snapshot merge, and the three terminal outcomes
//! (`done` / `failed` / `cancelled`) that share [`App::finish_request`]. The
//! trivial session-lifecycle delegates (`handle_session_attached`,
//! `handle_status_text`, …) live here too, since they are part of the same
//! trait impl.
//!
//! The `App` fields and helper methods this module reads (`display_for`,
//! `resolve_daemon_session`, `sync_turn_images`, `term_status_override`, …)
//! stay on `App` / [`SessionDisplayState`] in `state/`; this module only owns
//! the event-handling behaviour. Unit tests for its helpers live in
//! `state/tests.rs` (which reaches `turn_has_live_content` through
//! `super::turn`).

use choreo_client_core::TurnEventHandler;
use choreo_client_core::dispatch::{SessionStateData, ToolCallEvent};
use choreo_proto::{OutputStream, SessionStatus, TokenUsage, Turn};
use std::borrow::Cow;

use super::{App, SessionDisplayState, merge_token_usage};

/// Invalidate the render cache entry for `turn_id`.
fn invalidate_turn_cache(display: &mut SessionDisplayState, turn_id: u32) {
    if let Some(idx) = display
        .visible_turn_ids
        .iter()
        .position(|id| *id == turn_id)
        && let Some(slot) = display.render_cache.get_mut(idx)
    {
        *slot = None;
    }
}

/// Decide whether the locally-accumulated version of a turn should win over
/// the daemon snapshot's version when merging an attach snapshot.
///
/// The snapshot is authoritative for finished turns, but for the in-flight
/// turn it contains only the empty placeholder inserted by `start_turn` — the
/// accumulated version (fed by `OutputChunk`, `ToolCallStarted` and
/// `ToolResultChunk` via the all-activity subscription) holds the real
/// streaming content.  Keep the accumulated turn whenever it carries content
/// the snapshot version lacks; otherwise prefer the snapshot, which is the
/// daemon's canonical state.
pub(super) fn turn_has_live_content(accumulated: &Turn, snapshot: &Turn) -> bool {
    (accumulated
        .assistant_text
        .as_deref()
        .is_some_and(|s| !s.is_empty())
        && snapshot.assistant_text.as_deref().is_none_or(str::is_empty))
        || (accumulated
            .assistant_reasoning
            .as_deref()
            .is_some_and(|s| !s.is_empty())
            && snapshot
                .assistant_reasoning
                .as_deref()
                .is_none_or(str::is_empty))
        || (!accumulated.tool_calls.is_empty() && snapshot.tool_calls.is_empty())
        || (!accumulated.tool_results.is_empty() && snapshot.tool_results.is_empty())
        || (!accumulated.displayed_images.is_empty() && snapshot.displayed_images.is_empty())
}

impl App {
    /// Tear down the per-session display state a terminal request outcome
    /// leaves behind.  A completion ([`App::handle_done`]), a failure
    /// ([`App::handle_failed`]), and a cancel ([`App::handle_cancelled`]) all
    /// end an in-flight request, so they share this teardown: the
    /// front-end-agnostic bookkeeping (clear the closing turn's tool-call
    /// descriptions, drop the request→turn mapping) runs in
    /// `SessionView::finish_request`, and the TUI-specific display state (the
    /// active-request set, the live token estimates, and the streaming slot)
    /// is reset here before the content is marked changed.
    fn finish_request(&mut self, session_id: u64, stream_id: u64) {
        let display = self.display_for(session_id);
        // The request→turn bookkeeping is shared with the GUI's `SessionView`.
        display.view.finish_request(stream_id);
        display.active.remove(&stream_id);
        display.live_input_estimate = 0;
        display.live_output_tokens = 0;
        display.streaming_turn_index = None;
        display.streaming_response = None;
        display.mark_content_changed();
    }
}

// ── TurnEventHandler implementation ──────────────────────────────────

impl TurnEventHandler for App {
    fn handle_image(
        &mut self,
        session_id: u64,
        turn_id: u32,
        key: choreo_proto::ImageKey,
        data: Option<Vec<u8>>,
    ) {
        tracing::trace!(%session_id, %turn_id, ?key, "handle_image");
        self.handle_image_reply(session_id, turn_id, key, data);
    }

    fn handle_turn_appended(&mut self, session_id: u64, turn_id: u32, turn: Turn) {
        tracing::trace!(%turn_id, "handle_turn_appended");
        self.sync_turn_images(session_id, turn_id, &turn);
        let display = self.display_for(session_id);
        invalidate_turn_cache(display, turn_id);
        display.view.insert_or_replace(turn_id, turn);
        // Replacement can change the rendered content even when the cache key's
        // other fields (widths, reasoning/collapse state) stay identical, so
        // bump the version to force a recompute on the next rebuild.
        display.bump_turn_version(turn_id);
        display.mark_content_changed();
    }

    fn handle_turns_undone(&mut self, session_id: u64, turn_ids: &[u32]) {
        tracing::trace!(?turn_ids, "handle_turns_undone");
        let display = self.display_for(session_id);
        for tid in turn_ids {
            invalidate_turn_cache(display, *tid);
            // Drop the content-version entry rather than bumping it: the
            // cache slot was invalidated above and undone turns are skipped
            // by rebuilds, so no cached rendering can survive for this turn;
            // `handle_turns_redone` re-invalidates the slot before
            // re-inserting, so a redone turn (even with byte-identical
            // content) always recomputes fresh.  Pruning keeps the version
            // map bounded by the live (non-undone) turn set instead of the
            // session's whole history.
            display.turn_versions.remove(tid);
            // Drop the user's reasoning-expansion preference for undone turns
            // so the map can't accumulate stale entries; a redo restores the
            // turn fresh with the derived default.
            display.reasoning_override.remove(tid);
            // Same for tool-result collapse preferences: a redo restores the
            // turn fresh, so stale (turn, call_id) overrides must not leak.
            display.tool_collapse_override.remove(tid);
            if let Some(turn) = display.view.turns.get_mut(tid) {
                turn.undone = true;
            }
        }
        display.mark_content_changed();
    }

    fn handle_turns_redone(
        &mut self,
        session_id: u64,
        turns: std::collections::BTreeMap<u32, Turn>,
    ) {
        // Never `?turns`: a `Turn` carries message content.
        tracing::trace!(count = turns.len(), "handle_turns_redone");
        // Sync images first, then get display to avoid borrow conflict.
        for (tid, turn) in &turns {
            self.sync_turn_images(session_id, *tid, turn);
        }
        let display = self.display_for(session_id);
        for (tid, turn) in turns {
            invalidate_turn_cache(display, tid);
            display.bump_turn_version(tid);
            display.view.insert_or_replace(tid, turn);
        }
        display.mark_content_changed();
    }

    fn handle_request_stream(
        &mut self,
        session_id: u64,
        stream_id: u64,
        stream: OutputStream,
        data: Cow<'_, str>,
    ) {
        let display = self.display_for(session_id);
        // Detect the first Answer chunk for this request: the turn has no
        // response text yet, so this chunk begins the response phase.
        let turn_id = display.view.request_to_turn.get(&stream_id).copied();
        let first_answer = matches!(stream, OutputStream::Answer)
            && turn_id
                .and_then(|id| display.view.turns.get(&id))
                .is_some_and(|t| t.assistant_text.is_none());

        display.view.stream_chunk(stream_id, &stream, &data);

        // The appended chunk changed the turn's rendered content: bump its
        // version so any rebuild (e.g. one triggered by an interleaved
        // `Done`/`TurnAppended` from another request or session) recomputes
        // this turn instead of serving the pre-chunk cached lines.
        if let Some(turn_id) = turn_id {
            display.bump_turn_version(turn_id);
        }

        // Auto-collapse reasoning when the response starts — drop any
        // explicit expansion override so the derived default (collapsed once
        // a response exists) takes over.  The user can re-expand it by
        // clicking the header.
        if first_answer && let Some(turn_id) = turn_id {
            display.reasoning_override.remove(&turn_id);
        }

        display.resolve_streaming_turn_index(stream_id);
        display.mark_streaming_changed();
    }

    fn handle_started(
        &mut self,
        session_id: u64,
        stream_id: u64,
        turn_id: u32,
        estimated_prompt_tokens: u32,
    ) {
        tracing::trace!(%stream_id, %turn_id, %estimated_prompt_tokens, "handle_started");
        let display = self.display_for(session_id);
        display.view.request_to_turn.insert(stream_id, turn_id);
        display.active.insert(stream_id);
        display.live_input_estimate = estimated_prompt_tokens;
        display.live_output_tokens = 0;
        display.streaming_turn_index = display
            .visible_turn_ids
            .iter()
            .position(|id| *id == turn_id);
    }

    fn handle_done(
        &mut self,
        session_id: u64,
        stream_id: u64,
        token_usage: Option<TokenUsage>,
        last_prompt_tokens: Option<u32>,
    ) {
        tracing::trace!(%stream_id, "handle_done");
        // Done always arrives with `Some` (the session task knows its id), but
        // resolve defensively anyway so this choke point can never write to an
        // unintended display if a connection-level path is ever added.
        let Some(session_id) = self.resolve_daemon_session(Some(session_id)) else {
            return;
        };
        // A completed turn is a terminal outcome `SessionStatus` cannot
        // express; record it as `done` so it survives the trailing idle status
        // the daemon broadcasts when the request finishes.
        self.term_status_override.insert(session_id, "done");
        self.term_status_dirty = true;
        // Apply the completed turn's token accounting before the shared
        // teardown resets the live estimates.  The final `TurnAppended` already
        // cleaned description entries via `insert_or_replace`, but if that
        // broadcast was dropped under load the map would keep them —
        // `finish_request` clears them for this turn so the map stays bounded by
        // in-flight calls even when the terminal broadcast is lost.
        {
            let display = self.display_for(session_id);
            if let Some(usage) = token_usage {
                display.token_usage = Some(usage);
                if last_prompt_tokens.is_none() {
                    display.last_prompt_tokens = Some(usage.input_tokens);
                }
            }
            if let Some(tokens) = last_prompt_tokens {
                display.last_prompt_tokens = Some(tokens);
            }
        }
        self.finish_request(session_id, stream_id);
    }

    fn handle_failed(&mut self, session_id: Option<u64>, stream_id: u64, error: String) {
        // Never `%error`: a failure message can embed provider/request text.
        tracing::trace!(%stream_id, error_len = error.len(), "handle_failed");
        // A connection-level failure (e.g. "no session attached" from
        // RunInput/SetModel/SetReasoningEffort) arrives with `session_id:
        // None` — no origin session — meaning "the attached session".  Resolve
        // it so the failure lands in the session the user is actually attached
        // to rather than a phantom display.
        let is_connection_level = session_id.is_none();
        let Some(session_id) = self.resolve_daemon_session(session_id) else {
            // Never `%error` (even here): a failure message can embed
            // provider/request text, so log only its length.
            tracing::debug!(%stream_id, error_len = error.len(), "dropping failure: no attached session to route the connection-level failure to");
            // No display to update, but a connection-level rejection (e.g.
            // "no session attached") is exactly what the user needs to see
            // on the status line.
            if is_connection_level {
                self.error = Some(error);
            }
            return;
        };
        // A request-level failure is a turn outcome `SessionStatus` cannot
        // express — record it so it survives the trailing idle status. A
        // connection-level rejection has no origin session and is not a
        // session turn outcome, so it is left to the status line.  A
        // cancellation never reaches here (it has its own `handle_cancelled`)
        // — this is a real failure, so it reports `error`.
        if !is_connection_level {
            self.term_status_override.insert(session_id, "error");
            self.term_status_dirty = true;
        }
        // A connection-level failure has no turn to render an error block in,
        // so the global status/error bar is its only surface.  A request-level
        // failure (a real session id) already renders the full error as the
        // turn's red block in the transcript — writing it here too would
        // print the same message twice on screen — so it is only recorded on
        // the per-session display.  (Written before the mutable display
        // borrow below so `self.error` is still reachable.)
        if is_connection_level {
            self.error = Some(error.clone());
        }
        // The per-session display records the failure for whichever session it
        // belongs to (rendered once the user views that session).
        self.display_for(session_id).error = Some(error);
        self.finish_request(session_id, stream_id);
    }

    fn handle_cancelled(&mut self, session_id: Option<u64>, stream_id: u64) {
        tracing::trace!(%stream_id, "handle_cancelled");
        // A connection-level cancel (no origin session) resolves to the
        // attached session, mirroring `handle_failed`, so it never lands in a
        // phantom display.  A cancel carries no text, so when there is no
        // session to route to there is nothing to show and we simply stop.
        let Some(session_id) = self.resolve_daemon_session(session_id) else {
            tracing::debug!(%stream_id, "dropping cancel: no attached session to route it to");
            return;
        };
        // A cancel is a terminal turn outcome `SessionStatus` cannot express,
        // but it is NOT a failure: report `idle` rather than `error` so the
        // cancelled turn clears its working state without a red error block.
        self.term_status_override.insert(session_id, "idle");
        self.term_status_dirty = true;
        // Deliberately NO error text (neither `self.error` nor `display.error`)
        // — a user cancel must not render as a failure.
        self.finish_request(session_id, stream_id);
    }

    fn handle_tool_call_event(&mut self, session_id: u64, stream_id: u64, event: ToolCallEvent) {
        let display = self.display_for(session_id);
        match event {
            ToolCallEvent::Started {
                call_id,
                tool_name,
                arguments_json,
                invocation_description,
            } => {
                // Look up the turn before mutating so the version bump below
                // can target the right turn (the start event may backfill the
                // stub's name/description — both visible in the rendered
                // header).
                let turn_id = display.view.request_to_turn.get(&stream_id).copied();
                display.view.tool_call_started(
                    stream_id,
                    call_id,
                    tool_name,
                    arguments_json,
                    &invocation_description,
                );
                if let Some(turn_id) = turn_id {
                    display.bump_turn_version(turn_id);
                }
                display.resolve_streaming_turn_index(stream_id);
                display.mark_streaming_changed();
            }
            ToolCallEvent::Finished { .. } => {}
            ToolCallEvent::Failed { .. } => {}
        }
    }

    fn handle_tool_result_chunk(
        &mut self,
        session_id: u64,
        stream_id: u64,
        call_id: String,
        data: Vec<u8>,
    ) {
        let text = String::from_utf8_lossy_owned(data);
        let display = self.display_for(session_id);
        // The chunk appends to `turn.tool_results[i].content` (rendered
        // live); bump the turn's content version so a rebuild between chunks
        // recomputes instead of reusing the pre-chunk cached lines — the
        // core fix for "scrollbar moves but results stay stuck".
        let turn_id = display.view.request_to_turn.get(&stream_id).copied();
        display.view.tool_result_chunk(stream_id, &call_id, &text);
        if let Some(turn_id) = turn_id {
            display.bump_turn_version(turn_id);
        }
        display.resolve_streaming_turn_index(stream_id);
        display.mark_streaming_changed();
    }

    fn handle_session_state(&mut self, state: SessionStateData) {
        tracing::debug!(
            turn_count = %state.turns.len(),
            ?state.selected_model,
            ?state.status,
            "handle_session_state"
        );
        let session_id = state.session_id;
        // SessionState snapshots are per-session: the daemon sends one for
        // the attached session on attach, but also broadcasts them for
        // background sessions (e.g. load_tools/unload_tools on that session
        // reach activity subscribers like the TUI).  Route the snapshot to
        // the session it belongs to, and only let the *attached* session's
        // snapshot drive the view switch and the status-bar fields below —
        // otherwise a background session's token usage / status / turns
        // would clobber the display the user is currently looking at.
        let is_attached = self.attached_session_id == Some(session_id);
        if is_attached {
            self.active_session_id = Some(session_id);
        }

        let SessionStateData {
            turns,
            title: _,
            selected_model,
            active_tool_groups,
            token_usage,
            context_window,
            last_prompt_tokens,
            status,
            reasoning_effort,
            reasoning_capability,
            ..
        } = state;

        // Merge the daemon snapshot with turns already accumulated locally
        // via the all-activity subscription (while the user was viewing
        // another session).  The snapshot is authoritative for finished
        // turns, but for an in-flight turn it only holds the empty
        // placeholder inserted by `start_turn` — the worker owns the live
        // content and only syncs back on RequestFinished.  The accumulated
        // turn carries the real streamed content, so it must win; otherwise
        // switching into a streaming session would blank the turn until the
        // next chunk arrived.
        let accumulated = {
            let display = self.display_for(session_id);
            std::mem::take(&mut display.view.turns)
        };
        let mut merged = turns;
        for (turn_id, acc_turn) in &accumulated {
            match merged.get_mut(turn_id) {
                Some(snap_turn) if turn_has_live_content(acc_turn, snap_turn) => {
                    *snap_turn = acc_turn.clone();
                }
                // Turn only known to the client (e.g. a turn created just
                // before this snapshot) — keep the accumulated version.
                None => {
                    merged.insert(*turn_id, acc_turn.clone());
                }
                // Snapshot is at least as complete — keep it.
                Some(_) => {}
            }
        }

        // Sync images before getting display to avoid borrow conflict.
        self.rendered_images.remove(&session_id);
        for (tid, turn) in &merged {
            self.sync_turn_images(session_id, *tid, turn);
        }
        let display = self.display_for(session_id);
        display.view.turns = merged;
        // Content versions must never outlive the turns they fingerprint:
        // drop entries whose turn left the view.  The snapshot merge is a
        // union today (accumulated turns are re-inserted below), so this is
        // defensive — it pins the invariant against any future path that
        // removes turns (undo keeps turns, only marking them undone).
        let live_turn_ids: Vec<u32> = display.view.turns.keys().copied().collect();
        display
            .turn_versions
            .retain(|turn_id, _| live_turn_ids.contains(turn_id));
        // The merge can silently replace a turn's content (the snapshot wins
        // when it is at least as complete, or the accumulated version wins
        // for the in-flight turn) — either way the cached rendering, built
        // from the pre-merge content, may now be stale.  Bump every turn the
        // client already knew about so the next rebuild recomputes rather
        // than reusing those lines.  Turns only present in the snapshot have
        // no cache entry, so they need no bump.
        for turn_id in accumulated.keys() {
            display.bump_turn_version(*turn_id);
        }
        display.selected_model = selected_model;
        // Merge, never overwrite: the attach snapshot can lag the fresher
        // total accumulated via the all-activity subscription for a mid-turn
        // session (see [`merge_token_usage`]), so a blind assignment would
        // regress the status bar's token readout until the next update.
        display.token_usage = merge_token_usage(&display.token_usage, &token_usage);
        if let Some(cw) = context_window {
            display.context_window = Some(cw);
        }
        // Gap-fill, never overwrite: the snapshot's last_prompt_tokens can
        // lag the value already broadcast to this client via the
        // all-activity subscription (the same cross-channel race as
        // token_usage), and unlike cumulative usage it is not monotonic, so
        // a max-merge is wrong.  Never regress a fresher value; the next
        // TokenUsageUpdate / Done refreshes it anyway.
        if display.last_prompt_tokens.is_none()
            && let Some(tokens) = last_prompt_tokens
        {
            display.last_prompt_tokens = Some(tokens);
        }
        if let Some(effort) = reasoning_effort {
            display.reasoning_effort = Some(effort);
        }
        if let Some(cap) = reasoning_capability {
            display.reasoning_capability = Some(cap);
        }
        display.mark_content_changed();
        let _ = display;
        // Only the attached session's snapshot may update the status bar's
        // per-attachment state — a background session's snapshot must not
        // overwrite the status/tool-group display while the user is viewing
        // the attached session.
        if is_attached {
            self.attached_status = Some(status);
            self.attached_tool_groups = active_tool_groups;
        }
    }

    fn handle_token_usage_update(
        &mut self,
        session_id: u64,
        token_usage: TokenUsage,
        last_prompt_tokens: Option<u32>,
    ) {
        tracing::trace!(
            ?token_usage,
            ?last_prompt_tokens,
            "handle_token_usage_update"
        );
        let display = self.display_for(session_id);
        display.token_usage = Some(token_usage);
        if let Some(tokens) = last_prompt_tokens {
            display.last_prompt_tokens = Some(tokens);
        }
        display.live_input_estimate = 0;
        display.live_output_tokens = 0;
    }

    fn handle_status_text(&mut self, text: String) {
        self.status = Some(text);
    }

    fn handle_error(&mut self, error: String) {
        self.error = Some(error);
    }

    fn handle_session_attached(&mut self, session_id: u64) {
        self.active_session_id = Some(session_id);
        self.attached_session_id = Some(session_id);
        self.term_status_dirty = true;
    }

    fn handle_session_created(
        &mut self,
        _session_id: u64,
        _title: Option<String>,
        _working_dir: Option<String>,
        _account_name: Option<String>,
        _selected_model: Option<String>,
        _reasoning_effort: Option<String>,
    ) {
    }

    fn handle_session_status_changed(
        &mut self,
        session_id: u64,
        status: SessionStatus,
        last_modified: i64,
    ) {
        self.handle_session_status_changed(session_id, &status, last_modified);
    }
}
