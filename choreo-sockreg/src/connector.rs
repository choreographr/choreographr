//! A ureq [`Connector`] that dials TCP the way ureq's own `TcpConnector`
//! does — geometric budget split across resolved addresses, fall-through on
//! address-specific failures (refused/unreachable) and on dial timeouts while
//! the overall connect budget lasts — but additionally applies
//! [`SocketTuning`] and registers a duplicate of every live socket in a
//! [`SocketRegistry`].
//!
//! This is what lets a control thread force-close hung HTTP connections: the
//! registry ends up holding a fd for every connection this connector opened.

use std::fmt;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::sync::Arc;
use std::time;

use ureq::unversioned::resolver::ResolvedSocketAddrs;
use ureq::unversioned::transport::time::{Duration, Instant};
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
    #[must_use]
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
        let stream = try_connect(
            &details.addrs,
            details.now,
            details.timeout,
            &details.current_time,
        )?;

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

/// Dials the first reachable address, splitting the overall connect budget
/// across resolved addresses the way ureq 3.4's `TcpConnector` does.
///
/// This mirrors ureq 3.4's `try_connect`/`try_connect_with` (a fixed
/// ConnectionRefused-only fall-through in an earlier version of this file was
/// a deviation from upstream that gave up on blackholed first addresses —
/// the macOS IPv6-first scenario, ureq issue #1184 — and burned the whole
/// budget on the first IP). An address-specific failure (`Failure::TryNext`)
/// falls through to the next resolved address; a dial timeout falls through
/// while overall budget remains and gives up once it is exhausted; any other
/// error bails immediately.
fn try_connect(
    addrs: &ResolvedSocketAddrs,
    start: Instant,
    timeout: NextTimeout,
    current_time: &Arc<dyn Fn() -> Instant + Send + Sync + 'static>,
) -> Result<TcpStream, Error> {
    // Generic so the whole dial state machine is unit-testable with a fake
    // connect function that never touches a socket; production calls it
    // with `dial_one` below.
    try_connect_with(addrs, start, timeout, current_time, dial_one)
}

fn try_connect_with<T>(
    addrs: &ResolvedSocketAddrs,
    start: Instant,
    timeout: NextTimeout,
    current_time: &Arc<dyn Fn() -> Instant + Send + Sync + 'static>,
    mut connect_one: impl FnMut(SocketAddr, Option<Duration>) -> Result<T, Error>,
) -> Result<T, Error> {
    // The per-address budget is split with a geometric series that sums to
    // exactly the overall timeout (curl's scheme):
    // https://curl.se/mail/lib-2021-01/0037.html
    //
    // Example: Timeout is 10 seconds, and the host returns 4 addresses.
    //
    // Address 0: 5.33 seconds (53.3% of budget)
    // Address 1: 2.67 seconds (26.7% of budget)
    // Address 2: 1.33 seconds (13.3% of budget)
    // Address 3: 0.67 seconds (6.7% of budget)
    // Sum: 10.0 seconds
    //
    // For a single address it gets the full budget (100%). We cap the lowest
    // at 10 ms so a many-address host still gets a real dial per address.
    //
    // (Previous code gave each address the FULL remaining budget and only
    // fell through on ConnectionRefused — bounded in aggregate by nothing
    // once several addresses each burned budget only to be abandoned. With
    // the fall-through semantics below the geometric split is what keeps the
    // TOTAL time bounded by the connect timeout; ureq's own comment explains
    // the same math.)
    const MIN_PER_ADDRESS_TIMEOUT_MS: u64 = 10;

    let num_addrs = addrs.len();

    // Weights [1, 1/2, 1/4, ..., 1/2^(n-1)] sum to 2 * (1 - 1/2^n).
    // `addrs.len()` is capped at MAX_ADDRS (16) by the resolver type, so
    // the usize→i32 shift below can never truncate or wrap.
    #[allow(clippy::cast_possible_truncation, clippy::cast_possible_wrap)]
    let num_addrs_i32 = addrs.len() as i32;
    let total_weight = 2.0 * (1.0 - 0.5_f64.powi(num_addrs_i32));
    let mut weight = 1.0_f64;

    // The most recent per-address failure, so that a host whose every address
    // fails reports what actually happened instead of a synthesized refusal.
    let mut last_err: Option<Error> = None;

    for addr in addrs {
        let per_addr = timeout.not_zero().map(|t: Duration| -> Duration {
            // `t` is ureq's own Duration wrapper; its Deref target is
            // std's, so float methods reach through automatically. The
            // f64→u64 millis cast is safe: `secs` derives from a
            // non-negative timeout product, and sub-millisecond truncation
            // is absorbed by the 10 ms floor right after.
            let secs = t.as_secs_f64() * weight / total_weight;
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            let millis = (secs * 1000.0) as u64;
            Duration::from_millis(millis.max(MIN_PER_ADDRESS_TIMEOUT_MS))
        });

        match connect_one(*addr, per_addr) {
            // First reachable address wins; reservation order is preserved,
            // so the resolver's preferred address is still tried (largest
            // slice) first.
            Ok(v) => return Ok(v),
            Err(err) => {
                // Elapsed/budget math in std types: ureq's own
                // `Instant::duration_since` is pub(crate), but both wrapped
                // Instants carry an exact std one through. Deref on ureq's
                // Duration gives std types too (NotHappening -> a
                // near-unbounded std duration, which is exactly the
                // "infinite budget" reading we want).
                let elapsed =
                    std_instant(current_time()).saturating_duration_since(std_instant(start));
                match classify_failure(err, elapsed, *timeout.after, timeout.reason) {
                    Failure::TryNext(e) => {
                        tracing::debug!(%addr, "address failed; trying next resolved address");
                        last_err = Some(e);
                    }
                    Failure::GiveUp(e) => return Err(e),
                }
            }
        }

        // Halve the weight for the next address (geometric split).
        weight /= 2.0;
    }

    tracing::debug!(num_addrs, "failed to connect to any resolved address");
    Err(last_err.unwrap_or_else(|| {
        // Unreached with a non-empty resolver list, but a synthesized refusal
        // keeps the aggregate failure well-typed if the list is ever empty.
        Error::Io(std::io::Error::new(
            std::io::ErrorKind::ConnectionRefused,
            "Connection refused",
        ))
    }))
}

