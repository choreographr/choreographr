//! Per-request client handlers for the connection thread.
//!
//! `dispatch_client_message` matches a decoded `ClientMessage` and routes it to
//! a `handle_*` function; every handler answers its request with exactly one
//! terminal reply (see `super::ReplyHandle` / `super::ClientCtx`). Split out of
//! `connection.rs` so that file stays focused on the connection context, the
//! single-writer transport layer, and the accept/read loops — mirroring the
//! `sessions.rs` -> `sessions/handlers.rs` split.

use super::{
    ClientCtx, ClientMessage, ClientMessageType, ContextConfig, DaemonCommand, DaemonMessageType,
    MessageKind, ReplyHandle, SessionCommand, SessionEvent, debug, info, io, mpsc,
    switch_attached_session, warn,
};

/// Dispatch a decoded `ClientMessage` through the shared handler functions.
/// Returns an error only when the daemon has disconnected (caller should
/// terminate the client connection).
pub(super) fn dispatch_client_message(msg: ClientMessage, ctx: &mut ClientCtx) -> io::Result<()> {
    // Capture the request id up front: it is stamped onto every reply this
    // dispatch produces, and the payload is matched by value below.
    let ClientMessage { id, inner } = msg;
    ctx.request_id = id;
    // The request's kind tag, computed before the payload is matched by value.
    // The defensive wildcard arm (a future wire variant) needs it to build a
    // typed `Failed` reply without re-deriving from the moved payload.
    let kind = inner.kind();
    match inner {
        ClientMessageType::CreateSession {
            title,
            parent_session_id,
            working_dir,
            context_config,
            account_name,
            selected_model,
            reasoning_effort,
        } => {
            // Mint the reply handle here and hand it to the handler by value:
            // `handle_client_create_session` answers on THIS thread (it blocks
            // on the daemon round-trip), so it owns the reply obligation.
            let handle = ctx.reply_handle();
            if !handle_client_create_session(
                title,
                parent_session_id,
                working_dir,
                context_config,
                account_name,
                selected_model,
                reasoning_effort,
                ctx,
                handle,
            ) {
                return Err(io::Error::new(
                    io::ErrorKind::ConnectionAborted,
                    "daemon disconnected",
                ));
            }
        }
        ClientMessageType::AttachSession { session_id } => {
            let handle = ctx.reply_handle();
            if !handle_client_attach_session(session_id, ctx, handle) {
                return Err(io::Error::new(
                    io::ErrorKind::ConnectionAborted,
                    "daemon disconnected",
                ));
            }
        }
        ClientMessageType::ListSessions => {
            debug!("client {}: ListSessions", ctx.client_id);
            let (reply, rx) = mpsc::channel();
            let _ = ctx.daemon_tx.send(DaemonCommand::ListSessions { reply });
            let handle = ctx.reply_handle();
            match rx.recv() {
                Ok(sessions) => handle.send(DaemonMessageType::Sessions { sessions }),
                // Daemon gone: the command loop that would have produced the
                // list no longer exists, so the reply is impossible (not
                // forgotten) and the connection is being torn down.
                Err(_) => handle.abandon(),
            }
        }
        ClientMessageType::SubscribeSessionsSummary => {
            let _ = ctx
                .daemon_tx
                .send(DaemonCommand::RegisterSummarySubscriber {
                    client_id: ctx.client_id,
                    writer: ctx.writer.clone(),
                });
            // No-arg ack: nothing is fire-and-forget. The register command is
            // applied by the daemon command loop; this just confirms receipt.
            ctx.ack(kind);
        }
        ClientMessageType::UnsubscribeSessionsSummary => {
            let _ = ctx
                .daemon_tx
                .send(DaemonCommand::UnregisterSummarySubscriber {
                    client_id: ctx.client_id,
                });
            ctx.ack(kind);
        }
        ClientMessageType::RunInput { input } => {
            debug!("client {}: RunInput", ctx.client_id);
            // Hand the reply obligation to the session thread: it sends the
            // TARGETED acceptance reply (`Started` on accept, `Failed` on
            // reject) plus the unchanged broadcast stream. The daemon assigns
            // the run's `stream_id` there and reports it on `Started`.
            let target = ctx.reply_target(kind);
            if let Some(tx) = ctx.attached_session_tx.as_ref() {
                let _ = tx.send(SessionCommand::RunInput {
                    input,
                    reply: Some(target),
                });
            } else {
                target.fail("no session attached");
            }
        }
        ClientMessageType::Cancel { stream_id } => {
            debug!("client {}: Cancel id={}", ctx.client_id, stream_id);
            // Route through the daemon so it can also cancel child
            // sub-sessions without requiring a round-trip message.
            if let Some(session_id) = *ctx.attached_session_id {
                let _ = ctx.daemon_tx.send(DaemonCommand::CancelRequest {
                    session_id,
                    stream_id,
                });
            }
            // A no-arg ack so `Cancel` is not fire-and-forget: the request was
            // received (the stream's own `Cancelled` broadcast is the outcome).
            ctx.ack(kind);
        }
        ClientMessageType::Undo => {
            debug!("client {}: Undo", ctx.client_id);
            let target = ctx.reply_target(kind);
            if let Some(tx) = ctx.attached_session_tx.as_ref() {
                let _ = tx.send(SessionCommand::Undo {
                    reply: Some(target),
                });
            } else {
                target.fail("no session attached");
            }
        }
        ClientMessageType::Redo => {
            debug!("client {}: Redo", ctx.client_id);
            let target = ctx.reply_target(kind);
            if let Some(tx) = ctx.attached_session_tx.as_ref() {
                let _ = tx.send(SessionCommand::Redo {
                    reply: Some(target),
                });
            } else {
                target.fail("no session attached");
            }
        }
        ClientMessageType::ContinueGeneration => {
            debug!("client {}: ContinueGeneration", ctx.client_id);
            let target = ctx.reply_target(kind);
            if let Some(tx) = ctx.attached_session_tx.as_ref() {
                let _ = tx.send(SessionCommand::RunInput {
                    input: b"Continue.".to_vec(),
                    reply: Some(target),
                });
            } else {
                target.fail("no session attached");
            }
        }
        ClientMessageType::Ping => {
            debug!("client {}: Ping", ctx.client_id);
            ctx.reply_handle().send(DaemonMessageType::Pong);
        }
        ClientMessageType::SetModel { model } => {
            info!(
                "client {}: SetModel model={} attached={}",
                ctx.client_id,
                model,
                ctx.attached_session_tx.is_some()
            );
            let target = ctx.reply_target(kind);
            if let Some(tx) = ctx.attached_session_tx.as_ref() {
                let _ = tx.send(SessionCommand::SetModel {
                    model,
                    reply: Some(target),
                });
            } else {
                target.fail("no session attached");
            }
        }
        ClientMessageType::SetReasoningEffort { effort } => {
            info!(
                "client {}: SetReasoningEffort effort={} attached={}",
                ctx.client_id,
                effort,
                ctx.attached_session_tx.is_some()
            );
            let target = ctx.reply_target(kind);
            if let Some(tx) = ctx.attached_session_tx.as_ref() {
                let _ = tx.send(SessionCommand::SetReasoningEffort {
                    effort,
                    reply: Some(target),
                });
            } else {
                target.fail("no session attached");
            }
        }
        ClientMessageType::GetReasoningEffort => {
            if let Some(tx) = ctx.attached_session_tx {
                let (reply, rx) = mpsc::channel();
                let _ = tx.send(SessionCommand::GetReasoningEffort { reply });
                let handle = ctx.reply_handle();
                if let Ok(effort) = rx.recv() {
                    // Session-scoped reply to the attached session: carry its
                    // real id (do NOT fall back to the None sentinel).
                    handle.send(DaemonMessageType::Session {
                        session_id: *ctx.attached_session_id,
                        event: SessionEvent::ReasoningEffortSet { effort },
                    });
                } else {
                    // The session thread is gone; the connection is being
                    // torn down, so the reply is impossible.
                    handle.abandon();
                }
            } else {
                ctx.reply_handle().send(DaemonMessageType::Session {
                    session_id: None,
                    event: SessionEvent::ReasoningEffortSet {
                        effort: "off".to_string(),
                    },
                });
            }
        }
        ClientMessageType::Unlock { private_key } => {
            info!("client {}: Unlock", ctx.client_id);
            handle_unlock_sync(ctx, kind, private_key);
        }
        ClientMessageType::BindKeystore { key } => {
            info!("client {}: BindKeystore", ctx.client_id);
            handle_bind_keystore_sync(ctx, kind, key);
        }
        ClientMessageType::Lock => {
            info!("client {}: Lock", ctx.client_id);
            let handle = ctx.reply_handle();
            handle_lock_sync(ctx, handle);
        }
        ClientMessageType::AddCredential {
            service,
            encrypted_payload,
            unlock_key,
        } => {
            info!(
                "client {}: AddCredential service={}",
                ctx.client_id, service
            );
            handle_add_credential_sync(&mut *ctx, kind, &service, encrypted_payload, unlock_key);
        }
        ClientMessageType::RemoveCredential { service } => {
            info!(
                "client {}: RemoveCredential service={}",
                ctx.client_id, service
            );
            let handle = ctx.reply_handle();
            handle_remove_credential_sync(ctx, handle, service);
        }
        ClientMessageType::AclAdd { pubkey } => {
            info!("client {}: AclAdd (local={})", ctx.client_id, ctx.is_unix);
            let handle = ctx.reply_handle();
            handle_acl_add_sync(ctx, handle, &pubkey);
        }
        ClientMessageType::ListModels => {
            debug!("client {}: ListModels", ctx.client_id);
            let session_id = *ctx.attached_session_id;
            let handle = ctx.reply_handle();
            handle_list_models_sync(ctx, handle, session_id);
        }
        ClientMessageType::GetImage {
            session_id,
            turn_id,
            key,
        } => {
            debug!(
                "client {}: GetImage session={} turn={} key={:?}",
                ctx.client_id, session_id, turn_id, key
            );
            let handle = ctx.reply_handle();
            handle_client_get_image(session_id, turn_id, key, ctx, handle);
        }
        ClientMessageType::RefreshModels { force } => {
            debug!("client {}: RefreshModels force={}", ctx.client_id, force);
            let handle = ctx.reply_handle();
            handle_refresh_models_sync(ctx, handle, force);
        }
        ClientMessageType::McpStatusRequest => {
            debug!("client {}: McpStatusRequest", ctx.client_id);
            let handle = ctx.reply_handle();
            handle_mcp_status_sync(ctx, handle);
        }
        ClientMessageType::McpReconnect { slug } => {
            debug!("client {}: McpReconnect slug={}", ctx.client_id, slug);
            let handle = ctx.reply_handle();
            handle_mcp_reconnect_sync(ctx, handle, slug);
        }
        ClientMessageType::McpReload => {
            debug!("client {}: McpReload", ctx.client_id);
            let handle = ctx.reply_handle();
            handle_mcp_reload_sync(ctx, handle);
        }
        ClientMessageType::McpTrust => {
            debug!("client {}: McpTrust", ctx.client_id);
            let handle = ctx.reply_handle();
            handle_mcp_trust_sync(ctx, handle, true);
        }
        ClientMessageType::McpUntrust => {
            debug!("client {}: McpUntrust", ctx.client_id);
            let handle = ctx.reply_handle();
            handle_mcp_trust_sync(ctx, handle, false);
        }
        ClientMessageType::McpTrustList => {
            debug!("client {}: McpTrustList", ctx.client_id);
            let handle = ctx.reply_handle();
            handle_mcp_trust_list_sync(ctx, handle);
        }
        ClientMessageType::DeleteSession { session_id } => {
            info!("client {}: DeleteSession id={}", ctx.client_id, session_id);
            let handle = ctx.reply_handle();
            handle_delete_session_sync(ctx, handle, session_id);
        }
        ClientMessageType::SetSessionPinned { session_id, pinned } => {
            info!(
                "client {}: SetSessionPinned id={} pinned={}",
                ctx.client_id, session_id, pinned
            );
            let handle = ctx.reply_handle();
            handle_set_session_flags_sync(ctx, handle, session_id, Some(pinned), None, kind);
        }
        ClientMessageType::SetSessionArchived {
            session_id,
            archived,
        } => {
            info!(
                "client {}: SetSessionArchived id={} archived={}",
                ctx.client_id, session_id, archived
            );
            let handle = ctx.reply_handle();
            handle_set_session_flags_sync(ctx, handle, session_id, None, Some(archived), kind);
        }
        ClientMessageType::GetCredential { service } => {
            let handle = ctx.reply_handle();
            handle_get_credential_sync(ctx, handle, service);
        }
        ClientMessageType::GetSessionState { session_id } => {
            debug!(
                "client {}: GetSessionState id={}",
                ctx.client_id, session_id
            );
            handle_get_session_state_sync(ctx, session_id);
        }
        ClientMessageType::AddAccount {
            name,
            provider,
            base_url,
            streaming,
            retry_max_attempts,
            connect_timeout_secs,
            request_timeout_secs,
            total_timeout_secs,
        } => {
            reply_from_daemon(
                ctx,
                |reply| DaemonCommand::AddAccountCmd {
                    name: name.clone(),
                    provider,
                    base_url,
                    streaming,
                    retry_max_attempts,
                    connect_timeout_secs,
                    request_timeout_secs,
                    total_timeout_secs,
                    reply,
                },
                |()| DaemonMessageType::AccountAdded { name: name.clone() },
                |e: String| DaemonMessageType::AccountAddFailed {
                    name: name.clone(),
                    error: e,
                },
            );
        }
        ClientMessageType::RemoveAccount { name } => {
            reply_from_daemon(
                ctx,
                |reply| DaemonCommand::RemoveAccountCmd {
                    name: name.clone(),
                    reply,
                },
                |()| DaemonMessageType::AccountRemoved { name: name.clone() },
                |e: String| DaemonMessageType::AccountRemoveFailed {
                    name: name.clone(),
                    error: e,
                },
            );
        }
        ClientMessageType::ListAccounts => {
            reply_from_daemon(
                ctx,
                |reply| DaemonCommand::ListAccountsCmd { reply },
                |accounts| DaemonMessageType::Accounts { accounts },
                |e: String| DaemonMessageType::AccountListFailed { error: e },
            );
        }
        ClientMessageType::SetSessionAccount { name } => {
            handle_client_set_session_account(name, ctx);
        }
        ClientMessageType::SubscribeAllActivity => {
            let _ = ctx
                .daemon_tx
                .send(DaemonCommand::RegisterActivitySubscriber {
                    client_id: ctx.client_id,
                    writer: ctx.writer.clone(),
                });
            ctx.ack(kind);
        }
        ClientMessageType::UnsubscribeAllActivity => {
            let _ = ctx
                .daemon_tx
                .send(DaemonCommand::UnregisterActivitySubscriber {
                    client_id: ctx.client_id,
                });
            ctx.ack(kind);
        }
        _ => {
            // Defensive net for a request the dispatch does not (yet) answer —
            // today only `GetSessionState`, which is defined on the wire but
            // has no connection-thread handler. The variant set IS the wire
            // contract, so this arm should not be reached in practice, but it
            // must still REPLY rather than drop: an unanswered request strands
            // the client's pending slot until its timeout.
            warn!("unhandled client message (kind {kind:?})");
            ctx.reply_handle().send(DaemonMessageType::Failed {
                kind,
                error: "unsupported request".into(),
            });
        }
    }
    Ok(())
}

