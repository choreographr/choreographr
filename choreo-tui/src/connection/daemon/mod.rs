//! The daemon-message **router**: [`handle_daemon_message`] is a dispatch table
//! over the per-subject sibling modules, one arm per wire variant.
//!
//! The connection module receives every daemon message here and must (a) apply
//! the TUI-specific state its family cares about before (b) letting the generic
//! `choreo_client_core::dispatch_daemon_message` render the rest. The arm bodies
//! that do (a) were one enormous `match` in a single file; they now live in the
//! sibling family modules, each owning one coherent subject:
//!
//! * [`session`] — every `DaemonMessageType::Session` envelope event plus the
//!   `Sessions` snapshot (it owns its own inner `match` over `(session_id,
//!   event)` because all its arms are one wire variant).
//! * [`models`] — `Models*` and `ModelReasoning`.
//! * [`accounts`] — `Accounts`, `Account*`, `Credential*`, `Acl*`.
//! * [`mcp`] — `Mcp*` and `CatalogUpdated`.
//! * [`keystore`] — `Unlocked`, `Bound`, `Locked`, `LockedError`,
//!   `CredentialAddFailed`, `KeystoreUnbound`, `Keystore`.
//! * [`lifecycle`] — the connection-level termination (`ShuttingDown`,
//!   `Evicted`) and the staged-flow `SetSessionChoice` commit failure.
//!
//! Each family function returns [`Consumed`] to make the arm's control flow
//! explicit: [`Consumed::Yes`] stops here (the family rendered its own state and
//! the generic dispatch would duplicate or misroute it), while [`Consumed::No`]
//! falls through to `dispatch_daemon_message`. This router preserves the exact
//! per-arm `return Ok(())`-vs-fall-through the original match relied on.
//!
//! A **newly added wire variant that no family claims** falls to the `_` arm
//! here, is treated as [`Consumed::No`], and still reaches
//! `dispatch_daemon_message` — whose exhaustive `match` in `choreo_client_core`
//! is the compile-time tripwire that forces every new variant to be handled
//! somewhere.
//!
//! The pending-reply slot is resolved **once** here and threaded into the arms
//! that need it (`is_list_models_reply` gates the model-list arms; the keystore
//! arms record or discard the pending unlock key) — never resolved twice.
use crate::state::App;
use choreo_client_core::{
    AutoBindAttempt, ClientError, Pending, PendingContext, attempt_keystore_auto_bind,
    dispatch_daemon_message, record_unlock_key,
};
use choreo_proto::{ClientMessage, DaemonMessage, DaemonMessageType, MessageKind};

mod accounts;
mod keystore;
mod lifecycle;
mod mcp;
mod models;
mod session;

