//! The MCP project-trust store: the set of project roots whose `.mcp.json`
//! the daemon is willing to honour.
//!
//! The daemon-tier `mcp.json` is trusted unconditionally (the user authored
//! it). A project's `.mcp.json` travels with a checkout the user may not have
//! written, so its servers — and, critically, any `${VAR}` expansion their
//! `env`/`headers` request — are gated behind an explicit whole-project trust
//! decision ([`ClientMessage::McpTrust`](choreo_proto::ClientMessage::McpTrust)).
//!
//! Trust is keyed by the EXACT canonical project root (see
//! [`crate::mcp::project_root_for`]) with NO ancestor inheritance: trusting
//! `/a` never trusts `/a/b`. Roots are canonicalized once at the entry point
//! (absolute + symlinks resolved) and stored as canonical literals, so the
//! comparison is a plain set-membership test against canonical paths.
//!
//! The store lives in the config directory (`trust.toml`, alongside the other
//! choreographr files) so the daemon's existing single-directory config
//! watcher can watch it by basename. It is TOML (choreographr's own files are
//! TOML; JSON is reserved for `mcp.json`, whose `mcpServers` shape is the MCP
//! standard). Reads are fail-CLOSED: a missing or malformed file yields an
//! empty trust set, never an error that could be mistaken for "trust
//! everything".

use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

/// The on-disk shape of `trust.toml`.
#[derive(Debug, Default, Serialize, Deserialize)]
struct TrustFile {
    /// The trusted project roots, as canonical absolute path strings.
    #[serde(default)]
    trusted: Vec<String>,
}

/// The daemon's MCP project-trust store.
///
/// Owned exclusively by the daemon command loop (the single writer), so no
/// lock is needed: all reads and mutations happen on that one thread.
pub struct McpTrustStore {
    /// The path of `trust.toml`.
    path: PathBuf,
    /// The trusted roots, canonical and de-duplicated (a set keeps membership
    /// O(log n) and serialization stable).
    trusted: BTreeSet<PathBuf>,
}

impl McpTrustStore {
    /// Load the trust store from `path`, failing CLOSED: a missing or
    /// malformed file yields an empty trust set (with a warning for the
    /// malformed case) rather than an error.
    ///
    /// A malformed file must never be treated as "trust everything": the
    /// safest interpretation of "I cannot read the trust list" is "I trust
    /// nothing".
    #[must_use]
    pub fn load(path: PathBuf) -> Self {
        let trusted = match std::fs::read_to_string(&path) {
            Ok(contents) => match toml::from_str::<TrustFile>(&contents) {
                Ok(file) => file
                    .trusted
                    .into_iter()
                    .map(PathBuf::from)
                    .collect::<BTreeSet<_>>(),
                Err(e) => {
                    tracing::warn!(
                        path = %path.display(),
                        error = %e,
                        "failed to parse trust.toml; treating the trust set as empty (fail-closed)"
                    );
                    BTreeSet::new()
                }
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                tracing::debug!(path = %path.display(), "no trust.toml yet; trust set empty");
                BTreeSet::new()
            }
            Err(e) => {
                tracing::warn!(
                    path = %path.display(),
                    error = %e,
                    "failed to read trust.toml; treating the trust set as empty (fail-closed)"
                );
                BTreeSet::new()
            }
        };
        Self { path, trusted }
    }

    /// The path of the backing `trust.toml`.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Whether `root` is trusted. `root` is canonicalized here so a caller may
    /// pass a raw (possibly symlinked or relative) path.
    #[must_use]
    pub fn is_trusted(&self, root: &Path) -> bool {
        self.trusted.contains(&canonicalize_root(root))
    }

    /// The trusted roots, in canonical sorted order.
    #[must_use]
    pub fn list(&self) -> Vec<PathBuf> {
        self.trusted.iter().cloned().collect()
    }