#[expect(clippy::too_many_arguments)]
/// Handle a `CreateSession` client message. Returns false if the daemon
/// disconnected, signaling `client_thread` to return.
pub(super) fn handle_client_create_session(
    title: Option<String>,
    parent_session_id: Option<u64>,
    working_dir: Option<String>,
    context_config: Option<ContextConfig>,
    account_name: Option<String>,
    selected_model: Option<String>,
    reasoning_effort: Option<String>,
    ctx: &mut ClientCtx<'_>,
    handle: ReplyHandle,
) -> bool {
    info!("client {}: CreateSession", ctx.client_id);
    let cwd_str = working_dir.clone();
    let (reply, rx) = mpsc::channel();
    let _ = ctx.daemon_tx.send(DaemonCommand::CreateSession {
        title: title.clone(),
        parent_session_id,
        working_dir: working_dir.map(std::path::PathBuf::from),
        reasoning_effort: reasoning_effort.clone(),
        selected_model: selected_model.clone(),
        context_config,
        account_name: account_name.clone(),
        active_tool_groups: Vec::new(),
        reply,
    });
    match rx.recv() {
        Ok(Ok((sid, _session_tx))) => {
            // _session_tx is discarded here because the
            // daemon keeps its own clone in active_sessions
            // (keyed by sid).  When the client later calls
            // AttachSession the daemon returns another clone
            // — no need to hold one in the connection thread.
            //
            // Don't auto-attach or detach here — the TUI
            // attaches explicitly via AttachSession when
            // the user presses Enter on a session.
            // This keeps the old session alive when
            // creating from the session manager page.
            // The reply to THIS connection's CreateSession is
            // `SessionCreatedForRequester` — the frontend may attach to it.
            // The daemon separately broadcasts `SessionCreated` to every
            // subscriber (see `DaemonState::handle_create_session`), where it
            // is notification-only and must not move a client's view.
            handle.send(DaemonMessageType::Session {
                session_id: Some(sid),
                event: SessionEvent::SessionCreatedForRequester {
                    title,
                    parent_session_id,
                    working_dir: cwd_str,
                    account_name,
                    selected_model,
                    reasoning_effort,
                },
            });
        }
        Ok(Err(e)) => {
            handle.send(DaemonMessageType::Session {
                session_id: None,
                event: SessionEvent::SessionFailed {
                    operation: "create_session".into(),
                    error: e.to_string(),
                },
            });
        }
        Err(_) => {
            // Daemon disconnected: release the obligation (the caller returns
            // false and the connection is torn down) so the guard does not
            // fire on an impossible reply.
            handle.abandon();
            return false;
        }
    }
    true
}

