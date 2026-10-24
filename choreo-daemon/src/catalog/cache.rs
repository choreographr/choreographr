//! The durable catalog cache (`catalog.bin`) and the filesystem locations the
//! runtime catalog pipeline touches.
//!
//! The cache is postcard behind a leading [`CATALOG_CACHE_SCHEMA`] version, so
//! a blob written by an older build is rejected before any provider entry is
//! decoded at the wrong byte offsets. The file's durable role is to be the
//! base's home **between events**: the maintenance thread holds no copy of the
//! base (see [`super`]), so the command loop re-reads it here whenever a user
//! overlay edit needs a re-merge.
//!
//! Two readers must agree, and they use the same load order (valid cache file →
//! embedded blob): the daemon's startup load, and [`load_persisted_base`].
//! [`write_catalog_cache`] is the single writer — the daemon command loop, the
//! same thread that calls [`load_persisted_base`], which is what makes the
//! re-read race-free.

use std::io;
use std::path::{Path, PathBuf};

use choreo_ai_protocols::{ProviderEntry, load_bundled_base, write_file_atomic};
use tracing::{debug, warn};

/// Postcard cache filename under the data dir.
const CATALOG_BIN_NAME: &str = "catalog.bin";

/// Schema version prefixed to the postcard cache blob.
///
/// Postcard is positional and non-self-describing, so a `ModelEntry` change
/// against a stale cache either fails (missing fields) or — worse — decodes
/// into a *wrong* catalog. Reasoning-echo requirements are wire validity now, so
/// a silently wrong catalog would 400 every request of an affected route: the
/// version makes a stale cache unambiguous and forces the embedded fallback.
/// Bump on every shape change of [`choreo_ai_protocols::ModelEntry`] or
/// `ProviderEntry`.
const CATALOG_CACHE_SCHEMA: u16 = 4;

/// User overlay filename under the config dir. `pub` so the shared config
/// transport's subscription (in `run_server`) registers the same basename.
pub const USER_OVERLAY_NAME: &str = "models-overlay.toml";

/// Filesystem locations the runtime catalog pipeline touches. Kept in one
/// struct so the daemon state, the maintenance-thread spawn, and the unit
/// tests all agree on where things live. The etag and the last-attempt
/// timestamp deliberately do NOT live here — they are persisted in the DB
/// (`catalog_state` table, see [`crate::db`]), not on the filesystem.
#[derive(Debug, Clone, Default)]
pub struct CatalogPaths {
    /// Postcard cache of the normalized models.dev base
    /// (`$XDG_DATA_HOME/choreographr/catalog.bin`).
    pub bin: PathBuf,
    /// User overlay TOML (`$XDG_CONFIG_HOME/choreographr/models-overlay.toml`),
    /// read for the user policy layer (the config transport watches its
    /// basename).
    pub overlay: PathBuf,
}

impl CatalogPaths {
    /// Resolve the standard locations, mirroring `crate::db::db_path` and
    /// `crate::config::config_path`: the shared resolver's data/config roots
    /// (platform `dirs` by default, or `{base}/data` and `{base}/config` under
    /// `--base-dir`), joined with the `choreographr` subdirectory. Falls back
    /// to empty paths (everything degrades to the embedded catalog) when the
    /// dirs lookup fails, so startup never hard-fails on an exotic HOME-less
    /// environment.
    pub fn from_dirs() -> Self {
        let data_dir = choreo_shared::paths::data_dir();
        let config_dir = choreo_shared::paths::config_dir();
        if let (Ok(data), Ok(config)) = (&data_dir, &config_dir) {
            Self {
                bin: data.join(CATALOG_BIN_NAME),
                overlay: config.join(USER_OVERLAY_NAME),
            }
        } else {
            warn!(
                ?data_dir,
                ?config_dir,
                "could not resolve catalog cache/overlay dirs; using the embedded catalog only",
            );
            Self::default()
        }
    }
}

