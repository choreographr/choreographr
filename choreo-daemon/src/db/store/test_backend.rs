//! A `redb` storage backend a test can break on demand.
//!
//! Producing a latched handle honestly would need a filesystem that fails
//! writes — a full disk, or the read-only remount a Linux filesystem performs
//! after an I/O error. [`BreakableBackend`] instead wraps `redb`'s in-memory
//! backend and starts failing its `write`/`sync_data` calls the moment its flag
//! is set, which is enough: `redb` latches the FIRST failed file operation, so a
//! backend that fails like that puts a real handle into the real latched state.
//!
//! Shared through the `test-utils` feature so the crate's integration suite can
//! use the same backend as the unit tests in [`super`] rather than keeping a
//! second copy; [`crate::db::BreakableBackend`] is that re-export.

use std::io;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use redb::{BackendError, StorageBackend};

/// An in-memory store that can be told to fail its writes, which is what a real
/// disk does when it fills up: the first write to fail is followed by every
/// later read and write failing too (that second half is `redb`'s latch, not
/// this type's job).
#[derive(Debug)]
pub struct BreakableBackend {
    inner: redb::backends::InMemoryBackend,
    failing: Arc<AtomicBool>,
}

impl BreakableBackend {
    /// A healthy backend plus the flag that breaks it: once the flag is set,
    /// every later `write` and `sync_data` fails with the `StorageFull` error
    /// whose message is "disk full (test)".
    #[must_use]
    pub fn new() -> (Self, Arc<AtomicBool>) {
        let failing = Arc::new(AtomicBool::new(false));
        let backend = Self {
            inner: redb::backends::InMemoryBackend::new(),
            failing: Arc::clone(&failing),
        };
        (backend, failing)
    }
}

/// An I/O error carrying a string only these tests produce, so a failure
/// surfaced to a caller can be told apart from `redb`'s own errors.
fn disk_full() -> BackendError {
    BackendError::Io(io::Error::new(
        io::ErrorKind::StorageFull,
        "disk full (test)",
    ))
}

impl StorageBackend for BreakableBackend {
    fn len(&self) -> Result<u64, BackendError> {
        self.inner.len()
    }

    fn read(&self, offset: u64, out: &mut [u8]) -> Result<(), BackendError> {
        self.inner.read(offset, out)
    }

    fn set_len(&self, len: u64) -> Result<(), BackendError> {
        self.inner.set_len(len)
    }

    // Only the write half breaks: a read still succeeds after the failure, so a
    // test can prove the escalated state is `redb`'s latch rather than this
    // backend refusing everything.
    fn sync_data(&self) -> Result<(), BackendError> {
        if self.failing.load(Ordering::Acquire) {
            return Err(disk_full());
        }
        self.inner.sync_data()
    }

    fn write(&self, offset: u64, data: &[u8]) -> Result<(), BackendError> {
        if self.failing.load(Ordering::Acquire) {
            return Err(disk_full());
        }
        self.inner.write(offset, data)
    }
}