/// Handle an `AttachSession` client message. Returns false if the daemon
/// disconnected, signaling `client_thread` to return.
pub(super) fn handle_client_attach_session(
    session_id: u64,
    ctx: &mut ClientCtx<'_>,
    handle: ReplyHandle,
) -> bool {
    info!("client {}: AttachSession id={}", ctx.client_id, session_id);
    let (reply, rx) = mpsc::channel();
    let _ = ctx
        .daemon_tx
        .send(DaemonCommand::AttachSession { session_id, reply });
    match rx.recv() {
        Ok(Ok(session_tx)) => {
            // Send SessionAttached before SessionCommand::Attach so that
            // the TUI's attached_session_id is set before SessionState
            // arrives — otherwise SessionState is silently dropped.
            handle.send(DaemonMessageType::Session {
                session_id: Some(session_id),
                event: SessionEvent::SessionAttached,
            });
            switch_attached_session(session_id, session_tx, ctx);
        }
        Ok(Err(e)) => {
            handle.send(DaemonMessageType::Session {
                session_id: None,
                event: SessionEvent::SessionFailed {
                    operation: "attach_session".into(),
                    error: e.to_string(),
                },
            });
        }
        Err(_) => {
            handle.abandon();
            return false;
        }
    }
    true
}

/// Handle a `SetSessionAccount` client message: verify the account exists
/// via the daemon, then set it on the attached session. The account check runs
/// HERE, on the connection thread, so the not-found and no-session cases reply
/// a targeted session-scoped `SessionFailed { operation: "set_account" }`
/// directly (the shape every front-end already renders); on success the
/// session thread acks through the minted reply target.
pub(super) fn handle_client_set_session_account(name: String, ctx: &mut ClientCtx) {
    // Clone the session sender so the reply-target mint below can borrow `ctx`
    // freely (an owned `Sender` avoids holding a borrow of `ctx` across it).
    let Some(tx) = ctx.attached_session_tx.clone() else {
        ctx.reply_handle().send(DaemonMessageType::Session {
            session_id: None,
            event: SessionEvent::SessionFailed {
                operation: "set_account".into(),
                error: "no session attached".into(),
            },
        });
        return;
    };
    // Verify the account exists before setting it.
    let (reply, rx) = mpsc::channel();
    let _ = ctx.daemon_tx.send(DaemonCommand::AccountExists {
        name: name.clone(),
        reply,
    });
    match rx.recv() {
        Ok(true) => {
            let target = ctx.reply_target(MessageKind::SetSessionAccount);
            let _ = tx.send(SessionCommand::SetAccount {
                name,
                reply: Some(target),
            });
        }
        _ => {
            ctx.reply_handle().send(DaemonMessageType::Session {
                session_id: *ctx.attached_session_id,
                event: SessionEvent::SessionFailed {
                    operation: "set_account".into(),
                    error: format!("account '{name}' not found"),
                },
            });
        }
    }
}

