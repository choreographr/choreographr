//! Keystore command handlers for [`DaemonState`] — the `DaemonCommand` arms
//! for `Unlock` / `BindKeystore` / `Lock` / `SaveCredential`, their free-fn
//! inners, the TOFU binding helpers, and the shared `unlock_tail`.
//!
//! This follows the child-module pattern of `daemon/subscriber_handlers.rs`
//! and `daemon/image_provider.rs`: `pub(super)` methods on `DaemonState`
//! dispatched from `handle_command` in the parent, so `daemon.rs` stays
//! focused on session CRUD, accounts, and the catalog. The pure keystore
//! SEMANTICS (binding adoption/verification, crypto, blob storage) still live
//! in the `choreo-keystore` crate — this module is the daemon's ORCHESTRATION
//! of them: ordering targeted replies ahead of transition broadcasts,
//! maintaining the cached `keystore_bound` flag, and unlocking daemon state
//! (credentials, accounts).
use super::DaemonState;
use crate::accounts::{AccountConfig, AccountManager};
use crate::broadcast::ReplyTarget;
use crate::db;
use choreo_keystore::ServiceCredential;
use choreo_proto::DaemonMessageType;
use std::collections::HashMap;
use std::io;
use std::sync::mpsc;
use tracing::{debug, info, warn};
use zeroize::{Zeroize, Zeroizing};

/// Failure of a keystore unlock/bind/add-credential operation, with the
/// UNBOUND case carried structurally so the connection layer can answer with
/// the distinct `DaemonMessageType::KeystoreUnbound` (guiding the client to
/// auto-bind) instead of the generic wrong-key `LockedError`. Kept as an enum
/// rather than a string sentinel because string matching on error text is how
/// such distinctions silently rot.
#[derive(Debug, thiserror::Error)]
pub enum KeystoreOpError {
    /// The daemon's keystore has no binding at all: the verify-only `Unlock`
    /// path must not adopt a key, so it fails with this.
    #[error(
        "keystore not initialized — no key is bound to this daemon yet; it will be bound automatically on next client connect"
    )]
    Unbound,
    /// Any other failure (wrong key, DB I/O, invalid key length, …).
    #[error("{0}")]
    Other(String),
}

impl DaemonState {
    /// Enqueue the TARGETED reply carried by `reply` (which owns the acting
    /// client's sink, the request id, and the daemon-wide lag counter), then
    /// ACK the command loop.
    ///
    /// This is the mechanism that makes the ORDERING INVARIANT real: the reply
    /// is enqueued HERE, by this thread, into the client's FIFO writer queue
    /// before any lock-state broadcast — so the broadcast can never overtake it
    /// (the connection thread's mpsc handoff does NOT order against a broadcast
    /// this thread makes to the same sink). A missing target (no acting client)
    /// logs and drops the reply, but still ACKs so the caller never deadlocks.
    fn send_targeted_ack(
        reply: Option<ReplyTarget>,
        inner: DaemonMessageType,
        ack: &mpsc::Sender<()>,
    ) {
        if let Some(target) = reply {
            target.send(inner);
        } else {
            warn!(
                ?inner,
                "no reply target for targeted keystore reply; dropping reply"
            );
        }
        let _ = ack.send(());
    }

    /// Enqueue the standard `CredentialAddFailed` targeted reply for the
    /// `save_credential` failure paths, then ACK the command loop. Every
    /// rejection path of `AddCredential` fails through this one method so
    /// the ORDERING INVARIANT cannot be dropped by a future edit.
    fn send_credential_add_failed(
        service: &str,
        error: String,
        reply: Option<ReplyTarget>,
        ack: &mpsc::Sender<()>,
    ) {
        Self::send_targeted_ack(
            reply,
            DaemonMessageType::CredentialAddFailed {
                service: service.to_string(),
                error,
            },
            ack,
        );
    }

