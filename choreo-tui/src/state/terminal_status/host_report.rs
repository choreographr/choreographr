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

use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use choreo_proto::SessionStatus;
use crossbeam_channel::{self as channel, Receiver, Sender, select};

use crate::state::App;
use crate::terminal::status::{sanitize_value, state_for};

/// The stable reporter source this program claims.
///
/// The host requires a source that never changes between runs, and that does
/// not collide with the host's own integrations (those are namespaced with a
/// `herdr:` prefix, so a bare program name is distinct by construction).
const SOURCE: &str = "choreo-tui";

/// The display name the host shows for this program.
const AGENT_NAME: &str = "Choreographr";

/// The host's opt-in marker, present in every pane of a hosting session.
const ENV_ENABLED: &str = "HERDR_ENV";
/// The marker naming the host's own binary, the program a report invokes.
const ENV_BIN: &str = "HERDR_BIN_PATH";
/// The marker naming the pane this program runs in.
const ENV_PANE: &str = "HERDR_PANE_ID";

/// The log/report name of the host this reporter talks to.
const HOST_NAME: &str = "herdr";

/// The flag carrying the session id a resume command restores.
const ATTACH_FLAG: &str = "--attach";

/// The cap on the `--message` text, in bytes.
///
/// Tighter than the OSC record's `msg` cap: the host renders this in a narrow
/// pane label rather than a terminal-wide status line. The sanitizer runs
/// first, so the cap only ever truncates text that is already one line of
/// printable characters.
const MAX_HOST_MESSAGE_BYTES: usize = 256;

/// How long one host invocation may take before it is abandoned.
///
/// The host binary is a local IPC client, so this is generous; the bound
/// exists so a wedged host can never accumulate behind the reporter.
const INVOCATION_TIMEOUT: Duration = Duration::from_secs(2);

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

/// Where and how to reach the hosting workspace manager's reporting API.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReporterConfig {
    /// The host's own binary, invoked as `<bin> pane …`.
    bin: String,
    /// The pane this program runs in.
    pane: String,
    /// This program's own file name (no directory): the first word of the
    /// resume command. Empty when it could not be read, which withholds the
    /// resume command rather than sending one the host would refuse.
    binary_name: String,
}

impl ReporterConfig {
    /// Build a configuration directly (the seam the integration test uses to
    /// point the reporter at a stand-in host binary).
    #[must_use]
    pub fn new(
        bin: impl Into<String>,
        pane: impl Into<String>,
        binary_name: impl Into<String>,
    ) -> Self {
        Self {
            bin: bin.into(),
            pane: pane.into(),
            binary_name: binary_name.into(),
        }
    }

    /// Detect the hosting workspace manager this process runs in.
    ///
    /// The only place the real environment and this program's own path are
    /// read; everything testable takes them injected.
    #[must_use]
    pub(crate) fn detect_from_env() -> Option<Self> {
        Self::from_env(&|name| std::env::var(name).ok(), &own_binary_name())
    }

    /// Detect the hosted environment from injected data, or `None` when this
    /// process is not in a pane that exposes the reporting API.
    ///
    /// Every marker is required: a host that sets only some of them (or a
    /// stale inherited value) never starts the reporter, which is what keeps
    /// the whole feature opt-in.
    fn from_env(env: &impl Fn(&str) -> Option<String>, binary_name: &str) -> Option<Self> {
        let present = |name: &str| env(name).filter(|value| !value.is_empty());
        present(ENV_ENABLED)?;
        Some(Self {
            bin: present(ENV_BIN)?,
            pane: present(ENV_PANE)?,
            binary_name: binary_name.to_string(),
        })
    }
}

/// This program's own file name (no directory), for the resume command.
///
/// Empty when it cannot be read: a resume command is optional, so a program
/// that cannot name itself still reports state.
fn own_binary_name() -> String {
    std::env::current_exe()
        .ok()
        .as_deref()
        .and_then(std::path::Path::file_name)
        .and_then(std::ffi::OsStr::to_str)
        .unwrap_or_default()
        .to_string()
}

