//! Detection of, and escalation from, a `redb` handle that has latched a
//! permanent I/O failure.
//!
//! `redb` latches the first failure of any file operation — a `read`, a
//! `write`, a `set_len`, or the `sync_data` at the end of a durable commit —
//! into an internal `io_failed` flag that nothing ever clears. Every later
//! operation on that `Database` then fails with `StorageError::PreviousIo`,
//! whose own message is "Please close and re-open the database". One `ENOSPC`
//! on a full disk, or the `EROFS` a Linux filesystem returns after remounting
//! itself read-only in response to an error, is enough to reach it.
//!
//! The daemon holds one handle for its whole life and clones it into every
//! session thread and the catalog worker, so it cannot re-open in place. The
//! first observation of a poison therefore **escalates**: the daemon tells
//! every client it is shutting down and stops. That is the honest response —
//! once the handle is latched nothing can be persisted, so a daemon that keeps
//! accepting work loses it silently — and the next launch re-opens the store,
//! which also runs `redb`'s crash repair.
//!
//! Detection rides the `db` module's single error conversion ([`super::db_err`])
//! rather than each call site: every storage failure, on any read or write
//! path, funnels through that one function, so one hook covers them all.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, LazyLock, Mutex, PoisonError, Weak};

use tracing::{debug, error};

/// The running daemon's escalation route, installed by `start_daemon_core`.
///
/// `None` when no daemon installed one — the `db` module's own unit tests, and
/// the startup window before the command loop exists — in which case a poison
/// is logged but not escalated.
static ESCALATION: LazyLock<Mutex<Option<Escalation>>> = LazyLock::new(|| Mutex::new(None));

/// Set once a poison has been escalated.
///
/// Every later storage operation on a latched handle re-observes the same
/// failure, so without this fence a session writing in a loop would emit one
/// shutdown broadcast per write and bury the clients it means to inform.
static ESCALATED: AtomicBool = AtomicBool::new(false);

struct Escalation {
    /// A `Weak` so this registry never extends the handle's life. Holding a
    /// strong reference would keep `redb`'s exclusive file lock held past the
    /// daemon's own drop, and the next launch could not open the store.
    handle: Weak<redb::Database>,
    /// Called at most once, from whichever thread first observes the poison.
    notify: Arc<dyn Fn() + Send + Sync>,
}

/// Route a latched store handle to `notify`, which shuts the daemon down.
///
/// Called once per running daemon, beside the command channel it escalates
/// through. A restarted embedded daemon calls it again; the fence is cleared
/// with the install because it describes the handle this install tracks, not
/// the process's earlier daemons.
pub(crate) fn install(handle: Weak<redb::Database>, notify: impl Fn() + Send + Sync + 'static) {
    ESCALATED.store(false, Ordering::Release);
    *ESCALATION.lock().unwrap_or_else(PoisonError::into_inner) = Some(Escalation {
        handle,
        notify: Arc::new(notify),
    });
}

/// Escalate if the running daemon's store handle has latched, reporting `context`
/// as the failure that exposed it.
///
/// Idempotent: only the first observation escalates, so this is safe to call
/// from every storage-failure path.
pub(super) fn observe(context: &str) {
    // Clone the route out of the registry before calling it: the notifier sends
    // on the daemon's command channel, and no other thread should wait on this
    // registry while it does.
    let Some(notify) = escalate_route() else {
        return;
    };
    if ESCALATED.swap(true, Ordering::AcqRel) {
        debug!(
            context,
            "storage failure on an already-escalated handle; not escalating again"
        );
        return;
    }
    error!(
        context,
        "the database handle latched a permanent I/O failure; every later read and write will \
         fail. Shutting down — restart to repair and re-open the store"
    );
    notify();
}

/// The installed notifier, when the handle it tracks has latched.
fn escalate_route() -> Option<Arc<dyn Fn() + Send + Sync>> {
    let guard = ESCALATION.lock().unwrap_or_else(PoisonError::into_inner);
    let escalation = guard.as_ref()?;
    let db = escalation.handle.upgrade()?;
    latched(&db).then(|| Arc::clone(&escalation.notify))
}

