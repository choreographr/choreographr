//! The process boundary: one bounded, blind host invocation.
//!
//! The host binary is a local IPC client started once per report, so this is
//! where the client's cost is bounded. Two rules shape it:
//!
//! - **Blind.** The child's stdin, stdout and stderr are null: nothing it prints
//!   can reach this program's terminal (which is in raw mode and owned by the
//!   TUI) or its log, and it can never block waiting for input.
//! - **Bounded, without a poll loop.** The child is owned by a short-lived
//!   reaper thread that blocks in `wait()`, and the caller waits for that
//!   hand-back on a channel raced against a timer — an event-driven wait, never
//!   a `try_wait` poll loop and never a sleep. A host that wedges therefore
//!   cannot stall the reporter: past the timeout the reaper is left to reap
//!   whenever the child finally exits, and only the latest state matters
//!   anyway.

use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::Duration;

use crossbeam_channel::{self as channel, select};

use super::ReporterConfig;

/// How long one host invocation may take before it is abandoned.
///
/// The host binary is a local IPC client, so this is generous; the bound
/// exists so a wedged host can never accumulate behind the reporter.
const INVOCATION_TIMEOUT: Duration = Duration::from_secs(2);

/// Run one host subcommand with its output discarded, waiting at most
/// [`INVOCATION_TIMEOUT`]. Returns whether the command was started.
///
/// A timed-out reaper is intentionally left blocked in `wait()` so the reporter
/// is never stalled behind a wedged host. Each such leftover is bounded by
/// `INVOCATION_TIMEOUT` and reports are human-rate, so they only accumulate
/// while a host stays permanently wedged.
pub(super) fn invoke(config: &ReporterConfig, argv: &[String], subcommand: &'static str) -> bool {
    let (child_tx, child_rx) = channel::bounded::<Child>(1);
    let (done_tx, done_rx) = channel::bounded::<()>(1);
    // The reaper is spawned BEFORE the child so a failure to spawn it costs no
    // process: without a reaper there would be nobody to wait for the child,
    // and an unreaped child is a zombie.
    let reaper = thread::Builder::new()
        .name("host-report-reap".to_string())
        .spawn(move || {
            if let Ok(mut child) = child_rx.recv() {
                let _ = child.wait();
            }
            // The reporter may already be gone after a timeout; a send to a
            // dropped channel is how that case ends.
            let _ = done_tx.send(());
        });
    let Ok(reaper) = reaper else {
        tracing::debug!(
            subcommand,
            "host report invocation skipped: no reaper thread"
        );
        return false;
    };
    let spawned = Command::new(config.bin())
        .args(argv)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
    let child = match spawned {
        Ok(child) => child,
        Err(error) => {
            // A missing or non-executable host binary is the host's problem,
            // not the session's: name the subcommand and the error kind only.
            tracing::debug!(
                subcommand,
                kind = ?error.kind(),
                "host report invocation could not start"
            );
            return false;
        }
    };
    if let Err(error) = child_tx.send(child) {
        // The reaper is gone, so nothing would wait for this child; kill and
        // reap it here rather than leak a zombie.
        let mut child = error.0;
        let _ = child.kill();
        let _ = child.wait();
        return false;
    }
    // Detaching the handle is deliberate: the reaper exits on its own once the
    // child does, and joining it here would defeat the timeout.
    drop(reaper);
    let timed_out = select! {
        recv(done_rx) -> _ => false,
        recv(channel::after(INVOCATION_TIMEOUT)) -> _ => true,
    };
    if timed_out {
        tracing::debug!(subcommand, "host report invocation exceeded its timeout");
    }
    true
}
