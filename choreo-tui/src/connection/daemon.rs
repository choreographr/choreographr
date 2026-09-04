use super::{SessionUpdateRouting, route_session_update};
use crate::state::{App, Page, ProviderInfo, merge_token_usage};
use crate::terminal_progress;
use choreo_client_core::{ClientError, dispatch_daemon_message, record_unlock_key};
use choreo_proto::{ClientMessage, DaemonMessage, RefreshStatus, SessionEvent};
use zeroize::Zeroize;

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
            // The daemon accepted the unlock key this credential carried —
            // record it per-daemon on CONFIRMED success only (never on send).
            record_confirmed_unlock_key(app);
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
        // A successful unlock (explicit /unlock or the connect-time
        // auto-unlock) means the daemon ACCEPTED the pending key — record it
        // per-daemon on confirmed success only. Latch the keystore as
        // UNLOCKED (clears the persistent lock banner). Deliberately no early
        // return: the generic dispatch below still emits the "keystore
        // unlocked" status.
        DaemonMessage::Unlocked => {
            app.keystore_locked = false;
            record_confirmed_unlock_key(app);
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
        DaemonMessage::Bound => {
            app.keystore_locked = false;
            record_confirmed_unlock_key(app);
        }
        // The daemon (re-)locked its keystore (/lock) or a freshly-connecting
        // client latched the subscribe-time lock-state push: set the
        // persistent lock flag so the banner reappears. An `Unlock` that
        // auto-locked temporarily is also confirmed here (a failed unlock
        // does NOT change lock state, so the subscribe-time `Locked` push —
        // not a transition broadcast — is what a locked daemon sends a fresh
        // client). No early return: the generic dispatch still prints the
        // "keystore locked" status.
        DaemonMessage::Locked => {
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
        DaemonMessage::LockedError { .. } => {
            app.keystore_locked = true;
            discard_rejected_unlock_key(app);
        }
        // Same pending-key discipline for a rejected AddCredential: drop the
        // in-flight key, leave the store alone.
        DaemonMessage::CredentialAddFailed { .. } => discard_rejected_unlock_key(app),
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
        DaemonMessage::KeystoreUnbound { .. } => {
            app.keystore_locked = true;
            // The stale verify key belongs to THIS frontend's pending-key
            // lifecycle: drop it (zeroized) BEFORE the shared state machine
            // runs, so the minted bind key can be held pending afterwards.
            discard_rejected_unlock_key(app);
            // Distinct guidance: this is not "wrong key" but "never bound" —
            // the fix is automatic, not something the user must do.
            app.status = Some(
                "keystore not initialized — a binding will be created automatically".to_string(),
            );
            // The once-per-connection latch and the mint+pre-send-record live
            // in the shared `choreo_client_core` state machine so the TUI and
            // GUI cannot drift on the bind-loop policy.
            match app.keystore_auto_bind.on_unbound(&app.connection_addr) {
                Ok(Some((key, msg))) => {
                    tracing::info!(
                        addr = %app.connection_addr,
                        "auto-binding unbound daemon with a fresh key"
                    );
                    // The minted key is held pending so the `Bound`
                    // confirmation records it through the SAME path as an
                    // `Unlocked` — one confirm flow for all three senders.
                    app.pending_unlock_key = Some(key.to_vec());
                    let _ = client_tx.send(msg);
                }
                // Bind-loop guard: a second `KeystoreUnbound` after our bind
                // was sent means the confirmation was lost or the daemon
                // re-keyed — surface an error, leave the connection as-is.
                Ok(None) => {
                    app.error = Some(
                        "[daemon] keystore still unbound after bind attempt — reconnect to retry"
                            .to_string(),
                    );
                }
                // Persist failure (refused pre-send) or store errors: the
                // daemon stays unbound and locked; the user can reconnect
                // to retry.
                Err(e) => {
                    tracing::warn!(addr = %app.connection_addr, %e, "auto-bind failed");
                    app.error = Some(format!("[error] auto-bind failed: {e}"));
                }
            }
        }
        _ => {}
    }

    // Dispatch remaining variants through the generic turn-event handler.
    dispatch_daemon_message(&message, app);
    Ok(())
}

