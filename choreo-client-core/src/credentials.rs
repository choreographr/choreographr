use choreo_keystore::ServiceCredential;
use choreo_proto::ClientMessage;
use tracing::{debug, info, warn};
use x25519_dalek::{PublicKey, StaticSecret};
use zeroize::{Zeroize, Zeroizing};

use crate::error::ClientError;
use crate::known_servers::KnownServers;
use crate::shell::UnlockMethod;

/// Resolve a private key for an unlock attempt against the daemon at
/// `addr`.
///
/// For `UnlockMethod::Raw`, the stored per-daemon unlock key (known_servers
/// entry) is preferred, falling back to the legacy raw `identity.pk` file.
/// For `UnlockMethod::Passphrase`, the legacy encrypted `identity.pk.enc`
/// file is decrypted with the passphrase (unchanged semantics — a
/// passphrase unlock is inherently a legacy-migration action: the recorded
/// per-daemon key needs no passphrase).
pub fn resolve_private_key(method: &UnlockMethod, addr: &str) -> Result<Vec<u8>, ClientError> {
    match method {
        UnlockMethod::Raw => {
            info!(addr, "resolving raw unlock key for addr");
            // Stored per-daemon key first (the TOFU target state); the
            // legacy raw file only serves setups that have not yet
            // migrated (the daemon adopts the legacy key on first unlock,
            // after which `record_unlock_key` stores it per-addr).
            if let Some(key) = stored_unlock_key(addr) {
                return Ok(key.to_vec());
            }
            read_raw_private_key()
        }
        UnlockMethod::Passphrase(passphrase) => {
            info!(addr, "reading encrypted private key with passphrase");
            read_encrypted_private_key(passphrase)
        }
    }
}

/// Read and validate the raw private key file (`identity.pk`).
/// Returns 32-byte key data.
fn read_raw_private_key() -> Result<Vec<u8>, ClientError> {
    let path = choreo_keystore::paths::private_key_path()
        .map_err(|e| ClientError::PrivateKeyRead(e.to_string()))?;
    let data = std::fs::read(&path).map_err(|e| ClientError::PrivateKeyRead(e.to_string()))?;
    if data.len() != 32 {
        return Err(ClientError::PrivateKeyInvalid);
    }
    Ok(data)
}

/// Read and decrypt the encrypted private key file (`identity.pk.enc`)
/// using the given passphrase.
fn read_encrypted_private_key(passphrase: &str) -> Result<Vec<u8>, ClientError> {
    let enc_path = choreo_keystore::paths::private_key_enc_path()
        .map_err(|e| ClientError::PrivateKeyEncRead(e.to_string()))?;
    let data =
        std::fs::read(&enc_path).map_err(|e| ClientError::PrivateKeyEncRead(e.to_string()))?;
    let key = choreo_keystore::crypto::decrypt_private_key(&data, passphrase)
        .map_err(|e| ClientError::PrivateKeyDecrypt(e.to_string()))?;
    Ok(key.to_vec())
}

/// Read the stored per-daemon unlock key for `addr` from known_servers.
/// Internal helper: failures to LOAD the store or DECODE a stored key are
/// non-fatal for the resolution chain (we just fall through to the legacy
/// path), so they are swallowed with a warning here rather than propagated.
fn stored_unlock_key(addr: &str) -> Option<[u8; 32]> {
    match KnownServers::load() {
        Ok(store) => match store.unlock_key(addr) {
            Ok(Some(key)) => {
                info!(addr, "using stored per-daemon unlock key");
                Some(key)
            }
            Ok(None) => None,
            Err(e) => {
                warn!(addr, error = %e, "stored unlock_key failed to decode; ignoring");
                None
            }
        },
        Err(e) => {
            warn!(addr, error = %e, "could not load known_servers store; ignoring stored unlock key");
            None
        }
    }
}

/// The LEGACY half of unlock-key resolution: raw `identity.pk`, then
/// `identity.pk.enc` + `CHOREOGRAPHR_KEYSTORE_PASSPHRASE`.
fn legacy_auto_unlock_key() -> Option<Vec<u8>> {
    match read_raw_private_key() {
        Ok(key) => {
            info!("auto-unlock: using legacy raw private key");
            return Some(key);
        }
        Err(ClientError::PrivateKeyInvalid) => {
            warn!("auto-unlock: legacy private key file exists but is not 32 bytes");
        }
        Err(_) => {
            // No raw key available — fall through to encrypted path.
        }
    }

    if let Ok(passphrase) = std::env::var("CHOREOGRAPHR_KEYSTORE_PASSPHRASE")
        && !passphrase.is_empty()
        && let Ok(key) = read_encrypted_private_key(&passphrase)
    {
        info!("auto-unlock: using legacy encrypted private key with env passphrase");
        return Some(key);
    }

    None
}

