//! The serve loop every transport runs: one writer thread draining the UI's
//! outgoing channel, the reader loop on the calling thread, and the teardown
//! that joins them.
//!
//! A client connection is one long-lived exchange in two directions, and every
//! transport needs the same four parts: an outbound [sink](ConnectionSink), an
//! inbound [source](ConnectionSource), a writer thread, and a shutdown path.
//! Only the two ends differ per transport — a framed unix socket, an encrypted
//! Noise stream, an in-process channel pair — so [`serve`] owns the shape and
//! the transports supply the ends. Extracting it is what keeps the invariants
//! below in ONE place instead of drifting across three hand-rolled copies:
//!
//! 1. The writer drains before it stops: its `select_biased!` lists the message
//!    arm first, so a queue that already holds messages is emptied before a
//!    simultaneous stop signal is honoured.
//! 2. A failed send [`ConnectionSink::close`]s the transport, which unblocks
//!    the reader's blocking read. Without it the reader would wait forever on a
//!    transport that can never send again.
//! 3. An external shutdown signal means different things per transport, so the
//!    hook branches on [`ConnectionSink::close_handle`]: a socket is force-closed
//!    (unblocking the reader), while a transport that cannot be closed from
//!    outside — the embedded channel — only has its writer stopped.
//! 4. The close handle is taken and the writer-stop sender cloned BEFORE the
//!    sink moves into the writer thread and before the reader tail keeps its own
//!    end, so neither is borrowed from the thread that owns it.
//!
//! The writer's two time bounds (`WRITER_STALL` and `message_budget`) are one
//! policy shared with the daemon's Noise writer, so they live in `choreo-proto`
//! beside the bounded-write loop; the connection applies them, the transport only
//! carries the bytes.

use crate::error::ClientError;
use choreo_proto::{ClientMessage, DaemonMessage, UnixStream};
use crossbeam_channel::Receiver;
use std::net::{Shutdown, TcpStream};
use std::thread;
use tracing::{error, warn};

/// A handle that can close this connection from another thread.
///
/// Both socket transports hand out a `try_clone`d stream for it: closing a
/// duplicate closes the connection itself, so whichever thread holds this can
/// unblock the reader.
pub(super) enum CloseHandle {
    /// A TCP (Noise) connection's underlying stream.
    Tcp(TcpStream),
    /// A local-domain unix-socket connection's stream.
    Unix(UnixStream),
}

impl CloseHandle {
    /// Close the connection. Best-effort: a failure means the socket is already
    /// gone, which is the state this call exists to reach.
    pub(super) fn close(self) {
        match self {
            Self::Tcp(stream) => {
                let _ = stream.shutdown(Shutdown::Both);
            }
            Self::Unix(stream) => {
                let _ = stream.shutdown(Shutdown::Both);
            }
        }
    }
}

/// This connection's outbound sink — the single-writer contract.
///
/// Exactly one instance is moved into the writer thread, so every outbound
/// message goes through it in order. A transport implements three things: how to
/// put one message on the wire, how to tear the transport down, and whether
/// another thread may do that instead.
pub(super) trait ConnectionSink {
    /// Put one already-framed message on the wire.
    ///
    /// The `String` is a display-ready description of the failure; the writer
    /// logs it and closes the transport, because a sink that cannot send is not
    /// coming back.
    fn send_message(&mut self, msg: &ClientMessage) -> Result<(), String>;

    /// Unblock this connection's reader: close the socket, or drop the channel
    /// end.
    fn close(&mut self);

    /// A handle another thread can close, or `None` when this transport cannot
    /// be force-closed from outside (the embedded channel).
    fn close_handle(&self) -> Option<CloseHandle>;
}

/// What one inbound read produced.
///
/// The message travels by VALUE on purpose: this value is created and matched
/// immediately in [`serve_source`], so its size is one transient stack slot,
/// whereas boxing it (the lint's suggestion) would add a heap allocation to
/// every inbound message on the connection's hot path.
#[expect(clippy::large_enum_variant)]
pub(super) enum Inbound {
    /// A decoded message to hand to the caller's handler.
    Message(DaemonMessage),
    /// A per-message decode failure: the framing is length-prefixed, so the
    /// stream is still in sync — log and carry on.
    Skip,
    /// Clean EOF: peer close, or channel close.
    Closed,
    /// The stream is gone or broken.
    Fatal(ClientError),
}

/// This connection's inbound source — the single-reader contract.
///
/// Titled after [`Inbound`] rather than a read-like name on purpose: a method
/// called `recv` would shadow the `crossbeam_channel::Receiver` inherent method
/// in the embedded transport's impl and recurse into itself.
pub(super) trait ConnectionSource {
    /// Read the next inbound event.
    fn next_message(&mut self) -> Inbound;
}

