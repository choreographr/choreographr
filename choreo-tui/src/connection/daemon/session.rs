//! The **session** family of daemon messages: every `DaemonMessageType::Session`
//! envelope event, plus the `Sessions` snapshot.
//!
//! This family owns its own `match` over the `(session_id, event)` pair rather
//! than exposing one function per wire variant: all its arms are a single wire
//! variant (`Session { session_id, event }`), so the router in
//! [`super::handle_daemon_message`] collapses them into one dispatch-table arm
//! and the ~20 arms — several of which carry six typed fields — read as one
//! subject here instead of a wall of six-argument delegations. The
//! `spawn_subsession` tool-call guard lives here with its arm, so the guard and
//! the body it protects cannot drift apart.
//!
//! Each arm reports whether it **consumed** the message ([`Consumed`]): some
//! render their own state and must stop the generic dispatch (which would push
//! duplicate or misrouted text), while the rest record per-session display
//! state and must still fall through to it.
use super::super::{route_session_update, setup_flow};
use super::Consumed;
use crate::state::{App, Page, SPAWN_SUBSESSION_TOOL, merge_token_usage};
use crate::terminal::progress;
use choreo_client_core::ClientError;
use choreo_proto::{ClientMessage, SessionEvent, SessionSummary};

/// Route one `DaemonMessageType::Session` envelope event.
///
/// `session_id` is the envelope's origin id (`None` for a connection-level
/// reply with no session scope) and `event` the inner event. `reply_id` is the
/// message's correlation id, threaded to [`route_session_update`] so a targeted
/// reply falls through while a background broadcast is suppressed.
pub(super) fn on_session_message(
    app: &mut App,
    client_tx: &crossbeam_channel::Sender<ClientMessage>,
    reply_id: Option<u64>,
    session_id: Option<u64>,
    event: &SessionEvent,
) -> Result<Consumed, ClientError> {
    // Re-borrow the id as `&Option<u64>` so the arm patterns below destructure
    // it by reference (matching the wire shape) without copying a fresh option
    // into every arm.
    let session_id = &session_id;
    match (session_id, event) {
        (
            Some(session_id),
            SessionEvent::SessionCreatedForRequester {
                parent_session_id,
                account_name,
                selected_model,
                reasoning_effort,
                ..
            },
        ) => {
            // Direct reply to THIS client's `CreateSession` — the only
            // create-driven event that moves the view (attach to the session
            // the local user just made). NOT gated by an "already known"
            // check: the notification broadcast (below) may have added the
            // session to the list first — their arrival order is not
            // guaranteed — and that must not suppress the attach.
            app.handle_session_created(
                *session_id,
                *parent_session_id,
                account_name.clone(),
                selected_model.clone(),
                reasoning_effort.clone(),
                client_tx,
            )?;
            // Early return so we don't fall through to dispatch_daemon_message,
            // which would push text to the chat history (duplicate / invisible
            // on the Session Manager page).
            Ok(Consumed::Yes)
        }
        (
            Some(session_id),
            SessionEvent::SessionCreated {
                parent_session_id,
                account_name,
                selected_model,
                reasoning_effort,
                ..
            },
        ) => {
            // Broadcast notification that SOME connection created a session
            // (possibly ours, via the broadcast half of the create). It must
            // never move the view — only the direct ForRequester reply above
            // does that — so this only keeps the session list current when the
            // user is looking at it. This is the fix for a session created by
            // another client (e.g. the phone's view following the laptop).
            app.note_session_created(*session_id, *parent_session_id, client_tx);
            // The one exception: when the ATTACHED session ran a single
            // `spawn_subsession` call, enter the child it just created. The
            // in-flight-count gate tells our own session's spawn apart from a
            // sub-session created by another client (no local call → no
            // navigation) and refuses to thrash the view across parallel spawns.
            if let Some(parent_id) = app.spawned_child_should_navigate(*parent_session_id) {
                app.navigate_to_spawned_child(
                    *session_id,
                    parent_id,
                    account_name.clone(),
                    selected_model.clone(),
                    reasoning_effort.clone(),
                    client_tx,
                )?;
            }
            // Early return: the generic dispatch would push text to the chat
            // history for a session the user never opened.
            Ok(Consumed::Yes)
        }
        (Some(session_id), SessionEvent::SessionAttached) => {
            app.handle_session_attached(*session_id);
            Ok(Consumed::No)
        }
        (
            Some(session_id),
            SessionEvent::SessionStatusChanged {
                status,
                last_modified,
            },
        ) => {
            // Detect an attached sub-session finishing BEFORE the status is
            // applied: the finish checks need the pre-transition (active) state.
            // The auto-entered spawned child is tracked by its own summary-
            // independent record (see `App::spawned_child_nav`); consult it
            // first so a spawn's finish is caught even when its summary row has
            // not arrived, and fall back to the summary-based manual-open path.
            let spawned_finish = app.note_spawned_child_status(*session_id, status);
            let switch_back =
                spawned_finish.or_else(|| app.attached_subsession_finished(*session_id, status));
            app.handle_session_status_changed(*session_id, status, *last_modified);
            // The user was reading the sub-session on the Chat page and it
            // just finished — jump back to its parent with a notification.
            if let Some(parent_id) = switch_back {
                app.switch_back_to_parent(*session_id, parent_id, client_tx)?;
            }
            // Return early: the generic dispatch would call the same handler
            // again via the TurnEventHandler trait, and the sessions-page
            // re-sort must only run once.
            Ok(Consumed::Yes)
        }
        (Some(session_id), SessionEvent::SessionDeleted) => {
            app.handle_session_deleted(*session_id);
            Ok(Consumed::No)
        }
        (Some(session_id), SessionEvent::SessionDeleteFailed { error }) => {
            app.handle_session_delete_failed(*session_id, error);
            Ok(Consumed::No)
        }
        (
            _,
            SessionEvent::SessionFailed {
                operation, error, ..
            },
        ) => {
            app.set_error(format!("[daemon] {operation} failed: {error}"));
            // A failed attach leaves the client-side view of the session without
            // a transcript, and the daemon's `SessionStatus` vocabulary cannot
            // say so — record it on the display of the session being attached
            // so the status bar keeps reporting it after the transient error
            // line above is cleared by the next keystroke.  The event carries no
            // session id, so the attach request this client has on the wire is
            // the one in question; an attach issued without that bookkeeping
            // falls back to the optimistic attachment.
            //
            // The view deliberately STAYS on the session: detaching would move
            // the user somewhere they did not ask to be, and the red error line
            // plus the status slot already explain what happened.  Re-entering
            // the session from the session list is the only retry.
            if operation.as_str() == "attach_session" {
                let target = app.pending_attach_id.take().or(app.attached_session_id);
                if let Some(session_id) = target {
                    app.display_for(session_id).attach_failed = true;
                }
            }
            // If we're on the Session Manager page, also show the error
            // right on that page so the user has immediate feedback — for the
            // create flow and for the pin/archive flow (both of which are
            // driven from that page and otherwise only flash the transient
            // status line).
            if app.page == Page::SessionManager
                && matches!(
                    operation.as_str(),
                    "create_session" | "set_session_pinned" | "set_session_archived"
                )
            {
                app.session_mgr.set_error(error.clone());
            }
            Ok(Consumed::No)
        }
        (
            Some(session_id),
            SessionEvent::SessionState {
                token_usage,
                context_window,
                last_prompt_tokens,
                working_dir,
                status,
                ..
            },
        ) => {
            // Only update progress data when the message is for the
            // currently-attached session; stale messages from a previous
            // session that the daemon is still draining should be ignored.
            //
            // Only overwrite with Some values — a SessionState that arrives
            // after Done may not yet reflect the just-completed turn's
            // usage, and a blind `= *last_prompt_tokens` would wipe the
            // value Done just set.
            if app.attached_session_id == Some(*session_id) {
                {
                    let display = app.display_for(*session_id);
                    // Merge, never overwrite: the snapshot's token_usage can
                    // lag the fresher total accumulated via the all-activity
                    // subscription for a mid-turn session (see
                    // [`merge_token_usage`]), so a blind assignment would
                    // regress the status bar's token readout until the next
                    // TokenUsageUpdate.
                    display.token_usage = merge_token_usage(&display.token_usage, token_usage);
                    if let Some(cw) = context_window {
                        display.context_window = Some(*cw);
                    }
                    // Gap-fill, never overwrite: the snapshot's
                    // last_prompt_tokens can lag the value already shown via
                    // the all-activity subscription (the same cross-channel
                    // race as token_usage above), and unlike cumulative usage
                    // it is not monotonic, so a max-merge is wrong.  Never
                    // regress a fresher value; the next TokenUsageUpdate /
                    // Done refreshes it anyway.
                    if display.last_prompt_tokens.is_none()
                        && let Some(tokens) = last_prompt_tokens
                    {
                        display.last_prompt_tokens = Some(*tokens);
                    }
                    // Reuse the allocation: `working_dir` is only borrowed
                    // from the message here.
                    display.working_dir.clone_from(working_dir);
                    display.progress_dirty = true;
                }
                app.attached_status = Some(status.clone());
            }
            // Fall through to dispatch_daemon_message for message processing.
            Ok(Consumed::No)
        }
        (
            Some(session_id),
            SessionEvent::Done {
                token_usage,
                last_prompt_tokens,
                ..
            },
        ) => {
            // Progress-bar updates only apply to the currently-attached
            // session.  A Done for a background session (received via
            // SubscribeAllActivity) must not clobber the attached session's
            // token display — the generic dispatch below routes the
            // per-session bookkeeping (request cleanup, token_usage) to the
            // correct session display via handle_done.
            if app.attached_session_id == Some(*session_id) {
                // Capture per-request token usage at turn end.
                // Only set progress_dirty when we actually write data —
                // a Done message without token info doesn't change state.
                let has_data = token_usage.is_some() || last_prompt_tokens.is_some();

                if let Some(usage) = token_usage {
                    let display = app.display_for(*session_id);
                    display.token_usage = Some(*usage);
                    // Many providers only supply token_usage without the
                    // separate last_prompt_tokens field.  Fall back to
                    // input_tokens so the progress bar always updates.
                    if last_prompt_tokens.is_none() {
                        display.last_prompt_tokens = Some(usage.input_tokens);
                    }
                }
                if let Some(tokens) = last_prompt_tokens {
                    let display = app.display_for(*session_id);
                    display.last_prompt_tokens = Some(*tokens);
                }

                if has_data {
                    let display = app.display_for(*session_id);
                    display.progress_dirty = true;
                    // Push the update directly instead of waiting for the
                    // render loop — bypasses any timing issues with the
                    // progress_dirty flag getting consumed before render.
                    if let (Some(cw), Some(tokens)) =
                        (display.context_window, display.last_prompt_tokens)
                    {
                        progress::update(Some(tokens), Some(cw), &app.terminal);
                    }
                }
            }
            // Fall through to dispatch_daemon_message.
            Ok(Consumed::No)
        }
        (
            session_id,
            SessionEvent::ModelSelected {
                model,
                reasoning_capability,
                ..
            },
        ) => {
            // Route the per-session display update to whichever session the
            // message belongs to (never the attached one when they differ).
            let reported = *session_id;
            if !route_session_update(app, reported, reply_id, |app, session_id| {
                app.handle_model_selected(session_id, model, reasoning_capability.clone());
            }) {
                // Background session: the per-session display was already
                // updated above; stop here so the generic dispatch's
                // "[daemon] selected model: …" status write does not
                // rewrite the global status line the user is looking at
                // (which would reflow the viewed viewport).
                tracing::debug!(
                    session_id = reported,
                    %model,
                    "suppressing status feedback for background session's model selection",
                );
                return Ok(Consumed::Yes);
            }
            // Attached session (or a targeted/connection-level reply): fall
            // through so the user's own `/model` command still prints its
            // confirmation to the status line.
            Ok(Consumed::No)
        }
        (session_id, SessionEvent::ModelSelectionFailed { model, error, .. }) => {
            // The failure counterpart of ModelSelected above.  There is no
            // display to update — the selection failed, so nothing was
            // recorded — but routing through the shared helper keeps
            // "resolved and gated as one operation" for every arm; the empty
            // update closure is deliberate.  A background session's rejected
            // selection must not clobber the global error line, while a
            // connection-level `None` ("no session attached") and the
            // attached session fall through so the user sees the rejection of
            // their own `/model` command.
            let reported = *session_id;
            if !route_session_update(app, reported, reply_id, |_, _| {}) {
                tracing::debug!(
                    session_id = reported,
                    %model,
                    error_len = error.len(),
                    "suppressing status feedback for background session's model selection failure",
                );
                return Ok(Consumed::Yes);
            }
            // Attached session (or a targeted/connection-level reply): fall
            // through so the generic dispatch writes the `[daemon] failed
            // to select model …` error line.
            Ok(Consumed::No)
        }
        (session_id, SessionEvent::ReasoningEffortSet { effort, .. }) => {
            // Route the per-session display update to whichever session the
            // message belongs to.  The daemon replies to a bare `/reasoning`
            // (GetReasoningEffort) with no attachment at the connection level
            // (`session_id: None`), which `route_session_update` resolves to
            // the attached session — so the effort lands in the attached
            // session's display and the gate does not swallow the user's own
            // feedback.
            let reported = *session_id;
            if !route_session_update(app, reported, reply_id, |app, session_id| {
                app.handle_reasoning_effort_set(session_id, effort.clone());
            }) {
                // Background session: the per-session display was already
                // updated above; stop here so the generic dispatch's
                // "[daemon] reasoning effort: …" status write does not
                // rewrite the global status line.
                tracing::debug!(
                    session_id = reported,
                    %effort,
                    "suppressing status feedback for background session's reasoning effort change",
                );
                return Ok(Consumed::Yes);
            }
            // Attached session (or a targeted/connection-level reply): fall
            // through so the user's own `/reasoning` command still prints
            // its confirmation to the status line.
            Ok(Consumed::No)
        }
        (session_id, SessionEvent::ReasoningEffortSetFailed { effort, error, .. }) => {
            // Reset only the session the rejection belongs to — a background
            // session's rejection must not flip the attached session's effort,
            // though its own display is still reset to match the daemon (the
            // daemon has already forced the effort back to "off").  A
            // connection-level `None` ("no session attached") resolves to
            // the attached session, matching ReasoningEffortSet above.
            let reported = *session_id;
            if !route_session_update(app, reported, reply_id, |app, session_id| {
                app.display_for(session_id).reasoning_effort = Some("off".to_string());
            }) {
                // Background session: log at debug — an agent thrashing an
                // unsupported effort in the background is not a warning
                // for the user — and stop here so neither the status-line
                // notice below nor the generic dispatch's `app.error`
                // write can clobber the global status/error line for a
                // session the user is not viewing.
                tracing::debug!(
                    session_id = reported,
                    %effort,
                    error_len = error.len(),
                    "suppressing status feedback for background session's reasoning effort rejection",
                );
                return Ok(Consumed::Yes);
            }
            // Attached session (or a targeted/connection-level reply): the
            // user's own `/reasoning` command failed — surface the
            // rejection notice and fall through so the generic dispatch
            // records the error as well.
            tracing::warn!(%effort, error_len = error.len(), "reasoning effort rejected by daemon");
            app.set_status(format!("reasoning effort rejected: {error}"));
            Ok(Consumed::No)
        }
        (session_id, SessionEvent::SessionAccountSet { account, .. }) => {
            // Route the per-session display update to whichever session the
            // message belongs to (never the attached one when they differ).
            // The daemon only ever reports a real session id here (a
            // no-session account change goes through SessionFailed), so the
            // connection-level `None` resolution is defensive but harmless.
            let reported = *session_id;
            if !route_session_update(app, reported, reply_id, |app, session_id| {
                app.handle_session_account_set(session_id, account);
            }) {
                // Background session: the per-session display was already
                // updated above; stop here so the generic dispatch's
                // "[daemon] session account set: …" status write does not
                // rewrite the global status line.
                tracing::debug!(
                    session_id = reported,
                    %account,
                    "suppressing status feedback for background session's account change",
                );
                return Ok(Consumed::Yes);
            }
            // The staged flow's own commit lands as this broadcast: it closes
            // the popups and clears the flow (guarded on the account it
            // staged). An account set by anything else leaves a flow in flight
            // alone.
            if *session_id == app.attached_session_id {
                setup_flow::on_account_set(app, client_tx, account);
            }
            // Attached session (or a targeted/connection-level reply): fall
            // through so the user's own `/account` command still prints
            // its confirmation to the status line.
            Ok(Consumed::No)
        }
        (Some(session_id), SessionEvent::ContextWindowResolved { context_window }) => {
            if app.attached_session_id == Some(*session_id)
                && let Some(display) = app.active_display()
            {
                display.context_window = Some(*context_window);
            }
            Ok(Consumed::No)
        }
        (Some(session_id), SessionEvent::SessionWorkingDirSet { path }) => {
            app.handle_session_working_dir_set(*session_id, path);
            Ok(Consumed::No)
        }
        (Some(session_id), SessionEvent::SessionTitleSet { title }) => {
            app.handle_session_title_set(*session_id, title);
            Ok(Consumed::No)
        }
        // TokenUsageUpdate is dispatched through the generic handler below.
        (Some(session_id), SessionEvent::LiveOutputTokenCount { output_tokens, .. }) => {
            // Route the live count to the session the message belongs to,
            // not the one the user happens to be viewing.  The TUI subscribes
            // to all session activity (SubscribeAllActivity), so these arrive
            // for every streaming session — writing to the active display
            // would let a background session's token count bleed into the
            // status bar of the session being viewed.  Each session keeps its
            // own live count; reset_for_session_switch preserves it, so the
            // count stays correct both while streaming in the background and
            // after the user switches to that session.
            let display = app.display_for(*session_id);
            display.live_output_tokens = *output_tokens;
            Ok(Consumed::No)
        }
        (
            session_id,
            SessionEvent::SessionFlagsChanged {
                pinned,
                archived_at,
            },
        ) => {
            // Per-session `pinned`/`archived_at` flag change. The daemon
            // broadcasts the post-change state to every client — the
            // requesting client included — and this broadcast is the STATE
            // update, not the acknowledgement. The requester's terminal ack is
            // a separate targeted reply (`Accepted` on success, a `SessionFailed`
            // event on failure). Applying the broadcast here, rather than
            // optimistically on the keypress, is what keeps the TUI's list in
            // agreement with the daemon.
            match session_id {
                Some(session_id) => {
                    app.handle_session_flags_changed(*session_id, *pinned, *archived_at);
                }
                None => {
                    tracing::debug!("SessionFlagsChanged without an origin session id; ignoring");
                }
            }
            // Must NOT fall through to the generic dispatch, which has nothing
            // to render for this event.
            Ok(Consumed::Yes)
        }
        (
            Some(session_id),
            SessionEvent::ToolCallFinished { tool_name, .. }
            | SessionEvent::ToolCallFailed { tool_name, .. },
        ) if tool_name.as_str() == SPAWN_SUBSESSION_TOOL => {
            // `spawn_subsession` blocks until the child it created completes, so
            // the parent's tool-call terminal event is a summary-independent
            // "child finished" signal. It catches a child that finished before
            // the TUI ever observed it active (whose active→idle status never
            // arrives), which the status-only path misses. Both finish paths
            // consume the single nav record, so exactly one switches back.
            if let Some((child_id, parent_id)) = app.note_spawned_child_tool_finished(*session_id) {
                app.switch_back_to_parent(child_id, parent_id, client_tx)?;
            }
            // No early return: fall through so the tool result still renders.
            Ok(Consumed::No)
        }
        _ => Ok(Consumed::No),
    }
}

/// Handle the full `Sessions` snapshot reply.
///
/// The Sessions handler manages the full lifecycle and must not fall through to
/// the generic dispatch (which would duplicate the summary output), so this
/// always reports [`Consumed::Yes`].
pub(super) fn on_sessions(
    app: &mut App,
    client_tx: &crossbeam_channel::Sender<ClientMessage>,
    sessions: &[SessionSummary],
) -> Result<Consumed, ClientError> {
    // The Sessions handler manages the full lifecycle and should not
    // fall through to the generic dispatch (which would duplicate
    // the summary output).
    app.handle_sessions(sessions, client_tx)?;
    // This is the initial (and any refresher) session snapshot. Apply
    // the one-shot `--attach <SESSION_ID>` request only AFTER the
    // snapshot — including its auto-attach to the most-recent session —
    // so the explicitly named session wins rather than being raced or
    // clobbered. `dispatch_pending_attach` takes the latch, so only the
    // first snapshot dispatches; a later ListSessions refresh is inert.
    app.dispatch_pending_attach(client_tx);
    Ok(Consumed::Yes)
}
