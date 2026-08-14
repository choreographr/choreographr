//! Atomic file writes for the catalog artifact pipeline.
//!
//! [`write_file_atomic`] is the shared writer behind every catalog artifact
//! `catalog-gen` produces: the committed `catalog/catalog.bin` and the
//! gitignored `catalog/models.dev.json` snapshot cache. It writes via a temp
//! file in the same directory + fsync + rename, so a reader — the daemon
//! embedding the blob, or a later generator run — always sees either the old
//! or the new file, never a torn one. The same helper lives in
//! `choreo-daemon` for the runtime cache; this copy serves the generator
//! binary and stays in the catalog module because both files it writes are
//! catalog artifacts.

use std::io::{self, Write};
use std::path::Path;

/// Write `bytes` to `path` atomically: a temp file in the same directory,
/// write + fsync, then rename over the target (atomic on POSIX). The temp
/// file must live in the same directory as the target so the rename never
/// crosses a filesystem boundary.
pub fn write_file_atomic(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(dir)?;
    let mut tmp = tempfile::NamedTempFile::new_in(dir)?;
    tmp.write_all(bytes)?;
    tmp.as_file().sync_all()?;
    tmp.persist(path).map_err(|e| e.error)?;
    // Best-effort directory fsync so the rename itself is durable: POSIX
    // rename durability requires syncing the parent directory, not just the
    // file. Some platforms/filesystems refuse to fsync a directory handle
    // (EINVAL) — a failure here only weakens crash durability, never
    // correctness, so it is logged rather than propagated.
    if let Some(dir) = path.parent() {
        match std::fs::File::open(dir).and_then(|d| d.sync_all()) {
            Ok(()) => {}
            Err(e) => {
                tracing::debug!(
                    path = %dir.display(),
                    error = %e,
                    "failed to fsync the catalog directory after rename",
                );
            }
        }
    }
    Ok(())
}
