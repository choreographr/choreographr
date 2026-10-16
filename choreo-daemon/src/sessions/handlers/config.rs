//! Session-configuration handlers: model and reasoning-effort selection,
//! title / working-directory / MCP-overlay / account changes, and the tool
//! group load / unload commands.

use super::{
    DaemonCommand, DaemonMessageType, InferenceProvider, MAX_TITLE_CHARS, Path, ReplyTarget,
    RequestContext, SessionEvent, SessionMcpOverlay, SessionState, UnicodeSegmentation, WarmPolicy,
    broadcast, debug, info, io, model_reasoning_capability, mpsc, persist_session_metadata, warn,
};

/// Set the model for this session and broadcast the change.
/// Rejects invalid model names by broadcasting `ModelSelectionFailed`
/// instead of mutating state.
pub(in crate::sessions) fn handle_set_model(
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
pub(in crate::sessions) fn validate_model_via_daemon(
    model: &str,
    ctx: &RequestContext,
) -> Result<(), String> {
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

/// Set the session title, broadcasting the change to subscribers and
/// notifying the daemon so session listings reflect the new title
/// immediately.
pub(in crate::sessions) fn handle_set_title(
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
pub(in crate::sessions) fn handle_set_mcp_overlay(
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
pub(in crate::sessions) fn handle_set_working_dir(
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
pub(in crate::sessions) fn handle_load_tools(
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
pub(in crate::sessions) fn handle_unload_tools(
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
pub(in crate::sessions) fn handle_set_provider_slug(
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
pub(in crate::sessions) fn handle_set_account(
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
    // Build the client when a credential is present. The resolved grant is
    // consumed here (the reply is not reused past this point); `None` (unknown
    // account) or a `None` grant (no credential stored) leaves the provider
    // unset so the next request retries lazily.
    let new_provider = resolved.and_then(|a| {
        InferenceProvider::from_account_config(&a.config, a.auth?, &state.registry).ok()
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
pub(in crate::sessions) fn handle_set_reasoning_effort(
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
pub(in crate::sessions) fn handle_get_reasoning_effort(
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
pub(in crate::sessions) fn handle_get_state(
    reply: &mpsc::Sender<io::Result<DaemonMessageType>>,
    state: &SessionState,
    ctx: &RequestContext,
) -> bool {
    let _ = reply.send(Ok(state.session_state_message(ctx.session_id)));
    false
}
