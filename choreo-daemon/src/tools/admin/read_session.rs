//! `read_session` — read the readable text of another session's conversation
//! (group: `core`).
//!
//! Unlike `get_session` (metadata only) and `list_sessions` (an index), this
//! tool renders the *text* of a session's turns: the user messages, the
//! assistant responses, and the assistant's displayed reasoning text. It
//! deliberately excludes tool-call inputs and tool results (noisy, and the
//! part of a transcript most likely to carry injected network content), and it
//! never touches the opaque reasoning round-trip artifacts (encrypted provider
//! blobs, thinking-block JSON) — those stay daemon-only, exactly as
//! `turn_for_client` keeps them off the wire.
//!
//! Reads go straight to the shared redb database via `crate::db::read_turns`,
//! the same read path `session_inspect` uses, so no daemon round-trip is needed
//! and any session's text is readable from any other — the daemon is a
//! single-user local process with no session access control. Only *committed*
//! turns are visible: a turn is persisted at the end of its agent-loop request,
//! so an in-flight draft is not yet readable.
//!
//! Output is bounded twice: each field is truncated at `max_field_chars`
//! (default 2000), and the assembled text is trimmed to the shared
//! `MAX_TOOL_OUTPUT_BYTES` budget by dropping whole turn blocks — the newest
//! first when reading the default tail window (so the answer you came for
//! survives), the earliest first when reading forward from `from` (so the
//! requested start survives).  A single surviving block that is still too
//! large — a multi-byte field near the character ceiling — is truncated at a
//! char boundary, so the byte budget holds whatever the field contents.

use crate::db::{read_session as read_session_record, read_turns};
use crate::tools::context::ToolContext;
use crate::tools::{MAX_TOOL_OUTPUT_BYTES, Tool, ToolExecError};
use choreo_keystore::ServiceCredential;
use choreo_proto::Turn;
use schemars::JsonSchema;
use serde::Deserialize;
use std::fmt::Write as _;
use std::path::Path;

/// Default per-field character cap, applied before the shared byte budget.
const DEFAULT_MAX_FIELD_CHARS: usize = 2000;
/// Default number of turns rendered when the caller gives no `limit`.
const DEFAULT_LIMIT: usize = 40;
/// Head-room reserved for the header and the trailing range/hint footer, so the
/// final text never exceeds `MAX_TOOL_OUTPUT_BYTES`.
const FOOTER_RESERVE: usize = 256;
/// Upper bound on the per-field cap: keeps one field's work bounded so a single
/// turn stays cheap to render and format.  This is a *character* cap — the
/// assembled text's *byte* budget is enforced separately by [`render`], because
/// a multi-byte field can occupy several bytes per character and so cannot be
/// bounded by a character count alone.
const MAX_FIELD_CEILING: usize = MAX_TOOL_OUTPUT_BYTES / 4;

// ── Args struct ─────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize, JsonSchema)]
pub(crate) struct ReadSessionArgs {
    /// Session ID whose conversation text to read.
    session_id: u64,
    /// Turn id to start from, inclusive. Omit to read the most recent turns.
    #[serde(default)]
    from: Option<u32>,
    /// Maximum number of turns to render (default 40).
    #[serde(default)]
    limit: Option<u32>,
    /// Per-field character cap before truncation (default 2000).
    #[serde(default)]
    max_field_chars: Option<usize>,
}

// ── read_session ────────────────────────────────────────────────────────────

