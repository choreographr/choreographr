use lru::LruCache;
use std::num::NonZeroUsize;
use std::sync::{Mutex, OnceLock};

/// A thread-safe, lazily-initialized LRU cache.
///
/// Uses a `OnceLock` for one-time initialization and a `Mutex` for
/// concurrent access.  The capacity `N` is a compile-time constant
/// specified via a const generic parameter.
///
/// The `get_or_insert_with` method holds the lock while the factory
/// closure runs.  For the TUI's single-threaded rendering this is
/// fine — there is never concurrent cache access.
pub struct GlobalLruCache<K, V, const N: usize> {
    inner: OnceLock<Mutex<LruCache<K, V>>>,
}

impl<K, V, const N: usize> GlobalLruCache<K, V, N>
where
    K: Eq + std::hash::Hash + Clone,
    V: Clone,
{
    pub const fn new() -> Self {
        Self {
            inner: OnceLock::new(),
        }
    }

    fn get_or_init(&self) -> &Mutex<LruCache<K, V>> {
        self.inner.get_or_init(|| {
            // Clamp to at least 1 so NonZeroUsize never fails —
            // 0 is a nonsensical capacity anyway.
            let cap = NonZeroUsize::new(N).unwrap_or(NonZeroUsize::MIN);
            Mutex::new(LruCache::new(cap))
        })
    }

    pub fn get_or_insert_with<F>(&self, key: &K, f: F) -> V
    where
        F: FnOnce() -> V,
    {
        let mut cache = self.get_or_init().lock().unwrap_or_else(|e| e.into_inner());
        if let Some(value) = cache.get(key) {
            return value.clone();
        }
        let value = f();
        cache.put(key.clone(), value.clone());
        value
    }
}