    /// Attempt to unlock the daemon with the given private key.
    ///
    /// VERIFY-ONLY: on an unbound keystore this must NOT adopt the key — the
    /// client gets `KeystoreUnbound` and auto-binds instead (binding happens
    /// exclusively via `BindKeystore`). On a bound keystore the key is
    /// verified and the shared unlock tail runs.
    pub(super) fn handle_unlock(
        &mut self,
        private_key: Vec<u8>,
        reply: Option<ReplyTarget>,
        ack: &mpsc::Sender<()>,
    ) {
        info!("Unlock attempt");
        // Capture the pre-unlock lock state so the transition broadcast below
        // fires only on a REAL locked→unlocked change (a re-unlock of an
        // already-unlocked daemon is a no-op for the banner, not a spammy
        // repeat). `handle_unlock_inner` -> `unlock_tail` clears `locked` on
        // success.
        let was_locked = self.locked;
        let result = handle_unlock_inner(self, private_key);
        // ORDERING INVARIANT: the targeted reply MUST be serialized to the
        // acting client's socket BEFORE the lock-state broadcast — the
        // client's key-recording correctness keys on the targeted reply. Both
        // travel in the SAME per-client FIFO writer queue, and the reply is
        // enqueued first HERE, so the broadcast can never overtake it.
        let reply_msg = match &result {
            Ok(()) => DaemonMessageType::Unlocked,
            Err(e) => unlock_error_reply(e),
        };
        Self::send_targeted_ack(reply, reply_msg, ack);
        // A successful unlock is a lock-state transition: fan it out to ALL
        // activity subscribers (the acting client already has its targeted
        // `Unlocked` queued; the duplicate is idempotent) so every connected
        // UI clears its lock banner — e.g. client B unlocking updates client
        // A's status bar.
        if result.is_ok() && was_locked {
            self.broadcast_keystore_state();
        }
        info!("Unlock result: success={}", result.is_ok());
    }

    /// Establish (TOFU-bind) the keystore binding — the ONLY path that can
    /// create it (`ClientMessageType::BindKeystore`). On an unbound keystore the
    /// presented key is ADOPTED (loud `KEYSTORE BOUND` log) and the shared
    /// unlock tail runs (bulk decrypt is a no-op on a fresh keystore, loads
    /// accounts, sets locked=false); on an already-bound keystore the key is
    /// verified with the existing wrong-key rejection semantics — no unlock,
    /// no overwrite.
    pub(super) fn handle_bind_keystore(
        &mut self,
        key: Vec<u8>,
        reply: Option<ReplyTarget>,
        ack: &mpsc::Sender<()>,
    ) {
        info!("BindKeystore attempt");
        let was_locked = self.locked;
        let result = handle_bind_keystore_inner(self, key);
        // ORDERING INVARIANT (see handle_unlock): the targeted `Bound`
        // confirmation is enqueued BEFORE the lock-state broadcast — the
        // client records the fresh bind key on this targeted reply.
        // `handle_bind_keystore_inner` ADOPTS on an unbound keystore (TOFU —
        // the sole adopt path), so `KeystoreOpError::Unbound` is unreachable
        // here; `unlock_error_reply` only ever yields `LockedError` on this
        // path. The arm is left to the shared helper rather than spelled out
        // as a match arm that advertises a behavior that cannot happen.
        let reply_msg = match &result {
            Ok(()) => DaemonMessageType::Bound,
            Err(e) => unlock_error_reply(e),
        };
        Self::send_targeted_ack(reply, reply_msg, ack);
        if result.is_ok() && was_locked {
            self.broadcast_keystore_state();
        }
        info!("BindKeystore result: success={}", result.is_ok());
    }

