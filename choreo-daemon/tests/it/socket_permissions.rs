//! Access control on the daemon's Unix socket — the fully-trusted local domain.
//!
//! A peer that reaches the socket is trusted without transport authentication
//! (it can read transcripts, retrieve plaintext credentials while the keystore
//! is unlocked, run tools, and add credentials), so the socket is bound `0600`
//! inside a directory only its owner can traverse, and on Linux the peer uid is
//! additionally verified with `SO_PEERCRED`.
//!
//! Binding a real socket and inspecting the resulting filesystem modes is an
//! IPC/filesystem boundary test, so it lives in this integration suite rather
//! than in a `src/` unit module.
#![cfg(unix)]

use choreo_daemon::run_server;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::thread;
use std::time::{Duration, Instant};

use crate::common;

/// The permission bits of `path` (the low nine bits — the type bits are not
/// part of the access decision).
fn mode_of(path: &Path) -> std::io::Result<u32> {
    Ok(std::fs::metadata(path)?.permissions().mode() & 0o777)
}

/// Wait (bounded) until `path` exists with exactly `want` permission bits.
///
/// The chmod runs immediately after the bind on the server thread, so the file
/// can briefly appear with its umask mode; polling for the final mode keeps the
/// assertion honest without a fixed sleep, and the deadline makes a daemon that
/// never tightens the socket fail loudly instead of hanging.
fn wait_for_mode(path: &Path, want: u32) {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if mode_of(path).is_ok_and(|mode| mode == want) {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for {} to reach mode {want:o} (currently {:o})",
            path.display(),
            mode_of(path).unwrap_or(0)
        );
        thread::sleep(Duration::from_millis(10));
    }
}

#[test]
#[ignore = "integration"]
fn bound_socket_is_0600_inside_an_owner_only_directory() {
    let tmp = tempfile::tempdir().expect("tempdir for socket");
    // A `run/` that does not exist yet, so the daemon creates it — the shape a
    // `--base-dir` install and the per-uid temp fallback both have.
    let run_dir = tmp.path().join("run");
    let socket_path = run_dir.join("choreographr.sock");
    let socket_str = socket_path.to_str().expect("valid socket path").to_string();

    let state = common::test_daemon_state();
    // Dummy transport key and an empty ACL: no TCP listener is configured, so
    // nothing here exercises the Noise path.
    let transport_sk = choreo_transport::key::TransportSecretKey::new([0u8; 32]);
    let acl = choreo_daemon::server::acl::SharedAcl::load(&tmp.path().join("no-acl.toml"));

    let handle = thread::spawn(move || {
        run_server(&socket_str, state, None, None, transport_sk, &acl, false)
    });

    wait_for_mode(&socket_path, 0o600);
    assert_eq!(
        mode_of(&run_dir).expect("run dir metadata"),
        0o700,
        "the directory the daemon created for its socket must be owner-only"
    );

    // Graceful shutdown via SIGINT — the same mechanism the lifecycle tests use
    // (delivering SIGINT to our own pid reaches the handler `run_server`
    // registered).
    let _ = rustix::process::kill_process(rustix::process::getpid(), rustix::process::Signal::INT);
    let result = handle.join().expect("server thread panicked");
    if let Err(e) = result {
        panic!("run_server exited with an error during shutdown: {e}");
    }
}
