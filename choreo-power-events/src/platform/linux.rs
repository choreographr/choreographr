//! Linux backend: systemd-logind's `PrepareForSleep(bool)` D-Bus signal.
//!
//! # Async→sync bridge (all inside the monitor thread)
//!
//! zbus 5 ships a fully blocking façade (`zbus::blocking`) behind the
//! `blocking-api` feature: its `SignalIterator` wraps the async
//! `SignalStream` and internally calls zbus's own `block_on`
//! (async-io/async-executor — no tokio) on every `next()`. We build the
//! connection, proxy and signal iterator synchronously HERE (in
//! `spawn_monitor`, so failures become `PowerMonitorError`), then hand the
//! iterator to the monitor thread, which just loops on it and forwards
//! `PrepareForSleep(true)`→`Sleep` / `(false)`→`Wake` to the crossbeam
//! channel. No tokio reactor, no sidecar runtime, no blocking recv inside
//! async code — the async parts live entirely inside zbus, on our thread.

use super::spawn_thread;
use crate::{PowerMonitor, PowerMonitorError, SuspendEvent};

/// The D-Bus well-known name of systemd-logind (on the system bus).
const LOGIND_DESTINATION: &str = "org.freedesktop.login1";
/// Object path of the logind manager object.
const LOGIND_PATH: &str = "/org/freedesktop/login1";
/// The manager interface carrying the sleep notifications.
const LOGIND_INTERFACE: &str = "org.freedesktop.login1.Manager";
/// Signal name: `PrepareForSleep(b starting)` — `true` just before sleep,
/// `false` just after resume.
const PREPARE_FOR_SLEEP: &str = "PrepareForSleep";

/// Subscribe to logind's `PrepareForSleep` and spawn the monitor thread.
///
/// Subscription errors surface synchronously from this call so
/// [`crate::PowerMonitor::new`] can report them (or `best_effort` can fall
/// back); once the iterator is handed to the thread it cannot fail — it
/// just blocks waiting for the next signal.
pub fn spawn_monitor() -> Result<PowerMonitor, PowerMonitorError> {
    // The system bus is where logind lives; `Connection::session()` would
    // return the *user's* session bus which usually proxies but is not
    // guaranteed to, so we go straight to the system bus.
    let connection = zbus::blocking::Connection::system().map_err(|source| {
        tracing::warn!(
            %source,
            "could not connect to the system D-Bus; logind power events unavailable"
        );
        PowerMonitorError::Connection {
            source: Box::new(source),
        }
    })?;

    let proxy = zbus::blocking::Proxy::new(
        &connection,
        LOGIND_DESTINATION,
        LOGIND_PATH,
        LOGIND_INTERFACE,
    )
    .map_err(|source| {
        tracing::warn!(
            %source,
            destination = LOGIND_DESTINATION,
            "could not create the logind proxy"
        );
        PowerMonitorError::Subscription {
            source: Box::new(source),
        }
    })?;

    // Subscribing (adding the match rule) happens here, synchronously; the
    // returned iterator is owned by the monitor thread from now on.
    let signals = proxy
        .receive_signal(PREPARE_FOR_SLEEP)
        .map_err(|source| {
            tracing::warn!(%source, signal = PREPARE_FOR_SLEEP, "could not subscribe to the logind signal");
            PowerMonitorError::Subscription {
                source: Box::new(source),
            }
        })?;

    // Unbounded: sleep/wake are human-rate events; the consumer (daemon
    // command loop) is always around to drain them in practice.
    let (sender, receiver) = crossbeam_channel::unbounded();

    spawn_thread("power-monitor-logind", move || {
        tracing::info!(
            destination = LOGIND_DESTINATION,
            signal = PREPARE_FOR_SLEEP,
            "subscribed to systemd-logind power notifications"
        );
        for signal in signals {
            // Body is the `starting` boolean: true = about to sleep,
            // false = just woke. A deserialization failure would mean a
            // broken logind — log and skip rather than killing the thread.
            let starting: bool = match signal.body().deserialize() {
                Ok(starting) => starting,
                Err(error) => {
                    tracing::warn!(%error, "unparsable PrepareForSleep body; ignoring signal");
                    continue;
                }
            };
            let event = if starting {
                tracing::info!("system is about to suspend (logind PrepareForSleep(true))");
                SuspendEvent::Sleep
            } else {
                tracing::info!("system has resumed (logind PrepareForSleep(false))");
                SuspendEvent::Wake
            };
            // Failing send means the consumer dropped the receiver — the
            // monitor's job is done; exit the thread quietly.
            if sender.send(event).is_err() {
                tracing::debug!("power-event receiver dropped; logind monitor exiting");
                break;
            }
        }
        // Reaching here means the D-Bus stream ended (connection closed) or
        // the consumer went away — the thread ends and is reaped naturally.
        tracing::debug!("logind power monitor loop ended");
    })?;

    Ok(PowerMonitor {
        events: receiver,
        // The sender moves into the monitor thread; `None` here means the
        // PowerMonitor itself is not holding a producing half.
        sender: None,
        // Connected + subscribed: this monitor is the real thing.
        active: true,
    })
}
#[cfg(test)]
mod tests {
    use super::*;

    // Compile-only assertion that the module's public shape matches the
    // platform contract (real subscription logic is exercised by the
    // ignored integration test, not unit tests).
    #[test]
    fn error_mapping_shapes() {
        fn box_error(e: zbus::Error) -> PowerMonitorError {
            PowerMonitorError::Connection {
                source: Box::new(e),
            }
        }
        let error = box_error(zbus::Error::Unsupported);
        assert!(error.to_string().contains("power-notification provider"));
    }
}
