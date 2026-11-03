//! The command-line contract with the hosting workspace manager.
//!
//! Everything the host is ever told is decided here: which state word and
//! one-line message a report carries, the `--seq` ordering value, the argv of a
//! report and of the release, and the resume command the host hands back to a
//! user. Keeping the whole shape in one module is what makes it auditable
//! against the host's own CLI, and what the integration suite
//! (`tests/it/host_report.rs`) records end to end.
//!
//! **One choke point for untrusted text.** [`report_for`] picks the state and
//! the raw message out of live session state; [`report_argv`] is the single
//! place a command line is assembled, and it sanitizes the message there —
//! control characters and the spoofing formats stripped, capped, and a value the
//! host's parser could mistake for an option withheld entirely.

use std::time::{SystemTime, UNIX_EPOCH};

use choreo_proto::SessionStatus;

use super::{HostReport, HostState};
use crate::state::terminal_status::tool_call_name;
use crate::terminal::status::{sanitize_value, state_for};

/// The stable reporter source this program claims.
///
/// The host requires a source that never changes between runs, and that does
/// not collide with the host's own integrations (those are namespaced with a
/// `herdr:` prefix, so a bare program name is distinct by construction).
const SOURCE: &str = "choreo-tui";

/// The display name the host shows for this program.
const AGENT_NAME: &str = "Choreographr";

/// The flag carrying the session id a resume command restores.
const ATTACH_FLAG: &str = "--attach";

/// The cap on the `--message` text, in bytes.
///
/// Tighter than the OSC record's `msg` cap: the host renders this in a narrow
/// pane label rather than a terminal-wide status line. The sanitizer runs
/// first, so the cap only ever truncates text that is already one line of
/// printable characters.
const MAX_HOST_MESSAGE_BYTES: usize = 256;

/// The report for the current state of the attached session.
///
/// `attached_session_id` is the session whose resume command the report
/// carries, `status` its live `SessionStatus`, and `login_status` the status
/// line of a live `/login` flow (which outranks the session status: a sign-in
/// waiting on a browser is `blocked` no matter what the session itself is
/// doing).
pub(super) fn report_for(
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
        message: status.and_then(tool_call_name).map(str::to_string),
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

/// The one-line `--message` text, or `None` when nothing is sent.
///
/// The same sanitizer and character class the OSC 7501 `msg` field goes
/// through (control characters and the spoofing formats stripped, truncated on
/// a char boundary), so the text is one inert line by construction. A value that
/// survives sanitization but still starts with `-` is withheld: it would be
/// read as an option by the host's own argument parser, which costs the label
/// but never the report.
fn sanitize_message(text: &str) -> Option<String> {
    let sanitized = sanitize_value(text, MAX_HOST_MESSAGE_BYTES);
    (!sanitized.is_empty() && !sanitized.starts_with('-')).then_some(sanitized)
}

/// Whether `name` is a bare program name the host can start.
///
/// An allow-list, not a deny-list: the host starts the resume command itself, so
/// its first word must be a plain file name resolvable on `PATH`. Rejecting a
/// few dangerous characters (`/`, `\`, `'`) would still admit `;`, `$`, a
/// backtick or an embedded space, any of which is worse if the host ever hands
/// the command to a shell. What a program name can legitimately contain —
/// `[A-Za-z0-9._+-]` — is the whole accepted set, and a leading `-` is refused
/// so the word can never read as an option.
fn is_plain_program_name(name: &str) -> bool {
    !name.is_empty()
        && !name.starts_with('-')
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '+' | '-'))
}

/// The `<binary> --attach <id>` resume command, or `None` when this program's
/// own name cannot form a valid one.
///
/// A name that [`is_plain_program_name`] refuses is withheld rather than sent to
/// be refused; the state report still goes out.
fn resume_argv(binary_name: &str, session_id: u64) -> Option<Vec<String>> {
    is_plain_program_name(binary_name).then(|| {
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
pub(super) fn report_argv(
    config: &super::ReporterConfig,
    report: &HostReport,
    seq: u64,
) -> Vec<String> {
    let mut argv = vec![
        "pane".to_string(),
        "report-agent".to_string(),
        config.pane().to_string(),
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
        && let Some(resume) = resume_argv(config.binary_name(), session_id)
    {
        argv.push("--agent-session-id".to_string());
        argv.push(session_id.to_string());
        argv.push("--".to_string());
        argv.extend(resume);
    }
    argv
}

/// Build the argv (without argv[0]) for the release of this pane's report.
pub(super) fn release_argv(config: &super::ReporterConfig, seq: u64) -> Vec<String> {
    vec![
        "pane".to_string(),
        "release-agent".to_string(),
        config.pane().to_string(),
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
pub(super) struct Seq {
    last: u64,
}

impl Seq {
    /// The next sequence number, given the current wall-clock millisecond.
    pub(super) fn next(&mut self, now_ms: u64) -> u64 {
        let seq = now_ms.max(self.last.saturating_add(1));
        self.last = seq;
        seq
    }
}

/// The current wall clock in milliseconds since the epoch, `0` if it is
/// unreadable (before the epoch), which the sequence helper then nudges up.
pub(super) fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| {
            u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX)
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::terminal_status::host_report::ReporterConfig;

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

        // Every name the allow-list refuses is withheld rather than sent — the
        // shell metacharacters especially, since their danger is the same as the
        // apostrophe's.
        for invalid in [
            "",
            "/usr/bin/choreo-tui",
            "C:\\choreo-tui.exe",
            "it's",
            "a\nb",
            "a b",
            "a;rm -rf /",
            "$(id)",
            "`id`",
            "\"quoted\"",
            "-dash",
        ] {
            assert_eq!(resume_argv(invalid, 1), None, "{invalid:?}");
        }

        // The characters a real file name may carry are all accepted.
        for valid in ["choreo-tui", "choreo_tui", "choreo.tui", "choreo+tui", "a1"] {
            assert!(is_plain_program_name(valid), "{valid:?}");
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
    fn a_message_the_host_would_read_as_an_option_is_withheld() {
        // The value would be parsed as a flag rather than as `--message`'s
        // value; the state still reaches the pane, only the label goes missing.
        let option_like = HostReport {
            state: HostState::Idle,
            message: Some("--seq 5".to_string()),
            session_id: None,
        };
        let argv = report_argv(&config(), &option_like, 1);
        assert!(!argv.iter().any(|arg| arg == "--message"), "{argv:?}");
        // A hyphen anywhere else is ordinary text.
        assert_eq!(sanitize_message("a - b").as_deref(), Some("a - b"));
        assert_eq!(sanitize_message("-"), None);
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
