//! Integration tests for the unified config-watching transport.
//!
//! The unit tests in `src/config_watch.rs` pin the pure routing/classification
//! logic; these exercise the real end-to-end path — `notify` observing a real
//! filesystem, the transport thread routing a real event to a subscribed
//! consumer. They bind real FS and use short bounded polls, so they belong in
//! `tests/` and are `#[ignore]`d (run via `cargo test-integration`).

use choreo_daemon::config_watch::{ChangeKind, ConfigChange, ConfigWatcher};
use crossbeam_channel::Receiver;
use std::thread;
use std::time::{Duration, Instant};

/// Poll `cond` every 10 ms until it returns true, panicking after ~5 s.
fn wait_for(what: &str, cond: impl Fn() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if cond() {
            return;
        }
        thread::sleep(Duration::from_millis(10));
    }
    panic!("timed out waiting for {what}");
}

/// Scan the subscriber's receiver for a change of the expected kind within a
/// bounded window, tolerating extra/noise events the platform may interleave
/// (e.g. macOS FSEvents emits extra events around the one we care about).
fn recv_kind(rx: &Receiver<ConfigChange>, expected: ChangeKind) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        match rx.try_recv() {
            Ok(c) if c.kind == expected => return,
            Ok(_) => continue,
            Err(_) => thread::sleep(Duration::from_millis(10)),
        }
    }
    panic!("did not observe a {expected:?} change within the timeout");
}

#[test]
#[ignore = "integration"]
fn config_watcher_surfaces_create_modify_remove() {
    let dir = tempfile::tempdir().unwrap();
    let cfg_dir = dir.path().join("choreographr");

    let mut watcher = ConfigWatcher::new(cfg_dir.clone());
    let rx = watcher.subscribe("accounts.toml");
    watcher.spawn();

    let target = cfg_dir.join("accounts.toml");
    // The transport thread creates the config dir; wait so writes are reliable.
    wait_for("the config dir to be created", || cfg_dir.is_dir());

    // Create.
    std::fs::write(&target, "x = 1\n").unwrap();
    recv_kind(&rx, ChangeKind::Create);

    // Modify.
    std::fs::write(&target, "x = 2\n").unwrap();
    recv_kind(&rx, ChangeKind::Modify);

    // Remove.
    std::fs::remove_file(&target).unwrap();
    recv_kind(&rx, ChangeKind::Remove);
}

#[test]
#[ignore = "integration"]
fn config_watcher_delivers_only_registered_basenames() {
    let dir = tempfile::tempdir().unwrap();
    let cfg_dir = dir.path().join("choreographr");

    let mut watcher = ConfigWatcher::new(cfg_dir.clone());
    let rx = watcher.subscribe("models-overlay.toml");
    watcher.spawn();

    let target = cfg_dir.join("models-overlay.toml");
    wait_for("the config dir to be created", || cfg_dir.is_dir());

    // Writing the registered file surfaces a change.
    std::fs::write(&target, "a = 1\n").unwrap();
    recv_kind(&rx, ChangeKind::Create);

    // Drain any extra events the platform queued around the create (some
    // platforms emit a trailing Modify), so the absence check below starts
    // from a clean queue.
    drain_all(&rx);

    // An unregistered file in the same directory must NOT be delivered: the
    // transport filters by basename, so the consumer hears nothing.
    let unregistered = cfg_dir.join("accounts.toml");
    std::fs::write(&unregistered, "x = 1\n").unwrap();
    let deadline = Instant::now() + Duration::from_millis(300);
    while Instant::now() < deadline {
        assert!(
            rx.try_recv().is_err(),
            "an unregistered basename must not be delivered to this subscriber"
        );
        thread::sleep(Duration::from_millis(10));
    }
}

/// Drain the receiver until it has been quiet for 50 ms (tolerates trailing
/// platform noise events).
fn drain_all(rx: &Receiver<ConfigChange>) {
    while rx.recv_timeout(Duration::from_millis(50)).is_ok() {}
}
