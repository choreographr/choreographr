//! A ureq [`Connector`] that dials TCP like ureq's own `TcpConnector` but
//! additionally applies [`SocketTuning`] and registers a duplicate of every
//! live socket in a [`SocketRegistry`].
//!
//! This is what lets a control thread force-close hung HTTP connections: the
//! registry ends up holding a fd for every connection this connector opened.

use std::fmt;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::ops::Deref;
use std::time;

use ureq::unversioned::resolver::ResolvedSocketAddrs;
use ureq::unversioned::transport::{
    Buffers, ConnectionDetails, Connector, Either, LazyBuffers, NextTimeout, Transport,
};
use ureq::{Error, Timeout};

use crate::socket_registry::OwnedSock;
use crate::{SocketId, SocketRegistry, SocketTuning};

/// A [`Connector`] that registers every socket it opens.
///
/// Cheaply cloneable (the registry is); `Debug + Send + Sync + 'static` as
/// required by the [`Connector`] trait bounds.
#[derive(Debug, Clone)]
pub struct RegisteringTcpConnector {
    registry: SocketRegistry,
}

impl RegisteringTcpConnector {
    /// Creates a connector feeding the given registry.
    pub fn new(registry: SocketRegistry) -> Self {
        Self { registry }
    }
}

impl<In: Transport> Connector<In> for RegisteringTcpConnector {
    // ureq 3.4 does NOT re-export its internal `TcpTransport` (only
    // `TcpConnector`), so a bespoke connector cannot reuse it in its `Out`
    // type — we ship our own transport below (`RegisteredTcpTransport`)
    // instead. Behavior matches ureq's socket transport; ours additionally
    // keeps the registry fd alive for the connection's lifetime.
    type Out = Either<In, RegisteredTcpTransport>;

    fn connect(
        &self,
        details: &ConnectionDetails,
        chained: Option<In>,
    ) -> Result<Option<Self::Out>, Error> {
        // Same contract as ureq's TcpConnector: a chained transport (e.g. a
        // SOCKS proxy connection from an earlier connector in the chain)
        // overrides whatever we would open ourselves — we only tune and
        // register sockets WE dial. (Proxied sockets arriving via `chained`
        // are intentionally not registered; wiring that up would need the
        // proxy connector to expose its fd, which ureq does not.)
        if chained.is_some() {
            tracing::trace!("chained transport present; skipping TCP dial");
            return Ok(chained.map(Either::A));
        }

        let config = &details.config;
        let stream = try_connect(&details.addrs, details.timeout)?;

        if config.no_delay() {
            stream.set_nodelay(true)?;
        }

        // Keepalive tuning is intentionally best-effort here: a sockopt
        // hiccup must not fail an otherwise healthy connection (apply itself
        // only errors on the fatal SO_KEEPALIVE case, which on a freshly
        // connected TCP stream is effectively impossible — so the let is
        // belt-and-braces, with the warn already logged inside).
        if let Err(e) = SocketTuning.apply_stream(&stream) {
            tracing::warn!(error = %e, "socket tuning failed on fresh connection");
        }

        // Register a DUPLICATE of the stream: the registry owns the dup (and
        // will close it on unregister/shutdown_all/prune), while the returned
        // transport keeps the original. `try_clone` dups the fd; the
        // registry's owned type (OwnedFd/OwnedSocket) takes ownership of that
        // dup. If the dup fails the connection still works — we just lose
        // force-close coverage for it (and RAII deregistration, since there
        // is nothing registered), hence warn not error.
        let registration = match stream.try_clone() {
            Ok(dup) => Some(self.registry.register(OwnedSock::from(dup))),
            Err(e) => {
                tracing::warn!(error = %e, "could not duplicate stream for socket registry");
                None
            }
        };

        let buffers = LazyBuffers::new(config.input_buffer_size(), config.output_buffer_size());
        Ok(Some(Either::B(RegisteredTcpTransport::new(
            stream,
            buffers,
            registration,
            self.registry.clone(),
        ))))
    }
}

/// Dials the first reachable address.
///
/// DEVIATION from ureq's TcpConnector: ureq splits the total timeout across
/// addresses with a geometric series (curl's RFC); we give every attempt the
/// FULL remaining budget, stopping at the first success. For our use (loopback
/// / direct IP endpoints, rarely >1 resolved address) the geometric splitting
/// is complexity without payoff; revisit only if this connector ever faces
/// many-address hostnames.
fn try_connect(addrs: &ResolvedSocketAddrs, timeout: NextTimeout) -> Result<TcpStream, Error> {
    for addr in addrs {
        match dial_one(*addr, timeout) {
            Ok(stream) => return Ok(stream),
            // Try the next resolved address; the caller (ureq) reports the
            // aggregate failure if none connect.
            Err(Error::Io(e)) if e.kind() == std::io::ErrorKind::ConnectionRefused => {
                tracing::debug!(%addr, "connection refused; trying next address");
                continue;
            }
            Err(e) => return Err(e),
        }
    }
    tracing::debug!("failed to connect to any resolved address");
    Err(Error::Io(std::io::Error::new(
        std::io::ErrorKind::ConnectionRefused,
        "Connection refused",
    )))
}

fn dial_one(addr: SocketAddr, timeout: NextTimeout) -> Result<TcpStream, Error> {
    // `not_zero()` is ureq's way of saying "a timeout was configured"; a
    // zero/absent timeout means block indefinitely like plain connect().
    let result = match timeout.not_zero() {
        Some(duration) => TcpStream::connect_timeout(&addr, *duration),
        None => TcpStream::connect(addr),
    };
    result.map_err(|e| {
        // Mirror ureq's tcp.rs: a dial timeout maps to the Connect timeout
        // reason so ureq's retry/timeout machinery classifies it correctly;
        // everything else stays an Io error (ConnectionRefused is matched
        // upstream to try the next address).
        if e.kind() == std::io::ErrorKind::TimedOut {
            Error::Timeout(Timeout::Connect)
        } else {
            e.into()
        }
    })
}

