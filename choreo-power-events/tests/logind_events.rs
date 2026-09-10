//! Integration test: real systemd-logind subscription (Linux only).
//!
//! Marked `#[ignore]` per the workspace test discipline — it binds a
//! system resource (the D-Bus system bus socket). Run explicitly with
//! `cargo test-integration` / `cargo nextest run --run-ignored only`.
//!
//! It never actually suspends the machine: it only verifies that the
//! subscription (system bus connection, logind proxy, `PrepareForSleep`
//! match rule) can be established and that the monitor reports itself as
//! active. If logind or the system bus is absent, the test logs the
//! reason and returns gracefully.

#![cfg(target_os = "linux")]

use crossbeam_channel::TryRecvError;

#[test]
#[ignore]
fn logind_subscription_establishes() {
    match choreo_power_events::PowerMonitor::new() {
        Ok(monitor) => {
            // Real subscription: events() must be empty (we did not sleep)
            // but the monitor reports the live, active mode.
            assert!(monitor.is_active());
            assert_eq!(monitor.events().try_recv(), Err(TryRecvError::Empty));
            eprintln!("logind subscription established; monitor is active");
        }
        Err(error) => {
            // Graceful skip: CI containers and non-systemd hosts have no
            // logind. This is the documented "best-effort" path.
            eprintln!("skipping: logind/system-bus unavailable: {error}");
        }
    }
}

// The macOS twin of this test is intentionally a compile-check-only stub
// (see `tests/macos_power.rs` documentation); actually asserting a live
// IOKit registration requires a mac runner, which the workspace does not
// gate on.
