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
