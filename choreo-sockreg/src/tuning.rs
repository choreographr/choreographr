//! TCP keepalive tuning applied to sockets right after connect.
//!
//! Rationale for the "best-effort per option" policy: the point of these
//! options is dead-peer detection on long-lived connections. If, say,
//! `TCP_KEEPINTVL` cannot be set on some exotic platform, the connection
//! still works and still has basic keepalives (from `SO_KEEPALIVE`) — failing
//! the whole connect over a tuning nicety would be worse than the disease.
//! Only the fundamental `SO_KEEPALIVE` switch is treated as fatal: without it
//! none of the other options mean anything, and a failure there usually
//! indicates the fd is not a TCP socket at all (a programming error worth
//! surfacing).

/// TCP keepalive settings for long-lived sockets.
///
/// Defaults (idle 45s, interval 10s, 5 probes, user timeout 30s) detect a
/// dead peer in well under two minutes, which is the usual sweet spot for
/// interactive control-plane connections: long enough to survive routine NAT
/// idle timeouts on short gaps, short enough that a user notices a hung
/// connection quickly.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SocketTuning;

impl SocketTuning {
    /// Master switch: keepalives on.
    pub const KEEPALIVE_ENABLED: bool = true;
    /// Seconds of idle time before the first keepalive probe goes out.
    pub const IDLE_SECS: u32 = 45;
    /// Seconds between retransmitted keepalive probes.
    pub const INTERVAL_SECS: u32 = 10;
    /// Unanswered probes before the connection is declared dead.
    pub const PROBE_COUNT: u32 = 5;
    /// Milliseconds a transmitted segment may stay unacknowledged before the
    /// kernel aborts the connection (Linux `TCP_USER_TIMEOUT`). This bounds
    /// the worst-case detection time far below what keepalives alone give.
    #[cfg(target_os = "linux")]
    pub const USER_TIMEOUT_MS: u32 = 30_000;

    /// Applies the tuning to a connected `TcpStream` (any platform).
    /// Convenience wrapper over [`Self::apply`] so callers never deal with
    /// the fd-vs-SOCKET distinction themselves.
    ///
    /// # Errors
    ///
    /// Propagates the platform [`Self::apply`] error (only the fatal
    /// `SO_KEEPALIVE` set failure; see its docs).
    pub fn apply_stream(&self, stream: &std::net::TcpStream) -> std::io::Result<()> {
        #[cfg(unix)]
        {
            use std::os::fd::AsFd;
            self.apply(stream.as_fd())
        }
        #[cfg(windows)]
        {
            use std::os::windows::io::AsSocket;
            self.apply(stream.as_socket())
        }
    }

