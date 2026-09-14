//! Autostart poll tests that wait on real time or real sockets.
//!
//! AGENTS.md's Test Discipline forbids time-based waits and filesystem/IPC
//! boundary tests in `src/` unit tests, so every `poll_until_listening` case
//! that actually sleeps (a probe that stays dead, a probe that flips live
//! mid-poll) and every real-unix-socket case lives here, marked `#[ignore]`
//! and run via `cargo test-integration`. The timing-free budget-contract
//! cases stay as unit tests in `src/autostart.rs`.

use choreo_proto::socket_listening;
use choreo_tui::autostart::poll_until_listening;
use std::time::Duration;

/// A probe that NEVER succeeds must time out — and the probe must actually
/// have run (the budget is checked before each probe, so a too-large budget
/// relative to the interval is the only thing that guarantees progress).
#[test]
#[ignore]
fn poll_returns_false_when_the_probe_never_succeeds() {
    let mut probes = 0;
    let ok = poll_until_listening(
        "unused",
        Duration::from_millis(1),
        Duration::from_millis(20),
        |_| {
            probes += 1;
            false
        },
    );
    assert!(!ok, "a never-listening socket must time out");
    assert!(probes > 0, "the probe must have run at least once");
}

/// Simulates the daemon coming up after a few failed probes (slow cold
/// start) — the success path that motivates the whole poll loop.
#[test]
#[ignore]
fn poll_returns_true_once_the_probe_flips_live() {
    let mut probes_left = 3;
    let ok = poll_until_listening(
        "unused",
        Duration::from_millis(1),
        Duration::from_secs(2),
        |_| {
            if probes_left == 0 {
                true
            } else {
                probes_left -= 1;
                false
            }
        },
    );
    assert!(ok, "a probe that flips live within the budget must succeed");
}

/// Drive `poll_until_listening` with the PRODUCTION probe
/// (`choreo_proto::socket_listening`, the same dial the connection path uses)
/// against a bound-and-listening socket in a temp dir. Cheap (one listener,
/// no processes) and it exercises the exact dial shape the spawn flow relies
/// on — including the negative half: a never-listening path reads as absent
/// through the same dial.
#[cfg(unix)]
#[test]
#[ignore]
fn poll_succeeds_against_a_real_listening_socket() {
    let dir = std::env::temp_dir().join(format!(
        "choreo-tui-autostart-test-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock is after the epoch")
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).expect("temp dir create");
    let sock = dir.join("test.sock");
    let listener = std::os::unix::net::UnixListener::bind(&sock).expect("bind");
    let path = sock.to_string_lossy().into_owned();

    assert!(
        poll_until_listening(
            &path,
            Duration::from_millis(1),
            Duration::from_secs(2),
            socket_listening
        ),
        "a real listening socket must be detected by the production dial"
    );

    // A never-listening path must also read as absent through the same dial
    // (the negative half of the contract, against the real OS).
    assert!(
        !poll_until_listening(
            &dir.join("absent.sock").to_string_lossy(),
            Duration::from_millis(1),
            Duration::from_millis(10),
            socket_listening
        ),
        "an absent socket must not be reported as live"
    );

    drop(listener);
    let _ = std::fs::remove_dir_all(&dir);
}
