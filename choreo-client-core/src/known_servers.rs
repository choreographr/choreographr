//! The client's pinned server public keys — the SSH `known_hosts` analogue.
//!
//! The trust model (see ARCHITECTURE.md "Enrollment & transport trust") is
//! asymmetric: the daemon learns client keys from the ACL, but the CLIENT
//! learns server keys by running the Noise XX first-contact handshake and
//! confirming the fingerprint with a human. Whatever key survives that
//! confirmation is PINNED here, keyed by `host:port`, and every later
//! connection to that address uses Noise IK against the pinned key — with a
//! loud, hard failure if the server's key ever changes (the `known_hosts`
//! behavior: key change = error, never a silent re-prompt).
//!
//! Store shape (`~/.config/choreographr/known_servers.toml`):
//!
//! ```toml
//! [[server]]
//! addr = "192.168.1.20:9443"
//! pubkey = "<base64 of the 32-byte transport.pub>"
//! ```
//!
//! Failure policy is deliberately tolerant on LOAD and strict on WRITE:
//! a missing, unreadable, or unparseable store loads as EMPTY (worst case:
//! the next connect is treated as first contact again, which re-runs the
//! human fingerprint confirmation — it can never silently trust a server),
//! while a bad individual entry is skipped with a warning. Writes, by
//! contrast, take an advisory exclusive file lock and rewrite the whole
//! file (the same discipline as `ensure_transport_keypair`), so concurrent
//! processes pinning different servers cannot tear the store.
//!
//! Concurrency caveat: the lock prevents TEARING, not lost updates —
//! `pin()` rewrites the whole file from this process's in-memory view, so
//! two processes that load the store concurrently and both pin serialize
//! their writes but the SECOND write clobbers the FIRST's new entry
//! (last-writer-wins). That is acceptable for a known_hosts analogue (the
//! store is small, per-user, and a vanished pin degrades to a re-confirmed
//! first contact — never silent trust), but it is why this store carries
//! only its two sanctioned mutable fields (the transport pin and the
//! per-daemon keystore unlock key) and must not grow into a
//! general-purpose mutable registry.
//!
//! Unlock keys: each daemon's credential keystore is governed by one
//! keypair whose private half is held CLIENT-side, one per daemon (TOFU —
//! the daemon adopts the first presented key and verifies every later
//! one). This store is that client-side home: `unlock_key` is the base64
//! 32-byte X25519 private key for the daemon at `addr`. Unix-socket
//! connections have no transport key to pin, so their entries exist purely
//! to carry the unlock key (`pubkey = None`).

use std::io::Write;
use std::path::{Path, PathBuf};

use base64::Engine as _;
use tracing::{debug, info, warn};

use crate::error::ClientError;

/// One pinned server: the address it was confirmed at and the public key
/// the human approved there. `addr` is the map key — `host:port` exactly as
/// the dialer spells it (see the module docs for the DHCP caveat: a server
/// whose address changes re-enters first contact under its new address).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct KnownServerEntry {
    /// Dial address (`host:port`, or the unix socket path for unix
    /// connections — see the module docs on unlock-key carriers).
    pub addr: String,
    /// Base64 (standard alphabet) of the confirmed 32-byte server static.
    /// `None` for unix-socket entries, which carry no transport pin — they
    /// exist only to hold the per-daemon unlock key.
    #[serde(default)]
    pub pubkey: Option<String>,
    /// Base64 (standard alphabet, same conventions as `pubkey`) of the
    /// 32-byte per-daemon keystore unlock key for this daemon.
    #[serde(default)]
    pub unlock_key: Option<String>,
}

/// The in-memory view of `known_servers.toml`, plus the path it persists to.
///
/// All mutation goes through [`KnownServers::pin`] / [`KnownServers::remove`],
/// which rewrite the file under an advisory lock before returning — so a
/// caller that pins successfully knows the pin is durable.
#[derive(Debug, Clone)]
pub struct KnownServers {
    entries: Vec<KnownServerEntry>,
    path: PathBuf,
}

