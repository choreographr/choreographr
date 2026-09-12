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
use tracing::{debug, info, trace, warn};

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
    #[must_use]
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
            .spawn(move || transport_loop(&self.subscribers, &self.dir));
    }
}

/// Map a raw `notify` event kind onto our coarse [`ChangeKind`]. Returns
/// `None` for pure `Access`/`Other` noise (e.g. macOS `FSEvents` access
/// reports) that must never trigger a reload.
fn classify(kind: notify::EventKind) -> Option<ChangeKind> {
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
    let Some(kind) = classify(event.kind) else {
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

/// Whether a raw event signals that the kernel's watch queue overflowed and
/// an unknown number of real events were dropped. `notify` 8.x surfaces the
/// inotify `IN_Q_OVERFLOW` as an `Ok(Event)` with `EventKind::Other` and the
/// `Flag::Rescan` attribute set (other backends use the same flag for their
/// analogous "you missed something" signal), so the flag — not the event kind
/// — is the reliable detection. Pure and unit-testable.
fn is_overflow(event: &notify::Event) -> bool {
    event.flag() == Some(notify::event::Flag::Rescan)
}

/// Read the current content of a watched file, or `None` when it is absent
/// (or transiently unreadable — subscribers re-read and fingerprint-gate
/// themselves, so a spurious "absent" snapshot can at worst cause one no-op
/// reload, never a wrong decision on our side).
fn snapshot_content(dir: &Path, basename: &Path) -> Option<Vec<u8>> {
    std::fs::read(dir.join(basename)).ok()
}

/// Compare the directory's current state against the watcher's last-known
/// view and derive the synthetic changes an overflow replay must deliver.
/// Returns `(basename, kind)` pairs for every registered basename whose state
/// diverges from `last_known`, and updates `last_known` to the fresh state:
///
/// - file present, previously absent → `Create` (also covers files whose
///   creation was swallowed by the overflow before any real event arrived);
/// - file present, content differs from last known → `Modify`;
/// - file gone, previously present → `Remove`;
/// - file present, content identical to last known → nothing (a real event
///   for that write already reached subscribers, or the change is pre-watch
///   baseline — replaying a no-op would be a spurious reload).
///
/// Pure relative to (dir, `last_known`) — no sleeps, no kernel overflow needed —
/// so unit tests drive it directly with a temp dir.
fn rescan_changes(
    dir: &Path,
    subscribers: &HashMap<PathBuf, Vec<Sender<ConfigChange>>>,
    last_known: &mut HashMap<PathBuf, Option<Vec<u8>>>,
) -> Vec<(PathBuf, ChangeKind)> {
    let mut out = Vec::new();
    for basename in subscribers.keys() {
        let current = snapshot_content(dir, basename);
        // Absent entries default to `None` ("file was not there before"), so
        // a first-ever rescan sees an existing file as a Create replay rather
        // than silently assuming it was already known.
        let prev = last_known.entry(basename.clone()).or_insert(None);
        let kind = match (&current, &*prev) {
            (Some(_), None) => Some(ChangeKind::Create),
            (Some(cur), Some(prev)) if cur != prev => Some(ChangeKind::Modify),
            (None, Some(_)) => Some(ChangeKind::Remove),
            // Identical content or absent-and-always-absent: nothing to replay.
            _ => None,
        };
        // Update the view regardless of whether a change is synthesized, so
        // repeated rescans are idempotent (a second overflow replays nothing).
        *prev = current;
        if let Some(kind) = kind {
            out.push((basename.clone(), kind));
        }
    }
    out
}

/// Record the current on-disk content of the routed basenames into the
/// last-known view after real (non-synthesized) events were delivered. Doing
/// this per real event keeps the view fresh so a subsequent overflow rescan
/// compares against the state subscribers already saw — a replay then emits
/// nothing for changes that were never dropped.
fn note_routed_state(
    dir: &Path,
    last_known: &mut HashMap<PathBuf, Option<Vec<u8>>>,
    routed: &[(PathBuf, ChangeKind)],
) {
    for (basename, _) in routed {
        let content = snapshot_content(dir, basename);
        last_known.insert(basename.clone(), content);
    }
}

/// The transport thread: owns the `notify` watcher, ensures the dir, and
/// routes every raw event to the matching subscribers. Re-arms the watch on a
/// fixed cadence while it is unarmed (a dir deleted at runtime, or a
/// creation failure above that has since been fixed).
fn transport_loop(subscribers: &HashMap<PathBuf, Vec<Sender<ConfigChange>>>, dir: &Path) {
    ensure_config_dir(dir);

    // Last-known content per registered basename, used only to decide what an
    // overflow rescan must replay (see `rescan_changes`). Owned exclusively by
    // this single thread — no shared state, it just shadows the FS state.
    let mut last_known: HashMap<PathBuf, Option<Vec<u8>>> = HashMap::new();

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
            match w.watch(dir, RecursiveMode::NonRecursive) {
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
            if let Ok(raw) = raw_rx.recv() {
                raw
            } else {
                info!("config watch raw channel closed; exiting");
                break;
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
                if armed && dir_was_removed(dir, &event) {
                    armed = false;
                    info!(
                        dir = %dir.display(),
                        "config directory removed; watch will re-arm on the next retry",
                    );
                }
                if is_overflow(&event) {
                    // The kernel queue overflowed: an unknown subset of real
                    // events was dropped, so replaying them from the event
                    // stream is impossible. Rescan the directory instead and
                    // synthesize the divergences from our last-known view —
                    // subscribers then cannot miss a dropped Create/Modify/
                    // Remove, which is what made the config_watch tests flake
                    // under full-suite parallel load.
                    warn!(
                        dir = %dir.display(),
                        "watcher queue overflow detected; rescanning the config dir",
                    );
                    for (basename, kind) in rescan_changes(dir, subscribers, &mut last_known) {
                        debug!(basename = %basename.display(), ?kind,
                            "overflow rescan synthesized a change");
                        deliver(subscribers, dir, &basename, kind);
                    }
                    debug!(
                        dir = %dir.display(),
                        tracked = last_known.len(),
                        "overflow rescan complete; last-known state refreshed",
                    );
                } else {
                    let routed = route(subscribers, &event);
                    // Snapshot BEFORE delivery (single thread — either order
                    // is race-free); what matters is refreshing the view per
                    // real event, so a subsequent overflow rescan compares
                    // against the freshest on-disk content rather than a
                    // stale view and replays nothing subscribers already
                    // saw (or never saw, because nothing diverged).
                    note_routed_state(dir, &mut last_known, &routed);
                    for (basename, kind) in routed {
                        trace!(basename = %basename.display(), ?kind, "delivering config change");
                        deliver(subscribers, dir, &basename, kind);
                    }
                }
            }
            Err(e) => {
                // A read error from the watch backend can also mean missed
                // events (platforms other than inotify report queue loss via
                // the error path), so replay via rescan here for the same
                // lost-event safety — the rescan itself is a no-op when
                // nothing actually diverged.
                warn!(error = %e, dir = %dir.display(),
                    "config watcher error; rescanning the config dir to replay any missed changes");
                for (basename, kind) in rescan_changes(dir, subscribers, &mut last_known) {
                    debug!(basename = %basename.display(), ?kind,
                        "error-path rescan synthesized a change");
                    deliver(subscribers, dir, &basename, kind);
                }
            }
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
            classify(notify::EventKind::Access(notify::event::AccessKind::Read)),
            None
        );
        assert_eq!(classify(notify::EventKind::Other), None);
        // Create/Modify/Remove map onto the coarse kinds.
        assert_eq!(
            classify(notify::EventKind::Create(notify::event::CreateKind::File)),
            Some(ChangeKind::Create)
        );
        assert_eq!(
            classify(notify::EventKind::Modify(notify::event::ModifyKind::Data(
                notify::event::DataChange::Any
            ))),
            Some(ChangeKind::Modify)
        );
        assert_eq!(
            classify(notify::EventKind::Remove(notify::event::RemoveKind::File)),
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
            assert_eq!(routed, vec![(overlay.clone(), ChangeKind::from_kind(kind))]);
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
        assert_eq!(routed, [] as [(PathBuf, ChangeKind); 0]);

        // Pure access noise never routes, even for a registered basename.
        let routed = route(
            &subs,
            &event(
                Path::new("/cfg/models-overlay.toml"),
                notify::EventKind::Access(notify::event::AccessKind::Read),
            ),
        );
        assert_eq!(routed, [] as [(PathBuf, ChangeKind); 0]);
    }

    #[test]
    fn is_overflow_detects_only_the_rescan_flag() {
        // notify 8.x surfaces inotify IN_Q_OVERFLOW as Other + Flag::Rescan.
        let overflow = event(Path::new("/cfg"), notify::EventKind::Other)
            .set_flag(notify::event::Flag::Rescan); // set_flag consumes and returns the event
        assert!(is_overflow(&overflow));
        // Real events (even Other noise) are never overflows.
        assert!(!is_overflow(&event(
            Path::new("/cfg/accounts.toml"),
            notify::EventKind::Create(notify::event::CreateKind::File)
        )));
        assert!(!is_overflow(&event(
            Path::new("/cfg"),
            notify::EventKind::Other
        )));
    }

    #[test]
    fn rescan_synthesizes_create_modify_remove_divergences() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();
        std::fs::write(d.join("accounts.toml"), "a = 1\n").unwrap();
        std::fs::write(d.join("models-overlay.toml"), "o = 1\n").unwrap();
        let mut subs: HashMap<PathBuf, Vec<Sender<ConfigChange>>> = HashMap::new();
        let (tx, _rx) = crossbeam_channel::unbounded();
        subs.insert(PathBuf::from("accounts.toml"), vec![tx]);
        let (tx2, _rx2) = crossbeam_channel::unbounded();
        subs.insert(PathBuf::from("models-overlay.toml"), vec![tx2]);
        let mut last_known: HashMap<PathBuf, Option<Vec<u8>>> = HashMap::new();

        // First rescan with no prior view: every existing registered file is a
        // replayed Create (its real Create may have been dropped). Compare as
        // a set — iteration order over the subscribers HashMap is arbitrary.
        let changes = rescan_changes(d, &subs, &mut last_known);
        let expected = vec![
            (PathBuf::from("accounts.toml"), ChangeKind::Create),
            (PathBuf::from("models-overlay.toml"), ChangeKind::Create),
        ];
        assert_eq!(changes.len(), 2);
        for pair in expected {
            assert!(changes.contains(&pair), "missing replay of {pair:?}");
        }
        // Rescans are idempotent: no divergence, nothing replayed.
        assert_eq!(
            rescan_changes(d, &subs, &mut last_known),
            [] as [(PathBuf, ChangeKind); 0]
        );

        // Content change with no real event seen → Modify.
        std::fs::write(d.join("accounts.toml"), "a = 2\n").unwrap();
        assert_eq!(
            rescan_changes(d, &subs, &mut last_known),
            vec![(PathBuf::from("accounts.toml"), ChangeKind::Modify)]
        );

        // Removal with no real event seen → Remove; the still-present overlay
        // file yields nothing.
        std::fs::remove_file(d.join("accounts.toml")).unwrap();
        assert_eq!(
            rescan_changes(d, &subs, &mut last_known),
            vec![(PathBuf::from("accounts.toml"), ChangeKind::Remove)]
        );
        assert_eq!(
            rescan_changes(d, &subs, &mut last_known),
            [] as [(PathBuf, ChangeKind); 0]
        );

        // Recreation after a Remove replays as a Create again.
        std::fs::write(d.join("accounts.toml"), "a = 3\n").unwrap();
        assert_eq!(
            rescan_changes(d, &subs, &mut last_known),
            vec![(PathBuf::from("accounts.toml"), ChangeKind::Create)]
        );
    }

    #[test]
    fn rescan_delivers_through_the_subscriber_channel() {
        // Pin that the synthesized changes flow out the SAME routing path as
        // real events (deliver over the registered senders).
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("accounts.toml"), "a = 1\n").unwrap();
        let mut subs: HashMap<PathBuf, Vec<Sender<ConfigChange>>> = HashMap::new();
        let (tx, rx) = crossbeam_channel::unbounded();
        subs.insert(PathBuf::from("accounts.toml"), vec![tx]);
        let mut last_known: HashMap<PathBuf, Option<Vec<u8>>> = HashMap::new();
        for (basename, kind) in rescan_changes(dir.path(), &subs, &mut last_known) {
            deliver(&subs, dir.path(), &basename, kind);
        }
        let got = rx.try_recv().unwrap();
        assert_eq!(got.kind, ChangeKind::Create);
        assert_eq!(got.path, dir.path().join("accounts.toml"));
    }

    #[test]
    fn note_routed_state_suppresses_replay_of_already_delivered_changes() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();
        std::fs::write(d.join("accounts.toml"), "a = 1\n").unwrap();
        let mut subs: HashMap<PathBuf, Vec<Sender<ConfigChange>>> = HashMap::new();
        let (tx, _rx) = crossbeam_channel::unbounded();
        subs.insert(PathBuf::from("accounts.toml"), vec![tx]);
        let mut last_known: HashMap<PathBuf, Option<Vec<u8>>> = HashMap::new();

        // A real Create was routed and delivered: note the routed state.
        let routed = vec![(PathBuf::from("accounts.toml"), ChangeKind::Create)];
        note_routed_state(d, &mut last_known, &routed);
        // An overflow right after must NOT replay it (subscribers already saw it).
        assert_eq!(
            rescan_changes(d, &subs, &mut last_known),
            [] as [(PathBuf, ChangeKind); 0]
        );

        // The file vanishing without a real Remove event still replays.
        std::fs::remove_file(d.join("accounts.toml")).unwrap();
        assert_eq!(
            rescan_changes(d, &subs, &mut last_known),
            vec![(PathBuf::from("accounts.toml"), ChangeKind::Remove)]
        );
    }

    // Map a notify kind back to our coarse kind for the assertion helper.
    impl ChangeKind {
        fn from_kind(kind: notify::EventKind) -> ChangeKind {
            classify(kind).expect("event kind is classified")
        }
    }
}
