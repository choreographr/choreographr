//! Single-flight locks serializing OAuth refresh per credential.
//!
//! A refresh grant's token (the refresh token) is single-use on a rotating
//! provider: two concurrent refreshes of one credential can each spend it, and
//! the loser's request fails with a terminal `invalid_grant` even though the
//! grant is healthy. A caller therefore holds the per-credential lock this
//! registry returns for the whole `load → exchange → persist` sequence — the
//! provider refresh loop keys it by the account name and the MCP authorization
//! manager's `acquire_refresh_guard` keys it by the MCP credential key — so
//! only one exchange is ever in flight and a loser re-reads the winner's
//! persisted rotation instead of spending a stale token.
//!
//! One registry, one primitive, one lock per credential key. Both engines reach
//! the same [`OwnedRefreshLock`] — a one-token [`crossbeam_channel`] semaphore
//! whose owned guard returns the token on drop. The provider path is a blocking
//! session thread and blocks on [`OwnedRefreshLock::lock`]; the MCP adapter runs
//! on the async sidecar and takes the identical owned guard from a
//! `spawn_blocking` task, then holds it across the refresh `await`. A
//! `std::sync::MutexGuard` is `!Send` and could not cross that `await`, so the
//! owned semaphore guard is the one primitive that serves both engines.
//!
//! This is a process-global registry of per-key locks, in the spirit of the
//! daemon's other single-purpose shared-state exceptions: it carries no
//! protocol data, the map lock is held only for the brief get-or-insert, and
//! each per-key lock only for the refresh itself. One daemon serves one
//! keystore, so a process-local lock is sufficient — there is no cross-process
//! file lock to coordinate. Do **not** call it reentrantly on a key you already
//! hold — the per-key semaphore is not reentrant and would deadlock.
//!
//! The key is the credential key: the account name for provider grants, and
//! the target-scoped `mcp:<slug>|<target-hash>` string for MCP grants, so the
//! two engines share one registry with no key collision between them.

use std::collections::HashMap;
use std::sync::{Arc, LazyLock, Mutex, PoisonError};
use tracing::{debug, trace};

/// An owned single-flight lock over a one-token crossbeam semaphore.
///
/// `std::sync::MutexGuard` is `!Send`, so it cannot be handed to `rmcp`'s
/// refresh guard, which is held across an `await` on the sidecar runtime. A
/// bounded channel seeded with one token gives an owned guard whose `Drop`
/// returns the token: acquiring blocks on `recv`, releasing `try_send`s the
/// token back. The guard owns only a `Sender<()>`, so it is `Send + 'static`.
pub struct OwnedRefreshLock {
    /// The semaphore whose single token guards the credential.
    semaphore: ChannelSemaphore,
}

impl OwnedRefreshLock {
    /// Block until the credential's token is free, returning the owned guard.
    ///
    /// May be called from a plain thread — the provider refresh path — or from a
    /// `spawn_blocking` task (the MCP adapter); it must never be called directly
    /// inside an async task, where the blocking `recv` would stall the runtime
    /// worker.
    pub fn lock(self: &Arc<Self>) -> OwnedRefreshGuard {
        // A `recv` error means the registry (the sole sender owner) is gone;
        // treat that as acquired so a shutdown cannot wedge the guard.
        let _ = self.semaphore.rx.recv();
        OwnedRefreshGuard {
            release: self.semaphore.tx.clone(),
            // PIN the lock for the guard's whole life: `prune` retains an entry
            // only while another owner holds it (`strong_count > 1`), so without
            // this the registry could evict a lock that is *currently held* and
            // let a concurrent acquirer mint a second semaphore for the same
            // credential — breaking single-flight. The prune map check counts
            // this strong handle.
            _lock: Arc::clone(self),
        }
    }
}

/// The one-token crossbeam semaphore backing an [`OwnedRefreshLock`].
struct ChannelSemaphore {
    /// Returns a token on guard drop.
    tx: crossbeam_channel::Sender<()>,
    /// Consumes a token to acquire.
    rx: crossbeam_channel::Receiver<()>,
}

impl ChannelSemaphore {
    /// A semaphore holding exactly one token.
    fn new() -> Self {
        let (tx, rx) = crossbeam_channel::bounded(1);
        // Seed the single token; the channel is empty until a guard returns it.
        let _ = tx.try_send(());
        Self { tx, rx }
    }
}

/// The owned guard returned by [`OwnedRefreshLock::lock`]; returns the token on
/// drop, releasing the lock.
#[must_use = "dropping the guard releases the refresh lock"]
pub struct OwnedRefreshGuard {
    /// The semaphore sender the token is returned to on drop. Dropped first.
    release: crossbeam_channel::Sender<()>,
    /// A strong handle keeping the locked registry entry alive while the guard
    /// exists, so [`OAuthRefreshLocks::prune`] cannot evict a held lock. The
    /// leading underscore marks it as held-only (never read); dropped last.
    _lock: Arc<OwnedRefreshLock>,
}

