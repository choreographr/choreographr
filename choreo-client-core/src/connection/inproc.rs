//! The embedded, in-process channel transport (the
//! [`super::ConnectionMode::InProcess`] mode).
//!
//! Messages travel as Rust VALUES over crossbeam channels — no codec, no
//! socket — and channel close IS the EOF in both directions, exactly like the
//! daemon-side embedded path.

use choreo_proto::{ClientMessage, DaemonMessage};
use crossbeam_channel::{Receiver as CrossbeamReceiver, Sender as CrossbeamSender};
use std::thread;
use tracing::{info, warn};

/// Serve a connection over an in-process channel pair.
///
/// Structure mirrors `serve_noise_connection` EXACTLY, with channels
/// replacing the transport:
/// - Reader (the CALLING thread — callers already run this on a spawned
///   thread): `for message in daemon_rx` forwards each value straight to
///   `handle_daemon_message`; the loop ends when the daemon drops its sender
///   (channel close IS the EOF — mapped to the same clean `Ok(())` the unix
///   path returns on EOF, so `UiEvent::ReaderClosed` semantics are identical).
/// - Writer: a dedicated thread draining `from_ui` into `daemon_tx` — an
///   event-driven `select_biased!` over `from_ui` (drained first) and the
///   internal writer-shutdown channel (the same structure the socket modes
///   use). Dropping `from_ui` ends the writer, which drops the last
///   client-side `daemon_tx` end — the daemon's embedded connection sees
///   channel close (= EOF) and runs its normal cleanup.
///
/// Shutdown is COOPERATIVE in-process (a deliberate difference from the TCP
/// path, where `Shutdown::Both` force-kills the socket): an external
/// `shutdown_rx` signal stops the writer thread via the internal
/// writer-shutdown channel, but the daemon-side channel can never be
/// force-closed from here — the reader ends when the EMBEDDED DAEMON closes
/// its end (its `EmbeddedDaemon::shutdown()` delivers `ShuttingDown` as a
/// value, then closes the channel, which unblocks this reader exactly the
/// way a daemon EOF unblocks the socket reader).
pub(super) fn run_daemon_connection_in_process(
    daemon_tx: CrossbeamSender<ClientMessage>,
    daemon_rx: CrossbeamReceiver<DaemonMessage>,
    mut handle_daemon_message: impl FnMut(DaemonMessage),
    from_ui: CrossbeamReceiver<ClientMessage>,
    shutdown_rx: Option<CrossbeamReceiver<()>>,
) {
    info!("serving in-process (embedded daemon) connection");

    // Internal writer-shutdown channel — same shape as the socket modes: the
    // reader signals it when it finishes, and the optional external shutdown
    // signal fans into it too (cooperative stop for the writer only; see the
    // function doc for why the reader cannot be force-closed). `bounded(1)`
    // crossbeam control-plane channel: the reader sends at most one stop
    // signal, so capacity 1 never blocks it (workspace channel convention).
    let (writer_shutdown_tx, writer_shutdown_rx) = crossbeam_channel::bounded::<()>(1);

    // Writer thread: drains `from_ui` into `daemon_tx` via an event-driven
    // `select_biased!` — a crossbeam send of a value replaces the socket
    // write, and a failed send (all daemon-side receivers dropped) is the
    // broken-pipe analogue.  The message arm is FIRST (biased) so queued UI
    // messages are drained before a simultaneous stop is honoured.
    // The in-process writer thread forwards each already-framed
    // [`ClientMessage`] produced by the front-end's
    // [`crate::pending::PendingReplies`] table (which allocated the request id
    // so it can correlate the reply) into the embedded daemon's channel.
    let writer_handle = thread::spawn(move || {
        loop {
            crossbeam_channel::select_biased! {
                recv(from_ui) -> msg => match msg {
                    Ok(framed) => {
                        if daemon_tx.send(framed).is_err() {
                            warn!(
                                "writer thread: daemon receiver gone (embedded connection closed)"
                            );
                            break;
                        }
                    }
                    // `from_ui` closed: the UI is done sending. Dropping
                    // `daemon_tx` (owned by this thread) is what delivers EOF
                    // to the daemon's embedded connection.
                    Err(_) => break,
                },
                // The reader finished (or the external shutdown fired): stop.
                recv(writer_shutdown_rx) -> _ => break,
            }
        }
    });

    // Optional external shutdown: no socket exists to `Shutdown::Both`, so
    // the signal only stops the WRITER (cooperative; documented above).
    if let Some(shutdown_rx) = shutdown_rx {
        // Clone the sender BEFORE the closure: the reader tail below keeps
        // its own end to stop the writer when the loop ends.
        let writer_tx = writer_shutdown_tx.clone();
        thread::spawn(move || {
            // `recv` returns `Err` when the sender is dropped WITHOUT sending;
            // either way the external shutdown is over, so stop the writer —
            // mirroring the socket modes, which force-close the stream when
            // their shutdown sender is dropped too. A send failure is benign:
            // the reader may have finished first and dropped its own
            // writer-shutdown sender side.
            let _ = shutdown_rx.recv();
            let _ = writer_tx.send(());
        });
    }

    // Reader loop: the calling thread. Channel close = clean EOF, exactly the
    // `Ok(())` the unix path returns on `UnexpectedEof`.
    for message in daemon_rx {
        handle_daemon_message(message);
    }
    info!("daemon reader loop ended normally (embedded daemon closed its channel)");

    // Signal the writer to stop and wait for it (same tail as the unix path).
    let _ = writer_shutdown_tx.send(());
    let _ = writer_handle.join();
}