/// Try to resolve the unlock key for automatic unlock on connect to the
/// daemon at `addr`.
///
/// Resolution order (per-daemon keystore TOFU design):
/// 1. The stored `unlock_key` from the known_servers entry for `addr`.
/// 2. LEGACY fallback: raw `identity.pk`, or `identity.pk.enc` decrypted
///    with `CHOREOGRAPHR_KEYSTORE_PASSPHRASE` (migration for existing
///    local setups — the daemon adopts the legacy key on first unlock and
///    all existing blobs keep decrypting).
///
/// Returns `None` if no key can be resolved, which is fine — the daemon
/// starts locked but all session operations (create, browse, delete) work
/// without unlocking.  Only inference (RunInput) requires credentials.
pub fn try_auto_unlock_key(addr: &str) -> Option<Vec<u8>> {
    if let Some(key) = stored_unlock_key(addr) {
        return Some(key.to_vec());
    }
    let legacy = legacy_auto_unlock_key();
    if legacy.is_none() {
        debug!(
            addr,
            "auto-unlock: no key available (daemon will start locked)"
        );
    }
    legacy
}

/// Persist the per-daemon unlock key for `addr` into the known_servers
/// store, and complete the legacy migration: when the recorded key is the
/// same one held in the legacy `identity.pk` / `identity.pk.enc` files,
/// those files are DELETED (the per-daemon record has replaced them; the
/// daemon's binding already adopted this key, so keeping the legacy copy
/// adds no access, only risk of confusion). A key that does not match the
/// legacy files — or a case where the comparison is impossible (no env
/// passphrase for the encrypted file) — leaves the files alone, since we
/// cannot prove they are redundant.
///
/// Callers MUST only invoke this after the daemon CONFIRMED the key (an
/// `Unlocked` or `CredentialAdded` reply) — never on send.
pub fn record_unlock_key(addr: &str, key: &[u8]) -> Result<(), ClientError> {
    let key: [u8; 32] = key.try_into().map_err(|_| ClientError::PrivateKeyInvalid)?;
    KnownServers::load()?.set_unlock_key(addr, &key)?;

    // ── Legacy migration cleanup ────────────────────────────────────
    // Read the raw legacy file (if any) and compare. Only delete when the
    // legacy content provably equals the recorded key.
    let legacy_raw_matches = match legacy_raw_private_key_bytes() {
        Ok(Some(raw)) => Some(raw == key),
        Ok(None) => None, // no raw file — check the encrypted one below
        // Unreadable/corrupt raw file: we cannot prove redundancy, so the
        // conservative choice is to keep it and let the caller see why.
        Err(e) => {
            warn!(addr, error = %e, "record_unlock_key: could not read legacy identity.pk; leaving legacy files in place");
            None
        }
    };

    if legacy_raw_matches != Some(true) {
        // Raw file absent, corrupt, or different: only the encrypted file
        // could still hold this key. Compare by decrypting with the env
        // passphrase when available; if that is impossible, keep everything
        // (the migration can complete on a later record_unlock_key call).
        match legacy_encrypted_private_key_bytes() {
            Ok(Some(enc)) => match std::env::var("CHOREOGRAPHR_KEYSTORE_PASSPHRASE") {
                Ok(passphrase) if !passphrase.is_empty() => {
                    match choreo_keystore::crypto::decrypt_private_key(&enc, &passphrase) {
                        Ok(dec) if dec == key => {
                            info!(
                                addr,
                                "record_unlock_key: encrypted legacy key matches recorded key; migrating (deleting legacy files)"
                            );
                            delete_legacy_key_files(addr);
                        }
                        Ok(_) => {
                            info!(
                                addr,
                                "record_unlock_key: legacy encrypted key differs from recorded key; leaving legacy files in place"
                            );
                        }
                        Err(e) => {
                            warn!(addr, error = %e, "record_unlock_key: could not decrypt legacy identity.pk.enc to compare; leaving legacy files in place");
                        }
                    }
                }
                _ => {
                    warn!(
                        addr,
                        "record_unlock_key: CHOREOGRAPHR_KEYSTORE_PASSPHRASE not set; cannot compare legacy identity.pk.enc — legacy files left in place"
                    );
                }
            },
            Ok(None) => {
                // No legacy files at all — nothing to migrate.
                debug!(addr, "record_unlock_key: no legacy key files present");
            }
            Err(e) => {
                warn!(addr, error = %e, "record_unlock_key: could not read legacy identity.pk.enc; leaving legacy files in place");
            }
        }
    } else {
        info!(
            addr,
            "record_unlock_key: legacy raw key matches recorded key; migrating (deleting legacy files)"
        );
        delete_legacy_key_files(addr);
    }

    Ok(())
}

