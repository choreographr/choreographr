//! Daemon autostart for the TUI's default (unix-socket) connection mode.
//!
//! There is NO pre-flight probe: the TUI connects to the daemon's unix socket
//! directly (the dial inside `choreo_client_core`), and only when that dial
//! itself fails because nothing is listening does it launch the `choreographr`
//! daemon binary (sibling of the TUI executable) with `--auto-exit` (so the
//! daemon cleans itself up when the TUI disconnects) and `--log-file` (so a
//! failed start is diagnosable from the log path the TUI reports on failure),
//! then the connection is retried. The TCP path (`--tcp-addr`) NEVER spawns
//! anything: remote daemons are not launchable from here by definition.
//!
//! All helpers are factored as pure functions over injected parameters (paths,
//! a probe closure) so they are unit-testable without real sockets; only
//! [`start_daemon`] performs the actual spawn.

use anyhow::Context;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

/// Poll interval between socket dials while waiting for the spawned daemon to
/// start listening. Short enough that startup feels instant, long enough not
/// to spin.
pub const POLL_INTERVAL: Duration = Duration::from_millis(100);

/// Total budget for waiting for the daemon's socket to appear. Generous
/// enough for a cold start on slow disks; past this the spawn is considered
/// failed and the child is killed.
pub const START_BUDGET: Duration = Duration::from_secs(5);

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
/// pid. The child's pid is unknowable before spawn via `std::process::Command`
/// (`Command` has no pre-spawn handle), so the TUI pid is the unique key that
/// distinguishes parallel spawns from different TUI instances sharing one
/// machine — each spawn gets its own log file and never clobbers another's.
pub(crate) fn daemon_log_path() -> PathBuf {
    std::env::temp_dir().join(format!("choreo-daemon-{}.log", std::process::id()))
}

/// Wait for the daemon WE JUST SPAWNED to come up: poll the socket path until
/// the injected probe reports a live listener or the budget expires. This is
/// not a pre-flight probe of a foreign daemon — it is the unavoidable wait for
/// our own child between the spawn and the connection retry. The probe is a
/// parameter (not hardcoded [`choreo_proto::socket_listening`]) so unit tests
/// can drive this with scripted closures and never sleep; the real-dial timing
/// behavior is exercised by `tests/autostart_poll.rs`.
///
/// Returns `true` only when the probe reported a live listener within the
/// budget. There is exactly one interval-sleep between probes (the first
/// probe fires immediately — a fast-starting daemon should not pay for a
/// sleep), and each sleep is clamped to the time remaining before the budget,
/// so the TOTAL wait is bounded by `budget` regardless of probe duration or
/// interval.
pub fn poll_until_listening(
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
        // Clamp the sleep to the budget's remainder so the loop cannot
        // overshoot `budget` by up to one full interval after the last failed
        // probe (the bug the doc above promises is impossible). A zero-length
        // sleep (budget exhausted right after a probe) is cheap and the
        // budget check at the top of the next iteration exits.
        let remaining = budget.saturating_sub(start.elapsed());
        std::thread::sleep(interval.min(remaining));
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

/// Start a daemon for the retrying connection attempt (the dial already found
/// nothing listening — that decision lives in `choreo_client_core`).
///
/// Runs on the TUI's connection thread while the alternate screen is active,
/// so nothing is printed to the terminal — the "starting the daemon" feedback
/// reaches the user through a status event on the UI channel (sent by the
/// connection task BEFORE this hook runs, so it is visible during the whole
/// wait), diagnostics go to the TUI log, and failure bails with actionable
/// errors naming the binary location, the log path, or the manual-start hint
/// (surfaced as the TUI's quit message).
pub(crate) fn start_daemon(socket_path: &str) -> anyhow::Result<()> {
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

    if poll_until_listening(
        socket_path,
        POLL_INTERVAL,
        START_BUDGET,
        choreo_proto::socket_listening,
    ) {
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
            name.starts_with("choreo-daemon-") && name.to_lowercase().ends_with(".log"),
            "the log name must be choreo-daemon-<pid>.log, got {name}"
        );
    }

    // ── Poll helper (injected probe — no sleeping, no real sockets) ──
    //
    // Everything here is timing-free: zero intervals/budgets make the loop
    // terminate without ever reaching the sleep, so these stay valid UNIT
    // tests. The cases that actually wait (a probe that stays dead, a probe
    // that flips live mid-poll) and every real-socket case moved to
    // tests/autostart_poll.rs per the no-time-based-waits rule for unit
    // tests.

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
}
