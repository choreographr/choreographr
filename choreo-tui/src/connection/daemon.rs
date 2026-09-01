use super::{SessionUpdateRouting, route_session_update};
use crate::state::{App, Page, ProviderInfo, merge_token_usage};
use crate::terminal_progress;
use choreo_client_core::{ClientError, dispatch_daemon_message};
use choreo_proto::{ClientMessage, DaemonMessage, RefreshStatus, SessionEvent};

pub(crate) fn handle_daemon_message(
    message: DaemonMessage,
    app: &mut App,
    client_tx: &std::sync::mpsc::Sender<ClientMessage>,
) -> Result<(), ClientError> {
    // Dispatch per-variant handlers first, then let the generic
    // dispatch in choreo_client_core handle the rest (text notifications,
    // stream appends, image assembly, etc.).
    match &message {
        DaemonMessage::Session {
            session_id: Some(session_id),
            event:
                SessionEvent::SessionCreated {
                    parent_session_id,
                    account_name,
                    selected_model,
                    reasoning_effort,
                    ..
                },
            ..
        } => {
            // Already known — nothing to do, and skip the generic dispatch too.
            if app
                .session_mgr
                .sessions
                .iter()
                .any(|s| s.session_id == *session_id)
            {
                return Ok(());
            }
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
            return Ok(());
        }
        DaemonMessage::Session {
            session_id: Some(session_id),
            event: SessionEvent::SessionAttached,
            ..
        } => {
            app.handle_session_attached(*session_id);
        }
        DaemonMessage::Session {
            session_id: Some(session_id),
            event:
                SessionEvent::SessionStatusChanged {
                    status,
                    last_modified,
                },
            ..
        } => {
            // Detect an attached sub-session finishing BEFORE the status is
            // applied: the finish check needs the pre-transition (active)
            // status to distinguish "just finished" from "still idle".
            let switch_back = app.attached_subsession_finished(*session_id, status);
            app.handle_session_status_changed(*session_id, status, *last_modified);
            // The user was reading the sub-session on the Chat page and it
            // just finished — jump back to its parent with a notification.
            if let Some(parent_id) = switch_back {
                app.switch_back_to_parent(*session_id, parent_id, client_tx)?;
            }
            // Return early: the generic dispatch would call the same handler
            // again via the TurnEventHandler trait, and the sessions-page
            // re-sort must only run once.
            return Ok(());
        }
        DaemonMessage::Sessions { sessions } => {
            // The Sessions handler manages the full lifecycle and should not
            // fall through to the generic dispatch (which would duplicate
            // the summary output).
            return app.handle_sessions(sessions, client_tx);
        }
        DaemonMessage::Session {
            session_id: Some(session_id),
            event: SessionEvent::SessionDeleted,
            ..
        } => {
            app.handle_session_deleted(*session_id);
        }
        DaemonMessage::Session {
            session_id: Some(session_id),
            event: SessionEvent::SessionDeleteFailed { error },
            ..
        } => {
            app.handle_session_delete_failed(*session_id, error);
        }
        DaemonMessage::Session {
            event: SessionEvent::SessionFailed {
                operation, error, ..
            },
            ..
        } => {
            app.error = Some(format!("[daemon] {operation} failed: {error}"));
            // If we're on the Session Manager page, also show the error
            // right on that page so the user has immediate feedback.
            if app.page == Page::SessionManager && operation == "create_session" {
                app.session_mgr.set_error(error.clone());
            }
        }
        // ── AI Provider Accounts ──────────────────────────
        DaemonMessage::Accounts { accounts } => {
            app.handle_accounts(accounts);
            // Don't return early here — fall through to dispatch_daemon_message
            // which will push the account list to the chat history so the
            // user sees the response to their `/account` command.
        }
        DaemonMessage::AccountListFailed { error } => {
            app.error = Some(format!("[daemon] failed to list accounts: {error}"));
            return Ok(());
        }
        DaemonMessage::AccountAdded { name } => {
            app.status = Some(format!("[daemon] account added: {name}"));
            let _ = client_tx.send(ClientMessage::ListAccounts);
        }
        DaemonMessage::AccountAddFailed { name, error } => {
            // The account never got created, so drop the credential modal the
            // wizard auto-opened right after submit — but ONLY when it is still
            // aimed at the failing account.  The daemon's failure reply can
            // arrive after the user has already dismissed that modal and
            // opened a DIFFERENT account's key modal; closing unconditionally
            // would discard their in-progress input.  The error line is still
            // surfaced unconditionally either way.
            if app.ai_providers.credential.target.as_deref() == Some(name.as_str()) {
                app.ai_providers.credential.close();
            }
            app.error = Some(format!("[daemon] failed to add account {name}: {error}"));
        }
        DaemonMessage::AccountRemoved { name } => {
            app.status = Some(format!("[daemon] account removed: {name}"));
            app.ai_providers.remove_account(name);
            let _ = client_tx.send(ClientMessage::ListAccounts);
        }
        DaemonMessage::AccountRemoveFailed { name, error } => {
            app.error = Some(format!("[daemon] failed to remove account {name}: {error}"));
        }
        // A credential mutation does not carry the updated account list, so
        // re-request it: the accounts page renders `has_credential` per
        // account, and without a refresh it would keep showing the stale
        // pre-credential state until the user leaves and re-enters the page.
        DaemonMessage::CredentialAdded { service } => {
            app.status = Some(format!("[daemon] credential added: {service}"));
            let _ = client_tx.send(ClientMessage::ListAccounts);
        }
        DaemonMessage::CredentialRemoved { service } => {
            app.status = Some(format!("[daemon] credential removed: {service}"));
            let _ = client_tx.send(ClientMessage::ListAccounts);
        }
        // ACL enrollment feedback: the direct reply says whether THIS add
        // worked; the broadcast tells every connected client the new total
        // (this client included, so one status line suffices per event).
        DaemonMessage::AclAddResult { ok, message } => {
            if *ok {
                app.status = Some(format!("[daemon] {message}"));
            } else {
                app.status = Some(format!("[daemon] acl add failed: {message}"));
            }
        }
        DaemonMessage::AclUpdated { clients } => {
            app.status = Some(format!(
                "[daemon] ACL updated — {clients} authorized client(s)"
            ));
        }

        DaemonMessage::Session {
            session_id: Some(session_id),
            event:
                SessionEvent::SessionState {
                    token_usage,
                    context_window,
                    last_prompt_tokens,
                    working_dir,
                    status,
                    ..
                },
            ..
        } => {
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
                    display.working_dir = working_dir.clone();
                    display.progress_dirty = true;
                }
                app.attached_status = Some(status.clone());
            }
            // Fall through to dispatch_daemon_message for message processing.
        }
        DaemonMessage::Session {
            session_id: Some(session_id),
            event:
                SessionEvent::Done {
                    token_usage,
                    last_prompt_tokens,
                    ..
                },
            ..
        } => {
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
                        terminal_progress::update_terminal_progress(Some(tokens), Some(cw));
                    }
                }
            }
            // Fall through to dispatch_daemon_message.
        }
        DaemonMessage::Session {
            session_id,
            event:
                SessionEvent::ModelSelected {
                    model,
                    reasoning_capability,
                    ..
                },
            ..
        } => {
            // Route the per-session display update to whichever session the
            // message belongs to (never the attached one when they differ).
            let reported = *session_id;
            match route_session_update(app, reported, |app, session_id| {
                app.handle_model_selected(session_id, model, reasoning_capability.clone());
            }) {
                SessionUpdateRouting::Suppress => {
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
                    return Ok(());
                }
                // Attached session (or a connection-level `None` reply): fall
                // through so the user's own `/model` command still prints its
                // confirmation to the status line.
                SessionUpdateRouting::FallThrough => {}
            }
        }
        DaemonMessage::Session {
            session_id,
            event: SessionEvent::ModelSelectionFailed { model, error, .. },
            ..
        } => {
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
            match route_session_update(app, reported, |_, _| {}) {
                SessionUpdateRouting::Suppress => {
                    tracing::debug!(
                        session_id = reported,
                        %model,
                        %error,
                        "suppressing status feedback for background session's model selection failure",
                    );
                    return Ok(());
                }
                // Attached session (or a connection-level `None` reply): fall
                // through so the generic dispatch writes the `[daemon] failed
                // to select model …` error line.
                SessionUpdateRouting::FallThrough => {}
            }
        }
        DaemonMessage::Session {
            session_id,
            event: SessionEvent::ReasoningEffortSet { effort, .. },
            ..
        } => {
            // Route the per-session display update to whichever session the
            // message belongs to.  The daemon replies to a bare `/reasoning`
            // (GetReasoningEffort) with no attachment at the connection level
            // (`session_id: None`), which `route_session_update` resolves to
            // the attached session — so the effort lands in the attached
            // session's display and the gate does not swallow the user's own
            // feedback.
            let reported = *session_id;
            match route_session_update(app, reported, |app, session_id| {
                app.handle_reasoning_effort_set(session_id, effort.clone());
            }) {
                SessionUpdateRouting::Suppress => {
                    // Background session: the per-session display was already
                    // updated above; stop here so the generic dispatch's
                    // "[daemon] reasoning effort: …" status write does not
                    // rewrite the global status line.
                    tracing::debug!(
                        session_id = reported,
                        %effort,
                        "suppressing status feedback for background session's reasoning effort change",
                    );
                    return Ok(());
                }
                // Attached session (or a connection-level `None` reply): fall
                // through so the user's own `/reasoning` command still prints
                // its confirmation to the status line.
                SessionUpdateRouting::FallThrough => {}
            }
        }
        DaemonMessage::Session {
            session_id,
            event: SessionEvent::ReasoningEffortSetFailed { effort, error, .. },
            ..
        } => {
            // Reset only the session the rejection belongs to — a background
            // session's rejection must not flip the attached session's effort,
            // though its own display is still reset to match the daemon (the
            // daemon has already forced the effort back to "off").  A
            // connection-level `None` ("no session attached") resolves to
            // the attached session, matching ReasoningEffortSet above.
            let reported = *session_id;
            match route_session_update(app, reported, |app, session_id| {
                app.display_for(session_id).reasoning_effort = Some("off".to_string());
            }) {
                SessionUpdateRouting::Suppress => {
                    // Background session: log at debug — an agent thrashing an
                    // unsupported effort in the background is not a warning
                    // for the user — and stop here so neither the status-line
                    // notice below nor the generic dispatch's `app.error`
                    // write can clobber the global status/error line for a
                    // session the user is not viewing.
                    tracing::debug!(
                        session_id = reported,
                        %effort,
                        %error,
                        "suppressing status feedback for background session's reasoning effort rejection",
                    );
                    return Ok(());
                }
                // Attached session (or a connection-level `None` reply): the
                // user's own `/reasoning` command failed — surface the
                // rejection notice and fall through so the generic dispatch
                // records the error as well.
                SessionUpdateRouting::FallThrough => {}
            }
            tracing::warn!(%effort, %error, "reasoning effort rejected by daemon");
            app.status = Some(format!("reasoning effort rejected: {error}"));
        }
        DaemonMessage::Session {
            session_id,
            event: SessionEvent::SessionAccountSet { account, .. },
            ..
        } => {
            // Route the per-session display update to whichever session the
            // message belongs to (never the attached one when they differ).
            // The daemon only ever reports a real session id here (a
            // no-session account change goes through SessionFailed), so the
            // connection-level `None` resolution is defensive but harmless.
            let reported = *session_id;
            match route_session_update(app, reported, |app, session_id| {
                app.handle_session_account_set(session_id, account);
            }) {
                SessionUpdateRouting::Suppress => {
                    // Background session: the per-session display was already
                    // updated above; stop here so the generic dispatch's
                    // "[daemon] session account set: …" status write does not
                    // rewrite the global status line.
                    tracing::debug!(
                        session_id = reported,
                        %account,
                        "suppressing status feedback for background session's account change",
                    );
                    return Ok(());
                }
                // Attached session (or a connection-level `None` reply): fall
                // through so the user's own `/account` command still prints
                // its confirmation to the status line.
                SessionUpdateRouting::FallThrough => {}
            }
        }
        DaemonMessage::Session {
            session_id: Some(session_id),
            event: SessionEvent::ContextWindowResolved { context_window },
            ..
        } => {
            if app.attached_session_id == Some(*session_id)
                && let Some(display) = app.active_display()
            {
                display.context_window = Some(*context_window);
            }
        }
        DaemonMessage::Session {
            session_id: Some(session_id),
            event: SessionEvent::SessionWorkingDirSet { path },
            ..
        } => {
            app.handle_session_working_dir_set(*session_id, path);
        }
        DaemonMessage::Session {
            session_id: Some(session_id),
            event: SessionEvent::SessionTitleSet { title },
            ..
        } => {
            app.handle_session_title_set(*session_id, title);
        }
        // TokenUsageUpdate is dispatched through the generic handler below.
        DaemonMessage::Session {
            session_id: Some(session_id),
            event: SessionEvent::LiveOutputTokenCount { output_tokens, .. },
            ..
        } => {
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
        }

        DaemonMessage::Models {
            models,
            selected_model,
        } => {
            if app.model_selector.is_open() {
                // While the selector is open, the reply populates the popup
                // and must NOT fall through to the generic dispatch, which
                // would print the whole list into the chat history.  Prefer
                // the daemon's reported selection, falling back to the
                // display's cached model when it is absent.
                let selected = selected_model.clone().or_else(|| {
                    app.active_display_ref()
                        .and_then(|d| d.selected_model.clone())
                });
                tracing::debug!(
                    count = models.len(),
                    ?selected,
                    "model selector: received model list"
                );
                app.model_selector.apply_models(models.clone(), selected);
                return Ok(());
            }
            // Selector closed: fall through to dispatch_daemon_message so
            // `/model` keeps printing the list into the chat history.
        }
        DaemonMessage::ModelsFailed { error } if app.model_selector.is_open() => {
            tracing::warn!(%error, "model selector: failed to list models");
            app.model_selector.apply_error(error.clone());
            return Ok(());
        }
        // Selector closed: fall through to the generic error handling.

        // ── S4: /refresh-models replies + catalog updates ────────────────
        DaemonMessage::ModelsRefreshed {
            providers,
            models,
            status,
        } => {
            let message = match status {
                RefreshStatus::UpToDate => {
                    format!("models up to date ({providers} providers, {models} models)")
                }
                RefreshStatus::Updated => {
                    format!("models updated ({providers} providers, {models} models)")
                }
                RefreshStatus::Forced => {
                    format!("models refreshed (forced) — {providers} providers, {models} models")
                }
            };
            tracing::info!(%message, "refresh-models reply");
            app.status = Some(message);
            return Ok(());
        }
        DaemonMessage::ModelsRefreshFailed { error } => {
            tracing::warn!(%error, "refresh-models failed");
            app.error = Some(format!("[daemon] refresh-models failed: {error}"));
            return Ok(());
        }
        DaemonMessage::CatalogUpdated { providers } => {
            // Replace the live provider list (the picker's source of truth)
            // and clamp the wizard selection if the list shrank. Only churn
            // the status line when the list actually changed — the daemon
            // also sends one on every activity-subscribe, which would
            // otherwise overwrite unrelated status messages at connect.
            let mapped = providers
                .iter()
                .map(|p| ProviderInfo {
                    slug: p.slug.clone(),
                    display_name: p.display_name.clone(),
                })
                .collect();
            if app.set_providers(mapped) {
                app.status = Some(format!(
                    "catalog updated ({} providers)",
                    app.providers.len()
                ));
            }
            return Ok(());
        }
        // ── Connection-level termination ─────────────────────────────
        // The daemon is going away (graceful shutdown) or has evicted this
        // client for lagging. In both cases the writer closes the socket
        // right after the message, so the TUI must stop and tell the user
        // why — the message is printed once the alternate screen is restored
        // (see `run_app`). An early return keeps the generic dispatch from
        // also pushing text into the chat history we are about to leave.
        DaemonMessage::ShuttingDown => {
            tracing::info!("daemon announced shutdown; quitting");
            app.should_quit = true;
            app.quit_message = Some("the server is shutting down".to_string());
            return Ok(());
        }
        DaemonMessage::Evicted => {
            tracing::warn!("evicted by the daemon for lag; quitting");
            app.should_quit = true;
            app.quit_message = Some(
                "disconnected by the daemon: evicted for falling behind the streaming lag limit"
                    .to_string(),
            );
            return Ok(());
        }
        _ => {}
    }

    // Dispatch remaining variants through the generic turn-event handler.
    dispatch_daemon_message(&message, app);
    Ok(())
}
