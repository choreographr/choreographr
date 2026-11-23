//! Atomic file writes for the catalog artifact pipeline.
//!
//! [`write_file_atomic`] is the shared writer behind every catalog artifact
//! `catalog-gen` produces: the committed `catalog/catalog.bin` and the
//! gitignored `catalog/models.dev.json` snapshot cache. It delegates to
//! [`choreo_shared::atomic_write::write_private_atomic`], the workspace's one
//! implementation of temp file + fsync + rename, so a reader — the daemon
//! embedding the blob, or a later generator run — always sees either the old or
//! the new file, never a torn one.

use std::io;
use std::path::Path;

/// Atomically replace `path` with `bytes` (temp file + rename in the same
/// directory, so a crash leaves either the old or the new file, never a
/// truncated one).
///
/// Kept as a named alias of
/// [`choreo_shared::atomic_write::write_private_atomic`] because the catalog
/// pipeline is its caller: the artifact semantics ("the new snapshot is
/// published whole or not at all") belong to this module, the mechanism does
/// not.
///
/// # Errors
///
/// Returns `io::Error` if the temp file cannot be written, flushed, or
/// renamed into place.
pub fn write_file_atomic(path: &Path, bytes: &[u8]) -> io::Result<()> {
    choreo_shared::atomic_write::write_private_atomic(path, bytes)
}
