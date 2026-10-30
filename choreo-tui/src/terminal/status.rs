//! OSC 7501 Program Status Protocol.
//!
//! Reports what each session is doing to the terminal using Mitchell
//! Hashimoto's Program Status Protocol (`ESC ] 7501 ; <pairs> ST`). The body
//! is a `:`-separated list of `key=value` pairs: `state` is the only required
//! key (`idle` / `working` / `done` / `blocked` / `error` / `clear`), `kind`
//! narrows a `blocked` state to what the program waits on (`permission` /
//! `question` / `auth`), `progress` is an integer 0-100, and `title` / `msg`
//! are standard base64 of UTF-8 text.
//!
//! **State-gated keys.** The spec ignores `kind` outside `blocked` and
//! `progress` outside `working`/`blocked`, so those keys are emitted only in
//! the states where they mean something — a `working` record never carries a
//! stale `kind=auth` that would read as "waiting on the user".
//!
//! **Output only.** Nothing here ever writes the query form
//! (`ESC ] 7501 ; ? ST`) or any other sequence expecting a terminal reply:
//! crossterm's input parser consumes no OSC reply, so the body of one would be
//! parsed as individual keystrokes and typed into the user's prompt.
//!
//! **Root vs child records.** OSC 9;4 progress ([`super::progress`]) writes the
//! terminal-wide ROOT record: a terminal is permitted to map 9;4 onto the root
//! record. A 7501 report replaces its record *completely*, so if this module
//! also wrote the root record, every 9;4 update would wipe the app/state/msg
//! of a real status report. Therefore every report here is a CHILD record —
//! one per session (`id=<session_id>`) and one for a login flow (`id=login`,
//! addressed by [`RecordKey`]) — and never the root. For the same reason each
//! record names `app=choreo-tui` explicitly — a child cannot inherit `app` from
//! an unlabeled root.

use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use std::fmt::Write as _;

use choreo_proto::SessionStatus;

use super::{is_control_char, is_unsafe_display_char, osc, write};

/// Stable machine-readable program name placed on every record (children
/// cannot inherit `app` from the unlabeled OSC 9;4 root).
pub(crate) const APP: &str = "choreo-tui";

/// Protocol cap on the decoded `title` field, in bytes (spec "Limits": 192
/// decoded, 256 encoded).
const MAX_TITLE_BYTES: usize = 192;

/// Protocol cap on the decoded `msg` field, in bytes (spec "Limits": 2048
/// decoded, 2732 encoded).
const MAX_MSG_BYTES: usize = 2048;

/// The `id` of the login flow's record. The spec's id grammar is
/// `[A-Za-z0-9_.+-]{1,32}` per dot-separated segment, so a bare `login` is a
/// valid id and cannot collide with a session record (whose id is numeric).
const LOGIN_ID: &str = "login";

/// How a `blocked` record is waiting on the user.
///
/// Valid only with `state=blocked`. The protocol also defines `permission` and
/// `question`; neither has a producer in the TUI yet, so they are not modelled
/// here — an unrecognized kind can then never be emitted, since every value of
/// this type has a wire form the spec defines.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BlockedKind {
    /// The program waits on the user to finish an authentication step — a
    /// browser OAuth / device-code hand-off, say.
    Auth,
}

impl BlockedKind {
    /// The protocol `kind` value.
    fn as_str(self) -> &'static str {
        match self {
            Self::Auth => "auth",
        }
    }
}

/// Which terminal record a report addresses.
///
/// The record set is one child record per session plus one for the login flow:
/// a sign-in that cannot proceed until the user acts in a browser is a
/// program-level `blocked` state, not any single session's, so it must not
/// borrow a session's record (nor the root, which OSC 9;4 owns).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum RecordKey {
    /// A session's record.
    Session(u64),
    /// The OAuth / device-code login flow's record.
    Login,
}

impl RecordKey {
    /// The protocol `id` value this key addresses.
    pub(crate) fn protocol_id(self) -> Cow<'static, str> {
        match self {
            Self::Session(id) => Cow::Owned(id.to_string()),
            Self::Login => Cow::Borrowed(LOGIN_ID),
        }
    }
}

