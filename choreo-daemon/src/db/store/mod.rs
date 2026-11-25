//! The daemon's `redb` handle, plus the latch-escalation state that rides the
//! handle it owns.
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
//! first observation of a latch therefore **escalates**: the daemon tells every
//! client it is shutting down and stops. That is the honest response — once the
//! handle is latched nothing can be persisted, so a daemon that keeps accepting
//! work loses it silently — and the next launch re-opens the store, which also
//! runs `redb`'s crash repair.
//!
//! ## Where the escalation state lives
//!
//! On the [`Store`] itself, beside the handle it guards. A latch is a property
//! of ONE `redb::Database`: the fence that makes the escalation one-shot and
//! the notifier that closes the loop are fields of the store that owns that
//! handle, so it is never ambiguous which store a failure belongs to. Two
//! stores in one process — a unit test per store, an embedded daemon beside its
//! host's — therefore escalate independently, and neither can silence nor
//! trigger the other. `redb` offers no way to un-latch a handle, so a
//! process-global registry would have to guess at that ownership; carrying the
//! state on the handle makes the question unrepresentable.
//!
//! Detection rides `Store::err`, the `db` module's single error conversion,
//! rather than each call site: every storage failure, on any read or write
//! path, funnels through it, so one hook covers them all. A failure that is NOT
//! storage — the zstd codec's corrupt rows, say — is built with
//! `io::Error::other` instead, because a data error says nothing about the
//! handle.

use std::io;
use std::ops::{Deref, DerefMut};
use std::path::Path;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};

use tracing::{debug, error, warn};

/// A storage backend a test can break on demand, so a latched handle can be
/// produced without filling a real disk. Shared by this module's unit tests and
/// the crate's integration suite.
#[cfg(any(test, feature = "test-utils"))]
pub mod test_backend;

/// The daemon's durable store: the `redb` database together with the
/// escalation state of the handle it owns.
///
/// One `Arc<Store>` is threaded to every thread that persists anything (the
/// command loop, each session thread, the catalog maintenance worker, the
/// connection threads serving on-demand image reads), so the store is the one
/// place a latched handle can be noticed.
pub struct Store {
    db: redb::Database,
    /// Set once by the daemon assembly; empty in unit tests and during startup.
    escalation: OnceLock<Escalation>,
}

/// What a latched handle escalates to, installed once per running daemon.
struct Escalation {
    /// The one-shot fence: every later operation on a latched handle
    /// re-observes the same failure, so only the first observation may
    /// escalate.
    escalated: AtomicBool,
    /// Requests the daemon shutdown. Cannot fail (it sets a flag and wakes the
    /// accept loop), so the fence needs no success retry.
    notify: Box<dyn Fn() + Send + Sync>,
}

impl Store {
    /// Open (or create) the store at `path`.
    ///
    /// [`open_db_at`](super::open_db_at) owns the create/stamp/harden sequence
    /// and hands back a [`Store`]; this is the name a caller that already speaks
    /// in stores looks for, and it adds nothing else — the file-level policy
    /// keeps exactly one home.
    ///
    /// # Errors
    ///
    /// Returns the errors [`open_db_at`](super::open_db_at) documents: the file
    /// cannot be created or opened (corruption, permissions, lock contention), a
    /// foreign-owned parent directory, or a failed initial schema stamp.
    pub fn open_at(path: &Path) -> io::Result<Self> {
        super::open_db_at(path)
    }

    /// Build a store over an injectable storage backend.
    ///
    /// The only way to swap the filesystem out, which is how a latched handle is
    /// produced in a test: `test_backend::BreakableBackend` starts healthy and
    /// fails every later write once its flag is set, exactly as a full disk
    /// does.
    ///
    /// # Errors
    ///
    /// Returns Err if `redb` cannot create a database over `backend`.
    #[cfg(any(test, feature = "test-utils"))]
    pub fn with_backend(backend: impl redb::StorageBackend + 'static) -> io::Result<Self> {
        let db = redb::Database::builder()
            .create_with_backend(backend)
            .map_err(|e| io::Error::other(format!("failed to create the store: {e}")))?;
        Ok(Self::from_db(db))
    }

    /// Wrap an already-open `redb::Database`.
    ///
    /// The construction seam [`open_db_at`](super::open_db_at) uses (it owns the
    /// handle from the moment `redb` returns it), and how the crate's own tests
    /// build a store over a fixture database — at a path or over an in-memory
    /// backend — whose schema state they control exactly. Production code opens
    /// through [`open_db_at`](super::open_db_at), which owns the file-level
    /// policy; nothing else in the daemon constructs a store.
    pub(crate) fn from_db(db: redb::Database) -> Self {
        Self {
            db,
            escalation: OnceLock::new(),
        }
    }