/// Our own socket transport (ureq's `TcpTransport` is not public). The
/// read/write behavior mirrors ureq's socket transport: set the socket
/// timeout only when the requested one changes, map socket timeouts to
/// `Error::Timeout`, and answer `is_open` with a non-blocking peek probe.
pub struct RegisteredTcpTransport {
    stream: TcpStream,
    buffers: LazyBuffers,
    // RAII registration: the SocketId `register` handed us plus a clone of
    // the registry, both held so `Drop` can deregister. `None` when the dup
    // failed at connect time (nothing was registered, nothing to remove).
    registration: Option<(SocketId, SocketRegistry)>,
    // Memoized last-set timeouts: setting a socket timeout is a syscall per
    // call site otherwise; ureq's transport caches the same way.
    timeout_write: Option<time::Duration>,
    timeout_read: Option<time::Duration>,
}

impl RegisteredTcpTransport {
    fn new(
        stream: TcpStream,
        buffers: LazyBuffers,
        registration: Option<SocketId>,
        registry: SocketRegistry,
    ) -> Self {
        Self {
            stream,
            buffers,
            registration: registration.map(|id| (id, registry)),
            timeout_read: None,
            timeout_write: None,
        }
    }

    /// The goal here is to only cause a syscall to set the timeout if it's
    /// necessary (lifted from ureq's tcp.rs, same rationale).
    fn maybe_update_timeout(
        timeout: NextTimeout,
        previous: &mut Option<time::Duration>,
        stream: &TcpStream,
        f: impl Fn(&TcpStream, Option<time::Duration>) -> std::io::Result<()>,
    ) -> Result<(), Error> {
        let maybe_timeout: Option<time::Duration> =
            // ureq's `Duration` is its own enum wrapper (Exact/NotHappening)
            // that derefs to std's; normalize to std's Duration here so the
            // memoized state and the socket setters share one type.
            timeout.not_zero().map(|d| *d.deref());

        if maybe_timeout != *previous {
            (f)(stream, maybe_timeout)?;
            *previous = maybe_timeout;
        }

        Ok(())
    }

    /// Same liveness probe as ureq's socket transport (and our
    /// `SocketRegistry::prune_dead`): flip non-blocking, peek a byte, flip
    /// back. `WouldBlock` == alive; data or error == done.
    fn probe_open(stream: &mut TcpStream) -> bool {
        if stream.set_nonblocking(true).is_err() {
            return false;
        }
        let mut buf = [0u8; 1];
        let alive = match stream.read(&mut buf) {
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => true,
            Ok(_) => false,
            Err(_) => false,
        };
        // Always restore blocking mode; the transport is written against
        // blocking sockets (timeouts carry the deadline instead).
        let _ = stream.set_nonblocking(false);
        alive
    }
}

impl Transport for RegisteredTcpTransport {
    fn buffers(&mut self) -> &mut dyn Buffers {
        &mut self.buffers
    }

    fn transmit_output(&mut self, amount: usize, timeout: NextTimeout) -> Result<(), Error> {
        Self::maybe_update_timeout(
            timeout,
            &mut self.timeout_write,
            &self.stream,
            TcpStream::set_write_timeout,
        )?;

        let output = &self.buffers.output()[..amount];
        match self.stream.write_all(output) {
            Ok(v) => Ok(v),
            Err(e) if e.kind() == std::io::ErrorKind::TimedOut => {
                Err(Error::Timeout(timeout.reason))
            }
            Err(e) => Err(e.into()),
        }
    }

    fn await_input(&mut self, timeout: NextTimeout) -> Result<bool, Error> {
        Self::maybe_update_timeout(
            timeout,
            &mut self.timeout_read,
            &self.stream,
            TcpStream::set_read_timeout,
        )?;

        let input = self.buffers.input_append_buf();
        let amount = match self.stream.read(input) {
            Ok(v) => v,
            Err(e) if e.kind() == std::io::ErrorKind::TimedOut => {
                return Err(Error::Timeout(timeout.reason));
            }
            Err(e) => return Err(e.into()),
        };
        self.buffers.input_appended(amount);

        Ok(amount > 0)
    }

    fn is_open(&mut self) -> bool {
        Self::probe_open(&mut self.stream)
    }
}

// RAII deregistration: when ureq drops the transport (it closes pooled
// connections when the agent dies or the pool evicts), `Drop` removes the
// registry entry and closes the registry's DUPLICATE fd via
// `SocketRegistry::unregister`. Two fds refer to the same socket here: the
// transport's own `stream` fd is closed by normal `Drop` semantics on
// `TcpStream`, and `unregister` closes the registry's dup exactly once.
// Double-closing is impossible by construction: if `shutdown_all` or
// `prune_dead` already removed the entry, `unregister` is a documented no-op
// (entry removal is the single close-ownership-transfer signal).
impl Drop for RegisteredTcpTransport {
    fn drop(&mut self) {
        if let Some((id, registry)) = self.registration.take() {
            registry.unregister(id);
        }
    }
}

// ureq requires `Debug + Send + Sync + 'static` on transports; derive-free
// Debug so the TcpStream's own Debug (which shows fds) doesn't leak noise.
impl fmt::Debug for RegisteredTcpTransport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RegisteredTcpTransport")
            .field("addr", &self.stream.peer_addr().ok())
            .finish()
    }
}