    /// Lock the daemon's keystore (`/lock`): clear every decrypted in-memory
    /// credential and its cached provider, flip `locked` back to `true`, and
    /// broadcast the `Locked` state to all activity subscribers.
    ///
    /// This is intentionally soft/cooperative: sessions are untouched (they
    /// stay browsable — only inference requires credentials), so locking just
    /// drops cleartext from memory and re-latches the banner. The encrypted
    /// blobs remain in the DB and re-decrypt on the next Unlock.
    pub(super) fn handle_lock(&mut self, reply: &mpsc::Sender<Result<(), String>>) {
        let was_locked = self.locked;
        // Wipe decrypted credentials (and their derived providers) from memory
        // now that the keystore is locked. `credentials` holds the plaintext
        // ServiceCredentials; dropping them is what "locked" means for this
        // daemon. Capture the count BEFORE the clear so the log reports what
        // was actually wiped, not a hardcoded zero.
        let credentials_cleared = self.credentials.len();
        self.credentials.clear();
        // The cached OAuth access tokens are derived from the credentials
        // being wiped, so they are cleared in the same step: a locked daemon
        // holds no cleartext bearer either.
        let access_tokens_cleared = self.access_tokens.len();
        self.access_tokens.clear();
        // No daemon-side provider cache to wipe anymore — instead invalidate
        // every live session's cached client so it rebuilds (against fresh
        // credentials) on its next request. Sessions stay browsable; only
        // inference re-resolves.
        self.drop_session_clients(None);
        self.x_credentials = None;
        self.locked = true;
        info!(
            credentials_cleared,
            access_tokens_cleared,
            "keystore locked: in-memory credentials and access tokens cleared"
        );
        // The acting client gets its `send_to_writer` `Locked` reply from the
        // connection layer; this transition broadcast reaches every connected
        // client (the acting one included, harmlessly idempotent). Gated on
        // `keystore_bound`: a `/lock` against an UNBOUND daemon clears nothing
        // there was to clear and reports no transition — the status stays
        // `Unbound` — so broadcasting here would only re-assert the same
        // unbound state every client already latched.
        if !was_locked && self.keystore_bound {
            self.broadcast_keystore_state();
        }
        let _ = reply.send(Ok(()));
    }

    /// Encrypt a credential to the daemon's binding public key, persist it,
    /// and update the in-memory credential map.
    ///
    /// The keystore must already be UNLOCKED: a locked daemon REJECTS the
    /// request ("unlock the keystore first") rather than implicitly unlocking
    /// — the add path no longer has any lock-state side effect. The credential
    /// arrives as typed plaintext (no key rides the request); the daemon reads
    /// its stored binding public key and encrypts the serialized credential to
    /// it before persisting, so the on-disk blob is readable only by an unlock
    /// that presents the binding's private key.
    pub(super) fn handle_save_credential(
        &mut self,
        service: String,
        credential: ServiceCredential,
        reply: Option<ReplyTarget>,
        ack: &mpsc::Sender<()>,
    ) {
        // Require an unlocked keystore. Adding while locked is refused so the
        // handler never changes lock state and never publishes a decrypted
        // credential into a keystore the operator believes is locked.
        if self.locked {
            info!(service = %service, "AddCredential rejected: keystore is locked");
            Self::send_credential_add_failed(
                &service,
                "unlock the keystore first".to_string(),
                reply,
                ack,
            );
            return;
        }

        // The binding public key is what the daemon encrypts to. An unbound
        // keystore cannot have reached the unlocked state (bind is the sole
        // adopt path and leaves it unlocked), so this is a defensive branch,
        // not a reachable first-run path.
        let binding = match db::get_keystore_binding(&self.db) {
            Ok(Some(pk)) => pk,
            Ok(None) => {
                Self::send_credential_add_failed(
                    &service,
                    "keystore is not bound; bind it before adding credentials".to_string(),
                    reply,
                    ack,
                );
                return;
            }
            Err(e) => {
                Self::send_credential_add_failed(
                    &service,
                    format!("failed to read keystore binding: {e}"),
                    reply,
                    ack,
                );
                return;
            }
        };

        // Serialize then encrypt to the binding. The plaintext is wrapped in
        // `Zeroizing` so the serialized secret bytes are wiped on every exit
        // path (the typed credential also zeroizes on drop).
        let plaintext = match postcard::to_allocvec(&credential) {
            Ok(p) => Zeroizing::new(p),
            Err(e) => {
                Self::send_credential_add_failed(
                    &service,
                    format!("failed to serialize credential: {e}"),
                    reply,
                    ack,
                );
                return;
            }
        };
        let blob = match choreo_keystore::crypto::encrypt_with_public_key(&binding, &plaintext) {
            Ok(b) => b,
            Err(e) => {
                Self::send_credential_add_failed(
                    &service,
                    format!("failed to encrypt credential: {e}"),
                    reply,
                    ack,
                );
                return;
            }
        };

        if let Err(e) = db::set_credential_blob(&self.db, &service, &blob) {
            Self::send_credential_add_failed(
                &service,
                format!("failed to save credential: {e}"),
                reply,
                ack,
            );
            return;
        }

        // In-memory bookkeeping: mirror an X credential for the `twitter`
        // service into the single `x_credentials` slot, and invalidate sessions
        // bound to an ApiKey service so they rebuild their client against the
        // new key lazily on their next request.
        if matches!(&credential, ServiceCredential::X { .. }) && service == "twitter" {
            self.x_credentials = Some(credential.clone());
        }
        if matches!(&credential, ServiceCredential::ApiKey { .. }) {
            self.drop_session_clients(Some(&service));
        }
        // A new credential replaces any previous one for this account, so any
        // cached access token derived from the old credential is stale and
        // must be dropped before the replacement is stored.
        self.access_tokens.remove(&service);
        self.credentials.insert(service.clone(), credential);

        info!(
            service = %service,
            "AddCredential: encrypted to the binding public key and persisted"
        );
        // ORDERING INVARIANT (see handle_unlock): the targeted CredentialAdded
        // reply is enqueued by THIS thread, so it precedes any other message to
        // this client's writer queue. AddCredential yields exactly ONE terminal
        // reply; there is no lock-state broadcast because the handler never
        // changes lock state.
        if let Some(target) = reply {
            target.send(DaemonMessageType::CredentialAdded { service });
        } else {
            warn!("no reply target for the targeted AddCredential reply; dropping reply");
        }
        let _ = ack.send(());
    }
}

