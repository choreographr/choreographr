//! The three transport entry threads.
//!
//! Each is a thin shell: read one message off its own transport, hand it to
//! [`super::ClientConn::dispatch`], and tear down with
//! [`super::ClientConn::finish`]. The transport-specific parts are exactly the
//! read, the writer sink it is paired with, and the error classification — the
//! state machine in between is shared.

mod embedded;
mod tcp;
mod unix;

pub(crate) use embedded::{EmbeddedConnArgs, embedded_client_thread};
pub(crate) use tcp::tcp_handshake_and_client_thread;
pub(crate) use unix::client_thread;
