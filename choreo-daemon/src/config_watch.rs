//! Unified config-file watching transport.
//!
//! The daemon has (and will keep adding) config files under one directory —
//! `$XDG_CONFIG_HOME/choreographr` — that must be hot-reloaded when the user
//! edits them (`models-overlay.toml`, `accounts.toml`, and future files). All
//! of them live in the SAME directory, and the `notify` crate watches
//! directories, not files: watching N files in one directory means N nearly
//! identical watchers that each re-implement dir-creation, event filtering,
//! re-arming, and channel forwarding. This module is the ONE such transport —
//! a single background thread owns the `notify` watcher on the config
//! directory and fans normalized, per-basename events out to registered
//! consumers over their own crossbeam channels.
//!
//! **Transport only, no policy.** The transport does not read files, apply
//! fingerprints, or mutate any daemon state. Consumers subscribe to the
//! basenames they care about and own all policy — re-read, fingerprint gate,
//! and the reload *command* sent to the daemon command loop (the single
//! writer of whatever they touch). This keeps the transport format-agnostic
//! (TOML vs JSON never leaks in) and keeps each domain's reload policy
//! colocated with the state it governs.
//!
//! **Threading.** This is a channel-only design: the transport thread only
//! sends `ConfigChange`s to subscriber channels (it never reads a config
//! file or mutates shared state), and consumers forward reload requests over
//! their own channels to the daemon command loop. No shared state crosses
//! threads.

use std::collections::HashMap;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crossbeam_channel::{Receiver, Sender};
use notify::{RecursiveMode, Watcher};
use tracing::{debug, info, warn};

/// A normalized filesystem change the transport surfaced for one watched
/// basename. Consumers re-read the file at their own known path and apply
/// their own fingerprint gate; this is only the "something happened here"
/// signal.
#[derive(Debug, Clone)]
pub struct ConfigChange {
    /// The watched file's full path (the config dir joined with its basename).
    pub path: PathBuf,
    /// Coarse-grained change kind (already stripped of `Access`/`Other`
    /// noise — the transport only surfaces create/modify/remove).
    pub kind: ChangeKind,
}

/// The change kinds the transport surfaces. Coarser than `notify`'s full
/// `EventKind` taxonomy because consumers only care whether a file was
/// created, written, or removed — never about `Access` reports or `Other`
/// noise.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChangeKind {
    Create,
    Modify,
    Remove,
}

/// How long the transport waits between re-arm attempts when the config
/// directory is not (yet) watchable. A short fixed cadence so a directory
/// created after startup is picked up promptly, without spamming warn logs
/// (re-arm retries log at debug while unarmed).
const REARM_INTERVAL: Duration = Duration::from_secs(5);

/// A handle to the shared config watcher. Build it, subscribe the basenames
/// each consumer cares about, then `spawn()` it. Registration happens before
/// spawn (the subscriber map is captured into the transport thread), which
/// keeps the transport thread's job trivial: it only routes events it already
/// knows about.
pub struct ConfigWatcher {
    dir: PathBuf,
    subscribers: HashMap<PathBuf, Vec<Sender<ConfigChange>>>,
}

impl ConfigWatcher {
    /// A watcher over `dir`. The directory is created (log-only on failure)
    /// by the transport thread when it spawns, so it does not need to exist
    /// yet.
    pub fn new(dir: PathBuf) -> Self {
        Self {
            dir,
            subscribers: HashMap::new(),
        }
    }

    /// Register interest in a basename (e.g. `"accounts.toml"`) and return a
    /// receiver that gets every create/modify/remove of that file. Multiple
    /// consumers may subscribe to the same basename; each gets its own copy.
    pub fn subscribe(&mut self, basename: &str) -> Receiver<ConfigChange> {
        let (tx, rx) = crossbeam_channel::unbounded();
        self.subscribers
            .entry(PathBuf::from(basename))
            .or_default()
            .push(tx);
        rx
    }

    /// Start the background transport thread. The thread is detached (it
    /// lives until the process exits, exactly like the catalog-maintenance
    /// thread) and owns the `notify` watcher; dropping the handle does not
    /// stop it.
    pub fn spawn(self) {
        let _ = std::thread::Builder::new()
            .name("config-watch".into())
            .spawn(move || transport_loop(self.subscribers, self.dir));
    }
}

/// Map a raw `notify` event kind onto our coarse [`ChangeKind`]. Returns
/// `None` for pure `Access`/`Other` noise (e.g. macOS FSEvents access
/// reports) that must never trigger a reload.
fn classify(kind: &notify::EventKind) -> Option<ChangeKind> {
    match kind {
        notify::EventKind::Create(_) => Some(ChangeKind::Create),
        notify::EventKind::Modify(_) => Some(ChangeKind::Modify),
        notify::EventKind::Remove(_) => Some(ChangeKind::Remove),
        _ => None,
    }
}

