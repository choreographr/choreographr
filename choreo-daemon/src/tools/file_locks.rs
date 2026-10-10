//! Per-file mutation locks.
//!
//! Each turn's tool calls are dispatched **concurrently** — every non-config
//! tool (`read_file`, `write_file`, `edit_file`, `delete_files`, …) runs on its
//! own thread (see `requests.rs`, "Phase 2: All remaining tools (concurrent)").
//! That means two mutations of the *same* file in one batch can interleave: the
//! read-modify-write in `edit_file` (`read_to_string` → match → atomic write)
//! lets both calls read the original and one silently lose its update.
//!
//! [`with_file_lock`] serializes mutations by canonical path so two writers of
//! one file run one at a time, while writers of different files stay parallel.
//! This mirrors pi's `withFileMutationQueue`. It is deliberately *not* applied
//! to readers (`read_file`, `grep`, …): the write is an atomic rename, so a
//! reader always sees either the old or the new file, never a torn one.
//!
//! This is a process-global registry of locks, in the spirit of the daemon's
//! other single-purpose shared-state exceptions: it carries no protocol data,
//! the map lock is held only for the brief map operations (never across the
//! mutation), and each per-path `Mutex` is held only for the mutation itself.
//! Do **not** call it reentrantly on a path you already hold — the per-path
//! `Mutex` is not reentrant and would deadlock.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex};

/// Registry of per-canonical-path mutation locks.
struct FileLocks {
    /// Canonical path → the lock guarding mutations of that file. Values are
    /// strong `Arc`s (not `Weak`): a weak handle would let two acquirers that
    /// race the last release mint *separate* mutexes for one path and fail to
    /// serialize. Entries are pruned on release once no other thread holds or
    /// awaits them (see `with_many`).
    map: Mutex<HashMap<PathBuf, Arc<Mutex<()>>>>,
}

impl FileLocks {
    fn new() -> Self {
        Self {
            map: Mutex::new(HashMap::new()),
        }
    }

    /// Run `f` holding the lock for each key in `keys` (already canonicalized,
    /// deduped, and sorted). Acquiring in a single sorted order is what makes
    /// two overlapping multi-path callers deadlock-free.
    fn with_many<R>(&self, keys: &[PathBuf], f: impl FnOnce() -> R) -> R {
        // Reserve one lock handle per key under the map lock (brief), so every
        // concurrent acquirer of a key shares the *same* `Mutex`.
        let handles: Vec<Arc<Mutex<()>>> = {
            let mut map = self
                .map
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            keys.iter()
                .map(|key| Arc::clone(map.entry(key.clone()).or_default()))
                .collect()
        };

        // Take the per-key locks in `keys` order; hold them across `f`.
        let guards: Vec<_> = handles
            .iter()
            .map(|handle| {
                handle
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
            })
            .collect();
        let out = f();
        drop(guards);

        // Prune entries no other thread holds or awaits: the map plus `handles`
        // are then the only owners (`strong_count == 2`). Doing the check under
        // the map lock closes the race with a concurrent reserve.
        {
            let mut map = self
                .map
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            for key in keys {
                if map
                    .get(key)
                    .is_some_and(|entry| Arc::strong_count(entry) == 2)
                {
                    map.remove(key);
                }
            }
        }
        drop(handles);

        out
    }
}

static FILE_LOCKS: LazyLock<FileLocks> = LazyLock::new(FileLocks::new);

/// The lock key for a resolved path: its canonical path when the target exists
/// (so two symlinks to one file share a lock), else the lexically-absolutized
/// path (so two concurrent *creates* of the same new file still serialize).
fn mutation_key(resolved: &Path) -> PathBuf {
    std::fs::canonicalize(resolved).unwrap_or_else(|_| absolute(resolved))
}

/// Absolutize `path` against the process working directory without touching the
/// filesystem (used when the target does not exist yet, so `canonicalize` fails).
fn absolute(path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir().map_or_else(|_| path.to_path_buf(), |dir| dir.join(path))
    }
}

/// Serialize a read-modify-write on `path` against any other concurrent
/// mutation of the same file (other sessions included).
pub(crate) fn with_file_lock<R>(path: &Path, f: impl FnOnce() -> R) -> R {
    let key = mutation_key(path);
    FILE_LOCKS.with_many(std::slice::from_ref(&key), f)
}