/// The `progress` value for a live session status, when it has an honest one.
///
/// A retrying request is the one place the TUI knows how far through the work
/// it is — attempt N of M — so it is reported as `attempt * 100 / max_attempts`.
/// The context-window usage percentage deliberately does NOT feed this key:
/// that is how full a fill gauge is, not how much of the job is done, and OSC
/// 9;4 already carries it.
pub(crate) fn progress_for(status: &SessionStatus) -> Option<u8> {
    match status {
        SessionStatus::Retrying {
            attempt,
            max_attempts,
            ..
        } if *max_attempts > 0 => {
            // Saturating: a provider reporting more attempts than its maximum
            // must not wrap into a nonsense percentage.
            let percent = attempt.saturating_mul(100) / max_attempts;
            Some(u8::try_from(percent).unwrap_or(100).min(100))
        }
        _ => None,
    }
}

/// One OSC 7501 report, before framing.
///
/// A struct rather than a positional argument list: seven fields, four of them
/// optional, would be unreadable at a call site.
pub(crate) struct StatusRecord<'a> {
    /// The protocol `id`: which record this report replaces.
    pub(crate) id: &'a str,
    /// The stable program name ([`APP`]).
    pub(crate) app: &'a str,
    /// The required `state`.
    pub(crate) state: &'a str,
    /// What a `blocked` record waits on; dropped for any other state.
    pub(crate) kind: Option<BlockedKind>,
    /// How far through a `working` / `blocked` record is, 0-100; dropped for
    /// any other state.
    pub(crate) progress: Option<u8>,
    /// Display text, sanitized and capped when framed.
    pub(crate) title: Option<&'a str>,
    /// Display text, sanitized and capped when framed.
    pub(crate) msg: Option<&'a str>,
}

impl<'a> StatusRecord<'a> {
    /// A report carrying only the always-present keys.
    pub(crate) fn new(id: &'a str, app: &'a str, state: &'a str) -> Self {
        Self {
            id,
            app,
            state,
            kind: None,
            progress: None,
            title: None,
            msg: None,
        }
    }
}

/// Map a session status to an OSC 7501 `state` value.
///
/// `done` / `error` are not representable here — `SessionStatus` has no such
/// variants — so those are supplied by the caller's override (see
/// `App::term_status_override`).
pub(crate) fn state_for(status: &SessionStatus) -> &'static str {
    match status {
        SessionStatus::Inactive | SessionStatus::Sleeping => "idle",
        SessionStatus::Sending
        | SessionStatus::Waiting
        | SessionStatus::Reasoning
        | SessionStatus::Responding
        | SessionStatus::ToolCall(_)
        | SessionStatus::Retrying { .. } => "working",
        // `SessionStatus` is `#[non_exhaustive]`; an unknown future variant is
        // reported as `idle` (the safe resting state) rather than omitted.
        _ => "idle",
    }
}

/// Standard base64 of UTF-8 text (the protocol's `msg`/`title` encoding). The
/// value charset permits `+`, `/`, and `=`, so the standard alphabet is used
/// with padding.
fn encode(text: &str) -> String {
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD.encode(text.as_bytes())
}

/// Strip control characters and the unsafe display characters, then truncate to
/// `max_bytes` on a char boundary.
///
/// The spec requires a report's decoded `title` / `msg` to contain no control
/// character and caps both by decoded byte length; a report that breaks either
/// is discarded whole, so the emitter must sanitize and cap before framing.
/// The non-joiner format class and the line/paragraph separators are stripped
/// too ([`is_unsafe_display_char`]): the spec's Security section calls for
/// disarming spoofing characters before free text is shown, a `title`/`msg` is
/// display text, and a separator would break the record's one-line shape —
/// exactly the characters the grid filter escapes. Truncation snaps to a char
/// boundary so a multi-byte character is never split.
fn sanitize_value(text: &str, max_bytes: usize) -> String {
    let mut out = String::with_capacity(text.len().min(max_bytes));
    for c in text.chars() {
        if is_control_char(c) || is_unsafe_display_char(c) {
            continue;
        }
        if out.len() + c.len_utf8() > max_bytes {
            break;
        }
        out.push(c);
    }
    out
}