fn execute_read_session(
    args: &ReadSessionArgs,
    _working_dir: Option<&Path>,
    ctx: Option<&ToolContext>,
) -> Result<String, ToolExecError> {
    let ctx = ctx.ok_or_else(|| ToolExecError("no session context".into()))?;
    let session_id = args.session_id;
    let record = read_session_record(&ctx.db, session_id)
        .map_err(|e| ToolExecError(format!("read session {session_id}: {e}")))?
        .ok_or_else(|| ToolExecError(format!("Session {session_id} not found.")))?;
    let all = read_turns(&ctx.db, session_id)
        .map_err(|e| ToolExecError(format!("read turns for session {session_id}: {e}")))?;
    // Undone turns are hidden history — the request builder skips them, so a
    // reader that showed them would misrepresent the conversation.
    let turns: Vec<(u32, Turn)> = all.into_iter().filter(|(_, t)| !t.undone).collect();

    let limit = match args.limit {
        // `u32 → usize` is lossless on every target we build for; the
        // saturating fallback keeps the conversion total on a hypothetical
        // 16-bit target instead of introducing a panic surface.
        Some(n) => usize::try_from(n).unwrap_or(usize::MAX).max(1),
        None => DEFAULT_LIMIT,
    };
    // Clamp the per-field cap so one rendered turn always fits the budget,
    // whatever the caller asks for.
    let max_field = args
        .max_field_chars
        .unwrap_or(DEFAULT_MAX_FIELD_CHARS)
        .clamp(1, MAX_FIELD_CEILING);

    // A `from` window reads forward from the requested turn; the default is a
    // tail window (the most recent `limit` turns), because a researched answer
    // lands at the end of the conversation.
    let keep_tail = args.from.is_none();
    let window: &[(u32, Turn)] = if let Some(start) = args.from {
        let begin = turns
            .iter()
            .position(|(id, _)| *id >= start)
            .unwrap_or(turns.len());
        let end = turns.len().min(begin.saturating_add(limit));
        turns.get(begin..end).unwrap_or_default()
    } else {
        let begin = turns.len().saturating_sub(limit);
        turns.get(begin..).unwrap_or_default()
    };

    Ok(render(
        session_id,
        record.title.as_deref(),
        &turns,
        window,
        max_field,
        keep_tail,
    ))
}

/// Assemble the header, the rendered turn blocks, and a range/hint footer,
/// trimming whole blocks until the whole thing fits the shared byte budget.
fn render(
    session_id: u64,
    title: Option<&str>,
    all: &[(u32, Turn)],
    window: &[(u32, Turn)],
    max_field: usize,
    keep_tail: bool,
) -> String {
    let title = title.unwrap_or("(untitled)");
    let mut blocks: Vec<(u32, String)> = Vec::new();
    let mut block_bytes = 0usize;
    for (turn_id, turn) in window {
        let block = render_turn(*turn_id, turn, max_field);
        if block.is_empty() {
            continue;
        }
        block_bytes += block.len();
        blocks.push((*turn_id, block));
    }

    let header = format!("session {session_id} \"{title}\" ({} turns)\n\n", all.len());

    // Trim whole turn blocks until the text fits the shared budget. Keep the
    // newest blocks for a tail read, the earliest for a forward read; always
    // retain at least one so a lone oversized turn still renders (its fields
    // are capped, so one block fits).
    while blocks.len() > 1 && header.len() + block_bytes + FOOTER_RESERVE > MAX_TOOL_OUTPUT_BYTES {
        let removed = if keep_tail {
            blocks.remove(0)
        } else {
            match blocks.pop() {
                Some(block) => block,
                None => break,
            }
        };
        block_bytes = block_bytes.saturating_sub(removed.1.len());
    }

    let mut out = header;
    if blocks.is_empty() {
        // Distinguish "the session has no readable text at all" from "the
        // requested window held none" (a `from` past the last turn, or a window
        // whose turns carry only tool activity) — the two call for different
        // follow-ups from the reader.
        if all.is_empty() {
            out.push_str("(no user or assistant text)\n");
        } else {
            let _ = writeln!(
                out,
                "(no readable text in the requested window of {} turns)",
                all.len()
            );
        }
        return out;
    }

    let shown_first = blocks.first().map_or(0, |(id, _)| *id);
    let shown_last = blocks.last().map_or(0, |(id, _)| *id);
    for (_, block) in &blocks {
        out.push_str(block);
    }
    // Drop the blank line each block leaves trailing so the footer hugs the
    // last rendered line.
    while out.ends_with('\n') {
        out.pop();
    }
    // A per-field *character* cap does not bound the *bytes* a multi-byte field
    // occupies, so the whole-block trimming above cannot guarantee the byte
    // budget on its own.  Cap the assembled body `FOOTER_RESERVE` below the
    // shared budget — leaving room for the footer appended below — so the final
    // text never exceeds `MAX_TOOL_OUTPUT_BYTES` whatever the field contents.
    let body_budget = MAX_TOOL_OUTPUT_BYTES.saturating_sub(FOOTER_RESERVE);
    if out.len() > body_budget {
        out = truncate_bytes(&out, body_budget);
    }

    let first_id = all.first().map_or(shown_first, |(id, _)| *id);
    let last_id = all.last().map_or(shown_last, |(id, _)| *id);
    let _ = write!(
        out,
        "\n\n[showing turns {shown_first}..{shown_last} of {}]",
        all.len()
    );
    if shown_last < last_id {
        let _ = write!(
            out,
            "\n[more after turn {shown_last}: pass from={}]",
            shown_last.saturating_add(1)
        );
    }
    if shown_first > first_id {
        let _ = write!(out, "\n[earlier turns exist: pass from={first_id}]");
    }
    out.push('\n');
    out
}