/// Serialize mutations touching any of `paths`. `paths` need not be sorted: the
/// keys are canonicalized, deduped, and sorted internally, so two calls whose
/// path sets overlap cannot deadlock.
pub(crate) fn with_file_locks<R>(paths: &[PathBuf], f: impl FnOnce() -> R) -> R {
    let mut keys: Vec<PathBuf> = paths.iter().map(|path| mutation_key(path)).collect();
    keys.sort();
    keys.dedup();
    FILE_LOCKS.with_many(&keys, f)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Barrier;
    use std::sync::mpsc;

    #[test]
    fn same_path_mutations_serialize_without_lost_updates() {
        // A read-modify-write increment under the lock must not lose updates
        // when many threads race the same path. Without the lock, concurrent
        // read-then-write strands increments; with it, every one lands.
        const THREADS: u32 = 16;
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("counter.txt");
        std::fs::write(&path, "0").unwrap();

        // Align the threads so they contend as tightly as possible.
        let barrier = Arc::new(Barrier::new(THREADS as usize));
        let handles: Vec<_> = (0..THREADS)
            .map(|_| {
                let path = path.clone();
                let barrier = Arc::clone(&barrier);
                std::thread::spawn(move || {
                    barrier.wait();
                    with_file_lock(&path, || {
                        let value: u32 = std::fs::read_to_string(&path)
                            .unwrap()
                            .trim()
                            .parse()
                            .unwrap();
                        std::fs::write(&path, (value + 1).to_string()).unwrap();
                    });
                })
            })
            .collect();
        for handle in handles {
            handle.join().unwrap();
        }

        let final_value: u32 = std::fs::read_to_string(&path)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        assert_eq!(
            final_value, THREADS,
            "lost updates: {final_value} != {THREADS}"
        );
    }

    #[test]
    fn different_paths_do_not_block_each_other() {
        let dir = tempfile::TempDir::new().unwrap();
        let a = dir.path().join("a.txt");
        let b = dir.path().join("b.txt");
        std::fs::write(&a, "").unwrap();
        std::fs::write(&b, "").unwrap();

        // Park a thread holding A's lock until released.
        let (a_held_tx, a_held_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel::<()>();
        let a_for_holder = a.clone();
        let holder = std::thread::spawn(move || {
            with_file_lock(&a_for_holder, || {
                a_held_tx.send(()).unwrap();
                release_rx.recv().unwrap();
            });
        });
        a_held_rx.recv().unwrap(); // A is now held.

        // Acquiring B must complete *while* A is held (distinct keys). A plain
        // blocking `recv` keeps this deterministic — no timers.
        let (b_done_tx, b_done_rx) = mpsc::channel();
        let b_for_other = b.clone();
        let other = std::thread::spawn(move || {
            with_file_lock(&b_for_other, || {
                b_done_tx.send(()).unwrap();
            });
        });
        b_done_rx
            .recv()
            .expect("acquiring a different path must not block on a held path");

        release_tx.send(()).unwrap();
        holder.join().unwrap();
        other.join().unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn mutation_key_resolves_symlinks_to_one_key() {
        let dir = tempfile::TempDir::new().unwrap();
        let target = dir.path().join("real.txt");
        std::fs::write(&target, "x").unwrap();
        let link = dir.path().join("link.txt");
        std::os::unix::fs::symlink(&target, &link).unwrap();

        assert_eq!(mutation_key(&link), mutation_key(&target));
    }

    #[test]
    fn mutation_key_falls_back_for_missing_path() {
        // A path that does not exist yet (a create) still yields a stable,
        // absolute key so two concurrent creates of it share a lock.
        let dir = tempfile::TempDir::new().unwrap();
        let missing = dir.path().join("new.txt");
        let key = mutation_key(&missing);
        assert!(key.is_absolute(), "key must be absolute: {key:?}");
        assert_eq!(key, mutation_key(&missing));
    }

    #[test]
    fn with_file_locks_dedups_and_sorts_without_deadlock() {
        // Two overlapping multi-path callers must not deadlock: keys are sorted
        // internally so acquisition order is global. Send the two sets from two
        // threads and assert both complete.
        let dir = tempfile::TempDir::new().unwrap();
        let files: Vec<PathBuf> = (0..3)
            .map(|i| {
                let p = dir.path().join(format!("f{i}.txt"));
                std::fs::write(&p, "").unwrap();
                p
            })
            .collect();

        let first = files.clone();
        let second = files.iter().rev().cloned().collect::<Vec<_>>();
        let (tx, rx) = mpsc::channel();
        let tx2 = tx.clone();
        let t1 = std::thread::spawn(move || {
            with_file_locks(&first, || tx.send(1u8).unwrap());
        });
        let t2 = std::thread::spawn(move || {
            with_file_locks(&second, || tx2.send(2u8).unwrap());
        });
        // Both completions must arrive (blocking recv; a deadlock would hang
        // and be caught by the harness, not by a timer).
        let mut seen = [rx.recv().unwrap(), rx.recv().unwrap()];
        seen.sort_unstable();
        assert_eq!(seen, [1, 2]);
        t1.join().unwrap();
        t2.join().unwrap();
    }
}