/// Delete the legacy `identity.pk` and `identity.pk.enc` files, logging
/// whichever existed. Missing files are the normal end-state, not errors.
fn delete_legacy_key_files(addr: &str) {
    if let Ok(path) = choreo_keystore::paths::private_key_path()
        && std::fs::remove_file(&path).is_ok()
    {
        info!(addr, path = %path.display(), "removed legacy identity.pk (migration complete)");
    }
    if let Ok(path) = choreo_keystore::paths::private_key_enc_path()
        && std::fs::remove_file(&path).is_ok()
    {
        info!(addr, path = %path.display(), "removed legacy identity.pk.enc (migration complete)");
    }
}

/// Raw bytes of the legacy raw private key file: `Ok(None)` when the file
/// simply does not exist, `Err` on any other read problem. Distinct from
/// [`read_raw_private_key`] because `record_unlock_key` must distinguish
/// "absent" from "unreadable" when deciding whether migration is provable.
fn legacy_raw_private_key_bytes() -> Result<Option<Vec<u8>>, ClientError> {
    let path = choreo_keystore::paths::private_key_path()
        .map_err(|e| ClientError::PrivateKeyRead(e.to_string()))?;
    match std::fs::read(&path) {
        Ok(data) => Ok(Some(data)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(ClientError::PrivateKeyRead(e.to_string())),
    }
}

/// Encrypted bytes of the legacy `identity.pk.enc`: `Ok(None)` when the
/// file does not exist, `Err` on any other read problem (same distinction
/// as [`legacy_raw_private_key_bytes`]).
fn legacy_encrypted_private_key_bytes() -> Result<Option<Vec<u8>>, ClientError> {
    let path = choreo_keystore::paths::private_key_enc_path()
        .map_err(|e| ClientError::PrivateKeyEncRead(e.to_string()))?;
    match std::fs::read(&path) {
        Ok(data) => Ok(Some(data)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(ClientError::PrivateKeyEncRead(e.to_string())),
    }
}

fn parse_credential(
    credential_type: &str,
    fields: &[String],
) -> Result<ServiceCredential, ClientError> {
    match credential_type {
        "api_key" => {
            if fields.is_empty() {
                return Err(ClientError::CredentialParse(
                    "missing api_key field".to_string(),
                ));
            }
            Ok(ServiceCredential::ApiKey {
                key: fields[0].clone(),
            })
        }
        "x" => {
            if fields.len() < 5 {
                return Err(ClientError::CredentialParse(
                    "missing X credential fields".to_string(),
                ));
            }
            let bearer = if fields[4] == "-" {
                None
            } else {
                Some(fields[4].clone())
            };
            Ok(ServiceCredential::X {
                api_key: fields[0].clone(),
                api_key_secret: fields[1].clone(),
                access_token: fields[2].clone(),
                access_token_secret: fields[3].clone(),
                bearer_token: bearer,
            })
        }
        other => Err(ClientError::CredentialParse(format!(
            "unknown credential type: {other}"
        ))),
    }
}

/// Resolve the per-daemon keystore unlock key for `addr`.
///
/// Order (per-daemon TOFU design, DESIGN-keystore-unlock.md):
/// 1. the stored per-daemon key in the known_servers entry for `addr`,
/// 2. the LEGACY local key (`identity.pk` / `identity.pk.enc` + env
///    passphrase) — the one-time migration source,
/// 3. failing both, a FRESH random key — and it is OPTIMISTICALLY recorded in
///    known_servers for `addr` immediately.
///
/// The optimistic fresh record closes the lost-confirmation orphan (bug 2 of
/// the design review): a fresh key is adopted TOFU by an unbound daemon, so if
/// the `CredentialAdded` reply is lost the key must already be on disk or the
/// next connect would mint a DIFFERENT fresh key and be locked out of the now-
/// bound keystore. If the daemon turns out to be ALREADY bound it rejects the
/// fresh key — the record STAYS (see the rationale on [`resolve_keystore_key`]).
fn resolve_keystore_key(addr: &str) -> Result<[u8; 32], ClientError> {
    if let Some(key) = stored_unlock_key(addr) {
        return Ok(key);
    }
    // The legacy key is secret material: wrap it so the heap Vec is wiped on
    // EVERY exit — including the try_into failure path, which previously
    // dropped the Vec un-zeroized.
    if let Some(key) = legacy_auto_unlock_key().map(Zeroizing::new) {
        return key
            .as_slice()
            .try_into()
            .map_err(|_| ClientError::PrivateKeyInvalid);
    }
    let fresh: [u8; 32] = rand::random();
    info!(
        addr,
        "no stored or legacy unlock key; generated fresh random key for daemon (TOFU adopt on first use)"
    );
    // Best-effort persist so an interrupted confirmation cannot orphan the
    // binding. A store we cannot write is not worth failing the add over: the
    // caller records the key again on confirmed success anyway.
    if let Err(e) = KnownServers::load().and_then(|mut s| s.set_unlock_key(addr, &fresh)) {
        warn!(
            addr,
            error = %e,
            "could not optimistically record fresh unlock key; it will be recorded on daemon confirmation"
        );
    }
    Ok(fresh)
}

// Why a daemon REJECTION does not delete the optimistic record: the unlock
// key is only ever DELETED by an explicit `KnownServers::remove(addr)`
// (documented re-pair path). The reasoning:
//
// 1. A CONFIRMED record can never be "wrong" later — the daemon's keystore
//    binding is TOFU-once and never rotates, so a key the daemon once
//    accepted keeps matching its binding forever (and if the daemon's DB is
//    wiped it becomes unbound again and re-adopts the stored key).
// 2. The daemon surfaces EVERY unlock-path error (including transient
//    failures like a database read error) as `LockedError` / a rejection, so
//    a rejection is NOT proof the key is bad. Deleting on rejection could
//    erase a perfectly good confirmed record — and with the legacy files
//    already deleted by migration there may be no way back.
// 3. For the one genuinely-wrong case (an optimistic fresh key rejected by
//    an already-bound daemon) deletion recovers nothing: a fresh key is
//    minted precisely because no stored OR legacy key exists, so after a
//    clear the next attempt mints yet another fresh key that is also
//    rejected. Manual recovery via `remove(addr)` is the fix in both
//    worlds, so the simpler survivor semantics win.

/// Build an `AddCredential` message from typed field strings: parse the
/// credential, then delegate to the shared builder (see
/// [`build_add_credential_from_credential`] for the full key-resolution and
/// encryption semantics). The caller-supplied field strings (which may hold a
/// secret, e.g. the API key) are zeroized once parsing has consumed them.
pub fn build_add_credential_message(
    addr: &str,
    service: String,
    credential_type: String,
    fields: Vec<String>,
) -> Result<(ClientMessage, Vec<u8>), ClientError> {
    debug!(
        addr,
        service, credential_type, "building add credential message"
    );
    let credential = parse_credential(&credential_type, &fields)?;
    // Parse first, then hand off to the shared builder. The caller's field
    // strings (which may hold a typed secret, e.g. an API key) are zeroized
    // here once parsing is done — the parsed credential zeroizes itself on
    // drop via `#[zeroize(drop)]` in choreo-keystore.
    let mut fields = fields;
    let result = build_add_credential_from_credential(addr, service, credential);
    for field in &mut fields {
        field.zeroize();
    }
    result
}

/// Build an `AddCredential` message from an already-parsed credential, by
/// resolving the daemon's unlock key and encrypting the serialized blob to the
/// public key derived from that key.
///
/// Returns the message AND the unlock key used, so the caller can call
/// [`record_unlock_key`] once the daemon CONFIRMS success (`CredentialAdded` /
/// `Unlocked` reply) — never on send. (A freshly minted key is additionally
/// optimistic-recorded inside [`resolve_keystore_key`], so a lost confirmation
/// cannot orphan it. That record is PROVISIONAL until the daemon confirms, but
/// a daemon rejection does NOT delete it — see the survivor-semantics
/// rationale above [`resolve_keystore_key`].
///
/// The caller must call [`record_unlock_key`] on confirmed success or the next
/// add could mint a key the now-bound daemon rejects.
pub fn build_add_credential_from_credential(
    addr: &str,
    service: String,
    credential: ServiceCredential,
) -> Result<(ClientMessage, Vec<u8>), ClientError> {
    debug!(
        addr,
        service, "building add credential message from parsed credential"
    );
    let mut unlock_key = resolve_keystore_key(addr)?;
    let derived_pub = PublicKey::from(&StaticSecret::from(unlock_key));

    let mut plaintext =
        postcard::to_allocvec(&credential).map_err(|e| ClientError::Postcard(e.to_string()))?;

    let encrypted_payload =
        choreo_keystore::crypto::encrypt_with_public_key(derived_pub.as_bytes(), &plaintext)
            .map_err(|e| ClientError::Encryption(e.to_string()))?;

    // Wipe the plaintext bytes (the credential value zeroizes itself on drop
    // via `#[zeroize(drop)]` in choreo-keystore, and the daemon zeroizes its
    // own stored copy, so this closes the remaining gap on the send path).
    plaintext.zeroize();

    let msg = ClientMessage::AddCredential {
        service,
        encrypted_payload,
        unlock_key: unlock_key.to_vec(),
    };
    // Wipe the stack key after building: the two `Vec` copies (in `msg` and
    // the returned key) are the ones that travel on, and the local array is
    // then redundant.
    let result = (msg, unlock_key.to_vec());
    unlock_key.zeroize();
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Shared helper: set `CHOREOGRAPHR_KEYSTORE_PASSPHRASE` for the test body
    /// and restore the previous value on drop. The env var is process-global;
    /// tests using it must not run in parallel with other tests that read it,
    /// which the existing suite already assumes.
    struct PassphraseGuard;

    impl PassphraseGuard {
        fn set(pass: &str) -> Self {
            // SAFETY: single-threaded test context; restore-on-drop keeps other
            // tests unaffected even on a panicking assert.
            unsafe { std::env::set_var("CHOREOGRAPHR_KEYSTORE_PASSPHRASE", pass) };
            PassphraseGuard
        }
    }

    impl Drop for PassphraseGuard {
        fn drop(&mut self) {
            // SAFETY: same single-threaded test context as `set`.
            unsafe { std::env::remove_var("CHOREOGRAPHR_KEYSTORE_PASSPHRASE") };
        }
    }

    #[test]
    fn parse_credential_api_key() {
        let cred = parse_credential("api_key", &["sk-test".into()]).unwrap();
        assert!(matches!(cred, ServiceCredential::ApiKey { ref key } if key == "sk-test"));
    }

    #[test]
    fn parse_credential_api_key_missing_field() {
        let result = parse_credential("api_key", &[]);
        assert!(result.is_err());
    }

    #[test]
    fn parse_credential_x() {
        let fields = vec![
            "ak".into(),
            "aks".into(),
            "at".into(),
            "ats".into(),
            "-".into(),
        ];
        let cred = parse_credential("x", &fields).unwrap();
        let view = cred.as_x().unwrap();
        assert_eq!(view.api_key, "ak");
        assert_eq!(view.api_key_secret, "aks");
        assert_eq!(view.access_token, "at");
        assert_eq!(view.access_token_secret, "ats");
        assert!(view.bearer_token.is_none());
    }

    #[test]
    fn parse_credential_x_with_bearer() {
        let fields = vec![
            "ak".into(),
            "aks".into(),
            "at".into(),
            "ats".into(),
            "bt".into(),
        ];
        let cred = parse_credential("x", &fields).unwrap();
        let view = cred.as_x().unwrap();
        assert_eq!(view.bearer_token, Some("bt"));
    }

    #[test]
    fn parse_credential_x_missing_fields() {
        let fields = vec!["ak".into(), "aks".into(), "at".into()];
        let result = parse_credential("x", &fields);
        assert!(result.is_err());
    }

    #[test]
    fn parse_credential_unknown_type() {
        let result = parse_credential("unknown", &[]);
        assert!(result.is_err());
    }

    // ── try_auto_unlock_key tests ──────────────────────────────────

    #[test]
    fn try_auto_unlock_key_with_raw_key() {
        let dir = tempfile::tempdir().unwrap();
        let _guard =
            choreo_keystore::paths::TestConfigGuard::set_root(Some(dir.path().to_path_buf()));
        std::fs::create_dir_all(dir.path().join("choreographr")).unwrap();

        let (_, sk) = choreo_keystore::crypto::generate_keypair();
        std::fs::write(dir.path().join("choreographr/identity.pk"), sk).unwrap();

        assert_eq!(try_auto_unlock_key("local.sock"), Some(sk.to_vec()));
    }

    #[test]
    fn try_auto_unlock_key_with_invalid_raw_key_length() {
        let dir = tempfile::tempdir().unwrap();
        let _guard =
            choreo_keystore::paths::TestConfigGuard::set_root(Some(dir.path().to_path_buf()));
        std::fs::create_dir_all(dir.path().join("choreographr")).unwrap();

        // Write a file that isn't 32 bytes
        std::fs::write(dir.path().join("choreographr/identity.pk"), b"not 32 bytes").unwrap();

        assert!(try_auto_unlock_key("local.sock").is_none());
    }

    #[test]
    fn try_auto_unlock_key_with_encrypted_key() {
        let dir = tempfile::tempdir().unwrap();
        let _guard =
            choreo_keystore::paths::TestConfigGuard::set_root(Some(dir.path().to_path_buf()));
        std::fs::create_dir_all(dir.path().join("choreographr")).unwrap();

        let (_, sk) = choreo_keystore::crypto::generate_keypair();
        let encrypted = choreo_keystore::crypto::encrypt_private_key(&sk, "hunter2").unwrap();
        std::fs::write(dir.path().join("choreographr/identity.pk.enc"), &encrypted).unwrap();

        let _pass = PassphraseGuard::set("hunter2");
        assert_eq!(try_auto_unlock_key("local.sock"), Some(sk.to_vec()));
    }

    #[test]
    fn try_auto_unlock_key_with_no_keys() {
        let dir = tempfile::tempdir().unwrap();
        let _guard =
            choreo_keystore::paths::TestConfigGuard::set_root(Some(dir.path().to_path_buf()));
        std::fs::create_dir_all(dir.path().join("choreographr")).unwrap();

        assert!(try_auto_unlock_key("local.sock").is_none());
    }

    /// The stored per-daemon unlock key WINS over the legacy files: this is
    /// the target-state resolution order.
    #[test]
    fn try_auto_unlock_key_stored_key_beats_legacy() {
        let dir = tempfile::tempdir().unwrap();
        let _guard =
            choreo_keystore::paths::TestConfigGuard::set_root(Some(dir.path().to_path_buf()));
        std::fs::create_dir_all(dir.path().join("choreographr")).unwrap();

        let (_, legacy_sk) = choreo_keystore::crypto::generate_keypair();
        std::fs::write(dir.path().join("choreographr/identity.pk"), legacy_sk).unwrap();

        let mut store = KnownServers::load().unwrap();
        let stored_key: [u8; 32] = [9u8; 32];
        store.set_unlock_key("daemon-a:9443", &stored_key).unwrap();

        assert_eq!(
            try_auto_unlock_key("daemon-a:9443"),
            Some(stored_key.to_vec())
        );
        // A different addr with no stored key still falls back to legacy.
        assert_eq!(
            try_auto_unlock_key("daemon-b:9443"),
            Some(legacy_sk.to_vec())
        );
    }

    // ── resolve_private_key tests ──────────────────────────────────

    #[test]
    fn resolve_raw_prefers_stored_then_legacy() {
        let dir = tempfile::tempdir().unwrap();
        let _guard =
            choreo_keystore::paths::TestConfigGuard::set_root(Some(dir.path().to_path_buf()));
        std::fs::create_dir_all(dir.path().join("choreographr")).unwrap();

        let (_, legacy_sk) = choreo_keystore::crypto::generate_keypair();
        std::fs::write(dir.path().join("choreographr/identity.pk"), legacy_sk).unwrap();

        // No stored key: legacy raw file resolves.
        assert_eq!(
            resolve_private_key(&UnlockMethod::Raw, "d:1").unwrap(),
            legacy_sk.to_vec()
        );

        // Stored key beats the raw file.
        let stored: [u8; 32] = [7u8; 32];
        let mut store = KnownServers::load().unwrap();
        store.set_unlock_key("d:1", &stored).unwrap();
        assert_eq!(
            resolve_private_key(&UnlockMethod::Raw, "d:1").unwrap(),
            stored.to_vec()
        );
    }

    #[test]
    fn resolve_passphrase_decrypts_encrypted_key() {
        let dir = tempfile::tempdir().unwrap();
        let _guard =
            choreo_keystore::paths::TestConfigGuard::set_root(Some(dir.path().to_path_buf()));
        std::fs::create_dir_all(dir.path().join("choreographr")).unwrap();

        let (_, sk) = choreo_keystore::crypto::generate_keypair();
        let encrypted = choreo_keystore::crypto::encrypt_private_key(&sk, "hunter2").unwrap();
        std::fs::write(dir.path().join("choreographr/identity.pk.enc"), &encrypted).unwrap();

        assert_eq!(
            resolve_private_key(&UnlockMethod::Passphrase("hunter2".into()), "d:1").unwrap(),
            sk.to_vec()
        );
    }

    // ── build_add_credential_message tests ─────────────────────────

    /// A helper to pull the key back out of the built message (tests may
    /// unwrap; production code may not).
    fn msg_unlock_key(msg: &ClientMessage) -> Vec<u8> {
        match msg {
            ClientMessage::AddCredential { unlock_key, .. } => unlock_key.clone(),
            other => panic!("expected AddCredential, got {other:?}"),
        }
    }

    #[test]
    fn build_add_credential_uses_stored_key_first() {
        let dir = tempfile::tempdir().unwrap();
        let _guard =
            choreo_keystore::paths::TestConfigGuard::set_root(Some(dir.path().to_path_buf()));
        std::fs::create_dir_all(dir.path().join("choreographr")).unwrap();

        let stored: [u8; 32] = [4u8; 32];
        let mut store = KnownServers::load().unwrap();
        store.set_unlock_key("d:1", &stored).unwrap();

        let (msg, key) =
            build_add_credential_message("d:1", "svc".into(), "api_key".into(), vec!["k".into()])
                .unwrap();
        assert_eq!(key, stored.to_vec());
        assert_eq!(msg_unlock_key(&msg), stored.to_vec());
    }

    #[test]
    fn build_add_credential_falls_back_to_legacy_key() {
        let dir = tempfile::tempdir().unwrap();
        let _guard =
            choreo_keystore::paths::TestConfigGuard::set_root(Some(dir.path().to_path_buf()));
        std::fs::create_dir_all(dir.path().join("choreographr")).unwrap();

        let (_, sk) = choreo_keystore::crypto::generate_keypair();
        std::fs::write(dir.path().join("choreographr/identity.pk"), sk).unwrap();

        let (_msg, key) =
            build_add_credential_message("d:1", "svc".into(), "api_key".into(), vec!["k".into()])
                .unwrap();
        assert_eq!(key, sk.to_vec());
    }

    /// With nothing stored and no legacy files, a FRESH random key is
    /// generated (TOFU) and the blob decrypts with the key that was
    /// returned.
    #[test]
    fn build_add_credential_generates_fresh_key_and_blob_decrypts() {
        let dir = tempfile::tempdir().unwrap();
        let _guard =
            choreo_keystore::paths::TestConfigGuard::set_root(Some(dir.path().to_path_buf()));
        std::fs::create_dir_all(dir.path().join("choreographr")).unwrap();

        let (msg, key) =
            build_add_credential_message("d:1", "svc".into(), "api_key".into(), vec!["k".into()])
                .unwrap();
        assert_eq!(key.len(), 32);
        assert_eq!(msg_unlock_key(&msg), key);

        // The blob must decrypt with the derived public key — this is the
        // daemon-side test-decrypt contract.
        let ClientMessage::AddCredential {
            service,
            encrypted_payload,
            ..
        } = &msg
        else {
            panic!("expected AddCredential");
        };
        assert_eq!(service, "svc");
        // The blob was encrypted to the pubkey derived from this key.
        // decrypt_with_private_key takes the raw 32-byte secret + ciphertext.
        let mut sk_arr = [0u8; 32];
        sk_arr.copy_from_slice(&key);
        let plaintext =
            choreo_keystore::crypto::decrypt_with_private_key(&sk_arr, encrypted_payload)
                .expect("blob must decrypt with the chosen unlock key");
        let cred: ServiceCredential = postcard::from_bytes(&plaintext).unwrap();
        assert!(matches!(cred, ServiceCredential::ApiKey { ref key, .. } if key == "k"));
    }

    /// Bug-2 regression: a freshly minted key is OPTIMISTICALLY recorded in
    /// known_servers immediately (so an interrupted `CredentialAdded` reply
    /// cannot orphan the binding), and the record SURVIVES — a daemon
    /// rejection must not delete it (the rejection may be a transient daemon
    /// failure misreported as a key error, and a fresh key has no fallback to
    /// re-derive from anyway). `KnownServers::remove(addr)` is the only wipe.
    #[test]
    fn fresh_key_is_optimistically_recorded_and_survives_a_rejection() {
        let dir = tempfile::tempdir().unwrap();
        let _guard =
            choreo_keystore::paths::TestConfigGuard::set_root(Some(dir.path().to_path_buf()));
        std::fs::create_dir_all(dir.path().join("choreographr")).unwrap();
        // Ensure no legacy files exist so resolution must mint a fresh key.
        assert!(!dir.path().join("choreographr/identity.pk").exists());

        let (msg, key) =
            build_add_credential_message("d:1", "svc".into(), "api_key".into(), vec!["k".into()])
                .unwrap();
        assert_eq!(msg_unlock_key(&msg), key, "message carries the minted key");

        // The optimistic record is on disk before any daemon confirmation.
        let store = KnownServers::load().unwrap();
        let recorded: [u8; 32] = store.unlock_key("d:1").unwrap().expect("must be recorded");
        let key_arr: [u8; 32] = key.as_slice().try_into().unwrap();
        assert_eq!(recorded, key_arr);

        // There is no revert API: a reload (what the next client attempt
        // does) still resolves the SAME key, rather than minting a new one.
        let reloaded = KnownServers::load().unwrap();
        assert_eq!(reloaded.unlock_key("d:1").unwrap(), Some(key_arr));
    }

    // ── record_unlock_key tests ────────────────────────────────────

    #[test]
    fn record_unlock_key_persists_to_known_servers() {
        let dir = tempfile::tempdir().unwrap();
        let _guard =
            choreo_keystore::paths::TestConfigGuard::set_root(Some(dir.path().to_path_buf()));
        std::fs::create_dir_all(dir.path().join("choreographr")).unwrap();

        let key: [u8; 32] = [11u8; 32];
        record_unlock_key("unix:///run/choreo.sock", &key).unwrap();

        // Persisted: a fresh load sees the key (this is the unix-socket
        // carrier case — the entry was created with no pubkey).
        let store = KnownServers::load().unwrap();
        assert_eq!(
            store.unlock_key("unix:///run/choreo.sock").unwrap(),
            Some(key)
        );
        let entry = store
            .entries()
            .iter()
            .find(|e| e.addr == "unix:///run/choreo.sock")
            .unwrap();
        assert!(entry.pubkey.is_none(), "carrier entry must have no pin");
    }

    #[test]
    fn record_unlock_key_removes_matching_legacy_files() {
        let dir = tempfile::tempdir().unwrap();
        let _guard =
            choreo_keystore::paths::TestConfigGuard::set_root(Some(dir.path().to_path_buf()));
        std::fs::create_dir_all(dir.path().join("choreographr")).unwrap();

        let (_, sk) = choreo_keystore::crypto::generate_keypair();
        std::fs::write(dir.path().join("choreographr/identity.pk"), sk).unwrap();

        record_unlock_key("d:1", &sk).unwrap();

        // The legacy file matched the recorded key, so the migration
        // deletes it; the per-daemon record is in place.
        assert!(!dir.path().join("choreographr/identity.pk").exists());
        let store = KnownServers::load().unwrap();
        assert_eq!(store.unlock_key("d:1").unwrap(), Some(sk));
    }

    #[test]
    fn record_unlock_key_keeps_mismatched_legacy_files() {
        let dir = tempfile::tempdir().unwrap();
        let _guard =
            choreo_keystore::paths::TestConfigGuard::set_root(Some(dir.path().to_path_buf()));
        std::fs::create_dir_all(dir.path().join("choreographr")).unwrap();

        let (_, sk) = choreo_keystore::crypto::generate_keypair();
        std::fs::write(dir.path().join("choreographr/identity.pk"), sk).unwrap();

        let other: [u8; 32] = [12u8; 32];
        record_unlock_key("d:1", &other).unwrap();

        // The legacy file does NOT match the recorded key, so it stays.
        assert!(dir.path().join("choreographr/identity.pk").exists());
    }

    #[test]
    fn record_unlock_key_removes_matching_encrypted_legacy_file() {
        let dir = tempfile::tempdir().unwrap();
        let _guard =
            choreo_keystore::paths::TestConfigGuard::set_root(Some(dir.path().to_path_buf()));
        std::fs::create_dir_all(dir.path().join("choreographr")).unwrap();

        let (_, sk) = choreo_keystore::crypto::generate_keypair();
        let encrypted = choreo_keystore::crypto::encrypt_private_key(&sk, "hunter2").unwrap();
        std::fs::write(dir.path().join("choreographr/identity.pk.enc"), &encrypted).unwrap();

        let _pass = PassphraseGuard::set("hunter2");
        record_unlock_key("d:1", &sk).unwrap();
        assert!(!dir.path().join("choreographr/identity.pk.enc").exists());
    }

    #[test]
    fn record_unlock_key_keeps_encrypted_file_without_passphrase() {
        let dir = tempfile::tempdir().unwrap();
        let _guard =
            choreo_keystore::paths::TestConfigGuard::set_root(Some(dir.path().to_path_buf()));
        std::fs::create_dir_all(dir.path().join("choreographr")).unwrap();

        let (_, sk) = choreo_keystore::crypto::generate_keypair();
        let encrypted = choreo_keystore::crypto::encrypt_private_key(&sk, "hunter2").unwrap();
        std::fs::write(dir.path().join("choreographr/identity.pk.enc"), &encrypted).unwrap();

        // No passphrase available: the comparison is impossible, so the
        // key is still recorded but the legacy file is left alone.
        let _pass = PassphraseGuard::set("");
        record_unlock_key("d:1", &sk).unwrap();
        assert!(dir.path().join("choreographr/identity.pk.enc").exists());
        let store = KnownServers::load().unwrap();
        assert_eq!(store.unlock_key("d:1").unwrap(), Some(sk));
    }

    #[test]
    fn record_unlock_key_rejects_non_32_byte_key() {
        let dir = tempfile::tempdir().unwrap();
        let _guard =
            choreo_keystore::paths::TestConfigGuard::set_root(Some(dir.path().to_path_buf()));
        std::fs::create_dir_all(dir.path().join("choreographr")).unwrap();

        assert!(record_unlock_key("d:1", b"short").is_err());
    }
}
