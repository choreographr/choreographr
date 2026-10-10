//! The wire protocol types: the two message envelopes and every payload they
//! carry.
//!
//! # The uniform correlation frame
//!
//! Both directions use one shaped envelope. A [`ClientMessage`] is
//! `{ id: u64, inner: ClientMessageType }`; a [`DaemonMessage`] is
//! `{ id: Option<u64>, inner: DaemonMessageType }`. A client allocates `id` as a
//! monotonic per-connection counter and the daemon MUST answer every request
//! with exactly one `DaemonMessage { id: Some(the same id), .. }` — the terminal
//! reply, success or failure. A broadcast is `DaemonMessage { id: None, .. }` and
//! never resolves a request.
//!
//! # Two orthogonal axes
//!
//! The **reply axis** (`id`) is per-connection and one-shot: one request, one
//! reply. The **stream axis** (`stream_id`, carried in the payload of the
//! streaming [`SessionEvent`]s a run fans out) is per-session and many-shot: a
//! single `RunInput`/`ContinueGeneration` fans many events out to EVERY session
//! subscriber, including mid-stream joiners. They are never merged, because two
//! clients each use their own request id 0, while a stream needs an id unique
//! across a namespace all subscribers share.
//!
//! The **daemon owns stream-id assignment**: a client does not choose a
//! `stream_id`. The session thread allocates one (per-session, monotonic) when it
//! accepts a `RunInput`/`ContinueGeneration` and reports it on the acceptance
//! reply and the `Started` broadcast; the client learns it from there (to key its
//! `stream_id → turn_id` map and to address a later `Cancel`).
//!
//! # Reply/broadcast overlap
//!
//! Reply-ness is a property of the SEND, not the payload type. The same `inner`
//! (e.g. `SessionState`, `ReasoningEffortSet`) may be emitted with `id: Some`
//! (a reply) or `id: None` (a broadcast); a client applies the payload's state
//! effect either way and resolves a pending slot only when `id` is `Some`. A
//! variant is split into a dedicated type only when the reply carries
//! requester-relative intent no broadcast may carry (the
//! `SessionCreatedForRequester` precedent).

use serde::{Deserialize, Serialize};

mod client;
mod common;
mod daemon;
mod session;

pub use client::*;
pub use common::*;
pub use daemon::*;
pub use session::*;

// TimestampMs lives at the module root rather than in a submodule so the
// `types` test suite — a child of this module — can still build it through its
// private tuple field, exactly as it did when every type shared this file.
/// Unix-epoch-milliseconds timestamp.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct TimestampMs(i64);

impl TimestampMs {
    /// Sentinel value for when the real timestamp is unavailable (e.g. corrupt
    /// DB entries).
    pub const ZERO: Self = Self(0);

    /// Current wall-clock time as `TimestampMs`.
    ///
    /// The u128→`i64` narrowing only truncates for clock readings past the
    /// year 29247 — practically never, so keep the original `as i64` behavior
    /// here rather than introducing an error path.
    #[must_use]
    #[expect(clippy::cast_possible_truncation)]
    pub fn now() -> Self {
        Self(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or_else(
                    |_| {
                        tracing::warn!("system clock before UNIX_EPOCH, using 0");
                        0
                    },
                    |d| d.as_millis() as i64,
                ),
        )
    }

    #[must_use]
    pub fn as_millis(&self) -> i64 {
        self.0
    }
}

#[cfg(test)]
mod tests;
