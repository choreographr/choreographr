//! Daemon autostart for the TUI's default (unix-socket) connection mode.
//!
//! When the TUI starts and nothing is listening on the daemon's unix socket,
//! it launches the `choreographr` daemon binary itself (sibling of the TUI
//! executable) with `--auto-exit` (so the daemon cleans itself up when the
//! TUI disconnects) and `--log-file` (so a failed start is diagnosable from
//! the log path the TUI reports on failure). The TCP path (`--tcp-addr`)
//! NEVER spawns anything: remote daemons are not launchable from here by
//! definition.
//!
//! All helpers are factored as pure functions over injected parameters
//! (paths, a probe closure) so they are unit-testable without real
//! processes; only [`start_daemon`] performs the actual spawn.

use anyhow::Context;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

// Same cross-platform unix-socket dialing mechanism the daemon-side
// connection path uses (choreo-client-core/src/connection.rs): std's
// UnixStream on unix, the uds_windows shim on Windows (std's Windows
// UnixStream is still unstable).
#[cfg(unix)]
use std::os::unix::net::UnixStream;
#[cfg(windows)]
use uds_windows::UnixStream;

/// Poll interval between socket probes while waiting for the spawned daemon
/// to start listening. Short enough that startup feels instant, long enough
/// not to spin.
const POLL_INTERVAL: Duration = Duration::from_millis(100);

/// Total budget for waiting for the daemon's socket to appear. Generous
/// enough for a cold start on slow disks; past this the spawn is considered
/// failed and the child is killed.
const START_BUDGET: Duration = Duration::from_secs(5);

/// Whether a daemon is currently accepting connections on the unix socket at
/// `path`. A successful connect proves a live listener; every failure
/// (missing file, stale socket, ECONNREFUSED) means "no daemon" for our
/// purposes — the daemon itself removes stale socket files at startup, so
/// the TUI never needs to distinguish stale from absent.
pub(crate) fn socket_accepting(path: &str) -> bool {
    UnixStream::connect(path).is_ok()
}

/// Path of the sibling `choreographr` daemon binary: same directory as the
/// running TUI executable, `.exe`-suffixed on Windows. Factored over the exe
/// directory so tests can pin the layout without `current_exe()`.
pub(crate) fn daemon_binary_path(exe_dir: &Path) -> PathBuf {
    let name = if cfg!(windows) {
        "choreographr.exe"
    } else {
        "choreographr"
    };
    exe_dir.join(name)
}

/// The daemon's log file path: under the PLATFORM temp dir (TMPDIR-aware —
/// see `init_file_logging` for the Termux rationale), keyed by the TUI's OWN
/// pid. The child's pid is unknowable before spawn via std::process::Command
/// (`Command` has no pre-spawn handle), so the TUI pid is the unique key that
/// distinguishes parallel spawns from different TUI instances sharing one
/// machine — each spawn gets its own log file and never clobbers another's.
pub(crate) fn daemon_log_path() -> PathBuf {
    std::env::temp_dir().join(format!("choreo-daemon-{}.log", std::process::id()))
}

/// Poll the socket path until the injected probe reports a live listener or
/// the budget expires. The probe is a parameter (not a hardcoded
/// [`socket_accepting`]) so tests can drive this without real sockets, and
/// so a caller could substitute a different liveness check if the wire
/// protocol ever needs one.
///
/// Returns `true` only when the probe reported a live listener within the
/// budget. There is exactly one interval-sleep between probes (the first
/// probe fires immediately — a fast-starting daemon should not pay for a
/// sleep), and the budget is checked before each probe so the total wait is
/// bounded by `budget` regardless of probe duration.
pub(crate) fn poll_until_listening(
    path: &str,
    interval: Duration,
    budget: Duration,
    mut probe: impl FnMut(&str) -> bool,
) -> bool {
    let start = Instant::now();
    loop {
        // Check the budget BEFORE probing: guarantees the wait is bounded
        // even if the probe itself is slow, and makes the zero-budget case
        // (tests, callers wanting a single shot) return immediately.
        if start.elapsed() >= budget {
            return false;
        }
        if probe(path) {
            return true;
        }
        std::thread::sleep(interval);
    }
}

/// Spawn the daemon binary detached from this TUI's lifecycle.
///
/// All three stdios are `Stdio::null()`: the TUI owns the terminal, so any
/// daemon stdout/stderr would corrupt the TUI display, and `--log-file`
/// (passed by the caller) already captures diagnostics. `spawn()` (no
/// `status()`/`output()`) keeps the daemon detached — the TUI does not wait
/// on it; it polls the socket instead.
fn spawn_daemon(binary: &Path, log_path: &Path) -> anyhow::Result<Child> {
    Command::new(binary)
        .arg("--auto-exit")
        .arg("--log-file")
        .arg(log_path)
        // No inheritance in any direction: the TUI's terminal is the child's
        // terminal only by accident of forking — a detached daemon must not
        // draw into it (would garble the alternate screen) and must not
        // outlive-orphan on shared stdin.
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .with_context(|| {
            format!(
                "failed to start the daemon ({}) — start it manually with `choreographr`",
                binary.display()
            )
        })
}