fn handle_unlock_inner(
    state: &mut DaemonState,
    mut private_key: Vec<u8>,
) -> Result<(), KeystoreOpError> {
    let key = zeroized_key_or_wipe(&mut private_key)?;
    // Wipe the heap `Vec` copy as soon as the stack array exists; only `key`
    // is used below and the Zeroizing wrapper wipes it on every exit path.
    private_key.zeroize();

    // VERIFY-ONLY enforcement against the persisted keystore binding. An
    // UNBOUND keystore is an error (no adoption — binding is exclusively the
    // BindKeystore path); a bound keystore rejects any key whose derived
    // public key differs (surfaces as LockedError).
    verify_keystore_binding(state, &key)?;

    // Shared bulk-decrypt + accounts-load + provider-resolve tail — also run
    // by `handle_bind_keystore_inner`, so the unlock and bind paths cannot
    // drift.
    unlock_tail(state, &key).map_err(|e| KeystoreOpError::Other(e.to_string()))
}

/// `BindKeystore` inner: validate + zeroize the key, then TOFU-adopt (unbound
/// keystore) or verify (bound keystore), then run the shared unlock tail.
/// On a fresh keystore the bulk decrypt is a no-op, but running the tail
/// unconditionally keeps "bound and unlocked with this key" one code path.
fn handle_bind_keystore_inner(
    state: &mut DaemonState,
    mut key: Vec<u8>,
) -> Result<(), KeystoreOpError> {
    // Validate + zeroize-on-exit the stack array, then wipe the heap `Vec`
    // copy (same discipline as the unlock path): the heap bytes must not
    // survive in a freed allocation, so they are zeroized BEFORE the
    // Zeroizing array takes over the `key` name below.
    let key = {
        let arr = zeroized_key_or_wipe(&mut key)?;
        key.zeroize();
        arr
    };

    // Adopt-if-unbound-else-verify: the ONLY adopt path in the daemon.
    bind_keystore(state, &key)?;

    // Same tail as `Unlock`: loads accounts and clears `locked`, so a
    // successful bind leaves the daemon unlocked.
    unlock_tail(state, &key).map_err(|e| KeystoreOpError::Other(e.to_string()))
}

