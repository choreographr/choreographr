//! The live-socket registry: track sockets so another thread can force-close
//! them, and probe them for liveness.
//!
//! Why a registry at all: worker threads routinely block in `read()` on a
//! socket. There is no way to interrupt a blocking `read` from outside the
//! thread (channels can't reach into a syscall), but `shutdown(fd, SHUT_RDWR)`
//! makes the blocked `read` return immediately. So a control thread needs the
//! fd numbers of all live sockets — which is exactly what this registry keeps.

use std::sync::atomic::{AtomicU64, Ordering};
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

/// A handle identifying one registration inside a [`SocketRegistry`].
///
/// Returned by [`SocketRegistry::register`] and handed back to
/// [`SocketRegistry::unregister`] by the RAII guard in the connector, so a
/// dropped transport removes exactly its own entry. Ids are monotonic,
/// non-zero, and unique *per registry instance* (each registry carries its
/// own `AtomicU64` counter, cloned with the registry's `Arc` — clones share
/// the counter and the list, so a registration through any clone yields ids
/// from the same sequence). Uniqueness per registry is all that is needed:
/// unregister only ever searches the registry the registration came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct SocketId(u64);

impl SocketId {
    /// Allocates the next id from a registry's shared counter. Non-zero by
    /// construction (the counter starts at 1), so 0 can serve as a sentinel
    /// elsewhere if it ever needs to.
    fn next(counter: &AtomicU64) -> Self {
        Self(counter.fetch_add(1, Ordering::Relaxed))
    }
}

/// One registry entry: the socket the registry owns plus the id that refers
/// to it.
#[derive(Debug)]
struct Entry {
    id: SocketId,
    sock: OwnedSock,
}

/// Cheaply cloneable registry of live socket fds.
///
/// Clones share the same fd list (internal `Arc<Mutex<Vec<Entry>>>`) and id
/// counter. The registry OWNS every fd registered into it; see the crate
/// docs for the ownership contract. Entries are normally removed by the RAII
/// guard on the transport that created them (`unregister` on `Drop`), so in
/// steady state the registry tracks only LIVE connections; `prune_dead` and
/// the [`MAX_REGISTERED_SOCKETS`] cap stay purely as backstops.
#[derive(Debug, Clone)]
pub struct SocketRegistry {
    // A std Mutex is fine here despite the channel-first house rule: this is
    // one of the sanctioned "single-purpose, minimally scoped" shared-state
    // shapes — the lock is held for a handful of fd syscalls and carries no
    // message traffic. Channels cannot express "reach into another thread's
    // blocked syscall", which is the whole point of the registry.
    sockets: Arc<Mutex<Vec<Entry>>>,
    // Per-REGISTRY (not per-process) id counter: shared via the Arc so all
    // clones of one registry hand out ids from the same monotonic sequence.
    next_id: Arc<AtomicU64>,
}