/// Decide which registered basenames a raw event should wake, and with what
/// kind. The watch is on the config *directory* (rename-safe: an editor that
/// writes temp + rename fires events for the directory), so unrelated files'
/// events arrive too — this filters by the basenames present in the event's
/// paths against the subscriber map. Pure and unit-testable.
fn route(
    subscribers: &HashMap<PathBuf, Vec<Sender<ConfigChange>>>,
    event: &notify::Event,
) -> Vec<(PathBuf, ChangeKind)> {
    let Some(kind) = classify(&event.kind) else {
        return Vec::new();
    };
    // The basenames named in this event (an event can carry several paths).
    let changed: Vec<&OsStr> = event.paths.iter().filter_map(|p| p.file_name()).collect();
    subscribers
        .keys()
        .filter(|b| changed.iter().any(|name| *name == b.as_os_str()))
        .cloned()
        .map(|b| (b, kind))
        .collect()
}

/// Whether a raw event signals that the watched directory itself was removed
/// (deleted or moved away at runtime). The watch on a removed directory is
/// dead; detecting this lets the transport drop `armed` and re-arm once the
/// directory is recreated, instead of never firing again.
fn dir_was_removed(dir: &Path, event: &notify::Event) -> bool {
    matches!(event.kind, notify::EventKind::Remove(_)) && event.paths.iter().any(|p| p == dir)
}

/// Deliver one change to every sender subscribed to `basename`. A dead
/// receiver is dropped silently — the consumer went away, its interest is
/// moot. `try_send` never blocks: subscriber channels are unbounded, so a
/// slow consumer cannot stall the transport thread.
fn deliver(
    subscribers: &HashMap<PathBuf, Vec<Sender<ConfigChange>>>,
    dir: &Path,
    basename: &Path,
    kind: ChangeKind,
) {
    if let Some(senders) = subscribers.get(basename) {
        let change = ConfigChange {
            path: dir.join(basename),
            kind,
        };
        for tx in senders {
            let _ = tx.try_send(change.clone());
        }
    }
}

/// Ensure the watched config directory exists before installing the watch.
/// `notify` cannot watch a directory that does not exist, and nothing creates
/// this dir until the user writes a config file — so on a fresh system the
/// initial watch would fail and only be picked up later by the re-arm. A
/// creation failure is logged, never fatal: the re-arm cadence retries, and
/// `try_send` drops events harmlessly in the meantime.
fn ensure_config_dir(dir: &Path) {
    match std::fs::create_dir_all(dir) {
        Ok(()) => debug!(dir = %dir.display(), "config dir ready"),
        Err(e) => warn!(
            dir = %dir.display(),
            error = %e,
            "failed to create the config dir; config-file auto-reload may be unavailable",
        ),
    }
}