/// Path to the known-servers store
/// (`~/.config/choreographr/known_servers.toml`), resolved through the
/// keystore's config-dir helper so test overrides and platform paths agree
/// with the rest of the config family.
pub fn known_servers_path() -> Result<PathBuf, ClientError> {
    choreo_keystore::paths::config_dir()
        .map(|dir| dir.join("known_servers.toml"))
        .map_err(|e| ClientError::Io(std::io::Error::other(e)))
}

/// Decode a stored base64 pubkey into its 32 raw bytes.
///
/// Returns `Err` on invalid base64 or a decoded length other than 32 —
/// callers treat that as "entry is garbage, skip it", never as "no pin".
fn decode_key(b64: &str, what: &str) -> Result<[u8; 32], ClientError> {
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(b64.trim())
        .map_err(|e| ClientError::CredentialParse(format!("invalid {what} base64: {e}")))?;
    bytes.try_into().map_err(|_| {
        ClientError::CredentialParse(format!("{what} must decode to exactly 32 bytes"))
    })
}

/// Encode a raw 32-byte key as the store's base64 form.
fn encode_key(pk: &[u8; 32]) -> String {
    base64::engine::general_purpose::STANDARD.encode(pk)
}

impl KnownServers {
    /// Load the store from its default path (see [`known_servers_path`]).
    ///
    /// A missing file is the normal first-run case: an empty store, not an
    /// error. See the module docs for the full failure policy.
    pub fn load() -> Result<Self, ClientError> {
        let path = known_servers_path()?;
        Self::load_from(&path)
    }