/// Build the OSC 7501 child record sequence for `rec`.
///
/// The body always carries `state`, `app`, and `id`. `kind` is appended only
/// for a `blocked` record and `progress` only for `working` / `blocked`,
/// clamped to the protocol's 0-100 range — the states in which the spec makes
/// them meaningful; elsewhere they are omitted rather than sent to be ignored.
/// `title` and `msg` are appended (base64) when present and non-empty after
/// sanitization. Both are run through [`sanitize_value`] so the emitted report
/// can never exceed the protocol's per-field limit nor carry a decoded control
/// character; an empty field is dropped rather than emitted as a meaningless
/// `title=`/`msg=`.
pub(crate) fn record(rec: &StatusRecord<'_>) -> String {
    let mut body = format!("state={}:app={}:id={}", rec.state, rec.app, rec.id);
    if rec.state == "blocked"
        && let Some(kind) = rec.kind
    {
        body.push_str(":kind=");
        body.push_str(kind.as_str());
    }
    if matches!(rec.state, "working" | "blocked")
        && let Some(progress) = rec.progress
    {
        // A `String` sink cannot fail, and `write!` avoids the throwaway
        // allocation a `format!` pushed onto the body would cost.
        let _ = write!(body, ":progress={}", progress.min(100));
    }
    if let Some(title) = rec.title {
        let title = sanitize_value(title, MAX_TITLE_BYTES);
        if !title.is_empty() {
            body.push_str(":title=");
            body.push_str(&encode(&title));
        }
    }
    if let Some(msg) = rec.msg {
        let msg = sanitize_value(msg, MAX_MSG_BYTES);
        if !msg.is_empty() {
            body.push_str(":msg=");
            body.push_str(&encode(&msg));
        }
    }
    osc(7501, &body)
}

/// Build the sequence that clears the child record `key` (and its children).
pub(crate) fn clear_one(key: RecordKey) -> String {
    osc(7501, &format!("state=clear:id={}", key.protocol_id()))
}

/// Build the sequence that clears every record on the terminal.
pub(crate) fn clear_all() -> String {
    osc(7501, "state=clear")
}

/// Publishes OSC 7501 child records, suppressing no-op rewrites.
///
/// The terminal replaces a record wholesale on each report, so re-sending an
/// unchanged record every frame is pure churn — and could fight a user's
/// terminal-side dismissal. `last` caches the exact sequence published per key,
/// so [`Self::sync`] writes only records that changed (or are new) and clears
/// records whose key is no longer present.
#[derive(Default)]
pub(crate) struct Publisher {
    last: HashMap<RecordKey, String>,
}

impl Publisher {
    /// Create an empty publisher.
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Publish `desired`, diffing against what was last published.
    pub(crate) fn sync(&mut self, desired: Vec<(RecordKey, String)>) {
        let published = self.sync_with(&mut |seq| write(seq), desired);
        if published > 0 {
            tracing::debug!(
                published,
                records = self.last.len(),
                "published program status records"
            );
        }
    }

    /// Diff `desired` against `last`, emitting changes through `emit` and
    /// returning how many sequences were written.
    ///
    /// Split from [`Self::sync`] so it is unit-testable against a capturing
    /// sink instead of a real terminal.
    fn sync_with(
        &mut self,
        emit: &mut dyn FnMut(&str),
        desired: Vec<(RecordKey, String)>,
    ) -> usize {
        // A previously-published key that is absent now is stale — its session
        // went idle, was deleted, or stopped being active, or the login modal
        // closed — so clear its record rather than leave a frozen `working` /
        // `blocked` record on the terminal.
        let desired_keys: HashSet<RecordKey> = desired.iter().map(|(key, _)| *key).collect();
        let stale: Vec<RecordKey> = self
            .last
            .keys()
            .copied()
            .filter(|key| !desired_keys.contains(key))
            .collect();

        let mut published = 0;
        for key in stale {
            self.last.remove(&key);
            emit(&clear_one(key));
            published += 1;
        }
        for (key, seq) in desired {
            if self.last.get(&key).map(String::as_str) != Some(seq.as_str()) {
                emit(&seq);
                self.last.insert(key, seq);
                published += 1;
            }
        }
        published
    }

    /// Clear every published record and forget the cache.
    ///
    /// Used on suspend and exit, where the records must not outlive the TUI's
    /// ownership of the display. A no-op (no write) when nothing was
    /// published, so an exit with no sessions emits no stray clear.
    pub(crate) fn clear_all(&mut self) {
        self.clear_all_with(&mut |seq| write(seq));
    }

    /// Sink-based inner form of [`Self::clear_all`], for unit testing.
    fn clear_all_with(&mut self, emit: &mut dyn FnMut(&str)) {
        if !self.last.is_empty() {
            emit(&clear_all());
            self.last.clear();
        }
    }
}

#[cfg(test)]
mod tests {
    use base64::Engine as _;

    use super::*;