    /// Trust `root`, returning its canonical form. Idempotent: re-trusting an
    /// already-trusted root is a successful no-op. Persists on a change.
    ///
    /// # Errors
    ///
    /// Returns a message when the trust file cannot be written.
    pub fn trust(&mut self, root: &Path) -> Result<PathBuf, String> {
        let canonical = canonicalize_root(root);
        if self.trusted.contains(&canonical) {
            return Ok(canonical);
        }
        // Persist the PROSPECTIVE set first, committing to memory only once the
        // write succeeds: a failed write must leave the store exactly as it
        // was, never reporting a root as trusted that `trust.toml` does not
        // actually record.
        let mut prospective = self.trusted.clone();
        prospective.insert(canonical.clone());
        self.write_trust(&prospective)?;
        self.trusted = prospective;
        tracing::info!(root = %canonical.display(), "trusted project MCP root");
        Ok(canonical)
    }

    /// Revoke trust for `root`, returning its canonical form. Idempotent:
    /// untrusting a root that was not trusted is a successful no-op.
    ///
    /// # Errors
    ///
    /// Returns a message when the trust file cannot be written.
    pub fn untrust(&mut self, root: &Path) -> Result<PathBuf, String> {
        let canonical = canonicalize_root(root);
        if !self.trusted.contains(&canonical) {
            return Ok(canonical);
        }
        // Same write-first discipline as `trust`: revoke in memory only after
        // the prospective set is durably on disk.
        let mut prospective = self.trusted.clone();
        prospective.remove(&canonical);
        self.write_trust(&prospective)?;
        self.trusted = prospective;
        tracing::info!(root = %canonical.display(), "revoked trust for project MCP root");
        Ok(canonical)
    }

    /// Write `trusted` to the trust file atomically with owner-only
    /// permissions (dir 0700, file 0600). The write goes to a sibling temp
    /// file that is then renamed over the target, so a reader never sees a
    /// partial file.
    ///
    /// The set is passed in rather than read from `self` so callers can
    /// persist a prospective set and only then adopt it in memory.
    fn write_trust(&self, trusted: &BTreeSet<PathBuf>) -> Result<(), String> {
        let file = TrustFile {
            trusted: trusted
                .iter()
                .map(|p| p.to_string_lossy().into_owned())
                .collect(),
        };
        let serialized = toml::to_string_pretty(&file)
            .map_err(|e| format!("failed to serialize trust.toml: {e}"))?;
        write_private_atomic(&self.path, serialized.as_bytes())
            .map_err(|e| format!("failed to write {}: {e}", self.path.display()))
    }
}

/// Canonicalize a project-root path: absolute and symlink-resolved. When the
/// path cannot be canonicalized (e.g. it does not exist yet — trusting a
/// directory before it has a `.mcp.json` is allowed), fall back to a purely
/// lexical absolute form (the current directory joined with the path) so the
/// value is still stable and comparable.
#[must_use]
pub fn canonicalize_root(root: &Path) -> PathBuf {
    match root.canonicalize() {
        Ok(p) => p,
        Err(_) => {
            if root.is_absolute() {
                normalize_lexically(root)
            } else {
                std::env::current_dir().map_or_else(
                    |_| normalize_lexically(root),
                    |cwd| normalize_lexically(&cwd.join(root)),
                )
            }
        }
    }
}

/// Resolve `.` and `..` components lexically (no filesystem access) so a
/// non-existent path still compares consistently.
fn normalize_lexically(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            std::path::Component::ParentDir => {
                out.pop();
            }
            std::path::Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// Write `bytes` to `path` atomically: a sibling temp file (0600) renamed over
/// the target; the parent directory is created (0700) if needed.
fn write_private_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write as _;
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(parent)?;
    set_dir_private(parent);
    let tmp = path.with_extension(format!("tmp.{}", std::process::id()));
    {
        let mut file = std::fs::File::create(&tmp)?;
        set_file_private(&tmp);
        file.write_all(bytes)?;
        file.sync_all()?;
    }
    std::fs::rename(&tmp, path)
}

#[cfg(unix)]
fn set_dir_private(dir: &Path) {
    use std::os::unix::fs::PermissionsExt as _;
    let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700));
}

#[cfg(not(unix))]
fn set_dir_private(_dir: &Path) {}

#[cfg(unix)]
fn set_file_private(path: &Path) {
    use std::os::unix::fs::PermissionsExt as _;
    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
}

