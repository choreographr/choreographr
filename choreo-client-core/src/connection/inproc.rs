//! The embedded, in-process channel transport (the
//! [`super::ConnectionMode::InProcess`] mode).
//!
//! Messages travel as Rust VALUES over crossbeam channels — no codec, no
//! socket — and channel close IS the EOF in both directions, exactly like the
//! daemon-side embedded path.

use crate::error::ClientError;
use choreo_proto::{ClientMessage, DaemonMessage};
use crossbeam_channel::{Receiver as CrossbeamReceiver, Sender as CrossbeamSender};
use tracing::info;

use super::serve::{CloseHandle, ConnectionSink, ConnectionSource, Inbound, serve};

/// The daemon→client channel end, read as a message source: channel close IS
/// the EOF.
impl ConnectionSource for CrossbeamReceiver<DaemonMessage> {
    fn next_message(&mut self) -> Inbound {
        match self.recv() {
            Ok(message) => Inbound::Message(message),
            // The embedded daemon dropped its sender: the same clean EOF a
            // socket read reports as `UnexpectedEof`.
            Err(_) => Inbound::Closed,
        }
    }
}

/// The client→daemon channel end as an outbound sink.
///
/// The sender lives behind an `Option` because dropping it is the channel
/// analogue of closing the socket: with no client-side sender left, the
/// daemon's embedded connection sees channel close (= EOF) and runs its normal
/// cleanup. [`ConnectionSink::close`] is therefore a `take`.
struct ChannelSink(Option<CrossbeamSender<ClientMessage>>);

impl ConnectionSink for ChannelSink {
    fn send_message(&mut self, message: &ClientMessage) -> Result<(), String> {
        let sender = self
            .0
            .as_ref()
            .ok_or_else(|| "in-process daemon channel is closed".to_string())?;
        // A closed channel here is the embedded analogue of a broken pipe: every
        // daemon-side receiver has been dropped.
        sender
            .send(message.clone())
            .map_err(|_| "in-process daemon receiver gone (embedded connection closed)".to_string())
    }

    fn close(&mut self) {
        // Dropping the last client-side sender delivers EOF to the daemon.
        self.0 = None;
    }

    fn close_handle(&self) -> Option<CloseHandle> {
        // The daemon owns the other end of this channel; there is no socket to
        // close, so an external shutdown cannot force-close the connection.
        None
    }
}

/// Serve a connection over an in-process channel pair.
///
/// Runs through the shared [`serve`] loop, so the reader (the CALLING thread —
/// callers already run this on a spawned thread) and the writer thread behave
/// exactly as they do for a socket: the writer drains `from_ui` (biased, so
/// queued messages go first) and the loop ends when the daemon drops its sender
/// (channel close IS the EOF — the same clean `Ok(())` the unix path returns on
/// EOF, so `UiEvent::ReaderClosed` semantics are identical).
///
/// Shutdown is COOPERATIVE in-process, the one deliberate difference from the
/// socket transports: an external `shutdown_rx` signal stops the writer thread
/// only, because the daemon-side channel can never be force-closed from here
/// ([`ChannelSink::close_handle`] returns `None`). The reader ends when the
/// EMBEDDED DAEMON closes its end — its `EmbeddedDaemon::shutdown()` delivers
/// `ShuttingDown` as a value, then closes the channel, which unblocks this
/// reader exactly the way a daemon EOF unblocks the socket reader.
///
/// # Errors
///
/// Returns [`ClientError`] only for a transport-level failure; an in-process
/// channel pair has none today, so this is always `Ok(())` — the signature
/// matches every other transport so the mode dispatch can return it directly.
pub(super) fn run_daemon_connection_in_process(
    daemon_tx: CrossbeamSender<ClientMessage>,
    daemon_rx: CrossbeamReceiver<DaemonMessage>,
    handle_daemon_message: impl FnMut(DaemonMessage),
    from_ui: CrossbeamReceiver<ClientMessage>,
    shutdown_rx: Option<CrossbeamReceiver<()>>,
) -> Result<(), ClientError> {
    info!("serving in-process (embedded daemon) connection");
    serve(
        daemon_rx,
        ChannelSink(Some(daemon_tx)),
        handle_daemon_message,
        from_ui,
        shutdown_rx,
    )
}