    /// The bare record for `key`: the always-present keys only.
    fn child(key: RecordKey, state: &str) -> String {
        let id = key.protocol_id();
        record(&StatusRecord::new(&id, APP, state))
    }

    /// A session record in `state` carrying optional extra fields.
    fn session(
        session_id: u64,
        state: &str,
        kind: Option<BlockedKind>,
        progress: Option<u8>,
        title: Option<&str>,
        msg: Option<&str>,
    ) -> String {
        let id = RecordKey::Session(session_id).protocol_id();
        record(&StatusRecord {
            kind,
            progress,
            title,
            msg,
            ..StatusRecord::new(&id, APP, state)
        })
    }

    /// Decode a base64 field of a built sequence.
    fn field(seq: &str, key: &str) -> Option<Vec<u8>> {
        let (_, tail) = seq.split_once(&format!(":{key}="))?;
        let encoded = tail.strip_suffix("\x1b\\")?;
        base64::engine::general_purpose::STANDARD
            .decode(encoded)
            .ok()
    }

    #[test]
    fn state_for_full_table() {
        assert_eq!(state_for(&SessionStatus::Inactive), "idle");
        assert_eq!(state_for(&SessionStatus::Sleeping), "idle");
        assert_eq!(state_for(&SessionStatus::Sending), "working");
        assert_eq!(state_for(&SessionStatus::Waiting), "working");
        assert_eq!(state_for(&SessionStatus::Reasoning), "working");
        assert_eq!(state_for(&SessionStatus::Responding), "working");
        assert_eq!(
            state_for(&SessionStatus::ToolCall("shell".into())),
            "working"
        );
        assert_eq!(
            state_for(&SessionStatus::Retrying {
                attempt: 2,
                max_attempts: 5,
                delay_ms: 250,
            }),
            "working"
        );
    }

    #[test]
    fn progress_for_only_counts_a_retrying_request() {
        let retrying = |attempt, max_attempts| SessionStatus::Retrying {
            attempt,
            max_attempts,
            delay_ms: 0,
        };
        assert_eq!(progress_for(&retrying(1, 4)), Some(25));
        assert_eq!(progress_for(&retrying(2, 4)), Some(50));
        assert_eq!(progress_for(&retrying(4, 4)), Some(100));
        // More attempts than the maximum still clamps to 100 rather than
        // wrapping.
        assert_eq!(progress_for(&retrying(9, 4)), Some(100));
        assert_eq!(progress_for(&retrying(0, 4)), Some(0));
        // A zero maximum carries no percentage at all (no divide by zero).
        assert_eq!(progress_for(&retrying(1, 0)), None);
        // Every other status has no honest "how far through" figure.
        for status in [
            SessionStatus::Inactive,
            SessionStatus::Sending,
            SessionStatus::Waiting,
            SessionStatus::Reasoning,
            SessionStatus::Responding,
            SessionStatus::ToolCall("shell".into()),
        ] {
            assert_eq!(progress_for(&status), None, "{status:?}");
        }
    }

    #[test]
    fn record_key_renders_the_protocol_id() {
        assert_eq!(RecordKey::Session(7).protocol_id(), "7");
        assert_eq!(RecordKey::Login.protocol_id(), "login");
    }

    #[test]
    fn record_carries_state_app_and_id() {
        assert_eq!(
            child(RecordKey::Session(7), "working"),
            "\x1b]7501;state=working:app=choreo-tui:id=7\x1b\\"
        );
    }

    #[test]
    fn record_appends_base64_title_and_msg() {
        // "hi" -> "aGk=".
        assert_eq!(
            session(7, "done", None, None, Some("hi"), Some("hi")),
            "\x1b]7501;state=done:app=choreo-tui:id=7:title=aGk=:msg=aGk=\x1b\\"
        );
    }

    #[test]
    fn record_omits_absent_title_and_msg() {
        let seq = child(RecordKey::Session(1), "idle");
        assert!(!seq.contains(":title="));
        assert!(!seq.contains(":msg="));
    }