/// Validate a raw key Vec as exactly 32 bytes, returning it zeroized on the
/// stack. Shared by the unlock/bind inners so the length check and the
/// zeroize-on-error discipline cannot drift between the two paths.
fn zeroized_key_or_wipe(key: &mut Vec<u8>) -> Result<Zeroizing<[u8; 32]>, KeystoreOpError> {
    // Zeroizing makes the wipe structural: the stack array is zeroed on EVERY
    // exit — early returns, the `?` operator, even a panic — so no per-path
    // zeroize call can be forgotten by a future edit.
    if let Ok(k) = key.as_slice().try_into() {
        Ok(Zeroizing::new(k))
    } else {
        // The presented bytes are unusable, but still secret material —
        // wipe the heap copy before returning so a failed operation does
        // not leave the key lying in a freed allocation.
        key.zeroize();
        Err(KeystoreOpError::Other(
            "invalid key: expected exactly 32 bytes".to_string(),
        ))
    }
}

/// Map a [`KeystoreOpError`] to the wire reply the Unlock/Bind paths send —
/// one shared mapping for both call sites, so the `Unbound` →
/// `KeystoreUnbound` guidance text and the wrong-key → error-message shape
/// cannot drift.
fn unlock_error_reply(e: &KeystoreOpError) -> DaemonMessageType {
    match e {
        KeystoreOpError::Unbound => DaemonMessageType::KeystoreUnbound {
            error: KeystoreOpError::Unbound.to_string(),
        },
        KeystoreOpError::Other(e) => DaemonMessageType::LockedError { error: e.clone() },
    }
}

/// TOFU keystore binding ADOPTION — used ONLY by the `BindKeystore` path.
/// Unbound keystore (no stored binding): ADOPT the presented key — derive its
/// X25519 public key and persist it. This is a one-time, security-relevant
/// event, so the log is deliberately LOUD. A bound keystore is verified
/// instead: a mismatching key is rejected (no overwrite, no unlock).
fn bind_keystore(state: &mut DaemonState, key: &[u8; 32]) -> Result<(), KeystoreOpError> {
    let binding = db::get_keystore_binding(&state.db)
        .map_err(|e| KeystoreOpError::Other(format!("failed to read keystore binding: {e}")))?;
    // x25519_dalek: the public key is what the binding stores (and what the
    // daemon encrypts credential blobs to), never the private key itself.
    let derived = x25519_dalek::PublicKey::from(&x25519_dalek::StaticSecret::from(*key));
    match binding {
        None => {
            db::set_keystore_binding(&state.db, derived.as_bytes()).map_err(|e| {
                KeystoreOpError::Other(format!("failed to persist keystore binding: {e}"))
            })?;
            // One-time, security-relevant event: LOUD on purpose. Operators
            // must be able to see when a daemon's keystore became bound to a
            // key (any later unlock requires exactly that key).
            info!(
                "KEYSTORE BOUND: adopted unlock key via BindKeystore (TOFU); \
                 public key (hex) = {} — all future Unlock attempts must \
                 present this key, others are rejected",
                hex::encode(derived.as_bytes())
            );
            // Flip the cached status flag the command loop reads to derive the
            // authoritative `KeystoreState` (`Unbound` → now bound). This is
            // the sole adoption path, so it can never drift from the persisted
            // binding.
            state.keystore_bound = true;
            Ok(())
        }
        Some(stored) if stored == *derived.as_bytes() => {
            // Re-bind with the already-bound key: idempotent success (the
            // unlock tail below still runs, which is the useful part).
            debug!("BindKeystore: key matches the existing binding");
            Ok(())
        }
        Some(_) => Err(KeystoreOpError::Other(
            "keystore is already bound and the presented key does not match; \
             refusing to overwrite the binding"
                .to_string(),
        )),
    }
}

