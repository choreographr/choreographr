//! Network-socket and filesystem-boundary tests for `SocketTuning`.
//!
//! Integration-tests-only by workspace Test Discipline: these bind real
//! loopback sockets and create a temp file, so they live here rather than in
//! the `src/` unit modules. Each platform's half is cfg-gated: the file
//! compiles as zero tests on every other target.
//!
//! Companion of the Windows registry integration tests in
//! `tests/windows_registry.rs` (same platform-split pattern).

use choreo_sockreg::SocketTuning;

/// Connected loopback TCP pair (client is the socket we tune).
///
/// Per the workspace's crossbeam house rule, everything below is single-
/// threaded, so no channel is needed here.
#[cfg(any(unix, windows))]
#[allow(clippy::expect_used)] // tests/ files only; see file-level note above
fn loopback_pair() -> (std::net::TcpStream, std::net::TcpStream) {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let client =
        std::net::TcpStream::connect(listener.local_addr().expect("addr")).expect("connect");
    let (server, _) = listener.accept().expect("accept");
    (client, server)
}

#[cfg(unix)]
mod unix_tests {
    use super::{SocketTuning, loopback_pair};
    use nix::sys::socket::{getsockopt, sockopt};
    use std::os::fd::AsFd;

    #[test]
    #[ignore = "integration test: binds a real loopback socket"]
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
    #[ignore = "integration test: performs filesystem I/O in the temp dir"]
    fn apply_fails_for_non_socket_fd() {
        // A regular file is not a socket: the fatal SO_KEEPALIVE set must
        // fail, proving the error path works (not silently "best-effort"ing
        // everything). Filesystem I/O => here in tests/, not src/.
        let file = std::fs::File::create(std::env::temp_dir().join("sockreg-not-a-socket"))
            .expect("create temp file");
        assert!(SocketTuning.apply(file.as_fd()).is_err());
    }
}

#[cfg(windows)]
mod windows_tests {
    use windows_sys::Win32::Networking::WinSock::{SO_KEEPALIVE, SOL_SOCKET, getsockopt};

    use super::{SocketTuning, loopback_pair};
    use std::os::windows::io::{AsRawSocket, AsSocket};

    #[test]
    #[ignore = "integration test: binds a real loopback socket"]
    fn tuning_applies_and_keepalive_round_trips() {
        let (client, _server) = loopback_pair();
        SocketTuning.apply_stream(&client).expect("apply");

        let mut enabled: u32 = 0;
        let mut len = i32::try_from(std::mem::size_of::<u32>()).unwrap_or_default();
        // SAFETY: `client` is a live connected socket; `enabled` is a
        // 4-byte BOOL out-buffer and `len` its size.
        let rc = unsafe {
            getsockopt(
                usize::try_from(client.as_socket().as_raw_socket()).unwrap_or_default(),
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