/// Load the cached normalized base from `path` (postcard). Logs a warning and
/// returns `None` when the file is missing, carries a different schema version,
/// or fails to deserialize — the caller then falls back to the embedded
/// `catalog.bin` (load order: valid cache file → embedded).
pub(crate) fn load_cached_base(path: &Path) -> Option<Vec<ProviderEntry>> {
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return None,
        Err(e) => {
            warn!(path = %path.display(), error = %e, "failed to read catalog cache");
            return None;
        }
    };
    // The version is a separate leading field, so a stale blob is rejected
    // before any provider entry is decoded out of the wrong byte offsets.
    let (version, rest) = match postcard::take_from_bytes::<u16>(&bytes) {
        Ok(parts) => parts,
        Err(e) => {
            warn!(
                path = %path.display(),
                error = %e,
                "catalog cache has no schema version; falling back to the embedded catalog",
            );
            return None;
        }
    };
    if version != CATALOG_CACHE_SCHEMA {
        warn!(
            path = %path.display(),
            cached = version,
            expected = CATALOG_CACHE_SCHEMA,
            "catalog cache schema changed; falling back to the embedded catalog",
        );
        return None;
    }
    match postcard::from_bytes(rest) {
        Ok(base) => Some(base),
        Err(e) => {
            warn!(
                path = %path.display(),
                error = %e,
                "catalog cache failed to deserialize; falling back to the embedded catalog",
            );
            None
        }
    }
}

/// The base the daemon is running, as it lives on disk: the runtime cache
/// (`catalog.bin`) when present and valid, else the embedded blob — the same
/// load order the startup path uses, so both always name the same base.
///
/// # Ordering invariant
///
/// This is only correct while `catalog.bin` reflects the base the daemon
/// actually applied. That holds by construction because the caller is the
/// command loop — the cache file's only writer — with one exception it must
/// handle itself: a live fetch whose cache persist FAILED, which leaves the
/// live base ahead of the disk. The command loop keeps that base in
/// `DaemonState::catalog_applied_base` and re-merges from there instead of
/// calling this function.
pub(crate) fn load_persisted_base(path: &Path) -> Vec<ProviderEntry> {
    if let Some(base) = load_cached_base(path) {
        return base;
    }
    // No usable cache: `load_cached_base` already warned if the file existed
    // but was unreadable or stale, so this is the ordinary absent-cache case (a
    // fresh install, or a cache-miss start whose first fetch has not landed
    // yet). The embedded blob is exactly what the startup load used in that
    // state, so the base still matches the running catalog.
    debug!(
        path = %path.display(),
        "no catalog cache on disk; re-merging over the embedded catalog",
    );
    load_bundled_base()
}

/// Persist the cache bin atomically (temp file in the same directory → fsync →
/// rename, so a reader sees either the old or the new file, never a torn one).
/// The models.dev **etag is NOT written here** — it is persisted to the DB by
/// the daemon command loop AFTER this returns ([`crate::daemon::DaemonState::
/// persist_catalog_cache`]), so a crash between the two writes leaves the OLD
/// etag paired with the OLD bin, which self-heals via a 200 on the next fetch
/// (a new etag over old content would 304 forever instead).
pub(crate) fn write_catalog_cache(base: &[ProviderEntry], bin_path: &Path) -> io::Result<()> {
    // Version first, then the entries: keeps a stale blob from being decoded at
    // the wrong offsets (see `CATALOG_CACHE_SCHEMA`).
    let mut bytes = postcard::to_allocvec(&CATALOG_CACHE_SCHEMA).map_err(io::Error::other)?;
    bytes.extend_from_slice(&postcard::to_allocvec(base).map_err(io::Error::other)?);
    write_file_atomic(bin_path, &bytes)
}

