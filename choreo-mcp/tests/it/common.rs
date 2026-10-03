//! Shared harness for `choreo-mcp`'s integration tests.
//!
//! Both the stdio suite (`mcp_integration`) and the Streamable HTTP suite
//! (`mcp_http_integration`) spawn a fixture and drive the client against it;
//! the per-test watchdog lives here so neither suite reimplements it.

use std::time::Duration;

/// Watchdog: the stdlib test harness has no per-test timeout, so a regression
/// that wedges connect/call/shutdown would hang CI. Abort if the body outlives
/// its budget; the client's configured timeouts bound a healthy run far lower.
pub fn watchdog() {
    std::thread::spawn(|| {
        std::thread::sleep(Duration::from_secs(120));
        eprintln!(
            "choreo-mcp integration test exceeded 120s; aborting to avoid an indefinite hang"
        );
        std::process::abort();
    });
}