impl Drop for OwnedRefreshGuard {
    fn drop(&mut self) {
        // `try_send` never blocks: the channel is empty at this point (the
        // token was consumed on acquire), and a full channel would only mean
        // the token was already returned.
        let _ = self.release.try_send(());
    }
}

/// A registry of per-credential OAuth refresh locks.
///
/// The map holds strong `Arc`s (not `Weak`): a weak handle would let two
/// acquirers that race the last release mint *separate* semaphores for one key
/// and fail to serialize. Entries are pruned once no other thread holds or
/// awaits them (see [`OAuthRefreshLocks::prune`]).
pub struct OAuthRefreshLocks {
    /// Credential key → the lock guarding a refresh, whichever engine runs it.
    map: Mutex<HashMap<String, Arc<OwnedRefreshLock>>>,
}

impl Default for OAuthRefreshLocks {
    fn default() -> Self {
        Self::new()
    }
}

impl OAuthRefreshLocks {
    /// Create an empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self {
            map: Mutex::new(HashMap::new()),
        }
    }

    /// Return the single-flight lock for `key`, inserting one if absent.
    ///
    /// The same key always yields the *same* `Arc`, so a second caller blocks
    /// on the first holder's lock rather than refreshing concurrently, and both
    /// engines — the provider's blocking path and the MCP adapter's
    /// `spawn_blocking` path — contend on that one semaphore.
    #[must_use]
    pub fn acquire(&self, key: &str) -> Arc<OwnedRefreshLock> {
        let mut map = self.map.lock().unwrap_or_else(PoisonError::into_inner);
        let lock = Arc::clone(map.entry(key.to_string()).or_insert_with(|| {
            Arc::new(OwnedRefreshLock {
                semaphore: ChannelSemaphore::new(),
            })
        }));
        trace!(key, "acquired OAuth refresh lock");
        lock
    }

    /// Drop entries no thread currently holds or awaits.
    ///
    /// Called after a refresh completes so the registry does not grow one entry
    /// per credential ever refreshed. An entry is retained while any other
    /// owner exists — the map plus a live holder (or waiter) make
    /// `strong_count` greater than one — and removed only when the map is its
    /// sole owner. The check runs under the map lock so it cannot race a
    /// concurrent reserve.
    pub fn prune(&self) {
        let removed = prune_map(&self.map);
        if removed > 0 {
            debug!(removed, "pruned idle OAuth refresh locks");
        }
    }
}

/// Retain only the entries another owner still holds or awaits; return the
/// count removed.
fn prune_map<T: Send + Sync + 'static>(map: &Mutex<HashMap<String, Arc<T>>>) -> usize {
    let mut guard = map.lock().unwrap_or_else(PoisonError::into_inner);
    let before = guard.len();
    guard.retain(|_, lock| Arc::strong_count(lock) > 1);
    before - guard.len()
}

/// The process-global registry the provider and MCP refresh engines share.
static OAUTH_REFRESH_LOCKS: LazyLock<OAuthRefreshLocks> = LazyLock::new(OAuthRefreshLocks::new);

/// Return the process-global single-flight refresh lock for `key`.
///
/// See [`OAuthRefreshLocks::acquire`]; this is the convenience entry point over
/// the shared registry.
#[must_use]
pub fn acquire(key: &str) -> Arc<OwnedRefreshLock> {
    OAUTH_REFRESH_LOCKS.acquire(key)
}