/// Record the daemon-confirmed unlock key per-daemon, exactly once.
///
/// The pending key (`app.pending_unlock_key`) is stored when an `Unlock`,
/// `AddCredential`, or (auto-) `BindKeystore` is SENT (see `run_app`'s
/// auto-unlock, the chat/credential handlers, and the `KeystoreUnbound` arm).
/// A daemon that accepts the key replies `Unlocked` / `CredentialAdded` /
/// `Bound` — and only then do we persist it into the daemon's known_servers
/// entry — the TOFU core of the per-daemon keystore design: a key the daemon
/// REJECTS (misbound keystore) is never recorded.
fn record_confirmed_unlock_key(app: &mut App) {
    let Some(key) = app.pending_unlock_key.take() else {
        return;
    };
    match record_unlock_key(&app.connection_addr, &key) {
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

/// The daemon REJECTED the pending unlock key (an Unlock or AddCredential
/// failure). Drop the in-flight key — zeroized, since it is secret material —
/// so a later, unrelated confirmation cannot attribute it. Deliberately does
/// NOT delete the known_servers record: see the survivor-semantics rationale
/// in `choreo-client-core` (`resolve_keystore_key`) — the stored key may be a
/// valid confirmed key (the daemon reports transient failures through the
/// same error), and manual re-pair (`remove(addr)`) is the recovery path for
/// a genuinely wrong one.
fn discard_rejected_unlock_key(app: &mut App) {
    if let Some(mut key) = app.pending_unlock_key.take() {
        key.zeroize();
        tracing::info!(
            addr = %app.connection_addr,
            "daemon rejected the presented unlock key; the known_servers record is kept"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_util::test_app;

    /// Drive one daemon message through `handle_daemon_message`. The generic
    /// dispatch's status/error handling needs a sender, but none of the
    /// lock-state messages send anything, so a disconnected sender works.
    fn dispatch(message: DaemonMessage, app: &mut App) {
        let (tx, _rx) = std::sync::mpsc::channel::<ClientMessage>();
        handle_daemon_message(message, app, &tx).expect("handle_daemon_message");
    }

    #[test]
    fn app_defaults_to_keystore_locked() {
        // Safest default: assume locked until the daemon reports otherwise.
        assert!(App::new().keystore_locked);
    }

    #[test]
    fn unlocked_message_clears_the_lock_flag() {
        let mut app = App::new(); // fresh App starts locked
        assert!(app.keystore_locked, "starts locked");
        dispatch(DaemonMessage::Unlocked, &mut app);
        assert!(
            !app.keystore_locked,
            "Unlocked must clear the persistent lock flag"
        );
    }

    #[test]
    fn locked_message_latches_true_from_unlocked() {
        let mut app = test_app();
        app.keystore_locked = false; // simulate an unlocked daemon
        dispatch(DaemonMessage::Locked, &mut app);
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
        // rejected: it must be reverted and the lock flag kept latched.
        app.pending_unlock_key = Some(vec![7u8; 32]);
        dispatch(
            DaemonMessage::LockedError {
                error: "unlock key does not match the keystore binding".into(),
            },
            &mut app,
        );
        assert!(
            app.keystore_locked,
            "a rejected unlock means the daemon is still locked"
        );
        // The rejection drops the pending key so a later, unrelated
        // confirmation cannot attribute it (see discard_rejected_unlock_key).
        assert!(app.pending_unlock_key.is_none());
    }

    #[test]
    fn locked_broadcast_does_not_touch_a_pending_unlock_key() {
        // A `Locked` broadcast (subscribe-time push or /lock transition) is
        // NOT a key rejection: it must latch the flag but leave the pending
        // auto-unlock key alone, so the Unlock reply (Unlocked/LockedError)
        // — which arrives next on the wire — decides the key's fate.
        let mut app = test_app();
        app.keystore_locked = true;
        let key = vec![9u8; 32];
        app.pending_unlock_key = Some(key.clone());
        dispatch(DaemonMessage::Locked, &mut app);
        assert!(app.keystore_locked);
        assert_eq!(
            app.pending_unlock_key,
            Some(key),
            "Locked must not drop the pending unlock key"
        );
    }

    #[test]
    fn unlock_then_lock_roundtrip_updates_the_flag() {
        // A full lock lifecycle: unlock clears the banner, /lock re-latches it.
        let mut app = test_app();
        dispatch(DaemonMessage::Unlocked, &mut app);
        assert!(!app.keystore_locked);
        dispatch(DaemonMessage::Locked, &mut app);
        assert!(app.keystore_locked);
    }

    // ── auto-bind (Bound / KeystoreUnbound) tests ──────────────────────

    #[test]
    fn bound_message_clears_lock_and_records_pending_key() {
        let (_dir, _guard) = choreo_client_core::test_support::isolate_config();
        let mut app = test_app();
        app.keystore_locked = true;
        app.connection_addr = "bound-test:1".to_string();
        app.pending_unlock_key = Some(vec![3u8; 32]);

        // Unlike the plain `dispatch` helper, capture what the handler SENDS
        // (a Bound flow must never send anything itself).
        let (tx, rx) = std::sync::mpsc::channel::<ClientMessage>();
        handle_daemon_message(DaemonMessage::Bound, &mut app, &tx).unwrap();

        assert!(!app.keystore_locked, "Bound must clear the lock flag");
        assert!(app.pending_unlock_key.is_none(), "pending key is consumed");
        let store = choreo_client_core::KnownServers::load().unwrap();
        assert_eq!(
            store.unlock_key("bound-test:1").unwrap(),
            Some([3u8; 32]),
            "the confirmed key is recorded per-daemon"
        );
        assert!(rx.try_recv().is_err(), "Bound sends no client messages");
    }

    #[test]
    fn keystore_unbound_auto_binds_once_per_connection() {
        let (_dir, _guard) = choreo_client_core::test_support::isolate_config();
        let mut app = test_app();
        app.keystore_locked = false; // daemon had been thought unlocked
        app.connection_addr = "unbound-test:1".to_string();
        // A stale pending key (the verify attempt that just failed with
        // KeystoreUnbound) must be discarded before the bind mints its own.
        app.pending_unlock_key = Some(vec![5u8; 32]);

        let (tx, rx) = std::sync::mpsc::channel::<ClientMessage>();
        handle_daemon_message(
            DaemonMessage::KeystoreUnbound {
                error: "keystore has no binding".into(),
            },
            &mut app,
            &tx,
        )
        .unwrap();

        assert!(app.keystore_locked, "unbound latches the lock-ish banner");
        assert!(app.keystore_auto_bind.attempted(), "the bind is latched");
        assert!(app.pending_unlock_key.is_some(), "minted key held pending");
        // Exactly one BindKeystore sent, carrying the key that was recorded
        // into known_servers PRE-SEND.
        let ClientMessage::BindKeystore { key } = rx.try_recv().unwrap() else {
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
            DaemonMessage::KeystoreUnbound {
                error: "still unbound".into(),
            },
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
        // bind, the daemon replies Bound, and the connection ends up unlocked
        // with the minted key recorded.
        let (_dir, _guard) = choreo_client_core::test_support::isolate_config();
        let mut app = test_app();
        app.connection_addr = "e2e-bind:1".to_string();

        let (tx, rx) = std::sync::mpsc::channel::<ClientMessage>();
        handle_daemon_message(
            DaemonMessage::KeystoreUnbound {
                error: "unbound".into(),
            },
            &mut app,
            &tx,
        )
        .unwrap();
        assert!(app.keystore_locked);
        let minted = match rx.try_recv().unwrap() {
            ClientMessage::BindKeystore { key } => key,
            other => panic!("expected BindKeystore, got {other:?}"),
        };

        handle_daemon_message(DaemonMessage::Bound, &mut app, &tx).unwrap();
        assert!(!app.keystore_locked, "Bound unlocks the daemon");
        assert!(app.pending_unlock_key.is_none());
        let store = choreo_client_core::KnownServers::load().unwrap();
        assert_eq!(
            store.unlock_key("e2e-bind:1").unwrap(),
            Some(minted.as_slice().try_into().unwrap()),
            "the minted key is the recorded key"
        );
    }
}