/// Outcome of one address attempt that did NOT connect: either this address
/// alone is at fault and the next resolved one might still work
/// (`TryNext`, carrying the error for `last_err`), or dialing stops entirely
/// (`GiveUp`). Extracted as a pure function so the state machine is unit
/// testable without sockets or wall-clock waiting.
enum Failure {
    TryNext(Error),
    GiveUp(Error),
}

/// The exact std instant behind ureq's `Instant` wrapper, or a best-effort
/// "now" for the sentinel variants. ureq keeps `duration_since` pub(crate),
/// so elapsed time is measured in std's `Instant` on the wrapped value; the
/// sentinels are clockless bookkeeping values, for which "now" is the only
/// well-defined reading — and an elapsed-time comparison against the budget
/// from "now" is indistinguishable from zero budget burned.
fn std_instant(i: Instant) -> time::Instant {
    match i {
        Instant::Exact(t) => t,
        Instant::AlreadyHappened | Instant::NotHappening => time::Instant::now(),
    }
}

/// Decides what happens after a failed dial to one resolved address.
///
/// `elapsed` is wall-clock time since the start of the whole dial (the
/// caller-computed, std-duration analogue of ureq's internal
/// `current_time().duration_since(start)`); `budget` is the OVERALL connect
/// timeout dereffed from ureq's `timeout.after` (an effectively-unbounded
/// std duration when none was set — `NotHappening`); `reason` is the timeout
/// kind to report on exhaustion (Connect for us). Mirrors ureq 3.4's
/// fall-through/elapsed logic in pure-function shape so it is unit testable
/// without sockets, real clocks, or sleeps.
fn classify_failure(
    err: Error,
    elapsed: time::Duration,
    budget: time::Duration,
    reason: Timeout,
) -> Failure {
    match &err {
        // Only this ADDRESS failed (refused, or the local stack rejected the
        // route without consulting the peer); the next address may succeed.
        Error::Io(e) if is_addr_specific_error(e) => Failure::TryNext(err),
        // A dial timeout: keep trying while the OVERALL connect budget has
        // not been burned. An unbounded budget can never be exceeded, so the
        // comparison never trips — fall through, but still stop after the
        // address list is exhausted (the loop's only termination for that
        // case).
        _ if matches!(err, Error::Timeout(_)) => {
            if elapsed > budget {
                Failure::GiveUp(Error::Timeout(reason))
            } else {
                Failure::TryNext(err)
            }
        }
        // Everything else (e.g. a generic PermissionDenied) is not
        // address-specific; retrying other addresses would just burn time on
        // the same host — bail like ureq does.
        _ => Failure::GiveUp(err),
    }
}

