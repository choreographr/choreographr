//! `session_inspect` — read-only diagnostic tool (group: `debug`).
//!
//! Reproduces the exact request the daemon would build for a session and
//! reports, per assistant turn, whether the provider's reasoning wire field
//! (`reasoning_content` for DeepSeek/Kimi chat) would be echoed back — and
//! which turns would be sent bare, risking a provider rejection like
//! DeepSeek's "The `reasoning_content` in the thinking mode must be passed
//! back to the API".
//!
//! Everything is computed from the persisted session record + turns using the
//! SAME code paths the agent loop uses (`build_chat_request_messages`,
//! `warn_on_missing_reasoning_artifacts`, `model_reasoning_passback`), so the
//! report is a faithful dry-run rather than a reimplementation that can drift.
//! The request messages themselves are serialized the way the provider
//! adapter would emit them (the manual `Serialize` impl re-emits artifacts
//! into the wire field), so the "would carry reasoning_content on the wire"
//! count is exactly what the upstream would see. Read-only: only redb read
//! transactions are opened; session state is never mutated.
//!
//! Privacy mirrors the codebase's thinking-content invariant: artifact
//! *metadata* (variant, wire field, byte size) and producer identity are
//! reported for any session, but message-text previews and raw reasoning
//! bytes are only rendered for the calling session (thinking blocks and
//! encrypted signatures otherwise never leave the daemon — same rule as
//! `turn_for_client`), and raw reasoning additionally requires `include_raw`.

use super::ToolExecError;
use crate::db::{read_session, read_turns};
use crate::reasoning::{build_chat_request_messages, warn_on_missing_reasoning_artifacts};
use crate::sessions::SessionState;
use crate::tools::Tool;
use crate::tools::context::ToolContext;
use choreo_ai_protocols::{ReasoningPassback, model_reasoning_passback};
use choreo_proto::{ChatReasoningField, ReasoningArtifact, ReasoningProducer, Turn};
use schemars::JsonSchema;
use serde::Deserialize;
use std::collections::BTreeMap;
use tracing::info;

/// Default cap on the per-turn ledger rows (computation always covers all
/// turns; only the rendered table is capped so a huge session stays a
/// compact report).
const DEFAULT_MAX_TURNS: usize = 512;
/// Truncation width for message-text previews (own session only).
const PREVIEW_LEN: usize = 120;
/// Truncation width for raw reasoning snippets (`include_raw`, own session).
const RAW_LEN: usize = 400;

#[derive(Debug, Deserialize, JsonSchema)]
pub struct SessionInspectArgs {
    /// Session ID to inspect. Defaults to the calling session.
    #[serde(default)]
    pub session_id: Option<u64>,
    /// Provider slug used to resolve the reasoning-passback policy and the
    /// same-model provenance check. Defaults to the most common slug recorded
    /// on the session's own turns, then to a model-name-prefix inference.
    #[serde(default)]
    pub provider: Option<String>,
    /// Model used to resolve the passback policy and the provenance check.
    /// Defaults to the session's selected model.
    #[serde(default)]
    pub model: Option<String>,
    /// Include truncated raw reasoning text per assistant turn. Honored only
    /// for the calling session — thinking content is sensitive and otherwise
    /// never leaves the daemon process.
    #[serde(default)]
    pub include_raw: bool,
    /// Cap the per-turn ledger to this many turns (default 512).
    #[serde(default)]
    pub max_turns: Option<usize>,
}

pub(crate) struct SessionInspect;

impl Tool for SessionInspect {
    type Args = SessionInspectArgs;
    type Return = String;
    type Error = ToolExecError;

