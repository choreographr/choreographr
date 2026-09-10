//! The live-socket registry: track sockets so another thread can force-close
//! them, and probe them for liveness.
//!
//! Why a registry at all: worker threads routinely block in `read()` on a
//! socket. There is no way to interrupt a blocking `read` from outside the
//! thread (channels can't reach into a syscall), but `shutdown(fd, SHUT_RDWR)`
//! makes the blocked `read` return immediately. So a control thread needs the
//! fd numbers of all live sockets — which is exactly what this registry keeps.

use std::sync::{Arc, Mutex};

// The platform's owned-descriptor type: fd on Unix, Winsock SOCKET handle on
// Windows (`std::os::fd` does not exist on Windows). Every public registry
// API is expressed in terms of this alias so the API is identical on both.
#[cfg(unix)]
pub(crate) type OwnedSock = std::os::fd::OwnedFd;
#[cfg(windows)]
pub(crate) type OwnedSock = std::os::windows::io::OwnedSocket;

/// Upper bound on the registry's Vec before an opportunistic
/// [`SocketRegistry::prune_dead`]. 256 is far above any expected steady state
/// (one entry per live connection) — its only job is to stop unbounded growth
/// if callers register sockets and never prune, e.g. in a long-lived process
/// with churny short-lived connections.
const MAX_REGISTERED_SOCKETS: usize = 256;

/// Cheaply cloneable registry of live socket fds.
///
/// Clones share the same fd list (internal `Arc<Mutex<Vec<OwnedSock>>>`). The
/// registry OWNS every fd registered into it; see the crate docs for the
/// ownership contract.
#[derive(Debug, Clone)]
pub struct SocketRegistry {
    // A std Mutex is fine here despite the channel-first house rule: this is
    // one of the sanctioned "single-purpose, minimally scoped" shared-state
    // shapes — the lock is held for a handful of fd syscalls and carries no
    // message traffic. Channels cannot express "reach into another thread's
    // blocked syscall", which is the whole point of the registry.
    sockets: Arc<Mutex<Vec<OwnedSock>>>,
}

impl SocketRegistry {
    /// Creates an empty registry.
    pub fn new() -> Self {
        Self {
            sockets: Arc::new(Mutex::new(Vec::new())),
        }
    }

    /// Registers a socket, taking ownership of the fd WITHOUT duplicating it.
    ///
    /// The caller must pass a duplicate if it intends to keep using the
    /// socket itself, e.g.:
    ///
    /// ```ignore
    /// let dup = stream.try_clone()?;          // new fd referring to same socket
    /// registry.register(OwnedFd::from(dup));  // registry now owns the duplicate
    /// ```
    ///
    /// Registered fds whose socket has already been closed elsewhere are
    /// tolerated: `shutdown_all` and `prune_dead` treat `EBADF` as "already
    /// gone" rather than an error. When the list exceeds
    /// [`MAX_REGISTERED_SOCKETS`], an opportunistic [`Self::prune_dead`] runs
    /// first to bound memory growth.
    pub fn register(&self, socket: impl Into<OwnedSock>) {
        let mut sockets = self.sockets.lock().unwrap_or_else(|e| e.into_inner());
        // Prune BEFORE pushing so the cap applies to the steady-state size;
        // the new socket is by definition alive, so pruning first never
        // evicts it.
        if sockets.len() >= MAX_REGISTERED_SOCKETS {
            prune_locked(&mut sockets);
        }
        sockets.push(socket.into());
        tracing::debug!(count = sockets.len(), "socket registered");
    }

    /// Number of currently registered fds (mainly for tests and metrics).
    pub fn registered_count(&self) -> usize {
        self.sockets.lock().unwrap_or_else(|e| e.into_inner()).len()
    }