    /// Applies the tuning to a socket fd. Unix implementation.
    ///
    /// Returns `Err` only if the fundamental `SO_KEEPALIVE` set fails (see
    /// the module docs for why the rest is best-effort with a warn! log).
    ///
    /// # Errors
    ///
    /// `Err` when the `SO_KEEPALIVE` set fails, when the follow-up get
    /// round-trip fails, or when the get shows the kernel ignored the set.
    #[cfg(unix)]
    pub fn apply(&self, fd: std::os::fd::BorrowedFd<'_>) -> std::io::Result<()> {
        use nix::sys::socket::sockopt;
        use nix::sys::socket::{getsockopt, setsockopt};

        // The one option whose failure is fatal (module docs explain why).
        setsockopt(&fd, sockopt::KeepAlive, &Self::KEEPALIVE_ENABLED)
            .map_err(std::io::Error::from)?;

        #[cfg(any(target_os = "linux", target_os = "android"))]
        {
            // Linux exposes all three keepalive timings plus a user timeout
            // as distinct sockopts.
            best_effort(
                "TCP_KEEPIDLE",
                &setsockopt(&fd, sockopt::TcpKeepIdle, &Self::IDLE_SECS),
            );
            best_effort(
                "TCP_KEEPINTVL",
                &setsockopt(&fd, sockopt::TcpKeepInterval, &Self::INTERVAL_SECS),
            );
            best_effort(
                "TCP_KEEPCNT",
                &setsockopt(&fd, sockopt::TcpKeepCount, &Self::PROBE_COUNT),
            );
            // TCP_USER_TIMEOUT specifically: nix 0.31 gates this sockopt to
            // `fuchsia | linux` (NOT android), and the same split as the
            // const below — so the set is linux-only. TCP_USER_TIMEOUT is
            // present in the android kernel (it is Linux), but nix does not
            // expose the option there; basic SO_KEEPALIVE plus the three
            // timing sockopts above still give good dead-link detection.
            #[cfg(target_os = "linux")]
            best_effort(
                "TCP_USER_TIMEOUT",
                &setsockopt(&fd, sockopt::TcpUserTimeout, &Self::USER_TIMEOUT_MS),
            );
        }

        #[cfg(target_os = "macos")]
        {
            // macOS has no TCP_KEEPIDLE/TCP_KEEPINTVL/TCP_KEEPCNT; its only
            // knob is TCP_KEEPALIVE (the idle-time constant, seconds).
            best_effort(
                "TCP_KEEPALIVE",
                &setsockopt(&fd, sockopt::TcpKeepAlive, &Self::IDLE_SECS),
            );
        }

        #[cfg(not(any(target_os = "linux", target_os = "android", target_os = "macos")))]
        {
            // Other Unixes: nix's TCP sockopt coverage varies; basic
            // SO_KEEPALIVE (set above) still applies with kernel defaults.
            tracing::debug!("no platform-specific keepalive tuning for this Unix target");
        }

        // Round-trip sanity: confirm the master switch actually stuck. Cheap
        // (one getsockopt) and catches kernels that accept-but-ignore.
        let enabled = getsockopt(&fd, sockopt::KeepAlive).map_err(std::io::Error::from)?;
        if enabled != Self::KEEPALIVE_ENABLED {
            return Err(std::io::Error::other(
                "SO_KEEPALIVE did not stick after setsockopt",
            ));
        }
        Ok(())
    }