/// Send a `DaemonCommand` that expects a reply and wait for the response.
/// Returns the reply value, or the `RecvError` if the daemon dropped the sender.
pub(super) fn request_daemon<R>(
    daemon_tx: &crossbeam_channel::Sender<DaemonCommand>,
    make_cmd: impl FnOnce(mpsc::Sender<R>) -> DaemonCommand,
) -> Result<R, mpsc::RecvError> {
    let (reply, rx) = mpsc::channel();
    if daemon_tx.send(make_cmd(reply)).is_err() {
        return Err(mpsc::RecvError);
    }
    rx.recv()
}

/// Run a daemon round-trip whose reply is the standard `Result<T, E>` shape and
/// answer the client with exactly one terminal reply: `on_ok`/`on_err` build
/// the payload, and a gone daemon `abandons` the handle (the reply is
/// impossible, not forgotten). The single place this repeated
/// `Ok(Ok(..))` / `Ok(Err(..))` / `Err(..)` mapping lives, so the handlers that
/// share it cannot drift.
fn reply_from_daemon<T, E: std::fmt::Display>(
    ctx: &ClientCtx,
    make: impl FnOnce(mpsc::Sender<Result<T, E>>) -> DaemonCommand,
    on_ok: impl FnOnce(T) -> DaemonMessageType,
    on_err: impl FnOnce(E) -> DaemonMessageType,
) {
    let handle = ctx.reply_handle();
    match request_daemon(ctx.daemon_tx, make) {
        Ok(Ok(value)) => handle.send(on_ok(value)),
        Ok(Err(error)) => handle.send(on_err(error)),
        // Daemon gone: the reply is impossible, not forgotten.
        Err(_) => handle.abandon(),
    }
}