impl SocketRegistry {
    /// Creates an empty registry.
    pub fn new() -> Self {
        // Start at 1 so no valid SocketId is ever 0 (see SocketId's docs).
        Self {
            sockets: Arc::new(Mutex::new(Vec::new())),
            next_id: Arc::new(AtomicU64::new(1)),
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
    ///
    /// Returns the [`SocketId`] for this registration: the caller (normally
    /// the connector's RAII guard) passes it back to [`Self::unregister`] to
    /// remove the entry — and close the registry's fd — when the transport
    /// is dropped.
    pub fn register(&self, socket: impl Into<OwnedSock>) -> SocketId {
        let id = SocketId::next(&self.next_id);
        let mut sockets = self.sockets.lock().unwrap_or_else(|e| e.into_inner());
        // Prune BEFORE pushing so the cap applies to the steady-state size;
        // the new socket is by definition alive, so pruning first never
        // evicts it.
        if sockets.len() >= MAX_REGISTERED_SOCKETS {
            prune_locked(&mut sockets);
        }
        sockets.push(Entry {
            id,
            sock: socket.into(),
        });
        tracing::debug!(count = sockets.len(), "socket registered");
        id
    }

    /// Removes the entry for `id` and closes the registry's fd for it.
    ///
    /// This is the RAII deregistration half of the lifecycle: the connector's
    /// transport guard calls it on `Drop` when ureq discards a connection, so
    /// the registry's duplicate fd dies exactly when the connection does.
    ///
    /// ## The single-ownership-transfer invariant (why no-op is correct)
    ///
    /// An entry is REMOVED as the first step of every path that closes its
    /// fd: `unregister` itself, `shutdown_all`, and `prune_locked`. Removal
    /// is therefore the one and only "this fd's close has been handed off to
    /// whichever code found the entry" signal. If `id` is not present —
    /// because `shutdown_all`/`prune_dead` already removed and closed it —
    /// `unregister` does NOTHING rather than guessing at an fd number: the
    /// guard can never double-close an fd the registry already closed (and
    /// can never close a recycled fd number belonging to something else).
    /// The two fd owners are independent (the transport holds its own fd,
    /// the registry holds its dup), so a no-op here leaves no leak: the
    /// transport's own fd is closed by normal `Drop` regardless.
    pub fn unregister(&self, id: SocketId) {
        let mut sockets = self.sockets.lock().unwrap_or_else(|e| e.into_inner());
        match sockets.iter().position(|e| e.id == id) {
            Some(pos) => {
                let entry = sockets.remove(pos);
                tracing::debug!(remaining = sockets.len(), "socket unregistered (RAII drop)");
                // Removing the entry above is what transfers close ownership
                // to this function; close our dup exactly once, logging any
                // EBADF (the socket may legitimately have died first).
                close_logged(entry.sock);
            }
            // Already gone: shutdown_all or prune_locked removed (and closed)
            // it. Deliberately a NO-OP — see the invariant in the doc above.
            None => {
                tracing::trace!(
                    ?id,
                    "unregister of unknown id; entry already removed, no-op"
                );
            }
        }
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
        use std::os::fd::AsRawFd;

        use nix::sys::socket::{Shutdown, shutdown};

        let mut sockets = self.sockets.lock().unwrap_or_else(|e| e.into_inner());
        let count = sockets.len();
        // Drain removes each entry FIRST, transferring close ownership to
        // this loop (the invariant `unregister` relies on: a concurrent or
        // later unregister of one of these ids finds nothing and no-ops).
        // Close via the shared `close_logged` helper (explicit into_raw_fd +
        // close, never Drop) so an fd already closed elsewhere surfaces as a
        // logged EBADF instead of a double-close.
        for entry in sockets.drain(..) {
            let raw = entry.sock.as_raw_fd();
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
            // do — through the same helper `prune_locked` and `unregister`
            // use, so all three close paths behave identically.
            close_logged(entry.sock);
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
        // Draining (instead of `clear()`) keeps the remove-then-close order
        // the ownership invariant expects; dropping OwnedSocket closes it.
        for entry in sockets.drain(..) {
            let _ = entry.sock;
        }
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
    ///
    /// Returns the number of dead entries removed and closed.
    #[cfg(unix)]
    pub fn prune_dead(&self) -> usize {
        let mut sockets = self.sockets.lock().unwrap_or_else(|e| e.into_inner());
        prune_locked(&mut sockets)
    }

    /// Windows no-op for now (probing needs `ioctlsocket`-based non-blocking
    /// recv; planned with the Winsock follow-up).
    #[cfg(not(unix))]
    pub fn prune_dead(&self) -> usize {
        tracing::warn!(
            "prune_dead is not implemented on this platform yet (planned Winsock follow-up)"
        );
        0
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
/// Returns the number of entries removed and closed.
#[cfg(unix)]
fn prune_locked(sockets: &mut Vec<Entry>) -> usize {
    use std::os::fd::AsFd;

    let before = sockets.len();
    // The loop walks by index because sockets.remove(i) shifts later entries
    // down — a straightforward `retain`-style iterator cannot express the
    // explicit per-entry close (which must happen in this order). Using
    // `.get(i)` instead of `sockets[i]` keeps the loop panic-free as required
    // by the workspace clippy deny on indexing; `.get` returning None is
    // unreachable here since `i` never exceeds `len` (the loop exits before
    // then, and `remove` always leaves i < len when shrinking happens).
    let mut i = 0;
    while let Some(entry) = sockets.get(i) {
        let alive = probe_alive(entry.sock.as_fd());
        if alive {
            i += 1;
            continue;
        }
        // Remove and close EXPLICITLY (into_raw_fd + close, like
        // shutdown_all and unregister) rather than dropping the OwnedFd: for
        // a socket that was already closed elsewhere (EBADF is one of the
        // "dead" verdicts), Drop would close it a second time — a real
        // double-close bug — and std's IO-safety runtime aborts the process
        // on it. The explicit path turns that into a logged EBADF. Removing
        // the entry first is also what makes a later unregister of this id a
        // no-op (the ownership-transfer invariant).
        let entry = sockets.remove(i);
        close_logged(entry.sock);
    }
    let pruned = before - sockets.len();
    if pruned > 0 {
        tracing::debug!(pruned, remaining = sockets.len(), "pruned dead sockets");
    }
    pruned
}

/// Windows cap-enforcement stand-in for the Unix probe-based prune: without
/// a liveness probe (Winsock follow-up) we cannot tell dead from alive, so
/// when the cap is hit we close-and-drop the OLDEST entries to stay bounded.
/// Same growth guarantee as the Unix path, weaker eviction policy.
#[cfg(windows)]
fn prune_locked(sockets: &mut Vec<Entry>) {
    while sockets.len() >= MAX_REGISTERED_SOCKETS {
        let oldest = sockets.remove(0);
        let _ = oldest.sock; // dropping OwnedSocket closes the handle
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

    // Shared close path for ALL entry-removal sites (shutdown_all,
    // prune_locked, unregister): one place for the explicit-close-with-
    // logging semantics, so the EBADF-tolerant behavior can never drift
    // between them.

    let raw = socket.into_raw_fd();
    if let Err(e) = close(raw) {
        tracing::debug!(fd = raw, error = %e, "close of registered socket failed");
    }
}

/// Windows variant of [`close_logged`]: `OwnedSocket`'s `Drop` closes the
/// SOCKET handle via `closesocket`, which is the closest analogue of
/// `close(fd)` Winsock gives us (the real Winsock bounded shutdown —
/// `shutdown(SD_BOTH)` + error mapping — is the WINDOWS-FOLLOW-UP). Created
/// so the RAII `unregister` (shared, uncfg'd) compiles and behaves correctly
/// on Windows too. CRITICAL: ownership must NOT be stolen from `OwnedSocket`
/// — `into_raw_socket()` TRANSFERS ownership out, and a raw SOCKET nobody
/// closes leaks the handle for the process's lifetime (exactly the bug the
/// Unix path's explicit `close(raw)` avoids). So we only LOG the handle via
/// a borrow (`as_raw_socket`), then drop the `OwnedSocket` and let `Drop` close
/// it — Drop's close errors are unobservable, which is acceptable: the Unix
/// path logs EBADF only because nix maps errno; the close-once-ownership
/// invariant this helper serves is identical on both platforms.
#[cfg(windows)]
fn close_logged(socket: OwnedSock) {
    use std::os::windows::io::AsRawSocket;

    // Borrow for logging first — the value is still owned and will be closed
    // exactly once by Drop below. `as_raw_socket` gives the numeric SOCKET
    // value without taking ownership (the equivalent of `as_raw_fd`).
    let raw = socket.as_raw_socket();
    tracing::debug!(
        handle = raw as usize,
        "registered socket closed (Winsock path)"
    );
    // Drop of `OwnedSocket` = closesocket, guaranteed once (the value was
    // never `into_raw_socket`'d, so no double-close and no leak).
    drop(socket);
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
        let id_a = registry.register(a);
        let clone = registry.clone();
        assert_eq!(registry.registered_count(), 1);
        registry.register(b);
        // Clone sees the same underlying list.
        assert_eq!(clone.registered_count(), 2);

        // Ids are distinct and increasing across the whole registry (clones
        // share the counter, so both entry points draw from one sequence).
        assert!(id_a < clone.register(pair().0));
    }

    #[test]
    fn unregister_removes_entry_and_closes_fd() {
        let registry = SocketRegistry::new();
        let (a, b) = pair();
        let id = registry.register(a);
        assert_eq!(registry.registered_count(), 1);

        // Unregister must close the registry's fd: the peer sees EOF (or a
        // reset) on its end of the pair.
        registry.unregister(id);
        assert_eq!(registry.registered_count(), 0);
        let mut buf = [0u8; 1];
        match read(b.as_fd(), &mut buf) {
            Ok(0) => {}                       // clean EOF: expected
            Err(nix::Error::ECONNRESET) => {} // also acceptable
            other => panic!("expected EOF/ECONNRESET after unregister, got {other:?}"),
        }
        // `b` is still open on our side (the close above hit the registry's
        // dup and `a`'s socketpair end); drop it to silence nothing.
    }

    #[test]
    fn unregister_unknown_id_is_noop() {
        let registry = SocketRegistry::new();
        let (a, _b) = pair();
        let id = registry.register(a);
        assert_eq!(registry.registered_count(), 1);

        // Ids are unique PER REGISTRY (documented on `SocketId`), so the
        // meaningful unknown-id cases here are: an id that was never handed
        // out by this registry (counter starts at 1, so 999 never was), and
        // an id for an entry that unregister has ALREADY removed (the exact
        // double-drop shape the RAII guard can produce). Both must be no-ops
        // that leave remaining entries intact and never touch an fd.
        registry.unregister(SocketId(999));
        registry.unregister(id); // first removal: real work
        registry.unregister(id); // second removal: must be a no-op
        assert_eq!(registry.registered_count(), 0);
    }

    #[test]
    fn unregister_after_shutdown_all_is_noop() {
        let registry = SocketRegistry::new();
        let (a, b) = pair();
        let id = registry.register(a);
        registry.shutdown_all();
        assert_eq!(registry.registered_count(), 0);

        // The fd the entry pointed at was already closed by shutdown_all.
        // If unregister were NOT a no-op here, it would close a stale fd
        // number (or, if we'd stored it, the raw number — a double-close
        // bug); the no-op is the whole invariant.
        registry.unregister(id);
        assert_eq!(registry.registered_count(), 0);
        // Sanity: the peer end still reads its own state fine (no crash from
        // a corrupted fd table is the real assertion).
        let _ = read(b.as_fd(), &mut [0u8; 1]);
    }

    #[test]
    fn unregister_after_prune_is_noop() {
        let registry = SocketRegistry::new();
        // Register a socket whose peer we then drop: the probe sees EOF, so
        // `prune_dead` legitimately removes it (robust against fd-number
        // reuse, unlike a manually closed raw fd — a recycled number could
        // accidentally probe as alive).
        let (doomed, peer) = pair();
        let dead_id = registry.register(doomed);
        drop(peer);
        // The live entry keeps BOTH ends alive so the probe keeps it.
        let (live, _peer) = pair();
        let id = registry.register(live);
        registry.prune_dead();
        assert_eq!(registry.registered_count(), 1);
        // The pruned entry's id must no-op (ownership was transferred to
        // prune_locked); the surviving entry's id must still unregister it.
        registry.unregister(dead_id); // no-op
        registry.unregister(id);
        assert_eq!(registry.registered_count(), 0);
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