    /// Applies the tuning to a socket handle. Windows implementation.
    ///
    /// `SO_KEEPALIVE` is the one fatal option (as on Unix); the timings go
    /// through `WSAIoctl(SIO_KEEPALIVE_VALS)`, which carries the idle time and
    /// probe interval in MILLISECONDS. Windows has no per-socket probe-COUNT
    /// knob (the `TcpMaxDataRetransmissions` registry/TCP global applies), so
    /// `PROBE_COUNT` is unused here — the same "best-effort per option" policy
    /// as the Unix path.
    ///
    /// # Errors
    ///
    /// `Err` when the `SO_KEEPALIVE` set fails, when the follow-up get fails,
    /// or when the get shows the kernel ignored the set.
    #[cfg(windows)]
    pub fn apply(&self, socket: std::os::windows::io::BorrowedSocket<'_>) -> std::io::Result<()> {
        use std::os::windows::io::AsRawSocket;

        use windows_sys::Win32::Networking::WinSock::{
            SIO_KEEPALIVE_VALS, SO_KEEPALIVE, SOCKET_ERROR, SOL_SOCKET, WSAIoctl, getsockopt,
            setsockopt, tcp_keepalive,
        };

        // SOCKET is `usize` in windows-sys but std's RawSocket differs on
        // 64-bit; `socket_handle` bridges them (see its docs).
        let raw = crate::socket_handle(socket.as_raw_socket());

        // The one option whose failure is fatal (module docs explain why). A
        // Win32 BOOL is a 4-byte u32 (1 = on).
        let enabled: u32 = 1;
        // SAFETY: `raw` is a live connected SOCKET; `enabled` is a 4-byte BOOL
        // for the SO_KEEPALIVE int option and the length matches.
        let rc = unsafe {
            setsockopt(
                raw,
                SOL_SOCKET,
                SO_KEEPALIVE,
                std::ptr::from_ref(&enabled).cast(),
                win32_opt_len(),
            )
        };
        if rc == SOCKET_ERROR {
            return Err(std::io::Error::last_os_error());
        }

        // Best-effort per-socket timings. SIO_KEEPALIVE_VALS wants a
        // tcp_keepalive { onoff, keepalivetime (ms), keepaliveinterval (ms) }.
        let vals = tcp_keepalive {
            onoff: 1,
            keepalivetime: Self::IDLE_SECS.saturating_mul(1000),
            keepaliveinterval: Self::INTERVAL_SECS.saturating_mul(1000),
        };
        let mut bytes_returned: u32 = 0;
        // SAFETY: in-buffer is the tcp_keepalive struct SIO_KEEPALIVE_VALS
        // expects; no out-buffer, no overlapped (synchronous call, so a null
        // OVERLAPPED and a None completion routine are correct).
        let rc = unsafe {
            WSAIoctl(
                raw,
                SIO_KEEPALIVE_VALS,
                std::ptr::from_ref(&vals).cast(),
                u32::try_from(std::mem::size_of::<tcp_keepalive>()).unwrap_or_default(),
                std::ptr::null_mut(),
                0,
                &raw mut bytes_returned,
                std::ptr::null_mut(),
                None,
            )
        };
        if rc == SOCKET_ERROR {
            tracing::warn!(
                error = %std::io::Error::last_os_error(),
                "best-effort SIO_KEEPALIVE_VALS could not be set"
            );
        }

        // Round-trip sanity: confirm the master switch actually stuck (mirrors
        // the Unix path's getsockopt check).
        let mut current: u32 = 0;
        let mut len = win32_opt_len();
        // SAFETY: `current` is a 4-byte BOOL out-buffer with `len` its size.
        let rc = unsafe {
            getsockopt(
                raw,
                SOL_SOCKET,
                SO_KEEPALIVE,
                std::ptr::from_mut(&mut current).cast(),
                &raw mut len,
            )
        };
        if rc == SOCKET_ERROR {
            return Err(std::io::Error::last_os_error());
        }
        if current == 0 {
            return Err(std::io::Error::other(
                "SO_KEEPALIVE did not stick after setsockopt",
            ));
        }
        Ok(())
    }
}

#[cfg(windows)]
/// The `optlen`/`socklen_t` for a 4-byte Win32 socket option (a `c_int` at the
/// FFI boundary). `size_of::<u32>()` is always 4, so the `try_from` fallback is
/// unreachable — it exists only to avoid a lossy `as` cast that clippy denies.
fn win32_opt_len() -> i32 {
    i32::try_from(std::mem::size_of::<u32>()).unwrap_or_default()
}

#[cfg(unix)]
/// Logs a failed best-effort sockopt at warn and continues — never panics,
/// never fails the enclosing `apply`. Borrows the result because `T` (the
/// sockopt value type) is not necessarily `Copy` and is never consumed here.
fn best_effort<T>(option: &str, result: &nix::Result<T>) {
    if let Err(e) = result {
        tracing::warn!(option, error = %e, "best-effort socket option could not be set");
    }
}

// Two separate cfg attrs (not `cfg(all(test, unix))`): clippy's
// `allow-expect-in-tests` only recognizes a bare `cfg(test)` when scanning
// enclosing scopes, so the combined form would leave the test helpers below
// subject to the workspace's expect/unwrap denies.
#[cfg(test)]
#[cfg(unix)]
mod tests {
    use std::net::{TcpListener, TcpStream};
    use std::os::fd::AsFd;

    use nix::sys::socket::{getsockopt, sockopt};

    use super::SocketTuning;

