//! Platform cfg dispatch for [`crate::PowerMonitor`] construction.
//!
//! Each cfg'd module exposes the same contract: `spawn_monitor()` performs
//! platform-specific subscription setup and returns either an active
//! monitor or the inert one (unsupported platforms). Setup that can fail
//! happens HERE (on the caller's thread) so errors surface from
//! [`crate::PowerMonitor::new`]; the long-lived event loop then runs on the
//! dedicated monitor thread.

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "macos")]
mod macos;

use crate::{PowerMonitor, PowerMonitorError};

/// Spawn/subscribe for the current platform (see crate docs for the
/// per-platform table).
///
/// # Errors
///
/// Returns [`PowerMonitorError::Connection`] / [`PowerMonitorError::Subscription`]
/// when the platform provider cannot be reached or subscribed to, and
/// [`PowerMonitorError::Spawn`] when the monitor thread cannot be created.
/// Never fails on platforms without a native mechanism.
pub fn spawn_monitor() -> Result<PowerMonitor, PowerMonitorError> {
    #[cfg(target_os = "linux")]
    return linux::spawn_monitor();
    #[cfg(target_os = "macos")]
    return macos::spawn_monitor();
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    return Ok(unsupported_monitor());
}

/// The inert fallback used on platforms without a wired-up mechanism
/// (Windows today): `Ok` with a never-firing receiver, logged ONCE so the
/// observability layer knows power events are unavailable and the
/// kernel-level dead-link detection (sockreg keepalives) is the active
/// layer.
// `allow(dead_code)` is scoped to exactly this function: on Linux/macOS the
// platform module below handles subscription and this fallback is never
// called, but it MUST stay compiled everywhere so the unsupported-platform
// path is never accidentally deleted when a new platform module is added.
#[allow(dead_code)]
fn unsupported_monitor() -> PowerMonitor {
    tracing::info!(
        platform = std::env::consts::OS,
        "power notifications unavailable; falling back to kernel-level dead-link detection"
    );
    PowerMonitor::inert()
}

/// Shared helper for the platform modules: spawn a named monitor thread,
/// mapping spawn failure to [`PowerMonitorError::Spawn`]. The thread is
/// daemon-like — it never blocks process exit handling and is not joined
/// (see the `Drop` rationale on [`crate::PowerMonitor`]).
// `allow(dead_code)`: the Windows/unsupported fallback never spawns a
// thread, but the helper must stay compiled on every target so each
// platform module can rely on it.
#[allow(dead_code)]
pub(crate) fn spawn_thread(
    name: &'static str,
    body: impl FnOnce() + Send + 'static,
) -> Result<(), PowerMonitorError> {
    std::thread::Builder::new()
        .name(name.to_string())
        .spawn(body)
        .map(|_| ())
        .map_err(|source| {
            tracing::error!(%source, thread = name, "failed to spawn power-monitor thread");
            PowerMonitorError::Spawn { source }
        })
}
