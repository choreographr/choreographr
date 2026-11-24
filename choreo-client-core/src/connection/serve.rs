//! The serve loop every transport runs: one writer thread draining the UI's
//! outgoing channel, the reader loop on the calling thread, and the teardown
//! that stops the writer and waits for it under a bound.
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
//! 5. The teardown's wait for the writer is BOUNDED. The writer announces its
//!    completion over its own one-shot channel, and the reader waits for that
//!    signal under [`WRITER_JOIN_GRACE`], so a writer wedged inside a send is
//!    detached rather than joined unboundedly. The grace is strictly greater than
//!    [`choreo_proto::CONNECTION_STALL`], and that ordering is load-bearing: the
//!    wait covers a writer that may be mid-syscall for up to the stall, so an equal
//!    grace would expire in exactly the worst case the wait exists to cover and
//!    detach a writer that was about to exit cleanly. The margin itself lives in
//!    `choreo-proto`'s I/O policy beside the stall it derives from, so neither end
//!    of a connection restates the relation.
//!
//! The writer's two time bounds (`CONNECTION_STALL` and `message_budget`) are one
//! policy shared with the daemon's writers, so they live in `choreo-proto`'s I/O
//! policy module rather than beside either loop; the connection applies them, the
//! transport only carries the bytes. The stall is the connection's — the reader
//! applies it too — while the rate-derived budget is writer-only, because a reader
//! cannot know a message's total size until it has all of it. That budget has no
//! ceiling, so invariant 5 is what keeps a large frame on a slow-but-alive link
//! from holding a reader that has already finished.

use crate::error::ClientError;
use choreo_proto::{ClientMessage, DaemonMessage, UnixStream, WRITER_JOIN_GRACE};
use crossbeam_channel::{Receiver, RecvTimeoutError};
use std::net::{Shutdown, TcpStream};
use std::thread;
use std::time::Duration;
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
/// the writer and wait for it under [`WRITER_JOIN_GRACE`] — bounded, so a writer
/// wedged inside a send is detached instead of waited on indefinitely.
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
    serve_with_grace(
        source,
        writer,
        handle,
        from_ui,
        shutdown_rx,
        WRITER_JOIN_GRACE,
    )
}

