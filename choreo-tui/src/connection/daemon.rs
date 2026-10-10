use super::route_session_update;
use crate::state::{App, Page, ProviderInfo, merge_token_usage};
use crate::terminal::progress;
use choreo_client_core::{
    AutoBindAttempt, ClientError, Pending, PendingContext, attempt_keystore_auto_bind,
    dispatch_daemon_message, record_unlock_key,
};
use choreo_proto::{
    ClientMessage, ClientMessageType, DaemonMessage, DaemonMessageType, KeystoreState, MessageKind,
    RefreshStatus, SessionEvent,
};

/// Whether a resolved pending slot names a `ListModels` request — i.e. the
/// reply answers THIS client's model-list request (the only requester of
/// `ListModels`), so it belongs in the model-selector popup rather than the
/// chat history.
fn is_list_models_reply(resolved: Option<&Pending>) -> bool {
    resolved.is_some_and(|pending| pending.kind == MessageKind::ListModels)
}

// Owned-message call sites live in test files outside connection/, so the
// by-value signature is kept for those ergonomic owned-value call sites.
pub(crate) fn handle_daemon_message(
    message: DaemonMessage,
    app: &mut App,
    client_tx: &crossbeam_channel::Sender<ClientMessage>,
) -> Result<(), ClientError> {
    // Resolve the reply's pending slot FIRST, before any state handling: a
    // `Some(id)` message answers one of our requests, and the resolved slot's
    // kind/context tell the arms below what the reply means (the model-list
    // popup, a keystore key to record). A broadcast (`id: None`) resolves
    // nothing. This is a side table: resolving never replaces the payload's own
    // state dispatch at the bottom.
    let mut resolved = message.id.and_then(|id| app.pending.resolve(id));
    // Dispatch per-variant handlers first, then let the generic
    // dispatch in choreo_client_core handle the rest (text notifications,
    // stream appends, image assembly, etc.).
    match &message.inner {
        DaemonMessageType::Session {
            session_id: Some(session_id),
            event:
                SessionEvent::SessionCreatedForRequester {
                    parent_session_id,
                    account_name,
                    selected_model,
                    reasoning_effort,
                    ..
                },
            ..
        } => {
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
            return Ok(());
        }
        DaemonMessageType::Session {
            session_id: Some(session_id),
            event:
                SessionEvent::SessionCreated {
                    parent_session_id, ..
                },
            ..
        } => {
            // Broadcast notification that SOME connection created a session
            // (possibly ours, via the broadcast half of the create). It must
            // never move the view — only the direct ForRequester reply above
            // does that — so this only keeps the session list current when the
            // user is looking at it. This is the fix for a session created by
            // another client (e.g. the phone's view following the laptop).
            app.note_session_created(*session_id, *parent_session_id, client_tx);
            // Early return: the generic dispatch would push text to the chat
            // history for a session the user never opened.
            return Ok(());
        }
        DaemonMessageType::Session {
            session_id: Some(session_id),
            event: SessionEvent::SessionAttached,
            ..
        } => {
            app.handle_session_attached(*session_id);
        }
        DaemonMessageType::Session {
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
        DaemonMessageType::Sessions { sessions } => {
            // The Sessions handler manages the full lifecycle and should not
            // fall through to the generic dispatch (which would duplicate
            // the summary output).
            return app.handle_sessions(sessions, client_tx);
        }
        DaemonMessageType::Session {
            session_id: Some(session_id),
            event: SessionEvent::SessionDeleted,
            ..
        } => {
            app.handle_session_deleted(*session_id);
        }
        DaemonMessageType::Session {
            session_id: Some(session_id),
            event: SessionEvent::SessionDeleteFailed { error },
            ..
        } => {
            app.handle_session_delete_failed(*session_id, error);
        }
        DaemonMessageType::Session {
            event: SessionEvent::SessionFailed {
                operation, error, ..
            },
            ..
        } => {
            app.error = Some(format!("[daemon] {operation} failed: {error}"));
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
        }
        // ── AI Provider Accounts ──────────────────────────
        DaemonMessageType::Accounts { accounts } => {
            app.handle_accounts(accounts);
            // Don't return early here — fall through to dispatch_daemon_message
            // which will push the account list to the chat history so the
            // user sees the response to their `/account` command.
        }
        DaemonMessageType::AccountListFailed { error } => {
            app.error = Some(format!("[daemon] failed to list accounts: {error}"));
            return Ok(());
        }
        DaemonMessageType::AccountAdded { name } => {
            app.status = Some(format!("[daemon] account added: {name}"));
            app.pending.send(client_tx, ClientMessageType::ListAccounts);
        }
        DaemonMessageType::AccountAddFailed { name, error } => {
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
        DaemonMessageType::AccountRemoved { name } => {
            app.status = Some(format!("[daemon] account removed: {name}"));
            app.ai_providers.remove_account(name);
            app.pending.send(client_tx, ClientMessageType::ListAccounts);
        }
        DaemonMessageType::AccountRemoveFailed { name, error } => {
            app.error = Some(format!("[daemon] failed to remove account {name}: {error}"));
        }
        // A credential mutation does not carry the updated account list, so
        // re-request it: the accounts page renders `has_credential` per
        // account, and without a refresh it would keep showing the stale
        // pre-credential state until the user leaves and re-enters the page.
        DaemonMessageType::CredentialAdded { service } => {
            app.status = Some(format!("[daemon] credential added: {service}"));
            // The daemon accepted the unlock key this credential carried —
            // record it per-daemon on CONFIRMED success only (never on send).
            record_confirmed_unlock_key(app, resolved.as_ref());
            app.pending.send(client_tx, ClientMessageType::ListAccounts);
        }
        DaemonMessageType::CredentialRemoved { service } => {
            app.status = Some(format!("[daemon] credential removed: {service}"));
            app.pending.send(client_tx, ClientMessageType::ListAccounts);
        }
        // ACL enrollment feedback: the direct reply says whether THIS add
        // worked; the broadcast tells every connected client the new total
        // (this client included, so one status line suffices per event).
        DaemonMessageType::AclAddResult { ok, message } => {
            if *ok {
                app.status = Some(format!("[daemon] {message}"));
            } else {
                app.status = Some(format!("[daemon] acl add failed: {message}"));
            }
        }
        DaemonMessageType::AclUpdated { clients } => {
            app.status = Some(format!(
                "[daemon] ACL updated — {clients} authorized client(s)"
            ));
        }

        DaemonMessageType::Session {
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
                    // Reuse the allocation: `working_dir` is only borrowed
                    // from the message here.
                    display.working_dir.clone_from(working_dir);
                    display.progress_dirty = true;
                }
                app.attached_status = Some(status.clone());
            }
            // Fall through to dispatch_daemon_message for message processing.
        }
        DaemonMessageType::Session {
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
                        progress::update(Some(tokens), Some(cw));
                    }
                }
            }
            // Fall through to dispatch_daemon_message.
        }
        DaemonMessageType::Session {
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
            if !route_session_update(app, reported, message.id, |app, session_id| {
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
                return Ok(());
            }
            // Attached session (or a targeted/connection-level reply): fall
            // through so the user's own `/model` command still prints its
            // confirmation to the status line.
        }
        DaemonMessageType::Session {
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
            if !route_session_update(app, reported, message.id, |_, _| {}) {
                tracing::debug!(
                    session_id = reported,
                    %model,
                    error_len = error.len(),
                    "suppressing status feedback for background session's model selection failure",
                );
                return Ok(());
            }
            // Attached session (or a targeted/connection-level reply): fall
            // through so the generic dispatch writes the `[daemon] failed
            // to select model …` error line.
        }
        DaemonMessageType::Session {
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
            if !route_session_update(app, reported, message.id, |app, session_id| {
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
                return Ok(());
            }
            // Attached session (or a targeted/connection-level reply): fall
            // through so the user's own `/reasoning` command still prints
            // its confirmation to the status line.
        }
        DaemonMessageType::Session {
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
            if !route_session_update(app, reported, message.id, |app, session_id| {
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
                return Ok(());
            }
            // Attached session (or a targeted/connection-level reply): the
            // user's own `/reasoning` command failed — surface the
            // rejection notice and fall through so the generic dispatch
            // records the error as well.
            tracing::warn!(%effort, error_len = error.len(), "reasoning effort rejected by daemon");
            app.status = Some(format!("reasoning effort rejected: {error}"));
        }
        DaemonMessageType::Session {
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
            if !route_session_update(app, reported, message.id, |app, session_id| {
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
                return Ok(());
            }
            // Attached session (or a targeted/connection-level reply): fall
            // through so the user's own `/account` command still prints
            // its confirmation to the status line.
        }
        DaemonMessageType::Session {
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
        DaemonMessageType::Session {
            session_id: Some(session_id),
            event: SessionEvent::SessionWorkingDirSet { path },
            ..
        } => {
            app.handle_session_working_dir_set(*session_id, path);
        }
        DaemonMessageType::Session {
            session_id: Some(session_id),
            event: SessionEvent::SessionTitleSet { title },
            ..
        } => {
            app.handle_session_title_set(*session_id, title);
        }
        // TokenUsageUpdate is dispatched through the generic handler below.
        DaemonMessageType::Session {
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

        DaemonMessageType::Models {
            models,
            selected_model,
        } => {
            // A `Models` reply that resolved a pending `ListModels` slot is the
            // answer to OUR model-list request, so it populates the selector
            // popup and must NOT fall through to the generic dispatch (which
            // would print the whole list into the chat history). Correlating on
            // the request id replaces the old "is the selector open?" guess:
            // the requester is known structurally.
            if is_list_models_reply(resolved.as_ref()) {
                // Prefer the daemon's reported selection, falling back to the
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
            // Not our reply (no pending ListModels slot): fall through to
            // dispatch_daemon_message so `/model` keeps printing the list into
            // the chat history.
        }
        DaemonMessageType::ModelsFailed { error } => {
            if is_list_models_reply(resolved.as_ref()) {
                tracing::warn!(
                    error_len = error.len(),
                    "model selector: failed to list models"
                );
                app.model_selector.apply_error(error.clone());
                return Ok(());
            }
            // Not our reply: fall through to the generic error handling.
        }

        // ── S4: /refresh-models replies + catalog updates ────────────────
        DaemonMessageType::ModelsRefreshed {
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
        DaemonMessageType::ModelsRefreshFailed { error } => {
            tracing::warn!(error_len = error.len(), "refresh-models failed");
            app.error = Some(format!("[daemon] refresh-models failed: {error}"));
            return Ok(());
        }
        // ── S5: /mcp replies ────────────────────────────────────────────
        // Bare `/mcp`'s status list AND a successful `/mcp reconnect` both
        // arrive as McpStatus. Render one server per line; the status bar
        // reserves the needed rows (status_error_height counts newlines), so
        // the whole list is visible rather than wrapped to one line.
        DaemonMessageType::McpStatus {
            servers,
            project_root,
            project_trusted,
            ignored_project_servers,
        } => {
            let mut lines = Vec::new();
            if servers.is_empty() {
                lines.push("no MCP servers configured".to_string());
            } else {
                lines.push(format!("MCP servers ({})", servers.len()));
                for server in servers {
                    lines.push(format!("[{}] {}", server.tier, server.summary()));
                }
            }
            if let Some(root) = project_root {
                let state = if *project_trusted {
                    "trusted"
                } else {
                    "UNTRUSTED (use /mcp trust)"
                };
                lines.push(format!("project root: {root} ({state})"));
            }
            if !ignored_project_servers.is_empty() {
                lines.push(format!(
                    "{} project server(s) ignored (untrusted): {}",
                    ignored_project_servers.len(),
                    ignored_project_servers.join(", ")
                ));
            }
            app.status = Some(lines.join("\n"));
            return Ok(());
        }
        DaemonMessageType::McpTrustUpdated {
            root: _,
            trusted,
            message,
        } => {
            let _ = trusted;
            app.status = Some(message.clone());
            return Ok(());
        }
        DaemonMessageType::McpTrustList { roots } => {
            if roots.is_empty() {
                app.status = Some("no trusted project MCP roots".to_string());
            } else {
                let mut lines = vec![format!("trusted project MCP roots ({})", roots.len())];
                for root in roots {
                    lines.push(root.clone());
                }
                app.status = Some(lines.join("\n"));
            }
            return Ok(());
        }
        DaemonMessageType::McpReconnectFailed { slug, error } => {
            tracing::warn!(%slug, error_len = error.len(), "mcp reconnect failed");
            app.error = Some(format!("[daemon] mcp reconnect {slug} failed: {error}"));
            return Ok(());
        }
        // A successful `/mcp reload` arrives as McpReloaded: the summary line
        // plus the refreshed per-server list (rendered the same way McpStatus
        // is — one server per line so the whole set is visible).
        DaemonMessageType::McpReloaded { summary, servers } => {
            let mut lines = vec![summary.clone()];
            for server in servers {
                lines.push(server.summary());
            }
            app.status = Some(lines.join("\n"));
            return Ok(());
        }
        DaemonMessageType::McpReloadFailed { error } => {
            tracing::warn!(error_len = error.len(), "mcp reload failed");
            app.error = Some(format!("[daemon] mcp reload failed: {error}"));
            return Ok(());
        }
        DaemonMessageType::CatalogUpdated { providers } => {
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
        DaemonMessageType::ShuttingDown => {
            tracing::info!("daemon announced shutdown; quitting");
            app.should_quit = true;
            app.quit_message = Some("the server is shutting down".to_string());
            return Ok(());
        }
        DaemonMessageType::Evicted => {
            tracing::warn!("evicted by the daemon for lag; quitting");
            app.should_quit = true;
            app.quit_message = Some(
                "disconnected by the daemon: evicted for falling behind the streaming lag limit"
                    .to_string(),
            );
            return Ok(());
        }
        // A successful unlock (explicit /unlock or the connect-time
        // auto-unlock) means the daemon ACCEPTED the pending key — record it
        // per-daemon on confirmed success only. Latch the keystore as
        // UNLOCKED (clears the persistent lock banner). Deliberately no early
        // return: the generic dispatch below still emits the "keystore
        // unlocked" status.
        DaemonMessageType::Unlocked => {
            app.keystore_locked = false;
            record_confirmed_unlock_key(app, resolved.as_ref());
        }
        // Targeted reply to our `BindKeystore`: the unbound daemon ADOPTED the
        // fresh key we minted and ran the shared unlock tail, so from the
        // client's perspective this is exactly an `Unlocked` — clear the lock
        // banner and record the pending (minted) key per-daemon. The key was
        // already persisted pre-send by `bind_fresh_daemon`, so the re-record
        // here is a no-op-safe rewrite that keeps the pending-confirm flow
        // uniform with the Unlock/AddCredential paths. Deliberately no early
        // return: the generic dispatch below still emits the "keystore bound
        // and unlocked" status.
        DaemonMessageType::Bound => {
            app.keystore_locked = false;
            record_confirmed_unlock_key(app, resolved.as_ref());
        }
        // The daemon (re-)locked its keystore (/lock) or a freshly-connecting
        // client latched the subscribe-time lock-state push: set the
        // persistent lock flag so the banner reappears. An `Unlock` that
        // auto-locked temporarily is also confirmed here (a failed unlock
        // does NOT change lock state, so the subscribe-time `Locked` push —
        // not a transition broadcast — is what a locked daemon sends a fresh
        // client). No early return: the generic dispatch still prints the
        // "keystore locked" status.
        DaemonMessageType::Locked => {
            app.keystore_locked = true;
        }
        // The daemon REJECTED the pending unlock key (Unlock path). Drop the
        // pending key (zeroized) so a later, unrelated confirmation cannot
        // attribute this rejection back — but do NOT touch the known_servers
        // record: a rejection is not proof the key is bad (the daemon maps
        // transient failures onto the same error), and the binding is
        // TOFU-immortal so a confirmed record can never be wrong. The stored
        // key is replaced only via the explicit re-pair path
        // (`KnownServers::remove(addr)`). Keep the lock flag latched locked
        // (a rejection means the daemon is still locked). No early return:
        // the generic dispatch still surfaces the error text.
        DaemonMessageType::LockedError { .. } => {
            app.keystore_locked = true;
            discard_rejected_unlock_key(app, &mut resolved);
        }
        // Same pending-key discipline for a rejected AddCredential: drop the
        // in-flight key, leave the store alone.
        DaemonMessageType::CredentialAddFailed { .. } => {
            discard_rejected_unlock_key(app, &mut resolved);
        }
        // Verify-only operation against a daemon whose keystore has NO
        // binding yet (either the subscribe-time lock-state push or the reply
        // to the connect-time auto-unlock attempt). The daemon has no
        // credentials available, so latch the lock-ish banner, and — exactly
        // like LockedError — drop the pending key: it was a verify attempt
        // that can never succeed against a nonexistent binding. Then AUTO-
        // BIND once per connection: `bind_fresh_daemon` mints a fresh CSPRNG
        // key, records it into known_servers PRE-SEND (mandatory — an unbound
        // daemon adopts whatever arrives first, so the record cannot be
        // wrong), and hands us the `BindKeystore` message. The daemon replies
        // `Bound`, which the arm above treats like `Unlocked`.
        DaemonMessageType::KeystoreUnbound { .. } => {
            app.keystore_locked = true;
            // The stale verify key belongs to THIS frontend's pending-key
            // lifecycle: drop it (zeroized) BEFORE the shared state machine
            // runs, so the minted bind key can be held pending afterwards.
            discard_rejected_unlock_key(app, &mut resolved);
            // Distinct guidance: this is not "wrong key" but "never bound" —
            // the fix is automatic, not something the user must do.
            app.status = Some(
                "keystore not initialized — a binding will be created automatically".to_string(),
            );
            // Bind-loop guard: a second `KeystoreUnbound` after our bind was
            // sent means the confirmation was lost or the daemon re-keyed —
            // surface an error, leave the connection as-is.
            if !trigger_keystore_auto_bind(app, client_tx) {
                app.error = Some(
                    "[daemon] keystore still unbound after bind attempt — reconnect to retry"
                        .to_string(),
                );
            }
        }
        // The daemon's authoritative keystore STATUS (subscribe-time push and
        // every transition). This is the signal that lets a first-run client
        // with NO key discover the keystore is `Unbound` and auto-bind — the
        // core of the fix: the client no longer has to guess from an operation
        // reply. No early return: the generic dispatch still prints the status.
        DaemonMessageType::Keystore { state } => match state {
            KeystoreState::Unbound => {
                app.keystore_locked = true;
                // The `KeystoreUnbound` reply to a connect-time auto-unlock
                // attempt may ALREADY have triggered the bind; this push
                // reports the SAME unbound fact, so only act when no bind is
                // in flight — otherwise the latch would surface a spurious
                // "still unbound" error and drop the minted pending key.
                if !app.keystore_auto_bind.attempted() {
                    discard_rejected_unlock_key(app, &mut resolved);
                    app.status = Some(
                        "keystore not initialized — a binding will be created automatically"
                            .to_string(),
                    );
                    let _ = trigger_keystore_auto_bind(app, client_tx);
                }
            }
            KeystoreState::Locked => {
                app.keystore_locked = true;
            }
            KeystoreState::Unlocked => {
                app.keystore_locked = false;
                record_confirmed_unlock_key(app, resolved.as_ref());
            }
        },
        DaemonMessageType::Session {
            session_id,
            event:
                SessionEvent::SessionFlagsChanged {
                    pinned,
                    archived_at,
                },
            ..
        } => {
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
            return Ok(());
        }
        _ => {}
    }

    // Dispatch remaining variants through the generic turn-event handler.
    dispatch_daemon_message(message, app);
    Ok(())
}

/// Trigger the once-per-connection auto-bind of an unbound daemon. The bind
/// POLICY (mint + pre-send record + latch + suppression) lives in the shared
/// `choreo_client_core` state machine (`attempt_keystore_auto_bind`) —
/// exactly like the underlying latch — so the TUI and GUI cannot drift on it;
/// this wrapper only maps the outcome to the TUI's status/error surfaces and
/// performs the send.
///
/// Returns `true` when the attempt was made (or the failure was surfaced),
/// and `false` when the bind-loop guard suppressed it because a bind was
/// already in flight on this connection.
fn trigger_keystore_auto_bind(
    app: &mut App,
    client_tx: &crossbeam_channel::Sender<ClientMessage>,
) -> bool {
    match attempt_keystore_auto_bind(&mut app.keystore_auto_bind, &app.connection_addr) {
        AutoBindAttempt::Bind { key, msg } => {
            // The minted key rides the request's pending context so the `Bound`
            // confirmation records it through the SAME path as an `Unlocked` —
            // one confirm flow for all senders.
            let id = app.pending.send(client_tx, msg);
            app.pending
                .set_context(id, PendingContext::UnlockKey(key.to_vec()));
            true
        }
        // Bind-loop guard: already attempted on this connection. The caller
        // (the `KeystoreUnbound` arm) surfaces its own reconnect-to-retry
        // error; the `Keystore { Unbound }` push pre-guards with `attempted()`
        // so this arm never fires there.
        AutoBindAttempt::Suppressed => false,
        // Persist failure (refused pre-send) or store errors: the daemon
        // stays unbound and locked; the user can reconnect to retry.
        AutoBindAttempt::Failed { error } => {
            app.error = Some(format!("[error] auto-bind failed: {error}"));
            true
        }
    }
}

/// Record the daemon-confirmed unlock key per-daemon, exactly once.
///
/// The key rides the request's pending context ([`PendingContext::UnlockKey`]),
/// attached when the `Unlock`/`AddCredential`/`BindKeystore` is SENT (see
/// `run_app`'s auto-unlock, the chat/credential handlers, and the auto-bind
/// path). A daemon that accepts the key replies `Unlocked` / `CredentialAdded`
/// / `Bound`, resolving that slot — and only then do we persist the key into
/// the daemon's `known_servers` entry. This is the TOFU core of the per-daemon
/// keystore design: a key the daemon REJECTS (misbound keystore) is never
/// recorded.
fn record_confirmed_unlock_key(app: &mut App, resolved: Option<&Pending>) {
    // The key lives in the resolved slot's context. A reply with no UnlockKey
    // context (a broadcast, or a request that carried no key) records nothing.
    let Some(key) = resolved.and_then(|pending| pending.context.unlock_key()) else {
        return;
    };
    match record_unlock_key(&app.connection_addr, key) {
        Ok(()) => {
            tracing::info!(
                addr = %app.connection_addr,
                "recorded daemon-confirmed unlock key per-daemon"
            );
        }
        Err(e) => {
            app.error = Some(format!("[error] failed to record unlock key: {e}"));
        }
    }
}

/// The daemon REJECTED the pending unlock key (an `Unlock` or `AddCredential`
/// failure). Drop the in-flight key — zeroized, since it is secret material —
/// so a later, unrelated confirmation cannot attribute it. Deliberately does
/// NOT delete the `known_servers` record: see the survivor-semantics rationale
/// in `choreo-client-core` (`resolve_keystore_key`) — the stored key may be a
/// valid confirmed key (the daemon reports transient failures through the
/// same error), and manual re-pair (`remove(addr)`) is the recovery path for
/// a genuinely wrong one. Takes the key out of the resolved slot so an
/// in-process reply cannot linger after the rejection.
fn discard_rejected_unlock_key(app: &mut App, resolved: &mut Option<Pending>) {
    // Take the slot out so it is dropped HERE; `PendingContext::drop` zeroizes
    // the held UnlockKey, so the secret is wiped on the rejection path exactly
    // as it is on every other exit path (resolve, timeout, connection reset). A
    // reply whose slot carried no key (or no slot at all) has nothing to
    // discard.
    let Some(pending) = resolved.take() else {
        return;
    };
    if !matches!(pending.context, PendingContext::UnlockKey(..)) {
        return;
    }
    tracing::info!(
        addr = %app.connection_addr,
        "daemon rejected the presented unlock key; the known_servers record is kept"
    );
    // `pending` drops here, zeroizing the key via its `Drop` impl.
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_util::test_app;

    /// Drive one daemon message through `handle_daemon_message`. The generic
    /// dispatch's status/error handling needs a sender, but none of the
    /// lock-state messages send anything, so a disconnected sender works.
    fn dispatch(message: DaemonMessage, app: &mut App) {
        let (tx, _rx) = crossbeam_channel::unbounded::<ClientMessage>();
        handle_daemon_message(message, app, &tx).expect("handle_daemon_message");
    }

    #[test]
    fn app_defaults_to_keystore_locked() {
        // Safest default: assume locked until the daemon reports otherwise.
        assert!(App::new().keystore_locked);
    }

    #[test]
    fn failed_reply_surfaces_an_error_line() {
        // A terminal `Failed` reply (the request's one id-bearing reply, for a
        // request with no richer session-scoped failure shape — e.g. an empty
        // `/undo`) must reach the user as an error, not be silently dropped.
        // The correlation id rides the envelope; the payload becomes the error.
        let mut app = test_app();
        dispatch(
            DaemonMessage::reply(
                1,
                DaemonMessageType::Failed {
                    kind: MessageKind::Undo,
                    error: "nothing to undo".into(),
                },
            ),
            &mut app,
        );
        assert!(
            app.error
                .as_deref()
                .is_some_and(|e| e.contains("nothing to undo")),
            "a Failed reply must set the error line, got {:?}",
            app.error
        );
    }

    #[test]
    fn accepted_reply_is_a_silent_success() {
        // `Accepted` carries no state of its own (the mutation's broadcast is
        // the state), so it must not write an error or a status line.
        let mut app = test_app();
        dispatch(
            DaemonMessage::reply(
                2,
                DaemonMessageType::Accepted {
                    kind: MessageKind::SetSessionPinned,
                },
            ),
            &mut app,
        );
        assert!(app.error.is_none(), "Accepted must not surface an error");
    }

    #[test]
    fn unlocked_message_clears_the_lock_flag() {
        let mut app = App::new(); // fresh App starts locked
        assert!(app.keystore_locked, "starts locked");
        dispatch(
            DaemonMessage::broadcast(DaemonMessageType::Unlocked),
            &mut app,
        );
        assert!(
            !app.keystore_locked,
            "Unlocked must clear the persistent lock flag"
        );
    }

    #[test]
    fn locked_message_latches_true_from_unlocked() {
        let mut app = test_app();
        app.keystore_locked = false; // simulate an unlocked daemon
        dispatch(
            DaemonMessage::broadcast(DaemonMessageType::Locked),
            &mut app,
        );
        assert!(
            app.keystore_locked,
            "Locked (transition or subscribe push) must set the flag"
        );
    }

    #[test]
    fn locked_error_latches_true_and_rejects_pending_key() {
        let mut app = test_app();
        app.keystore_locked = false;
        // An unconfirmed key (e.g. the optimistic fresh-key record) is being
        // rejected: it must be reverted and the lock flag kept latched. The
        // key rides the pending slot's context, so a TARGETED (id-bearing)
        // rejection resolves it.
        let (tx, _rx) = crossbeam_channel::unbounded::<ClientMessage>();
        let id = app.pending.send(
            &tx,
            ClientMessageType::Unlock {
                private_key: vec![7u8; 32],
            },
        );
        app.pending
            .set_context(id, PendingContext::UnlockKey(vec![7u8; 32]));
        handle_daemon_message(
            DaemonMessage::reply(
                id,
                DaemonMessageType::LockedError {
                    error: "unlock key does not match the keystore binding".into(),
                },
            ),
            &mut app,
            &tx,
        )
        .unwrap();
        assert!(
            app.keystore_locked,
            "a rejected unlock means the daemon is still locked"
        );
        // The rejection consumes the pending slot so a later, unrelated
        // confirmation cannot attribute it (see discard_rejected_unlock_key).
        assert!(app.pending.is_empty(), "the rejected slot is consumed");
    }

    #[test]
    fn locked_broadcast_does_not_touch_a_pending_unlock_key() {
        // A `Locked` broadcast (subscribe-time push or /lock transition) is
        // NOT a key rejection: it must latch the flag but leave the pending
        // auto-unlock slot alone, so the Unlock reply (Unlocked/LockedError) —
        // which arrives next on the wire — decides the key's fate.
        let mut app = test_app();
        app.keystore_locked = true;
        let (tx, _rx) = crossbeam_channel::unbounded::<ClientMessage>();
        let id = app.pending.send(
            &tx,
            ClientMessageType::Unlock {
                private_key: vec![9u8; 32],
            },
        );
        app.pending
            .set_context(id, PendingContext::UnlockKey(vec![9u8; 32]));
        handle_daemon_message(
            DaemonMessage::broadcast(DaemonMessageType::Locked),
            &mut app,
            &tx,
        )
        .unwrap();
        assert!(app.keystore_locked);
        assert_eq!(
            app.pending.len(),
            1,
            "a Locked broadcast must not consume the pending unlock slot"
        );
    }

    #[test]
    fn unlock_then_lock_roundtrip_updates_the_flag() {
        // A full lock lifecycle: unlock clears the banner, /lock re-latches it.
        let mut app = test_app();
        dispatch(
            DaemonMessage::broadcast(DaemonMessageType::Unlocked),
            &mut app,
        );
        assert!(!app.keystore_locked);
        dispatch(
            DaemonMessage::broadcast(DaemonMessageType::Locked),
            &mut app,
        );
        assert!(app.keystore_locked);
    }

    // ── auto-bind (Bound / KeystoreUnbound) tests ──────────────────────

    #[test]
    fn bound_message_clears_lock_and_records_pending_key() {
        let (_dir, _guard) = choreo_client_core::test_support::isolate_config();
        let mut app = test_app();
        app.keystore_locked = true;
        app.connection_addr = "bound-test:1".to_string();

        // A `BindKeystore` whose reply (`Bound`) confirms the carried key: the
        // key rides the request's pending context and is recorded when the
        // targeted `Bound` resolves the slot.
        let (tx, _rx) = crossbeam_channel::unbounded::<ClientMessage>();
        let id = app
            .pending
            .send(&tx, ClientMessageType::BindKeystore { key: vec![3u8; 32] });
        app.pending
            .set_context(id, PendingContext::UnlockKey(vec![3u8; 32]));
        handle_daemon_message(
            DaemonMessage::reply(id, DaemonMessageType::Bound),
            &mut app,
            &tx,
        )
        .unwrap();

        assert!(!app.keystore_locked, "Bound must clear the lock flag");
        assert!(app.pending.is_empty(), "pending key is consumed");
        let store = choreo_client_core::KnownServers::load().unwrap();
        assert_eq!(
            store.unlock_key("bound-test:1").unwrap(),
            Some([3u8; 32]),
            "the confirmed key is recorded per-daemon"
        );
    }

    #[test]
    fn keystore_unbound_auto_binds_once_per_connection() {
        let (_dir, _guard) = choreo_client_core::test_support::isolate_config();
        let mut app = test_app();
        app.keystore_locked = false; // daemon had been thought unlocked
        app.connection_addr = "unbound-test:1".to_string();

        let (tx, rx) = crossbeam_channel::unbounded::<ClientMessage>();
        handle_daemon_message(
            DaemonMessage::broadcast(DaemonMessageType::KeystoreUnbound {
                error: "keystore has no binding".into(),
            }),
            &mut app,
            &tx,
        )
        .unwrap();

        assert!(app.keystore_locked, "unbound latches the lock-ish banner");
        assert!(app.keystore_auto_bind.attempted(), "the bind is latched");
        // Exactly one BindKeystore sent, carrying the key that was recorded
        // into known_servers PRE-SEND. The minted key rides the bind's pending
        // context (checked indirectly by the store pre-send record below).
        let framed = rx.try_recv().unwrap();
        let ClientMessageType::BindKeystore { key } = framed.inner else {
            panic!("auto-bind must send BindKeystore");
        };
        assert!(rx.try_recv().is_err(), "exactly one bind message");
        let store = choreo_client_core::KnownServers::load().unwrap();
        assert_eq!(
            store.unlock_key("unbound-test:1").unwrap(),
            Some(key.as_slice().try_into().unwrap()),
            "the minted key is recorded pre-send"
        );

        // A SECOND KeystoreUnbound on the same connection must NOT re-bind:
        // surface an error instead (bind-loop guard).
        app.error = None;
        handle_daemon_message(
            DaemonMessage::broadcast(DaemonMessageType::KeystoreUnbound {
                error: "still unbound".into(),
            }),
            &mut app,
            &tx,
        )
        .unwrap();
        assert!(rx.try_recv().is_err(), "no second bind attempt");
        assert!(
            app.error.is_some(),
            "the repeat unbound report is surfaced as an error"
        );
    }

    #[test]
    fn auto_bind_flow_end_to_end_unlocks_on_bound() {
        // The full connect-time unbound flow: KeystoreUnbound mints+sends the
        // bind, the daemon replies Bound to that request's id, and the
        // connection ends up unlocked with the minted key recorded.
        let (_dir, _guard) = choreo_client_core::test_support::isolate_config();
        let mut app = test_app();
        app.connection_addr = "e2e-bind:1".to_string();

        let (tx, rx) = crossbeam_channel::unbounded::<ClientMessage>();
        handle_daemon_message(
            DaemonMessage::broadcast(DaemonMessageType::KeystoreUnbound {
                error: "unbound".into(),
            }),
            &mut app,
            &tx,
        )
        .unwrap();
        assert!(app.keystore_locked);
        // Recover the id the bind was sent under from the captured frame (the
        // same id `pending.send` recorded), so the reply targets the slot.
        let framed = rx.try_recv().unwrap();
        let bind_id = framed.id;
        let ClientMessageType::BindKeystore { key: minted } = framed.inner else {
            panic!("expected BindKeystore");
        };

        handle_daemon_message(
            DaemonMessage::reply(bind_id, DaemonMessageType::Bound),
            &mut app,
            &tx,
        )
        .unwrap();
        assert!(!app.keystore_locked, "Bound unlocks the daemon");
        assert!(app.pending.is_empty());
        let store = choreo_client_core::KnownServers::load().unwrap();
        assert_eq!(
            store.unlock_key("e2e-bind:1").unwrap(),
            Some(minted.as_slice().try_into().unwrap()),
            "the minted key is the recorded key"
        );
    }

    #[test]
    fn keystore_unbound_status_push_auto_binds() {
        // The server-authoritative signal: a `Keystore { Unbound }` push (sent
        // at subscribe time) triggers the SAME auto-bind as the
        // `KeystoreUnbound` reply — this is what lets a FIRST-RUN client with
        // no key bind a fresh daemon, the bug this change fixes.
        let (_dir, _guard) = choreo_client_core::test_support::isolate_config();
        let mut app = test_app();
        app.connection_addr = "push-bind:1".to_string();

        let (tx, rx) = crossbeam_channel::unbounded::<ClientMessage>();
        handle_daemon_message(
            DaemonMessage::broadcast(DaemonMessageType::Keystore {
                state: KeystoreState::Unbound,
            }),
            &mut app,
            &tx,
        )
        .unwrap();

        assert!(app.keystore_locked, "unbound latches the lock banner");
        assert!(app.keystore_auto_bind.attempted(), "bind latched");
        let framed = rx.try_recv().unwrap();
        let ClientMessageType::BindKeystore { key } = framed.inner else {
            panic!("unbound status push must auto-bind");
        };
        let store = choreo_client_core::KnownServers::load().unwrap();
        assert_eq!(
            store.unlock_key("push-bind:1").unwrap(),
            Some(key.as_slice().try_into().unwrap()),
            "the minted key is recorded pre-send"
        );

        // A SECOND unbound push on the same connection is inert: no re-bind
        // and no spurious "still unbound" error.
        app.error = None;
        handle_daemon_message(
            DaemonMessage::broadcast(DaemonMessageType::Keystore {
                state: KeystoreState::Unbound,
            }),
            &mut app,
            &tx,
        )
        .unwrap();
        assert!(rx.try_recv().is_err(), "no second bind attempt");
        assert!(app.error.is_none(), "duplicate unbound push is silent");
    }

    #[test]
    fn keystore_locked_and_unlocked_status_push_latch() {
        // The status push latches the banner in both directions.
        let mut app = test_app();
        app.keystore_locked = false;
        let (tx, _rx) = crossbeam_channel::unbounded::<ClientMessage>();
        handle_daemon_message(
            DaemonMessage::broadcast(DaemonMessageType::Keystore {
                state: KeystoreState::Locked,
            }),
            &mut app,
            &tx,
        )
        .unwrap();
        assert!(app.keystore_locked);
        handle_daemon_message(
            DaemonMessage::broadcast(DaemonMessageType::Keystore {
                state: KeystoreState::Unlocked,
            }),
            &mut app,
            &tx,
        )
        .unwrap();
        assert!(!app.keystore_locked);
    }
}