    fn name(&self) -> &'static str {
        "session_inspect"
    }

    fn group(&self) -> &'static str {
        "debug"
    }

    fn description(&self) -> &'static str {
        "Read-only reasoning-echo diagnostic: dry-runs the request the daemon \
         would build for a session and reports which assistant turns carry the \
         provider's reasoning field on the wire and which would be sent bare \
         (the DeepSeek/Kimi 400 \"reasoning_content must be passed back\" risk)."
    }

    fn describe_invocation(&self, args: &Self::Args) -> String {
        match args.session_id {
            Some(id) => format!("Inspecting session {id} reasoning-echo status."),
            None => "Inspecting current session reasoning-echo status.".into(),
        }
    }

    fn return_string(ret: &Self::Return) -> String {
        ret.clone()
    }

    fn execute(
        &self,
        args: Self::Args,
        _x_credentials: Option<&crate::tools::ServiceCredential>,
        _working_dir: Option<&std::path::Path>,
        ctx: Option<&ToolContext>,
    ) -> Result<Self::Return, Self::Error> {
        let ctx =
            ctx.ok_or_else(|| ToolExecError("session_inspect requires a session context".into()))?;
        let session_id = args.session_id.unwrap_or(ctx.session_id);
        let report = build_report(ctx, session_id, &args)?;
        info!(
            session_id,
            report_bytes = report.len(),
            "session_inspect completed"
        );
        Ok(report)
    }
}