pub(super) fn handle_unlock_sync(ctx: &mut ClientCtx, kind: MessageKind, private_key: Vec<u8>) {
    // The daemon command loop enqueues the targeted reply (Unlocked /
    // KeystoreUnbound / LockedError) through the minted reply target DIRECTLY
    // onto this client's writer queue BEFORE its lock-state broadcast — see
    // ORDERING INVARIANT in `DaemonState::handle_unlock`. This thread only waits
    // for the ack so a dropped daemon channel is reported.
    let result = request_daemon(ctx.daemon_tx, |ack| DaemonCommand::Unlock {
        private_key,
        reply: Some(ctx.reply_target(kind)),
        ack,
    });
    if result.is_err() {
        warn!("daemon disconnected while handling unlock");
    }
}

/// Handle `ClientMessageType::BindKeystore`: the ONLY path that can create the
/// keystore binding. On an unbound keystore the daemon adopts the key (loud
/// TOFU log), runs the shared unlock tail, and the client gets the targeted
/// `DaemonMessageType::Bound` reply (sent by the daemon loop into this client's
/// sink BEFORE the lock-state broadcast — see ORDERING INVARIANT in
/// `handle_unlock`); on an already-bound keystore a wrong key is rejected
/// with the existing wrong-key semantics (`LockedError`) — no unlock, no
/// overwrite.
pub(super) fn handle_bind_keystore_sync(ctx: &mut ClientCtx, kind: MessageKind, key: Vec<u8>) {
    let result = request_daemon(ctx.daemon_tx, |ack| DaemonCommand::BindKeystore {
        key,
        reply: Some(ctx.reply_target(kind)),
        ack,
    });
    if result.is_err() {
        warn!("daemon disconnected while handling bind keystore");
    }
}

/// Reply to a `ClientMessageType::Lock` (`/lock`): the daemon clears its
/// in-memory credentials, flips to the locked state, and broadcasts `Locked`
/// to every activity subscriber. This per-action reply confirms the wipe to
/// the acting client directly; the transition broadcast reaches it too (it
/// is an activity subscriber), harmlessly idempotent — the TUI latches
/// `keystore_locked` either way.
pub(super) fn handle_lock_sync(ctx: &mut ClientCtx<'_>, handle: ReplyHandle) {
    let result = request_daemon(ctx.daemon_tx, |reply| DaemonCommand::Lock { reply });
    match result {
        Ok(Ok(())) => handle.send(DaemonMessageType::Locked),
        Ok(Err(e)) => handle.send(DaemonMessageType::LockedError { error: e }),
        // Daemon gone: the reply is impossible, not forgotten.
        Err(_) => handle.abandon(),
    }
}

pub(super) fn handle_list_models_sync(
    ctx: &mut ClientCtx<'_>,
    handle: ReplyHandle,
    attached_session_id: Option<u64>,
) {
    let result = request_daemon(ctx.daemon_tx, |reply| DaemonCommand::ListModels {
        session_id: attached_session_id,
        reply,
    });
    match result {
        Ok(Ok((models, selected_model))) => handle.send(DaemonMessageType::Models {
            models,
            selected_model,
        }),
        Ok(Err(e)) => handle.send(DaemonMessageType::ModelsFailed { error: e }),
        Err(_) => handle.abandon(),
    }
}

