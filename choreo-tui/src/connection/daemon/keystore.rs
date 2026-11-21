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
use super::Consumed;
use crate::state::App;
use choreo_client_core::Pending;
use choreo_proto::{ClientMessage, KeystoreState};

/// An `Unlocked` reply: accept the pending key and clear the lock banner.
pub(super) fn on_unlocked(app: &mut App, resolved: Option<&Pending>) -> Consumed {
    // A successful unlock (explicit /unlock or the connect-time
    // auto-unlock) means the daemon ACCEPTED the pending key — record it
    // per-daemon on confirmed success only. Latch the keystore as
    // UNLOCKED (clears the persistent lock banner). Deliberately no early
    // return: the generic dispatch below still emits the "keystore
    // unlocked" status.
    app.keystore_locked = false;
    super::record_confirmed_unlock_key(app, resolved);
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
    super::record_confirmed_unlock_key(app, resolved);
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
    super::discard_rejected_unlock_key(app, resolved);
    Consumed::No
}

/// A `CredentialAddFailed` reply: same pending-key discipline as a rejected
/// unlock (the add carries no key, so there is normally nothing to discard).
pub(super) fn on_credential_add_failed(app: &mut App, resolved: &mut Option<Pending>) -> Consumed {
    // Same pending-key discipline for a rejected AddCredential: drop the
    // in-flight key, leave the store alone.
    super::discard_rejected_unlock_key(app, resolved);
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
    super::discard_rejected_unlock_key(app, resolved);
    // Distinct guidance: this is not "wrong key" but "never bound" —
    // the fix is automatic, not something the user must do.
    app.set_status(
        "keystore not initialized — a binding will be created automatically".to_string(),
    );
    // Bind-loop guard: a second `KeystoreUnbound` after our bind was
    // sent means the confirmation was lost or the daemon re-keyed —
    // surface an error, leave the connection as-is.
    if !super::trigger_keystore_auto_bind(app, client_tx) {
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
                super::discard_rejected_unlock_key(app, resolved);
                app.set_status(
                    "keystore not initialized — a binding will be created automatically"
                        .to_string(),
                );
                let _ = super::trigger_keystore_auto_bind(app, client_tx);
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
            super::record_confirmed_unlock_key(app, resolved.as_ref());
        }
    }
    Consumed::No
}