/// Prune idle entries from the process-global refresh-lock registry.
///
/// See [`OAuthRefreshLocks::prune`].
pub fn prune() {
    OAUTH_REFRESH_LOCKS.prune();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn same_key_yields_the_same_lock() {
        let registry = OAuthRefreshLocks::new();
        let first = registry.acquire("account-a");
        let second = registry.acquire("account-a");
        assert!(
            Arc::ptr_eq(&first, &second),
            "a key must map to one shared lock so refreshes serialize"
        );
    }

    #[test]
    fn different_keys_yield_distinct_locks() {
        let registry = OAuthRefreshLocks::new();
        let a = registry.acquire("account-a");
        let b = registry.acquire("mcp:github|deadbeef");
        assert!(!Arc::ptr_eq(&a, &b));
    }

    #[test]
    fn prune_keeps_a_currently_held_lock() {
        let registry = OAuthRefreshLocks::new();
        // Hold the lock, then prune: a live holder must keep its entry so a
        // racing acquirer still contends on the same mutex.
        let held = registry.acquire("account-a");
        registry.prune();
        let again = registry.acquire("account-a");
        assert!(
            Arc::ptr_eq(&held, &again),
            "prune must not drop an entry another thread still holds"
        );
    }

    #[test]
    fn prune_drops_an_idle_entry() {
        let registry = OAuthRefreshLocks::new();
        let first = registry.acquire("account-a");
        // Drop the only external owner; the map is now the sole holder.
        drop(first);
        // With the map as the sole owner a prune removes the entry outright, so
        // the registry cannot grow one lock per credential ever refreshed. The
        // map contents are asserted directly because comparing `Arc` addresses
        // before and after is allocator-dependent (a fresh allocation may reuse
        // the freed address).
        registry.prune();
        let map = registry.map.lock().unwrap_or_else(PoisonError::into_inner);
        assert!(
            !map.contains_key("account-a"),
            "an idle entry must be pruned so the registry cannot grow unbounded"
        );
    }

    #[test]
    fn global_acquire_returns_a_lock() {
        // The process-global entry point must return a lock, and the same key
        // must be stable across calls.
        let first = acquire("global-account");
        let second = acquire("global-account");
        assert!(Arc::ptr_eq(&first, &second));
        // Keep both alive past the prune so a concurrent test's prune cannot
        // drop this key mid-assert.
        prune();
    }

    #[test]
    fn one_key_yields_one_lock_for_both_call_styles() {
        // The provider path takes the lock and blocks on its semaphore...
        let registry = OAuthRefreshLocks::new();
        let provider_style = registry.acquire("shared-key");
        let provider_guard = provider_style.lock();
        drop(provider_guard);
        // ...and the MCP path takes the *same* `Arc` and the identical owned
        // guard, so the two engines serialize against one another.
        let mcp_style = registry.acquire("shared-key");
        assert!(
            Arc::ptr_eq(&provider_style, &mcp_style),
            "one key must map to one lock whichever engine acquires it"
        );
        let mcp_guard = mcp_style.lock();
        assert!(Arc::strong_count(&mcp_style) > 1);
        drop(mcp_guard);
    }

    #[test]
    fn a_held_guard_makes_the_next_acquirer_block_on_the_same_lock() {
        let registry = OAuthRefreshLocks::new();
        let first = registry.acquire("single-flight-key");
        let guard = first.lock();

        // A second acquirer gets the SAME `Arc`, so it contends rather than
        // minting a fresh lock.
        let second = registry.acquire("single-flight-key");
        assert!(
            Arc::ptr_eq(&first, &second),
            "a second acquirer must contend on the same lock, not mint a new one"
        );

        // The held guard consumed the semaphore's only token, so any other
        // `lock()` on this `Arc` blocks. This is a deterministic (no-sleep)
        // probe of single-flight: an empty channel is exactly what makes the
        // second `recv` wait.
        assert!(
            first.semaphore.rx.try_recv().is_err(),
            "a held guard must leave the semaphore empty so a second lock() blocks"
        );

        // The held guard also pins the entry (map + `first` + `second` + the
        // guard's strong handle), so a concurrent prune cannot evict a held lock
        // and let a racing acquirer mint a second semaphore.
        assert!(
            Arc::strong_count(&first) >= 3,
            "the held guard must pin its entry against a concurrent prune"
        );

        // Releasing returns the token: probing consumes it, which is exactly the
        // proof that the next `lock()` would succeed rather than block.
        drop(guard);
        assert!(
            first.semaphore.rx.try_recv().is_ok(),
            "dropping the guard must return the token so the next acquirer proceeds"
        );
    }

    #[test]
    fn owned_guard_is_send_and_static() {
        fn assert_send_static<T: Send + 'static>(_: T) {}
        let registry = OAuthRefreshLocks::new();
        let lock = registry.acquire("k");
        let guard = lock.lock();
        assert_send_static(guard);
    }

    #[test]
    fn prune_keeps_an_entry_with_a_live_owned_guard() {
        // Regression: the owned guard must PIN its entry even after the caller's
        // Arc is dropped, so a concurrent prune cannot evict a HELD lock and let
        // a racing acquirer mint a second semaphore for the same credential.
        let registry = OAuthRefreshLocks::new();
        let lock = registry.acquire("mcp:docs|pin");
        let held_ptr = Arc::as_ptr(&lock);
        let guard = lock.lock();
        // Drop the caller's handle: only the held guard keeps the entry alive.
        drop(lock);
        registry.prune();
        let while_held = registry.acquire("mcp:docs|pin");
        assert_eq!(
            Arc::as_ptr(&while_held),
            held_ptr,
            "a held guard must keep its entry alive across a prune"
        );

        // Once every owner is gone the entry is pruned, so the registry cannot
        // grow one entry per credential ever refreshed. As above, the map
        // contents are asserted directly rather than comparing `Arc` addresses
        // across the free boundary.
        drop(while_held);
        drop(guard);
        registry.prune();
        assert!(
            !registry
                .map
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .contains_key("mcp:docs|pin"),
            "an idle entry is pruned once released"
        );
    }
}