/// The report for the current state of the attached session.
///
/// `attached_session_id` is the session whose resume command the report
/// carries, `status` its live `SessionStatus`, and `login_status` the status
/// line of a live `/login` flow (which outranks the session status: a sign-in
/// waiting on a browser is `blocked` no matter what the session itself is
/// doing).
pub(crate) fn report_for(
    attached_session_id: Option<u64>,
    status: Option<&SessionStatus>,
    login_status: Option<&str>,
) -> HostReport {
    if let Some(status_line) = login_status {
        return HostReport {
            state: HostState::Blocked,
            message: Some(status_line.to_string()),
            session_id: attached_session_id,
        };
    }
    HostReport {
        state: host_state(status),
        // The OSC 7501 record's `msg` source: the tool NAME of a running tool
        // call, never its arguments, and nothing at all for the other states.
        message: status.and_then(super::tool_call_name).map(str::to_string),
        session_id: attached_session_id,
    }
}

/// The host's state word for a live session status.
///
/// Derived from [`state_for`] rather than matching `SessionStatus` again, so a
/// status the terminal reports as `working` is reported as `working` here too
/// — the shared derivation is what keeps the two consumers in agreement.
fn host_state(status: Option<&SessionStatus>) -> HostState {
    match status.map(state_for) {
        Some("working") => HostState::Working,
        // `idle` for an absent status too: with nothing known about the
        // session, the resting state is the truthful report.
        _ => HostState::Idle,
    }
}

/// The one-line `--message` text, or `None` when nothing survives.
///
/// The same sanitizer and character class the OSC 7501 `msg` field goes
/// through (control characters and the spoofing formats stripped, truncated on
/// a char boundary), so the text is one inert line by construction.
fn sanitize_message(text: &str) -> Option<String> {
    let sanitized = sanitize_value(text, MAX_HOST_MESSAGE_BYTES);
    (!sanitized.is_empty()).then_some(sanitized)
}

/// The `<binary> --attach <id>` resume command, or `None` when this program's
/// own name cannot form a valid one.
///
/// The host starts the resume command directly, so its first word must be a
/// plain program name resolvable on `PATH` — no directory separator, no
/// apostrophe, no control character. A name that cannot satisfy that is
/// withheld rather than sent to be refused; the state report still goes out.
fn resume_argv(binary_name: &str, session_id: u64) -> Option<Vec<String>> {
    let plain = !binary_name.is_empty()
        && !binary_name.contains(['/', '\\', '\''])
        && !binary_name.chars().any(crate::terminal::is_control_char);
    plain.then(|| {
        vec![
            binary_name.to_string(),
            ATTACH_FLAG.to_string(),
            session_id.to_string(),
        ]
    })
}

/// Build the argv (without argv[0]) for one state report.
///
/// This is the single choke point where a command line is assembled, so the
/// message is sanitized here and nowhere else: no caller can hand the host a
/// live control sequence, a multi-line value, or an unbounded string.
fn report_argv(config: &ReporterConfig, report: &HostReport, seq: u64) -> Vec<String> {
    let mut argv = vec![
        "pane".to_string(),
        "report-agent".to_string(),
        config.pane.clone(),
        "--source".to_string(),
        SOURCE.to_string(),
        "--agent".to_string(),
        AGENT_NAME.to_string(),
        "--state".to_string(),
        report.state.as_str().to_string(),
        "--seq".to_string(),
        seq.to_string(),
    ];
    if let Some(message) = report.message.as_deref().and_then(sanitize_message) {
        argv.push("--message".to_string());
        argv.push(message);
    }
    // The session id and the resume command are one optional group: the host
    // rejects a resume command that is not preceded by a report from the same
    // source naming its session, so the two always travel together.
    if let Some(session_id) = report.session_id
        && let Some(resume) = resume_argv(&config.binary_name, session_id)
    {
        argv.push("--agent-session-id".to_string());
        argv.push(session_id.to_string());
        argv.push("--".to_string());
        argv.extend(resume);
    }
    argv
}

/// Build the argv (without argv[0]) for the release of this pane's report.
fn release_argv(config: &ReporterConfig, seq: u64) -> Vec<String> {
    vec![
        "pane".to_string(),
        "release-agent".to_string(),
        config.pane.clone(),
        "--source".to_string(),
        SOURCE.to_string(),
        "--agent".to_string(),
        AGENT_NAME.to_string(),
        "--seq".to_string(),
        seq.to_string(),
    ]
}

