use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use std::path::{Path, PathBuf};
use tracing::{debug, error, info, warn};

/// ACL of authorized client public keys for Noise IK authentication.
///
/// Loads from the TOML file at `authorized_clients_path()` (typically
/// `~/.config/choreographr/authorized_clients.toml`).  If the file does
/// not exist, all remote connections are rejected.
#[derive(Debug, PartialEq, Eq)]
pub struct Acl {
    keys: Vec<[u8; 32]>,
}

impl Acl {
    /// Load the ACL from an optional TOML file path.
    ///
    /// If the file does not exist, returns an empty ACL (no remote
    /// connections allowed).  Parse errors are logged and also result
    /// in an empty ACL.
    pub fn load(path: &Path) -> Self {
        let toml_str = match std::fs::read_to_string(path) {
            Ok(s) => s,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                info!("no authorized_clients.toml found, defaulting to empty ACL");
                return Acl { keys: Vec::new() };
            }
            Err(e) => {
                error!("failed to read authorized_clients.toml: {e}, using empty ACL");
                return Acl { keys: Vec::new() };
            }
        };
        match Acl::parse(&toml_str) {
            Ok(acl) => {
                info!(count = acl.keys.len(), "loaded authorized clients ACL");
                acl
            }
            Err(e) => {
                error!("failed to parse authorized_clients.toml: {e}, using empty ACL");
                Acl { keys: Vec::new() }
            }
        }
    }

    /// Parse the ACL from TOML text. The error is a parse failure — the
    /// caller decides the policy (initial load denies-all; hot-reload keeps
    /// the current keys). Invalid base64 or wrong-length pubkeys inside an
    /// otherwise-valid file are skipped with a warning (one bad line must
    /// not drop every client).
    fn parse(toml_str: &str) -> Result<Acl, toml::de::Error> {
        #[derive(serde::Deserialize)]
        struct ClientEntry {
            pubkey: String,
        }

        #[derive(serde::Deserialize)]
        struct AclFile {
            // `default` so an EMPTY file parses as "zero clients" (the
            // intentional deny-all) instead of a missing-field error — the
            // hot-reload policy depends on that distinction.
            #[serde(default)]
            client: Vec<ClientEntry>,
        }

        let parsed: AclFile = toml::from_str(toml_str)?;

        let mut keys = Vec::new();
        for entry in parsed.client {
            let bytes = match BASE64.decode(&entry.pubkey) {
                Ok(b) if b.len() == 32 => {
                    let mut arr = [0u8; 32];
                    arr.copy_from_slice(&b);
                    arr
                }
                _ => {
                    warn!(
                        "invalid pubkey in authorized_clients.toml: {}",
                        entry.pubkey
                    );
                    continue;
                }
            };
            keys.push(bytes);
        }

        Ok(Acl { keys })
    }

    /// Check whether a client's public key is authorized.
    pub fn contains(&self, pubkey: &[u8; 32]) -> bool {
        self.keys.contains(pubkey)
    }

    /// The number of authorized clients (for counts in replies/logs).
    pub fn len(&self) -> usize {
        self.keys.len()
    }

    /// Whether no clients are authorized.
    pub fn is_empty(&self) -> bool {
        self.keys.is_empty()
    }
}

/// A hot-reloadable ACL: the file path it was loaded from plus an
/// [`arc_swap::ArcSwap`] of the parsed key set.
///
/// This is the sanctioned shared-state exception #4 shape, exactly like the
/// provider catalog in `choreo-ai-protocols`: readers (every TCP handshake,
/// on its own connection thread) load a consistent snapshot lock-free, and
/// there is a STRICT single writer — the daemon command loop, via
/// [`SharedAcl::reload`] on `DaemonCommand::AclReload` events. Every change
/// REQUEST travels by channel; only the atomic store mutates the ACL. The
/// rationale lives in ARCHITECTURE.md's `server/acl.rs` row.
///
/// Holding the path here (rather than passing it to a reload handler)
/// guarantees the watcher, the initial load, and every reload all target
/// the SAME file even if the config-dir resolution would someday disagree.
pub struct SharedAcl {
    path: PathBuf,
    swap: arc_swap::ArcSwap<Acl>,
}