/// Build the full diagnostic report for `session_id`.
///
/// The (provider, model) pair is resolved "explicit arg wins, then recorded
/// session state, then inference" so the report is reproducible with a known
/// policy even when the session's own data is ambiguous; the report states
/// which source won so a wrong-looking passback is immediately attributable.
fn build_report(
    ctx: &ToolContext,
    session_id: u64,
    args: &SessionInspectArgs,
) -> Result<String, ToolExecError> {
    let record = read_session(&ctx.db, session_id)
        .map_err(|e| ToolExecError(format!("read session {session_id}: {e}")))?;
    let Some(record) = record else {
        return Err(ToolExecError(format!("session {session_id} not found")));
    };
    let turns_raw = read_turns(&ctx.db, session_id)
        .map_err(|e| ToolExecError(format!("read turns for session {session_id}: {e}")))?;
    let turns: BTreeMap<u32, Turn> = turns_raw.into_iter().collect();

    // The most common producer recorded on the session's own turns is the
    // best recorded guess for the live (provider, model) pair; it also makes
    // the provenance check exact for a session whose provider slug is not
    // "deepseek" (e.g. an opencode-style gateway account).
    let dominant_producer = dominant_producer(&turns);

    // Model first; the provider inference needs it.
    let (model, model_source) = match args.model.clone() {
        Some(m) => (m, "arg".to_string()),
        None => match record.selected_model.clone() {
            Some(m) => (m, "record".to_string()),
            None => match dominant_producer.as_ref().map(|p| p.model.clone()) {
                Some(m) => (m, "dominant-producer".to_string()),
                None => (String::new(), "unresolved".to_string()),
            },
        },
    };
    let (provider, provider_source) = match args.provider.clone() {
        Some(p) => (p, "arg".to_string()),
        None => match dominant_producer.as_ref().map(|p| p.provider_slug.clone()) {
            Some(slug) if !slug.is_empty() => (slug, "dominant-producer".to_string()),
            _ => {
                let inferred = infer_provider(&model);
                if inferred == "unknown" {
                    (inferred.to_string(), "unresolved".to_string())
                } else {
                    (inferred.to_string(), "model-prefix".to_string())
                }
            }
        },
    };
    if model.is_empty() {
        return Err(ToolExecError(
            "no model resolved: pass `model` or set the session's selected model".into(),
        ));
    }

    let passback = model_reasoning_passback(&provider, &model);

    // Reconstruct the state the builder reads. `build_chat_request_messages`
    // only consults `turns` (and the optional system prompt, which we pass as
    // None); setting `selected_model` keeps the reconstruction honest for any
    // future provenance reads that consult config.
    let mut state = SessionState::empty();
    state.turns = turns;
    state.config.selected_model = Some(model.clone());

    let guard_problems = warn_on_missing_reasoning_artifacts(&state, session_id, &provider, &model);
    let messages = build_chat_request_messages(&state, None, &provider, &model);

    // Wire accounting: serialize each built message exactly as the adapter
    // would (the manual `Serialize` impl re-emits the artifact into the wire
    // field), then count what the upstream would actually see.
    let mut assistant_count = 0usize;
    let mut wire_rc_count = 0usize;
    let mut wire_tool_no_rc = 0usize;
    for m in &messages {
        if m.role != "assistant" {
            continue;
        }
        assistant_count += 1;
        let value = serde_json::to_value(m)
            .map_err(|e| ToolExecError(format!("serialize request message: {e}")))?;
        let has_rc = value.get("reasoning_content").is_some();
        let has_tools = value
            .get("tool_calls")
            .and_then(|tc| tc.as_array())
            .map(|arr| !arr.is_empty())
            .unwrap_or(false);
        if has_rc {
            wire_rc_count += 1;
        }
        if has_tools && !has_rc {
            wire_tool_no_rc += 1;
        }
    }

    // Per-turn ledger, computed with the same gates as the builder so the
    // report can attribute a reason to each turn. The wire cross-check below
    // validates that the two views agree — a MISMATCH means the ledger logic
    // and the real builder have drifted, which is itself a diagnostic signal.
    let mut ledger = Vec::new();
    let mut risks: Vec<String> = Vec::new();
    let mut notes: Vec<String> = Vec::new();
    let mut infos: Vec<String> = Vec::new();
    let mut raw_lines: Vec<String> = Vec::new();
    let mut ledger_echo_count = 0usize;
    let mut ledger_tool_no_echo = 0usize;
    let own_session = session_id == ctx.session_id;
    let cap = args.max_turns.unwrap_or(DEFAULT_MAX_TURNS);

    for (idx, (turn_id, turn)) in state.turns.iter().enumerate() {
        let has_assistant_msg = turn.assistant_text.is_some() || !turn.tool_calls.is_empty();
        // A turn with no user text and no assistant contribution emits no
        // messages on the wire; skip it (nothing to echo or omit).
        if turn.user_text.is_none() && !has_assistant_msg {
            continue;
        }

        let producer = turn.reasoning_producer.as_ref();
        let same_model = producer.map(|p| (p.provider_slug.as_str(), p.model.as_str()))
            == Some((provider.as_str(), model.as_str()));
        let tool_involvement = !turn.tool_calls.is_empty() || !turn.tool_results.is_empty();
        let include_artifact = same_model
            && match passback {
                ReasoningPassback::None | ReasoningPassback::ResponseId => false,
                ReasoningPassback::ToolLoop => tool_involvement,
                ReasoningPassback::AllTurns | ReasoningPassback::Signature => true,
            };
        let wire_echo = include_artifact && turn.reasoning_artifact.is_some();

        let echo = if !has_assistant_msg {
            "n/a".to_string()
        } else if wire_echo {
            ledger_echo_count += 1;
            "yes".to_string()
        } else if !same_model {
            "no:producer-mismatch".to_string()
        } else if turn.reasoning_artifact.is_none() {
            if turn.assistant_reasoning.is_some() {
                "no:reasoning-without-artifact".to_string()
            } else {
                "no:no-artifact".to_string()
            }
        } else {
            match passback {
                ReasoningPassback::None => "no:passback-none".to_string(),
                ReasoningPassback::ResponseId => "no:passback-response-id".to_string(),
                ReasoningPassback::ToolLoop if !tool_involvement => "no:tool-loop-skip".to_string(),
                // AllTurns/Signature with a matching artifact should have
                // echoed; anything reaching here is a policy/code drift.
                _ => "no:policy-drift".to_string(),
            }
        };

        let art = turn
            .reasoning_artifact
            .as_ref()
            .map(fmt_artifact)
            .unwrap_or_else(|| "-".into());
        let prod = producer
            .map(|p| format!("{}/{}", p.provider_slug, p.model))
            .unwrap_or_else(|| "-".into());
        let same = if same_model { "y" } else { "n" };
        let user_len = turn.user_text.as_deref().map_or(0, str::len);
        let asst_len = turn.assistant_text.as_deref().map_or(0, str::len);
        let flags = format!(
            "{}{}",
            if turn.undone { "U" } else { "-" },
            if turn.error.is_some() { "E" } else { "-" },
        );

        if idx < cap {
            ledger.push(format!(
                "  t{turn_id:>5} [{flags}] u={user_len} a={asst_len} tc={} r={} art={art} prod={prod} same={same} echo={echo}",
                turn.tool_calls.len(),
                turn.tool_results.len(),
            ));
        }

        // Risk classification: "risk" means there is evidence the model
        // reasoned here (artifact bytes or displayed reasoning text) yet the
        // wire will omit the echo on a tool-call turn — exactly the shape
        // DeepSeek/Kimi reject with a `reasoning_content` 400.
        if has_assistant_msg && !turn.tool_calls.is_empty() && !wire_echo {
            if turn.reasoning_artifact.is_some() || turn.assistant_reasoning.is_some() {
                ledger_tool_no_echo += 1;
                risks.push(format!(
                    "  t{turn_id}: {echo} (user: {})",
                    own_preview(turn.user_text.as_deref(), own_session),
                ));
            } else if !turn.undone {
                // Tool call with no reasoning evidence at all: cannot
                // distinguish "model produced no reasoning" from "capture
                // lost it" — surfaced as a note, not a hard risk.
                notes.push(format!(
                    "  t{turn_id}: tool call with no artifact and no displayed reasoning",
                ));
            }
        }
        // Under ToolLoop, a plain-text assistant turn that produced reasoning
        // is deliberately sent bare — informational, matches the policy.
        if has_assistant_msg
            && turn.tool_calls.is_empty()
            && !wire_echo
            && turn.reasoning_artifact.is_some()
            && same_model
            && matches!(passback, ReasoningPassback::ToolLoop)
        {
            infos.push(format!(
                "  t{turn_id}: short answer with reasoning artifact — ToolLoop echoes only \
                 tool turns, so this turn is sent bare (harmless unless the provider \
                 requires an all-turns echo)"
            ));
        }

        // Raw reasoning snippets: own session only, and only when explicitly
        // requested — thinking text never leaves the daemon otherwise.
        if own_session
            && args.include_raw
            && let Some(ReasoningArtifact::ChatReasoning { field, bytes }) =
                turn.reasoning_artifact.as_ref()
        {
            let text = String::from_utf8_lossy(bytes);
            raw_lines.push(format!(
                "  t{turn_id} [{}]: {}",
                fmt_field(field),
                truncate(&text, RAW_LEN)
            ));
        }
    }

    let mismatch = if ledger_echo_count == wire_rc_count && ledger_tool_no_echo == wire_tool_no_rc {
        "OK"
    } else {
        "MISMATCH"
    };

    let mut out = String::new();
    out.push_str(&format!("session_inspect: session {session_id}\n"));
    out.push_str(&format!("  model={model} (from: {model_source})\n"));
    out.push_str(&format!(
        "  provider={provider} (from: {provider_source}) passback={passback:?}\n"
    ));
    out.push_str(&format!(
        "  record: title={} working_dir={} account={} groups={:?}\n",
        record.title.as_deref().unwrap_or("-"),
        record.working_dir.as_deref().unwrap_or("-"),
        record.account_name.as_deref().unwrap_or("-"),
        record.active_tool_groups,
    ));
    out.push_str(&format!(
        "  turns={} assistant_messages={} messages_on_wire={}\n",
        state.turns.len(),
        assistant_count,
        messages.len(),
    ));
    out.push('\n');
    out.push_str(&format!(
        "  daemon guard warn_on_missing_reasoning_artifacts: {guard_problems} problem(s)\n"
    ));
    out.push_str(&format!(
        "  wire dry-run: {wire_rc_count}/{assistant_count} assistant messages carry reasoning_content;\n"
    ));
    out.push_str(&format!(
        "  {wire_tool_no_rc} assistant tool-call message(s) carry NONE (provider-reject risk); ledger vs wire: {mismatch}\n"
    ));
    out.push('\n');

    if risks.is_empty() {
        out.push_str("RISK (tool-call turns with reasoning evidence but no wire echo): none\n");
    } else {
        out.push_str(&format!(
            "RISK (tool-call turns with reasoning evidence but no wire echo — DeepSeek-style 400 candidates):\n{}\n",
            risks.join("\n"),
        ));
    }
    if !notes.is_empty() {
        out.push_str(&format!(
            "NOTE (tool calls with no reasoning evidence at all — unverifiable):\n{}\n",
            notes.join("\n"),
        ));
    }
    if !infos.is_empty() {
        out.push_str(&format!(
            "INFO (policy-skips — expected under this passback policy):\n{}\n",
            infos.join("\n"),
        ));
    }
    if !raw_lines.is_empty() {
        out.push_str(&format!(
            "RAW reasoning (include_raw, own session):\n{}\n",
            raw_lines.join("\n"),
        ));
    }
    out.push_str(&format!(
        "\nper-turn ledger{}:\n{}\n",
        if state.turns.len() > cap {
            format!(" (first {cap} of {})", state.turns.len())
        } else {
            String::new()
        },
        ledger.join("\n"),
    ));
    Ok(out)
}