/// A strictly increasing `--seq` value.
///
/// The host drops a report whose sequence number is not above the last one it
/// saw, so the value must increase across restarts too — which is why it is
/// wall-clock milliseconds rather than a counter that starts at zero. Two
/// reports inside the same millisecond (or a clock that steps backwards) are
/// nudged up by one instead of repeating or decreasing.
#[derive(Default)]
struct Seq {
    last: u64,
}

impl Seq {
    /// The next sequence number, given the current wall-clock millisecond.
    fn next(&mut self, now_ms: u64) -> u64 {
        let seq = now_ms.max(self.last.saturating_add(1));
        self.last = seq;
        seq
    }
}

/// The current wall clock in milliseconds since the epoch, `0` if it is
/// unreadable (before the epoch), which the sequence helper then nudges up.
fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| {
            u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX)
        })
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
            pane_id_len = config.pane.len(),
            resume = !config.binary_name.is_empty(),
            "hosting workspace manager reporter starting"
        );
        let (tx, rx) = channel::bounded::<HostReport>(1);
        // The receiver is cloned so this side can drain an unsent report when
        // the sink overwrites it; the serving thread owns its own handle.
        let serve_rx = rx.clone();
        let handle = thread::Builder::new()
            .name("host-report".to_string())
            .spawn(move || serve(&config, &serve_rx))
            .map_err(|error| {
                tracing::debug!(kind = ?error.kind(), "hosting workspace manager reporter could not start");
            })
            .ok()?;
        Some(Self {
            tx,
            rx,
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
        let Self { tx, rx, handle } = self;
        drop((tx, rx));
        if let Some(handle) = handle {
            let _ = handle.join();
        }
    }
}