    /// Load from an explicit path (the test seam — production callers use
    /// [`KnownServers::load`]).
    pub fn load_from(path: &Path) -> Result<Self, ClientError> {
        match std::fs::read_to_string(path) {
            Ok(text) => {
                let entries = parse_store(&text);
                debug!(path = %path.display(), entries = entries.len(), "loaded known servers");
                Ok(KnownServers {
                    entries,
                    path: path.to_path_buf(),
                })
            }
            // Missing store = first run: empty, not an error.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                debug!(path = %path.display(), "no known_servers.toml yet; starting empty");
                Ok(KnownServers {
                    entries: Vec::new(),
                    path: path.to_path_buf(),
                })
            }
            // Unreadable (permissions, I/O error): empty-with-warning, per
            // the failure policy — worst case is a re-confirmed first
            // contact, never silent trust.
            Err(e) => {
                warn!(
                    path = %path.display(),
                    error = %e,
                    "could not read known_servers.toml; treating as empty (trust re-confirmation will be required)"
                );
                Ok(KnownServers {
                    entries: Vec::new(),
                    path: path.to_path_buf(),
                })
            }
        }
    }

    /// The pinned public key for `addr`, if one exists.
    ///
    /// `Ok(None)` = no pin (first contact); `Ok(Some(pk))` = pin exists, the
    /// caller must run IK against it and treat any mismatch as a hard
    /// error. Individual unparseable entries never surface here (they were
    /// dropped at load time with a warning). An entry WITHOUT a pubkey (a
    /// unix-socket unlock-key carrier) also yields `Ok(None)`: TCP callers
    /// see that as "unpinned", which is exactly the first-contact
    /// semantics they already handle.
    pub fn lookup(&self, addr: &str) -> Result<Option<[u8; 32]>, ClientError> {
        match self.entries.iter().find(|e| e.addr == addr) {
            None => Ok(None),
            Some(entry) => match &entry.pubkey {
                None => Ok(None),
                Some(b64) => Ok(Some(decode_key(b64, "pubkey")?)),
            },
        }
    }

    /// The stored per-daemon unlock key for `addr`, if one exists.
    /// Returns `Ok(None)` when the entry has no unlock_key (or no entry at
    /// all) — callers fall back to the legacy local key or generate a fresh
    /// one. Entries whose stored unlock_key does not decode to 32 bytes are
    /// dropped at load time, so this never surfaces garbage.
    pub fn unlock_key(&self, addr: &str) -> Result<Option<[u8; 32]>, ClientError> {
        match self
            .entries
            .iter()
            .find(|e| e.addr == addr)
            .and_then(|e| e.unlock_key.as_deref())
        {
            None => Ok(None),
            Some(b64) => Ok(Some(decode_key(b64, "unlock_key")?)),
        }
    }

    /// Record the per-daemon unlock key for `addr`, persisting immediately
    /// under the same advisory file lock as `pin`. If no entry exists for
    /// `addr` one is created with `pubkey: None` — that is the
    /// unix-socket carrier case (unix connections have nothing to pin;
    /// TCP daemons already have a pinned entry by the time a credential
    /// flow runs, so the existing entry is updated in place, keeping its
    /// pin intact).
    pub fn set_unlock_key(&mut self, addr: &str, key: &[u8; 32]) -> Result<(), ClientError> {
        let b64 = encode_key(key);
        match self.entries.iter_mut().find(|e| e.addr == addr) {
            Some(existing) => {
                // Replace only the unlock key — never clobber a transport
                // pin that a human already confirmed.
                existing.unlock_key = Some(b64);
            }
            None => {
                self.entries.push(KnownServerEntry {
                    addr: addr.to_string(),
                    pubkey: None,
                    unlock_key: Some(b64),
                });
            }
        }
        self.persist()?;
        info!(addr, "recorded per-daemon keystore unlock key");
        Ok(())
    }

    /// Pin `pk` as the confirmed server key for `addr`, persisting
    /// immediately under an advisory file lock.
    ///
    /// Pinning an address that already has an entry replaces ONLY its
    /// transport pin — the deliberate re-pairing path after a legitimate
    /// server key rotation: the human confirms the NEW fingerprint, then a
    /// re-pin, and only then does IK succeed again. The per-daemon keystore
    /// unlock key is an INDEPENDENT field and must survive a re-pin — an
    /// entry that carries both a pin and an unlock key (a TCP daemon after
    /// the first set_unlock_key) would otherwise silently lose the unlock
    /// key, locking the operator out of a now-misbound keystore. There is
    /// no in-code path that replaces a pin without this explicit call.
    pub fn pin(&mut self, addr: &str, pk: &[u8; 32]) -> Result<(), ClientError> {
        match self.entries.iter_mut().find(|e| e.addr == addr) {
            // Update the existing entry in place: swap the pin but leave
            // `unlock_key` untouched (see the doc comment above).
            Some(existing) => {
                existing.pubkey = Some(encode_key(pk));
            }
            None => self.entries.push(KnownServerEntry {
                addr: addr.to_string(),
                pubkey: Some(encode_key(pk)),
                // A brand-new entry inherits nothing, so there is no unlock
                // key to preserve yet.
                unlock_key: None,
            }),
        }
        self.persist()?;
        info!(
            addr,
            fingerprint = %choreo_transport::key::fingerprint(pk),
            "pinned server public key"
        );
        Ok(())
    }

    /// Remove the pin for `addr` (the "server key changed, delete the
    /// known_hosts entry to re-pair" path). Returns whether an entry was
    /// removed; persists only when something changed.
    pub fn remove(&mut self, addr: &str) -> Result<bool, ClientError> {
        let before = self.entries.len();
        self.entries.retain(|e| e.addr != addr);
        if self.entries.len() == before {
            return Ok(false);
        }
        self.persist()?;
        info!(addr, "removed pinned server public key");
        Ok(true)
    }

    /// All entries (read-only view — for UIs listing known daemons).
    pub fn entries(&self) -> &[KnownServerEntry] {
        &self.entries
    }

    /// Rewrite the whole store to `self.path` under an advisory exclusive
    /// lock (same discipline as `ensure_transport_keypair`): concurrent
    /// processes — two TUIs pinning different daemons — serialize instead
    /// of tearing each other's writes. Create the parent dir on first pin.
    fn persist(&self) -> Result<(), ClientError> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| ClientError::Io(std::io::Error::other(e)))?;
        }

        // Open (or create) with the lock held for the whole rewrite, then
        // truncate-in-place. A reader that sees the file mid-rewrite gets
        // the load-time tolerant path (parse failure -> empty -> re-confirm
        // trust), but the lock makes that a non-event in practice.
        #[cfg(unix)]
        let file = {
            use std::os::unix::fs::OpenOptionsExt;
            std::fs::OpenOptions::new()
                .create(true)
                .read(true)
                .write(true)
                .truncate(false)
                // 0o600: the store binds addresses to trust decisions, so
                // it is cheap to keep private even though pubkeys are not
                // secret.
                .mode(0o600)
                .open(&self.path)
        };
        #[cfg(not(unix))]
        let file = std::fs::OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(&self.path);
        let mut file = file.map_err(|e| ClientError::Io(std::io::Error::other(e)))?;
        file.lock()
            .map_err(|e| ClientError::Io(std::io::Error::other(e)))?;

        // Serialize the whole current view — never a delta — so the file is
        // always a faithful render of this KnownServers value.
        let text = toml::to_string_pretty(&StoreFile {
            server: self.entries.clone(),
        })
        .map_err(|e| ClientError::Io(std::io::Error::other(e)))?;
        file.set_len(0)
            .map_err(|e| ClientError::Io(std::io::Error::other(e)))?;
        std::io::Seek::seek(&mut file, std::io::SeekFrom::Start(0))
            .map_err(|e| ClientError::Io(std::io::Error::other(e)))?;
        file.write_all(text.as_bytes())
            .map_err(|e| ClientError::Io(std::io::Error::other(e)))?;
        file.sync_all()
            .map_err(|e| ClientError::Io(std::io::Error::other(e)))?;
        file.unlock()
            .map_err(|e| ClientError::Io(std::io::Error::other(e)))?;
        debug!(path = %self.path.display(), entries = self.entries.len(), "persisted known servers");
        Ok(())
    }
}

