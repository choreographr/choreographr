//! The socket- and frame-level I/O the wire protocol runs over.
//!
//! Four concerns, one per submodule, so a caller names the one it needs instead
//! of reaching into a single grab-bag:
//!
//! * [`path`] — where the daemon's unix-domain socket lives, and the
//!   cross-platform dial ([`UnixStream`], [`connect_unix`], [`socket_path`]) plus
//!   the "nothing is listening" classification.
//! * [`framing`] — the 4-byte length-prefixed [`write_message`] /
//!   [`read_message`] of one message.
//! * [`write`] — the deadline- and stall-bounded blocking write machinery and the
//!   per-message writer budget that sizes it.
//! * [`read`] — the deadline-bounded blocking read machinery.
//!
//! Everything is re-exported here so every caller keeps naming it through
//! `choreo_proto::`, and the two I/O modules state the bounds once each: the
//! handshake and the data plane, on both sides of a connection, all apply the
//! same numbers rather than carrying their own copies.

mod framing;
mod path;
mod read;
mod write;

pub use framing::{read_message, read_payload, write_message};
pub use path::{
    SOCKET_PATH_ENV, UnixStream, connect_unix, default_socket_path, dial_error_means_no_listener,
    socket_listening, socket_path,
};
pub use read::{TimedRead, read_exact_by, read_exact_with_stall};
pub use write::{
    CONNECTION_STALL, TimedWrite, WRITER_MIN_RATE_BYTES_PER_SEC, message_budget, write_all_by,
    write_all_with_stall,
};

// Re-exported for the crate's own tests, which drive the env-override seam
// directly. A non-test build has no caller, so the alias is `cfg(test)`-only and
// the real item stays reachable inside `path`.
#[cfg(test)]
pub(crate) use path::socket_path_impl;