/// Render one turn's readable text: the user message, then the assistant's
/// displayed reasoning, then the assistant response. Tool calls and tool
/// results are intentionally omitted; image bytes and reasoning artifacts never
/// appear.  A turn's request-level `error` text is likewise omitted — a failed
/// turn is read from its session directly for the provider error.
fn render_turn(turn_id: u32, turn: &Turn, max_field: usize) -> String {
    let mut out = String::new();
    push_section(
        &mut out,
        turn_id,
        "user",
        turn.user_text.as_deref(),
        max_field,
    );
    push_section(
        &mut out,
        turn_id,
        "reasoning",
        turn.assistant_reasoning.as_deref(),
        max_field,
    );
    push_section(
        &mut out,
        turn_id,
        "assistant",
        turn.assistant_text.as_deref(),
        max_field,
    );
    out
}

/// Append `[turn N] label:\n<text>\n\n` when `text` is present and non-empty.
fn push_section(out: &mut String, turn_id: u32, label: &str, text: Option<&str>, max_field: usize) {
    if let Some(text) = text.filter(|t| !t.is_empty()) {
        let _ = write!(
            out,
            "[turn {turn_id}] {label}:\n{}\n\n",
            truncate_chars(text, max_field)
        );
    }
}

/// Truncate to `max` characters (not bytes) so multi-byte text is never split,
/// appending an ellipsis when anything was dropped.
fn truncate_chars(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let mut out: String = text.chars().take(max).collect();
    out.push('\u{2026}');
    out
}

/// Cap `text` at `max_bytes` UTF-8 bytes, cutting on a char boundary and
/// appending an ellipsis when anything was dropped.
///
/// The byte twin of [`truncate_chars`]: used to enforce the assembled text's
/// byte budget, which a character cap cannot guarantee for multi-byte content.
fn truncate_bytes(text: &str, max_bytes: usize) -> String {
    if text.len() <= max_bytes {
        return text.to_string();
    }
    // Reserve room for the ellipsis, then back up to the nearest char boundary
    // so the slice below never splits a code point.
    let budget = max_bytes.saturating_sub('\u{2026}'.len_utf8());
    let mut end = budget.min(text.len());
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    let mut out = String::with_capacity(end + '\u{2026}'.len_utf8());
    // `end` was snapped to a char boundary above; `.get` keeps the slice total.
    out.push_str(text.get(..end).unwrap_or(""));
    out.push('\u{2026}');
    out
}

pub(crate) struct ReadSession;

impl Tool for ReadSession {
    type Args = ReadSessionArgs;
    type Return = String;
    type Error = ToolExecError;