/// Count occurrences of each distinct producer across the session's turns;
/// the most common one is the best recorded guess for the current
/// (provider, model) pair when the caller does not override it.
fn dominant_producer(turns: &BTreeMap<u32, Turn>) -> Option<ReasoningProducer> {
    let mut counts: BTreeMap<(String, String), usize> = BTreeMap::new();
    for turn in turns.values() {
        if let Some(p) = &turn.reasoning_producer {
            *counts
                .entry((p.provider_slug.clone(), p.model.clone()))
                .or_default() += 1;
        }
    }
    counts
        .into_iter()
        .max_by_key(|(_, count)| *count)
        .map(|((provider_slug, model), _)| ReasoningProducer {
            provider_slug,
            model,
        })
}

/// Best-effort provider slug from the model's name for the `passback` policy
/// lookup — only used when a session stores no producer at all.
fn infer_provider(model: &str) -> &'static str {
    let m = model.trim().to_ascii_lowercase();
    if m.contains("deepseek") {
        "deepseek"
    } else if m.contains("claude") || m.starts_with("op") {
        "anthropic"
    } else if m.contains("gemini") {
        "google"
    } else if m.contains("grok") {
        "xai"
    } else if m.contains("kimi") {
        "kimi"
    } else if m.starts_with("gpt-")
        || m.starts_with("o1")
        || m.starts_with("o3")
        || m.starts_with("o4")
        || m.starts_with("o5")
    {
        "openai"
    } else {
        "unknown"
    }
}

