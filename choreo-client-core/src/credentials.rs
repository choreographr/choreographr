use base64::Engine as _;
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
/// For `UnlockMethod::Raw`, unlock with the key ALREADY associated with this
/// daemon: the stored known_servers `unlock_key`, falling back to the legacy
/// raw `identity.pk` file — which is then COPIED into known_servers.toml so
/// the store becomes the single source of truth (the legacy file is never
/// deleted, merely superseded). Errors with [`ClientError::NoUnlockKey`] when
/// neither source has a key.
///
/// For `UnlockMethod::Key(key)`, the argument IS the unlock key (base64 of
/// the 32 raw bytes): it is decoded, validated, and returned — WRITE-FREE.
/// The caller records it via [`record_unlock_key`] ONLY when the daemon
/// confirms (`Unlocked` reply); nothing is written to known_servers on send.
pub fn resolve_private_key(method: &UnlockMethod, addr: &str) -> Result<Vec<u8>, ClientError> {
    match method {
        UnlockMethod::Raw => {
            info!(addr, "resolving stored unlock key for addr");
            stored_or_adopted_unlock_key(addr)?
                .map(|k| k.to_vec())
                .ok_or_else(|| ClientError::NoUnlockKey(addr.to_string()))
        }
        UnlockMethod::Key(key) => {
            info!(addr, "unlocking with caller-supplied base64 unlock key");
            // WRITE-FREE: decode + validate only. Recording happens on the
            // daemon's targeted confirmation (record_unlock_key), so a key
            // the daemon rejects never pollutes the store.
            let key = decode_base64_unlock_key(key)?;
            Ok(key.to_vec())
        }
    }
}

