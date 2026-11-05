//! Reporting the attached session's state to a hosting workspace manager.
//!
//! Some workspace managers host this program in a pane and expose a per-pane
//! reporting API: the program invokes the host's own binary to say what it is
//! doing, to name the session it holds, and to hand back a command that
//! restores that session. This module keeps such a pane's state in step with
//! the session the TUI is attached to, so the host can show "this pane needs
//! you" and can bring the session back in the same pane after a restart.
//!
//! **One derivation, two consumers.** The state word here comes from
//! `crate::terminal::status::state_for` — the exact function the OSC 7501
//! record's `state` comes from — and the message is the one-line text that
//! record's `msg` carries (a live login flow's status line, or the NAME of the
//! tool a session is running). That shared derivation is this module's point:
//! the terminal and the host can never disagree about what this session is
//! doing, and neither path can carry a prompt, a model response, or a tool's
//! arguments or output. The `--message` value is sanitized at the single point
//! where a command line is assembled, exactly as the OSC record is sanitized
//! where its sequence is framed.
//!
//! **Inert unless hosted.** Nothing here runs unless the host's marker
//! environment is present, so a plain terminal launch spawns no thread, builds
//! no argv, and invokes nothing.
//!
//! **Off the UI thread.** [`Reporter::start`] spawns one serving thread fed by
//! a capacity-1 channel whose sender overwrites any unsent report (only the
//! latest state is worth sending). Dropping the last sender — the UI loop's
//! teardown — ends the loop, which releases the pane before exiting. Every
//! invocation discards the host binary's output and is bounded by a timeout;
//! a failure is logged at `debug` with the subcommand name only, because a host
//! that cannot answer must never slow the client.
//!
//! **Layout.** The pieces sit in submodules beside this one: `config` detects
//! the hosted environment and holds what the host needs to be addressed, `argv`
//! is the whole command-line contract with the host (the report argv, the
//! release argv, the resume command, and the sequence number), and `invoke` is
//! the process boundary — spawn, reap, timeout. This module owns the report
//! types, the channel and the serving thread, and re-exports
//! [`ReporterConfig`], so a second hosted layer lands as a focused diff in
//! `config` + `argv` rather than as an edit to one long file.

mod argv;
mod config;
mod invoke;

use std::thread;
use std::time::Duration;

use crossbeam_channel::{self as channel, Receiver, Sender, select};

use argv::{Seq, now_ms, release_argv, report_argv};
pub use config::ReporterConfig;
use invoke::invoke;

use crate::state::App;

/// The log/report name of the host this reporter talks to.
const HOST_NAME: &str = "herdr";

/// How long [`Reporter::shutdown`] waits for the serving thread to finish (run
/// its release and exit) before detaching it.
///
/// A host that is local IPC answers in milliseconds, so this is generous; the
/// bound exists so a wedged host can never keep the client from quitting. Past
/// it the release is left best-effort and the thread is left to finish on its
/// own as the process exits.
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(1);

/// A state word the hosting workspace manager understands.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HostState {
    /// Nothing is running: the session is ready for a prompt.
    Idle,
    /// The session is producing or consuming something.
    Working,
    /// The session cannot continue until the user acts (a sign-in hand-off).
    Blocked,
}

impl HostState {
    /// The state word as the host's command line spells it.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::Working => "working",
            Self::Blocked => "blocked",
        }
    }
}

/// One report destined for the hosting workspace manager.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HostReport {
    /// The state word to report.
    pub state: HostState,
    /// The one-line status text, when the state has one. Raw here — it is
    /// sanitized and capped when the command line is assembled.
    pub message: Option<String>,
    /// The attached session, whose resume command the report carries. `None`
    /// when nothing is attached, which reports state without a resume command.
    pub session_id: Option<u64>,
}

/// The queue a report travels on, as the UI loop holds it.
///
/// Both ends are kept so a report can OVERWRITE an unsent one: crossbeam only
/// offers that by draining the receiver, and the serving thread is normally
/// blocked in `recv` anyway, so the drain runs only while the reporter is
/// mid-invocation.
pub struct HostReportSink {
    tx: Sender<HostReport>,
    rx: Receiver<HostReport>,
}

impl HostReportSink {
    /// Queue `report`, replacing any report not yet taken.
    ///
    /// Never blocks: the UI loop calls this, and only the latest state is worth
    /// sending, so a full queue drops the queued report in favour of this one.
    /// A disconnected queue means the reporter has already shut down, which
    /// needs no handling.
    pub fn publish(&self, report: HostReport) {
        match self.tx.try_send(report) {
            Ok(()) | Err(channel::TrySendError::Disconnected(_)) => {}
            Err(channel::TrySendError::Full(report)) => {
                let _ = self.rx.try_recv();
                let _ = self.tx.try_send(report);
            }
        }
    }
}