/// Handle a `GetImage` client message: read the requested turn attachment's
/// bytes (a displayed image or a tool-result vision image) and reply with a
/// targeted [`DaemonMessageType::Image`].
///
/// Only attachments of the session THIS connection is attached to are served —
/// the same trust boundary every other session-scoped command enforces. A
/// request for any other (or no) session is answered `None` rather than reading
/// an arbitrary session's attachments.
///
/// The connection now owns a redb handle (see [`ClientCtx::db`]), so the read
/// runs RIGHT HERE on the connection thread via [`crate::db::read_attachment`]
/// — a single O(log n) `get` against the attachment table, with no
/// command-loop round-trip and no reply channel to drain. Each connection
/// opens its own read transaction, so concurrent connections never serialize.
pub(super) fn handle_client_get_image(
    session_id: u64,
    turn_id: u32,
    key: choreo_proto::ImageKey,
    ctx: &ClientCtx,
    handle: ReplyHandle,
) {
    let data = if *ctx.attached_session_id == Some(session_id) {
        // `None` covers both "not found" and a redb read error; the client
        // treats them identically (mark the image failed, don't retry), so the
        // two collapse here intentionally — a transient redb hiccup must never
        // leak a raw error into the image-fetch protocol.
        match crate::db::read_attachment(ctx.db, session_id, turn_id, &key) {
            Ok(data) => data,
            Err(e) => {
                warn!(
                    session_id,
                    turn_id,
                    ?key,
                    error = %e,
                    "failed to read image attachment"
                );
                None
            }
        }
    } else {
        None
    };
    handle.send(DaemonMessageType::Image {
        session_id,
        turn_id,
        key,
        data,
    });
}

/// Handle a `RefreshModels` client message: forward the request to the daemon
/// (which hands it to the maintenance thread — the fetch never blocks this
/// connection), then route the reply back to the client. The request blocks
/// here until the maintenance thread has a result, which is the request/
/// response contract `/refresh-models` implies.
pub(super) fn handle_refresh_models_sync(
    ctx: &mut ClientCtx<'_>,
    handle: ReplyHandle,
    force: bool,
) {
    let result = request_daemon(ctx.daemon_tx, |reply| DaemonCommand::RefreshModels {
        force,
        reply,
    });
    match result {
        Ok(Ok(report)) => handle.send(DaemonMessageType::ModelsRefreshed {
            providers: report.providers,
            models: report.models,
            status: report.status,
        }),
        Ok(Err(e)) => handle.send(DaemonMessageType::ModelsRefreshFailed { error: e }),
        Err(_) => handle.abandon(),
    }
}

/// Convert the daemon's MCP status record into the wire type sent to clients.
///
/// The two structs carry the same fields, so this is a field-for-field move;
/// it exists as a named function so the conversion has one home and can be
/// unit-tested against a status record.
pub(super) fn wire_mcp_status(
    status: crate::mcp::McpServerStatus,
) -> choreo_proto::McpServerStatus {
    choreo_proto::McpServerStatus {
        slug: status.slug,
        tier: status.tier,
        transport: status.transport,
        target: status.target,
        connected: status.connected,
        tool_count: status.tool_count,
        server_name: status.server_name,
        server_version: status.server_version,
        last_error: status.last_error,
    }
}

/// Handle a `ClientMessageType::McpStatusRequest`: ask the daemon (the sole owner
/// of the `McpManager`) for every server visible to the ATTACHED session
/// (daemon tier plus that session's project servers), convert each record to
/// the wire type, and reply with [`DaemonMessageType::McpStatus`] — including the
/// session's project-root trust context.
pub(super) fn handle_mcp_status_sync(ctx: &mut ClientCtx<'_>, handle: ReplyHandle) {
    let session_id = *ctx.attached_session_id;
    let result = request_daemon(ctx.daemon_tx, |reply| DaemonCommand::McpStatus {
        session_id,
        reply,
    });
    match result {
        Ok(report) => send_mcp_status(handle, report),
        Err(_) => handle.abandon(),
    }
}

/// Send an [`McpStatusReport`](crate::mcp::McpStatusReport) as a wire
/// [`DaemonMessageType::McpStatus`], converting each server record and carrying the
/// project-root trust context.
pub(super) fn send_mcp_status(handle: ReplyHandle, report: crate::mcp::McpStatusReport) {
    let servers = report.servers.into_iter().map(wire_mcp_status).collect();
    handle.send(DaemonMessageType::McpStatus {
        servers,
        project_root: report
            .project_root
            .map(|p| p.to_string_lossy().into_owned()),
        project_trusted: report.project_trusted,
        ignored_project_servers: report.ignored_project_servers,
    });
}

/// Handle a `ClientMessageType::McpReconnect`: rebuild one MCP server's connection
/// through the daemon (which also swaps the refreshed tool catalogue), then
/// reply. Success is reported as a refreshed [`DaemonMessageType::McpStatus`] —
/// the same snapshot a status request would return, so the requester sees the
/// server's new connected state and tool count — while a failure is a
/// targeted [`DaemonMessageType::McpReconnectFailed`]. Both requests block here
/// until the daemon has a result, matching the `/mcp` request/reply contract.
pub(super) fn handle_mcp_reconnect_sync(
    ctx: &mut ClientCtx<'_>,
    handle: ReplyHandle,
    slug: String,
) {
    let result = request_daemon(ctx.daemon_tx, |reply| DaemonCommand::McpReconnect {
        slug: slug.clone(),
        reply,
    });
    match result {
        Ok(Ok(())) => {
            let session_id = *ctx.attached_session_id;
            let status = request_daemon(ctx.daemon_tx, |reply| DaemonCommand::McpStatus {
                session_id,
                reply,
            });
            match status {
                Ok(report) => send_mcp_status(handle, report),
                Err(_) => handle.abandon(),
            }
        }
        Ok(Err(e)) => handle.send(DaemonMessageType::McpReconnectFailed { slug, error: e }),
        Err(_) => handle.abandon(),
    }
}