/// Compact artifact descriptor for the ledger: variant tag + wire field (for
/// chat) + byte size. Never includes the payload bytes themselves.
fn fmt_artifact(artifact: &ReasoningArtifact) -> String {
    match artifact {
        ReasoningArtifact::ChatReasoning { field, bytes } => {
            format!("chat[{};{}B]", fmt_field(field), bytes.len())
        }
        ReasoningArtifact::AnthropicThinking(bytes) => format!("anthropic[{}B]", bytes.len()),
        ReasoningArtifact::GoogleSignatures(bytes) => format!("google-sig[{}B]", bytes.len()),
        ReasoningArtifact::ResponsesItems(bytes) => format!("responses[{}B]", bytes.len()),
    }
}

fn fmt_field(field: &ChatReasoningField) -> &'static str {
    match field {
        ChatReasoningField::ReasoningContent => "reasoning_content",
        ChatReasoningField::Reasoning => "reasoning",
        ChatReasoningField::ReasoningText => "reasoning_text",
    }
}

/// Message-text previews are scope-gated: empty unless the report is for the
/// calling session (thinking/context privacy mirrors `turn_for_client`).
fn own_preview(text: Option<&str>, own_session: bool) -> String {
    match (text, own_session) {
        (Some(t), true) => truncate(t, PREVIEW_LEN),
        _ => "-".into(),
    }
}