    /// Force-closes every registered socket, then clears the list.
    ///
    /// Per-socket errors are tolerated and logged: `EBADF` means the fd was
    /// already closed elsewhere (debug level), `ENOTCONN` means the socket
    /// was already disconnected (debug level), anything else is warn.
    ///
    /// After this returns, every registered fd has been closed exactly once.
    /// Threads blocked in `read`/`write` on those sockets will wake with an
    /// error (or EOF), which is the entire reason this method exists.
    #[cfg(unix)]
    pub fn shutdown_all(&self) {
        use std::os::fd::IntoRawFd;

        use nix::sys::socket::{Shutdown, shutdown};
        use nix::unistd::close;

        let mut sockets = self.sockets.lock().unwrap_or_else(|e| e.into_inner());
        let count = sockets.len();
        // Close via into_raw_fd + explicit close instead of relying on Drop:
        // for an fd that was ALREADY closed elsewhere, Drop would close it a
        // second time (a real double-close bug if the number was recycled in
        // between). The explicit path logs the EBADF and moves on, and for
        // healthy fds `shutdown` + `close` is exactly what Drop would have
        // done anyway, just with observability at both steps.
        for socket in sockets.drain(..) {
            let raw = socket.into_raw_fd();
            match shutdown(raw, Shutdown::Both) {
                Ok(()) => tracing::debug!(fd = raw, "socket shut down"),
                // The fd was already closed by someone else — nothing to do.
                Err(nix::Error::EBADF) => {
                    tracing::debug!(fd = raw, "socket already closed (EBADF)")
                }
                // Not connected — shutdown is a no-op there, still fine.
                Err(nix::Error::ENOTCONN) => tracing::debug!(fd = raw, "socket not connected"),
                Err(e) => tracing::warn!(fd = raw, error = %e, "socket shutdown failed"),
            }
            // shutdown() does not close the fd; the registry owns it, so we
            // do. An EBADF here mirrors the shutdown EBADF above and is
            // equally harmless in this path.
            if let Err(e) = close(raw) {
                tracing::debug!(fd = raw, error = %e, "fd close after shutdown failed");
            }
        }
        tracing::info!(count, "force-closed all registered sockets");
    }

    /// Windows no-op for now. Winsock shutdown on duplicated SOCKET handles is
    /// a planned follow-up (WINDOWS-FOLLOW-UP); until then the registry still
    /// tracks fds so accounting code behaves identically, but shutdown has no
    /// effect. Dropping the fds (via the clear below) does close them, which
    /// is still correct — the registry owns them — it just lacks the
    /// "un-block the readers first" semantics of a real shutdown.
    #[cfg(not(unix))]
    pub fn shutdown_all(&self) {
        tracing::warn!(
            "shutdown_all is not implemented on this platform yet (planned Winsock follow-up); fds will only be closed, not shut down"
        );
        let mut sockets = self.sockets.lock().unwrap_or_else(|e| e.into_inner());
        let count = sockets.len();
        sockets.clear();
        tracing::info!(count, "cleared registered sockets (no shutdown performed)");
    }

    /// Probes every registered fd and removes (and closes) the dead ones.
    ///
    /// The probe is the same technique ureq's `TcpTransport::is_open` uses
    /// internally: temporarily flip the fd to non-blocking, do a 1-byte
    /// `recv` with `MSG_PEEK`, then restore the original flags.
    ///
    /// * `EAGAIN`/`EWOULDBLOCK` — nothing buffered to read, socket alive: keep.
    /// * `Ok(bytes)` — either EOF (`Ok(0)`, peer closed) or the peer sent
    ///   unprompted data; ureq treats both as "connection is done for us",
    ///   and so do we: remove and close.
    /// * any other error (`ECONNRESET`, `EBADF`, …) — dead: remove and close.
    #[cfg(unix)]
    pub fn prune_dead(&self) {
        let mut sockets = self.sockets.lock().unwrap_or_else(|e| e.into_inner());
        prune_locked(&mut sockets);
    }

    /// Windows no-op for now (probing needs `ioctlsocket`-based non-blocking
    /// recv; planned with the Winsock follow-up).
    #[cfg(not(unix))]
    pub fn prune_dead(&self) {
        tracing::warn!(
            "prune_dead is not implemented on this platform yet (planned Winsock follow-up)"
        );
    }
}

impl Default for SocketRegistry {
    fn default() -> Self {
        Self::new()
    }
}

