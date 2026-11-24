//! The per-client connection: one writer thread, one read loop, one teardown.
//!
//! A connection is one long-lived exchange in two directions, and all three
//! transports (local Unix socket, TCP/Noise, in-process embedded channel) need the
//! same parts: an outbound sink, the state machine that dispatches inbound
//! messages, and a teardown that joins the writer. Only the wire differs, so the
//! shared machinery lives here and the transports are thin shells over it:
//!
//! * [`writer`] — the single-writer contract, the per-transport sinks, the one
//!   writer thread, and the outbound time policy both transports share (an
//!   absolute per-message budget plus a per-syscall stall, stated once in
//!   `choreo-proto`).
//! * [`reply`] — the exactly-one-reply guard every request is answered through.
//! * [`conn`] — the dispatch context, the connection itself, and its bounded
//!   teardown.
//! * [`threads`] — the three transport entry points.
//!
//! `handlers` (the per-request handlers) and the unit tests hang off this module
//! directly.

mod conn;
mod handlers;
mod reply;
mod threads;
mod writer;

// The daemon joins its writer under `choreo-proto`'s definition: the margin is
// stated once, beside the stall it derives from, so no connection end re-derives
// it.
pub(crate) use choreo_proto::WRITER_JOIN_GRACE;

// The accept paths and the daemon's state wiring reach these from outside the
// module; nothing else in the crate needs the rest.
pub(crate) use conn::ConnThreadArgs;
pub(crate) use threads::{
    EmbeddedConnArgs, client_thread, embedded_client_thread, tcp_handshake_and_client_thread,
};
pub(crate) use writer::{PEER_STALL, register_client_writer};

// The handler module and the unit tests build these directly.
pub(super) use conn::{ClientCtx, switch_attached_session};
pub(super) use reply::ReplyHandle;

// Only the unit tests name these: the connection itself reaches them through
// their owning modules.
#[cfg(test)]
pub(super) use writer::{ChannelConnectionWriter, ConnectionWriter, writer_thread};

#[cfg(test)]
mod tests;