/// Whether a family handler **consumed** a daemon message, i.e. whether the
/// router must stop and skip the generic dispatch.
///
/// The two states encode the split the original match made with `return Ok(())`
/// versus falling out of the arm: some handlers render their own output (a
/// session attach, a model list, a `/mcp` status) that the generic dispatch
/// would duplicate or push to the wrong surface, while the rest only apply
/// per-session display state and must still fall through so `/model`,
/// `/account`, `/unlock` and friends keep printing their confirmation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Consumed {
    /// The family handled the message fully; do not run the generic dispatch.
    Yes,
    /// The family applied its state but the message must still reach
    /// `dispatch_daemon_message`.
    No,
}

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
    // Dispatch per-variant handlers first, then let the generic dispatch in
    // choreo_client_core handle the rest (text notifications, stream appends,
    // image assembly, etc.). Each arm delegates to its subject family and
    // reports whether it already rendered the message.
    let consumed = match &message.inner {
        DaemonMessageType::Session { session_id, event } => {
            session::on_session_message(app, client_tx, message.id, *session_id, event)?
        }
        DaemonMessageType::Sessions { sessions } => session::on_sessions(app, client_tx, sessions)?,

        // ── Accounts / credentials / ACL ────────────────────────────────
        DaemonMessageType::Accounts { accounts } => accounts::on_accounts(app, accounts),
        DaemonMessageType::AccountListFailed { error } => {
            accounts::on_account_list_failed(app, error)
        }
        DaemonMessageType::AccountAdded { name } => {
            accounts::on_account_added(app, client_tx, name)
        }
        DaemonMessageType::AccountAddFailed { name, error } => {
            accounts::on_account_add_failed(app, name, error)
        }
        DaemonMessageType::AccountRemoved { name } => {
            accounts::on_account_removed(app, client_tx, name)
        }
        DaemonMessageType::AccountRemoveFailed { name, error } => {
            accounts::on_account_remove_failed(app, name, error)
        }
        DaemonMessageType::CredentialAdded { service } => {
            accounts::on_credential_added(app, client_tx, service)
        }
        DaemonMessageType::CredentialRemoved { service } => {
            accounts::on_credential_removed(app, client_tx, service)
        }
        DaemonMessageType::AclAddResult { ok, message } => {
            accounts::on_acl_add_result(app, *ok, message)
        }
        DaemonMessageType::AclUpdated { clients } => accounts::on_acl_updated(app, *clients),

        // ── Models + reasoning + /refresh-models ────────────────────────
        DaemonMessageType::Models {
            models,
            selected_model,
            suggested_model,
            provisional,
        } => models::on_models(
            app,
            client_tx,
            resolved.as_ref(),
            models,
            selected_model.as_ref(),
            suggested_model.as_ref(),
            *provisional,
        ),
        DaemonMessageType::ModelReasoning {
            capability,
            suggested_effort,
        } => models::on_model_reasoning(app, client_tx, capability, suggested_effort.as_ref()),
        DaemonMessageType::ModelsFailed { error } => {
            models::on_models_failed(app, client_tx, resolved.as_ref(), error)
        }
        DaemonMessageType::ModelsRefreshed {
            providers,
            models,
            status,
        } => models::on_models_refreshed(app, *providers, *models, *status),
        DaemonMessageType::ModelsRefreshFailed { error } => {
            models::on_models_refresh_failed(app, error)
        }
        DaemonMessageType::ModelsUpdated { account, models } => {
            models::on_models_updated(app, client_tx, account, models)
        }
        DaemonMessageType::ModelsUnavailable { account, error } => {
            models::on_models_unavailable(app, client_tx, account, error)
        }

        // ── MCP + catalog ───────────────────────────────────────────────
        DaemonMessageType::McpStatus {
            servers,
            project_root,
            project_trusted,
            ignored_project_servers,
        } => mcp::on_mcp_status(
            app,
            client_tx,
            servers,
            project_root.as_ref(),
            *project_trusted,
            ignored_project_servers,
        ),
        DaemonMessageType::McpTrustUpdated {
            trusted, message, ..
        } => mcp::on_mcp_trust_updated(app, *trusted, message),
        DaemonMessageType::McpTrustList { roots } => mcp::on_mcp_trust_list(app, roots),
        DaemonMessageType::McpReconnectFailed { slug, error } => {
            mcp::on_mcp_reconnect_failed(app, slug, error)
        }
        DaemonMessageType::McpReloaded { summary, servers } => {
            mcp::on_mcp_reloaded(app, summary, servers)
        }
        DaemonMessageType::McpReloadFailed { error } => mcp::on_mcp_reload_failed(app, error),
        DaemonMessageType::CatalogUpdated { providers } => mcp::on_catalog_updated(app, providers),

        // ── Keystore ────────────────────────────────────────────────────
        DaemonMessageType::Unlocked => keystore::on_unlocked(app, resolved.as_ref()),
        DaemonMessageType::Bound => keystore::on_bound(app, resolved.as_ref()),
        DaemonMessageType::Locked => keystore::on_locked(app),
        DaemonMessageType::LockedError { .. } => keystore::on_locked_error(app, &mut resolved),
        DaemonMessageType::CredentialAddFailed { .. } => {
            keystore::on_credential_add_failed(app, &mut resolved)
        }
        DaemonMessageType::KeystoreUnbound { .. } => {
            keystore::on_keystore_unbound(app, client_tx, &mut resolved)
        }
        DaemonMessageType::Keystore { state } => {
            keystore::on_keystore_state(app, client_tx, *state, &mut resolved)
        }

        // ── Connection lifecycle ────────────────────────────────────────
        DaemonMessageType::Failed {
            kind: MessageKind::SetSessionChoice,
            ..
        } => lifecycle::on_set_session_choice_failed(app, client_tx),
        DaemonMessageType::ShuttingDown => lifecycle::on_shutting_down(app),
        DaemonMessageType::Evicted => lifecycle::on_evicted(app),

        _ => Consumed::No,
    };

    // Dispatch remaining variants through the generic turn-event handler.
    if matches!(consumed, Consumed::No) {
        dispatch_daemon_message(message, app);
    }
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
            let id = app.pending.send(client_tx, *msg);
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
            app.set_error(format!("[error] auto-bind failed: {error}"));
            true
        }
    }
}

/// Record the daemon-confirmed unlock key per-daemon, exactly once.
///
/// The key rides the request's pending context ([`PendingContext::UnlockKey`]),
/// attached when `Unlock`/`BindKeystore` is SENT (see `run_app`'s auto-unlock
/// and the auto-bind path; `AddCredential` carries no key). A daemon that
/// accepts the key replies `Unlocked` / `Bound`, resolving that slot — and only
/// then do we persist the key into the daemon's `known_servers` entry. This is
/// the TOFU core of the per-daemon keystore design: a key the daemon REJECTS
/// (misbound keystore) is never recorded.
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
            app.set_error(format!("[error] failed to record unlock key: {e}"));
        }
    }
}

/// The daemon REJECTED a presented unlock key (an `Unlock` failure; the
/// `CredentialAddFailed` arm also routes here, though the add carries no key, so
/// there is nothing to discard). Drop the in-flight key — zeroized, since it is
/// secret material — so a later, unrelated confirmation cannot attribute it.
/// Deliberately does NOT delete the `known_servers` record: the stored key may
/// be a valid confirmed key (the daemon reports transient failures through the
/// same error), and manual re-pair (`remove(addr)`) is the recovery path for a
/// genuinely wrong one. Takes the key out of the resolved slot so an in-process
/// reply cannot linger after the rejection.
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
    use crate::tests::support::test_app;
    // The lock-state tests build these wire types by hand; they reach the
    // module through `super::*` but the router's own imports no longer pull
    // them in, so name them here.
    use choreo_proto::{ClientMessageType, KeystoreState};

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
            app.error().is_some_and(|e| e.contains("nothing to undo")),
            "a Failed reply must set the error line, got {:?}",
            app.error()
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
        assert!(app.error().is_none(), "Accepted must not surface an error");
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
        app.clear_error();
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
            app.error().is_some(),
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
        app.clear_error();
        handle_daemon_message(
            DaemonMessage::broadcast(DaemonMessageType::Keystore {
                state: KeystoreState::Unbound,
            }),
            &mut app,
            &tx,
        )
        .unwrap();
        assert!(rx.try_recv().is_err(), "no second bind attempt");
        assert!(app.error().is_none(), "duplicate unbound push is silent");
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