    #[test]
    fn kind_is_emitted_only_with_blocked() {
        // `blocked` is the one state where `kind` is meaningful.
        assert_eq!(
            session(1, "blocked", Some(BlockedKind::Auth), None, None, None),
            "\x1b]7501;state=blocked:app=choreo-tui:id=1:kind=auth\x1b\\"
        );
        // The spec ignores `kind` outside `blocked`, so it is not emitted and a
        // `working` record can never read as "waiting on the user".
        for state in ["idle", "working", "done", "error", "clear"] {
            let seq = session(1, state, Some(BlockedKind::Auth), None, None, None);
            assert!(!seq.contains(":kind="), "state={state}: {seq}");
        }
        // A `blocked` record with no kind carries no `kind` key at all.
        assert_eq!(
            session(1, "blocked", None, None, None, None),
            "\x1b]7501;state=blocked:app=choreo-tui:id=1\x1b\\"
        );
    }

    #[test]
    fn progress_is_emitted_only_with_working_or_blocked() {
        assert_eq!(
            session(1, "working", None, Some(40), None, None),
            "\x1b]7501;state=working:app=choreo-tui:id=1:progress=40\x1b\\"
        );
        assert_eq!(
            session(1, "blocked", Some(BlockedKind::Auth), Some(7), None, None),
            "\x1b]7501;state=blocked:app=choreo-tui:id=1:kind=auth:progress=7\x1b\\"
        );
        // Every other state drops it.
        for state in ["idle", "done", "error", "clear"] {
            let seq = session(1, state, None, Some(50), None, None);
            assert!(!seq.contains(":progress="), "state={state}: {seq}");
        }
        // 0 is a value, not an absence.
        assert!(session(1, "working", None, Some(0), None, None).contains(":progress=0"));
        // Clamped to the protocol's range.
        assert!(session(1, "working", None, Some(200), None, None).contains(":progress=100"));
        // Absent when there is no honest figure to report.
        assert!(!child(RecordKey::Session(1), "working").contains(":progress="));
    }

    #[test]
    fn record_strips_control_chars_from_title_and_msg() {
        // A decoded control character makes a terminal discard the whole
        // report, so the fields are stripped before base64. "ab" -> "YWI=",
        // "cd" -> "Y2Q=".
        let seq = session(1, "working", None, None, Some("a\u{1b}b"), Some("c\u{9d}d"));
        assert_eq!(
            seq,
            "\x1b]7501;state=working:app=choreo-tui:id=1:title=YWI=:msg=Y2Q=\x1b\\"
        );
    }

    #[test]
    fn record_strips_bidi_formatting_from_title_and_msg() {
        // An RLO (U+202E) is stripped from both fields; "ab" -> "YWI=".
        let seq = session(
            1,
            "working",
            None,
            None,
            Some("a\u{202e}b"),
            Some("a\u{202e}b"),
        );
        assert_eq!(
            seq,
            "\x1b]7501;state=working:app=choreo-tui:id=1:title=YWI=:msg=YWI=\x1b\\"
        );
    }

    #[test]
    fn record_strips_invisible_format_chars_and_separators() {
        // The same class the grid filter escapes: ZWSP, word joiner, soft
        // hyphen, and the line/paragraph separators are dropped from a record
        // field too (a separator would break the one-line record). "ab" ->
        // "YWI=".
        for stripped in ["\u{200b}", "\u{2060}", "\u{00ad}", "\u{2028}", "\u{2029}"] {
            let text = format!("a{stripped}b");
            let seq = session(1, "working", None, None, Some(&text), Some(&text));
            assert_eq!(
                seq,
                "\x1b]7501;state=working:app=choreo-tui:id=1:title=YWI=:msg=YWI=\x1b\\",
                "U+{:04X} must be stripped from both fields",
                stripped.chars().next().map_or(0, u32::from)
            );
        }
        // Legitimate joiners survive.
        let seq = session(1, "idle", None, None, Some("a\u{200c}b\u{200d}c"), None);
        assert_eq!(
            field(&seq, "title").as_deref(),
            Some("a\u{200c}b\u{200d}c".as_bytes())
        );
    }

    #[test]
    fn record_drops_a_field_that_sanitizes_to_empty() {
        let seq = session(1, "idle", None, None, Some("\u{1b}"), Some("\u{07}"));
        assert_eq!(seq, "\x1b]7501;state=idle:app=choreo-tui:id=1\x1b\\");
    }

    #[test]
    fn record_caps_title_at_the_protocol_byte_limit() {
        let title = "x".repeat(300);
        let seq = session(1, "done", None, None, Some(&title), None);
        let decoded = field(&seq, "title").expect("title field present");
        // 192 decoded bytes -> exactly 256 base64 chars.
        assert_eq!(decoded.len(), 192);
        assert_eq!(decoded, vec![b'x'; 192]);
    }

