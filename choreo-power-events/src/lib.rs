//! Platform sleep/wake notifications as crossbeam channel events.
//!
//! One small leaf crate, consumed by the daemon's command loop: when the
//! machine is about to suspend ([`SuspendEvent::Sleep`], sent BEFORE the
//! machine sleeps) or has just resumed ([`SuspendEvent::Wake`], sent AFTER
//! the machine is back up), an event lands on the channel returned by
//! [`PowerMonitor::events`]. The daemon uses it to suspend/refresh
//! keepalive-dependent state (e.g. sockreg's TCP keepalive timers become
//! meaningless across a sleep, and re-connection logic should run on wake).
//!
//! # Best-effort semantics (IMPORTANT)
//!
//! These events are a convenience layer, NOT a correctness layer. A machine
//! can suspend without the platform API firing (logind missing, notification
//! port failing, the process being `SIGSTOP`ped, a VM's exotic suspend path).
//! Consumers MUST treat the absence of a `Sleep`/`Wake` event as a
//! non-issue: the kernel-level dead-link detection in `choreo-sockreg`
//! (TCP keepalives) is the fallback layer that keeps correctness without
//! this crate. Never write code where a missed event breaks invariants.
//!
//! # Platform support
//!
//! | Platform | Mechanism | Notes |
//! |---|---|---|
//! | Linux | systemd-logind `PrepareForSleep(bool)` D-Bus signal | Requires a session/system D-Bus with logind. |
//! | macOS | `IORegisterForSystemPower` + `CFRunLoop` | `IOAllowPowerChange` is called for `kIOMessageSystemWillSleep` — declining blocks system sleep for everyone. |
//! | Windows (and anything else) | Inert fallback | [`PowerMonitor::new`] returns `Ok` with a receiver that never fires; `is_active()` is `false`. |
//!
//! On Linux, if logind is unreachable, [`PowerMonitor::new`] returns the
//! underlying error and [`PowerMonitor::best_effort`] logs once and falls
//! back to the inert mode — the daemon uses `best_effort`.
//!
//! # Threading model
//!
//! [`PowerMonitor::new`] spawns one dedicated, daemon-like monitor thread
//! that owns the platform subscription and is the single producer on an
//! unbounded crossbeam channel. All the platform-specific async→sync
//! bridging (zbus's `block_on` on Linux) lives entirely inside that thread
//! — no tokio, no async runtimes leak out of this crate. [`Drop`] is
//! best-effort and NEVER blocks on the monitor thread: the thread is
//! daemon-like (it blocks in the platform's notification wait, which offers
//! no synchronous "stop" we could wake it with), so `Drop` only logs and
//! leaves the channel to be reaped at process exit. A graceful shutdown can
//! simply drop the receiver; the producer's `send` then fails and the
//! monitor thread exits on its own.

#![forbid(unsafe_op_in_unsafe_fn)]

pub mod platform;

use std::fmt;

/// A machine power-transition notification.
///
/// Timing guarantees: [`SuspendEvent::Sleep`] is delivered before the
/// machine actually suspends (logind `PrepareForSleep(true)` / the `IOKit`
/// `kIOMessageSystemWillSleep` interest callback); [`SuspendEvent::Wake`]
/// is delivered after resume (logind `PrepareForSleep(false)` /
/// `kIOMessageSystemHasPoweredOn`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SuspendEvent {
    /// The machine is about to suspend. The handler should do only
    /// synchronous, fast work — the system may sleep momentarily after
    /// this event is delivered.
    Sleep,
    /// The machine has just resumed from suspend. Network state (sockets,
    /// keepalives, sessions) may have gone stale while asleep.
    Wake,
}

