use std::io;

use crate::shared::ProviderError;

/// Capacity of the bounded channel between the SSE reader thread and the
/// consumer.
///
/// Bounding the channel applies backpressure: a reader that produces events
/// faster than the consumer can JSON-parse them blocks on `send` instead of
/// buffering an unbounded number of events in memory.  The reader only ever
/// blocks on `send` while the consumer is between `recv` calls, so this
/// cannot deadlock; on cancellation the consumer's `SseStream` is dropped,
/// which drops the receiver and fails any in-flight `send`.
pub(crate) const SSE_CHANNEL_CAPACITY: usize = 64;

/// Messages forwarded from the dedicated SSE reader thread to the consumer.
///
/// The `Err` variant carries the reader's `io::Error` so a mid-stream socket
/// failure surfaces to the caller exactly as it would if the read had
/// happened inline.
#[derive(Debug)]
pub(crate) enum SseStreamMsg<T> {
    /// A parsed SSE event was produced.
    Event(T),
    /// The stream ended cleanly (`Ok(None)` from the underlying reader).
    End,
    /// The underlying read failed.
    Err(io::Error),
}

/// Handle to a spawned SSE reader thread: the parsed-event channel plus the
/// thread's abort signal and join handle.
///
/// Dropping the handle signals the reader thread to stop at its next loop
/// boundary (see [`SseStream::drop`]) and reaps the thread immediately if it
/// has already exited.
pub(crate) struct SseStream<T> {
    rx: crossbeam_channel::Receiver<SseStreamMsg<T>>,
    handle: Option<std::thread::JoinHandle<()>>,
    /// One-way abort signal to the reader thread: sending a message asks it
    /// to stop at its next loop boundary instead of parsing the remainder of
    /// a live stream.  Implemented as a channel message rather than a shared
    /// atomic so cross-thread communication stays channel-based (per the
    /// repo's thread-communication rules); the unbounded channel never blocks
    /// `Drop`.
    abort_tx: crossbeam_channel::Sender<()>,
    /// Hard wall-clock deadline for this response's whole attempt, supplied
    /// by the caller (see [`crate::retry::AttemptDeadline`]); `None` disables.
    /// ureq's own `timeout_global` cannot hard-cap a stream that keeps
    /// trickling keep-alive bytes faster than its ~1 s minimum socket
    /// timeout, so this consumer-side check is the real backstop: it fires on
    /// every poll regardless of incoming data.
    deadline: Option<std::time::Instant>,
}

impl<T> Drop for SseStream<T> {
    fn drop(&mut self) {
        // Signal the reader thread to stop at its next loop boundary.  If it
        // is blocked mid-`read()` it cannot react until the socket read
        // unblocks (bounded by the agent's idle/global timeouts), but it
        // stops immediately at its next opportunity rather than parsing the
        // remainder of the stream.  The blocked-read case is a documented
        // limitation: aborting the read itself would require closing the
        // underlying connection, which ureq's `Body` API does not expose.
        let _ = self.abort_tx.send(());
        if let Some(handle) = self.handle.take()
            && handle.is_finished()
        {
            // Already exited — reap now (no blocking).
            let _ = handle.join();
        }
        // Otherwise dropping the JoinHandle detaches the thread; the abort
        // signal stops it at the next opportunity, and process exit reaps it
        // if it is still blocked in a provider read.
    }
}