    /// Connected loopback TCP pair (client is the socket we tune).
    fn loopback_pair() -> (TcpStream, TcpStream) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let client = TcpStream::connect(listener.local_addr().expect("addr")).expect("connect");
        let (server, _) = listener.accept().expect("accept");
        (client, server)
    }

    #[test]
    fn tuning_applies_and_keepalive_round_trips() {
        let (client, _server) = loopback_pair();
        SocketTuning.apply(client.as_fd()).expect("apply");

        // Master switch is verifiable on every Unix.
        let enabled = getsockopt(&client.as_fd(), sockopt::KeepAlive).expect("getsockopt");
        assert!(enabled);

        // Where nix exposes getters for the timing options, verify the exact
        // values round-tripped.
        #[cfg(any(target_os = "linux", target_os = "android"))]
        {
            let idle = getsockopt(&client.as_fd(), sockopt::TcpKeepIdle).expect("get idle");
            assert_eq!(idle, SocketTuning::IDLE_SECS);
            let interval =
                getsockopt(&client.as_fd(), sockopt::TcpKeepInterval).expect("get interval");
            assert_eq!(interval, SocketTuning::INTERVAL_SECS);
            let count = getsockopt(&client.as_fd(), sockopt::TcpKeepCount).expect("get count");
            assert_eq!(count, SocketTuning::PROBE_COUNT);
            #[cfg(target_os = "linux")]
            {
                let user_timeout =
                    getsockopt(&client.as_fd(), sockopt::TcpUserTimeout).expect("get utimeout");
                assert_eq!(user_timeout, SocketTuning::USER_TIMEOUT_MS);
            }
        }
        #[cfg(target_os = "macos")]
        {
            let idle = getsockopt(&client.as_fd(), sockopt::TcpKeepAlive).expect("get idle");
            assert_eq!(idle, SocketTuning::IDLE_SECS);
        }
    }

    #[test]
    fn apply_fails_for_non_socket_fd() {
        // A regular file is not a socket: the fatal SO_KEEPALIVE set must
        // fail, proving the error path works (not silently "best-effort"ing
        // everything).
        let file = std::fs::File::create(std::env::temp_dir().join("sockreg-not-a-socket"))
            .expect("create temp file");
        assert!(SocketTuning.apply(file.as_fd()).is_err());
    }
}

// Windows twin of the Unix tuning test, kept as a SEPARATE module so each
// platform's test module is self-contained (and so the two cfg attrs can stay
// bare `cfg(test)` + `cfg(windows)` — see the note on the Unix module above
// for why the combined form breaks clippy's allow-expect-in-tests).
#[cfg(test)]
#[cfg(windows)]
mod windows_tests {
    use std::net::{TcpListener, TcpStream};
    use std::os::windows::io::{AsRawSocket, AsSocket};

    use windows_sys::Win32::Networking::WinSock::{SO_KEEPALIVE, SOL_SOCKET, getsockopt};

    use super::{SocketTuning, win32_opt_len};

    fn loopback_pair() -> (TcpStream, TcpStream) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let client = TcpStream::connect(listener.local_addr().expect("addr")).expect("connect");
        let (server, _) = listener.accept().expect("accept");
        (client, server)
    }

    #[test]
    fn tuning_applies_and_keepalive_round_trips() {
        let (client, _server) = loopback_pair();
        SocketTuning.apply_stream(&client).expect("apply");

        let mut enabled: u32 = 0;
        let mut len = win32_opt_len();
        // SAFETY: `client` is a live connected socket; `enabled` is a
        // 4-byte BOOL out-buffer and `len` its size.
        let rc = unsafe {
            getsockopt(
                crate::socket_handle(client.as_socket().as_raw_socket()),
                SOL_SOCKET,
                SO_KEEPALIVE,
                std::ptr::from_mut(&mut enabled).cast(),
                &raw mut len,
            )
        };
        assert_eq!(rc, 0, "getsockopt(SO_KEEPALIVE) failed");
        assert_eq!(enabled, 1, "SO_KEEPALIVE must be on after apply");
    }
}
