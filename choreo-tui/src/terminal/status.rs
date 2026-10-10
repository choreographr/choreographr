//! OSC 7501 Program Status Protocol.
//!
//! Reports what each session is doing to the terminal using Mitchell
//! Hashimoto's Program Status Protocol (`ESC ] 7501 ; <pairs> ST`). The body
//! is a `:`-separated list of `key=value` pairs; `state` is the only required
//! key (`idle` / `working` / `done` / `blocked` / `error` / `clear`), and
//! `title` / `msg` are standard base64 of UTF-8 text.
//!
//! **Root vs child records.** OSC 9;4 progress ([`super::progress`]) writes the
//! terminal-wide ROOT record: a terminal is permitted to map 9;4 onto the root
//! record. A 7501 report replaces its record *completely*, so if this module
//! also wrote the root record, every 9;4 update would wipe the app/state/msg
//! of a real status report. Therefore every report here is a CHILD record
//! keyed by session id (`id=<session_id>`) and never the root. For the same
//! reason each record names `app=choreo-tui` explicitly — a child cannot
//! inherit `app` from an unlabeled root.

use std::collections::{HashMap, HashSet};

use choreo_proto::SessionStatus;

use super::{is_bidi_control, is_control_char, osc, write};

/// Stable machine-readable program name placed on every record (children
/// cannot inherit `app` from the unlabeled OSC 9;4 root).
pub(crate) const APP: &str = "choreo-tui";

/// Protocol cap on the decoded `title` field, in bytes (spec "Limits": 192
/// decoded, 256 encoded).
const MAX_TITLE_BYTES: usize = 192;

/// Protocol cap on the decoded `msg` field, in bytes (spec "Limits": 2048
/// decoded, 2732 encoded).
const MAX_MSG_BYTES: usize = 2048;