    #[test]
    fn record_caps_msg_at_the_protocol_byte_limit_without_splitting_a_char() {
        // A run of 3-byte characters: 700 of them is 2100 bytes, over the
        // 2048 limit, so the cap keeps 682 (2046 bytes) and never splits one.
        let msg = "\u{2603}".repeat(700);
        let seq = session(1, "working", None, None, None, Some(&msg));
        let decoded = field(&seq, "msg").expect("msg field present");
        assert_eq!(decoded.len(), 2046);
        assert!(
            std::str::from_utf8(&decoded).is_ok(),
            "truncation must stay on a char boundary"
        );
    }

    #[test]
    fn clear_one_and_clear_all() {
        assert_eq!(
            clear_one(RecordKey::Session(42)),
            "\x1b]7501;state=clear:id=42\x1b\\"
        );
        assert_eq!(
            clear_one(RecordKey::Login),
            "\x1b]7501;state=clear:id=login\x1b\\"
        );
        assert_eq!(clear_all(), "\x1b]7501;state=clear\x1b\\");
    }

    #[test]
    fn publisher_writes_only_changed_records() {
        let mut publisher = Publisher::new();
        let mut out = String::new();
        let key = RecordKey::Session(1);

        publisher.sync_with(
            &mut |seq| out.push_str(seq),
            vec![(key, child(key, "working"))],
        );
        assert!(out.contains("state=working:app=choreo-tui:id=1"));
        let after_first = out.len();

        // Identical desired set: nothing written.
        publisher.sync_with(
            &mut |seq| out.push_str(seq),
            vec![(key, child(key, "working"))],
        );
        assert_eq!(
            out.len(),
            after_first,
            "an unchanged record must not rewrite"
        );

        // State changed: the record is republished in full.
        publisher.sync_with(
            &mut |seq| out.push_str(seq),
            vec![(key, child(key, "done"))],
        );
        let tail = out.get(after_first..).unwrap_or("");
        assert!(
            tail.contains("state=done:app=choreo-tui:id=1"),
            "a changed record must be rewritten"
        );
    }

    #[test]
    fn publisher_clears_removed_records() {
        let mut publisher = Publisher::new();
        let mut out = String::new();
        let one = RecordKey::Session(1);
        let two = RecordKey::Session(2);
        publisher.sync_with(
            &mut |seq| out.push_str(seq),
            vec![(one, child(one, "working")), (two, child(two, "working"))],
        );
        out.clear();

        // Session 1 disappeared: its record is cleared, session 2 untouched.
        publisher.sync_with(
            &mut |seq| out.push_str(seq),
            vec![(two, child(two, "working"))],
        );
        assert!(
            out.contains("state=clear:id=1"),
            "stale record must be cleared"
        );
        assert!(
            !out.contains("id=2"),
            "unchanged record must not be rewritten"
        );
    }

    #[test]
    fn publisher_clears_the_login_record_when_it_is_no_longer_desired() {
        let mut publisher = Publisher::new();
        let mut out = String::new();
        let login = child(RecordKey::Login, "blocked");
        assert!(login.contains("state=blocked:app=choreo-tui:id=login"));

        publisher.sync_with(
            &mut |seq| out.push_str(seq),
            vec![(RecordKey::Login, login)],
        );
        out.clear();

        // The modal closed: the login record is no longer desired and must be
        // cleared rather than left frozen on the terminal.
        publisher.sync_with(&mut |seq| out.push_str(seq), vec![]);
        assert_eq!(out, clear_one(RecordKey::Login));
    }

    #[test]
    fn publisher_clear_all_is_a_noop_when_empty() {
        let mut publisher = Publisher::new();
        let mut out = String::new();
        let key = RecordKey::Session(1);
        // Nothing published yet: clear_all emits nothing.
        publisher.clear_all_with(&mut |seq| out.push_str(seq));
        assert!(out.is_empty(), "an empty publisher must not emit a clear");

        // Publish one record, then clear: the clear is emitted and the cache
        // is dropped (a subsequent identical sync republishes).
        publisher.sync_with(&mut |_seq| {}, vec![(key, child(key, "working"))]);
        publisher.clear_all_with(&mut |seq| out.push_str(seq));
        assert_eq!(out, clear_all());

        out.clear();
        publisher.sync_with(
            &mut |seq| out.push_str(seq),
            vec![(key, child(key, "working"))],
        );
        assert!(out.contains("id=1"), "the cache was dropped by clear_all");
    }
}
