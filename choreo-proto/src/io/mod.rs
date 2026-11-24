//! The socket- and frame-level I/O the wire protocol runs over.
//!
//! Five concerns, one per submodule, so a caller names the one it needs instead
//! of reaching into a single grab-bag:
//!
//! * [`path`] — where the daemon's unix-domain socket lives, and the
//!   cross-platform dial ([`UnixStream`], [`connect_unix`], [`socket_path`]) plus
//!   the "nothing is listening" classification.
//! * [`framing`] — the 4-byte length-prefixed [`write_message`] /
//!   [`read_message`] of one message.
//! * [`policy`] — the connection's time policy in both directions: the
//!   per-syscall detection latency, the floor throughput that sizes a
//!   whole-message write budget, and the floor that keeps a zero from disabling
//!   either.
//! * [`write`] — the deadline- and stall-bounded blocking write machinery that
//!   applies the policy.
//! * [`read`] — the deadline- and stall-bounded blocking read machinery.
//!
//! Everything is re-exported here so every caller keeps naming it through
//! `choreo_proto::`, and the policy module states the numbers once: the
//! handshake and the data plane, on both sides of a connection, all apply the
//! same figures rather than carrying their own copies.

mod framing;
mod path;
mod policy;
mod read;
mod write;

pub use framing::{read_message, read_payload, write_message};
pub use path::{
    SOCKET_PATH_ENV, UnixStream, connect_unix, default_socket_path, dial_error_means_no_listener,
    socket_listening, socket_path,
};
pub use policy::{
    CONNECTION_STALL, MIN_SOCKET_STALL, WRITER_MIN_RATE_BYTES_PER_SEC, effective_stall,
    message_budget,
};
pub use read::{TimedRead, read_exact_by, read_exact_with_stall};
pub use write::{TimedWrite, write_all_by, write_all_with_stall, write_frame_bounded};

// Re-exported for the crate's own tests, which drive the env-override seam
// directly. A non-test build has no caller, so the alias is `cfg(test)`-only and
// the real item stays reachable inside `path`.
#[cfg(test)]
pub(crate) use path::socket_path_impl;
