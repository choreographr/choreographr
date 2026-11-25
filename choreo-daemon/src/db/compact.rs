//! Startup space reclamation for the `redb` store.
//!
//! ## Why the space accumulates
//!
//! `redb` never returns freed space to the filesystem. Deleting a session
//! removes its rows, and the pages they occupied become free for LATER writes,
//! but the file keeps its high-water length until something compacts it. An
//! install that deletes old sessions therefore keeps paying for them on disk
//! until the file is rewritten.
//!
//! ## Why [`CompactionMode::Maximum`]
//!
//! `Balanced` moves pages towards the front of the file and truncates the free
//! space that leaves at its end, but it keeps each page's contents: the free
//! space inside partially filled pages is left for later inserts to fill.
//! `Maximum` also packs those entries into fewer pages. Measured on a release
//! build, 64 MiB of logical data with every other row deleted:
//!
//! - 24 KiB values — the turns and attachments shape — cost 29.3 ms and left a
//!   42.8 MiB file under `Balanced`, 30.6 ms and the same 42.8 MiB under
//!   `Maximum`. Identical: a value that spans pages leaves packing nothing to
//!   pack.
//! - 200 B values — the sessions, `session_kv` and `meta` shape — cost 69.4 ms
//!   and left a 70.1 MiB file under `Balanced` (0.9 MiB reclaimed) but 155.2 ms
//!   and 35.0 MiB under `Maximum` (36.0 MiB reclaimed). Small rows leave most of
//!   every page empty when they go, and only `Maximum` takes that back.
//!
//! The cost `redb` documents for `Maximum` — inserts slower until the packed
//! pages split — does not materialise here: measured after the pass, inserts
//! took 21.9 ms against 24.8 ms under `Balanced` and grew the file by 35.0 MiB
//! against 70.1 MiB.
//!
//! ## Why every start, with no gate
//!
//! A pass walks the file, so gating it on a share-and-size heuristic is
//! tempting — and wrong in both directions. It declines the largest absolute
//! wins, because 200 MiB free on a 1 GB file is only 20% of it; and it fires on
//! stores with nothing to reclaim, because the free-space estimate is
//! `file_bytes - (stored_bytes + metadata_bytes)` while `stored_bytes` excludes
//! the per-row and index overhead, so a fully compacted store still reads about
//! a quarter of its file as free. The cost of not gating is negligible: a
//! settled store's pass is a no-op in single-digit milliseconds — measured 0.3
//! ms (`Balanced`) and 0.4 ms (`Maximum`) on the large-row shape, 3.7 ms and
//! 4.5 ms on the small-row one. The pass therefore runs unconditionally.
//!
//! ## Crash safety
//!
//! `redb` compacts under two-phase commit, so an interrupted pass leaves either
//! the pre-pass image or the compacted one visible — never a half-written store
//! — and an interrupted pass is repaired when the store is next opened.
//!
//! The caller owns *when* the pass runs; see [`crate::daemon::open`], which
//! places it after the pre-migration backup and the migrations and before the
//! store is shared (because `redb::Database::compact` takes `&mut self`).
//!
//! ## A failed pass is never fatal
//!
//! Reclamation is an optimisation, so nothing here stops the daemon. Two
//! outcomes are possible and [`StartupCompaction`] names them: the store is
//! usable, or the pass failed in a way that latched the handle and the caller
//! must re-open. The second case is why the failure cannot simply be swallowed:
//! `redb` latches the first failed file operation for the life of the handle, so
//! every later read and write through that handle fails — but the FILE is
//! intact, and a fresh handle over it works. Compaction is itself a write pass,
//! so on a nearly-full disk it can CAUSE the out-of-space it fails on; refusing
//! to start would then make a transient disk-full condition unstartable, and
//! every relaunch would pay the same failed pass again.
//!
//! [`CompactionMode::Maximum`]: redb::CompactionMode::Maximum

use std::path::Path;

use tracing::{error, info, warn};

use super::store::Store;

/// What one startup pass decided about the store it ran on.
///
/// The pass never fails the daemon, so it reports an outcome rather than an
/// error: the caller acts on it, and the recovery that needs both the path and
/// ownership of the handle (re-opening) stays at the call site that has them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StartupCompaction {
    /// The pass ran, failed harmlessly, or had nothing to do. The store handed
    /// in is usable and whatever was reclaimed stays reclaimed.
    StoreUsable,
    /// The pass failed AND latched the handle: every later operation on the
    /// `Store` handed in fails with `PreviousIo`. The file itself is intact, so
    /// the caller must drop that store and re-open the same path to continue
    /// without reclamation.
    HandleLatched,
}

impl StartupCompaction {
    /// Whether the pass latched the handle handed in (so it must be re-opened).
    #[must_use]
    pub fn latched(self) -> bool {
        matches!(self, Self::HandleLatched)
    }
}