impl SharedAcl {
    /// Load the ACL from `path` and wrap it in a hot-reloadable holder.
    pub fn load(path: &Path) -> std::sync::Arc<Self> {
        let acl = Acl::load(path);
        std::sync::Arc::new(SharedAcl {
            path: path.to_path_buf(),
            swap: arc_swap::ArcSwap::from_pointee(acl),
        })
    }

    /// The file this ACL reloads from.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Check a client key against the CURRENT snapshot. Lock-free: this is
    /// called from every TCP handshake on its own connection thread.
    pub fn contains(&self, pubkey: &[u8; 32]) -> bool {
        self.swap.load().contains(pubkey)
    }

    /// The number of authorized clients in the CURRENT snapshot (for the
    /// `AclUpdated` broadcast's count).
    pub fn len(&self) -> usize {
        self.swap.load().keys.len()
    }

    /// Whether the CURRENT snapshot authorizes no clients.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Re-read the ACL file and swap the snapshot in if it changed.
    ///
    /// Called ONLY from the daemon command loop (the single writer).
    ///
    /// Failure policy — deliberately DIFFERENT from the initial load, which
    /// denies-all when the file is missing: a reload that cannot produce a
    /// confident parse (file missing, unreadable, or torn mid-save) keeps
    /// the CURRENT keys, because the alternative would un-authorize live
    /// clients on a transient editor artifact. A valid file with zero
    /// entries, by contrast, IS an intentional deny-all and swaps in. No
    /// swap (and no churn) happens unless the parsed key set differs.
    pub fn reload(&self) {
        // Read + parse separately so "cannot read" and "cannot parse" are
        // distinguishable from "parsed, zero entries": only the last swaps
        // in an empty ACL.
        let fresh = match std::fs::read_to_string(&self.path) {
            Ok(text) => match Acl::parse(&text) {
                Ok(acl) => acl,
                Err(e) => {
                    warn!(
                        path = %self.path.display(),
                        error = %e,
                        "ACL reload: file is not valid TOML; keeping the current ACL"
                    );
                    return;
                }
            },
            Err(e) => {
                warn!(
                    path = %self.path.display(),
                    error = %e,
                    "ACL reload: file unreadable; keeping the current ACL"
                );
                return;
            }
        };

        let previous = self.swap.load();
        if **previous == fresh {
            debug!(path = %self.path.display(), "ACL unchanged; no swap");
            return;
        }
        self.swap.store(std::sync::Arc::new(fresh));
        info!(
            path = %self.path.display(),
            "ACL reloaded from disk (hot-reload)"
        );
    }
}

/// Append one `[[client]]` entry to the ACL file at `path` under the
/// advisory exclusive file lock — the single write discipline shared by the
/// daemon's `/acl add` handler and the `choreographr acl-add` CLI, so
/// concurrent reload reads, socket-driven adds, and CLI adds serialize
/// instead of tearing each other's entries. Creates the parent dir and the
/// file itself as needed. Returns after an fsync: the entry is on disk.
pub fn append_key_locked(path: &Path, key: &[u8; 32]) -> Result<(), String> {
    use base64::Engine as _;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("cannot create the ACL directory: {e}"))?;
    }
    #[cfg(unix)]
    let file: std::fs::File = {
        use std::os::unix::fs::OpenOptionsExt;
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .read(true)
            .mode(0o600)
            .open(path)
            .map_err(|e| format!("cannot open the ACL file: {e}"))?
    };
    #[cfg(not(unix))]
    let file: std::fs::File = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .read(true)
        .open(path)
        .map_err(|e| format!("cannot open the ACL file: {e}"))?;
    file.lock()
        .map_err(|e| format!("cannot lock the ACL file: {e}"))?;
    // Append mode (O_APPEND): the entry lands atomically at the end even
    // against another process's concurrent append.
    use std::io::Write;
    write!(
        &file,
        "[[client]]\npubkey = \"{}\"\n",
        base64::engine::general_purpose::STANDARD.encode(key)
    )
    .map_err(|e| format!("cannot write the ACL entry: {e}"))?;
    file.sync_all()
        .map_err(|e| format!("cannot flush the ACL file: {e}"))?;
    file.unlock()
        .map_err(|e| format!("cannot unlock the ACL file: {e}"))?;
    Ok(())
}