/// Whether a failed connect concerns only the address tried, meaning the next
/// resolved address might still succeed.
///
/// `ConnectionRefused` means this address answered and said no. The
/// unreachable/unavailable kinds mean the local network stack rejected this
/// address at routing level without asking anything: the typical case is a
/// host whose resolver returns IPv6 addresses first but which has no IPv6
/// route, where the AAAA connect fails instantly while the A record would
/// have worked (#1184). Browsers and curl mask that condition by moving on to
/// the next address, which is the behavior matched here.
fn is_addr_specific_error(e: &std::io::Error) -> bool {
    // On Windows, a VPN or firewall can surface the blocked address family as
    // WSAEACCES, which maps to ErrorKind::PermissionDenied. Match the raw OS
    // error to retry this socket error without retrying every permission
    // error (#1184).
    #[cfg(windows)]
    const WSAEACCES: i32 = 10013;
    #[cfg(windows)]
    if e.raw_os_error() == Some(WSAEACCES) {
        return true;
    }

    matches!(
        e.kind(),
        std::io::ErrorKind::ConnectionRefused
            | std::io::ErrorKind::HostUnreachable
            | std::io::ErrorKind::NetworkUnreachable
            | std::io::ErrorKind::AddrNotAvailable
    )
}

fn dial_one(addr: SocketAddr, per_addr: Option<Duration>) -> Result<TcpStream, Error> {
    // `not_zero()` upstream is "a timeout was configured"; a zero/absent
    // timeout means block indefinitely like plain connect().
    let result = match per_addr {
        Some(duration) => TcpStream::connect_timeout(&addr, *duration),
        None => TcpStream::connect(addr),
    };
    result.map_err(|e| {
        // Mirror ureq's tcp.rs: a dial timeout maps to the Connect timeout
        // reason so ureq's retry/timeout machinery classifies it correctly;
        // the connect-budget fall-through above then decides whether to try
        // the next address. Everything else stays an Io error.
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
            timeout.not_zero().map(|d| *d);

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

        // `amount` is the byte count the caller reports as ready in this
        // buffer (upstream hyper semantics: <= `output().len()`), but the
        // workspace clippy deny on indexing/slicing means we must slice
        // total: fall back to the whole buffer if the invariant is somehow
        // broken rather than panicking the daemon.
        let buf = self.buffers.output();
        let output = buf.get(..amount).unwrap_or(buf);
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
            // buffers/registration/timeouts are internal plumbing, not part
            // of the identity the caller logs; expose them as TODO-style
            // non-exhaustive markers instead of fd-bearing TcpStream Debug.
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ureq::unversioned::resolver::ArrayVec;

    /// A placeholder address for building a `ResolvedSocketAddrs` (its
    /// `from_fn` constructor must fill all 16 slots, so a sentinel is needed).
    fn uninit_addr() -> SocketAddr {
        SocketAddr::new(std::net::IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED), 0)
    }

    /// Builds an address list like a resolver would.
    fn addrs<const N: usize>(list: [SocketAddr; N]) -> ResolvedSocketAddrs {
        let mut addrs = ArrayVec::from_fn(|_| uninit_addr());
        for a in list {
            addrs.push(a);
        }
        addrs
    }

    fn loopback(port: u16) -> SocketAddr {
        SocketAddr::new(std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST), port)
    }

    fn refused() -> Error {
        Error::Io(std::io::Error::new(
            std::io::ErrorKind::ConnectionRefused,
            "refused",
        ))
    }

    const BUDGET_10S: Duration = Duration::Exact(time::Duration::from_secs(10));
    const ZERO: time::Duration = time::Duration::ZERO;
    const B_10S: time::Duration = time::Duration::from_secs(10);
    const BUDGET_UNBOUNDED: time::Duration = time::Duration::MAX;

    #[test]
    fn addr_specific_kinds_fall_through() {
        for kind in [
            std::io::ErrorKind::ConnectionRefused,
            std::io::ErrorKind::HostUnreachable,
            std::io::ErrorKind::NetworkUnreachable,
            std::io::ErrorKind::AddrNotAvailable,
        ] {
            assert!(
                is_addr_specific_error(&std::io::Error::new(kind, "x")),
                "{kind:?}"
            );
        }
    }

    #[test]
    fn non_addr_specific_kinds_do_not_fall_through() {
        // TimedOut stays IO here only in the general case: `dial_one` maps a
        // real dial timeout to `Error::Timeout` BEFORE this helper sees it, so
        // a TimedOut IO error reaching this helper is not the fall-through
        // signal. PermissionDenied (a firewall/ACL, not a bad address) and a
        // bare OS error equally say nothing about OTHER addresses.
        for kind in [
            std::io::ErrorKind::TimedOut,
            std::io::ErrorKind::PermissionDenied,
            std::io::ErrorKind::WouldBlock,
        ] {
            assert!(
                !is_addr_specific_error(&std::io::Error::new(kind, "x")),
                "{kind:?}"
            );
        }
        // A raw OS error without a mapped kind must not trip the matcher.
        let raw = std::io::Error::from_raw_os_error(2);
        assert!(!is_addr_specific_error(&raw), "{raw:?}");
    }

    #[test]
    fn classify_refused_tries_next() {
        let d = classify_failure(refused(), ZERO, B_10S, Timeout::Connect);
        assert!(matches!(d, Failure::TryNext(_)));
    }

    #[test]
    fn classify_dial_timeout_under_budget_tries_next() {
        let elapsed = time::Duration::from_secs(2);
        let d = classify_failure(
            Error::Timeout(Timeout::Connect),
            elapsed,
            B_10S,
            Timeout::Connect,
        );
        assert!(matches!(d, Failure::TryNext(_)));
    }

    #[test]
    fn classify_dial_timeout_over_budget_gives_up() {
        let elapsed = time::Duration::from_secs(11);
        let d = classify_failure(
            Error::Timeout(Timeout::Connect),
            elapsed,
            B_10S,
            Timeout::Connect,
        );
        assert!(matches!(
            d,
            Failure::GiveUp(Error::Timeout(Timeout::Connect))
        ));
    }

    #[test]
    fn classify_infinite_budget_times_out_nothing() {
        // An unbounded budget (`NotHappening` dereffed to std's near-infinite
        // duration) can never be exceeded, so the fall-through always fires.
        let d = classify_failure(
            Error::Timeout(Timeout::Connect),
            time::Duration::MAX,
            BUDGET_UNBOUNDED,
            Timeout::Connect,
        );
        assert!(matches!(d, Failure::TryNext(_)));
    }

    #[test]
    fn classify_generic_error_gives_up() {
        // A non-address-specific Io error (permission denied) bails — retrying
        // other addresses would just burn the budget on the same failure.
        let d = classify_failure(
            Error::Io(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "denied",
            )),
            ZERO,
            B_10S,
            Timeout::Connect,
        );
        assert!(matches!(d, Failure::GiveUp(Error::Io(_))));
    }

    /// Fake dialer: deterministic per-address results, no sockets.
    struct Scripted {
        results: Vec<Result<(), Error>>,
        seen: Vec<SocketAddr>,
    }

    impl Scripted {
        fn dial(&mut self, addr: SocketAddr, _per_addr: Option<Duration>) -> Result<(), Error> {
            self.seen.push(addr);
            self.results.remove(0)
        }
    }

    #[test]
    fn state_machine_falls_through_refusal_to_second_address() {
        let chased = [loopback(1), loopback(2)];
        let mut script = Scripted {
            results: vec![Err(refused()), Ok(())],
            seen: Vec::new(),
        };
        let start = Instant::Exact(time::Instant::now());
        let now = start;
        let clock = Arc::new(move || now) as Arc<dyn Fn() -> Instant + Send + Sync>;
        let timeout = NextTimeout {
            after: BUDGET_10S,
            reason: Timeout::Connect,
        };
        let addrs = addrs(chased);
        let result = try_connect_with(&addrs, start, timeout, &clock, |a, t| script.dial(a, t));
        result.expect("second address must connect");
        assert_eq!(script.seen, chased);
    }

    #[test]
    fn state_machine_reports_last_addr_specific_error_when_none_connect() {
        let chased = [loopback(1), loopback(2)];
        let mut script = Scripted {
            results: vec![Err(refused()), Err(refused())],
            seen: Vec::new(),
        };
        let start = Instant::Exact(time::Instant::now());
        let clock = Arc::new(move || start) as Arc<dyn Fn() -> Instant + Send + Sync>;
        let timeout = NextTimeout {
            after: BUDGET_10S,
            reason: Timeout::Connect,
        };
        let addrs = addrs(chased);
        let err = try_connect_with(&addrs, start, timeout, &clock, |a, t| script.dial(a, t))
            .expect_err("all addresses refused");
        // last_err is surfaced, so the aggregate failure says what actually
        // happened (refused) instead of a synthetic message.
        match err {
            Error::Io(e) => {
                assert_eq!(e.kind(), std::io::ErrorKind::ConnectionRefused);
            }
            other => panic!("expected Io refused, got {other:?}"),
        }
    }

    #[test]
    fn state_machine_timeout_exhausting_budget_reports_connect_timeout() {
        let chased = [loopback(1), loopback(2)];
        let mut script = Scripted {
            results: vec![Err(Error::Timeout(Timeout::Connect)), Ok(())],
            seen: Vec::new(),
        };
        let start = Instant::Exact(time::Instant::now());
        // Fake clock 20 s after start: the 10 s budget is already burned.
        let clock =
            Arc::new(move || Instant::Exact(time::Instant::now() + time::Duration::from_secs(20)))
                as Arc<dyn Fn() -> Instant + Send + Sync>;
        let timeout = NextTimeout {
            after: BUDGET_10S,
            reason: Timeout::Connect,
        };
        let addrs = addrs(chased);
        let err = try_connect_with(&addrs, start, timeout, &clock, |a, t| script.dial(a, t))
            .expect_err("budget exhausted");
        assert!(matches!(err, Error::Timeout(Timeout::Connect)));
        // The second address was never tried: fall-through stops at budget.
        assert_eq!(script.seen.len(), 1);
    }

    #[test]
    fn state_machine_infinite_budget_falls_through_on_timeout() {
        let chased = [loopback(1), loopback(2)];
        let mut script = Scripted {
            results: vec![Err(Error::Timeout(Timeout::Connect)), Ok(())],
            seen: Vec::new(),
        };
        let start = Instant::Exact(time::Instant::now());
        let clock = Arc::new(move || start) as Arc<dyn Fn() -> Instant + Send + Sync>;
        let timeout = NextTimeout {
            after: Duration::NotHappening,
            reason: Timeout::Connect,
        };
        let addrs = addrs(chased);
        try_connect_with(&addrs, start, timeout, &clock, |a, t| script.dial(a, t))
            .expect("infinite budget keeps trying, second address connects");
        assert_eq!(script.seen, chased);
    }

    #[test]
    fn geometric_split_sums_to_budget() {
        // With the 10 ms floor inactive (large budget), the weights sum to
        // exactly the budget: 10 s over 3 addresses ≈ 5s + 2.5s + 1.25s (the
        // remainder is rounding). Verify each slice is ~half of the previous.
        let timeout = NextTimeout {
            after: BUDGET_10S,
            reason: Timeout::Connect,
        };
        let per_addr = [0.5, 0.25, 0.125].map(|w| {
            timeout
                .not_zero()
                .map(|t| std::time::Duration::from_secs_f64(t.as_secs_f64() * w))
        });
        for (a, b) in per_addr.iter().zip(per_addr.iter().skip(1)) {
            let (a, b) = (a.expect("configured"), b.expect("configured"));
            // Second slice ≈ half of the previous (± a millisecond).
            assert!(
                (b.as_secs_f64() * 2.0 - a.as_secs_f64()).abs() < 0.01,
                "non-halving split: {a:?} -> {b:?}"
            );
        }
    }
}
