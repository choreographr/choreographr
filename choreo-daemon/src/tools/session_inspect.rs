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
use crate::reasoning::{
    build_chat_request_messages, include_reasoning_artifact, warn_on_missing_reasoning_artifacts,
};
use crate::sessions::SessionState;
use crate::tools::Tool;
use crate::tools::context::ToolContext;
use choreo_ai_protocols::{
    ReasoningPassback, model_reasoning_passback, provider_slug_for_model,
    requires_reasoning_content,
};
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
    /// on the session's own turns, then to the catalog's model→provider
    /// mapping.
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
/// session state, then the catalog's model→provider mapping" so the report is
/// reproducible with a known policy even when the session's own data is
/// ambiguous; the report states which source won so a wrong-looking passback
/// is immediately attributable.
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

    // Model first; the provider lookup needs it.
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
            _ => match provider_slug_for_model(&model) {
                Some(slug) => (slug, "catalog".to_string()),
                // The model is unknown to the catalog; report it as
                // unresolvable rather than guessing a slug that would feed a
                // wrong passback/reasoning_content policy.
                None => ("unknown".to_string(), "unresolved".to_string()),
            },
        },
    };
    if model.is_empty() {
        return Err(ToolExecError(
            "no model resolved: pass `model` or set the session's selected model".into(),
        ));
    }

    let passback = model_reasoning_passback(&provider, &model);
    // DeepSeek/Kimi chat: the builder injects an empty `reasoning_content` on
    // assistant messages with nothing to echo, so those turns carry the field
    // on the wire (not bare). Mirror it here or the ledger-vs-wire parity
    // check would disagree with the dry-run.
    let requires_rc = requires_reasoning_content(&provider, &model);

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
    let mut wire_empty: Vec<String> = Vec::new();
    for (msg_index, m) in messages.iter().enumerate() {
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
        // "must not be empty" hazard: an assistant message with no content,
        // no tool calls, and no non-empty reasoning echo is exactly what
        // OpenAI-compatible providers reject with a 400. Any turn reaching
        // this state on the wire is a hard failure candidate regardless of
        // the passback accounting.
        let content_empty = value
            .get("content")
            .and_then(|c| c.as_str())
            .is_none_or(str::is_empty);
        let reasoning_non_empty = ["reasoning_content", "reasoning", "reasoning_text"]
            .iter()
            .filter_map(|k| value.get(*k).and_then(|v| v.as_str()))
            .any(|s| !s.is_empty());
        if content_empty && !has_tools && !reasoning_non_empty {
            wire_empty.push(format!(
                "  position {msg_index}: \"must not be empty\" 400 candidate"
            ));
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
        // Same helper the builder uses — the empty-message fallback (a
        // content-less, tool-less turn on a requires_rc model echoes its
        // same-model artifact so the wire message is never empty) is applied
        // here automatically, keeping the ledger and the wire in lockstep.
        let include_artifact =
            include_reasoning_artifact(turn, &provider, &model, passback, requires_rc);
        let echo_has_artifact = include_artifact && turn.reasoning_artifact.is_some();
        // Whether this turn's assistant message carries SOME
        // `reasoning_content` on the wire: the real artifact text, or (on
        // models that require the field, e.g. DeepSeek/Kimi) an injected
        // empty string. The builder injects it only for non-undone turns that
        // emit a message and have nothing to echo — mirror that exactly.
        let wire_rc_present =
            !turn.undone && has_assistant_msg && (echo_has_artifact || requires_rc);

        let echo = if !has_assistant_msg {
            "n/a".to_string()
        } else if echo_has_artifact {
            "yes".to_string()
        } else if wire_rc_present {
            "yes(empty)".to_string()
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

        // Wire-parity counters (vs. the serialized-message dry-run below):
        // the builder skips `undone` turns entirely, so only non-undone turns
        // produce a message on the wire and count toward the parity check —
        // otherwise any undone turn with an artifact/tool call would make the
        // ledger and the wire tallies disagree on sessions containing undos.
        if !turn.undone && has_assistant_msg {
            if wire_rc_present {
                ledger_echo_count += 1;
            } else if !turn.tool_calls.is_empty() {
                ledger_tool_no_echo += 1;
            }
        }

        // Risk classification: "risk" means there is evidence the model
        // reasoned here (artifact bytes or displayed reasoning text) yet the
        // wire will omit ALL reasoning_content on a tool-call turn — exactly
        // the shape DeepSeek/Kimi reject with a `reasoning_content` 400.
        if has_assistant_msg && !turn.tool_calls.is_empty() && !wire_rc_present {
            if turn.reasoning_artifact.is_some() || turn.assistant_reasoning.is_some() {
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
        // is deliberately sent bare — informational, matches the policy (and
        // suppressed on models that now inject an empty reasoning_content).
        if has_assistant_msg
            && turn.tool_calls.is_empty()
            && !wire_rc_present
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
    if wire_empty.is_empty() {
        out.push_str("  empty assistant message(s) on the wire: none\n");
    } else {
        out.push_str(&format!(
            "  empty assistant message(s) on the wire (\"must not be empty\" 400 candidates):\n{}\n",
            wire_empty.join("\n"),
        ));
    }
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

    /// A non-DeepSeek OpenAI-compat chat producer (ToolLoop passback but no
    /// `reasoning_content` injection) so the bare-turn classification paths
    /// stay exercisable independent of the DeepSeek injection behavior.
    fn groq_producer(model: &str) -> ReasoningProducer {
        ReasoningProducer {
            provider_slug: "groq".into(),
            model: model.into(),
        }
    }

    const GROQ_MODEL: &str = "groq/llama-3.3-70b-versatile";

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
        // Non-DeepSeek chat (no injection): t0 tool turn with artifact +
        // matching producer → echoed; t1 tool turn that reasoned (display
        // text) but captured no artifact → would be sent bare → the
        // DeepSeek-style 400 candidate.
        let (_dir, ctx) = seed(
            42,
            42,
            GROQ_MODEL,
            vec![
                (
                    0,
                    tool_turn(
                        "c1",
                        "grep",
                        Some(chat_artifact("think one")),
                        Some(groq_producer(GROQ_MODEL)),
                        Some("think one"),
                    ),
                ),
                (
                    1,
                    tool_turn(
                        "c2",
                        "exec",
                        None,
                        Some(groq_producer(GROQ_MODEL)),
                        Some("think two"),
                    ),
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
    fn deepseek_tool_turn_without_any_reasoning_is_injected_empty() {
        // The opencode-go/deepseek-v4-flash 400 shape: a tool turn with no
        // reasoning at all. After the injection fix the builder still emits
        // `reasoning_content: ""`, so the report must show it as echoing an
        // empty value (not a bare tool turn / not a risk), and the wire
        // dry-run must report 0 bare tool-call messages.
        let (_dir, ctx) = seed(
            42,
            42,
            "deepseek-v4-flash",
            vec![(0, tool_turn("c1", "exec", None, Some(ds_producer()), None))],
        );
        let out = run(&ctx, inspect_args(None));
        assert!(out.contains("echo=yes(empty)"), "{out}");
        assert!(
            out.contains("RISK (tool-call turns with reasoning evidence but no wire echo): none"),
            "{out}"
        );
        assert!(
            !out.contains("NOTE (tool calls with no reasoning evidence"),
            "{out}"
        );
        assert!(out.contains("carry NONE (provider-reject risk); "), "{out}");
        assert!(
            out.contains("0 assistant tool-call message(s) carry NONE"),
            "{out}"
        );
        assert!(out.contains("ledger vs wire: OK"), "{out}");
    }

    #[test]
    fn tool_turn_without_any_reasoning_still_counts_toward_wire_parity() {
        // A non-DeepSeek tool turn with neither an artifact nor displayed
        // reasoning is sent bare, so the ledger's tool-no-echo counter must
        // include it — otherwise the ledger-vs-wire cross-check reports
        // MISMATCH even though the wire dry-run is exact.
        let (_dir, ctx) = seed(
            42,
            42,
            GROQ_MODEL,
            vec![(
                0,
                tool_turn("c1", "exec", None, Some(groq_producer(GROQ_MODEL)), None),
            )],
        );
        let out = run(&ctx, inspect_args(None));
        assert!(out.contains("echo=no:no-artifact"), "{out}");
        assert!(
            out.contains("NOTE (tool calls with no reasoning evidence"),
            "{out}"
        );
        // Both the ledger and the wire must count this one bare tool message
        // (and the echoed-message total must agree too) → parity OK.
        assert!(out.contains("ledger vs wire: OK"), "{out}");
    }

    #[test]
    fn producer_mismatch_drops_echo_as_risk() {
        // Artifact produced by openai/gpt-4 while the session runs groq
        // (non-DeepSeek, no injection) → the provenance gate drops the echo
        // and the tool-call turn goes bare.
        let (_dir, ctx) = seed(
            42,
            42,
            GROQ_MODEL,
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
        // A non-DeepSeek (no-injection) final-text assistant turn with an
        // artifact is deliberately sent bare under ToolLoop (echo only on
        // tool turns) — INFO, not RISK.
        let (_dir, ctx) = seed(
            42,
            42,
            GROQ_MODEL,
            vec![(
                0,
                text_turn(
                    "answer",
                    Some(chat_artifact("think one")),
                    Some(groq_producer(GROQ_MODEL)),
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
    fn requires_rc_empty_content_turn_echoes_artifact_on_wire() {
        // The reported bug shape (opencode-go deepseek→kimi mid-session
        // switch): a turn recorded as reasoning-only — empty content, no tool
        // calls, but a same-model artifact. ToolLoop alone would skip the
        // echo and the injected empty `reasoning_content` cannot make the
        // message non-empty, so upstream 400s with "the message ... must not
        // be empty". The empty-message fallback must echo the artifact: the
        // report shows echo=yes (real text, not the empty injection), NO
        // empty-message candidates on the wire dry-run, and the daemon guard
        // counts 0 problems.
        let (_dir, ctx) = seed(
            42,
            42,
            "deepseek-v4-flash",
            vec![(
                0,
                text_turn(
                    "",
                    Some(chat_artifact("real reasoning text")),
                    Some(ds_producer()),
                ),
            )],
        );
        let out = run(&ctx, inspect_args(None));
        assert!(out.contains("echo=yes"), "artifact echoed: {out}");
        assert!(
            out.contains("empty assistant message(s) on the wire: none"),
            "no empty assistant message on the wire: {out}"
        );
        assert!(
            out.contains("daemon guard warn_on_missing_reasoning_artifacts: 0 problem(s)"),
            "guard clean: {out}"
        );
        assert!(out.contains("ledger vs wire: OK"), "{out}");
    }

    #[test]
    fn requires_rc_empty_content_turn_without_artifact_is_reported() {
        // Same reasoning-only shape but NO artifact available to fill the
        // message: the builder cannot self-heal, so the report must surface
        // the wire-empty candidate (the provider's "must not be empty" 400)
        // and the daemon guard must count it as a problem.
        let (_dir, ctx) = seed(
            42,
            42,
            "deepseek-v4-flash",
            vec![(0, text_turn("", None, Some(ds_producer())))],
        );
        let out = run(&ctx, inspect_args(None));
        assert!(
            out.contains(
                "empty assistant message(s) on the wire (\"must not be empty\" 400 candidates)"
            ),
            "wire-empty candidate surfaced: {out}"
        );
        assert!(
            out.contains("daemon guard warn_on_missing_reasoning_artifacts: 1 problem(s)"),
            "guard flags the unfixable turn: {out}"
        );
        assert!(out.contains("ledger vs wire: OK"), "{out}");
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
