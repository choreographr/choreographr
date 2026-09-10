//! Shared registry for force-closing live sockets and applying TCP keepalive
//! tuning.
//!
//! Two responsibilities, one small leaf crate:
//!
//! 1. [`SocketRegistry`] — tracks live sockets (by duplicate fd) so a control
//!    thread can [`SocketRegistry::shutdown_all`] them out from under
//!    whichever worker threads are blocked reading/writing. This is the
//!    event-driven way to un-block a reader: `shutdown(SHUT_RDWR)` makes the
//!    blocked `read` return immediately, so no polling or sleeps are needed.
//! 2. [`SocketTuning`] — TCP keepalive options applied once right after
//!    connect, so a half-dead connection (NAT timeout, cable pull) is noticed
//!    by the kernel instead of hanging forever.
//!
//! # Ownership model
//!
//! [`SocketRegistry::register`] takes ownership of the fd it is given — it
//! does NOT duplicate it. Callers that want to keep using the socket must
//! pass a duplicate (e.g. `TcpStream::try_clone(&stream)` followed by
//! `OwnedFd::from(dup)`). The registry closes every fd it holds (via
//! `prune_dead` for dead sockets, via `shutdown_all` at teardown).
//!
//! # Portability
//!
//! All real functionality is Unix-only (`cfg(unix)`, implemented via `nix`).
//! On non-Unix targets (Windows, for now) the same public API compiles but
//! `shutdown_all` / `prune_dead` / `SocketTuning::apply` are logged no-ops.
//! Windows Winsock shutdown (`WSASendDisconnect` / `closesocket` on a
//! duplicated `SOCKET`) is a planned follow-up — the call sites are marked
//! with `WINDOWS-FOLLOW-UP` comments so they are easy to find.

#![forbid(unsafe_op_in_unsafe_fn)]

pub mod socket_registry;
pub mod tuning;

#[cfg(feature = "ureq")]
pub mod connector;

pub use socket_registry::SocketRegistry;
pub use tuning::SocketTuning;

#[cfg(feature = "ureq")]
pub use connector::RegisteringTcpConnector;