/// Errors from constructing a [`PowerMonitor`].
#[derive(Debug, thiserror::Error)]
pub enum PowerMonitorError {
    /// Could not reach the platform's power-notification provider
    /// (e.g. the D-Bus session connection or the system bus with logind).
    #[error("failed to connect to the platform power-notification provider: {source}")]
    Connection {
        /// Underlying transport/connection failure.
        source: Box<dyn std::error::Error + Send + Sync>,
    },
    /// Connected, but the subscription itself failed (signal match rule,
    /// registration with the power manager, …).
    #[error("failed to subscribe to power notifications: {source}")]
    Subscription {
        /// Underlying subscription failure.
        source: Box<dyn std::error::Error + Send + Sync>,
    },
    /// Could not spawn the dedicated monitor thread.
    #[error("failed to spawn the power-monitor thread: {source}")]
    Spawn {
        /// The thread-spawn I/O error.
        source: std::io::Error,
    },
    /// The current platform has a native mechanism but this build does not
    /// support wiring it up (kept for completeness; the shipped cfg layout
    /// routes unsupported platforms to the inert fallback instead).
    #[error("power monitoring is unsupported on this platform: {platform}")]
    Unsupported {
        /// Human-readable platform identifier.
        platform: String,
    },
}

/// A live monitor that emits [`SuspendEvent`]s on a crossbeam channel.
///
/// Not `Clone` — each monitor owns its subscription thread and channel
/// (cloning would silently double-deliver every event).
pub struct PowerMonitor {
    /// The receiving half; the daemon `select!`s on this.
    events: crossbeam_channel::Receiver<SuspendEvent>,
    /// The sending half kept alive so the receiver reports `Empty` (never
    /// `Disconnected`) while the monitor exists — an inert monitor must be
    /// indistinguishable from an active-but-quiet one to a `select!`
    /// consumer. `allow(dead_code)`: only read implicitly via Drop.
    #[allow(dead_code)]
    sender: Option<crossbeam_channel::Sender<SuspendEvent>>,
    /// Whether real platform notifications are wired up (`true`) or this
    /// monitor is the inert fallback (`false`). Drives [`Self::is_active`].
    active: bool,
}

impl fmt::Debug for PowerMonitor {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // The receiver is not Debug; report the mode and whether a sender
        // is still held so logs never pretend the channel contents are
        // inspectable.
        f.debug_struct("PowerMonitor")
            .field("active", &self.active)
            .field("sender", &self.sender.is_some())
            .field("events", &"<receiver>")
            .finish()
    }
}

impl PowerMonitor {
    /// Subscribe to platform power events on a dedicated monitor thread.
    ///
    /// # Platform behaviour
    /// - **Linux**: connects to the session/system D-Bus and subscribes to
    ///   logind's `PrepareForSleep` signal. Errors (no bus, no logind,
    ///   subscription rejected) are returned as
    ///   [`PowerMonitorError::Connection`] / [`PowerMonitorError::Subscription`].
    /// - **macOS**: registers an `IOKit` system-power interest callback on a
    ///   CFRunLoop-driven monitor thread. **The monitor calls
    ///   `IOAllowPowerChange` for every sleep notification** — declining
    ///   would block the whole system from sleeping.
    /// - **Windows / other**: returns `Ok` with an inert monitor whose
    ///   receiver never fires (logged once with `info!`); use
    ///   [`PowerMonitor::is_active`] to report which mode you got.
    ///
    /// # Errors
    ///
    /// Returns [`PowerMonitorError::Connection`] / [`PowerMonitorError::Subscription`]
    /// when the platform provider cannot be reached or subscribed to (Linux),
    /// and [`PowerMonitorError::Spawn`] when the monitor thread cannot be
    /// created. Never fails on platforms without a native mechanism.
    pub fn new() -> Result<Self, PowerMonitorError> {
        platform::spawn_monitor()
    }

    /// Like [`PowerMonitor::new`], but falls back to the inert monitor on
    /// any subscription failure instead of erroring — for callers where
    /// power events are pure optimization. The failure is logged once with
    /// `warn!` (or `info!` for the inert-platform case).
    pub fn best_effort() -> Self {
        match Self::new() {
            Ok(monitor) => monitor,
            Err(error) => {
                // Why warn, not error: the keepalive fallback layer makes
                // events optional; a missing provider is degraded, not broken.
                tracing::warn!(
                    %error,
                    "power notifications unavailable; falling back to kernel-level \
                     dead-link detection"
                );
                Self::inert()
            }
        }
    }

