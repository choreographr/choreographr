//! The **keystore** family of daemon messages: unlock/lock outcomes, the
//! authoritative keystore status push, the unbound-daemon reply, and the
//! credential-add failure.
//!
//! `Unlocked`, `Bound`, `Locked`, `LockedError`, `CredentialAddFailed`,
//! `KeystoreUnbound` and `Keystore` all concern the per-daemon keystore
//! binding/unlock lifecycle (the TOFU key record and the persistent lock
//! banner), so they share one file. They drive the pending-key slot — the
//! record happens on a resolved `Unlocked`/`Bound`, the discard on a rejected
//! key — so they take the resolved slot mutably where needed. Every arm falls
//! through to the generic dispatch so the "keystore unlocked/locked/…" status
//! text still renders.
//!
//! The module also owns the three helpers this family alone uses, so the router
//! in `daemon/mod.rs` stays a dispatch table: the auto-bind wrapper
//! (`trigger_keystore_auto_bind`) and the record/discard pair for the pending
//! unlock key (`record_confirmed_unlock_key`, `discard_rejected_unlock_key`).
use super::Consumed;
use crate::state::App;
use choreo_client_core::{
    AutoBindAttempt, Pending, PendingContext, attempt_keystore_auto_bind, record_unlock_key,
};
use choreo_proto::{ClientMessage, KeystoreState};

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

/// An `Unlocked` reply: accept the pending key and clear the lock banner.
pub(super) fn on_unlocked(app: &mut App, resolved: Option<&Pending>) -> Consumed {
    // A successful unlock (explicit /unlock or the connect-time
    // auto-unlock) means the daemon ACCEPTED the pending key — record it
    // per-daemon on confirmed success only. Latch the keystore as
    // UNLOCKED (clears the persistent lock banner). Deliberately no early
    // return: the generic dispatch below still emits the "keystore
    // unlocked" status.
    app.keystore_locked = false;
    record_confirmed_unlock_key(app, resolved);
    Consumed::No
}

/// A `Bound` reply: an unbound daemon adopted our minted key, so treat it
/// exactly like an `Unlocked`.
pub(super) fn on_bound(app: &mut App, resolved: Option<&Pending>) -> Consumed {
    // Targeted reply to our `BindKeystore`: the unbound daemon ADOPTED the
    // fresh key we minted and ran the shared unlock tail, so from the
    // client's perspective this is exactly an `Unlocked` — clear the lock
    // banner and record the pending (minted) key per-daemon. The key was
    // already persisted pre-send by `bind_fresh_daemon`, so the re-record
    // here is a no-op-safe rewrite that keeps the pending-confirm flow
    // uniform with the Unlock/AddCredential paths. Deliberately no early
    // return: the generic dispatch below still emits the "keystore bound
    // and unlocked" status.
    app.keystore_locked = false;
    record_confirmed_unlock_key(app, resolved);
    Consumed::No
}

/// A `Locked` broadcast/push: latch the persistent lock banner.
pub(super) fn on_locked(app: &mut App) -> Consumed {
    // The daemon (re-)locked its keystore (/lock) or a freshly-connecting
    // client latched the subscribe-time lock-state push: set the
    // persistent lock flag so the banner reappears. An `Unlock` that
    // auto-locked temporarily is also confirmed here (a failed unlock
    // does NOT change lock state, so the subscribe-time `Locked` push —
    // not a transition broadcast — is what a locked daemon sends a fresh
    // client). No early return: the generic dispatch still prints the
    // "keystore locked" status.
    app.keystore_locked = true;
    Consumed::No
}

/// A `LockedError` reply: the daemon rejected the pending unlock key.
pub(super) fn on_locked_error(app: &mut App, resolved: &mut Option<Pending>) -> Consumed {
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
    app.keystore_locked = true;
    discard_rejected_unlock_key(app, resolved);
    Consumed::No
}

/// A `CredentialAddFailed` reply: same pending-key discipline as a rejected
/// unlock (the add carries no key, so there is normally nothing to discard).
pub(super) fn on_credential_add_failed(app: &mut App, resolved: &mut Option<Pending>) -> Consumed {
    // Same pending-key discipline for a rejected AddCredential: drop the
    // in-flight key, leave the store alone.
    discard_rejected_unlock_key(app, resolved);
    Consumed::No
}