    /// The underlying `redb::Database`.
    pub(crate) fn db(&self) -> &redb::Database {
        &self.db
    }

    /// The underlying `redb::Database`, mutably — compaction is the one caller,
    /// because `redb::Database::compact` takes `&mut self`.
    pub(crate) fn db_mut(&mut self) -> &mut redb::Database {
        &mut self.db
    }

    /// Install the escalation route: called once by the daemon assembly, with a
    /// notifier that asks the command loop to shut down.
    ///
    /// Takes `&self` because the store is shared through an `Arc` by the time the
    /// route exists. A second call is a programming error and is ignored with a
    /// warning rather than a panic: the first route is the one the running
    /// daemon wired its command channel into, and a mis-assembled startup must
    /// not take the process down over a duplicate.
    pub(crate) fn set_escalation(&self, notify: impl Fn() + Send + Sync + 'static) {
        let escalation = Escalation {
            escalated: AtomicBool::new(false),
            notify: Box::new(notify),
        };
        if self.escalation.set(escalation).is_err() {
            warn!("store escalation route is already installed; keeping the first one");
        }
    }

    /// Whether this handle has latched a permanent I/O failure.
    ///
    /// `try_begin_write` rather than `begin_write`, for two reasons: it reports
    /// the latch before it contends for the write slot, so the answer is exact
    /// even while another thread holds a write transaction, and it never blocks
    /// — which matters on an error path reached from a session thread.
    ///
    /// `PreviousIo` is unambiguous. A healthy handle yields `Ok` (dropped
    /// immediately, aborting an empty transaction) or `WouldBlock` while another
    /// writer is live, and neither matches.
    pub(crate) fn is_latched(&self) -> bool {
        matches!(
            self.db().try_begin_write(),
            Err(redb::TransactionError::Storage(
                redb::StorageError::PreviousIo
            ))
        )
    }

    /// Turn a storage failure's message into the caller's error, escalating if
    /// the handle behind it has latched.
    ///
    /// Every storage failure the `db` module reports is constructed here, which
    /// is what makes this the one place that can notice a `redb` handle latching
    /// itself unusable. Cheap when it has not: the probe is a flag read plus one
    /// atomic, and no route means no probe at all.
    pub(crate) fn err(&self, msg: String) -> io::Error {
        self.observe(&msg);
        io::Error::other(msg)
    }

    /// Escalate if this handle has latched, reporting `context` as the failure
    /// that exposed it.
    ///
    /// Idempotent: only the first observation escalates, so this is safe to call
    /// from every storage-failure path.
    fn observe(&self, context: &str) {
        let Some(escalation) = self.escalation.get() else {
            return;
        };
        if !self.is_latched() {
            return;
        }
        // The fence is claimed BEFORE the notifier runs, so a session writing in
        // a loop cannot emit one shutdown broadcast per write and bury the
        // clients it means to inform.
        if escalation.escalated.swap(true, Ordering::AcqRel) {
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
        (escalation.notify)();
    }
}

/// Transactions go through the deref, so the ~150 `store.begin_write()` /
/// `store.begin_read()` call sites read exactly as they did over a bare
/// `redb::Database`. Error construction must go through `Store::err` instead:
/// that is where a latch is observed, and building a `redb` failure into an
/// `io::Error` by hand would skip the detection entirely.
impl Deref for Store {
    type Target = redb::Database;

    fn deref(&self) -> &Self::Target {
        self.db()
    }
}

/// See the [`Deref`] impl: the mutable access exists for the one redb call that
/// needs it (`compact`, at startup, before the store is shared).
impl DerefMut for Store {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.db_mut()
    }
}

// Hand-written because the escalation route holds a `Box<dyn Fn>`: a derive
// cannot name it, and what matters to a caller that logs a store is whether a
// route is installed, not the closure.
impl std::fmt::Debug for Store {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Store")
            .field("db", &self.db)
            .field("escalation_installed", &self.escalation.get().is_some())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::AtomicUsize;

    use super::test_backend::BreakableBackend;
    use super::*;

    /// A store over a breakable backend, plus the flag that breaks it.
    fn breakable_store() -> (Store, Arc<AtomicBool>) {
        let (backend, failing) = BreakableBackend::new();
        let store = Store::with_backend(backend).expect("create the breakable store");
        (store, failing)
    }