/// VERIFY-ONLY keystore-binding enforcement for the `Unlock` path. `Unlock`
/// may never CREATE a binding:
///
/// * Unbound keystore (no stored binding): `KeystoreOpError::Unbound` — the
///   connection layer answers `KeystoreUnbound` and the client auto-binds
///   with a fresh key instead of this path silently adopting whatever was
///   presented.
/// * Bound keystore: derive the presented key's public key and compare
///   against the binding; a mismatch is `KeystoreOpError::Other`
///   (`LockedError`).
pub(crate) fn verify_keystore_binding(
    state: &DaemonState,
    key: &[u8; 32],
) -> Result<(), KeystoreOpError> {
    let binding = db::get_keystore_binding(&state.db)
        .map_err(|e| KeystoreOpError::Other(format!("failed to read keystore binding: {e}")))?;
    // x25519_dalek: the public key is what the binding stores (and what the
    // daemon encrypts credential blobs to), never the private key itself.
    let derived = x25519_dalek::PublicKey::from(&x25519_dalek::StaticSecret::from(*key));
    match binding {
        None => {
            debug!(
                "verify_keystore_binding: keystore has no binding; refusing verify-only operation"
            );
            Err(KeystoreOpError::Unbound)
        }
        Some(stored) if stored == *derived.as_bytes() => Ok(()),
        Some(_) => Err(KeystoreOpError::Other(
            "unlock key does not match the daemon's keystore binding".to_string(),
        )),
    }
}

/// The unlock TAIL run by `handle_unlock_inner` and
/// `handle_bind_keystore_inner`: bulk-decrypt every stored credential blob
/// with `key`, load accounts from TOML, and flip `locked` to `false`
/// (in-memory only). Factored out so the unlock and bind paths cannot drift.
pub(crate) fn unlock_tail(state: &mut DaemonState, key: &[u8; 32]) -> io::Result<()> {
    let credentials = decrypt_credential_blobs(state, key)?;
    finish_unlock(state, credentials)
}

/// Read every credential blob from the DB and decrypt them all with `key`.
/// Failure-tolerant per blob: a bad decrypt/decode is logged and skipped, not
/// fatal (see the bulk-unlock design: a poisoned blob must not lock out the
/// whole keystore).
fn decrypt_credential_blobs(
    state: &DaemonState,
    key: &[u8; 32],
) -> io::Result<HashMap<String, ServiceCredential>> {
    let blobs = db::get_all_credential_blobs(&state.db)
        .map_err(|e| io::Error::other(format!("failed to read credentials from database: {e}")))?;
    info!("Unlock: {} credential blobs in DB", blobs.len());

    let mut credentials = HashMap::new();
    let mut decrypt_failures = 0usize;
    for (service, blob) in &blobs {
        match choreo_keystore::crypto::decrypt_with_private_key(key, blob) {
            Ok(plaintext) => match postcard::from_bytes::<ServiceCredential>(&plaintext) {
                Ok(cred) => {
                    credentials.insert(service.clone(), cred);
                }
                Err(e) => {
                    warn!("Unlock: failed to decode credential '{}': {e}", service);
                    decrypt_failures += 1;
                }
            },
            Err(e) => {
                warn!("Unlock: failed to decrypt credential '{}': {e}", service);
                decrypt_failures += 1;
            }
        }
    }
    // Summary here (not in `finish_unlock`, which may never run on a DB
    // error): the log reports what this pass actually decrypted.
    info!(
        "Unlock: decrypted {}/{} credentials ({} failures): {:?}",
        credentials.len(),
        blobs.len(),
        decrypt_failures,
        credentials.keys().collect::<Vec<_>>()
    );
    Ok(credentials)
}