/// Map a session status to an OSC 7501 `state` value.
///
/// `done` / `error` are not representable here — `SessionStatus` has no such
/// variants — so those are supplied by the caller's override (see
/// `App::term_status_override`).
pub(crate) fn state_for(status: &SessionStatus) -> &'static str {
    match status {
        SessionStatus::Inactive | SessionStatus::Sleeping => "idle",
        SessionStatus::Inference | SessionStatus::ToolCall(_) | SessionStatus::Retrying { .. } => {
            "working"
        }
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

/// Strip control characters and bidi-formatting controls, then truncate to
/// `max_bytes` on a char boundary.
///
/// The spec requires a report's decoded `title` / `msg` to contain no control
/// character and caps both by decoded byte length; a report that breaks either
/// is discarded whole, so the emitter must sanitize and cap before framing.
/// Bidirectional-formatting characters are also stripped: the spec's Security
/// section calls for disarming them before free text is shown, and a title/
/// `msg` is display text. Truncation snaps to a char boundary so a multi-byte
/// character is never split.
fn sanitize_value(text: &str, max_bytes: usize) -> String {
    let mut out = String::with_capacity(text.len().min(max_bytes));
    for c in text.chars() {
        if is_control_char(c) || is_bidi_control(c) {
            continue;
        }
        if out.len() + c.len_utf8() > max_bytes {
            break;
        }
        out.push(c);
    }
    out
}

/// Build the OSC 7501 child record sequence for `id`.
///
/// The body always carries `state`, `app`, and `id`; `title` and `msg` are
/// appended (base64) when present and non-empty after sanitization. Both are
/// run through [`sanitize_value`] so the emitted report can never exceed the
/// protocol's per-field limit nor carry a decoded control character; an empty
/// field is dropped rather than emitted as a meaningless `title=`/`msg=`.
pub(crate) fn record(
    id: u64,
    app: &str,
    state: &str,
    title: Option<&str>,
    msg: Option<&str>,
) -> String {
    let mut body = format!("state={state}:app={app}:id={id}");
    if let Some(title) = title {
        let title = sanitize_value(title, MAX_TITLE_BYTES);
        if !title.is_empty() {
            body.push_str(":title=");
            body.push_str(&encode(&title));
        }
    }
    if let Some(msg) = msg {
        let msg = sanitize_value(msg, MAX_MSG_BYTES);
        if !msg.is_empty() {
            body.push_str(":msg=");
            body.push_str(&encode(&msg));
        }
    }
    osc(7501, &body)
}

/// Build the sequence that clears the child record `id` (and its children).
pub(crate) fn clear_one(id: u64) -> String {
    osc(7501, &format!("state=clear:id={id}"))
}

/// Build the sequence that clears every record on the terminal.
pub(crate) fn clear_all() -> String {
    osc(7501, "state=clear")
}

/// Publishes OSC 7501 child records, suppressing no-op rewrites.
///
/// The terminal replaces a record wholesale on each report, so re-sending an
/// unchanged record every frame is pure churn — and could fight a user's
/// terminal-side dismissal. `last` caches the exact sequence published per id,
/// so [`Self::sync`] writes only records that changed (or are new) and clears
/// records whose session is no longer present.
#[derive(Default)]
pub(crate) struct Publisher {
    last: HashMap<u64, String>,
}

impl Publisher {
    /// Create an empty publisher.
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Publish `desired`, diffing against what was last published.
    pub(crate) fn sync(&mut self, desired: Vec<(u64, String)>) {
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
    fn sync_with(&mut self, emit: &mut dyn FnMut(&str), desired: Vec<(u64, String)>) -> usize {
        // A previously-published id that is absent now is stale — its session
        // went idle, was deleted, or stopped being active — so clear its
        // record rather than leave a frozen `working` record on the terminal.
        let desired_ids: HashSet<u64> = desired.iter().map(|(id, _)| *id).collect();
        let stale: Vec<u64> = self
            .last
            .keys()
            .copied()
            .filter(|id| !desired_ids.contains(id))
            .collect();

        let mut published = 0;
        for id in stale {
            self.last.remove(&id);
            emit(&clear_one(id));
            published += 1;
        }
        for (id, seq) in desired {
            if self.last.get(&id).map(String::as_str) != Some(seq.as_str()) {
                emit(&seq);
                self.last.insert(id, seq);
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

    fn child(id: u64, state: &str) -> String {
        record(id, APP, state, None, None)
    }

    #[test]
    fn state_for_full_table() {
        assert_eq!(state_for(&SessionStatus::Inactive), "idle");
        assert_eq!(state_for(&SessionStatus::Sleeping), "idle");
        assert_eq!(state_for(&SessionStatus::Inference), "working");
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
    fn record_carries_state_app_and_id() {
        assert_eq!(
            child(7, "working"),
            "\x1b]7501;state=working:app=choreo-tui:id=7\x1b\\"
        );
    }

    #[test]
    fn record_appends_base64_title_and_msg() {
        // "hi" -> "aGk=".
        assert_eq!(
            record(7, APP, "done", Some("hi"), Some("hi")),
            "\x1b]7501;state=done:app=choreo-tui:id=7:title=aGk=:msg=aGk=\x1b\\"
        );
    }

    #[test]
    fn record_omits_absent_title_and_msg() {
        let seq = child(1, "idle");
        assert!(!seq.contains(":title="));
        assert!(!seq.contains(":msg="));
    }

    #[test]
    fn record_strips_control_chars_from_title_and_msg() {
        // A decoded control character makes a terminal discard the whole
        // report, so the fields are stripped before base64. "ab" -> "YWI=",
        // "cd" -> "Y2Q=".
        let seq = record(1, APP, "working", Some("a\u{1b}b"), Some("c\u{9d}d"));
        assert_eq!(
            seq,
            "\x1b]7501;state=working:app=choreo-tui:id=1:title=YWI=:msg=Y2Q=\x1b\\"
        );
    }

    #[test]
    fn record_strips_bidi_formatting_from_title_and_msg() {
        // An RLO (U+202E) is stripped from both fields; "ab" -> "YWI=".
        let seq = record(1, APP, "working", Some("a\u{202e}b"), Some("a\u{202e}b"));
        assert_eq!(
            seq,
            "\x1b]7501;state=working:app=choreo-tui:id=1:title=YWI=:msg=YWI=\x1b\\"
        );
    }

    #[test]
    fn record_drops_a_field_that_sanitizes_to_empty() {
        let seq = record(1, APP, "idle", Some("\u{1b}"), Some("\u{07}"));
        assert_eq!(seq, "\x1b]7501;state=idle:app=choreo-tui:id=1\x1b\\");
    }

    #[test]
    fn record_caps_title_at_the_protocol_byte_limit() {
        let title = "x".repeat(300);
        let seq = record(1, APP, "done", Some(&title), None);
        let encoded = seq
            .split(":title=")
            .nth(1)
            .and_then(|s| s.strip_suffix("\x1b\\"))
            .expect("title field present");
        // 192 decoded bytes -> exactly 256 base64 chars.
        assert_eq!(encoded.len(), 256, "got {encoded}");
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(encoded)
            .expect("valid base64");
        assert_eq!(decoded, vec![b'x'; 192]);
    }

    #[test]
    fn record_caps_msg_at_the_protocol_byte_limit_without_splitting_a_char() {
        // A run of 3-byte characters: 700 of them is 2100 bytes, over the
        // 2048 limit, so the cap keeps 682 (2046 bytes) and never splits one.
        let msg = "\u{2603}".repeat(700);
        let seq = record(1, APP, "working", None, Some(&msg));
        let encoded = seq
            .split(":msg=")
            .nth(1)
            .and_then(|s| s.strip_suffix("\x1b\\"))
            .expect("msg field present");
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(encoded)
            .expect("valid base64");
        assert_eq!(decoded.len(), 2046);
        assert!(
            std::str::from_utf8(&decoded).is_ok(),
            "truncation must stay on a char boundary"
        );
    }

    #[test]
    fn clear_one_and_clear_all() {
        assert_eq!(clear_one(42), "\x1b]7501;state=clear:id=42\x1b\\");
        assert_eq!(clear_all(), "\x1b]7501;state=clear\x1b\\");
    }

    #[test]
    fn publisher_writes_only_changed_records() {
        let mut publisher = Publisher::new();
        let mut out = String::new();

        publisher.sync_with(&mut |seq| out.push_str(seq), vec![(1, child(1, "working"))]);
        assert!(out.contains("state=working:app=choreo-tui:id=1"));
        let after_first = out.len();

        // Identical desired set: nothing written.
        publisher.sync_with(&mut |seq| out.push_str(seq), vec![(1, child(1, "working"))]);
        assert_eq!(
            out.len(),
            after_first,
            "an unchanged record must not rewrite"
        );

        // State changed: the record is republished in full.
        publisher.sync_with(&mut |seq| out.push_str(seq), vec![(1, child(1, "done"))]);
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
        publisher.sync_with(
            &mut |seq| out.push_str(seq),
            vec![(1, child(1, "working")), (2, child(2, "working"))],
        );
        out.clear();

        // Session 1 disappeared: its record is cleared, session 2 untouched.
        publisher.sync_with(&mut |seq| out.push_str(seq), vec![(2, child(2, "working"))]);
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
    fn publisher_clear_all_is_a_noop_when_empty() {
        let mut publisher = Publisher::new();
        let mut out = String::new();
        // Nothing published yet: clear_all emits nothing.
        publisher.clear_all_with(&mut |seq| out.push_str(seq));
        assert!(out.is_empty(), "an empty publisher must not emit a clear");

        // Publish one record, then clear: the clear is emitted and the cache
        // is dropped (a subsequent identical sync republishes).
        publisher.sync_with(&mut |_seq| {}, vec![(1, child(1, "working"))]);
        publisher.clear_all_with(&mut |seq| out.push_str(seq));
        assert_eq!(out, clear_all());

        out.clear();
        publisher.sync_with(&mut |seq| out.push_str(seq), vec![(1, child(1, "working"))]);
        assert!(out.contains("id=1"), "the cache was dropped by clear_all");
    }
}