/// The running reporter: the channel into its thread and the thread itself.
///
/// Dropping the last sender ends the serving loop, which releases the pane and
/// exits — so [`Self::shutdown`] is the release path and the only correct way
/// to stop it.
pub struct Reporter {
    tx: Sender<HostReport>,
    rx: Receiver<HostReport>,
    /// Signalled by the serving thread once it has run the release and is about
    /// to exit, so [`Self::shutdown`] can bound its wait.
    done: Receiver<()>,
    handle: Option<thread::JoinHandle<()>>,
}

impl Reporter {
    /// Start reporting to the hosting workspace manager in `config`.
    ///
    /// `None` when the serving thread cannot be spawned; the caller then has no
    /// reporter, and the feature is simply absent.
    #[must_use]
    pub fn start(config: ReporterConfig) -> Option<Self> {
        tracing::debug!(
            host = HOST_NAME,
            pane_id_len = config.pane_len(),
            resume = config.has_resume(),
            "hosting workspace manager reporter starting"
        );
        let (tx, rx) = channel::bounded::<HostReport>(1);
        // The receiver is cloned so this side can drain an unsent report when
        // the sink overwrites it; the serving thread owns its own handle.
        let serve_rx = rx.clone();
        let (done_tx, done_rx) = channel::bounded::<()>(1);
        let handle = thread::Builder::new()
            .name("host-report".to_string())
            .spawn(move || {
                serve(&config, &serve_rx);
                // Signal teardown completion so `shutdown` can stop waiting for a
                // host that has already released.
                let _ = done_tx.send(());
            })
            .map_err(|error| {
                tracing::debug!(kind = ?error.kind(), "hosting workspace manager reporter could not start");
            })
            .ok()?;
        Some(Self {
            tx,
            rx,
            done: done_rx,
            handle: Some(handle),
        })
    }

    /// Start reporting when this process runs in a host pane, else `None`.
    pub(crate) fn start_from_env() -> Option<Self> {
        Self::start(ReporterConfig::detect_from_env()?)
    }

    /// The queue the UI loop holds to publish reports on.
    #[must_use]
    pub fn sink(&self) -> HostReportSink {
        HostReportSink {
            tx: self.tx.clone(),
            rx: self.rx.clone(),
        }
    }

    /// Release the pane and wait for the reporter thread to exit.
    ///
    /// The caller must have dropped every other sender first (the UI loop
    /// clears its handle in teardown); this one's drop is then what lets the
    /// serving loop end.
    pub fn shutdown(self) {
        let Self {
            tx,
            rx,
            done,
            handle,
        } = self;
        // Dropping our senders ends the serving loop, which then runs the release
        // and signals `done`.
        drop((tx, rx));
        // Wait for that, but never let a wedged host stall quitting: past the
        // bound the thread is detached and the release is best-effort.
        let finished = select! {
            recv(done) -> _ => true,
            recv(channel::after(SHUTDOWN_TIMEOUT)) -> _ => false,
        };
        if let Some(handle) = handle
            && finished
        {
            let _ = handle.join();
        }
        // On timeout the `JoinHandle` is dropped (detached): the process is
        // already exiting, so the detached release is harmless.
    }
}

/// The reporter thread: report each queued state, then release on exit.
fn serve(config: &ReporterConfig, rx: &Receiver<HostReport>) {
    let mut seq = Seq::default();
    let mut reported = false;
    let mut last: Option<HostReport> = None;
    while let Ok(report) = rx.recv() {
        // The host already shows the previous state, so re-sending an identical
        // report would only cost another process spawn. The terminal-record
        // publisher dedupes the same way; a state or session change still goes
        // out.
        if last.as_ref() == Some(&report) {
            continue;
        }
        last = Some(report.clone());
        let argv = report_argv(config, &report, seq.next(now_ms()));
        reported |= invoke(config, &argv, "report-agent");
    }
    // The last sender is gone, so the client is exiting: release the pane so
    // the host stops showing a state this session no longer has, and stops
    // offering a resume command for a client that is already gone.
    if reported {
        let argv = release_argv(config, seq.next(now_ms()));
        invoke(config, &argv, "release-agent");
    }
    tracing::debug!(reported, "hosting workspace manager reporter stopped");
}

impl App {
    /// Queue the hosting workspace manager's report of the attached session,
    /// when a reporter is running.
    ///
    /// Called from the UI loop at the same point the OSC 7501 records are
    /// published, over the same state: the two consumers of one derivation.
    pub(crate) fn publish_host_report(&self) {
        let Some(sink) = self.host_report_sink.as_ref() else {
            return;
        };
        let report = argv::report_for(
            self.attached_session_id,
            self.attached_status.as_ref(),
            self.login.as_ref().map(|modal| modal.status.as_str()),
        );
        sink.publish(report);
    }
}
