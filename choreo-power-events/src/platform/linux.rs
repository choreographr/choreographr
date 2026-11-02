//! Linux backend: systemd-logind's `PrepareForSleep(bool)` D-Bus signal.
//!
//! # Async→sync bridge (all inside the monitor thread)
//!
//! zbus 6 exposes only an async API; its default `zruntime` backend runs the
//! connection on zbus's own helper thread, so no external executor (and in
//! particular no tokio) is needed. A synchronous program drives that API with
//! `zbus::block_on`, which polls a future to completion on the calling thread.
//! We therefore do ALL zbus work — connect, build the proxy, subscribe, and
//! consume the signal stream — inside a single `zbus::block_on` on the monitor
//! thread. Setup failures are reported back to [`spawn_monitor`] through a
//! one-shot readiness channel so they still surface from
//! [`crate::PowerMonitor::new`] on the caller's thread; once the thread is
//! running its loop cannot fail — it just blocks waiting for the next signal.

use super::spawn_thread;
use crate::{PowerMonitor, PowerMonitorError};
use futures_util::StreamExt;

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
/// Subscription errors surface synchronously from this call (via the
/// readiness handshake) so [`crate::PowerMonitor::new`] can report them (or
/// `best_effort` can fall back); once the signal stream is handed to the
/// thread's loop it cannot fail — it just blocks waiting for the next signal.
pub fn spawn_monitor() -> Result<PowerMonitor, PowerMonitorError> {
    // Unbounded: sleep/wake are human-rate events; the consumer (daemon
    // command loop) is always around to drain them in practice.
    let (sender, receiver) = crossbeam_channel::unbounded();

    // One-shot readiness handshake: the monitor thread reports the outcome of
    // its setup (connect + subscribe) here, so `spawn_monitor` can return that
    // error from the caller's thread rather than swallowing it on the thread.
    // A one-shot reply to one waiting consumer is exactly what a bounded(1)
    // channel models.
    let (ready_tx, ready_rx) = crossbeam_channel::bounded::<Result<(), PowerMonitorError>>(1);

    spawn_thread("power-monitor-logind", move || {
        zbus::block_on(async move {
            // Connect, build the proxy, and subscribe — all as one fallible
            // setup step whose error is carrying back over `ready_tx`.
            let signals = setup_logind().await;
            let signals = match signals {
                Ok(signals) => signals,
                Err(error) => {
                    // Nobody is listening after a setup failure; ignore the
                    // send result (the caller reports the error it receives).
                    let _ = ready_tx.send(Err(error));
                    return;
                }
            };

            // Pin the stream so `next()` works whether or not `SignalStream`
            // is `Unpin` (the pinned pointer is itself `Unpin`).
            let mut signals = std::pin::pin!(signals);

            // Setup succeeded: report readiness. A failed send means the
            // caller already dropped the monitor between spawning this thread
            // and the handshake completing — there is nothing to serve.
            if ready_tx.send(Ok(())).is_err() {
                return;
            }

            tracing::info!(
                destination = LOGIND_DESTINATION,
                signal = PREPARE_FOR_SLEEP,
                "subscribed to systemd-logind power notifications"
            );
            while let Some(signal) = signals.next().await {
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
                    crate::SuspendEvent::Sleep
                } else {
                    tracing::info!("system has resumed (logind PrepareForSleep(false))");
                    crate::SuspendEvent::Wake
                };
                // Failing send means the consumer dropped the receiver — the
                // monitor's job is done; exit the loop quietly.
                if sender.send(event).is_err() {
                    tracing::debug!("power-event receiver dropped; logind monitor exiting");
                    break;
                }
            }
            // Reaching here means the D-Bus stream ended (connection closed)
            // or the consumer went away — the thread ends and is reaped.
            tracing::debug!("logind power monitor loop ended");
        });
    })?;

    // Wait for the monitor thread's setup outcome. If the thread died before
    // reporting (a panic during setup disconnects the channel), surface an
    // error instead of hanging.
    match ready_rx.recv() {
        Ok(Ok(())) => Ok(PowerMonitor {
            events: receiver,
            // The sender moved into the monitor thread; `None` here means the
            // PowerMonitor itself is not holding a producing half.
            sender: None,
            // Connected + subscribed: this monitor is the real thing.
            active: true,
        }),
        Ok(Err(error)) => Err(error),
        Err(_) => Err(PowerMonitorError::Spawn {
            source: std::io::Error::other(
                "power-monitor thread exited before subscribing to logind",
            ),
        }),
    }
}

/// Connect to the system bus, build the logind proxy, and subscribe to the
/// `PrepareForSleep` signal. The returned stream is owned (`'static` — the
/// signal name is a `&'static str`), so it outlives this future and can be
/// consumed by the monitor thread's loop.
async fn setup_logind() -> Result<zbus::proxy::SignalStream<'static>, PowerMonitorError> {
    // The system bus is where logind lives; `Connection::session()` would
    // return the *user's* session bus which usually proxies but is not
    // guaranteed to, so we go straight to the system bus.
    let connection = zbus::Connection::system().await.map_err(|source| {
        tracing::warn!(
            %source,
            "could not connect to the system D-Bus; logind power events unavailable"
        );
        PowerMonitorError::Connection {
            source: Box::new(source),
        }
    })?;

    let proxy = zbus::Proxy::new(
        &connection,
        LOGIND_DESTINATION,
        LOGIND_PATH,
        LOGIND_INTERFACE,
    )
    .await
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

    // Subscribing (adding the match rule) happens here; the returned stream
    // owns a clone of the connection/proxy, so it keeps working after `proxy`
    // is dropped at the end of this function.
    proxy
        .receive_signal(PREPARE_FOR_SLEEP)
        .await
        .map_err(|source| {
            tracing::warn!(%source, signal = PREPARE_FOR_SLEEP, "could not subscribe to the logind signal");
            PowerMonitorError::Subscription {
                source: Box::new(source),
            }
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