/// Spawn a dedicated thread that runs the blocking SSE read loop and
/// forwards each parsed event through a crossbeam channel.
///
/// `deadline` is the hard wall-clock deadline for this response's whole
/// attempt (body read included); `None` disables it.  It is computed by the
/// caller before the request is sent (see [`crate::retry::AttemptDeadline`]),
/// so the consumer-side check spans DNS → connect → headers → body.
///
/// # Why a thread at all
///
/// `SseReader::next_event()` blocks inside `BufReader::read()` on the socket.
/// If the provider stalls (or trickles keep-alive bytes that never form a
/// complete event), that read can block indefinitely — and a loop that only
/// checks `cancel_rx` *between* reads would never see the user's Escape.
/// Moving the read onto its own thread lets the consumer `select!` on the
/// event channel, the cancellation channel, and the deadline timer at once,
/// so Escape (and deadline expiry) are noticed the moment they happen
/// instead of on a poll tick.
///
/// # Reader-thread lifetime
///
/// The returned [`SseStream`] carries an abort signal that the reader thread
/// checks at every loop boundary, so a cancelled or dropped stream stops the
/// thread as soon as it is not blocked inside a socket `read()`.  If the
/// thread is mid-`read()` when the consumer bails it lingers until the read
/// unblocks, but it holds no locks or shared state (the `Reader` is moved
/// in), is bounded by the agent's idle/global timeouts, and dies with the
/// process.
pub(crate) fn spawn_sse_reader<T, F>(
    mut next: F,
    deadline: Option<std::time::Instant>,
) -> SseStream<T>
where
    T: Send + 'static,
    F: FnMut() -> io::Result<Option<T>> + Send + 'static,
{
    let (tx, rx) = crossbeam_channel::bounded(SSE_CHANNEL_CAPACITY);
    let (abort_tx, abort_rx) = crossbeam_channel::unbounded::<()>();
    // Per-attempt deadline supplied by the caller (see
    // `retry::AttemptDeadline`): it is armed *before* the request is sent and
    // re-armed on each retry, so it covers the whole attempt (DNS → connect →
    // headers → body) rather than just this body read.
    tracing::trace!(?deadline, "spawning SSE reader thread");
    let handle = std::thread::spawn(move || {
        loop {
            // Abort check at the loop boundary: the consumer cancelling (or
            // dropping the stream) must stop the thread as soon as it is not
            // blocked inside a socket read.
            if abort_rx.try_recv().is_ok() {
                tracing::debug!("SSE reader thread aborted");
                return;
            }
            match next() {
                Ok(Some(item)) => {
                    // Bounded send: blocks only while the consumer is between
                    // `recv` calls, which is the intended backpressure.  If
                    // the consumer has dropped the receiver (cancelled or
                    // bailed), the send fails — nothing left to do, exit.
                    if tx.send(SseStreamMsg::Event(item)).is_err() {
                        tracing::debug!("SSE reader thread exiting: consumer dropped the channel");
                        return;
                    }
                }
                Ok(None) => {
                    tracing::trace!("SSE stream ended; forwarding End");
                    // The consumer is always draining when it is alive (it
                    // returns at End), so a bounded send here cannot block
                    // indefinitely; ignore an error if the consumer is gone.
                    let _ = tx.send(SseStreamMsg::End);
                    return;
                }
                Err(e) => {
                    tracing::debug!(error = %e, "SSE reader error; forwarding to consumer");
                    let _ = tx.send(SseStreamMsg::Err(e));
                    return;
                }
            }
        }
    });
    SseStream {
        rx,
        handle: Some(handle),
        abort_tx,
        deadline,
    }
}

/// Translate one message from the reader channel into the consumer's result,
/// mapping a dropped sender (reader died without a final message) to an Io
/// error.  Shared by both `select!` branches in [`recv_sse_event`].
fn handle_sse_msg<T>(
    msg: Result<SseStreamMsg<T>, crossbeam_channel::RecvError>,
) -> Result<Option<T>, ProviderError> {
    match msg {
        Ok(SseStreamMsg::Event(item)) => Ok(Some(item)),
        Ok(SseStreamMsg::End) => Ok(None),
        Ok(SseStreamMsg::Err(e)) => Err(ProviderError::Io(e)),
        Err(_) => {
            // The reader thread ended without sending End or Err — its
            // closure cannot return normally without sending one of
            // those, so this means the thread died unexpectedly.
            tracing::warn!("SSE reader thread terminated unexpectedly");
            Err(ProviderError::Io(io::Error::other(
                "SSE reader thread terminated unexpectedly",
            )))
        }
    }
}

