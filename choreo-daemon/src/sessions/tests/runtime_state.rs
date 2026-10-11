//! Runtime-only-field and helper tests: `loaded_skill_bodies` /
//! `context_cache` snapshot round-trips, bounded session-thread join polling,
//! and the default active tool-group set.

use super::*;
use crate::context::LoadedSkill;

// -- loaded_skill_bodies / context_cache field tests -------------------

#[test]
fn loaded_skill_bodies_default_is_empty() {
    let state = SessionState::empty();
    assert!(state.loaded_skill_bodies.is_empty());
}

#[test]
fn context_cache_default_is_none() {
    let state = SessionState::empty();
    assert!(state.context_cache.is_none());
}

#[test]
fn loaded_skill_bodies_survives_snapshot_round_trip() {
    let mut state = SessionState::empty();
    state.loaded_skill_bodies.push(LoadedSkill {
        name: "test".to_string(),
        body: "body content".to_string(),
    });

    let snap = state.snapshot();
    assert_eq!(snap.loaded_skill_bodies.len(), 1);
    assert_eq!(snap.loaded_skill_bodies[0].name, "test");

    let restored = SessionState::from_snapshot(snap, HashMap::new());
    assert_eq!(restored.loaded_skill_bodies.len(), 1);
    assert_eq!(restored.loaded_skill_bodies[0].name, "test");
    assert_eq!(restored.loaded_skill_bodies[0].body, "body content");
}

#[test]
fn context_cache_survives_snapshot_round_trip() {
    let mut state = SessionState::empty();
    state.context_cache = Some((42, Arc::new("cached content".to_string())));

    let snap = state.snapshot();
    assert_eq!(
        snap.context_cache,
        Some((42, Arc::new("cached content".to_string())))
    );

    let restored = SessionState::from_snapshot(snap, HashMap::new());
    assert_eq!(
        restored.context_cache,
        Some((42, Arc::new("cached content".to_string())))
    );
}

#[test]
fn shutdown_join_poll_joins_when_finished() {
    // Deterministic: the finish check passes immediately, so the reap
    // callback runs and no clock/sleep is consulted.
    let mut reaped = false;
    let exited = shutdown_join_poll(
        1,
        std::time::Duration::from_millis(30),
        || true,
        || reaped = true,
        std::time::Instant::now,
        |_| panic!("must not sleep when already finished"),
    );
    assert!(exited, "finished thread must be joined successfully");
    assert!(reaped, "the reap callback must run when the check passes");
}

#[test]
fn shutdown_join_poll_abandons_after_deadline() {
    // Deterministic: a fake clock advances 10 ms per read and sleep is a
    // no-op collector, so no real time elapses.  The loop must give up once
    // the 30 ms deadline passes and never sleep past the 50 ms poll cap.
    let base = std::time::Instant::now();
    let mut elapsed = std::time::Duration::ZERO;
    let mut clock = move || {
        elapsed += std::time::Duration::from_millis(10);
        base + elapsed
    };
    let mut slept: Vec<std::time::Duration> = Vec::new();
    let exited = shutdown_join_poll(
        1,
        std::time::Duration::from_millis(30),
        || false,
        || panic!("must not reap a thread that has not finished"),
        &mut clock,
        |d| slept.push(d),
    );
    assert!(!exited, "stuck thread must be abandoned, not joined");
    assert!(!slept.is_empty(), "the poll loop must sleep while waiting");
    assert!(
        slept
            .iter()
            .all(|d| *d <= std::time::Duration::from_millis(50)),
        "each sleep must respect the 50 ms poll cap"
    );
}

#[test]
fn poll_join_with_grace_abandons_stuck_thread() {
    // A thread blocked on a channel that is never sent to models a request
    // worker stuck in a provider read that a cancel cannot interrupt.  The
    // bounded join must give up after the grace period instead of hanging
    // the caller forever.  Fully deterministic: the fake clock advances
    // instantly and `sleep` is a no-op, so no real time elapses.
    let (tx, rx) = mpsc::channel::<()>();
    let handle = std::thread::spawn(move || {
        let _ = rx.recv();
    });
    let base = std::time::Instant::now();
    let mut elapsed = std::time::Duration::ZERO;
    let mut clock = move || {
        elapsed += std::time::Duration::from_millis(10);
        base + elapsed
    };
    let mut no_sleep = |_: std::time::Duration| {};
    let exited = poll_join_with_grace(
        handle,
        1,
        std::time::Duration::from_millis(30),
        &mut clock,
        &mut no_sleep,
    );
    assert!(!exited, "stuck thread must be abandoned, not joined");
    // Drop the last sender so the blocked thread's `recv` errors out and
    // it exits promptly (no leaked threads in the test runner).
    drop(tx);
}

#[test]
fn default_active_tool_groups_feature_branches() {
    // The default set always contains the three always-on groups; the
    // Coordination Platform group appears only when the `content` feature is
    // compiled in, and only under its post-rename name. The pre-rename
    // "coord" name must never be a default (stale persisted names are
    // silently ignored instead — see `default_active_tool_groups`).
    let groups = default_active_tool_groups();
    for always_on in ["core", "git", "shell"] {
        assert!(groups.contains(always_on), "missing {always_on}");
    }
    #[cfg(feature = "content")]
    {
        assert!(
            groups.contains("content"),
            "`content` feature on but group not in defaults: {groups:?}"
        );
        assert!(!groups.contains("coord"));
    }
    #[cfg(not(feature = "content"))]
    {
        assert!(
            !groups.contains("content") && !groups.contains("coord"),
            "content feature off but group in defaults: {groups:?}"
        );
    }
}