/// Decode a caller-supplied base64 unlock key into exactly 32 raw bytes
/// (the same encoding `known_servers.toml` stores). The decoded bytes are
/// zeroized if validation fails, so a bad key never lingers in a freed
/// allocation.
fn decode_base64_unlock_key(key: &str) -> Result<[u8; 32], ClientError> {
    let raw = Zeroizing::new(
        base64::engine::general_purpose::STANDARD
            .decode(key.trim())
            .map_err(|_| ClientError::PrivateKeyInvalid)?,
    );
    raw.as_slice()
        .try_into()
        .map_err(|_| ClientError::PrivateKeyInvalid)
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

/// Resolve the unlock key ALREADY associated with `addr`: the stored
/// known_servers `unlock_key`, falling back to the legacy raw `identity.pk`
/// file. A legacy hit is COPIED into the store (best-effort) so that
/// known_servers.toml becomes the single source of truth — the legacy file
/// is NEVER deleted, merely superseded. `Ok(None)` when neither source has
/// a usable key (daemon stays locked; all session operations still work).
fn stored_or_adopted_unlock_key(addr: &str) -> Result<Option<[u8; 32]>, ClientError> {
    if let Some(key) = stored_unlock_key(addr) {
        return Ok(Some(key));
    }
    match read_raw_private_key() {
        Ok(key) => {
            let key: [u8; 32] = key
                .as_slice()
                .try_into()
                .map_err(|_| ClientError::PrivateKeyInvalid)?;
            info!(
                addr,
                "using legacy raw private key; copying into known_servers.toml"
            );
            // Best-effort copy: if the store cannot be written the unlock
            // still proceeds with the legacy key, and the daemon-confirmed
            // `record_unlock_key` persists it later.
            if let Err(e) = KnownServers::load().and_then(|mut s| s.set_unlock_key(addr, &key)) {
                warn!(
                    addr,
                    error = %e,
                    "could not copy legacy unlock key into known_servers; it will be recorded on daemon confirmation"
                );
            }
            Ok(Some(key))
        }
        Err(ClientError::PrivateKeyInvalid) => {
            warn!(
                addr,
                "legacy private key file exists but is not 32 bytes; ignoring"
            );
            Ok(None)
        }
        Err(_) => Ok(None), // file absent — nothing to fall back to
    }
}

/// Try to resolve the unlock key for automatic unlock on connect to the
/// daemon at `addr`.
///
/// Resolution order (per-daemon keystore TOFU design):
/// 1. The stored `unlock_key` from the known_servers entry for `addr`.
/// 2. LEGACY fallback: the raw `identity.pk` file, COPIED into
///    known_servers.toml on first use (the legacy file is never deleted).
///
/// Returns `None` if no key can be resolved, which is fine — the daemon
/// starts locked but all session operations (create, browse, delete) work
/// without unlocking.  Only inference (RunInput) requires credentials.
pub fn try_auto_unlock_key(addr: &str) -> Option<Vec<u8>> {
    match stored_or_adopted_unlock_key(addr) {
        Ok(Some(key)) => Some(key.to_vec()),
        Ok(None) => {
            debug!(
                addr,
                "auto-unlock: no key available (daemon will start locked)"
            );
            None
        }
        Err(e) => {
            warn!(addr, error = %e, "auto-unlock: key resolution failed");
            None
        }
    }
}

/// Persist the per-daemon unlock key for `addr` into the known_servers
/// store. Legacy files are NEVER touched: no comparison, no deletion —
/// known_servers.toml simply supersedes them once it holds the key.
///
/// Callers MUST only invoke this after the daemon CONFIRMED the key (an
/// `Unlocked` or `CredentialAdded` reply) — never on send.
pub fn record_unlock_key(addr: &str, key: &[u8]) -> Result<(), ClientError> {
    let key: [u8; 32] = key.try_into().map_err(|_| ClientError::PrivateKeyInvalid)?;
    KnownServers::load()?.set_unlock_key(addr, &key)?;
    Ok(())
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

/// Resolve the per-daemon keystore unlock key for `addr` — VERIFY-ONLY
/// resolution: the stored known_servers key for `addr`, falling back to the
/// legacy raw `identity.pk` file (which `stored_or_adopted_unlock_key` copies
/// into the store).
///
/// There is NO fresh-mint fallback anymore: a new design reserves key
/// creation for the explicit bind flow ([`bind_fresh_daemon`]) — stored and
/// legacy keys are unlock-VERIFICATION keys only and must never be used to
/// establish a binding. When neither source has a key the caller gets
/// [`ClientError::NoUnlockKey`] and the frontend surfaces it.
fn resolve_keystore_key(addr: &str) -> Result<[u8; 32], ClientError> {
    stored_or_adopted_unlock_key(addr)?.ok_or_else(|| ClientError::NoUnlockKey(addr.to_string()))
}

/// Prepare an AUTO-BIND of a freshly connected daemon whose keystore is
/// unbound (learned from the subscribe-time lock state, or from a
/// `KeystoreUnbound` reply to an auto-unlock attempt).
///
/// A brand-new 32-byte CSPRNG key is minted with `rand` and recorded into
/// known_servers for `addr` BEFORE the `BindKeystore` message is returned —
/// pre-send recording is CORRECT for bind (unlike unlock/add): an unbound
/// daemon adopts whatever key arrives first, so if the confirmation is lost
/// the recorded key still matches the binding, and there is nothing to
/// overwrite. The returned `(key, message)` pair is sent as-is; the caller
/// MUST confirm on the targeted `DaemonMessage::Bound` reply via
/// [`record_unlock_key`] (a no-op-safe re-record of the already-persisted
/// key) to keep the pending-flow contract uniform.
///
/// Pre-held keys (stored or legacy) are NEVER used to bind: the binding must
/// always be fresh so a recorded key is provably the one the daemon adopted.
/// If the key cannot be persisted pre-send the bind is REFUSED — sending an
/// unrecorded bind key risks an unrecoverable orphaned binding.
pub fn bind_fresh_daemon(addr: &str) -> Result<([u8; 32], ClientMessage), ClientError> {
    // CSPRNG via rand's thread-local generator: the binding key is the root
    // of the daemon's credential confidentiality, so it must never be
    // predictable or reused.
    let fresh: [u8; 32] = rand::random();
    info!(
        addr,
        "minted fresh random keystore binding key for unbound daemon"
    );
    // Pre-send record is MANDATORY here (not best-effort): see doc above.
    KnownServers::load()?.set_unlock_key(addr, &fresh)?;
    debug!(addr, "recorded fresh bind key into known_servers pre-send");
    let msg = ClientMessage::BindKeystore {
        key: fresh.to_vec(),
    };
    Ok((fresh, msg))
}

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
/// VERIFY-ONLY key resolution: the stored per-daemon key or the legacy file —
/// NEVER a fresh key (fresh keys are minted exclusively by [`bind_fresh_daemon`]).
/// When neither source has a key this errors with [`ClientError::NoUnlockKey`]
/// and the frontend surfaces it (the daemon's keystore is either unbound —
/// in which case the client auto-binds first — or bound to a key this client
/// does not hold).
///
/// Returns the message AND the unlock key used, so the caller can call
/// [`record_unlock_key`] once the daemon CONFIRMS success (`CredentialAdded` /
/// `Unlocked` reply) — never on send.
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

        // The legacy raw key is COPIED into known_servers.toml on first use
        // (the store becomes the single source of truth; the file stays).
        let store = KnownServers::load().unwrap();
        assert_eq!(store.unlock_key("local.sock").unwrap(), Some(sk));
        assert!(dir.path().join("choreographr/identity.pk").exists());
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

    /// `/unlock <key>`: the argument IS the unlock key (base64 of the 32 raw
    /// bytes) — WRITE-FREE: it is decoded, validated, and returned, but NOT
    /// recorded pre-send. Recording happens only on the daemon's targeted
    /// confirmation (`record_unlock_key`).
    #[test]
    fn resolve_key_does_not_record_supplied_key_into_store() {
        let dir = tempfile::tempdir().unwrap();
        let _guard =
            choreo_keystore::paths::TestConfigGuard::set_root(Some(dir.path().to_path_buf()));
        std::fs::create_dir_all(dir.path().join("choreographr")).unwrap();

        let key: [u8; 32] = [21u8; 32];
        let b64 = base64::engine::general_purpose::STANDARD.encode(key);
        assert_eq!(
            resolve_private_key(&UnlockMethod::Key(b64), "d:1").unwrap(),
            key.to_vec()
        );
        // Write-free: even a fresh load sees nothing recorded for the addr —
        // a rejected key must never pollute the store.
        let store = KnownServers::load().unwrap();
        assert_eq!(store.unlock_key("d:1").unwrap(), None);
    }

    /// A supplied key that is not base64 — or not exactly 32 bytes once
    /// decoded — is rejected.
    #[test]
    fn resolve_key_rejects_bad_input() {
        let dir = tempfile::tempdir().unwrap();
        let _guard =
            choreo_keystore::paths::TestConfigGuard::set_root(Some(dir.path().to_path_buf()));
        std::fs::create_dir_all(dir.path().join("choreographr")).unwrap();

        assert!(resolve_private_key(&UnlockMethod::Key("not base64!!!".into()), "d:1").is_err());
        let short = base64::engine::general_purpose::STANDARD.encode([1u8; 16]);
        assert!(resolve_private_key(&UnlockMethod::Key(short), "d:1").is_err());
        // Nothing was recorded for the rejected inputs.
        assert_eq!(
            KnownServers::load().unwrap().unlock_key("d:1").unwrap(),
            None
        );
    }

    /// `/unlock` (Raw) with neither a stored key nor a legacy file is a
    /// clear NoUnlockKey error, not a silent failure.
    #[test]
    fn resolve_raw_without_any_key_is_a_clear_error() {
        let dir = tempfile::tempdir().unwrap();
        let _guard =
            choreo_keystore::paths::TestConfigGuard::set_root(Some(dir.path().to_path_buf()));
        std::fs::create_dir_all(dir.path().join("choreographr")).unwrap();

        assert!(matches!(
            resolve_private_key(&UnlockMethod::Raw, "d:1"),
            Err(ClientError::NoUnlockKey(_))
        ));
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

    /// Verify-only resolution: with nothing stored and no legacy files, the
    /// add FAILS with NoUnlockKey — no fresh key is minted on the add path
    /// (fresh keys are minted exclusively by `bind_fresh_daemon`).
    #[test]
    fn build_add_credential_without_any_key_is_a_clear_error() {
        let dir = tempfile::tempdir().unwrap();
        let _guard =
            choreo_keystore::paths::TestConfigGuard::set_root(Some(dir.path().to_path_buf()));
        std::fs::create_dir_all(dir.path().join("choreographr")).unwrap();

        assert!(matches!(
            build_add_credential_message("d:1", "svc".into(), "api_key".into(), vec!["k".into()]),
            Err(ClientError::NoUnlockKey(_))
        ));
        // And nothing was optimistically recorded by the failed attempt.
        assert_eq!(
            KnownServers::load().unwrap().unlock_key("d:1").unwrap(),
            None
        );
    }

    /// The blob encrypts to the pubkey derived from the RESOLVED (stored)
    /// key — the daemon-side test-decrypt contract — and the returned key
    /// equals the stored one.
    #[test]
    fn build_add_credential_blob_decrypts_with_stored_key() {
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
        let ClientMessage::AddCredential {
            service,
            encrypted_payload,
            ..
        } = &msg
        else {
            panic!("expected AddCredential");
        };
        assert_eq!(service, "svc");
        let plaintext =
            choreo_keystore::crypto::decrypt_with_private_key(&stored, encrypted_payload)
                .expect("blob must decrypt with the stored unlock key");
        let cred: ServiceCredential = postcard::from_bytes(&plaintext).unwrap();
        assert!(matches!(cred, ServiceCredential::ApiKey { ref key, .. } if key == "k"));
    }

    /// `bind_fresh_daemon`: mints a FRESH CSPRNG key (never the stored or
    /// legacy key), records it into known_servers PRE-SEND (so a lost
    /// confirmation cannot orphan the binding), and returns the
    /// `BindKeystore` message carrying exactly that key.
    #[test]
    fn bind_fresh_daemon_mints_fresh_key_records_pre_send_and_returns_message() {
        let dir = tempfile::tempdir().unwrap();
        let _guard =
            choreo_keystore::paths::TestConfigGuard::set_root(Some(dir.path().to_path_buf()));
        std::fs::create_dir_all(dir.path().join("choreographr")).unwrap();

        // Pre-existing stored + legacy keys: bind must NOT use either.
        let stored: [u8; 32] = [30u8; 32];
        let mut store = KnownServers::load().unwrap();
        store.set_unlock_key("d:1", &stored).unwrap();
        let (_, legacy_sk) = choreo_keystore::crypto::generate_keypair();
        std::fs::write(dir.path().join("choreographr/identity.pk"), legacy_sk).unwrap();

        let (key, msg) = bind_fresh_daemon("d:1").unwrap();
        assert_ne!(key, stored, "bind must NEVER reuse a stored key");
        assert_ne!(key, legacy_sk, "bind must NEVER reuse the legacy key");
        match &msg {
            ClientMessage::BindKeystore { key: wire_key } => {
                assert_eq!(wire_key, &key.to_vec(), "message carries the minted key");
            }
            other => panic!("expected BindKeystore, got {other:?}"),
        }
        // Pre-send record: on disk BEFORE any daemon confirmation.
        let recorded = KnownServers::load().unwrap().unlock_key("d:1").unwrap();
        assert_eq!(recorded, Some(key));

        // The pending-flow contract: confirming on the targeted `Bound` reply
        // via record_unlock_key re-records the SAME key (idempotent).
        record_unlock_key("d:1", &key).unwrap();
        assert_eq!(
            KnownServers::load().unwrap().unlock_key("d:1").unwrap(),
            Some(key)
        );
    }

    /// Two consecutive binds of the same addr mint DIFFERENT keys (fresh
    /// CSPRNG every time — never a cached/reused value).
    #[test]
    fn bind_fresh_daemon_always_mints_a_new_key() {
        let dir = tempfile::tempdir().unwrap();
        let _guard =
            choreo_keystore::paths::TestConfigGuard::set_root(Some(dir.path().to_path_buf()));
        std::fs::create_dir_all(dir.path().join("choreographr")).unwrap();

        let (k1, _) = bind_fresh_daemon("d:1").unwrap();
        let (k2, _) = bind_fresh_daemon("d:1").unwrap();
        assert_ne!(k1, k2, "each bind must mint a fresh key");
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

    /// Legacy files are NEVER deleted by record_unlock_key (or anything
    /// else): known_servers.toml supersedes them, but they stay on disk.
    #[test]
    fn record_unlock_key_never_touches_legacy_files() {
        let dir = tempfile::tempdir().unwrap();
        let _guard =
            choreo_keystore::paths::TestConfigGuard::set_root(Some(dir.path().to_path_buf()));
        std::fs::create_dir_all(dir.path().join("choreographr")).unwrap();

        let (_, sk) = choreo_keystore::crypto::generate_keypair();
        std::fs::write(dir.path().join("choreographr/identity.pk"), sk).unwrap();

        let other: [u8; 32] = [12u8; 32];
        record_unlock_key("d:1", &other).unwrap();

        // The record is in place and the legacy file is untouched.
        assert!(dir.path().join("choreographr/identity.pk").exists());
        let store = KnownServers::load().unwrap();
        assert_eq!(store.unlock_key("d:1").unwrap(), Some(other));
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