/// Handle a `ClientMessageType::McpReload`: ask the daemon (the sole owner of the
/// `McpManager`) to re-read the MCP config and reconcile the running servers,
/// which also swaps the refreshed tool catalogue, then reply. Success is
/// reported as [`DaemonMessageType::McpReloaded`] — the reload summary plus the
/// refreshed status list — while a config read/parse failure is a
/// [`DaemonMessageType::McpReloadFailed`].
pub(super) fn handle_mcp_reload_sync(ctx: &mut ClientCtx<'_>, handle: ReplyHandle) {
    let session_id = *ctx.attached_session_id;
    let result = request_daemon(ctx.daemon_tx, |reply| DaemonCommand::McpReload {
        session_id,
        reply,
    });
    match result {
        Ok(Ok(outcome)) => {
            let servers = outcome.servers.into_iter().map(wire_mcp_status).collect();
            handle.send(DaemonMessageType::McpReloaded {
                summary: outcome.summary,
                servers,
            });
        }
        Ok(Err(e)) => handle.send(DaemonMessageType::McpReloadFailed { error: e }),
        Err(_) => handle.abandon(),
    }
}

/// Handle a `ClientMessageType::McpTrust` / `McpUntrust`: set (or revoke) trust for
/// the ATTACHED session's project root, then reply with the resulting state.
pub(super) fn handle_mcp_trust_sync(ctx: &mut ClientCtx<'_>, handle: ReplyHandle, trusted: bool) {
    let Some(session_id) = *ctx.attached_session_id else {
        handle.send(DaemonMessageType::McpTrustUpdated {
            root: None,
            trusted: false,
            message: "no session attached".to_string(),
        });
        return;
    };
    let result = request_daemon(ctx.daemon_tx, |reply| DaemonCommand::McpTrustSet {
        session_id,
        trusted,
        reply,
    });
    match result {
        Ok(outcome) => handle.send(DaemonMessageType::McpTrustUpdated {
            root: outcome.root.map(|p| p.to_string_lossy().into_owned()),
            trusted: outcome.trusted,
            message: outcome.message,
        }),
        Err(_) => handle.abandon(),
    }
}

/// Handle a `ClientMessageType::McpTrustList`: reply with the trusted project roots.
pub(super) fn handle_mcp_trust_list_sync(ctx: &mut ClientCtx<'_>, handle: ReplyHandle) {
    let result = request_daemon(ctx.daemon_tx, |reply| DaemonCommand::McpTrustList { reply });
    match result {
        Ok(roots) => handle.send(DaemonMessageType::McpTrustList {
            roots: roots
                .into_iter()
                .map(|p| p.to_string_lossy().into_owned())
                .collect(),
        }),
        Err(_) => handle.abandon(),
    }
}

pub(super) fn handle_get_credential_sync(
    ctx: &mut ClientCtx<'_>,
    handle: ReplyHandle,
    service: String,
) {
    let result = request_daemon(ctx.daemon_tx, |reply| DaemonCommand::GetCredential {
        service: service.clone(),
        reply,
    });
    match result {
        Ok(Some(key)) => handle.send(DaemonMessageType::Credential {
            service,
            key: Some(key),
        }),
        Ok(None) => handle.send(DaemonMessageType::Credential { service, key: None }),
        Err(_) => handle.abandon(),
    }
}

/// Answer a `GetSessionState`: the daemon ensures the session thread is live
/// (loading it from the DB if it had slept) and returns its `SessionState`
/// snapshot, or a `NotFound` error. The snapshot IS the terminal reply (it is
/// the same `SessionEvent::SessionState` the attach push delivers), so the
/// daemon's `io::Result` maps straight onto the reply.
pub(super) fn handle_get_session_state_sync(ctx: &ClientCtx<'_>, session_id: u64) {
    reply_from_daemon(
        ctx,
        |reply| DaemonCommand::GetSessionState { session_id, reply },
        std::convert::identity,
        |e: io::Error| DaemonMessageType::Failed {
            kind: MessageKind::GetSessionState,
            error: e.to_string(),
        },
    );
}

pub(super) fn handle_delete_session_sync(
    ctx: &mut ClientCtx<'_>,
    handle: ReplyHandle,
    session_id: u64,
) {
    let result = request_daemon(ctx.daemon_tx, |reply| DaemonCommand::DeleteSession {
        session_id,
        reply,
    });
    match result {
        Ok(Ok(())) => {
            // The daemon broadcasts `SessionDeleted` to all summary subscribers
            // (this client included when it is viewing the session list); the
            // broadcast is unchanged, and this targeted `Accepted` is the
            // request's terminal reply.
            handle.send(DaemonMessageType::Accepted {
                kind: MessageKind::DeleteSession,
            });
        }
        // A delete failure is session-scoped and richer than a bare
        // `Failed`: carrying the `SessionDeleteFailed` event keeps the origin
        // session (which the ACP keys its pending delete on) and lets every
        // front-end reuse its existing session-scoped failure handling. The
        // reply still carries the correlation id (reply-ness is a property of
        // the send, not of the payload type).
        Ok(Err(e)) => handle.send(DaemonMessageType::Session {
            session_id: Some(session_id),
            event: SessionEvent::SessionDeleteFailed {
                error: e.to_string(),
            },
        }),
        // Daemon gone: the reply is impossible, not forgotten.
        Err(_) => handle.abandon(),
    }
}

