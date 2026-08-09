//! Integration tests for the bounded session-thread shutdown join.
//!
//! These exercise real threads and real time, so they are `#[ignore]`d like
//! all other integration tests (run with `cargo test -- --ignored`).
//!
//! They use the grace-parameterized seam
//! [`join_session_shutdown_with_grace_for_test`] with a 30 ms grace instead
//! of the production 5 s [`SESSION_SHUTDOWN_GRACE`], so each test completes
//! in a few hundred ms at most.

use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use choreo_daemon::join_session_shutdown_with_grace_for_test;

#[test]
#[ignore]
fn joins_a_finished_session_thread() {
    // A thread that returns immediately exits before the grace period
    // elapses, so the bounded join must report success.
    let handle = thread::spawn(|| {});
    let exited = join_session_shutdown_with_grace_for_test(handle, 1, Duration::from_millis(30));
    assert!(exited, "finished thread must be joined successfully");
}

#[test]
#[ignore]
fn abandons_a_stuck_session_thread() {
    // A thread blocked on a channel that is never sent to models a request
    // worker stuck in a provider read.  The bounded join must give up after
    // the grace period instead of hanging the caller forever.
    let (tx, rx) = mpsc::channel::<()>();
    let handle = thread::spawn(move || {
        let _ = rx.recv();
        let _ = tx; // keep the sender alive so recv never errors out
    });
    let exited = join_session_shutdown_with_grace_for_test(handle, 1, Duration::from_millis(30));
    assert!(!exited, "stuck thread must be abandoned, not joined");
}