    /// A counter the store's notifier bumps, so a test can count escalations.
    fn escalation_counter(store: &Store) -> Arc<AtomicUsize> {
        let escalations = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&escalations);
        store.set_escalation(move || {
            counter.fetch_add(1, Ordering::SeqCst);
        });
        escalations
    }

    /// A minimal persistable turn, so the test goes through `write_turn` — the
    /// turn-persistence path whose failure loses real work.
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
    /// later previous-IO error), the latch escalates exactly once, and the
    /// handle then probes as latched.
    ///
    /// The "disk full (test)" assertion is the regression guard for the retry
    /// loops this replaced: they discarded the first error and retried until the
    /// latch was visible, so the caller was told "a previous I/O error occurred"
    /// and the actual reason was lost.
    #[test]
    fn a_latched_store_escalates_once() {
        let (store, failing) = breakable_store();
        // A successful write first, so the failure below cannot be a creation
        // artefact.
        crate::db::write_turn(&store, 1, 0, &dummy_turn()).expect("healthy write");
        let escalations = escalation_counter(&store);
        assert!(
            !store.is_latched(),
            "a healthy handle must never probe as latched"
        );

        failing.store(true, Ordering::SeqCst);
        let first =
            crate::db::write_turn(&store, 1, 1, &dummy_turn()).expect_err("the write must fail");
        assert!(
            first.to_string().contains("disk full (test)"),
            "the caller must see the cause, not a masked previous-IO error: {first}"
        );
        assert_eq!(
            escalations.load(Ordering::SeqCst),
            1,
            "the first observation must escalate"
        );
        assert!(store.is_latched(), "the handle must now probe as latched");

        // Every later operation re-observes the same latch; only the first
        // escalates.
        crate::db::write_turn(&store, 1, 2, &dummy_turn()).expect_err("still failing");
        assert_eq!(
            escalations.load(Ordering::SeqCst),
            1,
            "escalation must be one-shot"
        );
    }

    /// A failure in one store cannot shut down the process through another: the
    /// probe follows the handle the notifier belongs to.
    #[test]
    fn an_unrelated_store_does_not_escalate() {
        let (healthy, _healthy_failing) = breakable_store();
        let (other, other_failing) = breakable_store();
        let escalations = escalation_counter(&healthy);

        other_failing.store(true, Ordering::SeqCst);
        crate::db::write_turn(&other, 1, 0, &dummy_turn()).expect_err("the other store must fail");

        assert!(other.is_latched(), "the failing store latched");
        assert!(
            !healthy.is_latched(),
            "the unrelated store is still healthy"
        );
        assert_eq!(
            escalations.load(Ordering::SeqCst),
            0,
            "an unrelated store's failure must not shut the daemon down"
        );
    }

    /// Two stores, each with its own notifier, both latch — and BOTH escalate,
    /// exactly once each. This is the property a process-global registry cannot
    /// hold: it tracks one handle, so the second store's failure is either
    /// misattributed to the first or silently dropped.
    #[test]
    fn escalation_is_per_store_not_per_process() {
        let (first, first_failing) = breakable_store();
        let (second, second_failing) = breakable_store();
        let first_escalations = escalation_counter(&first);
        let second_escalations = escalation_counter(&second);

        first_failing.store(true, Ordering::SeqCst);
        second_failing.store(true, Ordering::SeqCst);
        crate::db::write_turn(&first, 1, 0, &dummy_turn()).expect_err("the first store must fail");
        crate::db::write_turn(&second, 2, 0, &dummy_turn())
            .expect_err("the second store must fail");

        assert_eq!(
            first_escalations.load(Ordering::SeqCst),
            1,
            "the first store's own notifier must fire once"
        );
        assert_eq!(
            second_escalations.load(Ordering::SeqCst),
            1,
            "the second store's own notifier must fire once"
        );
        assert!(first.is_latched() && second.is_latched());
    }

    /// A data error is not a storage failure: a corrupt row must neither fire
    /// the notifier nor be read as a latched handle. The zstd codec builds its
    /// errors with `io::Error::other` for exactly this reason — routing them
    /// through `Store::err` would probe the database on every corrupt row and
    /// could escalate a perfectly healthy store.
    #[test]
    fn a_codec_error_does_not_escalate() {
        let (store, _failing) = breakable_store();
        let escalations = escalation_counter(&store);

        let err = crate::db::codec::zstd_decode(b"not a zstd frame")
            .expect_err("a corrupt blob must fail to decode");
        assert!(
            err.to_string().contains("zstd"),
            "the decode failure must describe itself: {err}"
        );
        assert_eq!(
            escalations.load(Ordering::SeqCst),
            0,
            "a corrupt row must not shut the daemon down"
        );
        assert!(
            !store.is_latched(),
            "a decode failure must not latch the handle"
        );
    }
}
