//! The daemon's server layers.
//!
//! `core` assembles the transport-independent daemon around `DaemonState`
//! (command loop, catalog maintenance, config/ACL/power watchers);
//! `connection` runs the per-client state machine and the single-writer
//! transport loop; `lifecycle` adds the Unix-socket and TCP/Noise adapters and
//! the shutdown orchestration; and [`acl`] holds the client-key ACL the Noise
//! handshake authenticates against. [`run_server`] wires them together.

pub mod acl;
pub(crate) mod connection;
pub(crate) mod core;
pub(crate) mod lifecycle;

pub use lifecycle::run_server;