/// Spawn the thin consumer that watches `authorized_clients.toml` edits
/// surfaced by the unified config transport and forwards them to the daemon
/// command loop.
///
/// Mirrors [`crate::accounts::spawn_accounts_watcher`] exactly: NO reading
/// or comparing here — the command loop (the ACL's single writer) does the
/// re-read + parse-compare + swap inside `SharedAcl::reload`, so watcher
/// noise, editor save bursts, and future self-writes (`/acl add`) are all
/// coalesced into harmless no-ops. The thread is detached and lives until
/// the process exits.
pub fn spawn_acl_watcher(
    daemon_tx: std::sync::mpsc::Sender<crate::daemon::DaemonCommand>,
    acl_rx: crossbeam_channel::Receiver<crate::config_watch::ConfigChange>,
) {
    let _ = std::thread::Builder::new()
        .name("acl-config-watch".into())
        .spawn(move || {
            for _first in acl_rx.iter() {
                // Coalesce a save burst (temp + rename fans out several
                // events) into ONE AclReload — the command loop's
                // parse-compare makes the redundant events no-ops anyway.
                while acl_rx.try_recv().is_ok() {}
                if daemon_tx
                    .send(crate::daemon::DaemonCommand::AclReload)
                    .is_err()
                {
                    tracing::info!("daemon command loop gone; stopping acl config watcher");
                    break;
                }
            }
        });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn load_nonexistent_file_returns_empty_acl() {
        let acl = Acl::load(Path::new("/nonexistent/acl.toml"));
        assert!(!acl.contains(&[0u8; 32]));
    }

    #[test]
    fn load_valid_file_loads_keys() {
        let tmp = tempfile::NamedTempFile::new().expect("tempfile");
        std::fs::write(
            tmp.path(),
            r#"
[[client]]
pubkey = "AQIDBAUGBwgJCgsMDQ4PEBESExQVFhcYGRobHB0eHyA="
"#,
        )
        .expect("write");
        let acl = Acl::load(tmp.path());

        // The base64-decoded key is 0x01..0x20 (32 bytes).
        let mut expected = [0u8; 32];
        for (i, elem) in expected.iter_mut().enumerate() {
            *elem = (i as u8) + 1;
        }
        assert!(acl.contains(&expected));

        // A different key should not match.
        assert!(!acl.contains(&[0u8; 32]));
    }

    #[test]
    fn load_invalid_toml_returns_empty_acl() {
        let tmp = tempfile::NamedTempFile::new().expect("tempfile");
        std::fs::write(tmp.path(), "not valid toml").expect("write");
        let acl = Acl::load(tmp.path());
        assert!(!acl.contains(&[0u8; 32]));
    }

    #[test]
    fn load_invalid_base64_skips_entry() {
        let tmp = tempfile::NamedTempFile::new().expect("tempfile");
        std::fs::write(
            tmp.path(),
            r#"
[[client]]
pubkey = "not-valid-base64!!"

[[client]]
pubkey = "AQIDBAUGBwgJCgsMDQ4PEBESExQVFhcYGRobHB0eHyA="
"#,
        )
        .expect("write");
        let acl = Acl::load(tmp.path());

        let mut expected = [0u8; 32];
        for (i, elem) in expected.iter_mut().enumerate() {
            *elem = (i as u8) + 1;
        }
        assert!(acl.contains(&expected));
    }

    #[test]
    fn load_wrong_length_key_skips_entry() {
        let tmp = tempfile::NamedTempFile::new().expect("tempfile");
        // b64 of a 16-byte value — wrong length.
        std::fs::write(
            tmp.path(),
            r#"
[[client]]
pubkey = "c29tZSAxNiBieXRlIG9r"

[[client]]
pubkey = "AQIDBAUGBwgJCgsMDQ4PEBESExQVFhcYGRobHB0eHyA="
"#,
        )
        .expect("write");
        let acl = Acl::load(tmp.path());

        let mut expected = [0u8; 32];
        for (i, elem) in expected.iter_mut().enumerate() {
            *elem = (i as u8) + 1;
        }
        assert!(acl.contains(&expected));
    }

    #[test]
    fn load_empty_file_returns_empty_acl() {
        let tmp = tempfile::NamedTempFile::new().expect("tempfile");
        let acl = Acl::load(tmp.path());
        assert!(!acl.contains(&[0u8; 32]));
    }

    // ── SharedAcl hot-reload policy ──────────────────────────────────

    const KEY_A: [u8; 32] = [1u8; 32];
    const KEY_B: [u8; 32] = [2u8; 32];

    fn acl_toml(key_b64: &str) -> String {
        format!("[[client]]\npubkey = \"{key_b64}\"\n")
    }

    /// The base64 forms of KEY_A / KEY_B.
    fn b64(key: &[u8; 32]) -> String {
        BASE64.encode(key)
    }

    #[test]
    fn reload_applies_added_and_removed_keys() {
        let tmp = tempfile::NamedTempFile::new().expect("tempfile");
        std::fs::write(tmp.path(), acl_toml(&b64(&KEY_A))).expect("write");
        let shared = SharedAcl::load(tmp.path());
        assert!(shared.contains(&KEY_A));
        assert!(!shared.contains(&KEY_B));

        // Add KEY B: reload makes it authoritative without a restart.
        std::fs::write(
            tmp.path(),
            format!("{}\n{}", acl_toml(&b64(&KEY_A)), acl_toml(&b64(&KEY_B))),
        )
        .expect("rewrite");
        shared.reload();
        assert!(shared.contains(&KEY_A));
        assert!(shared.contains(&KEY_B));

        // Remove KEY A: reload drops it.
        std::fs::write(tmp.path(), acl_toml(&b64(&KEY_B))).expect("rewrite");
        shared.reload();
        assert!(!shared.contains(&KEY_A));
        assert!(shared.contains(&KEY_B));
    }

    #[test]
    fn reload_keeps_current_keys_when_file_is_garbage_or_gone() {
        let tmp = tempfile::NamedTempFile::new().expect("tempfile");
        std::fs::write(tmp.path(), acl_toml(&b64(&KEY_A))).expect("write");
        let shared = SharedAcl::load(tmp.path());
        assert!(shared.contains(&KEY_A));

        // Torn/invalid TOML (an editor mid-save): current keys stay live.
        std::fs::write(tmp.path(), "not valid toml [[[").expect("write garbage");
        shared.reload();
        assert!(
            shared.contains(&KEY_A),
            "garbage file must not un-authorize"
        );

        // File removed entirely: current keys stay live (a Remove watcher
        // event must not deny-all a running daemon; a restart re-evaluates).
        let path = tmp.path().to_path_buf();
        std::fs::remove_file(&path).expect("remove");
        shared.reload();
        assert!(
            shared.contains(&KEY_A),
            "missing file must not un-authorize"
        );
    }

    #[test]
    fn reload_swaps_to_intentionally_empty_acl() {
        let tmp = tempfile::NamedTempFile::new().expect("tempfile");
        std::fs::write(tmp.path(), acl_toml(&b64(&KEY_A))).expect("write");
        let shared = SharedAcl::load(tmp.path());
        assert!(shared.contains(&KEY_A));

        // A valid file with ZERO entries is an intentional deny-all —
        // distinct from an unreadable file — and must apply.
        std::fs::write(tmp.path(), "").expect("truncate to empty");
        shared.reload();
        assert!(!shared.contains(&KEY_A), "explicit empty ACL denies all");
    }

    #[test]
    fn reload_without_change_is_a_no_op() {
        let tmp = tempfile::NamedTempFile::new().expect("tempfile");
        std::fs::write(tmp.path(), acl_toml(&b64(&KEY_A))).expect("write");
        let shared = SharedAcl::load(tmp.path());

        // Rewriting the SAME key set (byte-different file, same parsed
        // content — the common editor save) must not churn the snapshot:
        // the parse-compare gate is what makes the watcher's noisy events
        // and the self-writes from a future /acl add harmless.
        std::fs::write(
            tmp.path(),
            format!("\n# comment\n{}", acl_toml(&b64(&KEY_A))),
        )
        .expect("rewrite");
        shared.reload();
        assert!(shared.contains(&KEY_A));
    }
}