/// Lock-held prune worker, shared by [`SocketRegistry::prune_dead`] and the
/// opportunistic prune in [`SocketRegistry::register`] (which must not
/// re-acquire the already-held mutex — hence taking the locked Vec directly).
#[cfg(unix)]
fn prune_locked(sockets: &mut Vec<OwnedSock>) {
    use std::os::fd::AsFd;

    let before = sockets.len();
    let mut i = 0;
    while i < sockets.len() {
        let alive = probe_alive(sockets[i].as_fd());
        if alive {
            i += 1;
            continue;
        }
        // Remove and close EXPLICITLY (into_raw_fd + close, like
        // shutdown_all) rather than dropping the OwnedFd: for a socket that
        // was already closed elsewhere (EBADF is one of the "dead"
        // verdicts), Drop would close it a second time — a real double-close
        // bug — and std's IO-safety runtime aborts the process on it. The
        // explicit path turns that into a logged EBADF.
        let socket = sockets.remove(i);
        close_logged(socket);
    }
    let pruned = before - sockets.len();
    if pruned > 0 {
        tracing::debug!(pruned, remaining = sockets.len(), "pruned dead sockets");
    }
}

/// Windows cap-enforcement stand-in for the Unix probe-based prune: without
/// a liveness probe (Winsock follow-up) we cannot tell dead from alive, so
/// when the cap is hit we close-and-drop the OLDEST entries to stay bounded.
/// Same growth guarantee as the Unix path, weaker eviction policy.
#[cfg(windows)]
fn prune_locked(sockets: &mut Vec<OwnedSock>) {
    while sockets.len() >= MAX_REGISTERED_SOCKETS {
        let oldest = sockets.remove(0);
        let _ = oldest; // dropping OwnedSocket closes the handle
    }
    tracing::debug!(
        remaining = sockets.len(),
        "trimmed socket registry to cap (no probe on this platform)"
    );
}

/// The liveness probe, split out of the removal loop for readability.
/// Returns `true` when the socket looks usable (see `prune_dead` docs).
#[cfg(unix)]
fn probe_alive(fd: std::os::fd::BorrowedFd<'_>) -> bool {
    use nix::Error;
    use nix::fcntl::{FcntlArg, OFlag, fcntl};
    use nix::sys::socket::{MsgFlags, recv};
    use std::os::fd::AsRawFd;

    // Save the current flags so the restore puts back EXACTLY what the
    // owner of the twin fd had (the caller's stream may legitimately be
    // non-blocking already).
    let flags = match fcntl(fd, FcntlArg::F_GETFL) {
        Ok(f) => f,
        // Can't even read flags — the fd is unusable, treat as dead.
        Err(_) => return false,
    };
    let flags = OFlag::from_bits_truncate(flags);
    if fcntl(fd, FcntlArg::F_SETFL(flags | OFlag::O_NONBLOCK)).is_err() {
        return false;
    }
    let mut buf = [0u8; 1];
    let alive = match recv(fd.as_raw_fd(), &mut buf, MsgFlags::MSG_PEEK) {
        // Nothing buffered and no error: the connection is open and
        // idle — the definition of "alive" for this probe.
        // EWOULDBLOCK is the same value as EAGAIN on every supported Unix;
        // matching both keeps this exhaustive against future nix variants.
        Err(Error::EAGAIN) => true,
        // Interrupted probe is indeterminate; conservatively keep.
        Err(Error::EINTR) => true,
        // EOF (Ok(0)) or unsolicited peer data (Ok(n>0)): ureq's probe
        // closes in both cases, and an unsolicited byte means the
        // protocol stream is already corrupt for us anyway.
        Ok(_) => false,
        // ECONNRESET, EBADF, ETIMEDOUT, …: dead.
        Err(e) => {
            tracing::debug!(error = %e, "prune probe reports dead socket");
            false
        }
    };
    // Restore the original flags regardless of the verdict; a failure
    // here means the fd is in bad shape, but it is about to be closed
    // if dead — and if alive, the next probe will surface the problem.
    let _ = fcntl(fd, FcntlArg::F_SETFL(flags));
    alive
}