/// The FALLIBILITY BOUNDARY of the unlock tail: everything that can fail is
/// done on LOCALS first (`credentials` decrypted, accounts loaded, default
/// account resolved), and only then are the daemon-state fields assigned in
/// one go — so a FAILING unlock leaves the daemon EXACTLY as it was: no
/// decrypted plaintext is published into `state.credentials` with `locked`
/// still `true` (the previous pre-load order leaked freshly decrypted
/// secrets into reachable state on an account-load failure, which a later
/// `/lock` wouldn't even count as it only clears what it sees). The caller
/// methods broadcast `Unlocked` on the locked→unlocked transition.
fn finish_unlock(
    state: &mut DaemonState,
    credentials: HashMap<String, ServiceCredential>,
) -> io::Result<()> {
    // Load accounts from the daemon's OWN accounts path — the same file
    // `state.accounts` was loaded from (`AccountManager::path`), NOT the
    // global config-dir assumption: an embedder that opened state with an
    // explicit accounts path (see `daemon/open.rs`) gets ITS path reread,
    // not whatever `dirs::config_dir()` returns here. Loaded FIRST and on a
    // local, so a failure leaves the daemon exactly as it was (see the doc
    // above).
    let accounts_path = state.accounts.path().to_path_buf();
    let mut accounts = AccountManager::load(&accounts_path)
        .map_err(|e| io::Error::other(format!("failed to load accounts: {e}")))?;

    // If no accounts configured but an "openai" credential exists, create a
    // default account automatically so the user doesn't have to set one up.
    // Done on the LOCAL manager too: only the final assignments below publish
    // anything.
    if accounts.is_empty() && credentials.contains_key("openai") {
        let default_config = AccountConfig::simple("default", "openai");
        if let Err(e) = accounts.add(default_config) {
            tracing::warn!("failed to create default account: {e}");
        }
    }

    // The keystore is now fully decrypted into memory (credentials,
    // accounts): commit ALL of it to daemon state in one shot — this is the
    // single authoritative unlocked point shared by the `Unlock` and
    // `BindKeystore` paths.
    state.x_credentials = credentials
        .get("twitter")
        .filter(|c| matches!(c, ServiceCredential::X { .. }))
        .cloned();
    state.credentials = credentials;
    state.accounts = accounts;
    state.locked = false;

    // Log the accounts summary AFTER the assignment (the credential decrypt
    // summary is logged by `decrypt_credential_blobs`, before publishing).
    let account_names: Vec<String> = state
        .accounts
        .all_configs()
        .iter()
        .map(|c| c.name.clone())
        .collect();
    info!("Unlock: accounts loaded: {:?}", account_names);

    // No bulk provider resolution anymore: there is no daemon-side provider
    // cache. Each session builds its client lazily on its next request
    // against its own registry (see `SessionState::resolve_provider`).
    for config in state.accounts.all_configs() {
        info!(
            "Unlock: account '{}': has_credential={}",
            config.name,
            state.credentials.contains_key(&config.name)
        );
    }
    info!("Unlock: keystore decrypted; sessions will rebuild providers lazily on next use");

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::super::tests::{make_daemon_state, test_pub};
    use super::*;

    #[test]
    fn failed_unlock_tail_publishes_no_decrypted_state() {
        let (mut state, _rx) = make_daemon_state();
        let key: [u8; 32] = [7u8; 32];
        let blob = choreo_keystore::crypto::encrypt_with_public_key(
            &test_pub(key),
            &postcard::to_allocvec(&ServiceCredential::ApiKey {
                key: "sk-test".to_string(),
            })
            .unwrap(),
        )
        .unwrap();
        db::set_credential_blob(&state.db, "openai", &blob).unwrap();

        // Sabotage the accounts file the tail must load AFTER the bulk
        // decrypt: the decrypt succeeds (the blob above is valid), but the
        // accounts load FAILS — exactly the ordering that used to leave the
        // freshly decrypted plaintext in `state.credentials` with `locked`
        // still `true`. The tombstone proves the old partial publication.
        let accounts_path = state.accounts.path().to_path_buf();
        std::fs::write(&accounts_path, "definitely not valid TOML [[[").unwrap();

        let err = unlock_tail(&mut state, &key)
            .expect_err("an unloadable accounts file must fail the unlock tail");
        assert!(err.to_string().contains("accounts"), "got: {err}");

        // NOTHING was published: no decrypted credential leaked into
        // reachable state, lock state is untouched, and the account manager
        // was not replaced.
        assert!(
            state.credentials.is_empty(),
            "a failed tail must not publish decrypted credentials"
        );
        assert!(state.x_credentials.is_none(), "X creds must stay empty");
        assert!(state.locked, "a failed tail must not unlock the daemon");
        assert!(state.accounts.is_empty(), "accounts must stay as they were");
    }
}
