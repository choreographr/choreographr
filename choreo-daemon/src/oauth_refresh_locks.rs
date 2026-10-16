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
//! This is a process-global registry of per-key locks, in the spirit of the
//! daemon's other single-purpose shared-state exceptions: it carries no
//! protocol data, the map lock is held only for the brief get-or-insert, and
//! each per-key `Mutex` is held only for the refresh itself. One daemon serves
//! one keystore, so a process-local lock is sufficient — there is no
//! cross-process file lock to coordinate. Do **not** call it reentrantly on a
//! key you already hold — the per-key `Mutex` is not reentrant and would
//! deadlock.
//!
//! The key is the credential key: the account name for provider grants, and
//! the target-scoped `mcp:<slug>|<target-hash>` string for MCP grants, so the
//! two engines share one registry with no key collision between them.

use std::collections::HashMap;
use std::sync::{Arc, LazyLock, Mutex, PoisonError};
use tracing::{debug, trace};

/// A registry of per-credential OAuth refresh locks.
///
/// The map holds strong `Arc`s (not `Weak`): a weak handle would let two
/// acquirers that race the last release mint *separate* mutexes for one key and
/// fail to serialize. Entries are pruned once no other thread holds or awaits
/// them (see [`OAuthRefreshLocks::prune`]).
pub struct OAuthRefreshLocks {
    /// Credential key → the lock guarding refresh of that credential.
    map: Mutex<HashMap<String, Arc<Mutex<()>>>>,
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
    /// on the first holder's lock rather than refreshing concurrently.
    #[must_use]
    pub fn acquire(&self, key: &str) -> Arc<Mutex<()>> {
        let mut map = self.map.lock().unwrap_or_else(PoisonError::into_inner);
        // `or_default` mints an unlocked `Mutex<()>` on first use; `Arc::clone`
        // shares it so every acquirer of this key contends on one lock.
        let lock = Arc::clone(map.entry(key.to_string()).or_default());
        trace!(key, "acquired OAuth refresh lock");
        lock
    }

    /// Drop entries no thread currently holds or awaits.
    ///
    /// Called after a refresh completes so the map does not grow one entry per
    /// credential ever refreshed. An entry is retained while any other owner
    /// exists — the map plus a live holder (or waiter) make `strong_count`
    /// greater than one — and removed only when the map is its sole owner. The
    /// check runs under the map lock so it cannot race a concurrent reserve.
    pub fn prune(&self) {
        let mut map = self.map.lock().unwrap_or_else(PoisonError::into_inner);
        let before = map.len();
        map.retain(|_, lock| Arc::strong_count(lock) > 1);
        let removed = before - map.len();
        if removed > 0 {
            debug!(
                removed,
                remaining = map.len(),
                "pruned idle OAuth refresh locks"
            );
        }
    }
}

/// The process-global registry the provider and MCP refresh engines share.
static OAUTH_REFRESH_LOCKS: LazyLock<OAuthRefreshLocks> = LazyLock::new(OAuthRefreshLocks::new);

/// Return the process-global single-flight refresh lock for `key`.
///
/// See [`OAuthRefreshLocks::acquire`]; this is the convenience entry point over
/// the shared registry.
#[must_use]
pub fn acquire(key: &str) -> Arc<Mutex<()>> {
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
        let first_ptr = Arc::as_ptr(&first);
        // Drop the only external owner; the map is now the sole holder.
        drop(first);
        registry.prune();
        let second = registry.acquire("account-a");
        assert_ne!(
            first_ptr,
            Arc::as_ptr(&second),
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
}