/// Ensure the catalog **data dir** (cache parent) exists before the
/// maintenance pipeline starts. `write_file_atomic` would create it on first
/// persist anyway; creating it up front means the cache survives a run even if
/// the first fetch is skipped (a fresh cache). The **config dir** is not
/// created here — the unified config transport ([`crate::config_watch`]) owns
/// that, since it is shared across all watched files. A creation failure is
/// logged, never fatal — the daemon degrades to the embedded catalog and
/// manual `/refresh-models` reloads.
pub(super) fn ensure_runtime_dirs(paths: &CatalogPaths) {
    let Some(dir) = paths.bin.parent() else {
        return;
    };
    match std::fs::create_dir_all(dir) {
        Ok(()) => debug!(dir = %dir.display(), "catalog data dir ready"),
        Err(e) => warn!(
            dir = %dir.display(),
            error = %e,
            "failed to create the catalog data dir; cache persistence may be unavailable",
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::fixtures::tiny_base;

    #[test]
    fn cached_base_round_trips_through_postcard() {
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("catalog.bin");

        // No cache yet → None, and the caller falls back to embedded.
        assert!(load_cached_base(&bin).is_none());

        write_catalog_cache(&tiny_base(), &bin).unwrap();

        let loaded = load_cached_base(&bin).expect("cache loads");
        // ProviderEntry has no PartialEq; compare the load-bearing fields.
        assert_eq!(loaded.len(), tiny_base().len());
        assert_eq!(loaded[0].slug, "acme");
        assert_eq!(loaded[1].slug, "zoocorp");
    }

    #[test]
    fn corrupted_cache_falls_back_to_none() {
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("catalog.bin");
        std::fs::write(&bin, b"not postcard data").unwrap();
        // A corrupt cache must not brick the daemon: it logs a warning and
        // returns None so the embedded catalog.bin is used instead.
        assert!(load_cached_base(&bin).is_none());
    }

    #[test]
    fn stale_cache_schema_falls_back_to_none() {
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("catalog.bin");
        // A blob written by an older schema: valid postcard, wrong version. It
        // must be rejected rather than decoded against the current field layout
        // (reasoning-echo requirements are wire validity, so a wrong catalog
        // would 400 every request of an affected route).
        let mut bytes = postcard::to_allocvec(&(CATALOG_CACHE_SCHEMA - 1)).unwrap();
        bytes.extend_from_slice(&postcard::to_allocvec(&tiny_base()).unwrap());
        std::fs::write(&bin, bytes).unwrap();
        assert!(load_cached_base(&bin).is_none());
    }

    #[test]
    fn persisted_base_prefers_the_cache_file() {
        // The overlay-edit re-merge must see the base the daemon is running —
        // here a distinctive one-provider cache, not the 150+ provider
        // embedded blob.
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("catalog.bin");
        write_catalog_cache(&tiny_base(), &bin).unwrap();

        let base = load_persisted_base(&bin);
        assert_eq!(base.len(), tiny_base().len());
        assert_eq!(base[0].slug, "acme");
    }

    #[test]
    fn persisted_base_falls_back_to_the_embedded_blob() {
        // No cache (or an unusable one) → the SAME fallback the startup load
        // takes, so an overlay edit still re-merges over the base the daemon
        // actually applied.
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("missing").join("catalog.bin");

        let base = load_persisted_base(&bin);
        assert!(
            base.len() >= 150,
            "the embedded models.dev base has >=150 providers, got {}",
            base.len()
        );
        // Reading never creates the cache: persisting is a live fetch's job.
        assert!(
            !bin.exists(),
            "load_persisted_base must not write the cache"
        );
    }

    #[test]
    fn catalog_paths_resolve_under_choreographr_dirs() {
        // from_dirs() follows the same convention as db_path/accounts_config_path.
        // We can't assert the absolute value (the XDG dirs are env-dependent),
        // but the file NAMES must match the documented contract. The etag and
        // last-attempt timestamp are deliberately NOT here — they live in the
        // DB (catalog_state), not on the filesystem.
        let paths = CatalogPaths::from_dirs();
        assert!(paths.bin.ends_with("choreographr/catalog.bin"));
        assert!(paths.overlay.ends_with("choreographr/models-overlay.toml"));
    }

    #[test]
    fn ensure_runtime_dirs_creates_the_data_dir() {
        // The cache data dir (bin parent) is created up front so the cache can
        // be written even if the first fetch is skipped. The config dir is NOT
        // created here — the unified config transport owns that.
        let dir = tempfile::tempdir().unwrap();
        let paths = CatalogPaths {
            bin: dir.path().join("data/choreographr/catalog.bin"),
            overlay: dir.path().join("config/choreographr/models-overlay.toml"),
        };

        ensure_runtime_dirs(&paths);
        assert!(
            paths.bin.parent().unwrap().is_dir(),
            "data dir must exist after ensure_runtime_dirs"
        );

        // The config dir (overlay parent) is deliberately left for the config
        // transport to create — this function must not create it.
        assert!(
            !paths.overlay.parent().unwrap().exists(),
            "ensure_runtime_dirs must not create the config dir (the transport owns it)"
        );

        // Idempotent: a second pass must not error.
        ensure_runtime_dirs(&paths);
    }

    #[test]
    fn ensure_runtime_dirs_tolerates_empty_paths() {
        // A HOME-less fallback (CatalogPaths::default()) has empty paths —
        // no parent to create, no panic.
        ensure_runtime_dirs(&CatalogPaths::default());
    }
}
