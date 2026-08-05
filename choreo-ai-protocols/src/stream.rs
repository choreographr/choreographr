use std::io;
use std::sync::mpsc;
use std::time::Duration;

use crate::shared::ProviderError;

/// How often the consumer polls for new SSE events (and re-checks
/// cancellation) while a stream is quiet.
///
/// The reader thread may be blocked inside a socket `read()` for a long time
/// (a stalled or keep-alive-trickling provider), so the consumer cannot block
/// indefinitely on a plain `recv()` — it must wake up periodically to notice
/// a cancellation signal.  ~200 ms keeps Escape responsive without adding
/// meaningful latency to normal event flow.
pub(crate) const SSE_POLL_INTERVAL: Duration = Duration::from_millis(200);

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

/// Spawn a dedicated thread that runs the blocking SSE read loop and
/// forwards each parsed event through an mpsc channel.
///
/// # Why a thread at all
///
/// `SseReader::next_event()` blocks inside `BufReader::read()` on the socket.
/// If the provider stalls (or trickles keep-alive bytes that never form a
/// complete event), that read can block indefinitely — and a loop that only
/// checks `cancel_rx` *between* reads would never see the user's Escape.
/// Moving the read onto its own thread lets the consumer poll the channel
/// with `recv_timeout` and check cancellation every iteration, so Escape
/// lands within ~200 ms instead of never.
///
/// # Reader-thread lifetime
///
/// The thread lingers only if it is mid-`read()` when the consumer bails
/// (e.g. on cancellation).  It is detached, holds no locks or shared state
/// (the `Reader` is moved in), and is bounded by the agent's idle/global
/// timeouts, so it cannot wedge the process — and it dies with the process.
pub(crate) fn spawn_sse_reader<T, F>(mut next: F) -> mpsc::Receiver<SseStreamMsg<T>>
where
    T: Send + 'static,
    F: FnMut() -> io::Result<Option<T>> + Send + 'static,
{
    let (tx, rx) = mpsc::channel();
    tracing::trace!("spawning SSE reader thread");
    std::thread::spawn(move || {
        loop {
            match next() {
                Ok(Some(item)) => {
                    // If the consumer has dropped the receiver (cancelled or
                    // bailed), the send fails — nothing left to do, exit.
                    if tx.send(SseStreamMsg::Event(item)).is_err() {
                        tracing::debug!("SSE reader thread exiting: consumer dropped the channel");
                        return;
                    }
                }
                Ok(None) => {
                    tracing::trace!("SSE stream ended; forwarding End");
                    // Ignore a send error here: the consumer may already be gone.
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
    rx
}

/// Receive the next SSE event, polling with [`SSE_POLL_INTERVAL`] so that a
/// cancellation signal on `cancel_rx` is observed within ~200 ms even when
/// the stream is quiet or stalled.
///
/// Returns `Ok(None)` on a clean stream end, `Err(ProviderError::Cancelled)`
/// when a cancellation is pending, and `Err(ProviderError::Io(..))` when the
/// reader thread reports a read error or dies unexpectedly.
pub(crate) fn recv_sse_event<T>(
    rx: &mpsc::Receiver<SseStreamMsg<T>>,
    cancel_rx: Option<&mpsc::Receiver<()>>,
) -> Result<Option<T>, ProviderError> {
    loop {
        // Check cancellation before each poll.  Because the reader thread
        // decouples the blocking read from this loop, a cancel that arrives
        // while we are inside `recv_timeout` is noticed on the next iteration
        // at most ~200 ms later.
        if let Some(rx) = cancel_rx
            && rx.try_recv().is_ok()
        {
            tracing::debug!("SSE stream cancelled by user");
            return Err(ProviderError::Cancelled);
        }

        match rx.recv_timeout(SSE_POLL_INTERVAL) {
            Ok(SseStreamMsg::Event(item)) => return Ok(Some(item)),
            Ok(SseStreamMsg::End) => return Ok(None),
            Ok(SseStreamMsg::Err(e)) => return Err(ProviderError::Io(e)),
            Err(mpsc::RecvTimeoutError::Timeout) => {
                // No event yet; loop back and re-check cancellation.
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                // The reader thread ended without sending End or Err — its
                // closure cannot return normally without sending one of
                // those, so this means the thread died unexpectedly.
                tracing::warn!("SSE reader thread terminated unexpectedly");
                return Err(ProviderError::Io(io::Error::other(
                    "SSE reader thread terminated unexpectedly",
                )));
            }
        }
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
        let rx = spawn_sse_reader(move || items.next().unwrap_or(Ok(None)));

        assert!(matches!(rx.recv(), Ok(SseStreamMsg::Event(0))));
        assert!(matches!(rx.recv(), Ok(SseStreamMsg::Event(1))));
        assert!(matches!(rx.recv(), Ok(SseStreamMsg::Event(2))));
        assert!(matches!(rx.recv(), Ok(SseStreamMsg::End)));
    }

    #[test]
    fn reader_error_forwarded_as_err() {
        // A reader that immediately fails surfaces the io::Error verbatim.
        // (T is annotated — nothing constrains it when the closure only errs.)
        let mut items = std::iter::once(Err(io::Error::other("boom")));
        let rx: mpsc::Receiver<SseStreamMsg<i32>> =
            spawn_sse_reader(move || items.next().unwrap_or(Ok(None)));
        match rx.recv() {
            Ok(SseStreamMsg::Err(e)) => assert_eq!(e.to_string(), "boom"),
            other => panic!("expected SseStreamMsg::Err, got {other:?}"),
        }
    }

    #[test]
    fn reader_error_after_events_forwarded() {
        // One event, then a failure — both forwarded in order.
        let mut items =
            std::iter::once(Ok(Some(7))).chain(std::iter::once(Err(io::Error::other("late"))));
        let rx = spawn_sse_reader(move || items.next().unwrap_or(Ok(None)));
        assert!(matches!(rx.recv(), Ok(SseStreamMsg::Event(7))));
        match rx.recv() {
            Ok(SseStreamMsg::Err(e)) => assert_eq!(e.to_string(), "late"),
            other => panic!("expected SseStreamMsg::Err, got {other:?}"),
        }
    }

    // ── recv_sse_event ──────────────────────────────────────────────────

    #[test]
    fn pre_sent_cancel_returns_cancelled_immediately() {
        // A cancel signal sent BEFORE the call must win on the first
        // iteration, regardless of what the reader produces.  The reader
        // yields forever here; once the receiver is dropped at test end the
        // thread exits on its next failed send, so nothing leaks.
        let (cancel_tx, cancel_rx) = mpsc::channel();
        cancel_tx.send(()).unwrap();

        let mut items = std::iter::repeat_with(|| Ok(Some(0)));
        let rx = spawn_sse_reader(move || items.next().unwrap_or(Ok(None)));
        let err = recv_sse_event(&rx, Some(&cancel_rx)).unwrap_err();
        assert!(matches!(err, ProviderError::Cancelled));
    }

    #[test]
    fn end_maps_to_ok_none() {
        // A reader that ends immediately (Ok(None)) maps to Ok(None), i.e.
        // the loop's "clean break" signal.  (T is annotated — the closure
        // only yields None, so the item type is otherwise unconstrained.)
        let mut items = std::iter::once(Ok(None));
        let rx: mpsc::Receiver<SseStreamMsg<i32>> =
            spawn_sse_reader(move || items.next().unwrap_or(Ok(None)));
        assert_eq!(recv_sse_event(&rx, None).unwrap(), None);
    }

    #[test]
    fn event_maps_to_ok_some() {
        let mut items = std::iter::once(Ok(Some("hello".to_string())));
        let rx = spawn_sse_reader(move || items.next().unwrap_or(Ok(None)));
        let item = recv_sse_event(&rx, None).unwrap().expect("event");
        assert_eq!(item, "hello");
    }

    #[test]
    fn disconnected_maps_to_io_error() {
        // A channel whose sender has already been dropped (i.e. the reader
        // thread died without sending End or Err) surfaces as an Io error.
        // Constructed directly rather than via a spawned reader so the test
        // is fully deterministic — no thread, no race with the reader.
        let (tx, rx) = mpsc::channel::<SseStreamMsg<i32>>();
        drop(tx);
        match recv_sse_event(&rx, None) {
            Err(ProviderError::Io(e)) => {
                assert!(e.to_string().contains("terminated unexpectedly"))
            }
            other => panic!("expected Io error on disconnect, got {other:?}"),
        }
    }

    #[test]
    fn reader_error_maps_to_io_error() {
        let mut items = std::iter::once(Err(io::Error::other("socket reset")));
        let rx: mpsc::Receiver<SseStreamMsg<i32>> =
            spawn_sse_reader(move || items.next().unwrap_or(Ok(None)));
        match recv_sse_event(&rx, None) {
            Err(ProviderError::Io(e)) => assert_eq!(e.to_string(), "socket reset"),
            other => panic!("expected ProviderError::Io, got {other:?}"),
        }
    }
}