/// A `KeystoreUnbound` reply: latch the lock-ish banner, drop the stale verify
/// key, and auto-bind once per connection.
pub(super) fn on_keystore_unbound(
    app: &mut App,
    client_tx: &crossbeam_channel::Sender<ClientMessage>,
    resolved: &mut Option<Pending>,
) -> Consumed {
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
    app.keystore_locked = true;
    // The stale verify key belongs to THIS frontend's pending-key
    // lifecycle: drop it (zeroized) BEFORE the shared state machine
    // runs, so the minted bind key can be held pending afterwards.
    discard_rejected_unlock_key(app, resolved);
    // Distinct guidance: this is not "wrong key" but "never bound" —
    // the fix is automatic, not something the user must do.
    app.set_status(
        "keystore not initialized — a binding will be created automatically".to_string(),
    );
    // Bind-loop guard: a second `KeystoreUnbound` after our bind was
    // sent means the confirmation was lost or the daemon re-keyed —
    // surface an error, leave the connection as-is.
    if !trigger_keystore_auto_bind(app, client_tx) {
        app.set_error(
            "[daemon] keystore still unbound after bind attempt — reconnect to retry".to_string(),
        );
    }
    Consumed::No
}

/// The authoritative `Keystore` status push/broadcast.
pub(super) fn on_keystore_state(
    app: &mut App,
    client_tx: &crossbeam_channel::Sender<ClientMessage>,
    state: KeystoreState,
    resolved: &mut Option<Pending>,
) -> Consumed {
    // The daemon's authoritative keystore STATUS (subscribe-time push and
    // every transition). This is the signal that lets a first-run client
    // with NO key discover the keystore is `Unbound` and auto-bind — the
    // core of the fix: the client no longer has to guess from an operation
    // reply. No early return: the generic dispatch still prints the status.
    match state {
        KeystoreState::Unbound => {
            app.keystore_locked = true;
            // The `KeystoreUnbound` reply to a connect-time auto-unlock
            // attempt may ALREADY have triggered the bind; this push
            // reports the SAME unbound fact, so only act when no bind is
            // in flight — otherwise the latch would surface a spurious
            // "still unbound" error and drop the minted pending key.
            if !app.keystore_auto_bind.attempted() {
                discard_rejected_unlock_key(app, resolved);
                app.set_status(
                    "keystore not initialized — a binding will be created automatically"
                        .to_string(),
                );
                let _ = trigger_keystore_auto_bind(app, client_tx);
            }
        }
        KeystoreState::Locked => {
            app.keystore_locked = true;
            // With no stored key for this daemon there is nothing to
            // auto-unlock with: it was bound by another client. Say how to
            // get in rather than leaving only the lock icon. Shown as an
            // error because the generic dispatch below overwrites the
            // status line with the plain "keystore locked" text.
            if choreo_client_core::try_auto_unlock_key(&app.connection_addr).is_none() {
                app.set_error(
                    "this daemon's keystore was set up by another client — run \
                     /unlock <base64 unlock-key> with that client's unlock key"
                        .to_string(),
                );
            }
        }
        KeystoreState::Unlocked => {
            app.keystore_locked = false;
            record_confirmed_unlock_key(app, resolved.as_ref());
        }
    }
    Consumed::No
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests::support::test_app;
    // The wire types the lock-state tests build by hand: `KeystoreState` and
    // `ClientMessage` reach this module through `super::*`; the message
    // envelope, its type enum and the client-message variants the fixtures
    // construct are named here.
    use choreo_proto::{ClientMessageType, DaemonMessage, DaemonMessageType};

    // The one dispatcher definition lives beside `handle_daemon_message`; the
    // router's own tests call the same helper.
    use super::super::dispatch;
    // The auto-bind fixtures assert on what went out over the wire, so they
    // drive the full router rather than the single-dispatch helper.
    use super::super::handle_daemon_message;

    #[test]
    fn app_defaults_to_keystore_locked() {
        // Safest default: assume locked until the daemon reports otherwise.
        assert!(App::new().keystore_locked);
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
        // no key bind a fresh daemon at all.
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