/// Handle a `SetSessionPinned`/`SetSessionArchived` client message. On success
/// the requester gets a targeted `Accepted` IN ADDITION to the daemon's
/// `SessionFlagsChanged` broadcast (which stays `id: None` and reaches every
/// subscriber). On failure the requester gets a targeted session-scoped
/// `SessionFailed { operation, error }` (the operation names which of the two
/// messages it was) — the same event shape every front-end already renders, so
/// a pin/archive failure is never silently dropped. Follows the same shape as
/// [`handle_delete_session_sync`].
pub(super) fn handle_set_session_flags_sync(
    ctx: &mut ClientCtx<'_>,
    handle: ReplyHandle,
    session_id: u64,
    pinned: Option<bool>,
    archived: Option<bool>,
    kind: MessageKind,
) {
    let result = request_daemon(ctx.daemon_tx, |reply| DaemonCommand::SetSessionFlags {
        session_id,
        pinned,
        archived,
        reply,
    });
    match result {
        Ok(Ok(())) => handle.send(DaemonMessageType::Accepted { kind }),
        Ok(Err(e)) => handle.send(DaemonMessageType::Session {
            session_id: Some(session_id),
            event: SessionEvent::SessionFailed {
                operation: operation_for_kind(kind).to_string(),
                error: e.to_string(),
            },
        }),
        Err(_) => handle.abandon(),
    }
}

/// The operation label a session-scoped [`SessionEvent::SessionFailed`] carries
/// for the flag mutations, so a front-end can name the failed command (and the
/// session-manager page can surface it inline).
pub(super) fn operation_for_kind(kind: MessageKind) -> &'static str {
    match kind {
        MessageKind::SetSessionPinned => "set_session_pinned",
        MessageKind::SetSessionArchived => "set_session_archived",
        _ => "session mutation",
    }
}

pub(super) fn handle_add_credential_sync(
    ctx: &mut ClientCtx,
    kind: MessageKind,
    service: &str,
    encrypted_payload: Vec<u8>,
    // REQUIRED since the per-daemon keystore TOFU design (Task 1 made the
    // proto field non-optional): the credential must be usable immediately.
    unlock_key: Vec<u8>,
) {
    // The daemon command loop enqueues the targeted replies (Unlocked +
    // CredentialAdded, or the failure variant) through the minted reply target
    // DIRECTLY onto this client's writer queue BEFORE its lock-state broadcast —
    // see ORDERING INVARIANT in `DaemonState::handle_unlock`. This thread only
    // waits for the ack.
    let result = request_daemon(ctx.daemon_tx, |ack| DaemonCommand::SaveCredential {
        service: service.to_string(),
        encrypted_blob: encrypted_payload,
        unlock_key,
        reply: Some(ctx.reply_target(kind)),
        ack,
    });
    if result.is_err() {
        warn!("daemon disconnected while handling add credential");
    }
}

/// Enroll a client key in the daemon's ACL. LOCAL (Unix socket) connections
/// only: the check happens HERE, on the connection thread, so a remote
/// client gets its refusal without the command loop ever seeing the command.
/// The trust approver must be at the machine (console or ssh) — an
/// already-remote client must not be able to mint new trust.
pub(super) fn handle_acl_add_sync(ctx: &mut ClientCtx<'_>, handle: ReplyHandle, pubkey: &str) {
    if !ctx.is_unix {
        warn!(
            "client {}: AclAdd refused: remote connections cannot change the ACL",
            ctx.client_id
        );
        handle.send(DaemonMessageType::AclAddResult {
            ok: false,
            message: "ACL changes are only permitted from local connections".to_string(),
        });
        return;
    }
    let result = request_daemon(ctx.daemon_tx, |reply| DaemonCommand::AclAddCmd {
        pubkey: pubkey.to_string(),
        reply,
    });
    match result {
        Ok(Ok(count)) => handle.send(DaemonMessageType::AclAddResult {
            ok: true,
            message: format!("client key authorized ({count} client(s) now trusted)"),
        }),
        Ok(Err(e)) => handle.send(DaemonMessageType::AclAddResult {
            ok: false,
            message: e,
        }),
        Err(_) => handle.abandon(),
    }
}

pub(super) fn handle_remove_credential_sync(
    ctx: &mut ClientCtx<'_>,
    handle: ReplyHandle,
    service: String,
) {
    let result = request_daemon(ctx.daemon_tx, |reply| DaemonCommand::RemoveCredentialCmd {
        service: service.clone(),
        reply,
    });
    match result {
        Ok(Ok(())) => handle.send(DaemonMessageType::CredentialRemoved { service }),
        Ok(Err(e)) => handle.send(DaemonMessageType::CredentialRemoveFailed { service, error: e }),
        Err(_) => handle.abandon(),
    }
}
