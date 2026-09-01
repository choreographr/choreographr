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
//! first_seen_unix = 1756735200
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

use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use base64::Engine as _;
use tracing::{debug, info, warn};

use crate::error::ClientError;

/// One pinned server: the address it was confirmed at and the public key
/// the human approved there. `addr` is the map key — `host:port` exactly as
/// the dialer spells it (see the module docs for the DHCP caveat: a server
/// whose address changes re-enters first contact under its new address).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct KnownServerEntry {
    /// Dial address (`host:port`) this key was pinned under.
    pub addr: String,
    /// Base64 (standard alphabet) of the confirmed 32-byte server static.
    pub pubkey: String,
    /// Unix epoch seconds when the pin was confirmed (bookkeeping only —
    /// never used in trust decisions).
    #[serde(default)]
    pub first_seen_unix: Option<i64>,
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
fn decode_pubkey(b64: &str) -> Result<[u8; 32], ClientError> {
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(b64.trim())
        .map_err(|e| ClientError::CredentialParse(format!("invalid pubkey base64: {e}")))?;
    bytes
        .try_into()
        .map_err(|_| ClientError::CredentialParse("pubkey must decode to exactly 32 bytes".into()))
}

/// Encode a raw 32-byte key as the store's base64 form.
fn encode_pubkey(pk: &[u8; 32]) -> String {
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
    /// dropped at load time with a warning).
    pub fn lookup(&self, addr: &str) -> Result<Option<[u8; 32]>, ClientError> {
        match self.entries.iter().find(|e| e.addr == addr) {
            None => Ok(None),
            Some(entry) => Ok(Some(decode_pubkey(&entry.pubkey)?)),
        }
    }

    /// Pin `pk` as the confirmed server key for `addr`, persisting
    /// immediately under an advisory file lock.
    ///
    /// Pinning an address that already has an entry REPLACES it — that is
    /// the deliberate re-pairing path after a legitimate server key
    /// rotation: the human confirms the NEW fingerprint, then pins, and
    /// only then does IK succeed again. There is no in-code path that
    /// replaces a pin without this explicit call.
    pub fn pin(&mut self, addr: &str, pk: &[u8; 32]) -> Result<(), ClientError> {
        let entry = KnownServerEntry {
            addr: addr.to_string(),
            pubkey: encode_pubkey(pk),
            first_seen_unix: Some(
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .map(|d| d.as_secs() as i64)
                    // Clock before 1970 is absurd but not worth failing a
                    // trust decision over — the timestamp is bookkeeping.
                    .unwrap_or(0),
            ),
        };
        match self.entries.iter_mut().find(|e| e.addr == addr) {
            Some(existing) => *existing = entry,
            None => self.entries.push(entry),
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
/// load-time tolerant policy); individual entries whose pubkey does not
/// decode to 32 bytes are dropped with a warning and the healthy rest is
/// kept — one corrupted line must not unpin every server.
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
        .filter_map(|entry| match decode_pubkey(&entry.pubkey) {
            // Re-encode canonically: a hand-edited entry with stray
            // whitespace round-trips into the normalized form.
            Ok(pk) => Some(KnownServerEntry {
                pubkey: encode_pubkey(&pk),
                ..entry
            }),
            Err(e) => {
                warn!(
                    addr = %entry.addr,
                    error = %e,
                    "skipping invalid known_servers entry"
                );
                None
            }
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

    /// Re-pinning an address REPLACES the entry (the deliberate re-pairing
    /// path after a legitimate server key rotation) and persists the
    /// replacement.
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
                encode_pubkey(&[6u8; 32])
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