fn truncate(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        text.to_string()
    } else {
        let cut: String = text.chars().take(max).collect();
        format!("{cut}…")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{SessionRecord, write_session, write_turn};
    use crate::tools::context::ToolContext;
    use choreo_proto::{AssistantToolCallRecord, ReasoningProducer, TimestampMs, Turn};
    use std::sync::Arc;
    use std::sync::mpsc;

    // Helper: seed a minimal session record + turns in a temp db and return a
    // ToolContext for the given "owning" session (the TempDir stays alive for
    // the duration of the test via the returned guard).
    fn seed(
        owner: u64,
        target: u64,
        selected_model: &str,
        turns: Vec<(u32, Turn)>,
    ) -> (tempfile::TempDir, ToolContext) {
        let dir = tempfile::tempdir().unwrap();
        let db = Arc::new(redb::Database::create(dir.path().join("test.redb")).unwrap());
        let now = TimestampMs::now().as_millis();
        let record = SessionRecord {
            title: Some("t".into()),
            selected_model: Some(selected_model.into()),
            parent_session_id: None,
            working_dir: None,
            turn_count: turns.len() as u32,
            created_at: now,
            last_modified: now,
            active_tool_groups: vec!["core".into()],
            context_config: Default::default(),
            account_name: None,
            reasoning_effort: None,
            last_response_id: None,
            last_response_id_producer: None,
        };
        write_session(&db, target, &record).unwrap();
        for (tid, t) in turns {
            write_turn(&db, target, tid, &t).unwrap();
        }
        let (daemon_tx, _rx) = mpsc::channel();
        let ctx = ToolContext::new(owner, db, daemon_tx);
        (dir, ctx)
    }

    fn chat_artifact(text: &str) -> ReasoningArtifact {
        ReasoningArtifact::ChatReasoning {
            field: ChatReasoningField::ReasoningContent,
            bytes: text.as_bytes().to_vec(),
        }
    }

    fn tool_turn(
        call_id: &str,
        tool_name: &str,
        artifact: Option<ReasoningArtifact>,
        producer: Option<ReasoningProducer>,
        reasoning: Option<&str>,
    ) -> Turn {
        Turn {
            created_at: TimestampMs::now(),
            undone: false,
            error: None,
            user_text: Some("run it".into()),
            assistant_text: None,
            assistant_reasoning: reasoning.map(String::from),
            tool_calls: vec![AssistantToolCallRecord {
                call_id: call_id.into(),
                name: tool_name.into(),
                arguments_json: "{}".into(),
            }],
            token_usage: None,
            tool_results: Vec::new(),
            displayed_images: Vec::new(),
            reasoning_artifact: artifact,
            reasoning_producer: producer,
        }
    }

    fn text_turn(
        text: &str,
        artifact: Option<ReasoningArtifact>,
        producer: Option<ReasoningProducer>,
    ) -> Turn {
        Turn {
            created_at: TimestampMs::now(),
            undone: false,
            error: None,
            user_text: Some("hi".into()),
            assistant_text: Some(text.into()),
            assistant_reasoning: None,
            tool_calls: Vec::new(),
            token_usage: None,
            tool_results: Vec::new(),
            displayed_images: Vec::new(),
            reasoning_artifact: artifact,
            reasoning_producer: producer,
        }
    }

    fn ds_producer() -> ReasoningProducer {
        ReasoningProducer {
            provider_slug: "deepseek".into(),
            model: "deepseek-v4-flash".into(),
        }
    }

    fn inspect_args(session: Option<u64>) -> SessionInspectArgs {
        SessionInspectArgs {
            session_id: session,
            provider: None,
            model: None,
            include_raw: false,
            max_turns: None,
        }
    }

    fn run(ctx: &ToolContext, args: SessionInspectArgs) -> String {
        SessionInspect.execute(args, None, None, Some(ctx)).unwrap()
    }

    #[test]
    fn tool_turn_without_artifact_is_flagged_as_risk() {
        // t0: tool turn with artifact + matching producer → echoed.
        // t1: tool turn that reasoned (display text) but captured no artifact
        //     → would be sent bare → the DeepSeek-style 400 candidate.
        let (_dir, ctx) = seed(
            42,
            42,
            "deepseek-v4-flash",
            vec![
                (
                    0,
                    tool_turn(
                        "c1",
                        "grep",
                        Some(chat_artifact("think one")),
                        Some(ds_producer()),
                        Some("think one"),
                    ),
                ),
                (
                    1,
                    tool_turn("c2", "exec", None, Some(ds_producer()), Some("think two")),
                ),
            ],
        );
        let out = run(&ctx, inspect_args(None));
        assert!(out.contains("t    0"), "t0 must appear in ledger: {out}");
        assert!(out.contains("echo=yes"), "t0 must echo: {out}");
        assert!(out.contains("t    1"), "t1 must appear: {out}");
        assert!(out.contains("no:reasoning-without-artifact"), "{out}");
        assert!(out.contains("RISK (tool-call turns"), "{out}");
        assert!(out.contains("ledger vs wire: OK"), "{out}");
        // The daemon's own guard must flag the one bare tool turn too.
        assert!(out.contains("1 problem(s)"), "{out}");
    }

    #[test]
    fn producer_mismatch_drops_echo_as_risk() {
        // Artifact produced by openai/gpt-4 while the session's model is
        // deepseek-v4-flash → the provenance gate drops the echo and the
        // tool-call turn goes bare.
        let (_dir, ctx) = seed(
            42,
            42,
            "deepseek-v4-flash",
            vec![(
                0,
                tool_turn(
                    "c1",
                    "exec",
                    Some(chat_artifact("think one")),
                    Some(ReasoningProducer {
                        provider_slug: "openai".into(),
                        model: "gpt-4".into(),
                    }),
                    Some("think one"),
                ),
            )],
        );
        let out = run(&ctx, inspect_args(None));
        assert!(out.contains("no:producer-mismatch"), "{out}");
        assert!(out.contains("RISK (tool-call turns"), "{out}");
    }

    #[test]
    fn plain_text_turn_artifact_is_info_not_risk() {
        // A final-text assistant turn with an artifact is deliberately sent
        // bare under ToolLoop (echo only on tool turns) — INFO, not RISK.
        let (_dir, ctx) = seed(
            42,
            42,
            "deepseek-v4-flash",
            vec![(
                0,
                text_turn(
                    "answer",
                    Some(chat_artifact("think one")),
                    Some(ds_producer()),
                ),
            )],
        );
        let out = run(&ctx, inspect_args(None));
        assert!(out.contains("no:tool-loop-skip"), "{out}");
        assert!(out.contains("INFO (policy-skips"), "{out}");
        assert!(!out.contains("no:producer-mismatch"), "{out}");
        // The RISK section must be the "none" variant, not a list.
        assert!(
            out.contains("RISK (tool-call turns with reasoning evidence but no wire echo): none"),
            "{out}"
        );
    }

    #[test]
    fn unknown_session_is_an_error() {
        let (_dir, ctx) = seed(42, 42, "deepseek-v4-flash", vec![]);
        let err = SessionInspect
            .execute(inspect_args(Some(999)), None, None, Some(&ctx))
            .unwrap_err();
        assert!(err.0.contains("not found"), "{err:?}");
    }

    #[test]
    fn raw_reasoning_only_for_own_session() {
        let secret = "SECRET THINKING PAYLOAD";
        // Same-session inspection with include_raw: raw snippets rendered.
        let (_dir, ctx) = seed(
            7,
            7,
            "deepseek-v4-flash",
            vec![(
                0,
                tool_turn(
                    "c1",
                    "exec",
                    Some(chat_artifact(secret)),
                    Some(ds_producer()),
                    Some(secret),
                ),
            )],
        );
        let args = SessionInspectArgs {
            include_raw: true,
            ..inspect_args(None)
        };
        let out = run(&ctx, args);
        assert!(out.contains("RAW reasoning"), "{out}");
        assert!(out.contains(secret), "{out}");

        // Cross-session inspection: metadata only — the raw thinking text
        // must not leak, while the artifact descriptor still shows.
        let (_dir, ctx_other) = seed(
            7,  // caller
            99, // target
            "deepseek-v4-flash",
            vec![(
                0,
                tool_turn(
                    "c1",
                    "exec",
                    Some(chat_artifact(secret)),
                    Some(ds_producer()),
                    Some(secret),
                ),
            )],
        );
        let out = run(&ctx_other, inspect_args(Some(99)));
        assert!(
            !out.contains(secret),
            "raw reasoning leaked across sessions: {out}"
        );
        assert!(out.contains("art=chat[reasoning_content;"), "{out}");
    }

    #[test]
    fn registry_registers_session_inspect_in_debug_group() {
        let registry = crate::tools::ToolRegistry::new().build();
        let active: std::collections::HashSet<String> = ["debug".into()].into_iter().collect();
        let defs = registry.available_definitions(&active);
        let names: Vec<&str> = defs.iter().map(|d| d.function.name.as_str()).collect();
        assert!(names.contains(&"session_inspect"), "{names:?}");
        // The debug group must be advertised so load_tools can enable it.
        let group_names: Vec<String> = registry.group_names();
        assert!(group_names.iter().any(|g| g == "debug"), "{group_names:?}");
    }
}