/// Receive the next SSE event from a spawned reader thread.
///
/// Blocks until one of three things happens: an event (or clean end / error)
/// arrives on the reader channel, a cancellation signal arrives on
/// `cancel_rx`, or the hard wall-clock deadline expires.  Because the reader
/// thread decouples the blocking socket read from this wait, the wait itself
/// can be purely event-driven via `select!` — no polling interval.
///
/// Returns `Ok(None)` on a clean stream end, `Err(ProviderError::Cancelled)`
/// when a cancellation is pending, `Err(ProviderError::DeadlineExceeded)` when
/// the per-attempt wall-clock deadline expires, and
/// `Err(ProviderError::Io(..))` when the reader thread reports a read error
/// or dies unexpectedly.  On cancellation the reader thread's abort signal is
/// sent so it stops at its next loop boundary instead of parsing the
/// remainder of the stream.
pub(crate) fn recv_sse_event<T>(
    sse: &SseStream<T>,
    cancel_rx: Option<&crossbeam_channel::Receiver<()>>,
) -> Result<Option<T>, ProviderError> {
    // A never-ready stand-in channel so the cancellation arm below is always
    // wired up, even when the caller provided no cancellation channel.
    let never_rx = crossbeam_channel::never::<()>();
    let cancel: &crossbeam_channel::Receiver<()> = cancel_rx.unwrap_or(&never_rx);

    // Hard wall-clock deadline: fires even when a provider keeps trickling
    // keep-alive bytes (ureq's global timeout is floored at ~1 s per
    // socket read, so sub-second trickles can evade it).  Signal the
    // reader thread to stop, then surface a dedicated timeout error so
    // callers can distinguish a deadline expiry from a genuine socket
    // failure (and treat it as non-retryable by construction).
    if let Some(deadline) = sse.deadline
        && std::time::Instant::now() >= deadline
    {
        let _ = sse.abort_tx.send(());
        tracing::warn!("SSE stream exceeded total request deadline");
        return Err(ProviderError::DeadlineExceeded);
    }

    // Biased selection with cancellation first: a cancel already queued when
    // this call begins is selected deterministically (the biased fast path
    // scans arms in order), even if the reader has also queued events; a
    // cancel that lands mid-block merely *tends* to win over a simultaneous
    // event, and a lost race just means the event is delivered and the
    // cancel is observed on the next call.  (The cancel sender outlives the
    // worker, so the arm cannot spuriously fire on disconnect during a live
    // stream.)
    match sse.deadline {
        // With a deadline, also wait on an exact timer for the remaining
        // budget — the timer, not a poll interval, bounds the wait.
        Some(deadline) => {
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            crossbeam_channel::select_biased! {
                recv(cancel) -> _ => {
                    let _ = sse.abort_tx.send(());
                    tracing::debug!("SSE stream cancelled by user");
                    Err(ProviderError::Cancelled)
                }
                recv(sse.rx) -> msg => handle_sse_msg(msg),
                recv(crossbeam_channel::after(remaining)) -> _ => {
                    let _ = sse.abort_tx.send(());
                    tracing::warn!("SSE stream exceeded total request deadline");
                    Err(ProviderError::DeadlineExceeded)
                }
            }
        }
        // Without a deadline, only an event or a cancellation can wake
        // this wait — both handled by `select!`, so no timer is needed.
        None => crossbeam_channel::select_biased! {
            recv(cancel) -> _ => {
                let _ = sse.abort_tx.send(());
                tracing::debug!("SSE stream cancelled by user");
                Err(ProviderError::Cancelled)
            }
            recv(sse.rx) -> msg => handle_sse_msg(msg),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── spawn_sse_reader ────────────────────────────────────────────────
    //
    // All tests are deterministic: the closures are pure iterators (no
    // sleeping), and `recv()` simply blocks until the already-queued item
    // arrives, so ordering is guaranteed.

    #[test]
    fn events_then_end_forwarded_in_order() {
        // Three events followed by a clean end, produced by a pure iterator
        // driven inside a closure (each call to `next()` advances one item).
        let mut items = (0..3).map(|i| Ok(Some(i))).chain(std::iter::once(Ok(None)));
        let sse = spawn_sse_reader(move || items.next().unwrap_or(Ok(None)), None);

        assert!(matches!(sse.rx.recv(), Ok(SseStreamMsg::Event(0))));
        assert!(matches!(sse.rx.recv(), Ok(SseStreamMsg::Event(1))));
        assert!(matches!(sse.rx.recv(), Ok(SseStreamMsg::Event(2))));
        assert!(matches!(sse.rx.recv(), Ok(SseStreamMsg::End)));
    }

    #[test]
    fn reader_error_forwarded_as_err() {
        // A reader that immediately fails surfaces the io::Error verbatim.
        // (T is annotated — nothing constrains it when the closure only errs.)
        let mut items = std::iter::once(Err(io::Error::other("boom")));
        let sse: SseStream<i32> = spawn_sse_reader(move || items.next().unwrap_or(Ok(None)), None);
        match sse.rx.recv() {
            Ok(SseStreamMsg::Err(e)) => assert_eq!(e.to_string(), "boom"),
            other => panic!("expected SseStreamMsg::Err, got {other:?}"),
        }
    }

    #[test]
    fn reader_error_after_events_forwarded() {
        // One event, then a failure — both forwarded in order.
        let mut items =
            std::iter::once(Ok(Some(7))).chain(std::iter::once(Err(io::Error::other("late"))));
        let sse = spawn_sse_reader(move || items.next().unwrap_or(Ok(None)), None);
        assert!(matches!(sse.rx.recv(), Ok(SseStreamMsg::Event(7))));
        match sse.rx.recv() {
            Ok(SseStreamMsg::Err(e)) => assert_eq!(e.to_string(), "late"),
            other => panic!("expected SseStreamMsg::Err, got {other:?}"),
        }
    }

    // ── recv_sse_event ──────────────────────────────────────────────────

    #[test]
    fn pre_sent_cancel_returns_cancelled_immediately() {
        // A cancel signal sent BEFORE the call must win on the first
        // iteration, regardless of what the reader produces.  The reader
        // yields forever here; the abort signal armed by the cancellation
        // (plus the receiver being dropped at test end) stops the thread, so
        // nothing leaks.
        let (cancel_tx, cancel_rx) = crossbeam_channel::unbounded::<()>();
        cancel_tx.send(()).unwrap();

        let mut items = std::iter::repeat_with(|| Ok(Some(0)));
        let sse = spawn_sse_reader(move || items.next().unwrap_or(Ok(None)), None);
        let err = recv_sse_event(&sse, Some(&cancel_rx)).unwrap_err();
        assert!(matches!(err, ProviderError::Cancelled));
    }

    #[test]
    fn cancel_stops_reader_thread() {
        // A pre-sent cancel must not only return `Cancelled` but also stop the
        // reader thread.  Deterministic: the abort signal is consumed at the
        // reader's next loop boundary, and `join()` below blocks until the
        // pure-iterator reader exits (no time-based waits).
        let (cancel_tx, cancel_rx) = crossbeam_channel::unbounded::<()>();
        cancel_tx.send(()).unwrap();

        let mut items = std::iter::repeat_with(|| Ok(Some(0)));
        let mut sse = spawn_sse_reader(move || items.next().unwrap_or(Ok(None)), None);
        let err = recv_sse_event(&sse, Some(&cancel_rx)).unwrap_err();
        assert!(matches!(err, ProviderError::Cancelled));
        // Take the join handle out, then drop the stream: dropping the
        // receiver unblocks a reader that is blocked on a bounded `send`
        // (the cancel path stops draining), so the thread is guaranteed to
        // terminate before `join()` returns.
        let handle = sse.handle.take().expect("reader thread handle");
        drop(sse);
        handle.join().expect("reader thread must exit after cancel");
    }

    #[test]
    fn end_maps_to_ok_none() {
        // A reader that ends immediately (Ok(None)) maps to Ok(None), i.e.
        // the loop's "clean break" signal.  (T is annotated — the closure
        // only yields None, so the item type is otherwise unconstrained.)
        let mut items = std::iter::once(Ok(None));
        let sse: SseStream<i32> = spawn_sse_reader(move || items.next().unwrap_or(Ok(None)), None);
        assert_eq!(recv_sse_event(&sse, None).unwrap(), None);
    }

    #[test]
    fn event_maps_to_ok_some() {
        let mut items = std::iter::once(Ok(Some("hello".to_string())));
        let sse = spawn_sse_reader(move || items.next().unwrap_or(Ok(None)), None);
        let item = recv_sse_event(&sse, None).unwrap().expect("event");
        assert_eq!(item, "hello");
    }

    #[test]
    fn disconnected_maps_to_io_error() {
        // A channel whose sender has already been dropped (i.e. the reader
        // thread died without sending End or Err) surfaces as an Io error.
        // Constructed directly rather than via a spawned reader so the test
        // is fully deterministic — no thread, no race with the reader.
        let (tx, rx) = crossbeam_channel::unbounded::<SseStreamMsg<i32>>();
        drop(tx);
        let (abort_tx, _abort_rx) = crossbeam_channel::unbounded::<()>();
        let sse = SseStream {
            rx,
            handle: None,
            abort_tx,
            deadline: None,
        };
        match recv_sse_event(&sse, None) {
            Err(ProviderError::Io(e)) => {
                assert!(e.to_string().contains("terminated unexpectedly"))
            }
            other => panic!("expected Io error on disconnect, got {other:?}"),
        }
    }

    #[test]
    fn expired_deadline_returns_deadline_exceeded() {
        // A stream whose deadline is already in the past must fail immediately
        // with a dedicated `DeadlineExceeded` error (distinct from a genuine
        // socket `Io` error), before the reader produces anything.  Fully
        // deterministic: the past deadline is checked on the first poll, and
        // the channel is never touched.
        let (tx, rx) = crossbeam_channel::bounded::<SseStreamMsg<i32>>(SSE_CHANNEL_CAPACITY);
        let (abort_tx, _abort_rx) = crossbeam_channel::unbounded::<()>();
        let sse = SseStream {
            rx,
            handle: None,
            abort_tx,
            deadline: Some(std::time::Instant::now() - std::time::Duration::from_secs(1)),
        };
        match recv_sse_event(&sse, None) {
            Err(ProviderError::DeadlineExceeded) => {}
            other => panic!("expected DeadlineExceeded, got {other:?}"),
        }
        drop(tx);
    }

    #[test]
    fn future_deadline_allows_events() {
        // A deadline far in the future must not interfere with normal event
        // flow (the deadline check only fires once the deadline has passed).
        let mut items = std::iter::once(Ok(Some("hello".to_string())));
        let sse = spawn_sse_reader(
            move || items.next().unwrap_or(Ok(None)),
            Some(std::time::Instant::now() + std::time::Duration::from_secs(3600)),
        );
        let item = recv_sse_event(&sse, None).unwrap().expect("event");
        assert_eq!(item, "hello");
    }

    #[test]
    fn reader_error_maps_to_io_error() {
        let mut items = std::iter::once(Err(io::Error::other("socket reset")));
        let sse: SseStream<i32> = spawn_sse_reader(move || items.next().unwrap_or(Ok(None)), None);
        match recv_sse_event(&sse, None) {
            Err(ProviderError::Io(e)) => assert_eq!(e.to_string(), "socket reset"),
            other => panic!("expected ProviderError::Io, got {other:?}"),
        }
    }
}