/// The TOML file shape: an array of tables named `server`.
#[derive(serde::Serialize, serde::Deserialize)]
struct StoreFile {
    server: Vec<KnownServerEntry>,
}

/// Parse the store text. Unparseable WHOLE file -> empty + warning (the
/// load-time tolerant policy); individual entries whose PRESENT pubkey
/// does not decode to 32 bytes are dropped with a warning, while entries
/// with NO pubkey are kept (unix-socket unlock-key carriers) and entries
/// whose unlock_key is corrupt have just that field dropped — one bad
/// line must not unpin or unlock-wipe every server.
fn parse_store(text: &str) -> Vec<KnownServerEntry> {
    let parsed: StoreFile = match toml::from_str(text) {
        Ok(p) => p,
        Err(e) => {
            warn!(error = %e, "known_servers.toml is not valid TOML; treating as empty (trust re-confirmation will be required)");
            return Vec::new();
        }
    };
    parsed
        .server
        .into_iter()
        .filter_map(|entry| {
            // Re-encode canonically: a hand-edited entry with stray
            // whitespace round-trips into the normalized form. A present-
            // but-invalid pubkey means the entry is garbage (its trust
            // decision cannot be honored) — drop it. A present-but-invalid
            // unlock_key is more forgiving: drop just that field, keeping
            // the pin, so the caller re-resolves/re-records the key.
            let pubkey = match &entry.pubkey {
                Some(b64) => match decode_key(b64, "pubkey") {
                    Ok(pk) => Some(encode_key(&pk)),
                    Err(e) => {
                        warn!(
                            addr = %entry.addr,
                            error = %e,
                            "skipping invalid known_servers entry"
                        );
                        return None;
                    }
                },
                None => None,
            };
            let unlock_key = match &entry.unlock_key {
                Some(b64) => match decode_key(b64, "unlock_key") {
                    Ok(key) => Some(encode_key(&key)),
                    Err(e) => {
                        warn!(
                            addr = %entry.addr,
                            error = %e,
                            "dropping corrupt unlock_key from known_servers entry (pin kept); the key will be re-resolved"
                        );
                        None
                    }
                },
                None => None,
            };
            Some(KnownServerEntry {
                pubkey,
                unlock_key,
                ..entry
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The test override lives in choreo-keystore (the config-dir owner);
    /// every test here redirects it to its own tempdir. `TestConfigGuard`
    /// resets the override on drop — even on a panicking assert — so a
    /// failed test cannot leak the override into another test's thread; the
    /// TempDir must be held for the test's whole body (it owns the store
    /// directory) — callers bind BOTH returned guards.
    fn use_temp_config_root() -> (
        tempfile::TempDir,
        choreo_keystore::paths::TestConfigGuard,
        PathBuf,
    ) {
        let temp = tempfile::TempDir::new().expect("tempdir");
        let dir = temp.path().to_path_buf();
        let guard = choreo_keystore::paths::TestConfigGuard::set_root(Some(dir.clone()));
        (temp, guard, dir)
    }

    #[test]
    fn pin_lookup_remove_round_trip() {
        let (_temp, _guard, dir) = use_temp_config_root();
        let path = dir.join("choreographr").join("known_servers.toml");

        // First run: nothing pinned.
        let mut store = KnownServers::load_from(&path).expect("missing store loads empty");
        assert!(store.lookup("host1:9443").expect("lookup").is_none());

        // Pin, re-load from disk, look up: the pin is durable.
        let pk_a = [1u8; 32];
        store.pin("host1:9443", &pk_a).expect("pin");
        let reloaded = KnownServers::load_from(&path).expect("reload");
        assert_eq!(
            reloaded.lookup("host1:9443").expect("decode"),
            Some(pk_a),
            "pinned key must survive a reload"
        );

        // A second server coexists; removing the first leaves it intact.
        let pk_b = [2u8; 32];
        store.pin("host2:9443", &pk_b).expect("pin second");
        assert_eq!(store.entries().len(), 2);
        assert!(store.remove("host1:9443").expect("remove"));
        assert!(
            !store.remove("host1:9443").expect("remove again"),
            "second remove is a no-op"
        );
        let reloaded = KnownServers::load_from(&path).expect("reload");
        assert_eq!(reloaded.lookup("host1:9443").expect("decode"), None);
        assert_eq!(reloaded.lookup("host2:9443").expect("decode"), Some(pk_b));
    }

    /// Re-pinning an address replaces its transport pin in place (the
    /// deliberate re-pairing path after a legitimate server key rotation) and
    /// persists the replacement without duplicating the entry.
    #[test]
    fn pin_replaces_existing_entry() {
        let (_temp, _guard, dir) = use_temp_config_root();
        let path = dir.join("choreographr").join("known_servers.toml");
        let mut store = KnownServers::load_from(&path).expect("load");

        let pk_old = [3u8; 32];
        let pk_new = [4u8; 32];
        store.pin("host:9443", &pk_old).expect("pin old");
        store.pin("host:9443", &pk_new).expect("pin new");

        let reloaded = KnownServers::load_from(&path).expect("reload");
        assert_eq!(reloaded.entries().len(), 1, "re-pin must not duplicate");
        assert_eq!(reloaded.lookup("host:9443").expect("decode"), Some(pk_new));
    }

    /// Bug-1 regression: re-pinning an entry that also carries a stored
    /// per-daemon UNLOCK KEY must NOT wipe it — the two fields are
    /// independent, and dropping the key on a routine server-key rotation
    /// would lock the operator out of a now-misbound keystore.
    #[test]
    fn pin_preserves_stored_unlock_key() {
        let (_temp, _guard, dir) = use_temp_config_root();
        let path = dir.join("choreographr").join("known_servers.toml");
        let mut store = KnownServers::load_from(&path).expect("load");

        let unlock: [u8; 32] = [9u8; 32];
        let pk_old = [3u8; 32];
        let pk_new = [4u8; 32];
        store.pin("host:9443", &pk_old).expect("pin old");
        store
            .set_unlock_key("host:9443", &unlock)
            .expect("set unlock key");

        // The re-pin (deliberate re-pairing) changes only the transport pin.
        store.pin("host:9443", &pk_new).expect("pin new");

        let reloaded = KnownServers::load_from(&path).expect("reload");
        assert_eq!(
            reloaded.lookup("host:9443").expect("decode"),
            Some(pk_new),
            "the new pin must be recorded"
        );
        assert_eq!(
            reloaded.unlock_key("host:9443").expect("decode"),
            Some(unlock),
            "re-pin must preserve the per-daemon unlock key"
        );
    }

    /// A stored unlock key SURVIVES everything short of an explicit
    /// `remove(addr)`: there is no programmatic path that erases one (a
    /// daemon REJECTING the key must not delete the record — the rejection
    /// may be a transient daemon-side failure misreported as a key error,
    /// and the keystore binding is TOFU-immortal so a confirmed record can
    /// never be 'wrong'). `remove` is the one deliberate wipe, and it takes
    /// the transport pin with it.
    #[test]
    fn stored_unlock_key_survives_rejections_and_is_removed_only_explicitly() {
        let (_temp, _guard, dir) = use_temp_config_root();
        let path = dir.join("choreographr").join("known_servers.toml");
        let mut store = KnownServers::load_from(&path).expect("load");

        let pk = [3u8; 32];
        let unlock: [u8; 32] = [9u8; 32];
        store.pin("host:9443", &pk).expect("pin");
        store.set_unlock_key("host:9443", &unlock).expect("set");

        // A reload (what a client restart does) still resolves the key.
        let reloaded = KnownServers::load_from(&path).expect("reload");
        assert_eq!(
            reloaded.unlock_key("host:9443").expect("decode"),
            Some(unlock)
        );

        // The explicit removal path wipes the whole entry (pin included).
        let mut store2 = KnownServers::load_from(&path).expect("load 2");
        assert!(store2.remove("host:9443").expect("remove"));
        let after = KnownServers::load_from(&path).expect("reload after remove");
        assert_eq!(after.lookup("host:9443").expect("decode"), None);
        assert_eq!(after.unlock_key("host:9443").expect("decode"), None);
    }

    /// Torn / garbage store files load as EMPTY (never an error, never a
    /// half-store) — the worst case is a re-confirmed first contact, and a
    /// subsequent pin rewrites a healthy file over the garbage.
    #[test]
    fn corrupt_store_loads_empty_and_pin_recovers() {
        let (_temp, _guard, dir) = use_temp_config_root();
        let path = dir.join("choreographr").join("known_servers.toml");
        std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
        std::fs::write(&path, "not valid toml [[[").expect("write garbage");

        let mut store = KnownServers::load_from(&path).expect("corrupt store loads empty");
        assert!(store.entries().is_empty());

        let pk = [5u8; 32];
        store.pin("host:9443", &pk).expect("pin over garbage");
        let reloaded = KnownServers::load_from(&path).expect("reload");
        assert_eq!(reloaded.lookup("host:9443").expect("decode"), Some(pk));
    }

    /// An entry whose pubkey does not decode to 32 bytes is dropped with a
    /// warning while healthy entries in the same file are kept — one bad
    /// line must not unpin every server.
    #[test]
    fn invalid_entry_is_skipped_others_kept() {
        let (_temp, _guard, dir) = use_temp_config_root();
        let path = dir.join("choreographr").join("known_servers.toml");
        std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
        std::fs::write(
            &path,
            format!(
                "[[server]]\naddr = \"bad:1\"\npubkey = \"not-base64!!!\"\n\n\
                 [[server]]\naddr = \"good:1\"\npubkey = \"{}\"\n",
                encode_key(&[6u8; 32])
            ),
        )
        .expect("write mixed store");

        let store = KnownServers::load_from(&path).expect("load");
        assert_eq!(store.entries().len(), 1, "only the healthy entry survives");
        assert!(store.lookup("bad:1").expect("lookup").is_none());
        assert_eq!(store.lookup("good:1").expect("decode"), Some([6u8; 32]));

        // Wrong decoded length (valid base64, not 32 bytes) is equally
        // skipped.
        std::fs::write(
            &path,
            format!(
                "[[server]]\naddr = \"short:1\"\npubkey = \"{}\"\n",
                base64::engine::general_purpose::STANDARD.encode([7u8; 16])
            ),
        )
        .expect("write short-key store");
        let store = KnownServers::load_from(&path).expect("load");
        assert!(store.entries().is_empty());
    }
}