    fn name(&self) -> &'static str {
        "read_session"
    }

    fn group(&self) -> &'static str {
        "core"
    }

    fn description(&self) -> &'static str {
        "Read the text of another session's conversation by its ID: each user \
         message, the assistant's reasoning text, and the assistant response, \
         oldest-first or the most recent window. Tool call inputs and results \
         are not included. Use list_sessions to find an ID and get_session for \
         metadata only."
    }

    fn describe_invocation(&self, args: &Self::Args) -> String {
        format!("Reading session {}.", args.session_id)
    }

    fn return_string(ret: &Self::Return) -> String {
        ret.clone()
    }

    fn execute(
        &self,
        args: Self::Args,
        _x_credentials: Option<&ServiceCredential>,
        working_dir: Option<&Path>,
        ctx: Option<&ToolContext>,
    ) -> Result<Self::Return, Self::Error> {
        execute_read_session(&args, working_dir, ctx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{SessionRecord, write_session, write_turn};
    use choreo_proto::{ContextConfig, TimestampMs, ToolResultRecord};
    use std::sync::Arc;

    // -- helpers --------------------------------------------------------------

    /// Seed a `target` session with `turns` and return a context whose calling
    /// session is `owner` (the `TempDir` guard keeps the DB alive).
    fn seed(owner: u64, target: u64, turns: Vec<(u32, Turn)>) -> (tempfile::TempDir, ToolContext) {
        let dir = tempfile::tempdir().unwrap();
        let db = Arc::new(redb::Database::create(dir.path().join("test.redb")).unwrap());
        let now = TimestampMs::now().as_millis();
        let turn_count = u32::try_from(turns.len()).unwrap();
        let record = SessionRecord {
            title: Some("research".into()),
            selected_model: Some("gpt-4".into()),
            parent_session_id: None,
            working_dir: None,
            turn_count,
            created_at: now,
            last_modified: now,
            active_tool_groups: vec!["core".into()],
            context_config: ContextConfig::default(),
            account_name: None,
            reasoning_effort: None,
            last_response_id: None,
            last_response_id_producer: None,
            pinned: false,
            archived_at: None,
        };
        write_session(&db, target, &record).unwrap();
        for (tid, turn) in turns {
            write_turn(&db, target, tid, &turn).unwrap();
        }
        let (daemon_tx, _rx) = crossbeam_channel::unbounded();
        let ctx = ToolContext::new(owner, db, daemon_tx);
        (dir, ctx)
    }

    fn turn(user: Option<&str>, assistant: Option<&str>, reasoning: Option<&str>) -> Turn {
        Turn {
            created_at: TimestampMs::now(),
            undone: false,
            error: None,
            user_text: user.map(String::from),
            assistant_text: assistant.map(String::from),
            assistant_reasoning: reasoning.map(String::from),
            tool_calls: Vec::new(),
            token_usage: None,
            tool_results: Vec::new(),
            displayed_images: Vec::new(),
            reasoning_artifact: None,
            reasoning_producer: None,
        }
    }

    fn args(session_id: u64) -> ReadSessionArgs {
        ReadSessionArgs {
            session_id,
            from: None,
            limit: None,
            max_field_chars: None,
        }
    }

    fn run(ctx: &ToolContext, args: ReadSessionArgs) -> Result<String, ToolExecError> {
        ReadSession.execute(args, None, None, Some(ctx))
    }

    // -- read path ------------------------------------------------------------

    #[test]
    fn reads_another_sessions_user_and_assistant_text() {
        let (_dir, ctx) = seed(
            42,
            99,
            vec![(
                0,
                turn(Some("what is deepseek pricing"), Some("$0.27 per M"), None),
            )],
        );
        let out = run(&ctx, args(99)).unwrap();
        assert!(out.contains("what is deepseek pricing"), "{out}");
        assert!(out.contains("$0.27 per M"), "{out}");
        assert!(out.contains("session 99"), "{out}");
        assert!(out.contains("research"), "{out}");
    }

    #[test]
    fn reads_reasoning_text_from_another_session() {
        // Reasoning display text is readable cross-session (only the opaque
        // artifact bytes stay daemon-only).
        let (_dir, ctx) = seed(
            42,
            99,
            vec![(0, turn(Some("q"), Some("a"), Some("SECRET THINKING")))],
        );
        let out = run(&ctx, args(99)).unwrap();
        assert!(out.contains("SECRET THINKING"), "{out}");
        assert!(out.contains("reasoning"), "{out}");
    }

    #[test]
    fn omits_tool_calls_and_tool_results() {
        let mut t = turn(Some("user-question"), Some("assistant-answer"), None);
        t.tool_results = vec![ToolResultRecord {
            call_id: "c1".into(),
            name: "read_file".into(),
            content: "TOOL OUTPUT CONTENT".into(),
            is_error: false,
            invocation_description: "Reading a file.".into(),
            image: None,
        }];
        let (_dir, ctx) = seed(42, 99, vec![(0, t)]);
        let out = run(&ctx, args(99)).unwrap();
        assert!(out.contains("user-question"), "{out}");
        assert!(out.contains("assistant-answer"), "{out}");
        assert!(!out.contains("TOOL OUTPUT CONTENT"), "{out}");
        assert!(!out.contains("read_file"), "{out}");
    }

    #[test]
    fn skips_undone_turns() {
        let mut hidden = turn(Some("HIDDEN-Q"), Some("HIDDEN-A"), None);
        hidden.undone = true;
        let (_dir, ctx) = seed(
            42,
            99,
            vec![
                (0, hidden),
                (1, turn(Some("visible-q"), Some("visible-a"), None)),
            ],
        );
        let out = run(&ctx, args(99)).unwrap();
        assert!(!out.contains("HIDDEN"), "{out}");
        assert!(out.contains("visible-q"), "{out}");
    }

    // -- windowing ------------------------------------------------------------

    #[test]
    fn tail_window_keeps_recent_turns_and_hints_at_earlier() {
        let turns: Vec<(u32, Turn)> = (0..5)
            .map(|i| {
                (
                    i,
                    turn(
                        Some(&format!("user-{i}")),
                        Some(&format!("answer-{i}")),
                        None,
                    ),
                )
            })
            .collect();
        let (_dir, ctx) = seed(42, 99, turns);
        let a = ReadSessionArgs {
            session_id: 99,
            from: None,
            limit: Some(2),
            max_field_chars: None,
        };
        let out = run(&ctx, a).unwrap();
        assert!(out.contains("user-3") && out.contains("answer-3"), "{out}");
        assert!(out.contains("user-4") && out.contains("answer-4"), "{out}");
        assert!(!out.contains("user-0"), "{out}");
        assert!(out.contains("showing turns 3..4 of 5"), "{out}");
        assert!(out.contains("from=0"), "earlier-turn hint: {out}");
    }

    #[test]
    fn from_window_reads_forward_and_hints_at_more() {
        let turns: Vec<(u32, Turn)> = (0..5)
            .map(|i| {
                (
                    i,
                    turn(
                        Some(&format!("user-{i}")),
                        Some(&format!("answer-{i}")),
                        None,
                    ),
                )
            })
            .collect();
        let (_dir, ctx) = seed(42, 99, turns);
        let a = ReadSessionArgs {
            session_id: 99,
            from: Some(1),
            limit: Some(2),
            max_field_chars: None,
        };
        let out = run(&ctx, a).unwrap();
        assert!(out.contains("user-1") && out.contains("user-2"), "{out}");
        assert!(!out.contains("user-0"), "{out}");
        assert!(!out.contains("user-3"), "{out}");
        assert!(out.contains("from=3"), "more-after hint: {out}");
        assert!(out.contains("from=0"), "earlier-turns hint: {out}");
    }

    #[test]
    fn output_stays_within_budget() {
        let big = "x".repeat(500_000);
        let turns: Vec<(u32, Turn)> = (0..50)
            .map(|i| (i, turn(Some(&big), Some(&big), Some(&big))))
            .collect();
        let (_dir, ctx) = seed(42, 99, turns);
        let out = run(&ctx, args(99)).unwrap();
        assert!(out.len() <= MAX_TOOL_OUTPUT_BYTES, "{} bytes", out.len());
    }

    #[test]
    fn output_stays_within_budget_for_multibyte_fields() {
        // A per-field *character* cap does not bound bytes: '€' is 3 bytes, so a
        // field near the character ceiling can occupy 3× its char count.  The
        // assembled body is byte-capped, so the shared budget holds regardless.
        let big = "€".repeat(200_000);
        let turns: Vec<(u32, Turn)> = (0..50)
            .map(|i| (i, turn(Some(&big), Some(&big), Some(&big))))
            .collect();
        let (_dir, ctx) = seed(42, 99, turns);
        let out = run(&ctx, args(99)).unwrap();
        assert!(out.len() <= MAX_TOOL_OUTPUT_BYTES, "{} bytes", out.len());
    }

    #[test]
    fn from_past_the_last_turn_reports_an_empty_window() {
        // `from` beyond the last turn selects nothing; the reader must say the
        // window was empty rather than that the session has no text.
        let turns: Vec<(u32, Turn)> = (0..3)
            .map(|i| {
                (
                    i,
                    turn(
                        Some(&format!("user-{i}")),
                        Some(&format!("answer-{i}")),
                        None,
                    ),
                )
            })
            .collect();
        let (_dir, ctx) = seed(42, 99, turns);
        let a = ReadSessionArgs {
            session_id: 99,
            from: Some(99),
            limit: None,
            max_field_chars: None,
        };
        let out = run(&ctx, a).unwrap();
        assert!(
            out.contains("no readable text in the requested window"),
            "{out}"
        );
    }

    // -- errors / registration ------------------------------------------------

    #[test]
    fn unknown_session_is_an_error() {
        let (_dir, ctx) = seed(42, 99, vec![]);
        let err = run(&ctx, args(1000)).unwrap_err();
        assert!(err.0.contains("not found"), "{err:?}");
    }

    #[test]
    fn missing_context_is_an_error() {
        let err = ReadSession.execute(args(1), None, None, None).unwrap_err();
        assert!(err.0.contains("no session context"), "{err:?}");
    }

    #[test]
    fn registry_registers_read_session_in_core_group() {
        let registry = crate::tools::ToolRegistry::new().build();
        let active: std::collections::HashSet<String> = ["core".into()].into_iter().collect();
        let defs = registry.available_definitions(&active);
        let names: Vec<&str> = defs.iter().map(|d| d.function.name.as_str()).collect();
        assert!(names.contains(&"read_session"), "{names:?}");
    }
}