/// The transport thread: owns the `notify` watcher, ensures the dir, and
/// routes every raw event to the matching subscribers. Re-arms the watch on a
/// fixed cadence while it is unarmed (a dir deleted at runtime, or a
/// creation failure above that has since been fixed).
fn transport_loop(subscribers: HashMap<PathBuf, Vec<Sender<ConfigChange>>>, dir: PathBuf) {
    ensure_config_dir(&dir);

    // The notify callback forwards raw events to a channel — all routing
    // policy lives on this thread, keeping the notify-owned callback thread
    // trivially small (mirrors the catalog maintenance thread's discipline).
    let (raw_tx, raw_rx) = crossbeam_channel::unbounded::<Result<notify::Event, notify::Error>>();
    let mut watcher: Option<notify::RecommendedWatcher> =
        match notify::recommended_watcher(move |res: Result<notify::Event, notify::Error>| {
            let _ = raw_tx.send(res);
        }) {
            Ok(w) => Some(w),
            Err(e) => {
                warn!(
                    error = %e,
                    "failed to create the filesystem watcher; config-file changes \
                     will not reload automatically",
                );
                None
            }
        };

    let mut armed = false;
    loop {
        // Last-resort re-arm while unarmed (a dir deleted at runtime, or a
        // dir whose creation failed at spawn). Debug, not warn: this retries
        // on a fixed cadence until it succeeds, so a warn would spam the log.
        if !armed && let Some(w) = watcher.as_mut() {
            match w.watch(&dir, RecursiveMode::NonRecursive) {
                Ok(()) => {
                    armed = true;
                    info!(
                        dir = %dir.display(),
                        "config directory is now watchable; auto-reload armed",
                    );
                }
                Err(e) => {
                    tracing::debug!(
                        dir = %dir.display(),
                        error = %e,
                        "config directory still not watchable; will retry",
                    );
                }
            }
        }

        // Block on raw events when armed; while unarmed use a short recv
        // timeout so the re-arm retry cadence paces without burning CPU. The
        // two recv calls have different error types (RecvError vs
        // RecvTimeoutError), so they are handled in separate arms that both
        // fall through to the same routing below.
        let raw = if armed {
            match raw_rx.recv() {
                Ok(raw) => raw,
                Err(_) => {
                    info!("config watch raw channel closed; exiting");
                    break;
                }
            }
        } else {
            match raw_rx.recv_timeout(REARM_INTERVAL) {
                Ok(raw) => raw,
                Err(crossbeam_channel::RecvTimeoutError::Timeout) => {
                    // Re-arm retry cadence; loop to attempt the watch again.
                    continue;
                }
                Err(crossbeam_channel::RecvTimeoutError::Disconnected) => {
                    info!("config watch raw channel closed; exiting");
                    break;
                }
            }
        };
        match raw {
            Ok(event) => {
                // If the watched directory itself was removed (deleted/moved at
                // runtime), the inotify/kqueue watch is now dead — drop `armed`
                // so the loop re-arms once the directory comes back, instead of
                // sitting on a stale watch that never fires again.
                if armed && dir_was_removed(&dir, &event) {
                    armed = false;
                    info!(
                        dir = %dir.display(),
                        "config directory removed; watch will re-arm on the next retry",
                    );
                }
                for (basename, kind) in route(&subscribers, &event) {
                    deliver(&subscribers, &dir, &basename, kind);
                }
            }
            Err(e) => warn!(error = %e, "config watcher error"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(path: &Path, kind: notify::EventKind) -> notify::Event {
        notify::Event {
            kind,
            paths: vec![path.to_path_buf()],
            attrs: notify::event::EventAttributes::default(),
        }
    }

    #[test]
    fn classify_strips_access_and_other_noise() {
        // Pure Access/Other events must never surface a reload.
        assert_eq!(
            classify(&notify::EventKind::Access(notify::event::AccessKind::Read)),
            None
        );
        assert_eq!(classify(&notify::EventKind::Other), None);
        // Create/Modify/Remove map onto the coarse kinds.
        assert_eq!(
            classify(&notify::EventKind::Create(notify::event::CreateKind::File)),
            Some(ChangeKind::Create)
        );
        assert_eq!(
            classify(&notify::EventKind::Modify(notify::event::ModifyKind::Data(
                notify::event::DataChange::Any
            ))),
            Some(ChangeKind::Modify)
        );
        assert_eq!(
            classify(&notify::EventKind::Remove(notify::event::RemoveKind::File)),
            Some(ChangeKind::Remove)
        );
    }

    #[test]
    fn dir_was_removed_detects_only_the_watched_directory() {
        let dir = Path::new("/cfg");
        // A Remove event whose path IS the watched dir signals the dir is gone.
        assert!(dir_was_removed(
            dir,
            &event(
                dir,
                notify::EventKind::Remove(notify::event::RemoveKind::Folder)
            )
        ));
        // Removing a file *inside* the dir is not the dir going away.
        assert!(!dir_was_removed(
            dir,
            &event(
                &dir.join("accounts.toml"),
                notify::EventKind::Remove(notify::event::RemoveKind::File)
            )
        ));
        // A non-Remove event (e.g. Modify) is never a dir removal.
        assert!(!dir_was_removed(
            dir,
            &event(
                dir,
                notify::EventKind::Modify(notify::event::ModifyKind::Name(
                    notify::event::RenameMode::To
                ))
            )
        ));
    }

    #[test]
    fn route_matches_by_basename_only() {
        let overlay = PathBuf::from("models-overlay.toml");
        let accounts = PathBuf::from("accounts.toml");
        let mut subs: HashMap<PathBuf, Vec<Sender<ConfigChange>>> = HashMap::new();
        let (tx, _rx) = crossbeam_channel::unbounded();
        subs.insert(overlay.clone(), vec![tx]);
        let (tx2, _rx2) = crossbeam_channel::unbounded();
        subs.insert(accounts.clone(), vec![tx2]);

        // Our file: Create/Modify/Remove all route.
        for kind in [
            notify::EventKind::Create(notify::event::CreateKind::File),
            notify::EventKind::Modify(notify::event::ModifyKind::Data(
                notify::event::DataChange::Any,
            )),
            notify::EventKind::Remove(notify::event::RemoveKind::File),
        ] {
            let routed = route(&subs, &event(Path::new("/cfg/models-overlay.toml"), kind));
            assert_eq!(
                routed,
                vec![(overlay.clone(), ChangeKind::from_kind(&kind))]
            );
        }

        // A different file in the same directory does not route to overlay.
        let routed = route(
            &subs,
            &event(
                Path::new("/cfg/accounts.toml"),
                notify::EventKind::Create(notify::event::CreateKind::File),
            ),
        );
        assert_eq!(routed, vec![(accounts.clone(), ChangeKind::Create)]);

        // Unregistered file in the same directory routes nowhere.
        let routed = route(
            &subs,
            &event(
                Path::new("/cfg/config.toml"),
                notify::EventKind::Modify(notify::event::ModifyKind::Data(
                    notify::event::DataChange::Any,
                )),
            ),
        );
        assert!(routed.is_empty());

        // Pure access noise never routes, even for a registered basename.
        let routed = route(
            &subs,
            &event(
                Path::new("/cfg/models-overlay.toml"),
                notify::EventKind::Access(notify::event::AccessKind::Read),
            ),
        );
        assert!(routed.is_empty());
    }

    // Map a notify kind back to our coarse kind for the assertion helper.
    impl ChangeKind {
        fn from_kind(kind: &notify::EventKind) -> ChangeKind {
            classify(kind).expect("event kind is classified")
        }
    }
}