    /// Construct the inert fallback monitor directly: `is_active()` is
    /// `false` and the receiver never fires. Used on unsupported platforms
    /// and by [`PowerMonitor::best_effort`] after a subscription failure.
    ///
    /// Public so tests (and callers that want the no-op mode explicitly)
    /// can build it without reaching platform code.
    #[must_use]
    pub fn inert() -> Self {
        // Unbounded: the (potential) producer is the monitor thread and the
        // events are human-rate; an inert monitor simply never sends.
        let (sender, receiver) = crossbeam_channel::unbounded();
        Self {
            events: receiver,
            // The inert monitor has no thread, so IT owns the sender —
            // dropping it here would surface as Disconnected to the
            // consumer instead of a quiet, empty channel.
            sender: Some(sender),
            active: false,
        }
    }

    /// The event channel. The single producer is the monitor thread; on
    /// this (inert) monitor nothing is ever sent.
    #[must_use]
    pub fn events(&self) -> &crossbeam_channel::Receiver<SuspendEvent> {
        &self.events
    }

    /// `true` if real platform power notifications are wired up; `false`
    /// for the inert fallback (unsupported platform or best-effort
    /// fallback after failure). Callers log this once at startup so the
    /// observability layer knows which mode the daemon is in.
    #[must_use]
    pub fn is_active(&self) -> bool {
        self.active
    }
}

impl Drop for PowerMonitor {
    fn drop(&mut self) {
        // Best-effort teardown, deliberately NOT blocking: the monitor
        // thread blocks inside the platform's notification wait (zbus's
        // `block_on` on Linux, CFRunLoopRun on macOS) and neither offers a
        // synchronous stop we can drive from here. Dropping the receiver
        // means the producer's next `send` fails and the thread exits on
        // its own; a process that never drops it is reaped at exit. This
        // thread is daemon-like by design.
        tracing::debug!(
            active = self.active,
            "PowerMonitor dropped; monitor thread (if any) will exit when its \
             channel send fails or the process exits"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossbeam_channel::TryRecvError;

    #[test]
    fn suspend_event_semantics() {
        // Copy/PartialEq/Eq are load-bearing: consumers stash the last
        // event and match on it in a select! arm.
        let sleep = SuspendEvent::Sleep;
        let wake = SuspendEvent::Wake;
        assert_eq!(sleep, SuspendEvent::Sleep);
        assert_eq!(wake, SuspendEvent::Wake);
        assert_ne!(sleep, wake);
        let copied = sleep; // Copy, not a move — proves the derive
        assert_eq!(copied, sleep);
        // Debug is used in tracing payloads.
        assert_eq!(format!("{sleep:?}"), "Sleep");
        assert_eq!(format!("{wake:?}"), "Wake");
    }

    #[test]
    fn inert_monitor_is_inactive_and_silent() {
        let monitor = PowerMonitor::inert();
        assert!(!monitor.is_active());
        // No events are ever produced: a non-blocking peek must report an
        // empty channel (never a sleep/wait — unit tests are time-free).
        assert_eq!(monitor.events().try_recv(), Err(TryRecvError::Empty));
    }

    #[test]
    fn error_type_construction() {
        // thiserror Display contracts the daemon logs rely on.
        let connection = PowerMonitorError::Connection {
            source: "no session bus".into(),
        };
        assert_eq!(
            connection.to_string(),
            "failed to connect to the platform power-notification provider: no session bus"
        );
        let subscription = PowerMonitorError::Subscription {
            source: "match rule rejected".into(),
        };
        assert_eq!(
            subscription.to_string(),
            "failed to subscribe to power notifications: match rule rejected"
        );
        let spawn = PowerMonitorError::Spawn {
            source: std::io::Error::other("thread pool exhausted"),
        };
        assert_eq!(
            spawn.to_string(),
            "failed to spawn the power-monitor thread: thread pool exhausted"
        );
        let unsupported = PowerMonitorError::Unsupported {
            platform: "plan9".to_string(),
        };
        assert_eq!(
            unsupported.to_string(),
            "power monitoring is unsupported on this platform: plan9"
        );
        // `source()` chaining keeps the inner error reachable for chained
        // logging in the daemon.
        assert!(std::error::Error::source(&spawn).is_some());
        assert!(std::error::Error::source(&unsupported).is_none());
    }
}