/// Ensure a daemon is running on the unix socket at `path`; if not, spawn
/// one and wait for its socket. This is the whole autostart flow.
///
/// Failure bails with actionable errors naming the binary location, the log
/// path, or the manual-start hint — everything a user needs to recover
/// without reading the TUI source.
pub(crate) fn start_daemon(socket_path: &str) -> anyhow::Result<()> {
    println!("No daemon running — starting choreographr…");

    // Resolve the daemon binary next to THIS executable. The binary pair is
    // built into the same target dir / install prefix, so sibling lookup is
    // the only path that works for both `cargo run` and installed setups —
    // and it breaks loudly (with the directory named) when the layout
    // differs, instead of silently searching PATH.
    let exe = std::env::current_exe().context("failed to locate the TUI executable")?;
    let exe_dir = exe
        .parent()
        .context("TUI executable has no parent directory; cannot locate the daemon binary")?;
    let binary = daemon_binary_path(exe_dir);
    if !binary.is_file() {
        anyhow::bail!(
            "the daemon binary was not found at {} — start the daemon manually with `choreographr`",
            binary.display()
        );
    }

    let log_path = daemon_log_path();
    tracing::info!(binary = %binary.display(), log = %log_path.display(), "spawning daemon");
    let mut child = spawn_daemon(&binary, &log_path)?;

    if poll_until_listening(socket_path, POLL_INTERVAL, START_BUDGET, socket_accepting) {
        tracing::info!(path = socket_path, "daemon started and listening");
        return Ok(());
    }

    // The TUI owns cleaning up its own failed spawn: kill the child and reap
    // it so no zombie is left. Both are best-effort — a child that already
    // exited (the common failure: it died on startup, e.g. a port/socket
    // conflict) makes `kill()` fail harmlessly; a reap failure is
    // unobservable from here.
    let _ = child.kill();
    let _ = child.wait();
    anyhow::bail!(
        "the daemon did not start listening on {socket_path} within {}s — check the daemon log at {}",
        START_BUDGET.as_secs(),
        log_path.display()
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── Sibling-binary path helper ───────────────────────────────

    #[cfg(unix)]
    #[test]
    fn daemon_binary_is_a_plain_sibling_on_unix() {
        let path = daemon_binary_path(Path::new("/usr/local/bin"));
        assert_eq!(path, PathBuf::from("/usr/local/bin/choreographr"));
    }

    #[cfg(windows)]
    #[test]
    fn daemon_binary_gets_the_exe_suffix_on_windows() {
        let path = daemon_binary_path(Path::new(r"C:\tools"));
        assert_eq!(path, PathBuf::from(r"C:\tools\choreographr.exe"));
    }

    #[test]
    fn daemon_log_path_is_under_the_platform_temp_dir() {
        // Mirrors the TUI log-path contract: TMPDIR-aware platform temp dir,
        // pid-keyed name so parallel TUI spawns never clobber each other.
        let path = daemon_log_path();
        assert_eq!(
            path.parent(),
            Some(std::env::temp_dir().as_path()),
            "the daemon log must be under env::temp_dir(), got {path:?}"
        );
        let name = path.file_name().and_then(|n| n.to_str()).expect("utf8");
        assert!(
            name.starts_with("choreo-daemon-") && name.ends_with(".log"),
            "the log name must be choreo-daemon-<pid>.log, got {name}"
        );
    }

    // ── Poll helper (injected probe — no real sockets) ───────────

    #[test]
    fn poll_succeeds_when_the_probe_is_immediately_live() {
        assert!(poll_until_listening(
            "unused",
            Duration::ZERO,
            Duration::from_secs(1),
            |_| true
        ));
    }

    #[test]
    fn poll_fails_on_a_zero_budget_without_probing() {
        // Budget is checked BEFORE the probe, so a zero budget must return
        // false even for an always-live socket — pinning the bounded-wait
        // contract (no unbounded probe loop is possible).
        let mut probes = 0;
        let ok = poll_until_listening("unused", Duration::ZERO, Duration::ZERO, |_| {
            probes += 1;
            true
        });
        assert!(!ok, "a zero budget must never report success");
        assert_eq!(probes, 0, "the probe must not run past the budget");
    }

    #[test]
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

    #[test]
    fn poll_returns_true_once_the_probe_flips_live() {
        // Simulates the daemon coming up after a few failed probes (slow
        // cold start) — the success path that motivates the whole poll loop.
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

    // ── Poll helper against a REAL unix socket in a temp dir ─────

    /// Real-socket test: drive `poll_until_listening` with the production
    /// probe closure (`socket_accepting`) against a bound-and-listening
    /// socket in a temp dir. Cheap (one listener, no processes) and it
    /// exercises the exact probe the spawn flow relies on.
    #[cfg(unix)]
    #[test]
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
                socket_accepting
            ),
            "a real listening socket must be detected by the production probe"
        );

        // A never-listening path must also read as absent through the same
        // probe (the negative half of the contract, against the real OS).
        assert!(
            !poll_until_listening(
                &dir.join("absent.sock").to_string_lossy(),
                Duration::from_millis(1),
                Duration::from_millis(10),
                socket_accepting
            ),
            "an absent socket must not be reported as live"
        );

        drop(listener);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