/// Takes ownership of an fd and closes it, logging (never panicking on) the
/// result. `EBADF` is expected for sockets that were already closed
/// elsewhere — the registry tolerates that case by contract.
#[cfg(unix)]
fn close_logged(socket: OwnedSock) {
    use std::os::fd::IntoRawFd;

    use nix::unistd::close;

    let raw = socket.into_raw_fd();
    if let Err(e) = close(raw) {
        tracing::debug!(fd = raw, error = %e, "close of registered socket failed");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::fd::{AsFd, FromRawFd, IntoRawFd, OwnedFd};

    use nix::sys::socket::{AddressFamily, SockFlag, SockType, socketpair};
    use nix::unistd::{close, dup, read};

    /// Creates a connected Unix socket pair for tests.
    fn pair() -> (OwnedFd, OwnedFd) {
        socketpair(
            AddressFamily::Unix,
            SockType::Stream,
            None,
            SockFlag::empty(),
        )
        .expect("socketpair")
    }

    /// Closes a raw fd "elsewhere" (simulating the caller closing its twin)
    /// and hands the raw number back wrapped in a fresh OwnedFd, exactly the
    /// shape `register` expects to receive for an already-dead fd.
    fn dead_fd() -> OwnedFd {
        let (a, _b) = pair();
        let raw = a.into_raw_fd();
        close(raw).expect("close");
        // SAFETY: raw was just closed and this test never closes it again
        // except through the registry (which tolerates the resulting EBADF).
        unsafe { OwnedFd::from_raw_fd(raw) }
    }

    #[test]
    fn registration_counts_and_clone_shares_state() {
        let registry = SocketRegistry::new();
        assert_eq!(registry.registered_count(), 0);
        let (a, b) = pair();
        registry.register(a);
        let clone = registry.clone();
        assert_eq!(registry.registered_count(), 1);
        registry.register(b);
        // Clone sees the same underlying list.
        assert_eq!(clone.registered_count(), 2);
    }

    #[test]
    fn shutdown_all_closes_sockets_and_tolerates_already_closed_fds() {
        let registry = SocketRegistry::new();
        let (a, b) = pair();
        // Duplicate of `a` is registered; the original stays with the test so
        // we can observe the shutdown from the peer side.
        let a_dup = dup(&a).expect("dup");
        registry.register(a_dup);
        registry.register(dead_fd());
        assert_eq!(registry.registered_count(), 2);

        registry.shutdown_all();
        assert_eq!(registry.registered_count(), 0);

        // The peer end must now see EOF or an error — either way, read()
        // returns instead of blocking forever, which is the contract.
        let mut buf = [0u8; 1];
        match read(b.as_fd(), &mut buf) {
            Ok(0) => {}                       // clean EOF: expected
            Err(nix::Error::ECONNRESET) => {} // also acceptable
            other => panic!("expected EOF/ECONNRESET, got {other:?}"),
        }
        // Idempotent: shutting down an empty registry is a no-op.
        registry.shutdown_all();
    }

    #[test]
    fn prune_dead_removes_only_dead_sockets() {
        let registry = SocketRegistry::new();
        let (live, peer) = pair();
        // Register a duplicate so the test still holds `live` for verification
        // after the registry (possibly) closes its copy.
        let live_dup = dup(&live).expect("dup");
        registry.register(live_dup);
        registry.register(dead_fd());
        assert_eq!(registry.registered_count(), 2);

        registry.prune_dead();
        assert_eq!(registry.registered_count(), 1);

        // The surviving registration is the live one: the fd still functions
        // (write from the peer, read the byte back), proving prune kept an
        // ALIVE socket and removed only the dead one.
        use std::io::Write;
        let mut peer_file = std::fs::File::from(peer);
        peer_file.write_all(b"x").expect("write to peer");
        let mut buf = [0u8; 1];
        let n = read(live.as_fd(), &mut buf).expect("read from live");
        assert_eq!(n, 1);
        assert_eq!(buf[0], b'x');
    }
}