/// [`serve`] with the teardown's writer wait exposed as a parameter.
///
/// The same loop in every other respect; the grace is injectable so a test can
/// shrink it to a few milliseconds instead of waiting out [`WRITER_JOIN_GRACE`].
fn serve_with_grace<S, W>(
    source: S,
    writer: W,
    handle: impl FnMut(DaemonMessage),
    from_ui: Receiver<ClientMessage>,
    shutdown_rx: Option<Receiver<()>>,
    grace: Duration,
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
    // A one-shot "the writer thread finished" signal. Kept separate from `done_tx`
    // below: that one exists to end the shutdown watcher, and a writer signal on
    // it would end the watcher while the reader is still running.
    let (writer_done_tx, writer_done_rx) = crossbeam_channel::bounded::<()>(1);
    // A one-shot "the connection finished" signal, so the shutdown watcher below
    // exits — and drops its cloned socket fd — when the reader ends WITHOUT an
    // external shutdown ever firing. Without it the watcher thread and its fd
    // would outlive the connection until the shutdown sender is dropped.
    let (done_tx, done_rx) = crossbeam_channel::bounded::<()>(1);

    // The writer thread is the connection's single send site. Every outbound
    // value is already a framed `ClientMessage`: the front-end owns a
    // `PendingReplies` table, allocates the per-connection request id itself, and
    // forwards the frame here so it can resolve the reply by that id.
    let writer_handle = thread::spawn(move || {
        writer_loop(writer, &from_ui, &stop_rx);
        // Announce completion so the reader's teardown can bound its wait instead
        // of blocking on a writer that may be inside a size-scaled socket write. A
        // panic drops this sender instead, which disconnects the receiver and
        // reads as "finished" there too.
        let _ = writer_done_tx.send(());
    });

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
    // Signal the writer to stop, then wait for it, so no send is in flight when
    // this returns — but only for the grace. The wait is a channel event, not a
    // poll: the writer sends its completion on every exit path, and a panic drops
    // the sender, which surfaces as `Disconnected`.
    let _ = stop_tx.send(());
    match writer_done_rx.recv_timeout(grace) {
        // The writer finished: the join returns immediately, and taking it reaps
        // the thread rather than leaving a finished handle behind.
        Ok(()) => {
            let _ = writer_handle.join();
        }
        // Still inside a send after the whole grace. Detach: dropping the handle
        // lets the thread finish (and exit) on its own. The connection is over
        // either way, and the writer's own stall reaps a wedged peer within
        // `CONNECTION_STALL`; only a SLOW-BUT-ALIVE peer reaches this arm — which
        // is exactly the transfer the uncapped per-message budget exists to allow,
        // so waiting it out here would hold this caller for as long as the message
        // takes.
        Err(RecvTimeoutError::Timeout) => {
            warn!(
                grace = ?grace,
                "writer thread is still sending after the teardown grace; detaching it"
            );
            drop(writer_handle);
        }
        // The writer thread panicked, so no completion was ever sent: join anyway
        // so the panic is not silently swallowed.
        Err(RecvTimeoutError::Disconnected) => {
            let _ = writer_handle.join();
        }
    }
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

#[cfg(test)]
mod tests {
    use super::{CloseHandle, ConnectionSink, ConnectionSource, Inbound, serve_with_grace};
    use choreo_proto::{ClientMessage, ClientMessageType, WRITER_JOIN_GRACE};
    use crossbeam_channel::Receiver;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::Duration;

    /// Grace for the DETACH test: short enough that the pinned writer is abandoned
    /// promptly. The joined-writer test uses the production grace instead — it must
    /// never be the grace, rather than the join, that decides its outcome.
    const TEST_GRACE: Duration = Duration::from_millis(20);

    /// A reader that ends on its very first poll.
    ///
    /// The teardown is the subject here, so the reader contributes no timing of
    /// its own: `Inbound::Closed` ends `serve_source` immediately and puts the
    /// calling thread straight into the writer wait.
    struct EndedSource;

    impl ConnectionSource for EndedSource {
        fn next_message(&mut self) -> Inbound {
            Inbound::Closed
        }
    }

    /// A sink that records its own destruction.
    ///
    /// The writer thread owns the sink, so the flag can only be set by that
    /// thread running to its end — which is exactly what "joined" means. It is a
    /// single bit with no payload, in the spirit of the workspace's lock-free
    /// single-bit flags.
    struct DropWatchedSink {
        dropped: Arc<AtomicBool>,
    }

    impl ConnectionSink for DropWatchedSink {
        fn send_message(&mut self, _message: &ClientMessage) -> Result<(), String> {
            Ok(())
        }

        fn close(&mut self) {}

        fn close_handle(&self) -> Option<CloseHandle> {
            // A fake sink has no socket to duplicate.
            None
        }
    }

    impl Drop for DropWatchedSink {
        fn drop(&mut self) {
            self.dropped.store(true, Ordering::SeqCst);
        }
    }

    /// A sink whose send does not return until the test releases it, which parks
    /// the writer thread inside `send_message` — the one state the bounded wait
    /// exists to cover.
    struct WedgedSink {
        release: Receiver<()>,
    }

    impl ConnectionSink for WedgedSink {
        fn send_message(&mut self, _message: &ClientMessage) -> Result<(), String> {
            // Pairs with the test's send; returns immediately on disconnect, so a
            // test that has already finished cannot hang the writer here.
            let _ = self.release.recv();
            Ok(())
        }

        fn close(&mut self) {}

        fn close_handle(&self) -> Option<CloseHandle> {
            None
        }
    }

    /// A writer that has finished is joined before `serve_with_grace` returns.
    ///
    /// The queue stays open for the whole call, so the writer is alive and parked
    /// in its `select_biased!` when the reader ends: nothing but the join can have
    /// dropped the sink by the time the call returns. The grace is the production
    /// one — a healthy writer signals completion immediately, so it costs nothing
    /// here and a loaded scheduler cannot turn the run into a spurious detach.
    #[test]
    fn a_finished_writer_is_joined_before_serve_returns() {
        let dropped = Arc::new(AtomicBool::new(false));
        let (_from_ui_tx, from_ui_rx) = crossbeam_channel::unbounded::<ClientMessage>();

        let result = serve_with_grace(
            EndedSource,
            DropWatchedSink {
                dropped: Arc::clone(&dropped),
            },
            |_| {},
            from_ui_rx,
            None,
            WRITER_JOIN_GRACE,
        );

        assert!(
            result.is_ok(),
            "a clean reader EOF is success, got {result:?}"
        );
        assert!(
            dropped.load(Ordering::SeqCst),
            "the writer thread must have been joined, so its sink must already be dropped"
        );
    }

    /// A writer still inside a send when the reader ends is detached once the
    /// grace expires.
    ///
    /// The assertion IS that the call returns: an unbounded join would park here
    /// until the release below, which the test only sends afterwards.
    #[test]
    fn a_writer_stuck_in_a_send_is_detached_when_the_grace_expires() {
        let (from_ui_tx, from_ui_rx) = crossbeam_channel::unbounded::<ClientMessage>();
        // One queued message, so the writer is inside `send_message` — not parked
        // on an empty queue — when the reader ends and the wait begins. The biased
        // select takes the message arm first, so the stop signal cannot preempt it.
        from_ui_tx
            .send(ClientMessage::request(0, ClientMessageType::Ping))
            .expect("the queue is open");

        // Zero capacity: the send pairs with the writer's blocked receive, so the
        // release below is what lets the detached thread finish.
        let (release_tx, release_rx) = crossbeam_channel::bounded::<()>(0);
        let result = serve_with_grace(
            EndedSource,
            WedgedSink {
                release: release_rx,
            },
            |_| {},
            from_ui_rx,
            None,
            TEST_GRACE,
        );
        assert!(
            result.is_ok(),
            "a clean reader EOF is success, got {result:?}"
        );

        // Let the pinned writer go so the detached thread exits here rather than
        // outliving the test.
        let _ = release_tx.send(());
        // Closing the queue as well means the writer's next loop iteration ends
        // whichever arm it takes.
        drop(from_ui_tx);
    }
}
