//! The per-direction single-actor slot.
//!
//! One `NoiseStream` has exactly one reader thread and exactly one writer
//! thread. A second actor in either direction is not a performance problem, it
//! is silent corruption: two senders interleave fragments of different logical
//! messages on the wire, and two readers splice them back together the wrong
//! way, neither with any error. The daemon and the client both give a connection
//! one thread per direction, so this is a structural invariant — the slot turns
//! a future violation into a loud protocol error instead of a corrupted stream.
//!
//! Each slot is an `Arc<AtomicBool>` held across every `try_clone`, so the clones
//! of one connection contend for the same right (see AGENTS.md's sanctioned
//! shared-state exception for the Noise transport).

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::error::TransportError;

/// A direction's single-actor slot, shared across every `try_clone` handle of
/// one connection.
pub(super) struct SingleSlot {
    held: Arc<AtomicBool>,
    /// The entry point this slot guards, used in the refusal message so an
    /// operator can tell a concurrent send from a concurrent receive.
    what: &'static str,
}

impl SingleSlot {
    /// A free (unheld) slot for the entry point named `what`.
    pub(super) fn new(what: &'static str) -> Self {
        Self {
            held: Arc::new(AtomicBool::new(false)),
            what,
        }
    }

    /// Claim the slot for one call, or fail if another already holds it.
    ///
    /// The returned [`SlotGuard`] releases the slot on every exit path,
    /// including an early `?` — without that, an actor that errored mid-message
    /// would leave the flag set and wedge the stream.
    pub(super) fn acquire(&self) -> Result<SlotGuard, TransportError> {
        // The acquire/release pair is what makes exclusion independent of any
        // compiler-reordering argument: the critical section holds socket
        // syscalls, not just memory, so the flag must not be observed as
        // released while a prior holder's writes are still in flight. Relaxed
        // would rely on the syscall alone acting as a barrier, which is not a
        // property the atomic gives us.
        if self
            .held
            .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            return Err(TransportError::ConcurrentAccess { what: self.what });
        }
        Ok(SlotGuard(Arc::clone(&self.held)))
    }
}

impl Clone for SingleSlot {
    fn clone(&self) -> Self {
        Self {
            held: Arc::clone(&self.held),
            what: self.what,
        }
    }
}

/// Holds a [`SingleSlot`] for the duration of one call and frees it on drop.
///
/// It owns its own `Arc` clone rather than borrowing the field, so it does not
/// hold a borrow of the stream across the call body.
pub(super) struct SlotGuard(Arc<AtomicBool>);

impl Drop for SlotGuard {
    fn drop(&mut self) {
        // Release pairs with the acquire in `acquire`: everything the guard's
        // critical section did is visible to the next holder.
        self.0.store(false, Ordering::Release);
    }
}