#[cfg(not(unix))]
fn set_file_private(_path: &Path) {}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_store() -> (tempfile::TempDir, McpTrustStore) {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("trust.toml");
        let store = McpTrustStore::load(path);
        (dir, store)
    }

    #[test]
    fn missing_file_is_empty_and_untrusted() {
        let (_dir, store) = temp_store();
        assert!(!store.is_trusted(Path::new("/some/project")));
        assert_eq!(store.list(), [] as [PathBuf; 0]);
    }

    #[test]
    fn exact_match_only_no_inheritance() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path().join("proj");
        std::fs::create_dir_all(&root).unwrap();
        let child = root.join("sub");
        std::fs::create_dir_all(&child).unwrap();

        let mut store = McpTrustStore::load(dir.path().join("trust.toml"));
        store.trust(&root).expect("trust");

        assert!(store.is_trusted(&root));
        // The canonical form of the child is NOT trusted: no ancestor
        // inheritance.
        assert!(!store.is_trusted(&child));
    }

    #[test]
    fn symlink_normalization_collapses_to_canonical_target() {
        let dir = tempfile::tempdir().expect("tempdir");
        let real = dir.path().join("real");
        std::fs::create_dir_all(&real).unwrap();
        let link = dir.path().join("link");

        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(&real, &link).unwrap();
            let mut store = McpTrustStore::load(dir.path().join("trust.toml"));
            store.trust(&link).expect("trust via symlink");
            // Trusting the symlink trusts the resolved target, and querying
            // through either spelling matches.
            assert!(store.is_trusted(&real));
            assert!(store.is_trusted(&link));
        }
        #[cfg(not(unix))]
        {
            // Non-unix: canonicalize is a no-op-ish; still idempotent.
            let _ = link;
        }
    }

    #[test]
    fn garbage_file_is_fail_closed() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("trust.toml");
        std::fs::write(&path, "this is not = valid = toml [[[").unwrap();
        let store = McpTrustStore::load(path);
        assert!(store.list().is_empty(), "garbage must not trust anything");
    }

    #[test]
    fn trust_untrust_round_trip_persists() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path().join("proj");
        std::fs::create_dir_all(&root).unwrap();
        let path = dir.path().join("trust.toml");

        {
            let mut store = McpTrustStore::load(path.clone());
            store.trust(&root).expect("trust");
        }
        // A fresh store reads the persisted set back.
        let reloaded = McpTrustStore::load(path.clone());
        assert!(reloaded.is_trusted(&root));

        {
            let mut store = McpTrustStore::load(path.clone());
            store.untrust(&root).expect("untrust");
        }
        let after = McpTrustStore::load(path);
        assert!(!after.is_trusted(&root));
    }

    #[test]
    fn trust_is_idempotent() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path().join("proj");
        std::fs::create_dir_all(&root).unwrap();
        let mut store = McpTrustStore::load(dir.path().join("trust.toml"));
        store.trust(&root).expect("first");
        store.trust(&root).expect("second");
        assert_eq!(store.list().len(), 1);
    }

    #[test]
    fn untrust_unknown_root_is_noop() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut store = McpTrustStore::load(dir.path().join("trust.toml"));
        store.untrust(Path::new("/never/trusted")).expect("noop");
        assert_eq!(store.list(), [] as [PathBuf; 0]);
    }

    #[test]
    fn failed_write_leaves_trust_set_unchanged() {
        // Point the trust file at `<regular-file>/trust.toml`: the parent is an
        // existing regular FILE, so `write_private_atomic`'s
        // `create_dir_all(parent)` fails deterministically (no timing needed).
        let dir = tempfile::tempdir().expect("tempdir");
        let not_a_dir = dir.path().join("not-a-dir");
        std::fs::write(&not_a_dir, b"i am a file").unwrap();
        let mut store = McpTrustStore::load(not_a_dir.join("trust.toml"));

        let root = dir.path().join("proj");
        std::fs::create_dir_all(&root).unwrap();

        let result = store.trust(&root);
        assert!(result.is_err(), "write into a file-as-parent must fail");
        // The failed persist must not have mutated the in-memory set.
        assert!(!store.is_trusted(&root));
        assert_eq!(store.list(), [] as [PathBuf; 0]);

        // Symmetric case: untrusting an absent root is a no-op even when the
        // backing path is unwritable.
        store.untrust(&root).expect("absent root is a no-op");
        assert_eq!(store.list(), [] as [PathBuf; 0]);
    }
}
