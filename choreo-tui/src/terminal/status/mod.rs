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
/// `segment ("/" segment)*` with `segment := [A-Za-z0-9_.+-]{1,32}`, so a bare
/// `login` is a single valid segment and cannot collide with a session record
/// (whose id is that session's numeric id).
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
///
/// `pub(crate)` because the host reporter (`state::host_report`) puts the same
/// `title`/`msg` text on a command line: one sanitizer, so the terminal record
/// and the host report can never disagree about what a field's text is.
pub(crate) fn sanitize_value(text: &str, max_bytes: usize) -> String {
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
    ///
    /// Every sequence a diff produces is accumulated and written to the
    /// terminal in **one** pass, so a status change that touches five sessions
    /// costs one lock/write/flush of stdout rather than five — each of which is
    /// a window in which the writer could interleave with a frame.
    pub(crate) fn sync(&mut self, desired: Vec<(RecordKey, String)>) {
        let mut out = String::new();
        let published = self.sync_with(&mut |seq| out.push_str(seq), desired);
        if published > 0 {
            write(&out);
            tracing::debug!(
                published,
                records = self.last.len(),
                bytes = out.len(),
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
mod tests;