/// The writer thread's body: drain `from_ui` into `writer` until either channel
/// closes or a send fails.
///
/// The `select_biased!` lists the message arm FIRST so a queue that already
/// holds messages is fully drained before a simultaneous stop is honoured — a
/// connection that is being torn down never drops outbound messages that were
/// already accepted from the UI.
///
/// The two receivers are borrowed: the writer thread's closure owns them for the
/// thread's whole life, and nothing here hands them on.
pub(super) fn writer_loop<W: ConnectionSink>(
    mut writer: W,
    from_ui: &Receiver<ClientMessage>,
    stop: &Receiver<()>,
) {
    loop {
        crossbeam_channel::select_biased! {
            recv(from_ui) -> msg => match msg {
                Ok(message) => {
                    if let Err(error) = writer.send_message(&message) {
                        warn!(%error, "writer thread send failed; closing the connection");
                        // A failed send means the transport is unusable, so
                        // close it: that is what unblocks the reader's blocking
                        // read, and therefore what ends the connection instead
                        // of leaving the reader waiting on a dead transport.
                        writer.close();
                        break;
                    }
                }
                // `from_ui` closed: the UI is done sending.
                Err(_) => break,
            },
            // The reader finished (or the external shutdown fired): stop.
            recv(stop) -> _ => break,
        }
    }
}

/// Serve an established connection: spawn the writer thread, install the
/// optional shutdown hook, run the reader loop on the calling thread, then stop
/// and join the writer.
///
/// Returns the reader's result — `Ok(())` on a clean EOF, the reader's error
/// when the stream broke. The writer never contributes an error of its own: a
/// failed send closes the transport, and the reader then reports what it sees.
pub(super) fn serve<S, W>(
    source: S,
    writer: W,
    handle: impl FnMut(DaemonMessage),
    from_ui: Receiver<ClientMessage>,
    shutdown_rx: Option<Receiver<()>>,
) -> Result<(), ClientError>
where
    S: ConnectionSource,
    W: ConnectionSink + Send + 'static,
{
    // Taken BEFORE the sink moves into the writer thread (`close_handle` needs
    // `&self`, and the shutdown hook needs its own copy of the socket). Only
    // when there IS a hook to hand it to: an unrequested duplicate socket would
    // otherwise sit open for the whole life of the connection.
    let close_handle = shutdown_rx
        .as_ref()
        .and_then(|_shutdown| writer.close_handle());

    // Channel to stop the writer when the reader finishes. `bounded(1)` is a
    // crossbeam control-plane channel: the reader sends at most one stop signal,
    // so capacity 1 never blocks it.
    let (stop_tx, stop_rx) = crossbeam_channel::bounded::<()>(1);
    // A one-shot "the connection finished" signal, so the shutdown watcher below
    // exits — and drops its cloned socket fd — when the reader ends WITHOUT an
    // external shutdown ever firing. Without it the watcher thread and its fd
    // would outlive the connection until the shutdown sender is dropped.
    let (done_tx, done_rx) = crossbeam_channel::bounded::<()>(1);

    // The writer thread is the connection's single send site. Every outbound
    // value is already a framed `ClientMessage`: the front-end owns a
    // `PendingReplies` table, allocates the per-connection request id itself, and
    // forwards the frame here so it can resolve the reply by that id.
    let writer_handle = thread::spawn(move || writer_loop(writer, &from_ui, &stop_rx));

    if let Some(shutdown_rx) = shutdown_rx {
        if let Some(close_handle) = close_handle {
            // A socket transport can be force-closed from another thread, so an
            // external shutdown does exactly what a failed write does: close the
            // socket, which unblocks the reader immediately. `select!` also exits
            // on `done`, so a connection that ended on its own does not leave this
            // thread (and the duplicated fd) parked until the sender drops.
            thread::spawn(move || {
                crossbeam_channel::select! {
                    recv(shutdown_rx) -> _ => close_handle.close(),
                    recv(done_rx) -> _ => {}
                }
            });
        } else {
            // No force-close exists (the embedded channel), so an external
            // shutdown can only stop the writer: the daemon's channel end
            // belongs to the daemon, and closing it from here would be reaching
            // into another component's transport.
            //
            // Clone the sender BEFORE the reader tail keeps its own end: the
            // reader still has to be able to stop the writer when its own loop
            // ends.
            let writer_stop = stop_tx.clone();
            thread::spawn(move || {
                crossbeam_channel::select! {
                    recv(shutdown_rx) -> _ => {
                        let _ = writer_stop.send(());
                    }
                    recv(done_rx) -> _ => {}
                }
            });
        }
    }

    let result = serve_source(source, handle);
    // Signal the writer to stop and wait for it, so no send is in flight when
    // this returns.
    let _ = stop_tx.send(());
    let _ = writer_handle.join();
    // Let the shutdown watcher (if any) exit now that the connection is over. A
    // panicking reader drops `done_tx` instead, which disconnects `done_rx` and
    // wakes the watcher just the same.
    let _ = done_tx.send(());
    result
}

/// The reader half alone, for the public `run_daemon_reader`.
///
/// Classifies the three non-message outcomes once for every source: a decode
/// skip is logged and carrying on, a clean EOF ends the loop successfully, and a
/// fatal error is logged and returned.
pub(super) fn serve_source<S: ConnectionSource>(
    mut source: S,
    mut handle: impl FnMut(DaemonMessage),
) -> Result<(), ClientError> {
    loop {
        match source.next_message() {
            Inbound::Message(message) => handle(message),
            Inbound::Skip => {}
            Inbound::Closed => break,
            Inbound::Fatal(failure) => {
                error!(%failure, "daemon reader ended on a transport error");
                return Err(failure);
            }
        }
    }
    tracing::info!("daemon reader loop ended normally");
    Ok(())
}