/// The reporter thread: report each queued state, then release on exit.
fn serve(config: &ReporterConfig, rx: &Receiver<HostReport>) {
    let mut seq = Seq::default();
    let mut reported = false;
    while let Ok(report) = rx.recv() {
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

/// Run one host subcommand with its output discarded, waiting at most
/// [`INVOCATION_TIMEOUT`]. Returns whether the command was started.
///
/// The child is owned by a short-lived reaper thread that blocks in `wait()`,
/// and this thread waits for that hand-back on a channel raced against a timer
/// — an event-driven wait, never a `try_wait` poll loop. A host binary that
/// wedges therefore cannot stall the reporter: past the timeout the reaper is
/// left to reap whenever the child finally exits, and only the latest state
/// matters anyway.
fn invoke(config: &ReporterConfig, argv: &[String], subcommand: &'static str) -> bool {
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
    let spawned = Command::new(&config.bin)
        .args(argv)
        // Nothing the host prints can reach this program's terminal (which is
        // in raw mode and owned by the TUI) or its log.
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
    if child_tx.send(child).is_err() {
        // The reaper is gone, so nothing would ever reap this child; it was
        // already started, so report only that the invocation did not complete.
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
        let report = report_for(
            self.attached_session_id,
            self.attached_status.as_ref(),
            self.login.as_ref().map(|modal| modal.status.as_str()),
        );
        sink.publish(report);
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;

    /// An injected environment built from literal pairs.
    fn env_of(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let map: HashMap<String, String> = pairs
            .iter()
            .map(|(name, value)| ((*name).to_string(), (*value).to_string()))
            .collect();
        move |name| map.get(name).cloned()
    }

    fn config() -> ReporterConfig {
        ReporterConfig::new("/usr/bin/host", "pane-7", "choreo-tui")
    }

    fn report(state: HostState) -> HostReport {
        HostReport {
            state,
            message: None,
            session_id: None,
        }
    }

    /// Every `SessionStatus` variant, so a new one cannot slip past the table.
    fn all_statuses() -> Vec<SessionStatus> {
        vec![
            SessionStatus::Sleeping,
            SessionStatus::Inactive,
            SessionStatus::Sending,
            SessionStatus::Waiting,
            SessionStatus::Reasoning,
            SessionStatus::Responding,
            SessionStatus::ToolCall("shell".to_string()),
            SessionStatus::Retrying {
                attempt: 2,
                max_attempts: 5,
                delay_ms: 250,
            },
        ]
    }

    #[test]
    fn the_state_table_agrees_with_the_osc_record() {
        for status in all_statuses() {
            let expected = match state_for(&status) {
                "working" => HostState::Working,
                _ => HostState::Idle,
            };
            assert_eq!(
                host_state(Some(&status)),
                expected,
                "{status:?} must report the same state the OSC record does"
            );
        }
        // The direction the table exists for: work in progress is `working`,
        // everything at rest is `idle`.
        assert_eq!(
            host_state(Some(&SessionStatus::Responding)),
            HostState::Working
        );
        assert_eq!(
            host_state(Some(&SessionStatus::ToolCall("shell".to_string()))),
            HostState::Working
        );
        assert_eq!(
            host_state(Some(&SessionStatus::Reasoning)),
            HostState::Working
        );
        assert_eq!(host_state(Some(&SessionStatus::Inactive)), HostState::Idle);
        assert_eq!(host_state(Some(&SessionStatus::Sleeping)), HostState::Idle);
        // Nothing known about the session is the resting state.
        assert_eq!(host_state(None), HostState::Idle);
    }

    #[test]
    fn a_live_login_is_blocked_and_carries_its_status_line() {
        let report = report_for(
            Some(7),
            Some(&SessionStatus::Responding),
            Some("Waiting for the browser sign-in…"),
        );
        assert_eq!(report.state, HostState::Blocked);
        assert_eq!(
            report.message.as_deref(),
            Some("Waiting for the browser sign-in…")
        );
        assert_eq!(report.session_id, Some(7));
    }

    #[test]
    fn the_message_is_the_osc_msg_source_and_never_tool_arguments() {
        let working = report_for(
            Some(1),
            Some(&SessionStatus::ToolCall("read_file".to_string())),
            None,
        );
        assert_eq!(working.state, HostState::Working);
        assert_eq!(working.message.as_deref(), Some("read_file"));

        // A streaming turn with no tool carries no text at all: the model's
        // output never leaves the TUI.
        let streaming = report_for(Some(1), Some(&SessionStatus::Responding), None);
        assert_eq!(streaming.message, None);
        assert_eq!(streaming.state, HostState::Working);

        // Nothing attached and nothing running: idle, with no resume group.
        let resting = report_for(None, None, None);
        assert_eq!(resting.state, HostState::Idle);
        assert_eq!(resting.message, None);
        assert_eq!(resting.session_id, None);
    }

    #[test]
    fn the_resume_command_is_a_plain_program_name_and_a_bounded_argv() {
        let resume = resume_argv("choreo-tui", 42).expect("a plain name forms a resume command");
        assert_eq!(resume, vec!["choreo-tui", "--attach", "42"]);
        let first_word = resume.first().map_or("", String::as_str);
        assert!(
            !first_word.contains(['/', '\\']),
            "no directory separator: {first_word:?}"
        );
        assert!(!resume.iter().any(|arg| arg.contains('\'')));
        assert!(!resume.iter().any(|arg| arg.chars().any(char::is_control)));
        assert!(
            resume.len() <= 64,
            "the host caps a resume command at 64 args"
        );

        // A name the host could not start is withheld rather than sent.
        for invalid in [
            "",
            "/usr/bin/choreo-tui",
            "C:\\choreo-tui.exe",
            "it's",
            "a\nb",
        ] {
            assert_eq!(resume_argv(invalid, 1), None, "{invalid:?}");
        }
    }

    #[test]
    fn the_sequence_never_repeats_or_decreases() {
        let mut seq = Seq::default();
        assert_eq!(seq.next(1_000), 1_000);
        // Two reports in the same millisecond: nudged up, never repeated.
        assert_eq!(seq.next(1_000), 1_001);
        // A clock that steps backwards still increases.
        assert_eq!(seq.next(500), 1_002);
        assert_eq!(seq.next(4_000), 4_000);
    }

    #[test]
    fn no_reporter_without_every_marker() {
        let chained = [
            ("HERDR_ENV", "1"),
            ("HERDR_BIN_PATH", "/usr/bin/host"),
            ("HERDR_PANE_ID", "pane-7"),
        ];
        // Any missing marker means no config, hence no thread and no argv.
        for missing in 0..chained.len() {
            let partial: Vec<(&str, &str)> = chained
                .iter()
                .enumerate()
                .filter(|(index, _)| *index != missing)
                .map(|(_, pair)| *pair)
                .collect();
            assert_eq!(
                ReporterConfig::from_env(&env_of(&partial), "choreo-tui"),
                None,
                "{partial:?} must not start a reporter"
            );
        }
        // An empty value is not a marker claim either.
        let empty = [
            ("HERDR_ENV", ""),
            ("HERDR_BIN_PATH", ""),
            ("HERDR_PANE_ID", ""),
        ];
        assert_eq!(
            ReporterConfig::from_env(&env_of(&empty), "choreo-tui"),
            None
        );

        assert_eq!(
            ReporterConfig::from_env(&env_of(&chained), "choreo-tui"),
            Some(config())
        );
    }

    #[test]
    fn the_report_argv_carries_state_seq_and_the_resume_group() {
        let report = HostReport {
            state: HostState::Working,
            message: Some("read_file".to_string()),
            session_id: Some(42),
        };
        assert_eq!(
            report_argv(&config(), &report, 1_700_000_000_000),
            vec![
                "pane",
                "report-agent",
                "pane-7",
                "--source",
                "choreo-tui",
                "--agent",
                "Choreographr",
                "--state",
                "working",
                "--seq",
                "1700000000000",
                "--message",
                "read_file",
                "--agent-session-id",
                "42",
                "--",
                "choreo-tui",
                "--attach",
                "42",
            ]
        );
    }

    #[test]
    fn the_report_argv_omits_the_optional_parts_when_absent() {
        let bare = report_argv(&config(), &report(HostState::Idle), 7);
        assert_eq!(
            bare,
            vec![
                "pane",
                "report-agent",
                "pane-7",
                "--source",
                "choreo-tui",
                "--agent",
                "Choreographr",
                "--state",
                "idle",
                "--seq",
                "7",
            ]
        );
        assert!(!bare.iter().any(|arg| arg == "--message"));
        assert!(!bare.iter().any(|arg| arg == "--"));

        // A session with no usable program name still reports its state, and
        // the resume group is withheld as a unit.
        let no_resume = report_argv(
            &ReporterConfig::new("/usr/bin/host", "pane-7", ""),
            &HostReport {
                state: HostState::Working,
                message: None,
                session_id: Some(9),
            },
            8,
        );
        assert!(!no_resume.iter().any(|arg| arg == "--agent-session-id"));
        assert!(!no_resume.iter().any(|arg| arg == "--"));
    }

    #[test]
    fn the_message_is_sanitized_and_capped_at_the_command_line() {
        let hostile = HostReport {
            state: HostState::Working,
            message: Some("a\u{1b}]52;c;x\u{7}b\nc".to_string()),
            session_id: None,
        };
        let argv = report_argv(&config(), &hostile, 1);
        let message = argv
            .iter()
            .position(|arg| arg == "--message")
            .and_then(|index| argv.get(index + 1))
            .expect("a message that survives sanitization is emitted");
        assert!(!message.chars().any(char::is_control), "{message:?}");
        assert!(!message.contains('\n'), "the host is promised one line");
        assert!(message.starts_with("a]52;c;x"), "{message:?}");

        // A cap that only ever truncates already-clean text.
        let long = HostReport {
            state: HostState::Idle,
            message: Some("x".repeat(MAX_HOST_MESSAGE_BYTES * 4)),
            session_id: None,
        };
        let argv = report_argv(&config(), &long, 1);
        let message = argv
            .iter()
            .position(|arg| arg == "--message")
            .and_then(|index| argv.get(index + 1))
            .expect("a capped message is still emitted");
        assert_eq!(message.len(), MAX_HOST_MESSAGE_BYTES);

        // Text that sanitizes away entirely emits no `--message` at all.
        let empty = HostReport {
            state: HostState::Idle,
            message: Some("\u{1b}\u{7}".to_string()),
            session_id: None,
        };
        assert!(
            !report_argv(&config(), &empty, 1)
                .iter()
                .any(|arg| arg == "--message")
        );
    }

    #[test]
    fn the_release_argv_names_the_pane_and_the_source() {
        assert_eq!(
            release_argv(&config(), 1_700_000_000_001),
            vec![
                "pane",
                "release-agent",
                "pane-7",
                "--source",
                "choreo-tui",
                "--agent",
                "Choreographr",
                "--seq",
                "1700000000001",
            ]
        );
    }
}