/// Reclaim a `redb` store's free space before it is shared with the daemon's
/// threads.
///
/// Runs unconditionally: see the module docs for why gating it is not worth it,
/// and for why a failure is reported rather than raised.
pub(crate) fn compact_at_startup(store: &mut Store, path: &Path) -> StartupCompaction {
    // The lengths only fill the log line below, so an unreadable path — an
    // in-memory store, or a file moved out from under us — must never fail
    // startup: an unknown length logs as `None`.
    let before = file_len(path);
    match store.db_mut().compact(redb::CompactionMode::Maximum) {
        Ok(performed) => {
            let after = file_len(path);
            info!(
                performed,
                file_bytes_before = ?before,
                file_bytes_after = ?after,
                reclaimed = ?reclaimed(before, after),
                "database compaction finished"
            );
            StartupCompaction::StoreUsable
        }
        Err(e) => {
            // Built through `Store::err`, the `db` module's single storage-error
            // conversion, so a latch behind this failure is observed exactly as
            // it is on every other path. No escalation route exists this early
            // in startup, so this only constructs the error.
            let err = store.err(format!("redb compact: {e}"));
            if store.is_latched() {
                // Nothing more can be written through THIS handle, but the file
                // is fine — the caller re-opens it and runs without the pass.
                error!(
                    error = %err,
                    "database compaction latched the store handle; re-opening without reclaiming space"
                );
                return StartupCompaction::HandleLatched;
            }
            // Not a shape `compact()` documents for a store with no live
            // transaction or savepoint (we hold neither at startup), but
            // reclamation is an optimisation: a daemon that cannot shrink its
            // file keeps running at its current size.
            warn!(
                error = %err,
                "database compaction failed; continuing at the file's current size"
            );
            StartupCompaction::StoreUsable
        }
    }
}

/// Test-only seam for [`compact_at_startup`], so the crate's integration suite
/// can drive the REAL startup pass (its `Maximum` mode and its failure
/// decisions, which a test re-implementing the `redb` call would silently stop
/// matching) against a store on a real file.
///
/// Deliberately a wrapper rather than exposing `Store::db_mut` to the suite: the
/// mutable-handle route is the smaller hole, but it moves the mode constant into
/// the test and lets the test pass while production compacts differently.
#[cfg(any(test, feature = "test-utils"))]
pub fn compact_at_startup_for_test(store: &mut Store, path: &Path) -> StartupCompaction {
    compact_at_startup(store, path)
}

/// The file's length, or `None` when it cannot be read.
fn file_len(path: &Path) -> Option<u64> {
    std::fs::metadata(path).ok().map(|m| m.len())
}

/// The bytes a pass returned, when both lengths are known.
///
/// Saturating: a file that grew across the pass has nothing to reclaim, and must
/// not underflow a `u64` subtraction.
fn reclaimed(before: Option<u64>, after: Option<u64>) -> Option<u64> {
    Some(before?.saturating_sub(after?))
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    use super::*;
    use crate::db::store::test_backend::BreakableBackend;
    use crate::db::{kv_get, kv_set};

    /// A store over `redb`'s in-memory backend: no file for `file_len` to read,
    /// which is exactly the "unknown length" path, plus the flag that breaks the
    /// backend's writes.
    fn in_memory_store() -> (Store, Arc<AtomicBool>) {
        let (backend, failing) = BreakableBackend::new();
        let store = Store::with_backend(backend).expect("create the in-memory store");
        (store, failing)
    }

    /// A healthy store compacts cleanly over an unknown file length, and stays
    /// usable afterwards: the pass leaves no transaction, savepoint or stale
    /// handle behind.
    #[test]
    fn a_healthy_in_memory_store_compacts_cleanly() {
        let (mut store, _failing) = in_memory_store();
        // A couple of written rows, so the pass has real pages to walk.
        kv_set(&store, 1, "a", b"one").expect("write row a");
        kv_set(&store, 1, "b", b"two").expect("write row b");

        assert_eq!(
            compact_at_startup(&mut store, Path::new("in-memory")),
            StartupCompaction::StoreUsable,
            "a healthy store must compact cleanly, with an unknown file length"
        );

        kv_set(&store, 1, "c", b"three").expect("the store must still accept writes");
        assert_eq!(
            kv_get(&store, 1, "c").expect("read the row written after the pass"),
            Some(b"three".to_vec()),
            "the row written after the pass must be readable"
        );
        assert_eq!(
            kv_get(&store, 1, "a").expect("read a row written before the pass"),
            Some(b"one".to_vec()),
            "the pass must preserve the rows already stored"
        );
    }

    /// A pass whose failure latched the handle is REPORTED, not swallowed: the
    /// store handed in is unusable and the caller must re-open it.
    #[test]
    fn a_compaction_that_latches_the_handle_is_reported() {
        let (mut store, failing) = in_memory_store();
        // A successful write first, so the failure below cannot be a
        // creation artefact.
        kv_set(&store, 1, "a", b"one").expect("write a row");
        failing.store(true, Ordering::SeqCst);

        assert!(
            compact_at_startup(&mut store, Path::new("in-memory")).latched(),
            "a pass that latches the handle must be reported as such"
        );
        assert!(
            store.is_latched(),
            "the backend's write failure must latch the handle"
        );
    }

    /// The recovery the outcome exists for: the file survives a latched pass, so
    /// a FRESH handle over it works. Reclamation is an optimisation; a daemon
    /// must not refuse to start over one — a pass on a full disk latches the
    /// handle and would otherwise make the store unstartable until space was
    /// freed.
    #[test]
    fn the_file_outlives_a_latched_pass_and_reopens_cleanly() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.redb");
        let mut store = crate::db::open_db_at(&path).expect("open a file-backed store");
        kv_set(&store, 1, "a", b"one").expect("write a row");

        // A healthy pass leaves the store usable.
        assert_eq!(
            compact_at_startup(&mut store, &path),
            StartupCompaction::StoreUsable
        );

        // The store's file is still a usable store after the pass. (The latched
        // case cannot be produced over a real file without a full disk; the
        // in-memory test above covers the latch, and this covers the property
        // the recovery depends on: re-opening the same path yields a working
        // handle that sees the pre-pass rows.)
        drop(store);
        let reopened = crate::db::open_db_at(&path).expect("re-open the store after the pass");
        assert_eq!(
            kv_get(&reopened, 1, "a").expect("read back the pre-pass row"),
            Some(b"one".to_vec())
        );
    }
}