/// Whether `db` has latched a permanent I/O failure.
///
/// `try_begin_write` rather than `begin_write`, for two reasons: it reports the
/// latch before it contends for the write slot, so the answer is exact even
/// while another thread holds a write transaction, and it never blocks — which
/// matters on an error path reached from a session thread.
///
/// `PreviousIo` is unambiguous. A healthy handle yields `Ok` (dropped
/// immediately, aborting an empty transaction) or `WouldBlock` while another
/// writer is live, and neither matches.
fn latched(db: &redb::Database) -> bool {
    matches!(
        db.try_begin_write(),
        Err(redb::TransactionError::Storage(
            redb::StorageError::PreviousIo
        ))
    )
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicUsize;

    use redb::{BackendError, StorageBackend};

    use super::*;

    /// An in-memory store that can be told to fail its writes, which is what a
    /// real disk does when it fills up: the first write to fail is followed by
    /// every later read and write failing too.
    #[derive(Debug)]
    struct BreakableBackend {
        inner: redb::backends::InMemoryBackend,
        failing: Arc<AtomicBool>,
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

    /// An I/O error carrying a string only this test produces, so a failure
    /// surfaced to a caller can be told apart from `redb`'s own errors.
    fn disk_full() -> BackendError {
        BackendError::Io(std::io::Error::new(
            std::io::ErrorKind::StorageFull,
            "disk full (test)",
        ))
    }

    fn breakable_store() -> (Arc<redb::Database>, Arc<AtomicBool>) {
        let failing = Arc::new(AtomicBool::new(false));
        let db = redb::Database::builder()
            .create_with_backend(BreakableBackend {
                inner: redb::backends::InMemoryBackend::new(),
                failing: Arc::clone(&failing),
            })
            .expect("create the breakable store");
        (Arc::new(db), failing)
    }

    /// A minimal persistable turn, so the test goes through `write_turn` — the
    /// turn-persistence path that used to swallow its first error in a retry
    /// loop, and the one whose failure loses real work.
    fn dummy_turn() -> choreo_proto::Turn {
        choreo_proto::Turn {
            created_at: choreo_proto::TimestampMs::now(),
            undone: false,
            error: None,
            user_text: Some("hello".into()),
            assistant_text: None,
            assistant_reasoning: None,
            tool_calls: Vec::new(),
            token_usage: None,
            tool_results: Vec::new(),
            displayed_images: Vec::new(),
            reasoning_artifact: None,
            reasoning_producer: None,
        }
    }

    /// The whole contract in one case: the caller sees the REAL cause (not a
    /// later previous-IO error), the poison escalates exactly once, and the
    /// handle then probes as latched.
    ///
    /// The "disk full (test)" assertion is the regression guard for the retry
    /// loops this replaced: they discarded the first error and retried until
    /// the latch was visible, so the caller was told "a previous I/O error
    /// occurred" and the actual reason was lost.
    #[test]
    fn a_failed_write_escalates_once_and_keeps_the_real_cause() {
        let (db, failing) = breakable_store();
        // A successful write first, so the failure below cannot be a creation
        // artefact.
        crate::db::write_turn(&db, 1, 0, &dummy_turn()).expect("healthy write");

        let escalations = Arc::new(AtomicUsize::new(0));
        {
            let counter = Arc::clone(&escalations);
            install(Arc::downgrade(&db), move || {
                counter.fetch_add(1, Ordering::SeqCst);
            });
        }
        assert!(
            !latched(&db),
            "a healthy handle must never probe as latched"
        );

        failing.store(true, Ordering::SeqCst);
        let first =
            crate::db::write_turn(&db, 1, 1, &dummy_turn()).expect_err("the write must fail");
        assert!(
            first.to_string().contains("disk full (test)"),
            "the caller must see the cause, not a masked previous-IO error: {first}"
        );
        assert_eq!(
            escalations.load(Ordering::SeqCst),
            1,
            "the first observation must escalate"
        );
        assert!(latched(&db), "the handle must now probe as latched");

        // Every later operation re-observes the same latch; only the first
        // escalates.
        crate::db::write_turn(&db, 1, 2, &dummy_turn()).expect_err("still failing");
        assert_eq!(
            escalations.load(Ordering::SeqCst),
            1,
            "escalation must be one-shot"
        );
    }

    /// The probe follows the INSTALLED handle, so a failure in some other store
    /// cannot shut the daemon down.
    #[test]
    fn a_failure_in_an_unrelated_store_does_not_escalate() {
        let (installed, _installed_failing) = breakable_store();
        let (other, other_failing) = breakable_store();

        let escalations = Arc::new(AtomicUsize::new(0));
        {
            let counter = Arc::clone(&escalations);
            install(Arc::downgrade(&installed), move || {
                counter.fetch_add(1, Ordering::SeqCst);
            });
        }

        other_failing.store(true, Ordering::SeqCst);
        crate::db::write_turn(&other, 1, 0, &dummy_turn()).expect_err("the other store must fail");

        assert!(latched(&other), "the failing store latched");
        assert!(!latched(&installed), "the installed store is still healthy");
        assert_eq!(
            escalations.load(Ordering::SeqCst),
            0,
            "an unrelated store's failure must not shut the daemon down"
        );
    }
}
