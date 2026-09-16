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
//! `OwnedFd::from(dup)`).
//!
//! # RAII lifecycle (register → track → unregister on drop)
//!
//! [`SocketRegistry::register`] returns a [`SocketId`]; the caller passes it
//! back to [`SocketRegistry::unregister`] when the connection is done
//! (normally from a `Drop` impl), which removes the entry and closes the
//! registry's fd exactly once. With the `ureq` feature,
//! `RegisteredTcpTransport` does this automatically, so in steady state the
//! registry's size equals the number of LIVE connections — ureq can close
//! pooled connections at any time without leaking entries. Entry removal is
//! the single close-ownership-transfer signal: if `shutdown_all` or
//! `prune_dead` already removed an entry, `unregister` is a documented
//! no-op, so a guard can never double-close an fd the registry closed
//! first. `prune_dead` and the 256-entry cap in the registry remain purely
//! as backstops.
//!
//! # Portability
//!
//! Unix uses `nix`. Windows implements the same public API via `windows-sys`:
//! `shutdown_all` issues a Winsock `shutdown(SD_BOTH)` (to un-block a peer
//! thread) then closes the duplicate handle; `prune_dead` probes liveness via a
//! purely observational zero-timeout `WSAPoll` plus a `recv(MSG_PEEK)` verdict
//! only when an event is pending — deliberately NOT the FIONBIO flip
//! `TcpTransport::is_open` uses (changing the mode is not permitted while a
//! peer thread is blocked on the socket and is shared with the twin handle);
//! and `SocketTuning::apply` sets `SO_KEEPALIVE` plus the timings through
//! `WSAIoctl(SIO_KEEPALIVE_VALS)`.

#![forbid(unsafe_op_in_unsafe_fn)]

pub mod socket_registry;
pub mod tuning;

#[cfg(feature = "ureq")]
pub mod connector;

pub use socket_registry::{SocketId, SocketRegistry};
pub use tuning::SocketTuning;

#[cfg(feature = "ureq")]
pub use connector::RegisteringTcpConnector;

/// Bridges std's `RawSocket` (the value `AsRawSocket::as_raw_socket` returns)
/// to the `SOCKET` type `windows-sys` expects at the FFI boundary.
///
/// std models a Windows socket as `u64` on 64-bit targets (and `u32` on
/// 32-bit), while `windows-sys` models the Win32 `SOCKET` (`UINT_PTR`) as
/// `usize`. Both are pointer-width, so the conversion is *infallible* on every
/// Windows target — `usize::try_from` is used only to keep clippy's lossy-cast
/// lints quiet, and its `0` fallback is unreachable (0 is not a valid SOCKET;
/// the OS would reject it). Shared by the registry's `shutdown`/`probe` and the
/// tuning FFI so all three call sites convert identically.
#[cfg(windows)]
pub(crate) fn socket_handle(raw: std::os::windows::io::RawSocket) -> usize {
    usize::try_from(raw).unwrap_or_default()
}
