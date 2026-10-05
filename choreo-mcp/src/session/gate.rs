//! Concurrent-call admission: pure arithmetic for the per-server call cap.

use crate::error::McpError;
use crate::protocol::CallToolResult;
use crate::session::CallRequest;
use crate::session::cancel::CancelToken;
use crossbeam_channel::Sender;

/// A tool invocation waiting for a free concurrency slot.
///
/// The dispatcher admits a call straight away when a slot is free, otherwise it
/// parks the request here until an in-flight call completes. The queued entry
/// carries everything the eventual spawn needs, plus its cancellation token so a
/// session cancel can reach a call that has not started yet.
pub(super) struct QueuedCall {
    pub(super) call_id: u64,
    pub(super) session_id: u64,
    pub(super) request: CallRequest,
    pub(super) reply: Sender<Result<CallToolResult, McpError>>,
    pub(super) chunk_tx: Option<crossbeam_channel::Sender<Vec<u8>>>,
    pub(super) cancel: CancelToken,
}

/// Per-server accounting for the concurrent-call cap.
///
/// Kept as pure arithmetic (no channels, no threads) so the admission decision —
/// admit up to `cap`, queue the rest, promote one per completion — is
/// unit-testable without spawning anything. The dispatcher owns the single
/// instance and the [`QueuedCall`] deque that mirrors `queued`.
pub(super) struct CallGate {
    cap: usize,
    pub(super) active: usize,
    pub(super) queued: usize,
}

impl CallGate {
    /// Build a gate with `cap` slots, clamped to at least one so a misconfigured
    /// zero can never wedge the dispatcher.
    pub(super) fn new(cap: usize) -> Self {
        Self {
            cap: cap.max(1),
            active: 0,
            queued: 0,
        }
    }

    /// Admit a call: `true` when a slot is free (and is now taken), `false` when
    /// the call must wait in the queue.
    pub(super) fn admit(&mut self) -> bool {
        if self.active < self.cap {
            self.active += 1;
            true
        } else {
            self.queued += 1;
            false
        }
    }

    /// Whether a queued call can be promoted into a free slot.
    pub(super) fn has_capacity(&self) -> bool {
        self.active < self.cap
    }

    /// Move one queued call into an active slot. Only call when
    /// [`has_capacity`](Self::has_capacity) is true and the deque is non-empty.
    pub(super) fn promote(&mut self) {
        debug_assert!(self.has_capacity() && self.queued > 0);
        self.queued = self.queued.saturating_sub(1);
        self.active += 1;
    }

    /// Record that a call was parked WITHOUT going through
    /// [`admit`](Self::admit), used while a reconnect is in flight.
    ///
    /// The call is pushed to the dispatcher's queue so it can be promoted once
    /// the rebuilt engine is ready. `admit` already increments `queued` when it
    /// refuses, so this is called only when `admit` was skipped entirely.
    pub(super) fn queue(&mut self) {
        self.queued += 1;
    }

    /// Record that an active call finished, freeing a slot.
    pub(super) fn complete(&mut self) {
        self.active = self.active.saturating_sub(1);
    }

    /// Record that a queued call was abandoned without ever running.
    pub(super) fn abandon(&mut self) {
        self.queued = self.queued.saturating_sub(1);
    }
}
